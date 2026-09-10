//! Rollback and hard cleanup for a worker startup that failed partway.
//!
//! One reason to exist: `a start` may have spawned a worker that then died
//! mid-startup, and something must guarantee that worker's tree, cgroup,
//! and record do not linger. This is the last-resort machinery -- stop and
//! pin descendants through pidfds, prove quiescence, kill the stopped tree
//! -- that a failed launch runs before it reports the original error.

use super::*;
use crate::pidfd::{Deadline, PidHandle};

// The worker's contained descendant sweep is bounded at two seconds. Leave
// another second for signal delivery, startup unwind, and record/fsync work.
pub(super) const STARTUP_TERM_GRACE: Duration = Duration::from_secs(3);
pub(super) const STARTUP_REAP_POLL: Duration = Duration::from_millis(10);
pub(super) const STARTUP_CONTAINMENT_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const RETIRED_SESSIONS_DIR: &str = "retired-sessions";
/// A corrupt or hostile startup tree must not consume every descriptor in the
/// launcher. The actual limit is reduced further to fit the launcher's live
/// RLIMIT_NOFILE budget before any process is stopped.
pub(super) const STARTUP_MAX_DESCENDANTS: usize = 4096;
pub(super) const STARTUP_FD_RESERVE: u64 = 16;
pub(super) const WORKER_REAPER_POLL: Duration = Duration::from_millis(100);
pub(super) const STARTUP_READY_RPC_SLICE: Duration = Duration::from_millis(100);
pub(super) static WORKER_REAPER: Mutex<Option<mpsc::Sender<Child>>> = Mutex::new(None);

pub(super) fn worker_reaper_loop(receiver: mpsc::Receiver<Child>) {
    let mut children: Vec<Child> = Vec::new();
    loop {
        let received = if children.is_empty() {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(WORKER_REAPER_POLL)
        };
        match received {
            Ok(child) => children.push(child),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(child) = receiver.try_recv() {
            children.push(child);
        }
        children.retain_mut(|child| match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(error) => {
                eprintln!("aplexer: wait for worker {} failed: {error}", child.id());
                false
            }
        });
    }
}

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

pub(super) type StartupDescendant = PidHandle;

pub(super) fn signal_startup_descendant(
    descendant: &StartupDescendant,
    signal: i32,
    deadline: Deadline,
) -> Result<()> {
    deadline.check("signalling startup process tree")?;
    descendant.signal(signal)?;
    deadline.check("signalling startup process tree")
}

pub(super) fn pidfd_exited(descendant: &StartupDescendant, deadline: Deadline) -> Result<bool> {
    let mut pollfd = libc::pollfd {
        fd: descendant.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        deadline.check("polling startup process handles")?;
        let result = unsafe { libc::poll(&mut pollfd, 1, 0) };
        if result == 0 {
            return Ok(false);
        }
        if result == 1 {
            if pollfd.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                return Ok(true);
            }
            bail!(
                "unexpected pidfd poll events for descendant {}: {:#x}",
                descendant.pid(),
                pollfd.revents
            );
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error).context("poll descendant pidfd");
        }
    }
}

pub(super) fn process_state_and_start_time(
    pid: u32,
    deadline: Deadline,
) -> Result<Option<(char, u64)>> {
    deadline.check("reading startup process state")?;
    let stat_path = format!("/proc/{pid}/stat");
    let stat = match fs::read_to_string(&stat_path) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {stat_path}")),
    };
    deadline.check("reading startup process state")?;
    let after_comm = stat
        .rfind(')')
        .and_then(|end| stat.get(end + 1..))
        .ok_or_else(|| anyhow!("malformed {stat_path}"))?;
    let fields = after_comm.split_whitespace().collect::<Vec<_>>();
    let state = fields
        .first()
        .and_then(|value| value.chars().next())
        .ok_or_else(|| anyhow!("{stat_path} has no process state"))?;
    let start_time_ticks = fields
        .get(19)
        .ok_or_else(|| anyhow!("{stat_path} has no process start time"))?
        .parse()
        .with_context(|| format!("parse process start time from {stat_path}"))?;
    Ok(Some((state, start_time_ticks)))
}

pub(super) fn startup_descendant_quiescent(
    descendant: &StartupDescendant,
    deadline: Deadline,
) -> Result<bool> {
    if pidfd_exited(descendant, deadline)? {
        return Ok(true);
    }
    match process_state_and_start_time(descendant.pid(), deadline)? {
        Some((_, start_time_ticks)) if start_time_ticks != descendant.start_time_ticks() => {
            bail!(
                "descendant {} changed identity while its pidfd remained live",
                descendant.pid()
            )
        }
        Some((state, _)) => Ok(matches!(state, 'T' | 't' | 'Z' | 'X' | 'x')),
        None if pidfd_exited(descendant, deadline)? => Ok(true),
        None => bail!(
            "descendant {} disappeared from /proc while its pidfd remained live",
            descendant.pid()
        ),
    }
}

