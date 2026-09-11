use super::*;

pub(crate) fn remove_session_state(paths: &Paths, id: Uuid) -> Result<()> {
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    fs::remove_dir_all(paths.state_session(id))?;
    let _ = fs::remove_dir_all(paths.runtime_session(id));
    Ok(())
}

/// How long `cmd_kill` waits, after an accepted kill RPC, for the worker to
/// remove the killed session's durable record itself. Normal finalization
/// lands within milliseconds (bounded above by the worker's attach-drain
/// window), so this is a settling pause, not a retry campaign; the deadline
/// only bounds the pathological cases, which are reported, never looped on.
pub(crate) const KILL_RECORD_REMOVAL_WAIT: Duration = Duration::from_secs(5);

/// Outcome of waiting for a killed session's record to disappear. The
/// worker that accepted the kill RPC removes the record during
/// finalization, but only when finalization ran clean and proved the
/// containment domain empty -- so `Kept` (worker exited, record stayed)
/// means the worker had something to say about this exit, and `Pending`
/// (worker still alive at the deadline) means the removal is still in
/// flight or the worker is holding the evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KillRecordOutcome {
    Removed,
    Kept,
    Pending,
}

/// After an accepted kill RPC, watch the record directory until the worker
/// deletes it (or until [`KILL_RECORD_REMOVAL_WAIT`] runs out). Only the
/// worker may remove a record whose worker process is still finishing --
/// deleting it client-side would race the worker's own final record write
/// into a persist-error retry loop -- so this observes instead of acting.
pub(crate) fn wait_for_kill_record_removal(paths: &Paths, id: Uuid) -> KillRecordOutcome {
    let deadline = Instant::now() + KILL_RECORD_REMOVAL_WAIT;
    // Polled at 5 ms, not 25 ms: the worker's fast-path finalization for an
    // accepted kill (benchmark PLAN P0.2) removes the record in tens of
    // milliseconds, so a 25 ms quantum here is a large fraction of the whole
    // `a kill` latency. Short-lived and infrequent -- one wait per kill.
    while Instant::now() < deadline {
        if !paths.record(id).exists() {
            return KillRecordOutcome::Removed;
        }
        thread::sleep(Duration::from_millis(5));
    }
    match read_record(&paths.record(id)) {
        Ok(record) if record.worker_alive() => KillRecordOutcome::Pending,
        _ => KillRecordOutcome::Kept,
    }
}

/// A worker pid may still exist even though its control socket is gone.
/// Only this one rare case counts as "force-cleanable": a live, reachable
/// worker can also fail an RPC, but then it must not be signalled directly.
/// ESRCH is success because the process may exit between checks.
pub(crate) fn force_kill_stale_worker(record: &SessionRecord) -> Result<()> {
    signal_recorded_worker(record, libc::SIGKILL).context("force-kill unreachable worker")
}

/// The kill RPC did not reach a working worker. Classify why, then take
/// exactly one of four client-side paths: return transient failures, stop
/// a provably-unreachable worker, finalize a broken workload, or remove a
/// record whose worker already finished.
fn handle_failed_kill_rpc(
    paths: &Paths,
    record: &SessionRecord,
    error: anyhow::Error,
    signal: i32,
    grace_ms: u64,
    json_output: bool,
) -> Result<()> {
    let worker_alive = record.worker_alive();
    // A missing socket file, or a leftover socket with no listener
    // (SIGKILL leaves the file; connect then fails with
    // ConnectionRefused), proves an "alive" pid is unreachable. A
    // mere RPC timeout/reset can be transient, so those still return.
    let socket_missing = worker_alive && !record.socket_path.exists();
    let stale_socket = error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|cause| cause.kind() == io::ErrorKind::ConnectionRefused)
    });
    if worker_alive && !socket_missing && !stale_socket {
        return Err(error);
    }
    if socket_missing || stale_socket {
        return stop_unreachable_worker(paths, record, signal, grace_ms, json_output);
    }
    if !record.worker_finished() {
        return finalize_broken_workload(paths, record, signal, grace_ms, json_output);
    }
    remove_finished_record(paths, record, signal, grace_ms, json_output)
}

/// The worker pid is provably unreachable (socket gone or refusing):
/// stop it directly, then either remove the state (containment already
/// proven empty) or recover the containment client-side and mark the
/// record as the evidence.
fn stop_unreachable_worker(
    paths: &Paths,
    record: &SessionRecord,
    signal: i32,
    grace_ms: u64,
    json_output: bool,
) -> Result<()> {
    preflight_broken_containment_recovery(record)?;
    force_kill_stale_worker(record)?;
    if record.containment_proven_empty() {
        remove_session_state(paths, record.id)
            .with_context(|| format!("remove stale session {}", record.id))?;
        eprintln!(
            "a: removed session {} after stopping unreachable worker pid {}",
            record.id,
            record.worker_pid.unwrap_or(0),
        );
    } else {
        recover_broken_containment(record, signal, grace_ms)?;
        mark_broken_workload_killed(paths, record)?;
        eprintln!(
            "a: killed session {} (worker pid {} was unreachable; containment cleanup confirmed)",
            record.id,
            record.worker_pid.unwrap_or(0),
        );
    }
    if json_output {
        println!("{}", json!({"id":record.id,"signal":signal}));
    }
    Ok(())
}

/// The worker is gone but never recorded the workload's exit: recover the
/// containment client-side and mark the record Failed as evidence.
fn finalize_broken_workload(
    paths: &Paths,
    record: &SessionRecord,
    signal: i32,
    grace_ms: u64,
    json_output: bool,
) -> Result<()> {
    recover_broken_containment(record, signal, grace_ms)?;
    mark_broken_workload_killed(paths, record)?;
    // That finalization was client-side and deliberately kept the
    // evidence for a broken workload; the worker is already gone,
    // so there is no worker-side removal to wait for below.
    if json_output {
        println!(
            "{}",
            json!({"id":record.id,"signal":signal,"record_removed":false})
        );
    }
    Ok(())
}

