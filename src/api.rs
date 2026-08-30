//! Library API used by the Python bindings and the `a` CLI.
//!
//! These functions are the source of truth. The CLI prints them; the Python
//! package calls them in-process (no subprocess of `a`).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::{
    atomic_write_json, canonical_workspace, cleanup_recorded_cgroup_until, command_exists,
    ensure_private_dir, list_records, parse_byte_size, process_start_time_ticks,
    public_session_record, read_record, session_metadata_env, validate_tag, worker_executable,
    Config, FileLock, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION,
};

struct LaunchEnvironmentGuard(PathBuf);

impl Drop for LaunchEnvironmentGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

// The worker's contained descendant sweep is bounded at two seconds. Leave
// another second for signal delivery, startup unwind, and record/fsync work.
const STARTUP_TERM_GRACE: Duration = Duration::from_secs(3);
const STARTUP_REAP_POLL: Duration = Duration::from_millis(10);
const STARTUP_CONTAINMENT_TIMEOUT: Duration = Duration::from_secs(2);
/// A corrupt or hostile startup tree must not consume every descriptor in the
/// launcher. The actual limit is reduced further to fit the launcher's live
/// RLIMIT_NOFILE budget before any process is stopped.
const STARTUP_MAX_DESCENDANTS: usize = 4096;
const STARTUP_FD_RESERVE: u64 = 16;

/// Owns every artifact created for a session until its worker is ready.
/// Normal error paths call `rollback` so cleanup failures can be reported;
/// `Drop` is the panic/early-return safety net.
struct StartupGuard<'a> {
    paths: &'a Paths,
    id: Uuid,
    child: Option<Child>,
    armed: bool,
}

impl<'a> StartupGuard<'a> {
    fn new(paths: &'a Paths, id: Uuid) -> Self {
        Self {
            paths,
            id,
            child: None,
            armed: true,
        }
    }

    fn track_child(&mut self, child: Child) {
        self.child = Some(child);
    }

    fn child_mut(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("startup child must be tracked after spawn")
    }

    fn disarm(&mut self) {
        self.armed = false;
        // Dropping Child leaves a successfully-started worker running.
        self.child.take();
    }

    fn rollback(&mut self) -> Result<()> {
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

impl Drop for StartupGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = self.rollback() {
            eprintln!(
                "aplexer: startup rollback for {} failed: {error:#}",
                self.id
            );
        }
    }
}

fn reaped_worker_cleanup_confirmed(record_path: &Path, worker_pid: u32) -> bool {
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

fn persist_independent_cleanup_proof(record_path: &Path, worker_pid: u32) -> Result<()> {
    let mut record = read_record(record_path)?;
    if record.worker_pid.is_some() && record.worker_pid != Some(worker_pid) {
        bail!("startup record worker identity changed before cleanup proof persistence");
    }
    record.phase = Phase::Failed;
    record.containment_empty = true;
    record.updated_at_ms = crate::now_ms();
    record.error.get_or_insert_with(|| {
        "worker did not complete startup; launcher independently emptied containment".into()
    });
    atomic_write_json(record_path, &record).context("persist independent containment proof")
}

fn reaped_startup_child_result(
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

fn terminate_and_reap_startup_child(
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

struct StartupDescendant {
    pid: u32,
    start_time_ticks: u64,
    pidfd: File,
}

#[derive(Clone, Copy)]
struct CleanupDeadline(Instant);

impl CleanupDeadline {
    fn after(duration: Duration) -> Self {
        Self(Instant::now() + duration)
    }

    fn check(self, operation: &str) -> Result<()> {
        if Instant::now() >= self.0 {
            bail!("timed out {operation}");
        }
        Ok(())
    }

    fn sleep_poll(self, operation: &str) -> Result<()> {
        self.check(operation)?;
        let remaining = self.0.saturating_duration_since(Instant::now());
        thread::sleep(STARTUP_REAP_POLL.min(remaining));
        self.check(operation)
    }
}

fn proc_entry_disappeared(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
    })
}

fn open_startup_descendant(
    pid: u32,
    deadline: CleanupDeadline,
) -> Result<Option<StartupDescendant>> {
    deadline.check("opening startup process handle")?;
    let start_time_ticks = match process_start_time_ticks(pid) {
        Ok(value) => value,
        Err(error) if proc_entry_disappeared(&error) => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("identify descendant {pid}")),
    };
    deadline.check("opening startup process handle")?;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("open pidfd for descendant {pid}"));
    }
    let pidfd = unsafe { File::from_raw_fd(fd) };
    deadline.check("opening startup process handle")?;
    match process_start_time_ticks(pid) {
        Ok(current) if current == start_time_ticks => {
            deadline.check("opening startup process handle")?;
            Ok(Some(StartupDescendant {
                pid,
                start_time_ticks,
                pidfd,
            }))
        }
        Ok(_) => bail!("descendant {pid} changed identity while opening its pidfd"),
        Err(error) if proc_entry_disappeared(&error) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("recheck descendant {pid} identity")),
    }
}

