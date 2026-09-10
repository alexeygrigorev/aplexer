//! Bringing the worker process itself up: the startup checkpoints the
//! test hooks drive, the guard that rolls every created resource back if
//! bring-up fails, the gate that holds runtime threads until the Running
//! record is committed, and the one-shot launch environment.
//!
//! One reason to exist: a worker that fails partway through startup must
//! leave a truthful `Failed` record and nothing else behind, whichever
//! step failed.

use super::*;

pub(super) fn startup_checkpoint(point: &str) -> Result<()> {
    if TERMINATION_REQUESTED.load(Ordering::SeqCst) {
        bail!("worker startup cancelled by termination signal");
    }
    #[cfg(feature = "startup-test-hooks")]
    if let Ok(spec) = env::var("APLEXER_TEST_EXIT_WORKER_AT") {
        // "<checkpoint>:<exit status>". Unlike the failure hook below this
        // leaves through `process::exit`, so the worker's own StartupGuard
        // never runs and the durable record keeps whatever non-terminal
        // phase it had. That is the only way to build the two shapes the
        // API's "worker exited during startup" handling must still reject:
        // a worker gone with no terminal record at all, and one gone
        // cleanly (status 0) that never recorded an exit.
        if let Some((target, status)) = spec.split_once(':') {
            if target == point {
                std::process::exit(status.parse().unwrap_or(1));
            }
        }
    }
    #[cfg(feature = "startup-test-hooks")]
    if env::var("APLEXER_TEST_FAIL_WORKER_STARTUP_AT").as_deref() == Ok(point) {
        bail!("injected worker startup failure at {point}");
    }
    #[cfg(not(feature = "startup-test-hooks"))]
    let _ = point;
    Ok(())
}

