//! The launcher's guard over a starting session: every artifact created for
//! it is owned here until the worker is ready, and a failed start unwinds
//! through it -- TERM to the worker, then the hard containment cleanup if
//! the worker does not finish its own rollback in time.

use super::*;

// The worker's contained descendant sweep is bounded at two seconds. Leave
// another second for signal delivery, startup unwind, and record/fsync work.
pub(super) const STARTUP_TERM_GRACE: Duration = Duration::from_secs(3);
pub(super) const STARTUP_REAP_POLL: Duration = Duration::from_millis(10);

/// Owns every artifact the launcher created for a session until its worker
/// is ready. Normal error paths call `rollback` so cleanup failures can be
/// reported; `Drop` is the panic/early-return safety net. Named for the
/// launcher side so it cannot be confused with the worker's own
/// `worker::StartupGuard`, which owns the resources the worker process
/// creates during its bring-up.
pub(super) struct LaunchGuard<'a> {
    pub(super) paths: &'a Paths,
    pub(super) id: Uuid,
    pub(super) child: Option<Child>,
    pub(super) armed: bool,
}

impl<'a> LaunchGuard<'a> {
    pub(super) fn new(paths: &'a Paths, id: Uuid) -> Self {
        Self {
            paths,
            id,
            child: None,
            armed: true,
        }
    }

    pub(super) fn track_child(&mut self, child: Child) {
        self.child = Some(child);
    }

    pub(super) fn child_mut(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("startup child must be tracked after spawn")
    }

    /// Transfer a successfully-started worker to the shared detached waiter. The CLI
    /// normally exits long before the worker, but embedders (notably Python)
    /// can outlive many sessions; merely dropping `Child` there leaves every
    /// completed worker as a zombie owned by the host process.
    ///
    /// Keep the child in this guard until the waiter has been created and has
    /// accepted it. If either step fails, rollback still owns the process and
    /// can terminate it instead of leaking an unreapable child handle.
    pub(super) fn hand_off_to_reaper(&mut self) -> Result<()> {
        let child = self
            .child
            .take()
            .expect("ready worker must still be owned by startup guard");
        let worker_pid = child.id();
        let mut child = Some(child);
        let mut reaper = WORKER_REAPER
            .lock()
            .map_err(|_| anyhow!("worker reaper registry lock poisoned"))?;
        for _ in 0..2 {
            if reaper.is_none() {
                let (sender, receiver) = mpsc::channel();
                if let Err(error) = thread::Builder::new()
                    .name("aplexer-worker-reaper".into())
                    .spawn(move || worker_reaper_loop(receiver))
                {
                    self.child = child.take();
                    return Err(error).context("spawn worker reaper");
                }
                *reaper = Some(sender);
            }
            let sender = reaper
                .as_ref()
                .expect("worker reaper sender was just initialized");
            match sender.send(child.take().expect("worker child sent only once")) {
                Ok(()) => {
                    self.armed = false;
                    return Ok(());
                }
                Err(error) => {
                    child = Some(error.0);
                    *reaper = None;
                }
            }
        }
        self.child = child;
        bail!("worker reaper exited before accepting worker {worker_pid}")
    }

    pub(super) fn rollback(&mut self) -> Result<()> {
        if !std::mem::replace(&mut self.armed, false) {
            return Ok(());
        }

        let mut failures = Vec::new();
        let containment_confirmed = self
            .child
            .as_mut()
            .map(|child| {
                terminate_and_reap_startup_child(child, &self.paths.record(self.id), &mut failures)
            })
            .unwrap_or(true);
        self.child.take();

        if containment_confirmed {
            for (what, path) in [
                ("runtime state", self.paths.runtime_session(self.id)),
                ("durable state", self.paths.state_session(self.id)),
            ] {
                match fs::remove_dir_all(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        failures.push(format!("remove {what} {}: {error}", path.display()))
                    }
                }
            }
        } else {
            failures.push(format!(
                "startup containment for {} could not be confirmed; preserved runtime and durable state",
                self.id
            ));
        }

