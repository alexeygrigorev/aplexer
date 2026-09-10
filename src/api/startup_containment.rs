//! Hard cleanup for a worker startup that failed partway and did not unwind
//! itself: stop and pin descendants through pidfds, prove quiescence, kill
//! the stopped tree, and only then destroy the subreaper root.
//!
//! One reason to exist: `a start` may have spawned a worker that then died
//! or hung mid-startup, and something must guarantee that worker's tree,
//! cgroup, and record do not linger. This is the last-resort machinery a
//! failed launch runs before it reports the original error.

use super::*;
use crate::pidfd::{Deadline, PidHandle};

pub(super) const STARTUP_CONTAINMENT_TIMEOUT: Duration = Duration::from_secs(2);
/// A corrupt or hostile startup tree must not consume every descriptor in the
/// launcher. The actual limit is reduced further to fit the launcher's live
/// RLIMIT_NOFILE budget before any process is stopped.
pub(super) const STARTUP_MAX_DESCENDANTS: usize = 4096;
pub(super) const STARTUP_FD_RESERVE: u64 = 16;

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
mod tests {
    use super::*;

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
}
