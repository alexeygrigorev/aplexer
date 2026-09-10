//! The workload's life story: the events the runtime threads feed in, and
//! the one thread that turns them into finalization.
//!
//! One reason to exist: exactly one place decides what a session's death
//! means. PtyEof/ChildExit/output exhaustion feed the same loop; the loop
//! kills the containment domain, writes the terminal record (or keeps it as
//! evidence when finalization cannot be proven), drains attached clients,
//! and exits the process -- in that order, under the kill gate.

use super::*;

pub(super) enum LifeEvent {
    PtyEof,
    PtyError(String),
    WaiterError(String),
    ChildExit {
        code: Option<i32>,
        signal: Option<i32>,
    },
}

/// A waiter failure means nobody owns the tracked Child any longer. Before
/// the subreaper is allowed to exit, repeatedly kill, reap, and inspect its
/// complete containment domain. Only an observed empty domain is proof.
pub(super) fn cleanup_after_lifecycle_failure(runtime: &WorkerRuntime) -> Result<()> {
    let _serialized = lock(&runtime.kill_gate)?;
    let cgroup = lock(&runtime.cgroup)?.clone();
    let deadline = Instant::now() + DESCENDANT_KILL_TIMEOUT;
    loop {
        if let Some(cgroup) = &cgroup {
            cgroup
                .kill_all_until(deadline)
                .context("kill failed lifecycle cgroup")?;
        } else {
            signal_descendants(std::process::id(), libc::SIGKILL)
                .context("kill failed lifecycle descendants")?;
        }
        reap_adopted_children().context("reap failed lifecycle descendants")?;
        if !runtime.workload_populated()? {
            if let Ok(mut state) = runtime.workload.lock() {
                state.running = false;
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out proving failed lifecycle containment empty");
        }
        thread::sleep(DESCENDANT_POLL_INTERVAL);
    }
}

pub(super) enum LifecycleWake {
    Event(LifeEvent),
    CleanupPoll,
    Disconnected,
}

/// Block indefinitely while the tracked child is still running (or while a
/// post-exit PTY is still held open by a descendant). Timed containment scans
/// are needed only after both the leader exit and PTY EOF are known: at that
/// point an adopted descendant can exit without producing another LifeEvent.
pub(super) fn wait_for_lifecycle_wake(
    rx: &mpsc::Receiver<LifeEvent>,
    cleanup_polling: bool,
) -> LifecycleWake {
    if !cleanup_polling {
        return match rx.recv() {
            Ok(event) => LifecycleWake::Event(event),
            Err(_) => LifecycleWake::Disconnected,
        };
    }
    match rx.recv_timeout(DESCENDANT_POLL_INTERVAL) {
        Ok(event) => LifecycleWake::Event(event),
        Err(mpsc::RecvTimeoutError::Timeout) => LifecycleWake::CleanupPoll,
        // Once both producer threads have ended the channel remains
        // permanently disconnected, so recv_timeout returns immediately.
        // Retain the intended cleanup cadence instead of turning that state
        // into a busy loop while adopted descendants drain.
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            thread::sleep(DESCENDANT_POLL_INTERVAL);
            LifecycleWake::CleanupPoll
        }
    }
}

/// Remove a cleanly finished session's durable state, after fencing every
/// later durable write (`WorkerRuntime::mark_finalized`): the periodic
/// flush thread and any in-flight connection keep running through the
/// connection-drain window below, and `atomic_write_*` recreates parent
/// directories, so an unfenced write would resurrect the directory with a
/// `phase: exiting` record and a dead worker pid.
///
/// A removal that could not happen (a read-only state dir, a vanished
/// mount) is reported rather than exited on silently. Nothing is lost when
/// it fails: the record left behind still says the worker was running, its
/// pid is about to be gone, and `a prune` reaps that shape -- but the
/// operator should be able to see why a session they ended is still listed.
fn remove_finished_state(runtime: &WorkerRuntime) {
    let id = match runtime.mark_finalized() {
        Ok(id) => id,
        Err(error) => {
            eprintln!("aplexer worker: fence writes before removing finished session: {error:#}");
            return;
        }
    };
    if let Err(error) = fs::remove_dir_all(runtime.paths.state_session(id)) {
        eprintln!("aplexer worker: remove finished session {id} state: {error:#}");
    }
}

