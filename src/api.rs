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
    atomic_write_json, canonical_workspace, command_exists, ensure_private_dir, list_records,
    parse_byte_size, process_start_time_ticks, public_session_record, read_record,
    session_metadata_env, validate_tag, worker_executable, Config, FileLock, Limits, Paths, Phase,
    SessionRecord, SCHEMA_VERSION,
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
    // An ordinary (non-cgroup) startup failure written with no workload pid
    // proves there was no workload containment to leak. A terminal lifecycle
    // record's ExitInfo is written only after the fail-closed containment-empty
    // check. Limited startup failures remain ambiguous because the systemd
    // scope can exist before workload_pid is assigned.
    (record.phase == Phase::Failed && record.workload_pid.is_none() && !record.limits.requested())
        || (matches!(record.phase, Phase::Exited | Phase::Failed) && record.exit.is_some())
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

    match hard_cleanup_startup_child(child) {
        Ok(()) => true,
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

fn proc_entry_disappeared(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
    })
}

fn open_startup_descendant(pid: u32) -> Result<Option<StartupDescendant>> {
    let start_time_ticks = match process_start_time_ticks(pid) {
        Ok(value) => value,
        Err(error) if proc_entry_disappeared(&error) => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("identify descendant {pid}")),
    };
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("open pidfd for descendant {pid}"));
    }
    let pidfd = unsafe { File::from_raw_fd(fd) };
    match process_start_time_ticks(pid) {
        Ok(current) if current == start_time_ticks => Ok(Some(StartupDescendant {
            pid,
            start_time_ticks,
            pidfd,
        })),
        Ok(_) => bail!("descendant {pid} changed identity while opening its pidfd"),
        Err(error) if proc_entry_disappeared(&error) => Ok(None),
        Err(error) => Err(error).with_context(|| format!("recheck descendant {pid} identity")),
    }
}

