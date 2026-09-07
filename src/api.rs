//! Library API used by the Python bindings and the `a` CLI.
//!
//! These functions are the source of truth. The CLI prints them; the Python
//! package calls them in-process (no subprocess of `a`).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::ffi::CString;
use std::fs::{self, File};
use std::io;
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

use crate::agent_kind::{detect_agent, AgentKind, DEFAULT_PROC_ROOT};
use crate::{
    atomic_write_json, canonical_workspace, cleanup_recorded_cgroup_until, command_exists,
    ensure_private_dir, ensure_sigchld_compatible_for_child_management, frame_json,
    kill_grace_duration, list_records, parse_byte_size, process_start_time_ticks,
    public_session_record, read_frame, read_persisted_history_tail, read_record,
    read_session_record, reap_verdict, resolve_record, session_metadata_env, validate_tag,
    worker_executable, write_frame, write_json, Config, ContainmentReap, FileLock, FrameKind,
    Limits, Operation, Paths, Phase, Request, Response, SessionRecord, MAX_FRAME_BYTES,
    PROTOCOL_VERSION, SCHEMA_VERSION,
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
const RETIRED_SESSIONS_DIR: &str = "retired-sessions";
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
    use crate::ExitInfo;

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

    fn startup_record(
        phase: Phase,
        exit: Option<ExitInfo>,
        containment_empty: Option<bool>,
    ) -> SessionRecord {
        SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id: Uuid::nil(),
            workspace: PathBuf::from("/ws"),
            tag: "main".into(),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/true".into()],
            cwd: PathBuf::from("/ws"),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: 1024,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase,
            worker_pid: Some(1),
            workload_pid: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty,
            socket_path: PathBuf::from("/ws/control.sock"),
            history_path: PathBuf::from("/ws/history.bin"),
            exit,
            error: None,
        }
    }

    /// `(description, phase, exit, containment_empty, expected_accept)`.
    type AcceptanceCase = (&'static str, Phase, Option<ExitInfo>, Option<bool>, bool);

    fn clean_exit() -> Option<ExitInfo> {
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
    fn exited_worker_startup_acceptance_matrix() {
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

/// Which agent is running inside `record`'s workload process tree right now
/// (see `crate::agent_kind`), or `None` when nothing recognisable is there.
///
/// Detection is query-time only and never persisted: the record on disk has
/// no `agent` field, so it can never be stale. Two guards keep the answer
/// honest rather than merely present:
///
/// * A record whose phase is already terminal (`exited`/`failed`) is not
///   probed at all. Its `workload_pid` names a process that is gone, and a
///   recycled numeric pid could otherwise make an unrelated `claude` on the
///   box look like this dead session's agent.
/// * A record with no recorded `workload_pid` has no handle to walk.
pub fn record_agent(record: &SessionRecord) -> Option<AgentKind> {
    if !record.worker_phase_active() {
        return None;
    }
    detect_agent(Path::new(DEFAULT_PROC_ROOT), record.workload_pid?)
}

pub fn snapshot_json(paths: &Paths, running: bool) -> Result<Value> {
    let mut records = list_records(paths)?;
    if running {
        records.retain(|r| r.worker_phase_active() && r.worker_alive());
    }
    let mut enriched = Vec::with_capacity(records.len());
    // One clock for the whole snapshot, so two rows created in the same
    // instant cannot land on opposite sides of the startup window.
    let now = crate::now_ms();
    for record in &records {
        let mut value = serde_json::to_value(public_session_record(record))?;
        let worker_alive = record.worker_alive();
        value["worker_alive"] = json!(worker_alive);
        // The derived liveness fact, identical to `a status`'s `state:`
        // line (see `observed_state`). Machine consumers were previously
        // handed only the persisted `phase`, which a killed worker leaves
        // at "running" forever, so a zombie record was indistinguishable
        // from a live session on the wire.
        value["state"] = json!(crate::observed_state(
            &record.phase,
            worker_alive,
            record.created_at_ms,
            now
        ));
        // Which agent is live inside the session right now, detected from the
        // workload's process tree at query time (`record_agent`). Always
        // present, `null` when no agent is detectable -- every pocketshell
        // session is `engine: "shell"` with the agent started by hand inside
        // it, so `engine` cannot answer this and a consumer needs one key it
        // can read unconditionally.
        value["agent"] = json!(record_agent(record));
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
    // Same query-time agent detection every `a list --json`/`a snapshot` row
    // carries, so the two commands cannot disagree about which agent is in a
    // session.
    value["agent"] = json!(record_agent(&current));
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

/// Capture live history bytes, falling back to the bounded persisted tail only
/// when the worker is terminal or known gone.
pub fn capture_bytes(paths: &Paths, selector: &str, max_bytes: Option<usize>) -> Result<Vec<u8>> {
    let record = selected_record(paths, selector)?;
    match rpc_capture(&record, max_bytes) {
        Ok(data) => Ok(data),
        Err(_) if record.worker_finished() || !record.worker_alive() => {
            read_persisted_history_tail(&record.history_path, max_bytes)
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

/// Result of fencing the spawn-to-worker-lock gap for one record.
pub(crate) enum PrePidFence {
    /// Nothing can come up under this record while the guard (if any) lives.
    /// `None` means no fence was needed: the record is past the gap, so its
    /// `worker_pid` is the authority and `worker_alive()` already answered.
    Fenced(Option<FileLock>),
    /// A worker exists for this record even though it has not registered a
    /// pid yet, so `worker_alive()` reads false for a session that is very
    /// much coming up. Carries the lock path for the caller's diagnostic.
    WorkerHoldsLock(PathBuf),
}

/// Fence a record whose worker may have been spawned but has not reached its
/// first required lock yet.
///
/// `start_session` writes a `Starting` record with `worker_pid: None` before
/// it spawns anything, so for a few tens of milliseconds a perfectly healthy
/// session is on disk as `phase: starting, worker_pid: null` -- and
/// `worker_alive()` is false for a `None` pid. Any predicate that reads "no
/// live worker" as "free to destroy" will happily take that session's state
/// out from under a live spawn. The worker's own first action is to acquire
/// `paths.worker_lock(id)` exclusively, so holding that lock is both the
/// detector (it is already held => a worker exists) and the fence (we hold
/// it => a worker that has not got there yet will fail its acquisition and
/// cannot proceed after we have destroyed the record).
///
/// Callers must keep the returned guard alive across every removal, exactly
/// as `a forget` does. `rename`'s claim check (issue #13) uses the same
/// fence for the same reason, holding it across its record update: its
/// verdict must not read a coming-up session as free either, and while
/// rename destroys nothing, holding the lock keeps a worker that has not
/// reached its acquisition yet from coming up on top of the pair the
/// rename just handed out.
pub(crate) fn fence_pre_pid_worker(paths: &Paths, record: &SessionRecord) -> Result<PrePidFence> {
    if !record.worker_phase_active() || record.worker_pid.is_some() {
        return Ok(PrePidFence::Fenced(None));
    }
    let lock_path = paths.worker_lock(record.id);
    match FileLock::exclusive(&lock_path, true) {
        Ok(lock) => Ok(PrePidFence::Fenced(Some(lock))),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .and_then(io::Error::raw_os_error)
                .is_some_and(|code| code == libc::EAGAIN || code == libc::EWOULDBLOCK) =>
        {
            Ok(PrePidFence::WorkerHoldsLock(lock_path))
        }
        Err(error) => Err(error),
    }
}

/// Forget a session record without signalling any process.
///
/// The one implementation of `a forget`: the force gate, the live-worker
/// refusal, the pre-PID worker fence, both directory removals, and the
/// "workload processes may survive" warning live only here. `a forget`'s
/// `cmd_forget` resolves the CLI's target spellings and then calls this, so
/// the Python binding cannot diverge from the CLI on the operation with the
/// least recoverable outcome (issue #11).
pub fn forget_session(paths: &Paths, selector: &str, force: bool) -> Result<Value> {
    if !force {
        // Names both spellings because both callers land here: `--force` on
        // the CLI, `force=True` from the Python binding.
        bail!("forget requires --force (force=True from the Python binding)");
    }
    let selected = selected_record(paths, selector)?;
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    // Resolve happened before taking the registry lock. Re-read under the
    // lock so a concurrent rename or lifecycle update cannot make a stale
    // liveness decision destructive.
    let current = read_record(&paths.record(selected.id))
        .with_context(|| format!("re-read session {} before forgetting", selected.id))?;
    if current.worker_alive() {
        bail!(
            "session {} still has a live worker; refusing to forget it",
            current.id
        );
    }
    let _startup_absence_lock = match fence_pre_pid_worker(paths, &current).with_context(|| {
        format!(
            "cannot fence session {}'s pre-PID worker; refusing to forget it",
            current.id
        )
    })? {
        PrePidFence::Fenced(lock) => lock,
        PrePidFence::WorkerHoldsLock(lock_path) => bail!(
            "session {} still has a worker holding {}; refusing to forget it",
            current.id,
            lock_path.display()
        ),
    };

    let containment_proven_empty = current.containment_proven_empty();
    match fs::remove_dir_all(paths.runtime_session(current.id)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove forgotten session runtime state"),
    }
    fs::remove_dir_all(paths.state_session(current.id))
        .with_context(|| format!("remove forgotten session {} durable state", current.id))?;

    // The record is gone, so this warning is the only remaining trace that
    // uncontained workload processes may still be running. It goes to stderr
    // rather than into the JSON alone so a CLI user cannot miss it; the
    // `workload_may_survive` key carries the same fact for programmatic
    // callers.
    let workload_may_survive = !containment_proven_empty;
    if workload_may_survive {
        eprintln!(
            "a: forgot session {} without signalling any process; containment was not proven empty, so workload processes may survive",
            current.id
        );
    } else {
        eprintln!(
            "a: forgot session {} without signalling any process (containment was proven empty)",
            current.id
        );
    }
    Ok(json!({
        "id": current.id,
        "forgotten": true,
        "signalled": false,
        "containment_proven_empty": containment_proven_empty,
        "workload_may_survive": workload_may_survive,
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
    /// Never fail because the requested `workspace+tag` is live: when that
    /// pair is held by a session `start_session` would refuse to supersede,
    /// claim the next free `<tag>-2`, `<tag>-3`, … suffix instead. This is
    /// what makes `a new` mean "another session in this workspace" (where
    /// `a here` means create-or-attach), and it is decided under the registry
    /// lock, so the caller cannot race another start into its suffix.
    pub fresh: bool,
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

fn archive_superseded_session(paths: &Paths, id: Uuid) -> Result<PathBuf> {
    let retired_root = paths.state_root.join(RETIRED_SESSIONS_DIR);
    ensure_private_dir(&retired_root)?;
    let source = paths.state_session(id);
    let archived = retired_root.join(id.to_string());
    if archived.try_exists()? {
        bail!(
            "cannot retire superseded session {id}: archive {} already exists",
            archived.display()
        );
    }
    fs::rename(&source, &archived).with_context(|| {
        format!(
            "atomically retire superseded session {id} from {} to {}",
            source.display(),
            archived.display()
        )
    })?;
    if let Err(error) = (|| -> Result<()> {
        File::open(paths.state_root.join("sessions"))?.sync_all()?;
        File::open(&retired_root)?.sync_all()?;
        Ok(())
    })() {
        return match restore_superseded_session(paths, id, &archived) {
            Ok(()) => Err(error).context("sync retired predecessor transaction"),
            Err(restore_error) => Err(anyhow!(
                "sync retired predecessor transaction: {error:#}; restore also failed: {restore_error:#}"
            )),
        };
    }
    Ok(archived)
}

/// Retire the predecessor that `start_session` decided it could take the
/// `workspace+tag` from, re-deciding against the record as it stands on disk
/// right now.
///
/// The verdict formed before the spawn is advisory by construction: a worker
/// startup can take seconds, and `reap_verdict` is built out of live probes
/// (`/proc` liveness for the worker and the workload leader, and the
/// kernel's own view of a recorded cgroup), none of which the registry lock
/// freezes. It holds off other aplexer commands, not the world: a recycled
/// pid can make a dead `workload_pid` read alive again, and a containment
/// domain the caller could not inspect a moment ago may answer now. So
/// re-read and re-run the same predicate before destroying anything -- the
/// same rule `a prune`'s `reap_session_state` follows, for the same reason.
///
/// Refusing here is a start FAILURE, not a silent downgrade: the caller
/// rolls the freshly started replacement back rather than leaving two
/// durable records claiming one selector.
fn archive_reclaimed_predecessor(paths: &Paths, existing: &SessionRecord) -> Result<PathBuf> {
    let current = read_session_record(paths, existing.id).with_context(|| {
        format!(
            "re-read superseded session {} before retiring it",
            existing.id
        )
    })?;
    if reap_verdict(&current).is_none() {
        bail!(
            "superseded session {} is live again (state: {}); refusing to retire it",
            current.id,
            current.observed_state()
        );
    }
    archive_superseded_session(paths, existing.id)
}

fn restore_superseded_session(paths: &Paths, id: Uuid, archived: &Path) -> Result<()> {
    let destination = paths.state_session(id);
    fs::rename(archived, &destination).with_context(|| {
        format!(
            "restore superseded session {id} from {} to {}",
            archived.display(),
            destination.display()
        )
    })?;
    File::open(paths.state_root.join("sessions"))?.sync_all()?;
    if let Some(parent) = archived.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn cleanup_superseded_archive(path: &Path) -> Result<()> {
    #[cfg(feature = "startup-test-hooks")]
    if std::env::var_os("APLEXER_TEST_FAIL_SUPERSEDED_CLEANUP").is_some() {
        bail!("injected superseded-session cleanup failure");
    }
    fs::remove_dir_all(path).with_context(|| format!("remove archive {}", path.display()))?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

/// Forces the fast-workload startup interleaving that `start_session`'s
/// readiness poll can otherwise only lose by chance: block until the worker
/// has finished its job, unlinked its control socket and exited, so the poll
/// loop below can never observe a live socket to Ping.
///
/// Timing alone does not reproduce this on an idle machine -- 25+ repetitions
/// of the fast-workload test pass locally -- which is exactly how the
/// readiness-Ping gate reached a release with this ordering unhandled. The
/// non-default `startup-test-hooks` feature is the authorization boundary for
/// pinning it; default and release builds do not contain this path.
#[cfg(feature = "startup-test-hooks")]
fn await_worker_exit_before_readiness_poll(
    startup: &mut StartupGuard<'_>,
    paths: &Paths,
    id: Uuid,
) -> Result<()> {
    if std::env::var_os("APLEXER_TEST_AWAIT_WORKER_EXIT_BEFORE_READINESS_POLL").is_none() {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // `Child::try_wait` caches the reaped status, so the poll loop's own
        // `try_wait` still observes this exit rather than an "already reaped"
        // error.
        let exited = startup.child_mut().try_wait()?.is_some();
        if exited && !paths.socket(id).exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("test hook timed out waiting for worker {id} to exit before readiness poll");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Whether a worker that has already exited left durable proof that its
/// session started and ran, rather than failing during startup.
///
/// `start_session` commits readiness by Pinging the worker's live control
/// socket. A fast workload (a one-shot shell command, an agent binary that
/// rejects its config immediately) can run to completion, persist its exit,
/// unlink that socket and exit inside a single 25ms poll interval, so the
/// readiness arm never gets a socket to Ping. The durable terminal record is
/// the evidence the vanished socket can no longer provide, and it is strictly
/// stronger: it is the same record a successful Ping would have returned.
///
/// Deliberately narrow -- every other shape still fails startup:
///
/// * a non-terminal phase (`Starting`/`Running`): a worker that vanished
///   without recording that it ever ran;
/// * `Phase::Failed`: the worker's own recorded failure, reported with its
///   reason by the poll loop's `Failed` arm rather than laundered into a
///   completed session here;
/// * a terminal phase with no `exit`: nothing proves the workload ran;
/// * `containment_empty` not explicitly `Some(true)`: no durable proof that
///   the containment domain is empty.
///
/// The caller additionally requires the worker's own exit status to be zero,
/// because a crashed worker is a startup failure whatever its record claims.
///
/// `Phase::Exiting` is accepted for symmetry with the readiness arm above,
/// which treats `Running | Exiting | Exited` alike. That combination is
/// currently unreachable -- `run_lifecycle` writes `Exiting` in exactly one
/// place, with `exit` still `None` -- but it is pinned by the table test
/// below so a future writer cannot change the answer silently.
///
/// The containment conjunct is the local enforcement of this function's
/// safety property. Today it is implied: `run_lifecycle` writes the one and
/// only production record carrying `exit` in a single update that also sets
/// `containment_empty`, and it selects `Phase::Exited` exactly when that
/// lifecycle recorded no error -- which it can only do after observing the
/// domain empty. Asserting it here turns that cross-file induction into an
/// invariant checked on a record already in hand, so a future lifecycle
/// change that wrote `Exited` with an unproven domain would fail startup
/// instead of silently reporting success for a session with an escaped
/// descendant.
///
/// `containment_empty` is `Option<bool>` only for on-disk records written
/// before the field existed (see `SessionRecord::containment_proven_empty`).
/// It cannot be `None` here: `start_session` writes the initial record with
/// an explicit `Some(false)`, and every later writer -- the worker's
/// lifecycle and startup-failure paths, and this process's own
/// `persist_independent_cleanup_proof` -- writes an explicit `Some`. The
/// record read here was written by the worker this same call just spawned,
/// so the legacy shape is unreachable and the strict `Some(true)` comparison
/// cannot reject a genuinely completed session.
fn exited_worker_completed_startup(record: &SessionRecord) -> bool {
    matches!(record.phase, Phase::Exiting | Phase::Exited)
        && record.exit.is_some()
        && record.containment_empty == Some(true)
}

/// Reconstruct a start response for a worker that finished cleanly and
/// deleted its own record (natural exit, Ctrl-D, or an in-startup kill).
/// The on-disk record is already gone; this is only what `a start --json`
/// returns to the client that launched it.
fn auto_removed_completion(mut record: SessionRecord) -> SessionRecord {
    if !matches!(record.phase, Phase::Exited) {
        record.phase = Phase::Exited;
    }
    record.containment_empty = Some(true);
    if record.exit.is_none() {
        record.exit = Some(crate::ExitInfo {
            code: None,
            signal: None,
            oom_killed: false,
            exited_at_ms: crate::now_ms(),
        });
    }
    record
}

/// The `SessionRecord::parent_session` value for a session being started
/// here: the calling process's ambient `APLEXER_SESSION_ID` stamp (see
/// `discover_session_id`, which also walks ancestor environments), kept only
/// when it names a record that still exists. Deliberately infallible -- a
/// stale stamp (parent already killed/forgotten, a leftover export, an
/// unparsable value) means "no recorded lineage", never a failed start.
fn resolve_parent_session(paths: &Paths) -> Option<Uuid> {
    let parent = crate::discover_session_id()?;
    read_session_record(paths, parent).map(|_| parent).ok()
}

/// The tag a `--fresh` start should claim: the requested base itself while
/// nothing live holds it, otherwise the first `<base>-2`, `<base>-3`, …
/// suffix no live session holds either. "Live" here means exactly what the
/// supersede check in `start_session` refuses to take -- a holder
/// `reap_verdict` would hand over does not count, so a dead `main-2` is
/// reclaimed under its own name rather than skipped. `None` means no
/// candidate fits `validate_tag` any more, which for a valid base can only
/// be the 64-byte length cap.
pub fn pick_fresh_tag(records: &[SessionRecord], workspace: &Path, base: &str) -> Option<String> {
    // Any live holder counts, not just the first record with the pair: a
    // rename that took a dead holder's name leaves the corpse next to the
    // live session (issue #13), and `--fresh` must read that pair as taken.
    let live_holder = |tag: &str| {
        records
            .iter()
            .filter(|r| r.workspace == workspace && r.tag == tag)
            .any(|r| crate::reap_verdict(r).is_none())
    };
    // Suffixes start at 2: a bare `main` plus `main-2` reads as "the main
    // one and its first sibling", not as an off-by-one list. A base that
    // already ends in `-<number>` (or cannot be suffixed numerically at all)
    // simply continues from the next integer.
    let mut candidate = base.to_string();
    while live_holder(&candidate) {
        candidate = match candidate
            .rsplit_once('-')
            .and_then(|(stem, n)| n.parse::<u64>().ok().map(|n| format!("{stem}-{}", n + 1)))
        {
            Some(next) => next,
            None => format!("{base}-2"),
        };
        if validate_tag(&candidate).is_err() {
            return None;
        }
    }
    Some(candidate)
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
    // Read under the registry lock taken above, and keep holding it through
    // the whole spawn: this read IS the locked read, and no other aplexer
    // command can modify the registry until this call returns.
    let registry = list_records(paths)?;
    // The pair can be held by more than one record: `a rename` takes a pair
    // from a dead holder but leaves the corpse in place for `a prune`
    // (issue #13), so "the holder" must not be whoever `read_dir` lists
    // first. A live holder always wins -- it is who the supersede check
    // below refuses to displace -- and only when every holder is reclaimable
    // does the first dead one become the predecessor this start archives.
    let holder_of = |tag: &str| {
        let mut holders = registry
            .iter()
            .filter(|r| r.workspace == workspace && r.tag == tag);
        holders
            .find(|r| crate::reap_verdict(r).is_none())
            .or_else(|| {
                registry
                    .iter()
                    .find(|r| r.workspace == workspace && r.tag == tag)
            })
    };
    let mut tag = req.tag.clone();
    // Held for the rest of the call when the predecessor is a pre-PID stub,
    // so a worker that was spawned into it cannot come up on top of the
    // state we are about to archive and delete.
    let mut _superseded_fence: Option<FileLock> = None;
    let mut reclaim: Option<ContainmentReap> = None;
    if req.fresh {
        // `--fresh` promises "always creates": a requested pair held by
        // something live is not an error, it is a reason to move to the next
        // free suffix. A pair that is free, or held only by a record
        // `reap_verdict` would hand over, keeps the exact requested tag --
        // the reclaim path below already owns taking those.
        let Some(chosen) = pick_fresh_tag(&registry, &workspace, &req.tag) else {
            bail!(
                "no free tag: every `{0}`, `{0}-2`, `{0}-3`, … candidate in this \
                 workspace is taken or would exceed the tag length limit",
                req.tag
            );
        };
        if chosen != req.tag {
            tag = chosen;
        }
    }
    let superseded = holder_of(&tag).cloned();
    if let Some(existing) = &superseded {
        // Taking this pair means archiving and then DELETING the holder's
        // durable state -- the same destruction `a prune` performs -- so it
        // must clear the same bar, `reap_verdict`. `worker_finished()`, the
        // old test, required a terminal phase that a SIGKILLed worker never
        // gets to write, so a zombie (worker dead, `phase` stuck at
        // `running`) held its `workspace+tag` forever and `a start` could
        // only succeed if something else pruned it first.
        let Some(verdict) = reap_verdict(existing) else {
            bail!(
                "workspace+tag already belongs to session {} (state: {}); rename it or choose a different tag",
                existing.id,
                existing.observed_state()
            );
        };
        _superseded_fence = match fence_pre_pid_worker(paths, existing).with_context(|| {
            format!("cannot fence session {}'s pre-PID worker", existing.id)
        })? {
            PrePidFence::Fenced(lock) => lock,
            PrePidFence::WorkerHoldsLock(lock_path) => bail!(
                "workspace+tag already belongs to session {}, whose worker still holds {}; rename it or choose a different tag",
                existing.id,
                lock_path.display()
            ),
        };
        reclaim = Some(verdict);
    }
    let mut startup = StartupGuard::new(paths, id);
    let result = (|| -> Result<SessionRecord> {
        ensure_private_dir(&paths.state_session(id))?;
        ensure_private_dir(&paths.runtime_session(id))?;
        // Environment values may contain credentials. Hand them to the worker
        // through a private, one-shot runtime file instead of placing them in
        // the durable/public session record returned by list/status/watch.
        //
        // Written WITHOUT fsync (benchmark PLAN P0.3): this file is deleted
        // by the guard below as soon as startup completes, so crash
        // durability across it is not required -- a crash before the worker
        // reads it just fails this start, which rollback cleans up. The
        // session record below keeps its full fsync + parent-sync durability
        // (see worker_startup_transaction tests). Saves two fsyncs on every
        // `a start`. Mode 0600: it carries secrets.
        let launch_environment_path = paths.runtime_session(id).join("launch-environment.json");
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&launch_environment_path)
                .with_context(|| format!("create {}", launch_environment_path.display()))?;
            serde_json::to_writer_pretty(&mut file, &launch.env)
                .with_context(|| format!("write {}", launch_environment_path.display()))?;
            use std::io::Write as _;
            file.write_all(b"\n")?;
        }
        let _launch_environment_guard = LaunchEnvironmentGuard(launch_environment_path);
        let now = crate::now_ms();
        let parent_session = resolve_parent_session(paths);
        let record = SessionRecord {
            parent_session,
            schema_version: SCHEMA_VERSION,
            id,
            workspace: workspace.clone(),
            tag,
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
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
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
        #[cfg(feature = "startup-test-hooks")]
        await_worker_exit_before_readiness_poll(&mut startup, paths, id)?;
        let started = Instant::now();
        let timeout = Duration::from_millis(req.startup_timeout_ms);
        let mut last_seen = record.clone();
        loop {
            // Check the deadline first so a zero timeout is deterministic,
            // independent of whether the worker wins the scheduling race.
            if started.elapsed() >= timeout {
                bail!(
                    "worker did not become ready within {} ms",
                    req.startup_timeout_ms
                );
            }
            // A clean finish deletes the record (natural exit, Ctrl-D, kill).
            // That is success for this start, not "worker vanished".
            if !paths.record(id).exists() {
                if let Some(status) = startup.child_mut().try_wait()? {
                    if status.success() {
                        return Ok(auto_removed_completion(last_seen));
                    }
                    bail!("worker exited during startup: {status}");
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let current = read_session_record(paths, id).context("read worker startup record")?;
            last_seen = current.clone();
            match current.phase {
                Phase::Running | Phase::Exiting | Phase::Exited if current.socket_path.exists() => {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    let probe_timeout = remaining.min(STARTUP_READY_RPC_SLICE);
                    if probe_worker_ready(&current, id, probe_timeout)? {
                        // The Ping response is the readiness commit. Read once
                        // more so a very short-lived workload can return its
                        // newest durable phase -- or report the auto-removed
                        // completion if the worker already deleted the record.
                        return match read_session_record(paths, id) {
                            Ok(latest) => Ok(latest),
                            Err(_) if !paths.record(id).exists() => {
                                Ok(auto_removed_completion(current))
                            }
                            Err(error) => {
                                Err(error).context("read worker startup record after ready ping")
                            }
                        };
                    }
                }
                Phase::Failed => bail!(
                    "worker startup failed: {}",
                    current.error.unwrap_or_else(|| "unknown error".into())
                ),
                _ => {}
            }
            if let Some(status) = startup.child_mut().try_wait()? {
                // A worker that exited cleanly after durably recording a
                // completed session did not fail to start (see
                // `exited_worker_completed_startup`). The same is true of
                // a worker that finished and auto-removed its record: that
                // deletion is itself the proof the lifecycle completed.
                // An unreadable-but-still-present record falls through to
                // the failure below rather than replacing the startup
                // diagnosis with a read error.
                if status.success() {
                    if !paths.record(id).exists() {
                        return Ok(auto_removed_completion(current));
                    }
                    if let Ok(final_record) = read_session_record(paths, id) {
                        if exited_worker_completed_startup(&final_record) {
                            return Ok(final_record);
                        }
                    }
                }
                bail!("worker exited during startup: {status}");
            }
            // Polled at 5 ms, not 25 ms (benchmark PLAN P0.3): worker startup
            // is a serial chain (record write -> spawn -> PTY -> workload ->
            // Running record -> Ping), and every 25 ms quantum here is pure
            // launcher idle time on top of it. Short-lived and infrequent --
            // one wait per `a start`.
            thread::sleep(Duration::from_millis(5));
        }
    })();

    match result {
        Ok(record) => {
            // Move the predecessor out of the active registry atomically
            // before handing off the replacement. Until this succeeds the
            // startup guard can still roll the new worker back without ever
            // exposing two durable records for one selector.
            let archived = if let Some(existing) = &superseded {
                match archive_reclaimed_predecessor(paths, existing) {
                    Ok(path) => Some(path),
                    Err(retire_error) => {
                        return match startup.rollback() {
                            Ok(()) => Err(retire_error),
                            Err(rollback_error) => Err(anyhow!(
                                "retire predecessor failed: {retire_error:#}; replacement rollback also failed: {rollback_error:#}"
                            )),
                        };
                    }
                }
            } else {
                None
            };

            if let Err(handoff_error) = startup.hand_off_to_reaper() {
                let restore_error = match (&superseded, &archived) {
                    (Some(existing), Some(archived)) => {
                        restore_superseded_session(paths, existing.id, archived).err()
                    }
                    _ => None,
                };
                let rollback_error = startup.rollback().err();
                let mut message = format!("hand off replacement worker: {handoff_error:#}");
                if let Some(error) = restore_error {
                    message.push_str(&format!("; restore predecessor failed: {error:#}"));
                }
                if let Some(error) = rollback_error {
                    message.push_str(&format!("; replacement rollback failed: {error:#}"));
                }
                bail!(message);
            }

            if let (Some(existing), Some(archived)) = (superseded, archived) {
                if let Err(error) = cleanup_superseded_archive(&archived) {
                    bail!(
                        "replacement {} is ready and manageable by UUID, but superseded session {} cleanup failed; its remaining evidence is retained at {}: {error:#}",
                        record.id,
                        existing.id,
                        archived.display()
                    );
                }
                let _ = fs::remove_dir_all(paths.runtime_session(existing.id));
                // Say plainly when the pair was taken from a record whose
                // worker never proved its containment domain empty, exactly
                // as `a prune` reports the same class of removal. The
                // predecessor's manual-investigation trail is gone with it,
                // and a caller that only ever sees a successful `a start`
                // would otherwise have no way to know that happened.
                if reclaim == Some(ContainmentReap::NoRemainingHandle) {
                    eprintln!(
                        "a: reclaimed workspace+tag from broken session {} without a containment proof; its worker died without recording one and nothing addressable remained",
                        existing.id
                    );
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

#[cfg(test)]
mod fresh_tag_tests {
    use super::*;

    /// A record liveness is decided by pid probes (`reap_verdict`), so a
    /// "live" holder only needs a pid that exists -- the test process's own
    /// -- and a reclaimable one needs no pids plus an empty-containment
    /// shape, exactly like `mod reclaim_tests`' zombie fixture.
    fn record(workspace: &str, tag: &str, worker_pid: Option<u32>) -> SessionRecord {
        SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id: Uuid::new_v4(),
            workspace: PathBuf::from(workspace),
            tag: tag.to_string(),
            engine: "shell".to_string(),
            profile: None,
            command: vec![],
            cwd: PathBuf::from(workspace),
            env: Default::default(),
            env_unset: Default::default(),
            limits: Default::default(),
            history_bytes: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase: Phase::Running,
            worker_pid,
            workload_pid: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(false),
            socket_path: PathBuf::from("/nonexistent"),
            history_path: PathBuf::from("/nonexistent"),
            exit: None,
            error: None,
        }
    }

    fn live(workspace: &str, tag: &str) -> SessionRecord {
        record(workspace, tag, Some(std::process::id()))
    }

    fn dead(workspace: &str, tag: &str) -> SessionRecord {
        record(workspace, tag, None)
    }

    #[test]
    fn free_base_is_used_verbatim() {
        let ws = Path::new("/ws");
        assert_eq!(pick_fresh_tag(&[], ws, "main"), Some("main".into()));
    }

    #[test]
    fn live_base_moves_to_the_next_free_suffix() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "main")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main-2".into()));
    }

    #[test]
    fn suffix_walk_skips_taken_numbers() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "main"), live("/ws", "main-2")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main-3".into()));
    }

    #[test]
    fn reclaimable_holder_keeps_the_requested_tag() {
        // A dead `main` is not "someone else's session": the ordinary
        // reclaim path takes the exact name, so `--fresh` must not skip it.
        let ws = Path::new("/ws");
        let records = vec![dead("/ws", "main")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main".into()));
    }

    #[test]
    fn reclaimable_suffix_is_taken_under_its_own_name() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "main"), dead("/ws", "main-2")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main-2".into()));
    }

    #[test]
    fn other_workspaces_do_not_count() {
        let ws = Path::new("/ws");
        let records = vec![live("/elsewhere", "main"), live("/elsewhere", "main-2")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main".into()));
    }

    #[test]
    fn base_with_trailing_number_increments_from_it() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "review-2")];
        assert_eq!(
            pick_fresh_tag(&records, ws, "review-2"),
            Some("review-3".into())
        );
    }

    #[test]
    fn length_capped_base_reports_no_candidate() {
        let ws = Path::new("/ws");
        let base = "a".repeat(64);
        let records = vec![live("/ws", &base)];
        assert_eq!(pick_fresh_tag(&records, ws, &base), None);
    }
}

#[cfg(test)]
mod reclaim_tests {
    use super::*;
    use crate::{atomic_write_json, ContainmentReap};

    /// A registry containing exactly one record, with its paths wired to the
    /// throwaway state/runtime roots so `read_session_record`'s identity
    /// checks accept it.
    fn seeded_registry(
        record: &mut SessionRecord,
    ) -> (Paths, tempfile::TempDir, tempfile::TempDir) {
        let state_dir = tempfile::tempdir().unwrap();
        let runtime_dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: runtime_dir.path().to_path_buf(),
            state_root: state_dir.path().to_path_buf(),
            config_file: state_dir.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        record.socket_path = paths.socket(record.id);
        record.history_path = paths.history(record.id);
        fs::create_dir_all(paths.state_session(record.id)).unwrap();
        fs::create_dir_all(paths.runtime_session(record.id)).unwrap();
        atomic_write_json(&paths.record(record.id), record).unwrap();
        (paths, state_dir, runtime_dir)
    }

    /// The reported zombie shape: worker dead, `phase` stuck at `running`,
    /// nothing left running.
    fn zombie_record() -> SessionRecord {
        SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id: Uuid::new_v4(),
            workspace: PathBuf::from("/ws/zombie"),
            tag: "zt".to_string(),
            engine: "shell".to_string(),
            profile: None,
            command: vec![],
            cwd: PathBuf::from("/ws/zombie"),
            env: Default::default(),
            env_unset: Default::default(),
            limits: Default::default(),
            history_bytes: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase: Phase::Running,
            worker_pid: None,
            workload_pid: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(false),
            socket_path: PathBuf::from("/nonexistent"),
            history_path: PathBuf::from("/nonexistent"),
            exit: None,
            error: None,
        }
    }

    /// A reclaimable predecessor is retired by the ordinary archive
    /// transaction: durable state moves to `retired-sessions/<id>`, nothing
    /// is deleted yet, so the caller can still restore it.
    #[test]
    fn a_reclaimable_predecessor_is_archived_not_destroyed() {
        let mut record = zombie_record();
        let (paths, _state, _runtime) = seeded_registry(&mut record);
        assert!(reap_verdict(&record).is_some());

        let archived = archive_reclaimed_predecessor(&paths, &record).expect("archive predecessor");
        assert!(archived.join("session.json").exists(), "archive is empty");
        assert!(!paths.state_session(record.id).exists());

        restore_superseded_session(&paths, record.id, &archived).expect("restore predecessor");
        assert!(paths.record(record.id).exists());
    }

    /// The verdict `start_session` forms before it spawns is stale by
    /// construction: worker startup takes time, and `reap_verdict` is built
    /// from live probes the registry lock does not freeze (`/proc` liveness
    /// and the kernel's view of a cgroup). So the record is re-read and
    /// re-judged immediately before it is retired.
    ///
    /// Driven here through the fact that can genuinely change under a held
    /// registry lock: the workload leader pid coming back alive (a recycled
    /// pid). The caller's copy still says "dead, reclaimable"; disk says a
    /// process is running; the retire must refuse and leave the predecessor
    /// exactly where it was.
    #[test]
    fn retiring_a_predecessor_re_reads_the_record_before_destroying_it() {
        let stale = zombie_record();
        let mut on_disk = stale.clone();
        let mut leader = Command::new("sleep").arg("30").spawn().unwrap();
        on_disk.workload_pid = Some(leader.id());
        let (paths, _state, _runtime) = seeded_registry(&mut on_disk);

        // What the caller believes, formed before the spawn.
        assert_eq!(
            reap_verdict(&stale),
            Some(ContainmentReap::NoRemainingHandle)
        );

        let error = archive_reclaimed_predecessor(&paths, &stale)
            .expect_err("retire must refuse a predecessor that is live on disk");
        let error = format!("{error:#}");
        assert!(error.contains("is live again"), "{error}");
        assert!(error.contains(&stale.id.to_string()), "{error}");
        assert!(
            paths.record(stale.id).exists(),
            "a refused retire still moved the predecessor's durable state"
        );
        assert!(
            !paths
                .state_root
                .join(RETIRED_SESSIONS_DIR)
                .join(stale.id.to_string())
                .exists(),
            "a refused retire stranded the predecessor in the archive"
        );
        assert!(
            leader.try_wait().unwrap().is_none(),
            "the retire path must never signal anything"
        );

        // Same record, leader gone: reclaimable again. Proves the refusal
        // came from the re-read and not from a blanket refusal.
        leader.kill().unwrap();
        leader.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::process_alive(on_disk.workload_pid.unwrap()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        archive_reclaimed_predecessor(&paths, &stale).expect("archive once the leader is gone");
        assert!(!paths.state_session(stale.id).exists());
    }

    /// The fence itself, without a spawn: a record in the spawn-to-worker-lock
    /// gap (`phase: starting, worker_pid: null`) reads as `worker_alive:
    /// false` and is otherwise perfectly reclaimable, so only the worker
    /// lock stands between a live spawn and having its state taken.
    #[test]
    fn a_pre_pid_record_is_fenced_by_its_worker_lock() {
        let mut record = zombie_record();
        record.phase = Phase::Starting;
        let (paths, _state, _runtime) = seeded_registry(&mut record);
        assert!(!record.worker_alive());
        assert!(reap_verdict(&record).is_some());

        let held = FileLock::exclusive(&paths.worker_lock(record.id), true).unwrap();
        assert!(matches!(
            fence_pre_pid_worker(&paths, &record).unwrap(),
            PrePidFence::WorkerHoldsLock(_)
        ));
        drop(held);
        assert!(matches!(
            fence_pre_pid_worker(&paths, &record).unwrap(),
            PrePidFence::Fenced(Some(_))
        ));

        // Past the gap, the pid is the authority and no fence is taken --
        // otherwise every ordinary reclaim would contend on a lock the live
        // worker legitimately holds.
        let mut registered = record.clone();
        registered.worker_pid = Some(std::process::id());
        assert!(matches!(
            fence_pre_pid_worker(&paths, &registered).unwrap(),
            PrePidFence::Fenced(None)
        ));
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
