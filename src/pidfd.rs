//! The one pidfd/procfs toolkit: process-tree walks over `/proc`, pidfd
//! handles pinned to a process identity, and pidfd signalling.
//!
//! One reason to exist: three callers (the worker's containment sweeps, the
//! launcher's startup rollback, and the recorded-worker signal path) each
//! carried their own copy of "read every thread's `children` file", "open
//! a pidfd and re-check the start time", and "`pidfd_send_signal`, ESRCH is
//! fine". A recycled numeric pid must never be signalled in place of the
//! process a handle was opened for, and that rule is easier to keep in one
//! place. Every walk takes an optional [`Deadline`] so a bounded teardown
//! stops mid-walk when its budget runs out; every walk skips zombies, which
//! cannot run code and, having already reparented their children, hide no
//! subtree.

use anyhow::{bail, Context, Result};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::agent_kind::DEFAULT_PROC_ROOT;
use crate::{io_kind, pidfd_open, process_is_zombie, process_start_time_ticks};

/// A wall-clock budget shared by every step of a tree walk or teardown, so
/// a large or forking tree cannot multiply a timeout by phases or by the
/// number of processes it creates.
#[derive(Clone, Copy)]
pub(crate) struct Deadline(Instant);

impl Deadline {
    pub(crate) fn after(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }

    pub(crate) fn instant(self) -> Instant {
        self.0
    }

    pub(crate) fn check(self, operation: &str) -> Result<()> {
        if Instant::now() >= self.0 {
            bail!("timed out {operation}");
        }
        Ok(())
    }

    /// Sleep one poll interval (clipped to the remaining budget), failing
    /// on either side of the sleep if the budget is gone.
    pub(crate) fn sleep_poll(self, interval: Duration, operation: &str) -> Result<()> {
        self.check(operation)?;
        let remaining = self.0.saturating_duration_since(Instant::now());
        std::thread::sleep(interval.min(remaining));
        self.check(operation)
    }
}

fn check(deadline: Option<Deadline>, operation: &str) -> Result<()> {
    deadline.map_or(Ok(()), |deadline| deadline.check(operation))
}

/// Every child attached to any thread of `pid`, from
/// `<proc_root>/<pid>/task/*/children`. Reading only the thread-group
/// leader's file can miss children forked by another thread, which would
/// be a containment escape for multi-threaded tools. A task that vanishes
/// mid-read is skipped; a process with no `task` directory has no children.
///
/// `proc_root` is always the real `/proc` for containment work;
/// `agent_kind`'s detection walk shares this exact reader so its unit tests
/// can drive a synthetic tree instead of spawning processes.
pub(crate) fn direct_child_pids_in(proc_root: &Path, pid: u32) -> Result<Vec<u32>> {
    read_direct_children(proc_root, pid, None)
}

