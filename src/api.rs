//! Library API used by the Python bindings and the `a` CLI.
//!
//! These functions are the source of truth. The CLI prints them; the Python
//! package calls them in-process (no subprocess of `a`).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::{
    atomic_write_json, canonical_workspace, cleanup_recorded_cgroup_until, command_exists,
    ensure_private_dir, ensure_sigchld_compatible_for_child_management, frame_json,
    kill_grace_duration, list_records, parse_byte_size, process_start_time_ticks,
    public_session_record, read_frame, read_record, read_session_record, resolve_record,
    session_metadata_env, validate_tag, worker_executable, write_frame, write_json, Config,
    FileLock, FrameKind, Limits, Operation, Paths, Phase, Request, Response, SessionRecord,
    MAX_FRAME_BYTES, PROTOCOL_VERSION, SCHEMA_VERSION,
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
const WORKER_REAPER_POLL: Duration = Duration::from_millis(100);
const STARTUP_READY_RPC_SLICE: Duration = Duration::from_millis(100);
static WORKER_REAPER: Mutex<Option<mpsc::Sender<Child>>> = Mutex::new(None);

fn worker_reaper_loop(receiver: mpsc::Receiver<Child>) {
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

    /// Transfer a successfully-started worker to the shared detached waiter. The CLI
    /// normally exits long before the worker, but embedders (notably Python)
    /// can outlive many sessions; merely dropping `Child` there leaves every
    /// completed worker as a zombie owned by the host process.
    ///
    /// Keep the child in this guard until the waiter has been created and has
    /// accepted it. If either step fails, rollback still owns the process and
    /// can terminate it instead of leaking an unreapable child handle.
    fn hand_off_to_reaper(&mut self) -> Result<()> {
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
    record.containment_empty = Some(true);
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
    let missing_required_cgroup = record.limits.requested()
        && (record.containment_cgroup.is_none() || record.containment_cgroup_identity.is_none());
    let recorded_cgroup_identity = record.containment_cgroup_identity.clone();
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
                recorded_cgroup_identity.as_ref(),
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
            let env_unset = e.resolved_env_unset(name);
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

#[cfg(not(test))]
const CONTROL_RPC_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(test)]
const CONTROL_RPC_TIMEOUT: Duration = Duration::from_millis(100);

fn selected_record(paths: &Paths, selector: &str) -> Result<SessionRecord> {
    resolve_record(paths, Some(selector), None, None)
}

fn connect_control(record: &SessionRecord) -> Result<UnixStream> {
    let deadline = Instant::now() + CONTROL_RPC_TIMEOUT;
    let stream = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("connect {} timed out", record.socket_path.display());
        }
        match connect_startup_control(&record.socket_path, remaining) {
            Ok(stream) => break stream,
            Err(error)
                if error.raw_os_error() == Some(libc::EAGAIN) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("connect {}", record.socket_path.display()))
            }
        }
    };
    stream
        .set_read_timeout(Some(CONTROL_RPC_TIMEOUT))
        .context("set worker response deadline")?;
    stream
        .set_write_timeout(Some(CONTROL_RPC_TIMEOUT))
        .context("set worker request deadline")?;
    Ok(stream)
}

fn rpc_simple(record: &SessionRecord, operation: Operation, data: Option<&[u8]>) -> Result<Value> {
    let mut stream = connect_control(record)?;
    let request = Request::new(record.id, operation);
    let request_id = request.request_id.clone();
    write_json(&mut stream, &request)?;
    if let Some(data) = data {
        write_frame(&mut stream, FrameKind::Data, data)?;
    }
    let frame = read_frame(&mut stream)?.ok_or_else(|| anyhow!("worker closed connection"))?;
    let response: Response = frame_json(frame).context("parse worker response")?;
    if response.version != PROTOCOL_VERSION {
        bail!("worker response used unsupported protocol version");
    }
    if response.request_id != request_id {
        bail!("worker response request id mismatch");
    }
    response.into_result()
}