fn signal_startup_descendant_raw(descendant: &StartupDescendant, signal: i32) -> Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            descendant.pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error).with_context(|| format!("signal descendant {}", descendant.pid))
    }
}

fn signal_startup_descendant(
    descendant: &StartupDescendant,
    signal: i32,
    deadline: CleanupDeadline,
) -> Result<()> {
    deadline.check("signalling startup process tree")?;
    signal_startup_descendant_raw(descendant, signal)?;
    deadline.check("signalling startup process tree")
}

fn pidfd_exited(descendant: &StartupDescendant, deadline: CleanupDeadline) -> Result<bool> {
    let mut pollfd = libc::pollfd {
        fd: descendant.pidfd.as_raw_fd(),
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
                descendant.pid,
                pollfd.revents
            );
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error).context("poll descendant pidfd");
        }
    }
}

fn process_state_and_start_time(
    pid: u32,
    deadline: CleanupDeadline,
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

fn startup_descendant_quiescent(
    descendant: &StartupDescendant,
    deadline: CleanupDeadline,
) -> Result<bool> {
    if pidfd_exited(descendant, deadline)? {
        return Ok(true);
    }
    match process_state_and_start_time(descendant.pid, deadline)? {
        Some((_, start_time_ticks)) if start_time_ticks != descendant.start_time_ticks => {
            bail!(
                "descendant {} changed identity while its pidfd remained live",
                descendant.pid
            )
        }
        Some((state, _)) => Ok(matches!(state, 'T' | 't' | 'Z' | 'X' | 'x')),
        None if pidfd_exited(descendant, deadline)? => Ok(true),
        None => bail!(
            "descendant {} disappeared from /proc while its pidfd remained live",
            descendant.pid
        ),
    }
}

/// Reads children belonging to every thread in `pid`. Children forked by a
/// non-leader thread do not necessarily appear in the thread-group leader's
/// `children` file.
fn direct_startup_children(pid: u32, deadline: CleanupDeadline) -> Result<Vec<u32>> {
    deadline.check("scanning startup process tree")?;
    let tasks_path = format!("/proc/{pid}/task");
    let tasks = match fs::read_dir(&tasks_path) {
        Ok(tasks) => tasks,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {tasks_path}")),
    };
    let mut children = HashSet::new();
    for task in tasks {
        deadline.check("scanning startup process tree")?;
        let task = task.with_context(|| format!("enumerate {tasks_path}"))?;
        let Some(tid) = task
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let path = format!("/proc/{pid}/task/{tid}/children");
        deadline.check("scanning startup process tree")?;
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {path}")),
        };
        for value in text.split_whitespace() {
            deadline.check("scanning startup process tree")?;
            children.insert(
                value
                    .parse::<u32>()
                    .with_context(|| format!("parse child pid from {path}"))?,
            );
        }
    }
    deadline.check("scanning startup process tree")?;
    Ok(children.into_iter().collect())
}