pub(super) fn ensure_startup_worker_stopped(
    pid: u32,
    start_time_ticks: u64,
    deadline: Deadline,
) -> Result<()> {
    match process_state_and_start_time(pid, deadline)? {
        Some((_, current)) if current != start_time_ticks => {
            bail!("startup worker {pid} changed process identity")
        }
        Some(('T' | 't', _)) => Ok(()),
        Some(('Z' | 'X' | 'x', _)) => {
            bail!("startup worker {pid} exited before containment was confirmed")
        }
        Some(_) => bail!("startup worker {pid} resumed during containment inspection"),
        None => bail!("startup worker {pid} disappeared before containment was confirmed"),
    }
}

/// The stopped worker's live descendant tree, bounded by `max_descendants`
/// and `deadline`; the worker must still be stopped on both sides of the
/// walk, or the tree it describes is already stale.
pub(super) fn startup_descendant_pids(
    root: u32,
    root_start_time: u64,
    max_descendants: usize,
    deadline: Deadline,
) -> Result<Vec<u32>> {
    deadline.check("scanning startup process tree")?;
    ensure_startup_worker_stopped(root, root_start_time, deadline)?;
    let descendants = crate::pidfd::descendant_pids_until(root, deadline, max_descendants)
        .context("scan startup process tree")?;
    ensure_startup_worker_stopped(root, root_start_time, deadline)?;
    Ok(descendants)
}

pub(super) fn wait_for_worker_stopped(
    pid: u32,
    start_time_ticks: u64,
    deadline: Deadline,
) -> Result<()> {
    loop {
        match process_state_and_start_time(pid, deadline)? {
            Some((_, current)) if current != start_time_ticks => {
                bail!("startup worker {pid} changed process identity")
            }
            Some(('T' | 't', _)) => return Ok(()),
            Some(('Z' | 'X' | 'x', _)) => {
                bail!("startup worker {pid} exited before containment was inspected")
            }
            Some(_) => {}
            None => bail!("startup worker {pid} disappeared before containment was inspected"),
        }
        deadline.sleep_poll(
            STARTUP_REAP_POLL,
            &format!("stopping startup worker {pid} for containment inspection"),
        )?;
    }
}

pub(super) fn stop_and_pin_startup_descendants(
    root: u32,
    root_start_time: u64,
    max_descendants: usize,
    deadline: Deadline,
    descendants: &mut BTreeMap<u32, StartupDescendant>,
) -> Result<()> {
    loop {
        deadline.check("stabilizing startup worker descendant tree")?;
        let mut discovered_new = false;
        for pid in startup_descendant_pids(root, root_start_time, max_descendants, deadline)? {
            if descendants.contains_key(&pid) {
                continue;
            }
            if descendants.len() >= max_descendants {
                bail!("startup process tree exceeds safe pidfd limit of {max_descendants}");
            }
            if let Some(descendant) = PidHandle::open(pid, Some(deadline))? {
                // Check before the destructive signal, then record the handle
                // before checking again. If the deadline crosses during the
                // syscall, the caller still owns everything it must resume.
                deadline.check("stopping startup worker descendants")?;
                descendant.signal(libc::SIGSTOP)?;
                descendants.insert(pid, descendant);
                deadline.check("stopping startup worker descendants")?;
                discovered_new = true;
            }
        }

        loop {
            let mut all_stopped = true;
            for descendant in descendants.values() {
                deadline.check("quiescing startup worker descendants")?;
                all_stopped &= startup_descendant_quiescent(descendant, deadline)?;
            }
            if all_stopped {
                break;
            }
            deadline.sleep_poll(STARTUP_REAP_POLL, "quiescing startup worker descendants")?;
        }

        // Once every process known so far is stopped, a pass that discovers no
        // new pid closes the fork-vs-scan race: only an as-yet unknown process
        // could still have run between the earlier tree walk and SIGSTOP.
        if !discovered_new {
            return Ok(());
        }
    }
}

pub(super) fn wait_for_descendant_exit(
    descendants: &BTreeMap<u32, StartupDescendant>,
    deadline: Deadline,
) -> Result<()> {
    loop {
        let mut all_exited = true;
        for descendant in descendants.values() {
            deadline.check("waiting for startup worker descendants to exit")?;
            all_exited &= pidfd_exited(descendant, deadline)?;
        }
        if all_exited {
            return Ok(());
        }
        deadline.sleep_poll(
            STARTUP_REAP_POLL,
            "waiting for startup worker descendants to exit",
        )?;
    }
}