fn signal_startup_descendant(descendant: &StartupDescendant, signal: i32) -> Result<()> {
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

fn pidfd_exited(descendant: &StartupDescendant) -> Result<bool> {
    let mut pollfd = libc::pollfd {
        fd: descendant.pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
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

fn process_state_and_start_time(pid: u32) -> Result<Option<(char, u64)>> {
    let stat_path = format!("/proc/{pid}/stat");
    let stat = match fs::read_to_string(&stat_path) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {stat_path}")),
    };
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

fn startup_descendant_quiescent(descendant: &StartupDescendant) -> Result<bool> {
    if pidfd_exited(descendant)? {
        return Ok(true);
    }
    match process_state_and_start_time(descendant.pid)? {
        Some((_, start_time_ticks)) if start_time_ticks != descendant.start_time_ticks => {
            bail!(
                "descendant {} changed identity while its pidfd remained live",
                descendant.pid
            )
        }
        Some((state, _)) => Ok(matches!(state, 'T' | 't' | 'Z' | 'X' | 'x')),
        None if pidfd_exited(descendant)? => Ok(true),
        None => bail!(
            "descendant {} disappeared from /proc while its pidfd remained live",
            descendant.pid
        ),
    }
}

/// Reads children belonging to every thread in `pid`. Children forked by a
/// non-leader thread do not necessarily appear in the thread-group leader's
/// `children` file.
fn direct_startup_children(pid: u32) -> Result<Vec<u32>> {
    let tasks_path = format!("/proc/{pid}/task");
    let tasks = match fs::read_dir(&tasks_path) {
        Ok(tasks) => tasks,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {tasks_path}")),
    };
    let mut children = HashSet::new();
    for task in tasks {
        let task = task.with_context(|| format!("enumerate {tasks_path}"))?;
        let Some(tid) = task
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let path = format!("/proc/{pid}/task/{tid}/children");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {path}")),
        };
        for value in text.split_whitespace() {
            children.insert(
                value
                    .parse::<u32>()
                    .with_context(|| format!("parse child pid from {path}"))?,
            );
        }
    }
    Ok(children.into_iter().collect())
}

fn ensure_startup_worker_stopped(pid: u32, start_time_ticks: u64) -> Result<()> {
    match process_state_and_start_time(pid)? {
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

fn startup_descendant_pids(root: u32, root_start_time: u64) -> Result<Vec<u32>> {
    ensure_startup_worker_stopped(root, root_start_time)?;
    let mut pending = VecDeque::from([root]);
    let mut seen = HashSet::from([root]);
    let mut descendants = Vec::new();
    while let Some(parent) = pending.pop_front() {
        for child in direct_startup_children(parent)? {
            if seen.insert(child) {
                descendants.push(child);
                pending.push_back(child);
            }
        }
    }
    ensure_startup_worker_stopped(root, root_start_time)?;
    Ok(descendants)
}

fn wait_for_worker_stopped(pid: u32, start_time_ticks: u64, deadline: Instant) -> Result<()> {
    loop {
        match process_state_and_start_time(pid)? {
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
        if Instant::now() >= deadline {
            bail!("timed out stopping startup worker {pid} for containment inspection");
        }
        thread::sleep(STARTUP_REAP_POLL);
    }
}

fn stop_and_pin_startup_descendants(
    root: u32,
    root_start_time: u64,
    deadline: Instant,
) -> Result<BTreeMap<u32, StartupDescendant>> {
    let mut descendants = BTreeMap::new();
    loop {
        let mut discovered_new = false;
        for pid in startup_descendant_pids(root, root_start_time)? {
            if descendants.contains_key(&pid) {
                continue;
            }
            if let Some(descendant) = open_startup_descendant(pid)? {
                signal_startup_descendant(&descendant, libc::SIGSTOP)?;
                descendants.insert(pid, descendant);
                discovered_new = true;
            }
        }

        loop {
            let mut all_stopped = true;
            for descendant in descendants.values() {
                all_stopped &= startup_descendant_quiescent(descendant)?;
            }
            if all_stopped {
                break;
            }
            if Instant::now() >= deadline {
                bail!("timed out quiescing startup worker descendants");
            }
            thread::sleep(STARTUP_REAP_POLL);
        }

        // Once every process known so far is stopped, a pass that discovers no
        // new pid closes the fork-vs-scan race: only an as-yet unknown process
        // could still have run between the earlier tree walk and SIGSTOP.
        if !discovered_new {
            return Ok(descendants);
        }
        if Instant::now() >= deadline {
            bail!("timed out stabilizing startup worker descendant tree");
        }
    }
}

fn wait_for_descendant_exit(
    descendants: &BTreeMap<u32, StartupDescendant>,
    deadline: Instant,
) -> Result<()> {
    loop {
        let mut all_exited = true;
        for descendant in descendants.values() {
            all_exited &= pidfd_exited(descendant)?;
        }
        if all_exited {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out killing startup worker descendants");
        }
        thread::sleep(STARTUP_REAP_POLL);
    }
}

fn hard_cleanup_startup_child(child: &mut Child) -> Result<()> {
    let worker_pid = child.id();
    let worker_start_time = process_start_time_ticks(worker_pid)
        .with_context(|| format!("identify startup worker {worker_pid}"))?;
    if unsafe { libc::kill(worker_pid as libc::pid_t, libc::SIGSTOP) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("stop startup worker {worker_pid}"));
    }
    wait_for_worker_stopped(
        worker_pid,
        worker_start_time,
        Instant::now() + STARTUP_CONTAINMENT_TIMEOUT,
    )?;

    let descendants = stop_and_pin_startup_descendants(
        worker_pid,
        worker_start_time,
        Instant::now() + STARTUP_CONTAINMENT_TIMEOUT,
    )?;
    for descendant in descendants.values() {
        signal_startup_descendant(descendant, libc::SIGKILL)?;
    }
    wait_for_descendant_exit(&descendants, Instant::now() + STARTUP_CONTAINMENT_TIMEOUT)?;

    // No stopped descendant can fork, and every pinned descendant has exited.
    // One final complete tree walk proves that no unpinned process was missed
    // before destroying the subreaper root that keeps the tree discoverable.
    let remaining = startup_descendant_pids(worker_pid, worker_start_time)?;
    if remaining.iter().any(|pid| !descendants.contains_key(pid)) {
        bail!("startup worker descendant tree changed after quiescence");
    }

    signal_worker_group(worker_pid, libc::SIGKILL)
        .with_context(|| format!("kill startup worker session {worker_pid}"))?;
    let deadline = Instant::now() + STARTUP_CONTAINMENT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) => {
                return Err(error).with_context(|| format!("reap startup worker {worker_pid}"));
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out reaping startup worker {worker_pid} after SIGKILL");
        }
        thread::sleep(STARTUP_REAP_POLL);
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