fn ensure_startup_worker_stopped(
    pid: u32,
    start_time_ticks: u64,
    deadline: CleanupDeadline,
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

fn startup_descendant_pids(
    root: u32,
    root_start_time: u64,
    max_descendants: usize,
    deadline: CleanupDeadline,
) -> Result<Vec<u32>> {
    deadline.check("scanning startup process tree")?;
    ensure_startup_worker_stopped(root, root_start_time, deadline)?;
    let mut pending = VecDeque::from([root]);
    let mut seen = HashSet::from([root]);
    let mut descendants = Vec::new();
    while let Some(parent) = pending.pop_front() {
        deadline.check("scanning startup process tree")?;
        for child in direct_startup_children(parent, deadline)? {
            if seen.insert(child) {
                if descendants.len() >= max_descendants {
                    bail!(
                        "startup process tree exceeds safe descendant limit of {max_descendants}"
                    );
                }
                descendants.push(child);
                pending.push_back(child);
            }
        }
    }
    ensure_startup_worker_stopped(root, root_start_time, deadline)?;
    Ok(descendants)
}

fn wait_for_worker_stopped(
    pid: u32,
    start_time_ticks: u64,
    deadline: CleanupDeadline,
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
        deadline.sleep_poll(&format!(
            "stopping startup worker {pid} for containment inspection"
        ))?;
    }
}

fn stop_and_pin_startup_descendants(
    root: u32,
    root_start_time: u64,
    max_descendants: usize,
    deadline: CleanupDeadline,
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
            if let Some(descendant) = open_startup_descendant(pid, deadline)? {
                // Check before the destructive signal, then record the handle
                // before checking again. If the deadline crosses during the
                // syscall, the caller still owns everything it must resume.
                deadline.check("stopping startup worker descendants")?;
                signal_startup_descendant_raw(&descendant, libc::SIGSTOP)?;
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
            deadline.sleep_poll("quiescing startup worker descendants")?;
        }

        // Once every process known so far is stopped, a pass that discovers no
        // new pid closes the fork-vs-scan race: only an as-yet unknown process
        // could still have run between the earlier tree walk and SIGSTOP.
        if !discovered_new {
            return Ok(());
        }
    }
}

fn wait_for_descendant_exit(
    descendants: &BTreeMap<u32, StartupDescendant>,
    deadline: CleanupDeadline,
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
        deadline.sleep_poll("waiting for startup worker descendants to exit")?;
    }
}