/// The record already says the worker finished: recover any missing
/// containment proof, then remove the durable state.
fn remove_finished_record(
    paths: &Paths,
    record: &SessionRecord,
    signal: i32,
    grace_ms: u64,
    json_output: bool,
) -> Result<()> {
    if !record.containment_proven_empty() {
        recover_broken_containment(record, signal, grace_ms)?;
    }
    remove_session_state(paths, record.id)
        .with_context(|| format!("remove finished session {}", record.id))?;
    eprintln!("a: removed {} session {}", record.phase.name(), record.id);
    if json_output {
        println!(
            "{}",
            json!({"id":record.id,"signal":signal,"record_removed":true})
        );
    }
    Ok(())
}

/// The RPC was accepted, so the worker removes the record itself during
/// finalization. Give it a moment so `a kill` returns with the session
/// already gone from `a list` (a client that kills-then-lists must never
/// observe the exited corpse the old behavior left behind), and say so
/// plainly on the two outcomes where the record is still there.
fn report_kill_outcome(paths: &Paths, record: &SessionRecord, signal: i32, json_output: bool) {
    let removed = wait_for_kill_record_removal(paths, record.id);
    match removed {
        KillRecordOutcome::Removed => {}
        KillRecordOutcome::Kept => eprintln!(
            "a: killed session {}, but its worker kept the record (a finalize failure worth inspecting: `a status {}`)",
            record.id, record.id
        ),
        KillRecordOutcome::Pending => eprintln!(
            "a: killed session {}; its worker is still finalizing, the record disappears on its own unless the worker failed",
            record.id
        ),
    }
    if json_output {
        println!(
            "{}",
            json!({
                "id": record.id,
                "signal": signal,
                "record_removed": removed == KillRecordOutcome::Removed,
            })
        );
    }
}

pub(crate) fn cmd_kill(paths: &Paths, args: KillArgs, json_output: bool) -> Result<()> {
    let record = resolve(paths, &args.target)?;
    let signal = parse_signal(&args.signal)?;
    let grace = kill_grace_duration(args.grace_ms)?;
    let rpc = rpc_call_within(
        &record,
        Operation::Kill {
            signal,
            grace_ms: args.grace_ms,
        },
        None,
        aplexer::api::kill_response_timeout(grace),
    )
    .map(|(_, result)| result);
    if let Err(error) = rpc {
        return handle_failed_kill_rpc(paths, &record, error, signal, args.grace_ms, json_output);
    }
    report_kill_outcome(paths, &record, signal, json_output);
    Ok(())
}

pub(crate) fn preflight_broken_containment_recovery(record: &SessionRecord) -> Result<()> {
    if record.containment_proven_empty() {
        return Ok(());
    }
    let Some(locator) = record.containment_cgroup.as_deref() else {
        bail!(
            "session {} has no authoritative containment locator; refusing to stop its worker or remove runtime evidence",
            record.id
        );
    };
    validate_recorded_cgroup_locator(
        record.id,
        locator,
        record.containment_cgroup_identity.as_ref(),
    )
    .context("validate recorded cgroup before stopping unreachable worker")
}

/// Record that the client killed an orphaned workload after its worker died.
pub(crate) fn mark_broken_workload_killed(paths: &Paths, record: &SessionRecord) -> Result<()> {
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    let mut current = read_record(&paths.record(record.id)).unwrap_or_else(|_| record.clone());
    current.phase = Phase::Failed;
    current.containment_empty = Some(true);
    current.error =
        Some("worker died without recording workload exit; workload killed by `a kill`".into());
    current.updated_at_ms = now_ms();
    atomic_write_json(&paths.record(record.id), &current)?;
    let _ = fs::remove_dir_all(paths.runtime_session(record.id));
    Ok(())
}

/// Recover a session whose worker can no longer perform containment cleanup.
/// A leader PID or process group is intentionally insufficient: a workload
/// may daemonize through `setsid`, and after the subreaper worker dies there
/// is no complete process-tree root left to inspect. Resource-limited
/// sessions retain an authoritative cgroup locator; every other broken
/// session is preserved for manual investigation rather than reporting a
/// false cleanup success.
///
/// That preservation is `a kill`'s rule and is unchanged. It is NOT a
/// promise that the record survives forever: once the workload leader is
/// also gone, `a prune` reaps such a record on the grounds that no
/// programmatic handle to a survivor remains (see
/// `aplexer::containment_reap_verdict`, and
/// `tests/prune_dead_records.rs::prune_reaps_a_record_whose_setsid_descendant_escaped`,
/// which pins the case where an escaped `setsid` descendant outlives the
/// reap). `a kill` never does that: it still refuses, and still preserves
/// both directories, because unlike prune it would be claiming a cleanup.
pub(crate) fn recover_broken_containment(
    record: &SessionRecord,
    signal: i32,
    grace_ms: u64,
) -> Result<()> {
    let grace = kill_grace_duration(grace_ms)?;
    if record.containment_proven_empty() {
        return Ok(());
    }
    let Some(locator) = record.containment_cgroup.as_deref() else {
        bail!(
            "session {} has no authoritative containment locator; refusing to claim cleanup or remove its runtime evidence",
            record.id
        );
    };
    cleanup_recorded_cgroup(
        record.id,
        locator,
        record.containment_cgroup_identity.as_ref(),
        signal,
        grace,
    )
    .context("recover recorded cgroup containment")
}