        if failures.is_empty() {
            Ok(())
        } else {
            bail!("{}", failures.join("; "))
        }
    }
}

impl Drop for LaunchGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.rollback() {
            eprintln!(
                "aplexer: startup rollback for {} failed: {error:#}",
                self.id
            );
        }
    }
}

pub(super) fn reaped_worker_cleanup_confirmed(record_path: &Path, worker_pid: u32) -> bool {
    let Ok(record) = read_record(record_path) else {
        return false;
    };
    // The parent creates a Starting record with no worker pid. The worker
    // persists its pid before it can create a cgroup or spawn the workload, so
    // an unchanged record proves that no containment domain ever existed.
    if record.worker_pid.is_none() && record.workload_pid.is_none() {
        return true;
    }
    if record.worker_pid != Some(worker_pid) {
        return false;
    }
    // Once a worker registered itself, require either its explicit new proof
    // or the legacy ExitInfo proof recognized by containment_proven_empty().
    // Leader exit or a missing workload pid alone remain insufficient because
    // setsid descendants can survive both.
    record.containment_proven_empty()
}

pub(super) fn persist_independent_cleanup_proof(record_path: &Path, worker_pid: u32) -> Result<()> {
    let mut record = read_record(record_path)?;
    if record.worker_pid.is_some() && record.worker_pid != Some(worker_pid) {
        bail!("startup record worker identity changed before cleanup proof persistence");
    }
    record.phase = Phase::Failed;
    record.containment_empty = Some(true);
    record.updated_at_ms = crate::now_ms();
    record.error.get_or_insert_with(|| {
        "worker did not complete startup; launcher independently emptied containment".into()
    });
    atomic_write_json(record_path, &record).context("persist independent containment proof")
}

pub(super) fn reaped_startup_child_result(
    child: &Child,
    record_path: &Path,
    failures: &mut Vec<String>,
) -> bool {
    if reaped_worker_cleanup_confirmed(record_path, child.id()) {
        true
    } else {
        failures.push(format!(
            "worker {} exited before independent containment cleanup and left no conclusive cleanup record",
            child.id()
        ));
        false
    }
}

pub(super) fn terminate_and_reap_startup_child(
    child: &mut Child,
    record_path: &Path,
    failures: &mut Vec<String>,
) -> bool {
    let mut reaped = match child.try_wait() {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => {
            failures.push(format!(
                "inspect worker {} before rollback: {error}",
                child.id()
            ));
            false
        }
    };

    if !reaped {
        if let Err(error) = signal_worker_group(child.id(), libc::SIGTERM) {
            failures.push(format!("terminate worker session {}: {error}", child.id()));
        }
        let deadline = Instant::now() + STARTUP_TERM_GRACE;
        while Instant::now() < deadline {
            match child.try_wait() {
                Ok(Some(_)) => {
                    reaped = true;
                    break;
                }
                Ok(None) => thread::sleep(STARTUP_REAP_POLL),
                Err(error) => {
                    failures.push(format!(
                        "wait for worker {} after TERM: {error}",
                        child.id()
                    ));
                    break;
                }
            }
        }
    }

    if reaped {
        return reaped_startup_child_result(child, record_path, failures);
    }

    // Close the boundary race where the worker exits immediately after the
    // final poll above. Do not infer successful rollback merely from exit: an
    // external SIGKILL can reap the subreaper while descendants still live.
    match child.try_wait() {
        Ok(Some(_)) => return reaped_startup_child_result(child, record_path, failures),
        Ok(None) => {}
        Err(error) => failures.push(format!(
            "inspect worker {} before containment cleanup: {error}",
            child.id()
        )),
    }

    match hard_cleanup_startup_child(child, record_path) {
        Ok(()) => match persist_independent_cleanup_proof(record_path, child.id()) {
            Ok(()) => true,
            Err(error) => {
                failures.push(format!(
                    "persist independent cleanup proof for worker {}: {error:#}",
                    child.id()
                ));
                false
            }
        },
        Err(error) => {
            failures.push(format!(
                "independently clean worker {} containment: {error:#}",
                child.id()
            ));
            false
        }
    }
}