fn safe_startup_descendant_capacity(deadline: CleanupDeadline) -> Result<usize> {
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

fn startup_descendant_capacity(soft_limit: u64, open_fds: u64) -> Result<usize> {
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

fn kill_stopped_startup_tree(
    worker: &StartupDescendant,
    descendants: &BTreeMap<u32, StartupDescendant>,
) -> Result<()> {
    let mut failures = Vec::new();
    // Keep the subreaper stopped until every process we pinned has been sent
    // KILL. The retained session record remains the evidence for any process
    // that was racing discovery when recovery became necessary.
    for descendant in descendants.values() {
        if let Err(error) = signal_startup_descendant_raw(descendant, libc::SIGKILL) {
            failures.push(format!(
                "kill stopped startup descendant {}: {error:#}",
                descendant.pid
            ));
        }
    }
    if let Err(error) = signal_startup_descendant_raw(worker, libc::SIGKILL) {
        failures.push(format!(
            "kill stopped startup worker {}: {error:#}",
            worker.pid
        ));
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("{}", failures.join("; "))
    }
}

fn resume_stopped_startup_tree(
    worker: &StartupDescendant,
    descendants: &BTreeMap<u32, StartupDescendant>,
) -> Result<()> {
    // Resume the subreaper first so it can immediately continue the normal
    // TERM-driven rollback path. Then release each pinned child. Every signal
    // uses a pidfd, so recovery can never target a recycled numeric PID.
    if let Err(error) = signal_startup_descendant_raw(worker, libc::SIGCONT) {
        return kill_stopped_startup_tree(worker, descendants).with_context(|| {
            format!(
                "resume startup worker {} failed ({error:#}); fallback KILL also failed",
                worker.pid
            )
        });
    }
    let mut failures = Vec::new();
    for descendant in descendants.values() {
        if let Err(error) = signal_startup_descendant_raw(descendant, libc::SIGCONT) {
            // The worker is running its requested TERM rollback again. If an
            // individual child cannot be continued, remove that stopped
            // child through its same identity-pinned handle.
            if let Err(kill_error) = signal_startup_descendant_raw(descendant, libc::SIGKILL) {
                failures.push(format!(
                    "resume startup descendant {} failed ({error:#}); fallback KILL failed: {kill_error:#}",
                    descendant.pid
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

fn hard_cleanup_startup_child(child: &mut Child, record_path: &Path) -> Result<()> {
    // All discovery, handle acquisition, signalling, and waits share this one
    // deadline. A large/forking tree cannot multiply the timeout by phases or
    // by the number of processes it creates.
    let deadline = CleanupDeadline::after(STARTUP_CONTAINMENT_TIMEOUT);
    let worker_pid = child.id();
    deadline.check("reading startup containment record")?;
    let record = read_record(record_path).context("read startup containment record")?;
    deadline.check("reading startup containment record")?;
    if record.worker_pid.is_some() && record.worker_pid != Some(worker_pid) {
        bail!("startup record no longer belongs to worker {worker_pid}");
    }
    let missing_required_cgroup = record.limits.requested() && record.containment_cgroup.is_none();
    let recorded_cgroup = record.containment_cgroup;
    let max_descendants = safe_startup_descendant_capacity(deadline)?;
    // Opening a worker pidfd and exercising pidfd_send_signal(2) with signal
    // zero proves both required syscalls and permissions before SIGSTOP can
    // make any member of the tree dependent on our recovery path.
    let worker = open_startup_descendant(worker_pid, deadline)?.ok_or_else(|| {
        anyhow!("startup worker {worker_pid} exited before containment preflight")
    })?;
    signal_startup_descendant(&worker, 0, deadline)
        .context("preflight pidfd signalling support")?;
    let worker_start_time = worker.start_time_ticks;
    let mut descendants = BTreeMap::new();
    let mut worker_stopped = false;
    let mut worker_destroyed = false;

    let cleanup = (|| -> Result<()> {
        deadline.check("stopping startup worker for containment inspection")?;
        signal_startup_descendant_raw(&worker, libc::SIGSTOP)
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
                libc::SIGKILL,
                Duration::ZERO,
                deadline.0,
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
            deadline.sleep_poll("reaping startup worker after SIGKILL")?;
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

    #[test]
    fn pidfd_preflight_can_pin_and_probe_current_process() {
        let deadline = CleanupDeadline::after(Duration::from_secs(1));
        let handle = open_startup_descendant(std::process::id(), deadline)
            .expect("open current-process pidfd")
            .expect("current process is present");
        signal_startup_descendant(&handle, 0, deadline).expect("probe pidfd_send_signal");
    }

    #[test]
    fn expired_cleanup_deadline_stops_work_before_procfs_io() {
        let deadline = CleanupDeadline(Instant::now());
        let error = direct_startup_children(std::process::id(), deadline)
            .expect_err("expired scan must fail");
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn descriptor_budget_is_finite_and_preserves_headroom() {
        let deadline = CleanupDeadline::after(Duration::from_secs(1));
        let capacity = safe_startup_descendant_capacity(deadline).expect("descriptor budget");
        assert!((1..=STARTUP_MAX_DESCENDANTS).contains(&capacity));
        assert!(capacity <= STARTUP_MAX_DESCENDANTS);
    }

    #[test]
    fn descriptor_budget_rejects_exhaustion_and_caps_large_limits() {
        let required = STARTUP_FD_RESERVE + 1;
        assert!(startup_descendant_capacity(required, 0).is_err());
        assert!(startup_descendant_capacity(required + 10, 10).is_err());
        assert_eq!(
            startup_descendant_capacity(u64::MAX, 0).expect("large descriptor budget"),
            STARTUP_MAX_DESCENDANTS
        );
    }
}

/// `start_session` makes the worker a session leader in `pre_exec`. TERM asks
/// the worker's cancellation handler to unwind startup and clean its separate
/// workload containment domain; signalling the leader's group is only the
/// last-resort way to stop the worker itself after that grace period.
fn signal_worker_group(pid: u32, signal: i32) -> io::Result<()> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "worker pid exceeds pid_t"))?;
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

pub fn engines_json(paths: &Paths) -> Result<Value> {
    let config = Config::load(paths)?;
    let values = config
        .engines
        .iter()
        .map(|(name, e)| {
            let env_unset = e.resolved_env_unset();
            json!({
                "name": name,
                "command": e.command,
                "available": command_exists(&e.command),
                "env_unset_count": env_unset.len(),
                "env_unset": env_unset,
            })
        })
        .collect::<Vec<_>>();
    Ok(Value::Array(values))
}

pub fn profiles_json(paths: &Paths) -> Result<Value> {
    let mut profiles = Config::load(paths)?.profiles;
    for profile in profiles.values_mut() {
        profile.env = session_metadata_env(&profile.env);
    }
    Ok(serde_json::to_value(profiles)?)
}

pub fn launch_spec_json(
    paths: &Paths,
    engine: Option<&str>,
    profile: Option<&str>,
    cwd: Option<&Path>,
    no_skip_permissions: bool,
) -> Result<Value> {
    let config = Config::load(paths)?;
    let workspace = canonical_workspace(Path::new("."))?;
    let launch = config.resolve(
        Vec::new(),
        engine,
        profile,
        &workspace,
        cwd,
        &BTreeMap::new(),
        &Limits::default(),
        None,
    )?;
    let mut argv = launch.command.clone();
    if !no_skip_permissions {
        argv.extend(launch.skip_permissions_argv.clone());
    }
    let cwd = canonical_workspace(&launch.cwd).unwrap_or(launch.cwd);
    Ok(json!({
        "engine": launch.engine,
        "profile": launch.profile,
        "argv": argv,
        "env_set": launch.env,
        "env_unset": launch.env_unset,
        "cwd": cwd,
    }))
}

pub fn snapshot_json(paths: &Paths, running: bool) -> Result<Value> {
    let mut records = list_records(paths)?;
    if running {
        records.retain(|r| r.worker_phase_active() && r.worker_alive());
    }
    let mut enriched = Vec::with_capacity(records.len());
    for record in &records {
        let mut value = serde_json::to_value(public_session_record(record))?;
        value["worker_alive"] = json!(record.worker_alive());
        enriched.push(value);
    }
    Ok(Value::Array(enriched))
}

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub workspace: PathBuf,
    pub tag: String,
    pub engine: Option<String>,
    pub profile: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub command: Vec<String>,
    pub memory: Option<String>,
    pub pids: Option<u64>,
    pub cpu_quota_us: Option<u64>,
    pub cpu_period_us: u64,
    pub history_bytes: Option<usize>,
    pub no_skip_permissions: bool,
    pub startup_timeout_ms: u64,
    pub worker_rows: Option<u16>,
    pub worker_cols: Option<u16>,
    /// When set, spawn the worker as `python -m aplexer worker --id …`
    /// (Python bindings). Otherwise spawn the `aplexer` worker binary.
    pub python: Option<PathBuf>,
}

pub fn start_session(paths: &Paths, req: &StartRequest) -> Result<SessionRecord> {
    validate_tag(&req.tag)?;
    let workspace = canonical_workspace(&req.workspace)?;
    let id = Uuid::new_v4();
    let limits = Limits {
        memory_bytes: req.memory.as_deref().map(parse_byte_size).transpose()?,
        pids: req.pids,
        cpu_quota_us: req.cpu_quota_us,
        cpu_period_us: req.cpu_quota_us.map(|_| req.cpu_period_us),
    };
    let config = Config::load(paths)?;
    let mut launch = config.resolve(
        req.command.clone(),
        req.engine.as_deref(),
        req.profile.as_deref(),
        &workspace,
        req.cwd.as_deref(),
        &req.env,
        &limits,
        req.history_bytes,
    )?;
    if req.command.is_empty() && !req.no_skip_permissions {
        launch
            .command
            .extend(launch.skip_permissions_argv.iter().cloned());
    }
    if !command_exists(&launch.command) {
        bail!(
            "command is not executable or was not found in PATH: {}",
            launch
                .command
                .first()
                .map(String::as_str)
                .unwrap_or("<empty>")
        );
    }
    worker_command(id, req.python.as_deref())?;
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    if let Some(existing) = list_records(paths)?
        .into_iter()
        .find(|r| r.workspace == workspace && r.tag == req.tag)
    {
        if !existing.worker_finished() {
            bail!(
                "workspace+tag already belongs to session {}; rename it or choose a different tag",
                existing.id
            );
        }
        fs::remove_dir_all(paths.state_session(existing.id))
            .with_context(|| format!("remove superseded session {}", existing.id))?;
        let _ = fs::remove_dir_all(paths.runtime_session(existing.id));
    }
    let mut startup = StartupGuard::new(paths, id);
    let result = (|| -> Result<SessionRecord> {
        ensure_private_dir(&paths.state_session(id))?;
        ensure_private_dir(&paths.runtime_session(id))?;
        // Environment values may contain credentials. Hand them to the worker
        // through a private, one-shot runtime file instead of placing them in
        // the durable/public session record returned by list/status/watch.
        let launch_environment_path = paths.runtime_session(id).join("launch-environment.json");
        atomic_write_json(&launch_environment_path, &launch.env)?;
        let _launch_environment_guard = LaunchEnvironmentGuard(launch_environment_path);
        let now = crate::now_ms();
        let record = SessionRecord {
            schema_version: SCHEMA_VERSION,
            id,
            workspace: workspace.clone(),
            tag: req.tag.clone(),
            engine: launch.engine,
            profile: launch.profile,
            command: launch.command,
            cwd: launch.cwd,
            env: session_metadata_env(&launch.env),
            env_unset: launch.env_unset,
            limits: launch.limits,
            history_bytes: launch.history_bytes,
            created_at_ms: now,
            updated_at_ms: now,
            last_activity_ms: None,
            phase: Phase::Starting,
            worker_pid: None,
            workload_pid: None,
            containment_cgroup: None,
            containment_empty: false,
            socket_path: paths.socket(id),
            history_path: paths.history(id),
            exit: None,
            error: None,
        };
        atomic_write_json(&paths.record(id), &record)?;
        let worker_log = File::create(paths.state_session(id).join("worker.log"))
            .context("create worker log")?;
        let mut command = worker_command(id, req.python.as_deref())?;
        command
            .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
            .env("APLEXER_STATE_DIR", &paths.state_root)
            .env("APLEXER_CONFIG", &paths.config_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(worker_log));
        if let (Some(rows), Some(cols)) = (req.worker_rows, req.worker_cols) {
            command
                .arg("--rows")
                .arg(rows.to_string())
                .arg("--cols")
                .arg(cols.to_string());
        }
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Close the fork/exec-to-handler race: the worker inherits
                // these blocked signals, installs cancellation handlers as
                // its first startup action, then unblocks them. A timeout can
                // therefore never deliver the default terminating action in
                // the narrow window before the handler exists.
                let mut signals: libc::sigset_t = std::mem::zeroed();
                if libc::sigemptyset(&mut signals) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::sigaddset(&mut signals, libc::SIGTERM) != 0
                    || libc::sigaddset(&mut signals, libc::SIGINT) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                let result = libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut());
                if result != 0 {
                    return Err(io::Error::from_raw_os_error(result));
                }
                Ok(())
            });
        }
        startup.track_child(command.spawn().context("spawn worker")?);
        let started = Instant::now();
        let timeout = Duration::from_millis(req.startup_timeout_ms);
        loop {
            // Check the deadline first so a zero timeout is deterministic,
            // independent of whether the worker wins the scheduling race.
            if started.elapsed() >= timeout {
                bail!(
                    "worker did not become ready within {} ms",
                    req.startup_timeout_ms
                );
            }
            if let Ok(current) = read_record(&paths.record(id)) {
                match current.phase {
                    Phase::Running | Phase::Exiting | Phase::Exited
                        if current.socket_path.exists() =>
                    {
                        return Ok(current);
                    }
                    Phase::Failed => bail!(
                        "worker startup failed: {}",
                        current.error.unwrap_or_else(|| "unknown error".into())
                    ),
                    _ => {}
                }
            }
            if let Some(status) = startup.child_mut().try_wait()? {
                bail!("worker exited during startup: {status}");
            }
            thread::sleep(Duration::from_millis(25));
        }
    })();

    match result {
        Ok(record) => {
            startup.disarm();
            Ok(record)
        }
        Err(start_error) => match startup.rollback() {
            Ok(()) => Err(start_error),
            Err(rollback_error) => Err(anyhow!(
                "startup failed: {start_error:#}; rollback also failed: {rollback_error:#}"
            )),
        },
    }
}

fn worker_command(id: Uuid, python: Option<&Path>) -> Result<Command> {
    if let Some(python) = python {
        let mut command = Command::new(python);
        command.args(["-m", "aplexer", "worker", "--id", &id.to_string()]);
        return Ok(command);
    }
    let mut command = Command::new(worker_executable()?);
    command.arg("worker").arg("--id").arg(id.to_string());
    Ok(command)
}