pub(super) fn run_lifecycle(runtime: Arc<WorkerRuntime>, rx: mpsc::Receiver<LifeEvent>) {
    let mut pty_eof = false;
    let mut child_exit: Option<(Option<i32>, Option<i32>)> = None;
    let mut fatal: Option<String> = None;
    let mut containment_empty = false;
    loop {
        let cleanup_polling = child_exit.is_some() && pty_eof;
        match wait_for_lifecycle_wake(&rx, cleanup_polling) {
            LifecycleWake::Event(event) => match event {
                LifeEvent::PtyEof => {
                    pty_eof = true;
                    if let Ok(mut pty) = runtime.pty_write.lock() {
                        *pty = None;
                    }
                }
                LifeEvent::PtyError(message) => {
                    pty_eof = true;
                    fatal = Some(message.clone());
                    if let Ok(mut pty) = runtime.pty_write.lock() {
                        *pty = None;
                    }
                    runtime.output.fail_subscribers(message);
                }
                LifeEvent::WaiterError(message) => {
                    fatal = Some(message.clone());
                    if let Ok(mut pty) = runtime.pty_write.lock() {
                        *pty = None;
                    }
                    runtime.output.fail_subscribers(message);
                    break;
                }
                LifeEvent::ChildExit { code, signal } => {
                    child_exit = Some((code, signal));
                    // Natural exits (Ctrl-D, `exit`, a command that ran to
                    // completion, an externally signalled workload) get their
                    // exiting transition here, at the leader's death. An
                    // accepted `a kill` writes the same phase earlier, at
                    // acceptance, before teardown starts (issue #18) -- this
                    // write is then a no-op refresh that only stamps
                    // `updated_at_ms`.
                    let _ = runtime.update_record(|r| r.phase = Phase::Exiting);
                }
            },
            LifecycleWake::CleanupPoll => {}
            LifecycleWake::Disconnected => {
                fatal = Some("workload lifecycle channel disconnected".into());
                break;
            }
        }

        if child_exit.is_some() {
            if let Err(error) = reap_adopted_children() {
                fatal.get_or_insert_with(|| format!("reap descendants: {error:#}"));
            }
            match runtime.workload_populated() {
                Ok(populated) => {
                    if let Ok(mut state) = runtime.workload.lock() {
                        state.running = populated;
                    }
                    if pty_eof && !populated {
                        containment_empty = true;
                        break;
                    }
                }
                Err(error) => {
                    // Fail closed: never finalize evidence while we cannot
                    // establish that the containment domain is empty.
                    fatal.get_or_insert_with(|| format!("inspect descendants: {error:#}"));
                }
            }
        }
    }
    if !containment_empty && fatal.is_some() {
        match cleanup_after_lifecycle_failure(&runtime) {
            Ok(()) => containment_empty = true,
            Err(error) => {
                let message = format!("containment cleanup unproven: {error:#}");
                fatal = Some(match fatal {
                    Some(existing) => format!("{existing}; {message}"),
                    None => message,
                });
            }
        }
    }
    let (code, signal) = child_exit.unwrap_or((None, None));
    let (oom, mut cg) = match runtime.cgroup.lock() {
        Ok(mut g) => {
            let oom = g.as_ref().map(Cgroup::oom_killed).unwrap_or(false);
            let cg = if containment_empty { g.take() } else { None };
            (oom, cg)
        }
        Err(_) => (false, None),
    };
    let exit = ExitInfo {
        code,
        signal,
        oom_killed: oom,
        exited_at_ms: now_ms(),
    };
    // A clean, proven-empty finish leaves no record: a workload that
    // returned (zero or non-zero), Ctrl-D at a shell, a signalled workload,
    // and `a kill` are all the same path. A session that is over is gone
    // from `a list` the moment it is over -- not an `exited` tombstone that
    // sits there until somebody runs `a prune` -- and the post-mortem
    // writes that tombstone needed would be fsync-and-delete waste anyway
    // (benchmark PLAN P0.2).
    //
    // Keep the durable `finish` path only when:
    //
    //  * something failed (`fatal`: a history flush or record persist error,
    //    a PTY/waiter error) -- the record carries the reason, and nothing
    //    else would report it;
    //  * containment is not proven empty -- the record is the only remaining
    //    handle on a domain that may still hold live processes, and dropping
    //    it would strand them (`reap_verdict` / `a prune`'s bar);
    //  * the workload was OOM-killed -- `oom_killed` is a diagnosis the
    //    kernel made and the exit status alone does not carry, so it would
    //    be unrecoverable rather than merely unrecorded;
    //  * the operator asked for post-mortem records with `keep_exited = true`
    //    in the config, which restores the old `exited`-until-pruned rows.
    //
    // Read here rather than at worker startup so it costs nothing on the
    // start path and so editing the config takes effect for sessions that
    // are already running.
    let keep_exited = crate::config_keep_exited(&runtime.paths);
    let will_remove = fatal.is_none() && containment_empty && !oom && !keep_exited;
    if will_remove {
        runtime.output.finish_killed(exit.clone());
        if let Some(c) = cg.take() {
            c.cleanup();
        }
        remove_finished_state(&runtime);
    } else {
        if let Err(history_error) = runtime.output.flush_history(true) {
            let message = format!("persist final history: {history_error:#}");
            fatal = Some(match fatal {
                Some(existing) => format!("{existing}; {message}"),
                None => message,
            });
        }
        let error = fatal.clone();
        let mut record_retry = HISTORY_RETRY_INITIAL;
        loop {
            let final_error = error.clone();
            match runtime.update_record(|r| {
                r.phase = if final_error.is_some() {
                    Phase::Failed
                } else {
                    Phase::Exited
                };
                r.containment_empty = Some(containment_empty);
                r.exit = Some(exit.clone());
                r.error = final_error;
            }) {
                Ok(_) => break,
                Err(persist_error) => {
                    // Never exit with a durable record that still claims this
                    // worker/workload is running. Keep the control socket alive
                    // so Status can expose `record_persistence_error` while the
                    // lifecycle retries.
                    eprintln!(
                        "aplexer worker: persist final session state: {persist_error:#}; retrying in {}ms",
                        record_retry.as_millis()
                    );
                    thread::sleep(record_retry);
                    record_retry = record_retry.saturating_mul(2).min(HISTORY_RETRY_MAX);
                }
            }
        }
    }
    if !containment_empty {
        runtime
            .output
            // Cloned so the killed-session removal below can still ask
            // whether finalization failed; this path runs only when it did.
            .fail_subscribers(
                fatal
                    .clone()
                    .unwrap_or_else(|| "containment cleanup was not proven".into()),
            );
        // Retain the worker as the subreaper boundary, along with its socket,
        // cgroup handle, and runtime evidence. A later `a kill` can retry;
        // this monitor will finalize only after it independently observes the
        // resulting domain empty and durably records that proof.
        loop {
            if let Err(error) = reap_adopted_children() {
                eprintln!("aplexer worker: reap after lifecycle failure: {error:#}");
            }
            match runtime.workload_populated() {
                Ok(false) => {
                    if let Err(error) =
                        runtime.update_record(|record| record.containment_empty = Some(true))
                    {
                        eprintln!("aplexer worker: persist delayed containment proof: {error:#}");
                    } else {
                        cg = runtime
                            .cgroup
                            .lock()
                            .ok()
                            .and_then(|mut cgroup| cgroup.take());
                        break;
                    }
                }
                Ok(true) => {}
                Err(error) => {
                    eprintln!("aplexer worker: inspect failed lifecycle containment: {error:#}")
                }
            }
            thread::sleep(DESCENDANT_POLL_INTERVAL);
        }
    }
    // The fast path above already terminated subscribers, cleaned the cgroup,
    // and removed the state dir -- skip the durable post-mortem writes below,
    // which would just fsync-and-delete the same evidence (benchmark PLAN
    // P0.2). The `!containment_empty` recovery loop is unreachable here
    // (`will_remove` implies `containment_empty`).
    if !will_remove {
        runtime.output.finish(exit.clone());
        if let Some(cg) = cg {
            cg.cleanup();
        }
        // Failed and OOM sessions keep the terminal record, as does an
        // explicit `keep_exited = true`. A clean finish whose containment
        // proof arrived late (the recovery loop above) still removes:
        // SIGTERM-to-worker, a descendant that outlived the leader, and
        // Ctrl-D are the same "gone from `a list`" outcome as the fast
        // path. Any remaining `fatal` keeps the evidence.
        if fatal.is_none() && !oom && !keep_exited {
            remove_finished_state(&runtime);
        }
    }
    // The workload is gone and the final record/history are persisted;
    // a daemonless design must not leave a worker process (plus its
    // socket and runtime dir) behind for every session that ever ran.
    // Unlink the socket first so new clients fail fast and fall back to
    // the persisted record/history, then give in-flight connections
    // (the `kill` response, attach Exit events) a bounded window to
    // drain before exiting the process. Drained at the 5 ms kill cadence
    // (benchmark PLAN P0.2), not the 25 ms lifecycle one.
    let _ = fs::remove_file(&runtime.socket_path);
    let drain_deadline = Instant::now() + Duration::from_secs(3);
    while runtime.active_connections.load(Ordering::SeqCst) > 0 && Instant::now() < drain_deadline {
        thread::sleep(KILL_POLL_INTERVAL);
    }
    let _ = fs::remove_dir_all(&runtime.runtime_session_dir);
    std::process::exit(0);
}