/// Return the live session record when reachable, or the persisted record plus
/// explicit reachability evidence when the worker cannot answer.
pub fn status_json(paths: &Paths, selector: &str) -> Result<Value> {
    let persisted = selected_record(paths, selector)?;
    let (mut value, current, worker_reachable, rpc_error) =
        match rpc_simple(&persisted, Operation::Status, None) {
            Ok(value) => {
                let current: SessionRecord = serde_json::from_value(value.clone())
                    .context("worker returned an invalid status record")?;
                (value, current, true, None)
            }
            Err(error) => {
                let value = serde_json::to_value(public_session_record(&persisted))?;
                (value, persisted.clone(), false, Some(format!("{error:#}")))
            }
        };
    value["worker_alive"] = json!(current.worker_alive());
    value["worker_reachable"] = json!(worker_reachable);
    if let Some(error) = rpc_error {
        value["rpc_error"] = json!(error);
    }
    Ok(value)
}

/// Send bytes without transcoding, splitting only at the framing limit.
pub fn send_bytes(paths: &Paths, selector: &str, data: &[u8]) -> Result<usize> {
    let record = selected_record(paths, selector)?;
    for chunk in data.chunks(MAX_FRAME_BYTES) {
        let result = rpc_simple(&record, Operation::Send { bytes: chunk.len() }, Some(chunk))?;
        let reported = result
            .get("bytes")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| anyhow!("worker send response omitted byte count"))?;
        if reported != chunk.len() {
            bail!(
                "worker send byte count mismatch: sent {}, acknowledged {reported}",
                chunk.len()
            );
        }
    }
    Ok(data.len())
}

fn rpc_capture(record: &SessionRecord, max_bytes: Option<usize>) -> Result<Vec<u8>> {
    let mut stream = connect_control(record)?;
    let request = Request::new(record.id, Operation::Capture { max_bytes });
    let request_id = request.request_id.clone();
    write_json(&mut stream, &request)?;
    let response: Response = frame_json(
        read_frame(&mut stream)?.ok_or_else(|| anyhow!("worker closed before capture response"))?,
    )
    .context("parse worker capture response")?;
    if response.version != PROTOCOL_VERSION {
        bail!("worker response used unsupported protocol version");
    }
    if response.request_id != request_id {
        bail!("worker response request id mismatch");
    }
    let result = response.into_result()?;
    let reported = result
        .get("bytes")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| anyhow!("worker capture response omitted byte count"))?;
    let frame =
        read_frame(&mut stream)?.ok_or_else(|| anyhow!("worker closed before capture data"))?;
    if frame.kind != FrameKind::Data {
        bail!("worker returned a non-data capture frame");
    }
    if frame.payload.len() != reported {
        bail!(
            "worker capture byte count mismatch: reported {reported}, returned {}",
            frame.payload.len()
        );
    }
    Ok(frame.payload)
}

fn read_history_tail(path: &Path, requested: Option<usize>) -> Result<Vec<u8>> {
    let limit = requested.unwrap_or(MAX_FRAME_BYTES).min(MAX_FRAME_BYTES);
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let length = file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?
        .len();
    let count = length.min(limit as u64);
    file.seek(SeekFrom::Start(length - count))
        .with_context(|| format!("seek {}", path.display()))?;
    let mut bytes = Vec::with_capacity(count as usize);
    file.take(count)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read tail of {}", path.display()))?;
    Ok(bytes)
}

/// Capture live history bytes, falling back to the bounded persisted tail only
/// when the worker is terminal or known gone.
pub fn capture_bytes(paths: &Paths, selector: &str, max_bytes: Option<usize>) -> Result<Vec<u8>> {
    let record = selected_record(paths, selector)?;
    match rpc_capture(&record, max_bytes) {
        Ok(data) => Ok(data),
        Err(_) if record.worker_finished() || !record.worker_alive() => {
            read_history_tail(&record.history_path, max_bytes)
                .context("worker unavailable and persisted history cannot be read")
        }
        Err(error) => Err(error).context(
            "capture RPC failed while the worker process is still alive; refusing to return potentially stale persisted history",
        ),
    }
}