pub(super) fn safe_startup_descendant_capacity(deadline: Deadline) -> Result<usize> {
    deadline.check("preflighting startup containment resources")?;
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error()).context("read RLIMIT_NOFILE");
    }
    deadline.check("preflighting startup containment resources")?;
    let fd_dir = fs::read_dir("/proc/self/fd").context("count open launcher descriptors")?;
    let mut open_fds = 0_u64;
    for entry in fd_dir {
        deadline.check("counting open launcher descriptors")?;
        entry.context("enumerate open launcher descriptors")?;
        open_fds = open_fds
            .checked_add(1)
            .ok_or_else(|| anyhow!("open descriptor count overflow"))?;
    }
    deadline.check("preflighting startup containment resources")?;

    let soft_limit = if limit.rlim_cur == libc::RLIM_INFINITY {
        u64::MAX
    } else {
        limit.rlim_cur
    };
    startup_descendant_capacity(soft_limit, open_fds)
}

pub(super) fn startup_descendant_capacity(soft_limit: u64, open_fds: u64) -> Result<usize> {
    // One additional descriptor pins the worker itself. Keep a reserve for
    // diagnostics, record IO, and the registry lock so containment cannot
    // exhaust the launcher's ability to preserve evidence.
    let available_descendants = soft_limit
        .saturating_sub(open_fds)
        .saturating_sub(STARTUP_FD_RESERVE)
        .saturating_sub(1);
    let capacity = usize::try_from(available_descendants)
        .unwrap_or(usize::MAX)
        .min(STARTUP_MAX_DESCENDANTS);
    if capacity == 0 {
        bail!(
            "insufficient RLIMIT_NOFILE headroom for safe startup containment \
             ({open_fds} descriptors open, soft limit {soft_limit})"
        );
    }
    Ok(capacity)
}