pub(super) fn after_workload_spawn_checkpoint(pid: u32) -> Result<()> {
    #[cfg(feature = "startup-test-hooks")]
    if let Some(marker) = env::var_os("APLEXER_TEST_WORKER_STARTUP_MARKER") {
        atomic_write_bytes(std::path::Path::new(&marker), pid.to_string().as_bytes())
            .context("write worker startup test marker")?;
    }
    #[cfg(feature = "startup-test-hooks")]
    if env::var("APLEXER_TEST_HANG_WORKER_STARTUP_AT").as_deref() == Ok("after_workload_spawn") {
        // Deliberately ignore TERMINATION_REQUESTED. The non-default Cargo
        // feature is the authorization boundary for this destructive hook;
        // default and release builds do not contain the hang path.
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    #[cfg(feature = "startup-test-hooks")]
    if env::var("APLEXER_TEST_PAUSE_WORKER_STARTUP_AT").as_deref() == Ok("after_workload_spawn") {
        wait_for_termination_request()?;
    }
    #[cfg(not(feature = "startup-test-hooks"))]
    let _ = pid;
    startup_checkpoint("after_workload_spawn")
}
/// Owns every resource created before the worker's accept loop is committed.
/// Drop is a last-resort rollback; normal error paths call `rollback` so the
/// persisted failure contains the original error rather than a generic one.
pub(super) struct StartupGuard {
    pub(super) armed: bool,
    pub(super) record_path: std::path::PathBuf,
    pub(super) runtime_session_dir: std::path::PathBuf,
    pub(super) socket_path: std::path::PathBuf,
    pub(super) failure_record: SessionRecord,
    pub(super) cgroup: Option<Cgroup>,
    pub(super) cgroup_setup_started: bool,
    pub(super) child: Option<Arc<Mutex<Option<Child>>>>,
}

impl StartupGuard {
    pub(super) fn new(paths: &Paths, record: &SessionRecord) -> Self {
        Self {
            armed: true,
            record_path: paths.record(record.id),
            runtime_session_dir: paths.runtime_session(record.id),
            socket_path: paths.socket(record.id),
            failure_record: record.clone(),
            cgroup: None,
            cgroup_setup_started: false,
            child: None,
        }
    }

    pub(super) fn rollback(&mut self, error: &anyhow::Error) {
        self.cleanup(format!("{error:#}"));
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
        self.child = None;
        self.cgroup = None;
    }

    pub(super) fn cleanup(&mut self, message: String) {
        if !self.armed {
            return;
        }
        self.armed = false;

        let mut cleanup_failures = Vec::new();
        let deadline = Instant::now() + DESCENDANT_KILL_TIMEOUT;
        if let Some(cgroup) = &self.cgroup {
            if let Err(error) = cgroup.kill_all_until(deadline) {
                cleanup_failures.push(format!("kill startup cgroup: {error:#}"));
            }
        } else if let Err(error) = signal_descendants(std::process::id(), libc::SIGKILL) {
            cleanup_failures.push(format!("kill startup descendants: {error:#}"));
        }

        if let Some(slot) = &self.child {
            match slot.lock() {
                Ok(mut slot) => {
                    if let Some(mut child) = slot.take() {
                        if let Err(error) = child.kill() {
                            if error.kind() != io::ErrorKind::InvalidInput {
                                cleanup_failures
                                    .push(format!("kill startup workload leader: {error}"));
                            }
                        }
                        if let Err(error) = child.wait() {
                            cleanup_failures.push(format!("reap startup workload leader: {error}"));
                        }
                    }
                }
                Err(_) => cleanup_failures.push("startup child lock poisoned".into()),
            }
        }

        // Once the tracked leader has been waited, every remaining process
        // is an adopted child and may safely be reaped while the domain is
        // killed again until it is observed empty.
        if let Err(error) = kill_until_empty(self.cgroup.as_ref(), deadline) {
            cleanup_failures.push(format!("prove startup containment empty: {error:#}"));
        }
        if self.cgroup_setup_started && self.cgroup.is_none() {
            cleanup_failures.push(
                "cgroup setup spawned a helper but no authoritative locator was recorded".into(),
            );
        }

        self.failure_record.phase = Phase::Failed;
        self.failure_record.containment_empty = Some(cleanup_failures.is_empty());
        self.failure_record.error = Some(if cleanup_failures.is_empty() {
            message
        } else {
            format!(
                "{message}; containment cleanup unproven: {}",
                cleanup_failures.join("; ")
            )
        });
        self.failure_record.updated_at_ms = now_ms();
        match atomic_write_json(&self.record_path, &self.failure_record) {
            Ok(()) if self.failure_record.containment_empty == Some(true) => {
                if let Some(cgroup) = self.cgroup.take() {
                    cgroup.cleanup();
                }
                let _ = fs::remove_file(&self.socket_path);
                let _ = fs::remove_dir_all(&self.runtime_session_dir);
            }
            Ok(()) => {}
            Err(error) => eprintln!("aplexer worker: persist startup rollback: {error:#}"),
        }
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        self.cleanup("worker startup aborted before commit".into());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ThreadStart {
    Pending,
    Run,
    Abort,
}

pub(super) type ThreadStartGate = Arc<(Mutex<ThreadStart>, Condvar)>;

pub(super) fn await_thread_start(gate: &ThreadStartGate) -> bool {
    let (state, ready) = &**gate;
    let Ok(mut state) = state.lock() else {
        return false;
    };
    while *state == ThreadStart::Pending {
        let Ok(next) = ready.wait(state) else {
            return false;
        };
        state = next;
    }
    *state == ThreadStart::Run
}

pub(super) fn release_startup_threads(gate: &ThreadStartGate, decision: ThreadStart) {
    let (state, ready) = &**gate;
    if let Ok(mut state) = state.lock() {
        *state = decision;
        ready.notify_all();
    }
}

pub(super) fn load_launch_environment(
    path: &std::path::Path,
    legacy: LaunchEnvironment,
) -> Result<LaunchEnvironment> {
    match fs::read(path) {
        Ok(bytes) => {
            let bytes = SecretBytes(bytes);
            let environment = serde_json::from_slice(&bytes.0)
                .with_context(|| format!("parse private launch environment {}", path.display()))?;
            // Keeping a readable secret file after consumption is not a
            // recoverable warning. Fail startup so the transaction removes
            // the whole private runtime directory.
            fs::remove_file(path).with_context(|| {
                format!("remove consumed launch environment {}", path.display())
            })?;
            Ok(LaunchEnvironment(environment))
        }
        // Compatibility for sessions created by an older client, whose
        // launch values were stored directly in the record.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(legacy),
        Err(error) => Err(error)
            .with_context(|| format!("read private launch environment {}", path.display())),
    }
}

/// The durable record, read only once the worker lock is held.
///
/// Every destroyer of a pre-PID session (`a forget`, `a prune`, `a start`'s
/// reclaim) fences the worker through that lock and removes the durable
/// state while holding it. A record read *before* the lock could therefore
/// be stale: the destroyer unlinks the runtime dir, this worker recreates
/// it and acquires a fresh lock inode unopposed, and its first record
/// write resurrects a session that had just been forgotten. Read after the
/// lock, a record that is gone means the session no longer exists -- fail
/// the start and take the runtime dir this lock lives in back out, so the
/// refusal leaves nothing behind either.
pub(super) fn read_record_under_worker_lock(paths: &Paths, id: Uuid) -> Result<SessionRecord> {
    match read_session_record(paths, id) {
        Ok(record) => Ok(record),
        Err(error) => {
            if io_kind(&error) == Some(io::ErrorKind::NotFound) {
                let _ = fs::remove_dir_all(paths.runtime_session(id));
            }
            Err(error).with_context(|| {
                format!("session {id} has no durable record; refusing to start its worker")
            })
        }
    }
}

/// Bring the session up: consume the one-shot launch environment, publish
/// this process as the worker, bind the control socket, create the
/// containment domain and the PTY, spawn the workload, open history, and
/// start the runtime threads. Every resource created along the way is
/// owned by a `StartupGuard` until the last step commits; a failure rolls
/// them all back and persists a `Failed` record carrying the error.
pub(super) fn bring_up(
    paths: &Paths,
    mut record: SessionRecord,
    initial_size: Option<(u16, u16)>,
) -> Result<(UnixListener, FileIdentity, Arc<WorkerRuntime>)> {
    let id = record.id;
    let record_path = paths.record(id);
    let legacy_environment = LaunchEnvironment(std::mem::take(&mut record.env));
    record.env = session_metadata_env(&legacy_environment.0);
    let mut startup = StartupGuard::new(paths, &record);
    let setup = (|| -> Result<(UnixListener, FileIdentity, Arc<WorkerRuntime>)> {
        startup_checkpoint("after_worker_lock")?;
        let launch_environment_path = paths.runtime_session(id).join("launch-environment.json");
        let launch_environment =
            load_launch_environment(&launch_environment_path, legacy_environment)?;
        // Migrate a legacy record before exposing any further worker state,
        // retaining only non-secret roots needed for transcript discovery.
        record.worker_pid = Some(std::process::id());
        // Placement evidence (issue #1): the fork's pre_exec setsid() gave
        // this process a new session but left it in the ambient cgroup, so
        // whatever manager owns that cgroup can still kill this session
        // wholesale. Record where we actually are while we can still read
        // it -- after a manager-wide kill the path is gone and the failure
        // is unprovable, exactly the incident's `yolo` post-mortem problem.
        record.worker_cgroup = crate::placement::read_process_cgroup(std::process::id());
        record.updated_at_ms = now_ms();
        startup.failure_record = record.clone();
        atomic_write_json(&record_path, &record)?;
        startup_checkpoint("after_worker_record")?;

        let socket_path = paths.socket(id);
        if socket_path.exists() {
            fs::remove_file(&socket_path).context("remove stale control socket")?;
        }
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind {}", socket_path.display()))?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        let socket_identity = trusted_socket_identity(&socket_path)?;
        startup_checkpoint("after_control_socket")?;

        let requested_size = initial_size.unwrap_or((24, 80));
        let (rows, cols) = screen::validate_size(requested_size.0, requested_size.1)?;
        let cgroup = Cgroup::create(id, &record.limits, || {
            startup.cgroup_setup_started = true;
        })?;
        startup.cgroup = cgroup.clone();
        // Unlimited sessions (the common case: no memory/pids/cpu limits) have
        // no cgroup, so this second record write would persist byte-identical
        // containment fields plus a fresh timestamp -- a full fsync + parent
        // fsync for no new information (benchmark PLAN P0.3). Skip the write
        // and keep the already-persisted worker_pid record as the durable
        // state; the in-memory failure record is still updated for rollback.
        if cgroup.is_some() {
            record.containment_cgroup =
                cgroup.as_ref().map(|cgroup| cgroup.locator().to_path_buf());
            record.containment_cgroup_identity =
                cgroup.as_ref().map(|cgroup| cgroup.identity().clone());
            startup.failure_record = record.clone();
            atomic_write_json(&record_path, &record)?;
        } else {
            startup.failure_record = record.clone();
        }
        startup_checkpoint("after_cgroup")?;
        let (master_read, slave) = open_pty(rows, cols)?;
        let master_write = master_read.try_clone()?;
        let child_result = spawn_workload(
            &record,
            &launch_environment.0,
            master_read.as_raw_fd(),
            slave,
            cgroup.as_ref(),
        );
        // Launch values are one-shot: overwrite them as soon as spawn has
        // either succeeded or failed, never retaining them in the accept
        // loop or its background threads.
        drop(launch_environment);
        let child = child_result?;
        let pid = child.id();
        // Claim the leader before any code path can wait on it. The reaper
        // thread does not exist yet, but the claim is what documents (and
        // enforces) that `run_child_waiter` owns this pid's exit status.
        own_child_pid(pid);
        let child_slot = Arc::new(Mutex::new(Some(child)));
        startup.child = Some(Arc::clone(&child_slot));
        record.workload_pid = Some(pid);
        // Launch-time cgroup validation (issue #1): read where the workload
        // leader actually landed and, for a limited session, check that
        // against the scope systemd was asked to create for it. The
        // pre_exec cgroup.procs write is supposed to make a mismatch
        // impossible; if the two sources of truth ever disagree, say so in
        // worker.log instead of silently trusting the persisted locator.
        record.workload_cgroup = crate::placement::read_process_cgroup(pid);
        if let (Some(cgroup), Some(actual)) = (cgroup.as_ref(), record.workload_cgroup.as_deref()) {
            let expected = cgroup.proc_path();
            if actual != expected {
                eprintln!(
                    "warning: workload pid {pid} is in cgroup {actual}, not the recorded \
                     containment scope {expected}; resource limits may not apply to the \
                     workload's real location"
                );
            }
        }
        startup.failure_record = record.clone();
        // Publish the leader and cgroup locator before any injected or real
        // post-spawn failure. The launcher must never have to infer a
        // containment domain from an unpersisted in-memory PID.
        atomic_write_json(&record_path, &record)?;
        after_workload_spawn_checkpoint(pid)?;

        startup_checkpoint("before_history_open")?;
        validate_existing_history_node(&record.history_path)?;
        let history = History::open(record.history_path.clone(), record.history_bytes)?;
        startup_checkpoint("before_output_hub")?;
        let output = OutputHub::new(history, rows, cols, paths.screen_txt(id))?;
        record.phase = Phase::Running;
        record.updated_at_ms = now_ms();
        record.error = None;
        startup.failure_record = record.clone();
        let runtime = Arc::new(WorkerRuntime {
            id,
            paths: paths.clone(),
            record_path: record_path.clone(),
            runtime_session_dir: paths.runtime_session(id),
            socket_path,
            record: Mutex::new(record.clone()),
            pty_write: Mutex::new(Some(Arc::new(master_write))),
            workload: Mutex::new(WorkloadState {
                running: true,
                pgid: pid as i32,
            }),
            terminal: Mutex::new(TerminalState {
                rows,
                cols,
                clients: HashMap::new(),
                next_client_id: 1,
                activity_clock: 0,
            }),
            cgroup: Mutex::new(cgroup),
            kill_gate: Mutex::new(()),
            output,
            record_persistence_error: Mutex::new(None),
            active_connections: Arc::new(AtomicUsize::new(0)),
            last_activity_ms: AtomicU64::new(0),
        });
        start_worker_threads(
            Arc::clone(&runtime),
            master_read,
            Arc::clone(&child_slot),
            || {
                atomic_write_json(&record_path, &record)?;
                startup_checkpoint("after_running_record")
            },
        )?;
        Ok((listener, socket_identity, runtime))
    })();
    match setup {
        Ok(value) => {
            startup.disarm();
            Ok(value)
        }
        Err(error) => {
            startup.rollback(&error);
            Err(error)
        }
    }
}