/// Ask the live worker to stop its complete workload containment domain.
pub fn kill_session(paths: &Paths, selector: &str, signal: i32, grace_ms: u64) -> Result<()> {
    if !(1..=64).contains(&signal) {
        bail!("signal out of range");
    }
    kill_grace_duration(grace_ms)?;
    let record = selected_record(paths, selector)?;
    let result = rpc_simple(&record, Operation::Kill { signal, grace_ms }, None)?;
    if result.get("signalled").and_then(Value::as_bool) != Some(true) {
        bail!("worker kill response omitted confirmation");
    }
    Ok(())
}

/// Forget a session record without signalling any process. This preserves the
/// CLI's registry/startup locking and refuses a worker that may still be live.
pub fn forget_session(paths: &Paths, selector: &str, force: bool) -> Result<Value> {
    if !force {
        bail!("forget requires force=True");
    }
    let selected = selected_record(paths, selector)?;
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    let current = read_record(&paths.record(selected.id))
        .with_context(|| format!("re-read session {} before forgetting", selected.id))?;
    if current.worker_alive() {
        bail!(
            "session {} still has a live worker; refusing to forget it",
            current.id
        );
    }
    let _startup_absence_lock = if current.worker_phase_active() && current.worker_pid.is_none() {
        let lock_path = paths.worker_lock(current.id);
        match FileLock::exclusive(&lock_path, true) {
            Ok(lock) => Some(lock),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .and_then(io::Error::raw_os_error)
                    .is_some_and(|code| code == libc::EAGAIN || code == libc::EWOULDBLOCK) =>
            {
                bail!(
                    "session {} still has a worker holding {}; refusing to forget it",
                    current.id,
                    lock_path.display()
                )
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot fence session {}'s pre-PID worker; refusing to forget it",
                        current.id
                    )
                })
            }
        }
    } else {
        None
    };

    let containment_proven_empty = current.containment_proven_empty();
    match fs::remove_dir_all(paths.runtime_session(current.id)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove forgotten session runtime state"),
    }
    fs::remove_dir_all(paths.state_session(current.id))
        .with_context(|| format!("remove forgotten session {} durable state", current.id))?;
    Ok(json!({
        "id": current.id,
        "forgotten": true,
        "signalled": false,
        "containment_proven_empty": containment_proven_empty,
        "workload_may_survive": !containment_proven_empty,
    }))
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

fn connect_startup_control(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "startup control connection deadline expired",
        ));
    }
    let path_bytes = path.as_os_str().as_bytes();
    CString::new(path_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path contains NUL"))?;
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path_bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    unsafe {
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr(),
            address.sun_path.as_mut_ptr().cast::<u8>(),
            path_bytes.len(),
        );
    }
    let address_len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path_bytes.len() + 1)
        as libc::socklen_t;
    let raw_fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let connected = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            address_len,
        )
    };
    if connected != 0 {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => {}
            Some(libc::EINPROGRESS) | Some(libc::EALREADY) => {
                let timeout_ms = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
                let mut poll_fd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                loop {
                    let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
                    if ready > 0 {
                        break;
                    }
                    if ready == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("connect {} timed out", path.display()),
                        ));
                    }
                    let poll_error = io::Error::last_os_error();
                    if poll_error.kind() != io::ErrorKind::Interrupted {
                        return Err(poll_error);
                    }
                }
                let mut socket_error: libc::c_int = 0;
                let mut socket_error_len = std::mem::size_of_val(&socket_error) as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        fd.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        (&raw mut socket_error).cast::<libc::c_void>(),
                        &raw mut socket_error_len,
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if socket_error != 0 {
                    return Err(io::Error::from_raw_os_error(socket_error));
                }
            }
            // Linux AF_UNIX uses EAGAIN for a full listen backlog. Returning
            // immediately lets the outer startup loop retry without ever
            // blocking past its absolute deadline.
            _ => return Err(error),
        }
    }
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { UnixStream::from_raw_fd(fd.into_raw_fd()) })
}

