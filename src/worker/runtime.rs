//! The worker's shared runtime state: the record, the PTY write side, the
//! workload and terminal registries, and the operations every connection
//! performs against them.
//!
//! One reason to exist: every mutation of shared worker state -- a record
//! write, a PTY resize, an input send, a kill -- goes through one struct
//! whose methods say which lock each one holds and why, so a new caller
//! cannot invent a fourth lock order.

use super::*;

#[derive(Debug)]
pub(super) struct WorkloadState {
    pub(super) running: bool,
    pub(super) pgid: i32,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AttachedClient {
    pub(super) geometry: Option<(u16, u16)>,
    pub(super) last_activity: u64,
}

/// The one PTY has one size, even when several clients are attached. tmux's
/// default `window-size=latest` policy resolves that by giving the most
/// recently active sized client control of the PTY geometry. Keep the client
/// registry and the applied size behind the same mutex so two clients cannot
/// race an older resize past a newer activity event.
pub(super) struct TerminalState {
    pub(super) rows: u16,
    pub(super) cols: u16,
    pub(super) clients: HashMap<u64, AttachedClient>,
    pub(super) next_client_id: u64,
    pub(super) activity_clock: u64,
}

pub(super) struct WorkerRuntime {
    /// The session this worker serves. Immutable for the worker's life, so
    /// no caller needs to clone the whole record out of its mutex to
    /// learn it (every connection used to).
    pub(super) id: Uuid,
    pub(super) paths: Paths,
    pub(super) record_path: std::path::PathBuf,
    pub(super) runtime_session_dir: std::path::PathBuf,
    pub(super) socket_path: std::path::PathBuf,
    pub(super) record: Mutex<SessionRecord>,
    /// The PTY master's write side. `None` once the lifecycle sees EOF.
    /// Held as an `Arc` so `send` can clone the handle and write *outside*
    /// the mutex: a PTY write blocks whenever the tty input queue is full
    /// behind a stopped foreground job, and holding the lock across it
    /// used to block `Status` (foreground_command needs the fd), every
    /// resize, and the lifecycle's PtyEof handler behind one wedged client.
    pub(super) pty_write: Mutex<Option<Arc<File>>>,
    pub(super) workload: Mutex<WorkloadState>,
    pub(super) terminal: Mutex<TerminalState>,
    pub(super) cgroup: Mutex<Option<Cgroup>>,
    pub(super) kill_gate: Mutex<()>,
    pub(super) output: OutputHub,
    /// Most recent failure to durably write the session record. Kept live so
    /// status remains truthful while the lifecycle retries final evidence.
    pub(super) record_persistence_error: Mutex<Option<String>>,
    /// Connections currently being served; the lifecycle thread drains this
    /// (with a timeout) before exiting the worker so in-flight responses
    /// (e.g. the reply to the `kill` that ended the workload) are not lost.
    pub(super) active_connections: Arc<AtomicUsize>,
    /// Last PTY-output timestamp (ms since epoch), updated on every PTY read
    /// with a single relaxed atomic store -- no lock, no I/O -- so this can
    /// sit directly in the hot PTY-reader loop without reintroducing the
    /// per-read write amplification the history-persistence debounce fix
    /// (see HISTORY_FLUSH_INTERVAL) already solved once. The periodic flush
    /// thread piggybacks on that same tick to persist this into
    /// `SessionRecord::last_activity_ms`, and only when it actually changed.
    pub(super) last_activity_ms: AtomicU64,
}

impl WorkerRuntime {
    pub(super) fn record(&self) -> Result<SessionRecord> {
        Ok(lock(&self.record)?.clone())
    }
    pub(super) fn update_record<F>(&self, update: F) -> Result<SessionRecord>
    where
        F: FnOnce(&mut SessionRecord),
    {
        let mut record = lock(&self.record)?;
        // Checked under the record lock (see `OutputHub::finalized`): a
        // write that got here first has already landed before the removal,
        // and one that gets here later must not recreate the state dir.
        if self.output.finalized() {
            bail!(
                "session {} is finalized; its durable record has been removed",
                record.id
            );
        }
        // Persist a candidate before publishing it. Otherwise a failed Rename
        // can leak into live Status and an unrelated later activity write can
        // commit that rejected selector outside the registry lock.
        let mut candidate = record.clone();
        update(&mut candidate);
        candidate.updated_at_ms = now_ms();
        match atomic_write_json(&self.record_path, &candidate) {
            Ok(()) => {
                *record = candidate.clone();
                *lock(&self.record_persistence_error)? = None;
                Ok(candidate)
            }
            Err(error) => {
                *lock(&self.record_persistence_error)? = Some(format!("{error:#}"));
                Err(error)
            }
        }
    }
    /// Refuse every later durable write (see `OutputHub::finalized`). Both
    /// locks are held while the flag is set so a writer that already holds
    /// either one finishes before the flag is observed, and any later
    /// writer observes it.
    pub(super) fn mark_finalized(&self) -> Result<()> {
        let _hub = lock(&self.output.inner)?;
        let _record = lock(&self.record)?;
        self.output.finalized.store(true, Ordering::SeqCst);
        Ok(())
    }
    pub(super) fn send(&self, data: &[u8]) -> Result<()> {
        if !lock(&self.workload)?.running {
            bail!("workload has exited");
        }
        let file = lock(&self.pty_write)?
            .clone()
            .ok_or_else(|| anyhow!("PTY is closed"))?;
        // Unlocked: see `pty_write`.
        (&*file).write_all(data).context("write PTY")?;
        (&*file).flush()?;
        Ok(())
    }
    /// Apply a size while `terminal` is held. The shared state check avoids
    /// sending SIGWINCH for every keystroke from the already-active client.
    pub(super) fn apply_size(
        &self,
        terminal: &mut TerminalState,
        rows: u16,
        cols: u16,
    ) -> Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if (terminal.rows, terminal.cols) == (rows, cols) {
            return Ok(());
        }
        let pty = lock(&self.pty_write)?;
        let file = pty.as_ref().ok_or_else(|| anyhow!("PTY is closed"))?;
        let previous_size = (terminal.rows, terminal.cols);
        resize_screen_and_pty(&self.output, previous_size, (rows, cols), || {
            set_winsize(file.as_raw_fd(), rows, cols)
        })?;
        terminal.rows = rows;
        terminal.cols = cols;
        Ok(())
    }