pub(super) fn kill_stopped_startup_tree(
    worker: &StartupDescendant,
    descendants: &BTreeMap<u32, StartupDescendant>,
) -> Result<()> {
    let mut failures = Vec::new();
    // Keep the subreaper stopped until every process we pinned has been sent
    // KILL. The retained session record remains the evidence for any process
    // that was racing discovery when recovery became necessary.
    for descendant in descendants.values() {
        if let Err(error) = descendant.signal(libc::SIGKILL) {
            failures.push(format!(
                "kill stopped startup descendant {}: {error:#}",
                descendant.pid()
            ));
        }
    }
    if let Err(error) = worker.signal(libc::SIGKILL) {
        failures.push(format!(
            "kill stopped startup worker {}: {error:#}",
            worker.pid()
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{}", failures.join("; "))
    }
}

pub(super) fn resume_stopped_startup_tree(
    worker: &StartupDescendant,
    descendants: &BTreeMap<u32, StartupDescendant>,
) -> Result<()> {
    // Resume the subreaper first so it can immediately continue the normal
    // TERM-driven rollback path. Then release each pinned child. Every signal
    // uses a pidfd, so recovery can never target a recycled numeric PID.
    if let Err(error) = worker.signal(libc::SIGCONT) {
        return kill_stopped_startup_tree(worker, descendants).with_context(|| {
            format!(
                "resume startup worker {} failed ({error:#}); fallback KILL also failed",
                worker.pid()
            )
        });
    }
    let mut failures = Vec::new();
    for descendant in descendants.values() {
        if let Err(error) = descendant.signal(libc::SIGCONT) {
            // The worker is running its requested TERM rollback again. If an
            // individual child cannot be continued, remove that stopped
            // child through its same identity-pinned handle.
            if let Err(kill_error) = descendant.signal(libc::SIGKILL) {
                failures.push(format!(
                    "resume startup descendant {} failed ({error:#}); fallback KILL failed: {kill_error:#}",
                    descendant.pid()
                ));
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{}", failures.join("; "))
    }
}

pub(super) fn hard_cleanup_startup_child(child: &mut Child, record_path: &Path) -> Result<()> {
    // All discovery, handle acquisition, signalling, and waits share this one
    // deadline. A large/forking tree cannot multiply the timeout by phases or
    // by the number of processes it creates.
    let deadline = Deadline::after(STARTUP_CONTAINMENT_TIMEOUT);
    let worker_pid = child.id();
    deadline.check("reading startup containment record")?;
    let record = read_record(record_path).context("read startup containment record")?;
    deadline.check("reading startup containment record")?;
    if record.worker_pid.is_some() && record.worker_pid != Some(worker_pid) {
        bail!("startup record no longer belongs to worker {worker_pid}");
    }
    let missing_required_cgroup = record.limits.requested()
        && (record.containment_cgroup.is_none() || record.containment_cgroup_identity.is_none());
    let recorded_cgroup_identity = record.containment_cgroup_identity.clone();
    let recorded_cgroup = record.containment_cgroup;
    let max_descendants = safe_startup_descendant_capacity(deadline)?;
    // Opening a worker pidfd and exercising pidfd_send_signal(2) with signal
    // zero proves both required syscalls and permissions before SIGSTOP can
    // make any member of the tree dependent on our recovery path.
    let worker = PidHandle::open(worker_pid, Some(deadline))?.ok_or_else(|| {
        anyhow!("startup worker {worker_pid} exited before containment preflight")
    })?;
    signal_startup_descendant(&worker, 0, deadline)
        .context("preflight pidfd signalling support")?;
    let worker_start_time = worker.start_time_ticks();
    let mut descendants = BTreeMap::new();
    let mut worker_stopped = false;
    let mut worker_destroyed = false;

    let cleanup = (|| -> Result<()> {
        deadline.check("stopping startup worker for containment inspection")?;
        worker
            .signal(libc::SIGSTOP)
            .with_context(|| format!("stop startup worker {worker_pid}"))?;
        worker_stopped = true;
        deadline.check("stopping startup worker for containment inspection")?;
        wait_for_worker_stopped(worker_pid, worker_start_time, deadline)?;

        stop_and_pin_startup_descendants(
            worker_pid,
            worker_start_time,
            max_descendants,
            deadline,
            &mut descendants,
        )?;
        for descendant in descendants.values() {
            signal_startup_descendant(descendant, libc::SIGKILL, deadline)?;
        }
        wait_for_descendant_exit(&descendants, deadline)?;

        // systemd owns limited-session scope members, so they need not remain
        // descendants of the worker subreaper. With the worker and its known
        // tree stopped, the recorded cgroup is the authoritative second
        // containment domain. It shares the same deadline as pidfd cleanup.
        if let Some(locator) = &recorded_cgroup {
            cleanup_recorded_cgroup_until(
                record.id,
                locator,
                recorded_cgroup_identity.as_ref(),
                libc::SIGKILL,
                Duration::ZERO,
                deadline.instant(),
            )
            .context("empty recorded startup cgroup")?;
        }

        // No stopped descendant can fork, and every pinned descendant has
        // exited. A final bounded walk proves no unpinned process was missed
        // before destroying the subreaper root that makes the tree visible.
        let remaining =
            startup_descendant_pids(worker_pid, worker_start_time, max_descendants, deadline)?;
        if remaining.iter().any(|pid| !descendants.contains_key(pid)) {
            bail!("startup worker descendant tree changed after quiescence");
        }

        signal_startup_descendant(&worker, libc::SIGKILL, deadline)
            .with_context(|| format!("kill startup worker {worker_pid}"))?;
        worker_destroyed = true;
        loop {
            deadline.check("reaping startup worker after SIGKILL")?;
            match child.try_wait() {
                Ok(Some(_)) if missing_required_cgroup => {
                    bail!(
                        "limited startup had no recorded cgroup locator; local process tree was killed but complete containment cleanup is unproven"
                    )
                }
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("reap startup worker {worker_pid}"));
                }
            }
            deadline.sleep_poll(STARTUP_REAP_POLL, "reaping startup worker after SIGKILL")?;
        }
    })();

    match cleanup {
        Ok(()) => Ok(()),
        Err(error) if worker_stopped && !worker_destroyed => {
            match resume_stopped_startup_tree(&worker, &descendants) {
                Ok(()) => Err(error.context(
                    "hard startup cleanup failed; resumed the pinned process tree for TERM rollback",
                )),
                Err(recovery_error) => Err(error.context(format!(
                    "hard startup cleanup failed and stopped-tree recovery also failed: {recovery_error:#}"
                ))),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod startup_cleanup_tests {
    use super::*;
    use crate::ExitInfo;

    #[test]
    pub(super) fn descriptor_budget_is_finite_and_preserves_headroom() {
        let deadline = Deadline::after(Duration::from_secs(1));
        let capacity = safe_startup_descendant_capacity(deadline).expect("descriptor budget");
        assert!((1..=STARTUP_MAX_DESCENDANTS).contains(&capacity));
        assert!(capacity <= STARTUP_MAX_DESCENDANTS);
    }

    #[test]
    pub(super) fn descriptor_budget_rejects_exhaustion_and_caps_large_limits() {
        let required = STARTUP_FD_RESERVE + 1;
        assert!(startup_descendant_capacity(required, 0).is_err());
        assert!(startup_descendant_capacity(required + 10, 10).is_err());
        assert_eq!(
            startup_descendant_capacity(u64::MAX, 0).expect("large descriptor budget"),
            STARTUP_MAX_DESCENDANTS
        );
    }

    pub(super) fn startup_record(
        phase: Phase,
        exit: Option<ExitInfo>,
        containment_empty: Option<bool>,
    ) -> SessionRecord {
        let mut record = SessionRecord::fixture("/ws", "main");
        record.phase = phase;
        record.worker_pid = Some(1);
        record.containment_empty = containment_empty;
        record.exit = exit;
        record
    }

    /// `(description, phase, exit, containment_empty, expected_accept)`.
    type AcceptanceCase = (&'static str, Phase, Option<ExitInfo>, Option<bool>, bool);

    pub(super) fn clean_exit() -> Option<ExitInfo> {
        Some(ExitInfo {
            code: Some(0),
            signal: None,
            oom_killed: false,
            exited_at_ms: 2,
        })
    }

    /// Exhaustive matrix for the accept condition applied to an exited
    /// worker's durable record. Every clause of
    /// `exited_worker_completed_startup` is exercised in both directions:
    /// deleting any one of the three conjuncts turns at least one `false` row
    /// green, which is what makes this table load-bearing rather than
    /// decorative.
    #[test]
    pub(super) fn exited_worker_startup_acceptance_matrix() {
        let cases: &[AcceptanceCase] = &[
            // The fast-workload shape the readiness Ping can never observe:
            // ran to completion, recorded its exit, proved containment empty.
            (
                "exited with exit info and proven-empty containment",
                Phase::Exited,
                clean_exit(),
                Some(true),
                true,
            ),
            // Terminal phase, but nothing proves the workload ever ran.
            (
                "exited without exit info",
                Phase::Exited,
                None,
                Some(true),
                false,
            ),
            // The only shape the `exit.is_some()` conjunct guards on its own.
            (
                "exiting without exit info",
                Phase::Exiting,
                None,
                Some(true),
                false,
            ),
            // Pinned decision: symmetric with the readiness arm's
            // `Running | Exiting | Exited`, currently unreachable in practice.
            (
                "exiting with exit info and proven-empty containment",
                Phase::Exiting,
                clean_exit(),
                Some(true),
                true,
            ),
            // A worker that vanished mid-run never became ready, whatever
            // exit info happens to be on the record.
            (
                "running with exit info",
                Phase::Running,
                clean_exit(),
                Some(true),
                false,
            ),
            // The exact initial record `start_session` writes before the
            // worker registers itself.
            (
                "starting, as start_session first writes it",
                Phase::Starting,
                None,
                Some(false),
                false,
            ),
            // Same phase, but with every other clause satisfied, so this row
            // isolates the phase guard rather than riding on `exit`.
            (
                "starting with exit info and proven-empty containment",
                Phase::Starting,
                clean_exit(),
                Some(true),
                false,
            ),
            // The worker's own recorded failure is never laundered into a
            // completed session here.
            (
                "failed with exit info and proven-empty containment",
                Phase::Failed,
                clean_exit(),
                Some(true),
                false,
            ),
            // The safety clause: a terminal record whose containment domain
            // is NOT proven empty may have an escaped descendant, so
            // reporting startup success would be exactly the laundering this
            // predicate exists to prevent.
            (
                "exited with exit info but containment not proven empty",
                Phase::Exited,
                clean_exit(),
                Some(false),
                false,
            ),
            // Legacy/absent proof is not proof. Unreachable for a record
            // written by the worker this call spawned, but the predicate
            // must not silently widen if that ever stops holding.
            (
                "exited with exit info but no containment field",
                Phase::Exited,
                clean_exit(),
                None,
                false,
            ),
        ];
        // Collect every mismatch instead of stopping at the first, so
        // deleting a conjunct names the whole set of rows it breaks.
        let mut mismatches = Vec::new();
        for (name, phase, exit, containment_empty, expected) in cases {
            let record = startup_record(phase.clone(), exit.clone(), *containment_empty);
            let actual = exited_worker_completed_startup(&record);
            if actual != *expected {
                mismatches.push(format!("{name}: expected {expected}, got {actual}"));
            }
        }
        assert!(
            mismatches.is_empty(),
            "exited-worker acceptance matrix regressed:\n  {}",
            mismatches.join("\n  ")
        );
    }
}