fn read_direct_children(
    proc_root: &Path,
    pid: u32,
    deadline: Option<Deadline>,
) -> Result<Vec<u32>> {
    check(deadline, "scanning process tree")?;
    let tasks_path = proc_root.join(pid.to_string()).join("task");
    let tasks = match fs::read_dir(&tasks_path) {
        Ok(tasks) => tasks,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", tasks_path.display())),
    };
    let mut children = HashSet::new();
    for task in tasks {
        check(deadline, "scanning process tree")?;
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

/// Every process under `root` that can still run code, breadth-first.
///
/// Zombies are deliberately excluded. A process that has exited but has not
/// been reaped still appears in its parent's `children` file, still answers
/// `kill(pid, 0)`, and still yields a working pidfd -- so before this filter
/// existed one unreaped descendant made `workload_populated` report the
/// containment domain permanently populated (the lifecycle could never prove
/// it empty, so the worker never finished) and made `kill_descendants` spin
/// until its timeout, because SIGKILL to a zombie changes nothing.
///
/// Skipping a zombie hides no subtree: a process's children are reparented
/// to the nearest subreaper at the moment it exits, so by the time it is a
/// zombie it has none left to walk.
pub(crate) fn descendant_pids(root: u32) -> Result<Vec<u32>> {
    walk_descendants(root, None, None)
}

/// `descendant_pids` for a bounded teardown: checks `deadline` before every
/// procfs read and refuses a tree larger than `max_descendants`, so a
/// hostile or runaway tree cannot consume every descriptor the caller
/// would pin it with.
pub(crate) fn descendant_pids_until(
    root: u32,
    deadline: Deadline,
    max_descendants: usize,
) -> Result<Vec<u32>> {
    walk_descendants(root, Some(deadline), Some(max_descendants))
}

fn walk_descendants(
    root: u32,
    deadline: Option<Deadline>,
    max_descendants: Option<usize>,
) -> Result<Vec<u32>> {
    let mut pending = VecDeque::from([root]);
    let mut seen = HashSet::from([root]);
    let mut descendants = Vec::new();
    while let Some(parent) = pending.pop_front() {
        check(deadline, "scanning process tree")?;
        for child in read_direct_children(Path::new(DEFAULT_PROC_ROOT), parent, deadline)? {
            if !seen.insert(child) || process_is_zombie(child) {
                continue;
            }
            if max_descendants.is_some_and(|max| descendants.len() >= max) {
                bail!(
                    "process tree exceeds safe descendant limit of {}",
                    max_descendants.unwrap_or(0)
                );
            }
            descendants.push(child);
            pending.push_back(child);
        }
    }
    Ok(descendants)
}

/// A pidfd pinned to one process identity: the start time read before the
/// open must match the one read after it, so a numeric pid recycled between
/// a tree walk and the open can never redirect a signal.
#[derive(Debug)]
pub(crate) struct PidHandle {
    pid: u32,
    start_time_ticks: u64,
    pidfd: File,
}

impl PidHandle {
    /// `Ok(None)` when there is no such process by the time it is looked at
    /// -- gone from `/proc`, `ESRCH` from `pidfd_open`, or a different
    /// process (a recycled pid) behind the handle just opened. Any other
    /// failure to read the identity is an error, never a silent skip.
    pub(crate) fn open(pid: u32, deadline: Option<Deadline>) -> Result<Option<Self>> {
        check(deadline, "opening process handle")?;
        let start_time_ticks = match process_start_time_ticks(pid) {
            Ok(value) => value,
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("identify process {pid}")),
        };
        check(deadline, "opening process handle")?;
        let pidfd = match pidfd_open(pid) {
            Ok(pidfd) => pidfd,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("open pidfd for {pid}")),
        };
        check(deadline, "opening process handle")?;
        // Pin first, then re-read identity.
        match process_start_time_ticks(pid) {
            Ok(current) if current == start_time_ticks => Ok(Some(Self {
                pid,
                start_time_ticks,
                pidfd,
            })),
            Ok(_) => Ok(None),
            Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => Ok(None),
            Err(error) => Err(error).with_context(|| format!("recheck process {pid} identity")),
        }
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn start_time_ticks(&self) -> u64 {
        self.start_time_ticks
    }

    /// Signal through the pidfd; a process that has already exited is not
    /// an error.
    pub(crate) fn signal(&self, signal: i32) -> Result<()> {
        send_signal(&self.pidfd, signal).with_context(|| format!("signal process {}", self.pid))
    }
}

impl AsRawFd for PidHandle {
    fn as_raw_fd(&self) -> RawFd {
        self.pidfd.as_raw_fd()
    }
}

/// `pidfd_send_signal(2)` on any pidfd. `ESRCH` -- the process exited after
/// the handle was opened -- is success: there is nothing left to signal, and
/// the handle guarantees the signal never reached anything else.
pub(crate) fn send_signal(pidfd: &File, signal: i32) -> io::Result<()> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_pins_and_probes_the_current_process() {
        let deadline = Deadline::after(Duration::from_secs(1));
        let handle = PidHandle::open(std::process::id(), Some(deadline))
            .expect("open current-process pidfd")
            .expect("current process is present");
        assert_eq!(handle.pid(), std::process::id());
        assert_eq!(
            handle.start_time_ticks(),
            process_start_time_ticks(std::process::id()).unwrap()
        );
        handle.signal(0).expect("probe pidfd_send_signal");
    }

    #[test]
    fn an_expired_deadline_stops_a_walk_before_procfs_io() {
        let deadline = Deadline::after(Duration::ZERO);
        let error = descendant_pids_until(std::process::id(), deadline, 16)
            .expect_err("expired scan must fail");
        assert!(error.to_string().contains("timed out"));
        let error = PidHandle::open(std::process::id(), Some(deadline))
            .expect_err("expired open must fail");
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn a_gone_pid_opens_to_none() {
        // A pid nobody can hold: beyond pid_max.
        assert!(PidHandle::open(u32::MAX - 1, None).unwrap().is_none());
    }
}