    /// Resizes the live screen model *before* the PTY ioctl (design doc
    /// section 5.3): output already in flight at the old size is parsed at
    /// the new one -- a transient tmux shares too -- but this ordering
    /// means a subsequent attach's snapshot is never rendered against a
    /// model that's still the wrong shape for the geometry the workload was
    /// just told about.
    pub(super) fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let mut terminal = lock(&self.terminal)?;
        self.apply_size(&mut terminal, rows, cols)
    }

    pub(super) fn bump_client_activity(
        terminal: &mut TerminalState,
        client_id: u64,
    ) -> Option<(u16, u16)> {
        let geometry = terminal.clients.get(&client_id)?.geometry;
        if geometry.is_some() {
            terminal.activity_clock = terminal.activity_clock.saturating_add(1);
            if let Some(client) = terminal.clients.get_mut(&client_id) {
                client.last_activity = terminal.activity_clock;
            }
        }
        geometry
    }

    /// Registers a subscriber and renders its initial payload while holding
    /// the client-size mutex. This makes geometry selection + snapshot one
    /// indivisible operation with respect to another client's attach/input/
    /// resize, instead of allowing a concurrent client to change the model's
    /// dimensions between those two steps.
    pub(super) fn attach_client(
        &self,
        payload: AttachPayload,
        geometry: Option<(u16, u16)>,
    ) -> Result<(u64, u64, Vec<u8>, OutputReceiver)> {
        let mut terminal = lock(&self.terminal)?;
        let previous_size = (terminal.rows, terminal.cols);
        let client_id = terminal.next_client_id;
        terminal.next_client_id += 1;
        terminal.clients.insert(
            client_id,
            AttachedClient {
                geometry,
                last_activity: 0,
            },
        );
        if let Some((rows, cols)) = Self::bump_client_activity(&mut terminal, client_id) {
            // Attaching a real terminal makes it the latest active client,
            // matching tmux. Keep attach best-effort if the PTY is exiting.
            let _ = self.apply_size(&mut terminal, rows, cols);
        }
        match self.output.subscribe(payload) {
            Ok((subscription, initial, rx)) => Ok((client_id, subscription, initial, rx)),
            Err(error) => {
                terminal.clients.remove(&client_id);
                // The attach was never established, so it must not retain
                // ownership of the shared PTY geometry. No other client can
                // race us while `terminal` is held.
                let _ = self.apply_size(&mut terminal, previous_size.0, previous_size.1);
                Err(error)
            }
        }
    }

    /// Input activity transfers size ownership before the bytes reach the
    /// workload. Both operations happen under `terminal`, so another client
    /// cannot slip its resize between this client's activation and input.
    pub(super) fn send_from_client(&self, client_id: u64, data: &[u8]) -> Result<()> {
        let mut terminal = lock(&self.terminal)?;
        if let Some((rows, cols)) = Self::bump_client_activity(&mut terminal, client_id) {
            // Preserve input delivery if a closing/broken PTY rejects the
            // best-effort geometry update; `send` below remains the source
            // of truth for whether the workload can still accept input.
            let _ = self.apply_size(&mut terminal, rows, cols);
        }
        // PTY writes can block behind a stopped or backpressured workload.
        // Geometry ownership is settled above; never hold the global client
        // registry mutex while waiting for the workload to consume input.
        drop(terminal);
        self.send(data)
    }

    pub(super) fn resize_client(&self, client_id: u64, rows: u16, cols: u16) -> Result<()> {
        let (rows, cols) = screen::validate_size(rows, cols)?;
        let mut terminal = lock(&self.terminal)?;
        let previous_activity_clock = terminal.activity_clock;
        let client = terminal
            .clients
            .get_mut(&client_id)
            .ok_or_else(|| anyhow!("attached client is gone"))?;
        let previous_client = *client;
        client.geometry = Some((rows, cols));
        let (rows, cols) = Self::bump_client_activity(&mut terminal, client_id)
            .ok_or_else(|| anyhow!("attached client has no geometry"))?;
        if let Err(error) = self.apply_size(&mut terminal, rows, cols) {
            terminal.activity_clock = previous_activity_clock;
            if let Some(client) = terminal.clients.get_mut(&client_id) {
                *client = previous_client;
            }
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn signal_from_client(&self, client_id: u64, signal: i32) -> Result<()> {
        let mut terminal = lock(&self.terminal)?;
        if let Some((rows, cols)) = Self::bump_client_activity(&mut terminal, client_id) {
            let _ = self.apply_size(&mut terminal, rows, cols);
        }
        self.signal(signal)
    }

    /// If the latest client leaves, fall back to the most recently active
    /// remaining sized client. If no sized client remains, keep the current
    /// PTY size, as tmux does for a detached window.
    pub(super) fn detach_client(&self, client_id: u64) {
        let Ok(mut terminal) = self.terminal.lock() else {
            return;
        };
        let latest_before = terminal
            .clients
            .iter()
            .filter(|(_, client)| client.geometry.is_some())
            .max_by_key(|(_, client)| client.last_activity)
            .map(|(&id, _)| id);
        terminal.clients.remove(&client_id);
        if latest_before != Some(client_id) {
            return;
        }
        let fallback = terminal
            .clients
            .values()
            .filter_map(|client| {
                client
                    .geometry
                    .map(|geometry| (client.last_activity, geometry))
            })
            .max_by_key(|(activity, _)| *activity)
            .map(|(_, geometry)| geometry);
        if let Some((rows, cols)) = fallback {
            let _ = self.apply_size(&mut terminal, rows, cols);
        }
    }
    pub(super) fn signal(&self, signal: i32) -> Result<()> {
        let workload = lock(&self.workload)?;
        if !workload.running {
            bail!("workload has exited");
        }
        if unsafe { libc::kill(-workload.pgid, signal) } != 0 {
            return Err(io::Error::last_os_error()).context("signal process group");
        }
        Ok(())
    }
    pub(super) fn kill(&self, signal: i32, grace_ms: u64) -> Result<()> {
        let grace = kill_grace_duration(grace_ms)?;
        let _serialized = lock(&self.kill_gate)?;
        if !self.workload_populated()? {
            // Already empty: no teardown will start from this call, and the
            // lifecycle thread owns the record from here (its ChildExit
            // handler writes `Exiting`, issue #18). Writing again here would
            // race that finalization's record removal and could resurrect a
            // removed state dir (atomic_write_json recreates parents).
            return Ok(());
        }
        // The kill is accepted and teardown is about to start: persist the
        // transition BEFORE signalling (issue #18). Until now the durable
        // record kept its pre-kill phase, so for the whole window between
        // the accepted Kill RPC and finalization's record removal -- bounded
        // by the CLI's KILL_RECORD_REMOVAL_WAIT, indefinite if finalization
        // wedges -- `a snapshot` reported the dying session byte-for-byte
        // like a healthy one. Writing under the same record mutex
        // `update_record` holds, and strictly before the signal that leads
        // to the ChildExit -> finalization sequence, this write is always
        // first: the removal that follows can only delete it, never be
        // preceded by it. Best-effort on purpose -- a failed write must not
        // stop a kill or flip the accepted RPC to an error; the next writer
        // (ChildExit handler, finalization) retries the transition or
        // removes the record outright, and `record_persistence_error` keeps
        // surfacing the failure meanwhile. The termination monitor's
        // SIGTERM teardown shares this method, which is the same dying
        // fact about that session and gets the same honest record.
        if let Err(error) = self.update_record(|record| record.phase = Phase::Exiting) {
            eprintln!("aplexer worker: mark accepted-kill session exiting: {error:#}");
        }
        let grace_deadline = Instant::now()
            .checked_add(grace)
            .ok_or_else(|| anyhow!("kill grace deadline overflow"))?;
        let cleanup_deadline = grace_deadline
            .checked_add(DESCENDANT_KILL_TIMEOUT)
            .ok_or_else(|| anyhow!("kill cleanup deadline overflow"))?;
        let cgroup = lock(&self.cgroup)?.clone();
        if signal == libc::SIGKILL {
            if let Some(cg) = &cgroup {
                cg.kill_all_until(cleanup_deadline)?;
            } else {
                kill_descendants(std::process::id(), DESCENDANT_KILL_TIMEOUT)?;
            }
            return Ok(());
        }
        if let Some(cg) = &cgroup {
            cg.signal_all_until(signal, cleanup_deadline)?;
        } else {
            signal_descendants(std::process::id(), signal)?;
        }
        // Poll instead of sleeping the whole grace period: once the workload
        // is gone there is nothing to escalate to SIGKILL, and the response
        // to this request should not be delayed (the worker exits shortly
        // after the workload does, so a response stuck behind a long sleep
        // could be lost entirely). Polled at KILL_POLL_INTERVAL (5 ms), not
        // the 25 ms lifecycle cadence, so a workload that dies on the first
        // signal does not pay a quantization delay (benchmark PLAN P0.2).
        while self.workload_still_populated()? && Instant::now() < grace_deadline {
            thread::sleep(KILL_POLL_INTERVAL);
        }
        if self.workload_populated()? {
            if let Some(cg) = &cgroup {
                cg.kill_all_until(cleanup_deadline)?;
            } else {
                kill_descendants(std::process::id(), DESCENDANT_KILL_TIMEOUT)?;
            }
        }
        Ok(())
    }

    /// `workload_populated` for a poll loop: a process group that still
    /// answers `kill(-pgid, 0)` is populated without walking `/proc` at all.
    /// The 5 ms kill poll used to do a full descendant walk on every tick
    /// for an unlimited session -- dozens of `/proc/*/task/*/children`
    /// reads per tick while a workload ran out its grace window. Only a
    /// "yes" is taken from the probe: an empty group still needs the walk,
    /// because a `setsid` descendant leaves the group without leaving the
    /// domain. A zombie member keeps the group signalable until its parent
    /// (this worker's reaper thread, or the waiter for the leader) reaps
    /// it, which is immediate, so the answer is at most one poll late.
    pub(super) fn workload_still_populated(&self) -> Result<bool> {
        let signalable = {
            let workload = lock(&self.workload)?;
            workload.running && unsafe { libc::kill(-workload.pgid, 0) } == 0
        };
        if signalable {
            return Ok(true);
        }
        self.workload_populated()
    }

    /// Whether any process remains inside this session's containment domain.
    /// A leader exiting is not sufficient: a `setsid` descendant may have
    /// escaped the leader's process group while still belonging to the
    /// session. Limited sessions use the kernel's cgroup membership; ordinary
    /// sessions use the worker's subreaper descendant tree.
    pub(super) fn workload_populated(&self) -> Result<bool> {
        if let Some(cgroup) = lock(&self.cgroup)?.as_ref() {
            return cgroup.populated();
        }
        Ok(!descendant_pids(std::process::id())?.is_empty())
    }
    /// Rename this session within its workspace (or into a new one).
    ///
    /// The `workspace+tag` claim check answers the same question
    /// `start_session`'s supersede check answers -- "does any record still
    /// own this pair?" -- so it applies the same predicate, `reap_verdict`,
    /// and no third copy (issue #13). `rename` used to refuse on *any*
    /// conflicting record, dead or not, while `a start` reclaimed a pair
    /// held by a dead one: on the same box, the same dead record made one
    /// command succeed and the other fail with "already belongs to session
    /// <uuid>", naming a session `a list` showed as broken and nothing
    /// could attach to.
    ///
    /// What a dead conflict gets from rename is deliberately *not* what
    /// `a start` does to it: reclaiming archives and then deletes the
    /// holder's durable state, and rename is a metadata edit that must
    /// destroy nothing. So rename skips the dead holder and leaves its
    /// record for `a prune`, the routine cleaner of exactly this class.
    /// A live holder keeps its claim, and the refusal names its derived
    /// state and a next step, in `a start`'s own words.
    ///
    /// A pre-PID `Starting` holder is the one "dead"-looking shape that
    /// must still refuse (issue #9): a stub in the spawn-to-worker-lock gap
    /// is a healthy session coming up. Its worker lock is both the detector
    /// and the fence -- held means the session is very much coming up
    /// (refuse), acquired means nothing is behind the stub, and holding the
    /// fence across the update below keeps a worker that has not reached
    /// its acquisition yet from coming up on top of the pair this rename
    /// just handed out.
    pub(super) fn rename(
        &self,
        workspace: std::path::PathBuf,
        tag: String,
    ) -> Result<SessionRecord> {
        validate_tag(&tag)?;
        let workspace = canonical_workspace(&workspace)?;
        let _registry = registry_lock_within(&self.paths, RENAME_REGISTRY_WAIT)?;
        let conflicts: Vec<SessionRecord> = list_records(&self.paths)?
            .into_iter()
            .filter(|record| {
                record.id != self.id && record.workspace == workspace && record.tag == tag
            })
            .collect();
        // Every conflict must be reclaimable-dead, or the rename refuses:
        // a live holder -- live worker, live workload leader, or a
        // containment domain that still holds something -- owns its pair,
        // exactly as against `a start`.
        if let Some(live) = conflicts
            .iter()
            .find(|record| reap_verdict(record).is_none())
        {
            bail!(
                "workspace+tag already belongs to session {} (state: {}); rename it or choose a different tag",
                live.id,
                live.observed_state()
            );
        }
        // All conflicts are dead by the same verdict `a start` reclaims on.
        // Fence each pre-PID stub before skipping it, and hold the fences
        // across the update: the skip is only safe while nothing can come
        // up behind the stub.
        let _fences = conflicts
            .iter()
            .map(|record| {
                fence_or_refuse(&self.paths, record).with_context(|| {
                    format!(
                        "workspace+tag already belongs to session {}; rename it or choose a different tag",
                        record.id
                    )
                })
            })
            .collect::<Result<Vec<Option<FileLock>>>>()?;
        self.update_record(|r| {
            r.workspace = workspace;
            r.tag = tag;
        })
    }
    /// `a state-report <state>` (docs/pocketshell-integration-plan.md Open
    /// question #2): a hook running inside this session pushes its own
    /// semantic state. Validated here (not just at the CLI's `ValueEnum`
    /// layer) so a direct/malformed RPC from any caller can't write an
    /// unrecognised value into the record that `watch.rs`'s merge logic
    /// would then have to guess at -- the same defensive posture `rename`
    /// takes with `validate_tag` above.
    pub(super) fn report_state(&self, state: String) -> Result<SessionRecord> {
        validate_reported_state(&state)?;
        self.update_record(move |r| {
            r.reported_state = Some(state);
            r.reported_state_at_ms = Some(now_ms());
        })
    }
}

/// Take the registry lock without blocking past `wait` (see
/// `RENAME_REGISTRY_WAIT`). A lock still held at the deadline is reported
/// as a distinct, retryable "registry is busy" error rather than as a
/// failed rename.
pub(super) fn registry_lock_within(paths: &Paths, wait: Duration) -> Result<FileLock> {
    let deadline = Instant::now() + wait;
    loop {
        match FileLock::exclusive(&paths.registry_lock(), true) {
            Ok(lock) => return Ok(lock),
            Err(error) if io_kind(&error) == Some(io::ErrorKind::WouldBlock) => {
                if Instant::now() >= deadline {
                    bail!(
                        "registry is busy (another aplexer command holds {}); retry the rename",
                        paths.registry_lock().display()
                    );
                }
                thread::sleep(DESCENDANT_POLL_INTERVAL);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Keep the rendered model and kernel PTY geometry transactional. The model
/// must be resized first so concurrent output is parsed at the intended new
/// dimensions, but a rejected ioctl must not leave future snapshots claiming
/// a size the workload never received.
pub(super) fn resize_screen_and_pty(
    output: &OutputHub,
    previous_size: (u16, u16),
    new_size: (u16, u16),
    resize_pty: impl FnOnce() -> Result<()>,
) -> Result<()> {
    output.set_size(new_size.0, new_size.1)?;
    if let Err(error) = resize_pty() {
        if let Err(rollback_error) = output.set_size(previous_size.0, previous_size.1) {
            eprintln!(
                "aplexer worker: roll back screen after PTY resize failure: {rollback_error:#}"
            );
        }
        return Err(error);
    }
    Ok(())
}

/// The worker's lock-poisoning policy for shared state: fail the operation.
///
/// A poisoned mutex means a thread panicked while the state was
/// mid-update, so the record, the PTY handle, the client registry, the
/// hub's history-and-screen model, or the kill gate may be inconsistent;
/// every caller propagates the error (an RPC answers with it, a background
/// thread logs it) rather than acting on state it cannot trust. The one
/// deliberate exception is the per-subscriber queue in `hub.rs`
/// (`SubscriberShared::poisoned_lock`): that state belongs to exactly one
/// attached client, its worst inconsistency is a dropped chunk for that
/// client, and a panic on one client's writer thread must not take every
/// other client's queue -- or the PTY reader that fans out to them -- down
/// with it.
pub(super) fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| anyhow!("worker lock poisoned"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::hub::tests::test_hub;

    #[test]
    pub(super) fn failed_pty_resize_restores_the_previous_screen_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let before = hub.screen_snapshot().unwrap();

        let error = resize_screen_and_pty(&hub, (24, 80), (10, 20), || {
            bail!("injected PTY ioctl failure")
        })
        .unwrap_err();

        assert_eq!(error.to_string(), "injected PTY ioctl failure");
        assert_eq!(
            hub.screen_snapshot().unwrap(),
            before,
            "failed PTY resize left the screen model at the rejected size"
        );
    }

    /// A `WorkerRuntime` over a throwaway hub, with its durable record at
    /// `record_path` and every other path under `dir`.
    pub(super) fn test_runtime(
        dir: &tempfile::TempDir,
        record_path: std::path::PathBuf,
    ) -> WorkerRuntime {
        let mut record = SessionRecord::fixture(dir.path(), "before");
        record.socket_path = dir.path().join("control.sock");
        record.history_path = dir.path().join("history.bin");
        WorkerRuntime {
            id: record.id,
            paths: Paths {
                runtime_root: dir.path().join("runtime"),
                state_root: dir.path().join("state"),
                config_file: dir.path().join("config.toml"),
            },
            record_path,
            runtime_session_dir: dir.path().join("runtime-session"),
            socket_path: dir.path().join("control.sock"),
            record: Mutex::new(record),
            pty_write: Mutex::new(Some(Arc::new(File::open("/dev/null").unwrap()))),
            workload: Mutex::new(WorkloadState {
                running: true,
                pgid: 1,
            }),
            terminal: Mutex::new(TerminalState {
                rows: 24,
                cols: 80,
                clients: HashMap::new(),
                next_client_id: 1,
                activity_clock: 0,
            }),
            cgroup: Mutex::new(None),
            kill_gate: Mutex::new(()),
            output: test_hub(dir),
            record_persistence_error: Mutex::new(None),
            active_connections: Arc::new(AtomicUsize::new(0)),
            last_activity_ms: AtomicU64::new(0),
        }
    }

    #[test]
    pub(super) fn failed_record_persistence_does_not_publish_and_idle_activity_retries() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = dir.path().join("session.json");
        // Atomic rename onto a directory deterministically fails after the
        // candidate was serialized, exercising the publish boundary.
        fs::create_dir(&record_path).unwrap();
        let runtime = test_runtime(&dir, record_path);

        assert!(runtime
            .update_record(|candidate| candidate.tag = "after".into())
            .is_err());
        assert_eq!(runtime.record().unwrap().tag, "before");
        assert!(runtime.record_persistence_error.lock().unwrap().is_some());

        runtime.last_activity_ms.store(123, Ordering::Relaxed);
        let mut persisted_activity_ms = 0;
        assert!(persist_activity_checkpoint(&runtime, &mut persisted_activity_ms).is_err());
        assert_eq!(persisted_activity_ms, 0, "failed write advanced checkpoint");
        assert_eq!(runtime.record().unwrap().last_activity_ms, None);

        // No new activity occurs between attempts. Once the transient
        // destination failure is removed, the unchanged timestamp must still
        // be retried and published by the next tick.
        fs::remove_dir(&runtime.record_path).unwrap();
        persist_activity_checkpoint(&runtime, &mut persisted_activity_ms).unwrap();
        assert_eq!(persisted_activity_ms, 123);
        assert_eq!(runtime.record().unwrap().last_activity_ms, Some(123));
        assert!(runtime.record_persistence_error.lock().unwrap().is_none());
    }

    /// The clean-exit resurrection: `run_lifecycle` removes the state dir,
    /// then drains connections for up to 3 s before exiting, and in that
    /// window the periodic flush thread and a late attach both wrote into
    /// the removed directory (`atomic_write_*` recreates parents), leaving
    /// a `phase: exiting` record with a dead worker pid for `a list` to
    /// show as broken until `a prune`. Pins that once the lifecycle marks
    /// the session finalized, neither the record writer nor the history
    /// flusher recreates anything -- whether the write is an activity
    /// checkpoint, an attach stamp, or a forced flush.
    #[test]
    pub(super) fn finalized_session_refuses_every_later_durable_write() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state-session");
        let runtime = test_runtime(&dir, state_dir.join("session.json"));
        // Sanity: before finalization the same writers land on disk.
        runtime
            .update_record(|record| record.tag = "live".into())
            .expect("record write before finalization");
        runtime.output.append(b"output").unwrap();
        runtime.output.flush_history(true).unwrap();
        assert!(runtime.record_path.exists());
        assert!(dir.path().join("history.bin").exists());

        runtime.mark_finalized().unwrap();
        fs::remove_dir_all(&state_dir).unwrap();
        fs::remove_file(dir.path().join("history.bin")).unwrap();

        let error = runtime
            .update_record(|record| record.last_accessed_ms = Some(now_ms()))
            .expect_err("a finalized session must refuse record writes");
        assert!(format!("{error:#}").contains("finalized"), "{error:#}");
        assert!(
            runtime.record_persistence_error.lock().unwrap().is_none(),
            "a refused post-finalization write is not a persistence failure"
        );
        runtime.last_activity_ms.store(now_ms(), Ordering::Relaxed);
        let mut persisted_activity_ms = 0;
        persist_activity_checkpoint(&runtime, &mut persisted_activity_ms)
            .expect("the activity checkpoint quietly skips a finalized session");
        runtime.output.append(b"late output").unwrap();
        runtime.output.flush_history(false).unwrap();
        runtime.output.flush_history(true).unwrap();

        assert!(
            !state_dir.exists(),
            "a durable write after finalization resurrected the state dir"
        );
        assert!(
            !dir.path().join("history.bin").exists(),
            "a history flush after finalization resurrected the history file"
        );
    }

    /// `a start` holds the registry lock for its whole spawn-and-poll; a
    /// rename that blocked on it outlived the client's control deadline
    /// and then applied unobserved. It must refuse instead, in time, with
    /// an error that says the registry is busy and nothing changed.
    #[test]
    pub(super) fn rename_refuses_in_time_while_the_registry_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("session.json"));
        runtime.paths.ensure().unwrap();
        let _held = FileLock::exclusive(&runtime.paths.registry_lock(), true).unwrap();

        let started = Instant::now();
        let error = runtime
            .rename(dir.path().to_path_buf(), "renamed".into())
            .expect_err("rename must not wait out a held registry lock");
        let elapsed = started.elapsed();
        assert!(
            format!("{error:#}").contains("registry is busy"),
            "{error:#}"
        );
        assert!(
            elapsed >= RENAME_REGISTRY_WAIT
                && elapsed < RENAME_REGISTRY_WAIT + Duration::from_secs(1),
            "rename gave up after {elapsed:?}, expected about {RENAME_REGISTRY_WAIT:?}"
        );
        assert_eq!(runtime.record().unwrap().tag, "before");
        assert!(
            !runtime.record_path.exists(),
            "a refused rename wrote the record"
        );
    }
}
