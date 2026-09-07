//! Process-tree operations over /proc: the subreaper flag, descendant
//! enumeration, and descendant teardown.
//!
//! One reason to exist: for an unlimited session the worker's own descendant
//! tree IS the containment boundary, so signalling or killing a workload
//! means walking that tree through pidfd-held handles -- a recycled pid can
//! never be signalled in place of the process the handle was opened for --
//! and reaping adopted strays along the way.

use super::*;

/// Make the worker the reparenting boundary for daemonized workload
/// descendants. This is process-wide on Linux and must happen before the
/// workload is spawned. It does not require systemd or cgroup delegation.
pub(super) fn enable_child_subreaper() -> Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error()).context("enable child subreaper");
    }
    Ok(())
}

/// Read every child attached to any thread in a process. Reading only the
/// thread-group leader's `children` file can miss children forked by another
/// thread, which would create a containment escape for multi-threaded tools.
pub(super) fn direct_child_pids(pid: u32) -> Result<Vec<u32>> {
    direct_child_pids_in(Path::new(crate::agent_kind::DEFAULT_PROC_ROOT), pid)
}

/// `direct_child_pids` against an arbitrary `/proc` root. The containment
/// code always passes the real `/proc`; `agent_kind`'s detection walk shares
/// this exact reader so its unit tests can drive a synthetic tree instead of
/// spawning processes -- one walker, one set of `children` semantics.
pub(crate) fn direct_child_pids_in(proc_root: &Path, pid: u32) -> Result<Vec<u32>> {
    let tasks_path = proc_root.join(pid.to_string()).join("task");
    let tasks = match fs::read_dir(&tasks_path) {
        Ok(tasks) => tasks,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", tasks_path.display())),
    };
    let mut children = HashSet::new();
    for task in tasks {
        let Ok(task) = task else { continue };
        let Some(tid) = task
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let path = tasks_path.join(tid.to_string()).join("children");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
        };
        children.extend(
            text.split_whitespace()
                .filter_map(|value| value.parse::<u32>().ok()),
        );
    }
    Ok(children.into_iter().collect())
}

/// Every process under `root` that can still run code.
///
/// Zombies are deliberately excluded. A process that has exited but has not
/// been reaped still appears in its parent's `children` file, still answers
/// `kill(pid, 0)`, and still yields a working pidfd -- so before this filter
/// existed one unreaped descendant made `workload_populated` report the
/// containment domain permanently populated (the lifecycle could never prove
/// it empty, so the worker never finished) and made `kill_descendants` spin
/// until `DESCENDANT_KILL_TIMEOUT` and fail with "timed out killing contained
/// workload descendants", because SIGKILL to a zombie changes nothing.
///
/// Skipping a zombie hides no subtree: a process's children are reparented
/// to the nearest subreaper at the moment it exits, so by the time it is a
/// zombie it has none left to walk.
pub(super) fn descendant_pids(root: u32) -> Result<Vec<u32>> {
    let mut pending = VecDeque::from([root]);
    let mut seen = HashSet::from([root]);
    let mut descendants = Vec::new();
    while let Some(parent) = pending.pop_front() {
        for child in direct_child_pids(parent)? {
            if !seen.insert(child) {
                continue;
            }
            if crate::process_is_zombie(child) {
                continue;
            }
            descendants.push(child);
            pending.push_back(child);
        }
    }
    Ok(descendants)
}

pub(super) struct DescendantHandle {
    pid: u32,
    pidfd: File,
}

pub(super) fn open_descendant_handle(pid: u32) -> Result<Option<DescendantHandle>> {
    let start_time = match process_start_time_ticks(pid) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("open pidfd for descendant {pid}"));
    }
    let pidfd = unsafe { File::from_raw_fd(fd) };
    // Pin first, then re-read identity. A numeric pid recycled between the
    // tree walk and pidfd_open must never redirect a session signal.
    if process_start_time_ticks(pid).ok() != Some(start_time) {
        return Ok(None);
    }
    Ok(Some(DescendantHandle { pid, pidfd }))
}

pub(super) fn descendant_handles(root: u32) -> Result<Vec<DescendantHandle>> {
    descendant_pids(root)?
        .into_iter()
        .filter_map(|pid| open_descendant_handle(pid).transpose())
        .collect()
}

pub(super) fn signal_handle(handle: &DescendantHandle, signal: i32) -> Result<()> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            handle.pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).with_context(|| format!("signal descendant {}", handle.pid));
        }
    }
    Ok(())
}

pub(super) fn signal_descendants(root: u32, signal: i32) -> Result<usize> {
    let handles = descendant_handles(root)?;
    for handle in &handles {
        signal_handle(handle, signal)?;
    }
    Ok(handles.len())
}

/// Repeated scans close the fork-vs-scan race: the first pass stops the
/// parents, and later passes catch children created immediately before the
/// signal arrived. pidfds make every individual signal immune to pid reuse.
pub(super) fn kill_descendants(root: u32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if signal_descendants(root, libc::SIGKILL)? == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out killing contained workload descendants");
        }
        thread::sleep(DESCENDANT_POLL_INTERVAL);
    }
}

/// The leader has its own `Child::wait` thread. Only after that waiter has
/// reported completion may the lifecycle thread reap any other child,
/// avoiding a waitpid(-1) race that could steal the leader's exit status.
pub(super) fn reap_adopted_children() -> Result<usize> {
    let mut reaped = 0;
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid > 0 {
            reaped += 1;
            continue;
        }
        if pid == 0 {
            return Ok(reaped);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(reaped);
        }
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error).context("reap adopted workload descendant");
    }
}