/// A pathname and a persisted phase are not readiness evidence. Complete a
/// framed request/response round trip and require the worker to identify the
/// exact session the launcher just spawned.
fn probe_worker_ready(
    record: &SessionRecord,
    expected_id: Uuid,
    timeout: Duration,
) -> Result<bool> {
    let mut stream = match connect_startup_control(&record.socket_path, timeout) {
        Ok(stream) => stream,
        Err(_) => return Ok(false),
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = Request::new(expected_id, Operation::Ping);
    let request_id = request.request_id.clone();
    if write_json(&mut stream, &request).is_err() {
        return Ok(false);
    }
    let frame = match read_frame(&mut stream) {
        Ok(Some(frame)) => frame,
        Ok(None) | Err(_) => return Ok(false),
    };
    let response: Response = frame_json(frame).context("parse worker readiness response")?;
    if response.version != PROTOCOL_VERSION {
        bail!("worker readiness response used unsupported protocol version");
    }
    if response.request_id != request_id {
        bail!("worker readiness response request id mismatch");
    }
    let result = response
        .into_result()
        .context("worker readiness Ping failed")?;
    if result.get("pong").and_then(Value::as_bool) != Some(true) {
        bail!("worker readiness response omitted pong");
    }
    let reported_id = result
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("worker readiness response omitted session id"))?
        .parse::<Uuid>()
        .context("parse worker readiness session id")?;
    if reported_id != expected_id {
        bail!("worker readiness response identified session {reported_id}, expected {expected_id}");
    }
    Ok(true)
}

pub fn start_session(paths: &Paths, req: &StartRequest) -> Result<SessionRecord> {
    ensure_sigchld_compatible_for_child_management()?;
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
    let superseded = list_records(paths)?
        .into_iter()
        .find(|r| r.workspace == workspace && r.tag == req.tag);
    if let Some(existing) = &superseded {
        if !existing.worker_finished() {
            bail!(
                "workspace+tag already belongs to session {}; rename it or choose a different tag",
                existing.id
            );
        }
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
            containment_cgroup_identity: None,
            containment_empty: Some(false),
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
            let current = read_session_record(paths, id).context("read worker startup record")?;
            match current.phase {
                Phase::Running | Phase::Exiting | Phase::Exited if current.socket_path.exists() => {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    let probe_timeout = remaining.min(STARTUP_READY_RPC_SLICE);
                    if probe_worker_ready(&current, id, probe_timeout)? {
                        // The Ping response is the readiness commit. Read once
                        // more so a very short-lived workload can return its
                        // newest durable phase.
                        return read_session_record(paths, id).or(Ok(current));
                    }
                }
                Phase::Failed => bail!(
                    "worker startup failed: {}",
                    current.error.unwrap_or_else(|| "unknown error".into())
                ),
                _ => {}
            }
            if let Some(status) = startup.child_mut().try_wait()? {
                bail!("worker exited during startup: {status}");
            }
            thread::sleep(Duration::from_millis(25));
        }
    })();

    match result {
        Ok(record) => {
            startup.hand_off_to_reaper()?;
            // Keep a finished predecessor's post-mortem evidence until the
            // replacement has completed startup and its worker is safely
            // owned by the detached reaper. A failed replacement therefore
            // cannot destroy history, final screen, or transcript bindings.
            if let Some(existing) = superseded {
                if let Err(error) = fs::remove_dir_all(paths.state_session(existing.id)) {
                    eprintln!(
                        "aplexer: replacement {} is ready, but superseded session {} could not be removed: {error}",
                        record.id, existing.id
                    );
                } else {
                    let _ = fs::remove_dir_all(paths.runtime_session(existing.id));
                }
            }
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
