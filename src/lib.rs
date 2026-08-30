#![cfg(target_os = "linux")]

pub mod agent_events;
pub mod api;
pub mod messaging;
pub mod screen;
pub mod watch;
pub mod worker;

#[cfg(feature = "python")]
mod python;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;
pub const PROTOCOL_VERSION: u16 = 1;
pub const DEFAULT_HISTORY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_CGROUP_RECOVERY_MEMBERS: usize = 4096;
const MAX_CGROUP_PROCS_BYTES: u64 = 128 * 1024;
const CGROUP_RECOVERY_FD_RESERVE: u64 = 16;
/// Long enough for graceful shutdown, but bounded so an authenticated local
/// client cannot monopolize a worker's serialized kill path indefinitely.
pub const MAX_KILL_GRACE_MS: u64 = 30_000;

pub fn kill_grace_duration(grace_ms: u64) -> Result<Duration> {
    if grace_ms > MAX_KILL_GRACE_MS {
        bail!("kill grace exceeds maximum of {MAX_KILL_GRACE_MS} ms");
    }
    Ok(Duration::from_millis(grace_ms))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Session identity for `a whoami` / bare `a transcript` / messaging.
///
/// Prefer `APLEXER_SESSION_ID` on this process (the worker stamps it on the
/// workload). If a tool subprocess cleared its environment, walk parent
/// `/proc/<pid>/environ` until we find the stamp -- agent CLIs often spawn
/// `bash`/`env -i` without passing the aplexer vars through.
pub fn discover_session_id() -> Option<Uuid> {
    parse_session_id_env(env::var_os("APLEXER_SESSION_ID"))
        .or_else(session_id_from_ancestor_environ)
}

fn parse_session_id_env(raw: Option<std::ffi::OsString>) -> Option<Uuid> {
    let raw = raw?.into_string().ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse().ok()
}

fn session_id_from_ancestor_environ() -> Option<Uuid> {
    let mut pid = proc_ppid(std::process::id())?;
    for _ in 0..64 {
        if pid == 0 {
            break;
        }
        if let Some(id) = session_id_in_proc_environ(pid) {
            return Some(id);
        }
        let next = proc_ppid(pid)?;
        if next == pid {
            break;
        }
        pid = next;
    }
    None
}

fn proc_ppid(pid: u32) -> Option<u32> {
    let text = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

fn session_id_in_proc_environ(pid: u32) -> Option<Uuid> {
    let bytes = fs::read(format!("/proc/{pid}/environ")).ok()?;
    for entry in bytes.split(|b| *b == 0) {
        let Ok(s) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some(val) = s.strip_prefix("APLEXER_SESSION_ID=") {
            if let Ok(id) = val.parse() {
                return Some(id);
            }
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub runtime_root: PathBuf,
    pub state_root: PathBuf,
    pub config_file: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let uid = unsafe { libc::geteuid() };
        let runtime_root = if let Some(value) = env::var_os("APLEXER_RUNTIME_DIR") {
            PathBuf::from(value)
        } else if let Some(value) = env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(value).join("aplexer")
        } else {
            PathBuf::from(format!("/tmp/aplexer-{uid}"))
        };
        let state_root = if let Some(value) = env::var_os("APLEXER_STATE_DIR") {
            PathBuf::from(value)
        } else if let Some(value) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(value).join("aplexer")
        } else {
            home_dir()?.join(".local/state/aplexer")
        };
        let config_file = if let Some(value) = env::var_os("APLEXER_CONFIG") {
            PathBuf::from(value)
        } else if let Some(value) = env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(value).join("aplexer/config.toml")
        } else {
            home_dir()?.join(".config/aplexer/config.toml")
        };
        let paths = Self {
            runtime_root,
            state_root,
            config_file,
        };
        paths.ensure()?;
        Ok(paths)
    }

    pub fn ensure(&self) -> Result<()> {
        ensure_private_dir(&self.runtime_root)?;
        ensure_private_dir(&self.runtime_root.join("sessions"))?;
        ensure_private_dir(&self.state_root)?;
        ensure_private_dir(&self.state_root.join("sessions"))?;
        Ok(())
    }

    pub fn runtime_session(&self, id: Uuid) -> PathBuf {
        self.runtime_root.join("sessions").join(id.to_string())
    }
    pub fn state_session(&self, id: Uuid) -> PathBuf {
        self.state_root.join("sessions").join(id.to_string())
    }
    pub fn socket(&self, id: Uuid) -> PathBuf {
        self.runtime_session(id).join("control.sock")
    }
    pub fn record(&self, id: Uuid) -> PathBuf {
        self.state_session(id).join("session.json")
    }
    pub fn history(&self, id: Uuid) -> PathBuf {
        self.state_session(id).join("history.bin")
    }
    /// The dead-session fallback for `a capture --screen` (design doc
    /// section 5.5) -- the plain-text screen as it looked the moment the
    /// worker exited, written once by `OutputHub::finish`.
    pub fn screen_txt(&self, id: Uuid) -> PathBuf {
        self.state_session(id).join("screen.txt")
    }
    pub fn worker_lock(&self, id: Uuid) -> PathBuf {
        self.runtime_session(id).join("worker.lock")
    }
    pub fn registry_lock(&self) -> PathBuf {
        self.state_root.join("registry.lock")
    }
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("directory path is empty");
    }

    // Walk the path a component at a time. `create_dir_all` followed by a
    // path-based chmod leaves a check/use gap in which the final component
    // can be replaced with a symlink, causing us to chmod its target. Keeping
    // every component pinned by a directory fd, and refusing symlinks at each
    // `openat`, makes both creation and the eventual chmod refer to the inode
    // we actually inspected.
    let base = if path.is_absolute() { "/" } else { "." };
    let mut directory = open_directory_at(libc::AT_FDCWD, OsStr::new(base))
        .with_context(|| format!("open directory base for {}", path.display()))?;
    for component in path.components() {
        let name = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir => OsStr::new(".."),
            Component::Normal(name) => name,
            Component::Prefix(_) => unreachable!("Unix paths have no prefix component"),
        };
        let name_c = CString::new(name.as_bytes()).context("directory component contains NUL")?;
        let next = match open_directory_at(directory.as_raw_fd(), name) {
            Ok(next) => next,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                if unsafe { libc::mkdirat(directory.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
                    let mkdir_error = io::Error::last_os_error();
                    if mkdir_error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(mkdir_error)
                            .with_context(|| format!("create {}", path.display()));
                    }
                }
                open_directory_at(directory.as_raw_fd(), name)
                    .with_context(|| format!("open newly-created {}", path.display()))?
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("open {} without following symbolic links", path.display())
                });
            }
        };
        directory = next;
    }

    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(directory.as_raw_fd(), &mut stat) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("inspect {}", path.display()));
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFDIR {
        bail!("{} is not a real directory", path.display());
    }
    let uid = unsafe { libc::geteuid() };
    if stat.st_uid != uid {
        bail!(
            "{} is owned by uid {}, expected {}",
            path.display(),
            stat.st_uid,
            uid
        );
    }
    if unsafe { libc::fchmod(directory.as_raw_fd(), 0o700) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("chmod 0700 {}", path.display()));
    }
    Ok(())
}

fn open_directory_at(parent_fd: RawFd, name: &OsStr) -> io::Result<File> {
    let name = CString::new(name.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    fs::canonicalize(&absolute).or_else(|_| {
        let parent = absolute
            .parent()
            .ok_or_else(|| anyhow!("invalid workspace"))?;
        let leaf = absolute
            .file_name()
            .ok_or_else(|| anyhow!("invalid workspace"))?;
        Ok::<PathBuf, anyhow::Error>(fs::canonicalize(parent)?.join(leaf))
    })
}

pub fn validate_tag(tag: &str) -> Result<()> {
    if tag.is_empty() || tag.len() > 64 {
        bail!("tag must contain 1..64 bytes");
    }
    if !tag
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("tag may contain only ASCII letters, digits, '.', '_' and '-'");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_period_us: Option<u64>,
}

impl Limits {
    pub fn requested(&self) -> bool {
        self.memory_bytes.is_some() || self.pids.is_some() || self.cpu_quota_us.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Running,
    Exiting,
    Exited,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub oom_killed: bool,
    pub exited_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub schema_version: u32,
    pub id: Uuid,
    pub workspace: PathBuf,
    pub tag: String,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Vars the worker must strip from the spawned workload's environment
    /// (see `ResolvedLaunch::env_unset`; `#[serde(default)]` so records
    /// written before this field existed still parse, as an empty list --
    /// no retroactive unsetting for already-running/old sessions).
    #[serde(default)]
    pub env_unset: Vec<String>,
    #[serde(default)]
    pub limits: Limits,
    pub history_bytes: usize,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Last time the workload's PTY produced output, as observed by the
    /// worker (see worker.rs's periodic debounce thread) -- deliberately a
    /// separate field from `updated_at_ms`, which already fires on unrelated
    /// record writes (phase transitions, rename, ...) and would be a noisy,
    /// misleading proxy for "is this session's PTY currently active" if
    /// reused for that purpose. `a watch`'s `agent.state` heuristic
    /// (running/waiting) is built on this field; see its doc comments for
    /// the important caveat that PTY-output recency is a coarse proxy for
    /// activity, not true agent-semantic state (spec.md section 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<u64>,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_pid: Option<u32>,
    /// Kernel containment domain for resource-limited sessions. Recovery
    /// code must validate this path against the session id before using it;
    /// an absent locator is never equivalent to an empty domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment_cgroup: Option<PathBuf>,
    /// Durable proof that the worker (or an independent cgroup recovery)
    /// observed the complete containment domain empty. Numeric leader PIDs
    /// are not such proof because descendants may call `setsid` and outlive
    /// both the original process group and the worker.
    #[serde(default)]
    pub containment_empty: bool,
    pub socket_path: PathBuf,
    pub history_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Environment entries that are session metadata rather than launch
/// secrets. Transcript discovery needs these profile-specific roots after
/// the worker exits; every other launch value remains one-shot/private.
const SESSION_METADATA_ENV_KEYS: &[&str] = &["CLAUDE_CONFIG_DIR", "CODEX_HOME", "GROK_HOME"];

pub fn session_metadata_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.iter()
        .filter(|(key, _)| SESSION_METADATA_ENV_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

pub fn public_session_record(record: &SessionRecord) -> SessionRecord {
    let mut public = record.clone();
    public.env = session_metadata_env(&public.env);
    public
}

impl SessionRecord {
    pub fn selector(&self) -> String {
        format!("{}:{}", self.workspace.display(), self.tag)
    }

    /// New records persist `containment_empty` directly. Before that field
    /// existed, ExitInfo was written only after the lifecycle loop observed
    /// the full subreaper/cgroup domain empty, so it remains a valid legacy
    /// proof. A failed/starting record without either signal stays ambiguous.
    pub fn containment_proven_empty(&self) -> bool {
        self.containment_empty || self.exit.is_some()
    }

    /// Whether the worker process is present in `/proc`. This is a cheap,
    /// pessimistic check for legacy records. New records also pin the
    /// worker's boot and process start time, so a recycled numeric pid does
    /// not keep a dead session alive forever. An absent or unreadable
    /// identity sidecar deliberately falls back to the legacy pid check:
    /// uncertainty must not let prune/tag replacement delete a live worker.
    pub fn worker_alive(&self) -> bool {
        let Some(pid) = self.worker_pid else {
            return false;
        };
        if !process_alive(pid) {
            return false;
        }
        let identity = match read_worker_identity(self) {
            Ok(Some(identity)) if identity.pid == pid => identity,
            Ok(Some(_)) | Ok(None) | Err(_) => return true,
        };
        let Ok(boot_id) = linux_boot_id() else {
            return true;
        };
        if identity.boot_id != boot_id {
            return false;
        }
        match process_start_time_ticks(pid) {
            Ok(start_time) => start_time == identity.start_time_ticks,
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                false
            }
            Err(_) => true,
        }
    }

    /// Whether the worker's lifetime phase is still active according to its
    /// persisted record. A dead worker makes this record stale, not exited.
    pub fn worker_phase_active(&self) -> bool {
        matches!(
            self.phase,
            Phase::Starting | Phase::Running | Phase::Exiting
        )
    }

    /// The normal terminal state for this record: an explicitly recorded
    /// terminal phase and no worker process left to finalize anything.
    pub fn worker_finished(&self) -> bool {
        matches!(self.phase, Phase::Exited | Phase::Failed) && !self.worker_alive()
    }
}

pub fn read_record(path: &Path) -> Result<SessionRecord> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let record: SessionRecord =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    if record.schema_version != SCHEMA_VERSION {
        bail!("unsupported session schema {}", record.schema_version);
    }
    Ok(record)
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

struct AtomicTempGuard(PathBuf);

impl Drop for AtomicTempGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    ensure_private_dir(parent)?;
    let value = serde_json::to_value(value)?;
    persist_worker_identity_once(path, &value)?;
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .unwrap_or(OsStr::new("record"))
            .to_string_lossy(),
        std::process::id(),
        seq
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("create {}", temp.display()))?;
    let _temp_guard = AtomicTempGuard(temp.clone());
    serde_json::to_writer_pretty(&mut file, &value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temp, path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct ProcessIdentity {
    pid: u32,
    start_time_ticks: u64,
    boot_id: String,
}

const WORKER_IDENTITY_FILE: &str = "worker.identity.json";

fn read_worker_identity(record: &SessionRecord) -> Result<Option<ProcessIdentity>> {
    let parent = record
        .history_path
        .parent()
        .ok_or_else(|| anyhow!("session {} has no state directory", record.id))?;
    let path = parent.join(WORKER_IDENTITY_FILE);
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    let metadata = file.metadata()?;
    use std::os::unix::fs::MetadataExt;
    if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("untrusted worker identity file {}", path.display());
    }
    serde_json::from_reader(file).with_context(|| format!("parse {}", path.display()))
}

/// Capture the worker identity on the first record write that contains a
/// worker pid. `run_worker` writes `worker_pid` and immediately persists the
/// record, so doing this in the shared record writer keeps the identity
/// update coupled to that registration without giving later record writes a
/// chance to replace it after a pid has been recycled.
fn persist_worker_identity_once(path: &Path, value: &Value) -> Result<()> {
    if path.file_name() != Some(OsStr::new("session.json")) {
        return Ok(());
    }
    let Some(pid) = value
        .as_object()
        .and_then(|object| object.get("worker_pid"))
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
    else {
        return Ok(());
    };
    // Only the process registering itself may create this immutable file.
    // A different process rewriting a legacy/stale record must never bless
    // whichever unrelated process may now occupy its old numeric pid.
    if pid != std::process::id() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    let identity_path = parent.join(WORKER_IDENTITY_FILE);
    if identity_path.try_exists()? {
        return Ok(());
    }

    let identity = ProcessIdentity {
        pid,
        start_time_ticks: process_start_time_ticks(pid)
            .with_context(|| format!("inspect worker pid {pid} before recording its identity"))?,
        boot_id: linux_boot_id()?,
    };
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{WORKER_IDENTITY_FILE}.{}.{}.tmp",
        std::process::id(),
        seq
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .with_context(|| format!("create {}", temp.display()))?;
        serde_json::to_writer(&mut file, &identity)?;
        file.write_all(b"\n")?;
        file.sync_all()?;

        // A hard link is an atomic no-replace publication. If another writer
        // won the race, retain its earlier identity rather than refreshing it
        // from what may now be a recycled pid.
        match fs::hard_link(&temp, &identity_path) {
            Ok(()) => File::open(parent)?.sync_all()?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("publish {}", identity_path.display()));
            }
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}

pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    ensure_private_dir(parent)?;
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".history.{}.{}.tmp", std::process::id(), seq));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let _temp_guard = AtomicTempGuard(temp.clone());
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub struct FileLock {
    file: File,
}
impl FileLock {
    pub fn exclusive(path: &Path, nonblocking: bool) -> Result<Self> {
        let parent = path.parent().ok_or_else(|| anyhow!("lock has no parent"))?;
        ensure_private_dir(parent)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)?;
        let mut op = libc::LOCK_EX;
        if nonblocking {
            op |= libc::LOCK_NB;
        }
        if unsafe { libc::flock(file.as_raw_fd(), op) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("lock {}", path.display()));
        }
        Ok(Self { file })
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn list_records(paths: &Paths) -> Result<Vec<SessionRecord>> {
    let mut out = Vec::new();
    let root = paths.state_root.join("sessions");
    for entry in fs::read_dir(root)? {
        let entry = match entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let path = entry.path().join("session.json");
        if let Ok(record) = read_record(&path) {
            out.push(record);
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
    Ok(out)
}

pub fn process_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Linux process start time (field 22 of `/proc/<pid>/stat`), measured in
/// clock ticks since boot. Combined with the pid, this distinguishes a
/// persisted process from a later process that reused its numeric pid.
pub fn process_start_time_ticks(pid: u32) -> Result<u64> {
    let stat_path = format!("/proc/{pid}/stat");
    let stat = fs::read_to_string(&stat_path).with_context(|| format!("read {stat_path}"))?;
    // The parenthesized comm field may itself contain spaces or `)`, so split
    // after its final close-paren rather than tokenizing the whole line.
    let after_comm = stat
        .rfind(')')
        .and_then(|end| stat.get(end + 1..))
        .ok_or_else(|| anyhow!("malformed {stat_path}"))?;
    after_comm
        .split_whitespace()
        .nth(19) // field 3 is index 0 here; starttime is field 22
        .ok_or_else(|| anyhow!("{stat_path} has no process start time"))?
        .parse()
        .with_context(|| format!("parse process start time from {stat_path}"))
}

fn linux_boot_id() -> Result<String> {
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("read Linux boot identity")?;
    let boot_id = boot_id.trim();
    if boot_id.is_empty() {
        bail!("Linux boot identity is empty");
    }
    Ok(boot_id.to_owned())
}

/// Signal the worker recorded for a session only if it is still the exact
/// Linux process registered at startup. The pidfd pins the verified process
/// across the final check/signal boundary, so an exit and pid reuse cannot
/// redirect the signal to an unrelated process.
///
/// Records created before worker identities were introduced remain readable
/// and otherwise usable, but direct stale-worker signalling fails closed.
pub fn signal_recorded_worker(record: &SessionRecord, signal: i32) -> Result<()> {
    let Some(pid) = record.worker_pid else {
        return Ok(());
    };
    let parent = record
        .history_path
        .parent()
        .ok_or_else(|| anyhow!("session {} has no state directory", record.id))?;
    let identity_path = parent.join(WORKER_IDENTITY_FILE);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&identity_path)
        .with_context(|| {
            format!(
                "session {} has no trustworthy recorded worker identity; refusing to signal pid {}",
                record.id, pid
            )
        })?;
    let metadata = file.metadata()?;
    use std::os::unix::fs::MetadataExt;
    if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!(
            "session {} has an untrusted worker identity file; refusing to signal pid {}",
            record.id,
            pid
        );
    }
    let identity: ProcessIdentity = serde_json::from_reader(file)
        .with_context(|| format!("parse {}", identity_path.display()))?;
    if identity.pid != pid {
        bail!(
            "session {} recorded worker pid {}, but its identity belongs to pid {}; refusing to signal",
            record.id,
            pid,
            identity.pid
        );
    }

    let pidfd = match pidfd_open(pid) {
        Ok(pidfd) => pidfd,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("open pidfd for worker pid {pid}"));
        }
    };
    let current_boot_id = linux_boot_id()?;
    if current_boot_id != identity.boot_id {
        bail!(
            "worker pid {} for session {} was recorded during a different boot; refusing to signal",
            pid,
            record.id
        );
    }
    let current_start_time = match process_start_time_ticks(pid) {
        Ok(start_time) => start_time,
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if current_start_time != identity.start_time_ticks {
        bail!(
            "worker pid {} for session {} has been reused (recorded start {}, current start {}); refusing to signal",
            pid,
            record.id,
            identity.start_time_ticks,
            current_start_time
        );
    }

    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).with_context(|| format!("signal worker pid {pid} through pidfd"));
        }
    }
    Ok(())
}

fn pidfd_open(pid: u32) -> io::Result<File> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

pub fn resolve_record(
    paths: &Paths,
    selector: Option<&str>,
    workspace: Option<&Path>,
    tag: Option<&str>,
) -> Result<SessionRecord> {
    let records = list_records(paths)?;
    let mut matches = Vec::new();
    if let Some(raw) = selector {
        let needle = raw.to_ascii_lowercase();
        for record in records {
            let id = record.id.to_string();
            if id == needle
                || (needle.len() >= 8 && id.starts_with(&needle))
                || record.selector() == raw
            {
                matches.push(record);
            }
        }
        if matches.is_empty() {
            if let Some((workspace_text, tag_text)) = raw.rsplit_once(':') {
                if let Ok(ws) = canonical_workspace(Path::new(workspace_text)) {
                    for record in list_records(paths)? {
                        if record.workspace == ws && record.tag == tag_text {
                            matches.push(record);
                        }
                    }
                }
            }
        }
    } else {
        let ws = canonical_workspace(workspace.unwrap_or(Path::new(".")))?;
        let tag = tag.unwrap_or("default");
        for record in records {
            if record.workspace == ws && record.tag == tag {
                matches.push(record);
            }
        }
    }
    match matches.len() {
        0 => bail!("no matching session"),
        1 => Ok(matches.remove(0)),
        _ => bail!("selector is ambiguous; use a longer UUID"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EngineConfig {
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Additional vars to unset at spawn time, on top of the forced
    /// `PROVIDER_ENV_UNSET_VARS` union computed in `Config::resolve` -- a
    /// user-configured engine in `~/.config/aplexer/config.toml` can only
    /// ADD to that union, never opt out of it (see `Config::resolve` doc
    /// comment for why this is load-bearing, ported from PocketShell's
    /// `tools/pocketshell/src/pocketshell/engines.py::LaunchSpec.env_unset`
    /// / `_ordered_env_unset_union`).
    #[serde(default)]
    pub env_unset: Vec<String>,
    /// Argv appended after `command` when skip-permissions is requested
    /// (ported from PocketShell's `LaunchSpec.skip_permissions_argv` /
    /// `engines.py::builtin_manifests`). Empty means the engine has no such
    /// flag (e.g. `opencode`, `shell`) -- permissions are config-driven or
    /// not applicable.
    #[serde(default)]
    pub skip_permissions_argv: Vec<String>,
}
impl EngineConfig {
    /// The forced provider-key union this engine's launches will actually
    /// get (see `Config::resolve`'s doc comment) -- exposed so `a engines
    /// --json` (pocketshell-integration-plan.md 0.5) can report it without
    /// needing a full `Config::resolve` call (which requires a
    /// workspace/cwd that a plain engine listing has no reason to invent).
    pub fn resolved_env_unset(&self) -> Vec<String> {
        ordered_env_unset_union(&self.env_unset)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileConfig {
    #[serde(default)]
    pub engine: Option<String>,
    /// Override just the engine's executable (argv[0]); engine default
    /// arguments and skip-permissions argv still apply.
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub history_bytes: Option<usize>,
    #[serde(default)]
    pub limits: Limits,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ShortcutConfig {
    pub engine: String,
    #[serde(default)]
    pub profile: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default = "default_config_version")]
    pub version: u32,
    #[serde(default)]
    pub default_engine: Option<String>,
    #[serde(default)]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub engines: BTreeMap<String, EngineConfig>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
    #[serde(default)]
    pub shortcuts: BTreeMap<String, ShortcutConfig>,
}
fn default_config_version() -> u32 {
    1
}

/// A single engine's profile-discovery rule (spec.md 9.2 / 23: "Aplexer
/// should absorb PocketShell's existing profile discovery concepts"), ported
/// from PocketShell's `tools/pocketshell/src/pocketshell/profiles.py`.
struct ProfileDiscoveryRule {
    engine: &'static str,
    env_var: &'static str,
    default_dirname: &'static str,
    markers: &'static [&'static str],
    hints: &'static [&'static str],
}

/// Only claude and codex currently support a profile config dir (matches
/// PocketShell's `PROFILE_ENGINES`; opencode has no profile env var and grok
/// is not yet known to have one either, so neither is listed here).
const PROFILE_DISCOVERY_RULES: &[ProfileDiscoveryRule] = &[
    ProfileDiscoveryRule {
        engine: "claude",
        env_var: "CLAUDE_CONFIG_DIR",
        default_dirname: ".claude",
        markers: &[".claude.json", "settings.json"],
        hints: &["claude", "laude"],
    },
    ProfileDiscoveryRule {
        engine: "codex",
        env_var: "CODEX_HOME",
        default_dirname: ".codex",
        markers: &["config.toml", "auth.json"],
        hints: &["codex", "odex"],
    },
];

fn has_marker(dir: &Path, markers: &[&str]) -> bool {
    if !dir.is_dir() {
        return false;
    }
    markers.iter().any(|m| dir.join(m).is_file())
}

/// Auto-discovers non-default sibling profile dirs for claude/codex.
///
/// Conservative by construction, matching PocketShell's own discovery:
/// top-level `~/.<name>` dirs only, never recursive, a real marker file
/// required, and only directory-existence/marker-*name* checks -- this
/// never reads inside a config dir (that's where secrets such as
/// `auth.json` live).
///
/// Only the non-default sibling-dir case produces a `ProfileConfig`. An
/// engine's own default dir (e.g. `~/.claude`) deliberately gets no profile
/// entry: the engine's built-in command already resolves to that dir with
/// no `CLAUDE_CONFIG_DIR`/`CODEX_HOME` override needed, so a profile entry
/// for it would be a redundant no-op.
///
/// The returned map is keyed by the discovered directory's own stem minus
/// its leading dot (e.g. `~/.zlaude` -> `"zlaude"`), never by a humanized
/// display name -- `Config.profiles` is a single flat namespace shared by
/// every engine (unlike PocketShell's per-engine `Profile.name`), so two
/// engines' same-sounding profiles (e.g. both named "zai") would otherwise
/// silently clobber each other. A directory stem is collision-free by
/// construction: two different top-level dirs can never share a name.
fn discover_profiles() -> BTreeMap<String, ProfileConfig> {
    let mut out = BTreeMap::new();
    let home = match home_dir() {
        Ok(h) => h,
        Err(_) => return out,
    };
    let entries = match fs::read_dir(&home) {
        Ok(e) => e,
        Err(_) => return out,
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    names.sort();
    for rule in PROFILE_DISCOVERY_RULES {
        for stem in &names {
            if !stem.starts_with('.') || stem == rule.default_dirname {
                continue;
            }
            let lower = stem.to_ascii_lowercase();
            if !rule.hints.iter().any(|hint| lower.contains(hint)) {
                continue;
            }
            let dir = home.join(stem);
            if !has_marker(&dir, rule.markers) {
                continue;
            }
            let id = stem.trim_start_matches('.').to_string();
            if id.is_empty() {
                continue;
            }
            let mut env = BTreeMap::new();
            env.insert(rule.env_var.to_string(), dir.display().to_string());
            out.insert(
                id,
                ProfileConfig {
                    engine: Some(rule.engine.to_string()),
                    env,
                    ..ProfileConfig::default()
                },
            );
        }
    }
    out
}

/// Provider API-key-style env vars unset for every agent-engine launch, so
/// the agent falls back to its subscription auth instead of a per-token env
/// key. Ported verbatim (same order) from PocketShell's
/// `tools/pocketshell/src/pocketshell/engines.py::PROVIDER_ENV_UNSET_VARS`
/// (maintainer decision, pocketshell issue #703 -- subscription billing
/// across the board for codex/claude/opencode). `Config::resolve` unions
/// this with each engine's own `EngineConfig.env_unset`; the union is
/// forced (see that function's doc comment) -- a user config can only add
/// to this list, never remove from it.
const PROVIDER_ENV_UNSET_VARS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_REGION",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_ORG_ID",
    "OPENAI_PROJECT_ID",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "GROQ_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_API_KEY",
    "VERTEX_LOCATION",
    "VERTEX_AI_PROJECT",
    "DEEPSEEK_API_KEY",
    "XAI_API_KEY",
    "FIREWORKS_API_KEY",
    "CEREBRAS_API_KEY",
    "OPENROUTER_API_KEY",
    "TOGETHER_API_KEY",
    "TOGETHER_AI_API_KEY",
    "AZURE_API_KEY",
    "AZURE_RESOURCE_NAME",
    "AZURE_COGNITIVE_SERVICES_RESOURCE_NAME",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_ENDPOINT",
    "CLOUDFLARE_API_TOKEN",
    "CLOUDFLARE_ACCOUNT_ID",
    "CLOUDFLARE_GATEWAY_ID",
    "CLOUDFLARE_API_KEY",
    "HUGGING_FACE_API_KEY",
    "HF_TOKEN",
    "HF_API_TOKEN",
    "MOONSHOT_API_KEY",
    "MOONSHOTAI_API_KEY",
    "MINIMAX_API_KEY",
    "NEBIUS_API_KEY",
    "DEEPINFRA_API_KEY",
    "BASETEN_API_KEY",
    "VENICE_API_KEY",
    "SCALEWAY_API_KEY",
    "OVH_API_KEY",
    "CORTECS_API_KEY",
    "IONET_API_KEY",
    "VERCEL_API_KEY",
    "ZENMUX_API_KEY",
    "ZAI_API_KEY",
    "HELICONE_API_KEY",
    "OPENCODE_API_KEY",
    "OPENCODE_ZEN_API_KEY",
    "GITLAB_TOKEN",
    "GITLAB_INSTANCE_URL",
    "GITLAB_AI_GATEWAY_URL",
    "GITLAB_OAUTH_CLIENT_ID",
    "AICORE_SERVICE_KEY",
    "AICORE_DEPLOYMENT_ID",
    "AICORE_RESOURCE_GROUP",
    "OPENAI_COMPATIBLE_API_KEY",
    "LMSTUDIO_API_KEY",
    "OLLAMA_API_KEY",
    "302AI_API_KEY",
    "FIRMWARE_API_KEY",
    "2AI_API_KEY",
    "GEMINI_API_KEY",
];

/// Union `PROVIDER_ENV_UNSET_VARS` with an engine's own `env_unset`
/// additions, preserving first-seen order and dropping blanks -- ports
/// `engines.py::_ordered_env_unset_union`. The forced list always comes
/// first and is always present in the result; callers cannot construct a
/// smaller list by only passing `extra`.
fn ordered_env_unset_union(extra: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for name in PROVIDER_ENV_UNSET_VARS
        .iter()
        .map(|s| s.to_string())
        .chain(extra.iter().cloned())
    {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    out
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut config = Config {
            version: 1,
            default_engine: Some("shell".into()),
            ..Config::default()
        };
        config.engines.insert(
            "shell".into(),
            EngineConfig {
                command: vec![shell, "-l".into()],
                env: BTreeMap::new(),
                env_unset: Vec::new(),
                skip_permissions_argv: Vec::new(),
            },
        );
        config.engines.insert(
            "codex".into(),
            EngineConfig {
                command: vec![
                    "codex".into(),
                    "-c".into(),
                    "check_for_update_on_startup=false".into(),
                ],
                env: BTreeMap::new(),
                env_unset: Vec::new(),
                // ported from pocketshell engines.py's codex LaunchSpec
                skip_permissions_argv: vec!["--dangerously-bypass-approvals-and-sandbox".into()],
            },
        );
        config.engines.insert(
            "claude".into(),
            EngineConfig {
                command: vec!["claude".into()],
                env: BTreeMap::new(),
                env_unset: Vec::new(),
                // ported from pocketshell engines.py's claude LaunchSpec
                skip_permissions_argv: vec!["--dangerously-skip-permissions".into()],
            },
        );
        config.engines.insert(
            "gemini".into(),
            EngineConfig {
                command: vec!["gemini".into()],
                env: BTreeMap::new(),
                env_unset: Vec::new(),
                // no pocketshell source for a gemini skip-permissions flag
                // (gemini is an aplexer-only extra, not in pocketshell's
                // built-in manifest) -- left empty.
                skip_permissions_argv: Vec::new(),
            },
        );
        config.engines.insert(
            "grok".into(),
            EngineConfig {
                command: vec!["grok".into()],
                env: BTreeMap::new(),
                env_unset: Vec::new(),
                // ported from pocketshell engines.py's grok LaunchSpec
                skip_permissions_argv: vec!["--always-approve".into()],
            },
        );
        // PocketShell built-in (tools/pocketshell/src/pocketshell/engines.py
        // ::builtin_manifests) that aplexer's engine set was missing --
        // required for aplexer to become authoritative for pocketshell's
        // engine registry (pocketshell-integration-plan.md 0.1).
        config.engines.insert(
            "opencode".into(),
            EngineConfig {
                command: vec!["opencode".into()],
                env: BTreeMap::new(),
                env_unset: Vec::new(),
                // opencode has no skip-permissions flag in pocketshell's
                // manifest -- permissions are config-driven (opencode.json).
                skip_permissions_argv: Vec::new(),
            },
        );
        // Auto-discovered profiles (spec.md 9.2/23) go in as defaults before
        // the user's file is merged, exactly like the built-in engines above
        // -- an explicit `[profiles.<id>]` entry in the user's config still
        // wins on a key collision via the `extend()` below.
        config.profiles.extend(discover_profiles());
        // Built-in quick-launch shortcuts (`a - <id>`, see cmd_quick_launch
        // in src/bin/a.rs): short mnemonics onto an (engine, profile) pair.
        // Same defaults-then-user-file-extends layering as engines/profiles
        // above, so `[shortcuts.<id>]` in the user's config can add new ones
        // or override these. "cl"/"co"/"g" are the plain engines; "clz"/
        // "coz"/"cog" additionally select the Z.AI/Go sibling profiles
        // discovered above (ids match those profiles' own dir-stem ids).
        config.shortcuts.insert(
            "cl".into(),
            ShortcutConfig {
                engine: "claude".into(),
                profile: None,
            },
        );
        config.shortcuts.insert(
            "co".into(),
            ShortcutConfig {
                engine: "codex".into(),
                profile: None,
            },
        );
        config.shortcuts.insert(
            "g".into(),
            ShortcutConfig {
                engine: "grok".into(),
                profile: None,
            },
        );
        config.shortcuts.insert(
            "clz".into(),
            ShortcutConfig {
                engine: "claude".into(),
                profile: Some("zlaude".into()),
            },
        );
        config.shortcuts.insert(
            "coz".into(),
            ShortcutConfig {
                engine: "codex".into(),
                profile: Some("zodex".into()),
            },
        );
        config.shortcuts.insert(
            "cog".into(),
            ShortcutConfig {
                engine: "codex".into(),
                profile: Some("godex".into()),
            },
        );
        if paths.config_file.exists() {
            let text = fs::read_to_string(&paths.config_file)?;
            let user: Config = toml::from_str(&text)
                .with_context(|| format!("parse {}", paths.config_file.display()))?;
            if user.version != 1 {
                bail!("unsupported config version {}", user.version);
            }
            if user.default_engine.is_some() {
                config.default_engine = user.default_engine;
            }
            if user.default_profile.is_some() {
                config.default_profile = user.default_profile;
            }
            config.engines.extend(user.engines);
            config.profiles.extend(user.profiles);
            config.shortcuts.extend(user.shortcuts);
        }
        Ok(config)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        &self,
        direct: Vec<String>,
        engine_name: Option<&str>,
        profile_name: Option<&str>,
        workspace: &Path,
        cwd: Option<&Path>,
        env_overrides: &BTreeMap<String, String>,
        limits: &Limits,
        history_bytes: Option<usize>,
    ) -> Result<ResolvedLaunch> {
        let selected_profile = profile_name
            .map(str::to_owned)
            .or_else(|| self.default_profile.clone());
        let profile = selected_profile
            .as_ref()
            .and_then(|name| self.profiles.get(name));
        if selected_profile.is_some() && profile.is_none() {
            bail!("unknown profile {}", selected_profile.as_deref().unwrap());
        }
        let selected_engine = engine_name
            .map(str::to_owned)
            .or_else(|| profile.and_then(|p| p.engine.clone()))
            .or_else(|| self.default_engine.clone())
            .unwrap_or_else(|| "shell".into());
        let engine = self
            .engines
            .get(&selected_engine)
            .ok_or_else(|| anyhow!("unknown engine {selected_engine}"))?;
        let direct_supplied = !direct.is_empty();
        let mut command = if direct_supplied {
            direct
        } else if let Some(cmd) = profile.and_then(|p| p.command.clone()) {
            cmd
        } else {
            let mut argv = engine.command.clone();
            if let Some(exec) = profile.and_then(|p| p.executable.clone()) {
                if argv.is_empty() {
                    argv.push(exec);
                } else {
                    argv[0] = exec;
                }
            }
            argv
        };
        if command.is_empty() {
            bail!("engine {selected_engine} has no command");
        }
        if !direct_supplied {
            if let Some(p) = profile {
                if p.command.is_none() {
                    command.extend(p.args.clone());
                }
            }
        }
        let mut merged_env = engine.env.clone();
        if let Some(p) = profile {
            merged_env.extend(p.env.clone());
        }
        merged_env.extend(env_overrides.clone());
        let mut merged_limits = profile.map(|p| p.limits.clone()).unwrap_or_default();
        if limits.memory_bytes.is_some() {
            merged_limits.memory_bytes = limits.memory_bytes;
        }
        if limits.pids.is_some() {
            merged_limits.pids = limits.pids;
        }
        if limits.cpu_quota_us.is_some() {
            merged_limits.cpu_quota_us = limits.cpu_quota_us;
        }
        if limits.cpu_period_us.is_some() {
            merged_limits.cpu_period_us = limits.cpu_period_us;
        }
        let launch_cwd = cwd
            .map(Path::to_path_buf)
            .or_else(|| profile.and_then(|p| p.cwd.clone()))
            .unwrap_or_else(|| workspace.to_path_buf());
        // Forced provider-key union (pocketshell-integration-plan.md 1.4/0.2):
        // `PROVIDER_ENV_UNSET_VARS` always comes first and is always
        // present, regardless of what `engine.env_unset` (sourced from
        // `~/.config/aplexer/config.toml`, possibly a fully user-defined
        // custom engine) contains -- a custom engine can only ADD names to
        // this list, never remove or replace it. Callers (`a start`, and
        // later `a launch-spec`/`a launch-exec`) are expected to actually
        // apply this list at spawn time (see worker.rs's spawn_workload),
        // not just report it.
        let env_unset = ordered_env_unset_union(&engine.env_unset);
        Ok(ResolvedLaunch {
            engine: selected_engine,
            profile: selected_profile,
            command,
            cwd: launch_cwd,
            env: merged_env,
            env_unset,
            skip_permissions_argv: engine.skip_permissions_argv.clone(),
            limits: merged_limits,
            history_bytes: history_bytes
                .or_else(|| profile.and_then(|p| p.history_bytes))
                .unwrap_or(DEFAULT_HISTORY_BYTES),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedLaunch {
    pub engine: String,
    pub profile: Option<String>,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    /// Forced union of `PROVIDER_ENV_UNSET_VARS` with the engine's own
    /// `env_unset` -- vars that must be absent from the spawned workload's
    /// environment, applied AFTER `env` at spawn time (an unset always wins
    /// over a set, matching pocketshell's `agents.py::build_env` ordering:
    /// the provider-key strip runs last so it beats even a profile that
    /// tries to inject one).
    pub env_unset: Vec<String>,
    /// Argv to append to `command` when skip-permissions is requested (see
    /// `EngineConfig::skip_permissions_argv`). `a start` appends this by
    /// default (unless `--no-skip-permissions` or an explicit `-- argv`);
    /// `a launch-spec`/`a launch-exec` do the same.
    pub skip_permissions_argv: Vec<String>,
    pub limits: Limits,
    pub history_bytes: usize,
}

pub fn executable_available(program: &str) -> bool {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return candidate.is_file();
    }
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Json = 1,
    Data = 2,
    End = 3,
}
#[derive(Debug)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

pub fn write_frame<W: Write>(writer: &mut W, kind: FrameKind, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        bail!("frame too large: {}", payload.len());
    }
    let mut header = [0u8; 12];
    header[..4].copy_from_slice(b"APX1");
    header[4] = kind as u8;
    header[8..12].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Frame>> {
    let mut header = [0u8; 12];
    match reader.read(&mut header[..1]) {
        Ok(0) => return Ok(None),
        Ok(1) => {}
        Ok(_) => unreachable!(),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return read_frame(reader),
        Err(e) => return Err(e.into()),
    }
    reader.read_exact(&mut header[1..])?;
    if &header[..4] != b"APX1" {
        bail!("invalid protocol magic");
    }
    if header[5..8] != [0, 0, 0] {
        bail!("unsupported frame flags");
    }
    let kind = match header[4] {
        1 => FrameKind::Json,
        2 => FrameKind::Data,
        3 => FrameKind::End,
        n => bail!("unknown frame type {n}"),
    };
    let length = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
    if length > MAX_FRAME_BYTES {
        bail!("frame exceeds maximum");
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    Ok(Some(Frame { kind, payload }))
}

pub fn write_json<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    write_frame(writer, FrameKind::Json, &serde_json::to_vec(value)?)
}
pub fn frame_json<T: for<'de> Deserialize<'de>>(frame: Frame) -> Result<T> {
    if frame.kind != FrameKind::Json {
        bail!("expected JSON frame");
    }
    Ok(serde_json::from_slice(&frame.payload)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    pub request_id: String,
    #[serde(flatten)]
    pub operation: Operation,
}
impl Request {
    pub fn new(operation: Operation) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4().to_string(),
            operation,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Ping,
    Status,
    Send {
        bytes: usize,
    },
    Capture {
        max_bytes: Option<usize>,
    },
    /// `history_bytes` keeps its original meaning (raw-tail replay size,
    /// used when `want_screen` is false or an old worker doesn't understand
    /// it). `want_screen`/`rows`/`cols` are additive fields (design doc
    /// section 6.1): an old worker's serde simply ignores unknown fields
    /// and falls back to today's raw-tail replay -- no worse than before --
    /// and an old client never sends them, so `want_screen` defaulting to
    /// `false` reproduces today's behavior exactly. `rows`/`cols`, when
    /// given, are the client's real terminal geometry (already
    /// reserved-rows-adjusted by the caller) so the worker can resize the
    /// PTY and the screen model *before* rendering the snapshot -- no
    /// wrong-size frame followed by a SIGWINCH repaint.
    Attach {
        history_bytes: Option<usize>,
        #[serde(default)]
        want_screen: bool,
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        cols: Option<u16>,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Kill {
        signal: i32,
        grace_ms: u64,
    },
    Rename {
        workspace: PathBuf,
        tag: String,
    },
    /// `a capture --screen` (design doc section 8): the rendered current
    /// screen (`plain: false`, same bytes `Attach`'s snapshot would carry)
    /// or its plain-text contents (`plain: true`, `ScreenTracker::contents`)
    /// -- "richer PocketShell previews" from spec.md section 17.
    CaptureScreen {
        plain: bool,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub version: u16,
    pub request_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
impl Response {
    pub fn ok(id: impl Into<String>, value: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: id.into(),
            ok: true,
            result: Some(value),
            error: None,
        }
    }
    pub fn error(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: id.into(),
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
    pub fn into_result(self) -> Result<Value> {
        if self.ok {
            Ok(self.result.unwrap_or(Value::Null))
        } else {
            bail!("{}", self.error.unwrap_or_else(|| "request failed".into()))
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    Exit {
        exit: ExitInfo,
    },
    Error {
        message: String,
    },
    /// The workload did something that invalidates the client's DECSTBM
    /// status-bar reservation: reset its scroll margins (RIS or a bare/
    /// full-range `\x1b[r`), flipped alternate-screen state, or issued an
    /// Erase in Display (`CSI ... J`, which ignores scroll margins per spec
    /// and so can wipe the reserved row even under an active sub-range --
    /// design doc section 7). Sent **only** to subscribers that attached
    /// with `want_screen: true` -- an old client's `serde_json::from_slice`
    /// would hard-fail on an unrecognized `event` tag, so gating this on
    /// the request flag (done at the worker's send site, not here) keeps
    /// old clients safe (design doc section 6.3).
    Layout {
        alt_screen: bool,
        margins_reset: bool,
        // `default` so a new client attaching to an OLD, already-running
        // worker (started before this field existed) doesn't hard-fail
        // deserializing that worker's `Layout` events -- workers are
        // long-lived and outlive a client rebuild, unlike the `event` tag
        // gating described above which only covers new tags, not new fields
        // on an existing one.
        #[serde(default)]
        erase_reset: bool,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AttachControl {
    Resize { rows: u16, cols: u16 },
    Signal { signal: i32 },
    Detach,
}

/// How stale the persisted history file may get behind the in-memory ring.
/// Live reads (capture/attach) are always served from memory; the file only
/// matters after the worker is gone, so a worker crash loses at most this
/// much of the tail.
pub const HISTORY_FLUSH_INTERVAL: Duration = Duration::from_millis(500);

/// A raw byte log -- no line/wrap-flag structure, and it must stay that way.
/// If a future feature wants to render captured history at a specific width,
/// implement it by replaying these bytes into a *fresh* `vt100::Parser`
/// constructed at that width (re-parse from scratch), never by calling
/// resize/`set_size` on a parser that already processed content at a
/// different width -- re-parsing is deterministic, in-place reflow of a
/// populated grid is exactly the class of bug that garbles tmux scrollback.
pub struct History {
    path: PathBuf,
    cap: usize,
    bytes: VecDeque<u8>,
    dirty: bool,
    last_flush: Instant,
}
impl History {
    pub fn open(path: PathBuf, cap: usize) -> Result<Self> {
        let existing = fs::read(&path).unwrap_or_default();
        let start = existing.len().saturating_sub(cap);
        let bytes = existing[start..].iter().copied().collect();
        Ok(Self {
            path,
            cap,
            bytes,
            dirty: false,
            last_flush: Instant::now(),
        })
    }
    /// Appends to the in-memory ring and persists at most once per
    /// HISTORY_FLUSH_INTERVAL. Persisting on every append rewrote the whole
    /// file (up to `cap` bytes) with two fsyncs for every PTY read of at
    /// most 32KB -- measured >100x write amplification whose backpressure
    /// throttled the workload itself to ~150KB/s of terminal output (11x
    /// slower than with persistence disabled). Callers must arrange a final
    /// flush() when output ends.
    pub fn append(&mut self, data: &[u8]) -> Result<()> {
        if self.cap == 0 {
            return Ok(());
        }
        if data.len() >= self.cap {
            self.bytes.clear();
            self.bytes
                .extend(data[data.len() - self.cap..].iter().copied());
        } else {
            self.bytes.extend(data.iter().copied());
            while self.bytes.len() > self.cap {
                self.bytes.pop_front();
            }
        }
        self.dirty = true;
        if self.last_flush.elapsed() >= HISTORY_FLUSH_INTERVAL {
            self.flush()?;
        }
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let contiguous: Vec<u8> = self.bytes.iter().copied().collect();
        atomic_write_bytes(&self.path, &contiguous)?;
        self.dirty = false;
        self.last_flush = Instant::now();
        Ok(())
    }
    pub fn snapshot(&self, max: Option<usize>) -> Vec<u8> {
        let count = max.unwrap_or(self.bytes.len()).min(self.bytes.len());
        self.bytes
            .iter()
            .skip(self.bytes.len() - count)
            .copied()
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct Cgroup {
    path: PathBuf,
    anchor_pid: u32,
    /// Shared across clones: the anchor pid must be killed at most once.
    /// After the first kill the pid is reaped (a background thread waits on
    /// it) and may be recycled by the kernel for an unrelated process of
    /// this user, so a second kill(anchor_pid, SIGKILL) -- e.g. from
    /// Cgroup::cleanup at session end, hours after spawn_workload already
    /// released the anchor -- could kill an innocent process.
    anchor_released: std::sync::Arc<AtomicBool>,
    initial_oom_kill: u64,
}
impl Cgroup {
    // A worker's own ambient cgroup (inherited from whatever spawned `a start`,
    // e.g. a tmux pane or SSH session) is never a safe place to nest a
    // resource-limited child: cgroup v2 refuses to enable controllers in
    // cgroup.subtree_control while the parent still has processes attached
    // directly ("no internal process" constraint) -- and the worker, plus
    // everything else in that ambient session, is exactly such a process.
    // Writing to memory.max there fails closed with EACCES rather than
    // applying a limit; forcing it through would risk taking down unrelated
    // sessions sharing that ambient cgroup, which is the one failure mode
    // this project exists to prevent.
    //
    // Instead we ask systemd-run to create a fresh, independently delegated
    // scope directly under the user's own slice (a sibling, not a nested
    // child, of the ambient cgroup) and hold it open with a placeholder
    // process until the real workload can be moved in.
    pub fn create(id: Uuid, limits: &Limits) -> Result<Option<Self>> {
        if !limits.requested() {
            return Ok(None);
        }
        let controllers = Path::new("/sys/fs/cgroup/cgroup.controllers");
        if !controllers.exists() {
            bail!("resource limits require cgroup v2");
        }
        let unit = format!("aplexer-workload-{id}");
        let mut command = Command::new("systemd-run");
        command
            .arg("--user")
            .arg("--scope")
            .arg("--collect")
            .arg(format!("--unit={unit}"))
            .arg("-p")
            .arg("Delegate=yes");
        if let Some(value) = limits.memory_bytes {
            command.arg("-p").arg(format!("MemoryMax={value}"));
            // Without a swap cap, hitting MemoryMax doesn't OOM-kill the
            // workload -- it swaps unboundedly instead, which both defeats
            // the purpose of a memory limit and risks host-wide I/O
            // pressure that *would* leak into unrelated sessions. A
            // memory-limited session gets no swap; a configurable swap
            // allowance is not yet exposed by the CLI.
            command.arg("-p").arg("MemorySwapMax=0");
        }
        if let Some(value) = limits.pids {
            command.arg("-p").arg(format!("TasksMax={value}"));
        }
        if let Some(quota) = limits.cpu_quota_us {
            let period = limits.cpu_period_us.unwrap_or(100_000);
            let percent = ((quota as f64 / period as f64) * 100.0).ceil().max(1.0) as u64;
            command.arg("-p").arg(format!("CPUQuota={percent}%"));
        }
        command
            .arg("--")
            .arg("sleep")
            .arg("infinity")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut anchor = command
            .spawn()
            .context("spawn systemd-run anchor; limits fail closed")?;
        let anchor_pid = anchor.id();
        thread::spawn(move || {
            let _ = anchor.wait();
        });
        let release_on_failure = |anchor_pid: u32| unsafe {
            libc::kill(anchor_pid as libc::pid_t, libc::SIGKILL);
        };
        let path = match wait_for_scope_cgroup(&unit, Duration::from_secs(5)) {
            Ok(path) => path,
            Err(error) => {
                release_on_failure(anchor_pid);
                return Err(error.context("limits fail closed"));
            }
        };
        if limits.memory_bytes.is_some() && !path.join("memory.max").exists() {
            release_on_failure(anchor_pid);
            bail!("systemd did not delegate the memory controller; limits fail closed");
        }
        if limits.pids.is_some() && !path.join("pids.max").exists() {
            release_on_failure(anchor_pid);
            bail!("systemd did not delegate the pids controller; limits fail closed");
        }
        let initial_oom_kill = read_counter(&path.join("memory.events"), "oom_kill").unwrap_or(0);
        Ok(Some(Self {
            path,
            anchor_pid,
            anchor_released: std::sync::Arc::new(AtomicBool::new(false)),
            initial_oom_kill,
        }))
    }
    /// Opens `cgroup.procs` for writing so the not-yet-exec'd workload child
    /// can move itself into the cgroup from inside a `pre_exec` closure
    /// (any process may write its own pid into a cgroup it has access to;
    /// this needs no cooperation from the parent after fork).
    ///
    /// We deliberately do not have the parent write the child's pid into
    /// `cgroup.procs` after `Command::spawn()` returns: `spawn()` itself
    /// blocks in the parent until the child either execs or reports a
    /// pre_exec failure, so any post-spawn, pre-exec rendezvous between
    /// parent and child (e.g. a gate the child waits on) deadlocks --
    /// the parent can never reach the code that would release it.
    pub fn open_procs(&self) -> Result<File> {
        OpenOptions::new()
            .write(true)
            .open(self.path.join("cgroup.procs"))
            .with_context(|| format!("open {}/cgroup.procs", self.path.display()))
    }
    pub fn locator(&self) -> &Path {
        &self.path
    }
    /// Kills the placeholder process that was keeping the delegated scope
    /// alive. Call this only after the real workload pid has been added to
    /// the cgroup, so the cgroup never goes empty (and gets garbage
    /// collected by systemd) before the real workload takes residence.
    pub fn release_anchor(&self) {
        if !self.anchor_released.swap(true, Ordering::SeqCst) {
            unsafe {
                libc::kill(self.anchor_pid as i32, libc::SIGKILL);
            }
        }
    }
    pub fn signal_all(&self, signal: i32) -> Result<()> {
        let pids = fs::read_to_string(self.path.join("cgroup.procs"))?;
        for text in pids.lines() {
            if let Ok(pid) = text.parse::<i32>() {
                unsafe {
                    libc::kill(pid, signal);
                }
            }
        }
        Ok(())
    }
    pub fn kill_all(&self) -> Result<()> {
        let kill = self.path.join("cgroup.kill");
        if kill.exists() {
            fs::write(kill, "1")?;
            Ok(())
        } else {
            self.signal_all(libc::SIGKILL)
        }
    }
    pub fn populated(&self) -> Result<bool> {
        Ok(read_counter(&self.path.join("cgroup.events"), "populated")? != 0)
    }
    pub fn oom_killed(&self) -> bool {
        read_counter(&self.path.join("memory.events"), "oom_kill").unwrap_or(0)
            > self.initial_oom_kill
    }
    /// Live telemetry for a still-running cgroup. A workload's own OOM kill
    /// only shows up in the session record's `exit` field once the tracked
    /// PTY-owning process itself exits -- a subprocess it launched can be
    /// OOM-killed by the kernel while the shell survives, which is common
    /// and otherwise invisible. `a status` surfaces this live instead of
    /// only at session exit.
    pub fn stats(&self) -> serde_json::Value {
        let read_value = |name: &str| -> Option<u64> {
            fs::read_to_string(self.path.join(name))
                .ok()
                .and_then(|text| text.trim().parse().ok())
        };
        let oom_kill_total =
            read_counter(&self.path.join("memory.events"), "oom_kill").unwrap_or(0);
        serde_json::json!({
            "memory_current": read_value("memory.current"),
            "memory_peak": read_value("memory.peak"),
            "memory_swap_current": read_value("memory.swap.current"),
            "oom_kill_count": oom_kill_total,
            "oom_kill_count_since_start": oom_kill_total.saturating_sub(self.initial_oom_kill),
            // Status telemetry is explicitly best-effort; lifecycle and kill
            // paths call `populated` directly and propagate every error.
            "populated": self.populated().ok(),
        })
    }
    pub fn cleanup(&self) {
        self.release_anchor();
        let _ = fs::remove_dir(&self.path);
    }
}

/// Validate and recover a resource-limited session through its recorded
/// kernel containment domain. A path that has disappeared after it was
/// durably recorded is empty by construction: cgroup v2 cannot remove a
/// populated cgroup. Every other inspection error fails closed.
pub fn cleanup_recorded_cgroup(
    id: Uuid,
    locator: &Path,
    signal: i32,
    grace: Duration,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(grace)
        .and_then(|deadline| deadline.checked_add(Duration::from_secs(2)))
        .ok_or_else(|| anyhow!("cgroup cleanup deadline overflow"))?;
    cleanup_recorded_cgroup_until(id, locator, signal, grace, deadline)
}

/// Deadline-sharing variant for startup rollback, where cgroup recovery must
/// consume the same wall-clock budget as procfs discovery and pidfd cleanup.
pub fn cleanup_recorded_cgroup_until(
    id: Uuid,
    locator: &Path,
    signal: i32,
    grace: Duration,
    deadline: Instant,
) -> Result<()> {
    check_cgroup_cleanup_deadline(deadline, "validating recorded cgroup")?;
    let Some(path) = validate_recorded_cgroup(id, locator)? else {
        check_cgroup_cleanup_deadline(deadline, "validating recorded cgroup")?;
        return Ok(());
    };
    check_cgroup_cleanup_deadline(deadline, "validating recorded cgroup")?;

    if signal == libc::SIGKILL {
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        kill_cgroup_path_until(&path, deadline)?;
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
    } else {
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup")?;
        signal_cgroup_path_until(&path, signal, deadline)?;
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup")?;
        let grace_deadline = Instant::now()
            .checked_add(grace)
            .ok_or_else(|| anyhow!("cgroup cleanup grace deadline overflow"))?
            .min(deadline);
        while cgroup_path_populated_until(&path, deadline)? && Instant::now() < grace_deadline {
            sleep_until_cgroup_deadline(grace_deadline, "waiting for recorded cgroup grace")?;
        }
        if cgroup_path_populated_until(&path, deadline)? {
            check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
            kill_cgroup_path_until(&path, deadline)?;
            check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        }
    }

    while cgroup_path_populated_until(&path, deadline)? {
        // Older cgroup-v2 mounts may not expose cgroup.kill. Repeat the
        // identity-pinned cgroup.procs fallback so a member that forked
        // between the first read and signal cannot escape cleanup.
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        kill_cgroup_path_until(&path, deadline)?;
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        sleep_until_cgroup_deadline(deadline, "proving recorded cgroup empty")?;
    }
    Ok(())
}

fn check_cgroup_cleanup_deadline(deadline: Instant, operation: &str) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("timed out {operation}");
    }
    Ok(())
}

fn sleep_until_cgroup_deadline(deadline: Instant, operation: &str) -> Result<()> {
    check_cgroup_cleanup_deadline(deadline, operation)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    thread::sleep(Duration::from_millis(25).min(remaining));
    check_cgroup_cleanup_deadline(deadline, operation)
}

fn validate_recorded_cgroup(id: Uuid, locator: &Path) -> Result<Option<PathBuf>> {
    let root = Path::new("/sys/fs/cgroup");
    let expected = format!("aplexer-workload-{id}.scope");
    if !locator.is_absolute()
        || !locator.starts_with(root)
        || locator.file_name() != Some(OsStr::new(&expected))
        || locator
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!(
            "untrusted recorded cgroup locator for session {id}: {}",
            locator.display()
        );
    }
    let canonical = match fs::canonicalize(locator) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("resolve recorded cgroup {}", locator.display()))
        }
    };
    let canonical_root = fs::canonicalize(root).context("resolve cgroup v2 root")?;
    if !canonical.starts_with(&canonical_root)
        || canonical.file_name() != Some(OsStr::new(&expected))
    {
        bail!(
            "recorded cgroup for session {id} escaped the cgroup root: {}",
            canonical.display()
        );
    }
    if !fs::metadata(&canonical)?.is_dir() {
        bail!(
            "recorded cgroup is not a directory: {}",
            canonical.display()
        );
    }
    Ok(Some(canonical))
}

fn cgroup_path_populated(path: &Path) -> Result<bool> {
    match read_counter(&path.join("cgroup.events"), "populated") {
        Ok(value) => Ok(value != 0),
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
            }) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn cgroup_path_populated_until(path: &Path, deadline: Instant) -> Result<bool> {
    check_cgroup_cleanup_deadline(deadline, "inspecting recorded cgroup")?;
    let populated = cgroup_path_populated(path)?;
    check_cgroup_cleanup_deadline(deadline, "inspecting recorded cgroup")?;
    Ok(populated)
}

fn read_cgroup_pids_until(path: &Path, deadline: Instant) -> Result<BTreeSet<i32>> {
    check_cgroup_cleanup_deadline(deadline, "reading recorded cgroup members")?;
    let procs = path.join("cgroup.procs");
    let file = match File::open(&procs) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", procs.display())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CGROUP_PROCS_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", procs.display()))?;
    check_cgroup_cleanup_deadline(deadline, "reading recorded cgroup members")?;
    if bytes.len() as u64 > MAX_CGROUP_PROCS_BYTES {
        bail!("recorded cgroup member list exceeds safe byte limit of {MAX_CGROUP_PROCS_BYTES}");
    }
    let text =
        std::str::from_utf8(&bytes).with_context(|| format!("decode {}", procs.display()))?;
    let mut pids = BTreeSet::new();
    for value in text.lines() {
        check_cgroup_cleanup_deadline(deadline, "parsing recorded cgroup members")?;
        if pids.len() >= MAX_CGROUP_RECOVERY_MEMBERS {
            bail!("recorded cgroup exceeds safe member limit of {MAX_CGROUP_RECOVERY_MEMBERS}");
        }
        let pid = value
            .parse::<i32>()
            .with_context(|| format!("parse pid in {}/cgroup.procs", path.display()))?;
        if pid <= 0 {
            bail!("invalid pid {pid} in {}/cgroup.procs", path.display());
        }
        pids.insert(pid);
    }
    Ok(pids)
}

struct CgroupMemberHandle {
    pid: i32,
    pidfd: File,
}

fn signal_cgroup_path_until(path: &Path, signal: i32, deadline: Instant) -> Result<()> {
    let candidates = read_cgroup_pids_until(path, deadline)?;
    let capacity = cgroup_recovery_pidfd_capacity(deadline)?;
    if candidates.len() > capacity {
        bail!(
            "recorded cgroup has {} members but only {capacity} pidfds can be opened safely",
            candidates.len()
        );
    }
    let mut members = Vec::with_capacity(candidates.len());
    for pid in candidates {
        check_cgroup_cleanup_deadline(deadline, "pinning recorded cgroup members")?;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(error).with_context(|| format!("open pidfd for cgroup member {pid}"));
        }
        members.push(CgroupMemberHandle {
            pid,
            pidfd: unsafe { File::from_raw_fd(fd) },
        });
    }

    // A pidfd pins process identity; this second membership snapshot ensures
    // each pinned identity still belongs to the recorded domain before it is
    // signalled. New forks are handled by the repeated populated/kill loop.
    let current = read_cgroup_pids_until(path, deadline)?;
    for member in members {
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup members")?;
        if !current.contains(&member.pid) {
            continue;
        }
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                member.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).with_context(|| format!("signal cgroup member {}", member.pid));
            }
        }
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup members")?;
    }
    Ok(())
}

fn cgroup_recovery_pidfd_capacity(deadline: Instant) -> Result<usize> {
    check_cgroup_cleanup_deadline(deadline, "preflighting cgroup recovery descriptors")?;
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error()).context("read RLIMIT_NOFILE for cgroup recovery");
    }
    check_cgroup_cleanup_deadline(deadline, "preflighting cgroup recovery descriptors")?;
    let descriptors = fs::read_dir("/proc/self/fd").context("count open recovery descriptors")?;
    let mut open = 0_u64;
    for descriptor in descriptors {
        check_cgroup_cleanup_deadline(deadline, "counting open recovery descriptors")?;
        descriptor.context("enumerate open recovery descriptors")?;
        open = open
            .checked_add(1)
            .ok_or_else(|| anyhow!("open recovery descriptor count overflow"))?;
    }
    let soft_limit = if limit.rlim_cur == libc::RLIM_INFINITY {
        u64::MAX
    } else {
        limit.rlim_cur
    };
    Ok(cgroup_recovery_pidfd_capacity_from_counts(soft_limit, open))
}

fn cgroup_recovery_pidfd_capacity_from_counts(soft_limit: u64, open: u64) -> usize {
    let available = soft_limit
        .saturating_sub(open)
        .saturating_sub(CGROUP_RECOVERY_FD_RESERVE);
    usize::try_from(available)
        .unwrap_or(usize::MAX)
        .min(MAX_CGROUP_RECOVERY_MEMBERS)
}

fn kill_cgroup_path_until(path: &Path, deadline: Instant) -> Result<()> {
    check_cgroup_cleanup_deadline(deadline, "checking recorded cgroup kill support")?;
    let kill = path.join("cgroup.kill");
    if kill.exists() {
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        let result = match fs::write(&kill, "1") {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("write {}", kill.display())),
        };
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        result
    } else {
        signal_cgroup_path_until(path, libc::SIGKILL, deadline)
    }
}
fn wait_for_scope_cgroup(unit: &str, timeout: Duration) -> Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        let output = Command::new("systemctl")
            .args([
                "--user",
                "show",
                &format!("{unit}.scope"),
                "-p",
                "ControlGroup",
                "--value",
            ])
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                let relative = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !relative.is_empty() && relative != "/" {
                    let path = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
                    if path.join("cgroup.procs").exists() {
                        return Ok(path);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for systemd scope {unit}.scope to appear");
        }
        thread::sleep(Duration::from_millis(20));
    }
}
fn read_counter(path: &Path, key: &str) -> Result<u64> {
    let text = fs::read_to_string(path)?;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some(key) {
            let value = parts
                .next()
                .ok_or_else(|| anyhow!("counter {key} in {} has no value", path.display()))?;
            return value
                .parse()
                .with_context(|| format!("parse counter {key} in {}", path.display()));
        }
    }
    bail!("counter {key} not found in {}", path.display())
}

pub fn parse_byte_size(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty byte size");
    }
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let value: u64 = raw[..split].parse()?;
    let suffix = raw[split..].trim().to_ascii_lowercase();
    let multiplier = match suffix.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024_u64.pow(2),
        "g" | "gb" | "gib" => 1024_u64.pow(3),
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        _ => bail!("unknown byte-size suffix {suffix}"),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("byte size overflow"))
}

pub fn parse_env(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for value in values {
        let (key, val) = value
            .split_once('=')
            .ok_or_else(|| anyhow!("environment override must be KEY=VALUE"))?;
        if key.is_empty() || key.as_bytes().contains(&0) || val.as_bytes().contains(&0) {
            bail!("invalid environment override");
        }
        out.insert(key.to_owned(), val.to_owned());
    }
    Ok(out)
}

pub fn os_to_utf8(value: &OsStr, what: &str) -> Result<String> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{what} must be valid UTF-8"))
}

pub fn command_exists(command: &[String]) -> bool {
    command
        .first()
        .map(|p| executable_available(p))
        .unwrap_or(false)
}

pub fn worker_executable() -> Result<PathBuf> {
    if let Some(path) = env::var_os("APLEXER_WORKER") {
        return Ok(PathBuf::from(path));
    }
    let current = env::current_exe()?;
    if let Some(parent) = current.parent() {
        let sibling = parent.join("aplexer");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    Ok(PathBuf::from("aplexer"))
}

pub fn set_cloexec(fd: RawFd, enabled: bool) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let next = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, next) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn open_pty(rows: u16, cols: u16) -> Result<(File, File)> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(io::Error::last_os_error()).context("posix_openpt");
    }
    let cleanup_master = || unsafe {
        libc::close(master);
    };
    if unsafe { libc::grantpt(master) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("grantpt");
    }
    if unsafe { libc::unlockpt(master) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("unlockpt");
    }
    // `libc::c_char` is unsigned on some Linux architectures (including
    // aarch64), so keep the buffer's element type aligned with libc rather
    // than assuming x86_64's signed `char`.
    let mut name = vec![0 as libc::c_char; 256];
    if unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("ptsname_r");
    }
    let slave = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave < 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("open PTY slave");
    }
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
    }
    Ok(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
}

pub fn set_winsize(fd: RawFd, rows: u16, cols: u16) -> Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(io::Error::last_os_error()).context("TIOCSWINSZ");
    }
    Ok(())
}

/// The name (from `/proc/<pgid>/comm`) of whatever is currently in the
/// foreground of the pty referred to by `fd` -- the same mechanism tmux
/// uses for `pane_current_command`: `tcgetpgrp(fd)` to get the foreground
/// process group of the pty (this updates automatically as the shell
/// forks/foregrounds jobs, standard POSIX job control -- no polling of the
/// workload itself needed), then read that pgid's name straight out of
/// procfs. `comm` is used over parsing `/proc/<pid>/stat`'s second field
/// because it's already a single line stripped of parens and args.
///
/// `fd` need not be `fd`'s own controlling terminal -- this is exactly how
/// tmux's server (which is not part of the pane's session) queries a pty
/// it merely holds the master side of. Best-effort throughout: any failure
/// (no foreground group yet, the process exited between the two syscalls,
/// procfs unmounted) yields `None` rather than an error, since this is a
/// cosmetic status-bar signal, never something worth failing a request or
/// blocking a hot loop over.
pub fn foreground_command(fd: RawFd) -> Option<String> {
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    if pgid <= 0 {
        return None;
    }
    let comm = fs::read_to_string(format!("/proc/{pgid}/comm")).ok()?;
    let trimmed = comm.trim_end_matches('\n');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn peer_uid(fd: RawFd) -> Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut _,
            &mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("SO_PEERCRED");
    }
    Ok(cred.uid)
}

pub fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

pub fn c_string(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("path contains NUL")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn ensure_private_dir_rejects_leaf_symlink_without_chmodding_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.path().join("link");
        symlink(&target, &link).unwrap();

        let error = ensure_private_dir(&link).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("without following symbolic links"),
            "{error:#}"
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn ensure_private_dir_rejects_symlink_ancestor_without_creating_beneath_it() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = root.path().join("link");
        symlink(&target, &link).unwrap();

        assert!(ensure_private_dir(&link.join("child")).is_err());
        assert!(!target.join("child").exists());
    }

    #[test]
    fn ensure_private_dir_validates_type_before_chmod() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("ordinary-file");
        fs::write(&file, b"not a directory").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(ensure_private_dir(&file).is_err());
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn ensure_private_dir_chmods_verified_directory() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("private");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_dir(&directory).unwrap();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn atomic_write_json_removes_temp_after_rename_failure() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("record.json");
        fs::create_dir(&destination).unwrap();

        assert!(atomic_write_json(&destination, &serde_json::json!({"secret": "value"})).is_err());
        assert_no_atomic_temps(root.path());
    }

    #[test]
    fn atomic_write_bytes_removes_temp_after_rename_failure() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("history.bin");
        fs::create_dir(&destination).unwrap();

        assert!(atomic_write_bytes(&destination, b"secret bytes").is_err());
        assert_no_atomic_temps(root.path());
    }

    fn assert_no_atomic_temps(directory: &Path) {
        let leftovers = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn session_record_write_persists_worker_start_identity_once() {
        let root = tempfile::tempdir().unwrap();
        let record_path = root.path().join("session.json");
        let pid = std::process::id();
        atomic_write_json(
            &record_path,
            &serde_json::json!({"worker_pid": pid, "value": 1}),
        )
        .unwrap();
        let identity_path = root.path().join(WORKER_IDENTITY_FILE);
        let original: ProcessIdentity =
            serde_json::from_slice(&fs::read(&identity_path).unwrap()).unwrap();
        assert_eq!(original.pid, pid);
        assert_eq!(original.boot_id, linux_boot_id().unwrap());
        assert_eq!(
            original.start_time_ticks,
            process_start_time_ticks(pid).unwrap()
        );

        // A later write must not refresh the immutable registration.
        atomic_write_json(
            &record_path,
            &serde_json::json!({"worker_pid": pid, "value": 2}),
        )
        .unwrap();
        let after: ProcessIdentity =
            serde_json::from_slice(&fs::read(&identity_path).unwrap()).unwrap();
        assert_eq!(after.pid, original.pid);
        assert_eq!(after.start_time_ticks, original.start_time_ticks);
    }

    fn liveness_record(state_dir: &Path) -> SessionRecord {
        let pid = std::process::id();
        SessionRecord {
            schema_version: SCHEMA_VERSION,
            id: Uuid::new_v4(),
            workspace: state_dir.to_path_buf(),
            tag: "identity-test".into(),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/true".into()],
            cwd: state_dir.to_path_buf(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: DEFAULT_HISTORY_BYTES,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_activity_ms: None,
            phase: Phase::Running,
            worker_pid: Some(pid),
            workload_pid: None,
            containment_cgroup: None,
            containment_empty: false,
            socket_path: state_dir.join("control.sock"),
            history_path: state_dir.join("history.bin"),
            exit: None,
            error: None,
        }
    }

    #[test]
    fn worker_liveness_rejects_recycled_pid_identity() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        let pid = record.worker_pid.unwrap();
        let identity = ProcessIdentity {
            pid,
            start_time_ticks: process_start_time_ticks(pid).unwrap() + 1,
            boot_id: linux_boot_id().unwrap(),
        };
        fs::write(
            state.path().join(WORKER_IDENTITY_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();

        assert!(!record.worker_alive());
        record.phase = Phase::Failed;
        assert!(record.worker_finished());
    }

    #[test]
    fn worker_liveness_uses_safe_legacy_fallback_for_missing_or_corrupt_identity() {
        let state = tempfile::tempdir().unwrap();
        let record = liveness_record(state.path());
        assert!(record.worker_alive(), "missing sidecar uses numeric pid");

        fs::write(state.path().join(WORKER_IDENTITY_FILE), b"not-json").unwrap();
        assert!(record.worker_alive(), "corrupt sidecar fails closed");

        let identity = ProcessIdentity {
            pid: record.worker_pid.unwrap() + 1,
            start_time_ticks: 0,
            boot_id: "corrupt".into(),
        };
        fs::write(
            state.path().join(WORKER_IDENTITY_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        assert!(record.worker_alive(), "pid mismatch fails closed");
    }

    #[test]
    fn frame_round_trip() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, FrameKind::Data, b"a\0b").unwrap();
        let mut cursor = io::Cursor::new(bytes);
        let frame = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(frame.kind, FrameKind::Data);
        assert_eq!(frame.payload, b"a\0b");
    }
    #[test]
    fn bounded_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::open(dir.path().join("h"), 4).unwrap();
        h.append(b"abcdef").unwrap();
        assert_eq!(h.snapshot(None), b"cdef");
    }
    #[test]
    fn sizes() {
        assert_eq!(parse_byte_size("2MiB").unwrap(), 2 * 1024 * 1024);
    }

    #[test]
    fn kill_grace_is_bounded_before_duration_or_deadline_math() {
        assert_eq!(
            kill_grace_duration(MAX_KILL_GRACE_MS).unwrap(),
            Duration::from_millis(MAX_KILL_GRACE_MS)
        );
        assert!(kill_grace_duration(MAX_KILL_GRACE_MS + 1).is_err());
        assert!(kill_grace_duration(u64::MAX).is_err());
    }

    #[test]
    fn cgroup_counter_read_errors_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("cgroup.events");

        assert!(read_counter(&events, "populated").is_err());
        fs::write(&events, "frozen 0\n").unwrap();
        assert!(read_counter(&events, "populated").is_err());
        fs::write(&events, "populated nope\n").unwrap();
        assert!(read_counter(&events, "populated").is_err());
        fs::write(&events, "populated 1\n").unwrap();
        assert_eq!(read_counter(&events, "populated").unwrap(), 1);
    }

    #[test]
    fn recorded_cgroup_cleanup_checks_deadline_before_locator_io() {
        let id = Uuid::new_v4();
        let locator = PathBuf::from(format!("/sys/fs/cgroup/aplexer-workload-{id}.scope"));
        let error = cleanup_recorded_cgroup_until(
            id,
            &locator,
            libc::SIGKILL,
            Duration::ZERO,
            Instant::now(),
        )
        .expect_err("expired cleanup must stop before locator inspection");
        assert!(error.to_string().contains("timed out validating"));
    }

    #[test]
    fn recorded_cgroup_cleanup_rejects_untrusted_locator() {
        let id = Uuid::new_v4();
        let error = cleanup_recorded_cgroup_until(
            id,
            Path::new("/tmp/not-a-cgroup"),
            libc::SIGKILL,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("untrusted locator must fail closed");
        assert!(error
            .to_string()
            .contains("untrusted recorded cgroup locator"));
    }

    #[test]
    fn cgroup_recovery_pidfds_preserve_descriptor_reserve() {
        assert_eq!(
            cgroup_recovery_pidfd_capacity_from_counts(CGROUP_RECOVERY_FD_RESERVE, 0),
            0
        );
        assert_eq!(
            cgroup_recovery_pidfd_capacity_from_counts(CGROUP_RECOVERY_FD_RESERVE + 7, 3),
            4
        );
        assert_eq!(
            cgroup_recovery_pidfd_capacity_from_counts(u64::MAX, 0),
            MAX_CGROUP_RECOVERY_MEMBERS
        );
    }

    #[test]
    fn cgroup_member_fallback_uses_identity_pinned_signal() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cgroup.procs"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        signal_cgroup_path_until(dir.path(), 0, Instant::now() + Duration::from_secs(1))
            .expect("pidfd signal-zero probe");
    }

    #[test]
    fn legacy_exit_info_remains_a_containment_proof() {
        let state = tempfile::tempdir().unwrap();
        let mut value = serde_json::to_value(liveness_record(state.path())).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("containment_cgroup");
        object.remove("containment_empty");
        object.insert("phase".into(), serde_json::json!("exited"));
        object.insert(
            "exit".into(),
            serde_json::json!({
                "code": 0,
                "signal": null,
                "oom_killed": false,
                "exited_at_ms": 2
            }),
        );
        let terminal: SessionRecord = serde_json::from_value(value.clone()).unwrap();
        assert!(terminal.containment_proven_empty());

        value.as_object_mut().unwrap().remove("exit");
        value
            .as_object_mut()
            .unwrap()
            .insert("phase".into(), serde_json::json!("failed"));
        let ambiguous: SessionRecord = serde_json::from_value(value).unwrap();
        assert!(!ambiguous.containment_proven_empty());
    }

    #[test]
    fn session_metadata_keeps_only_transcript_roots() {
        let env = BTreeMap::from([
            ("CODEX_HOME".to_string(), "/profiles/codex".to_string()),
            ("API_TOKEN".to_string(), "secret".to_string()),
        ]);
        assert_eq!(
            session_metadata_env(&env),
            BTreeMap::from([("CODEX_HOME".to_string(), "/profiles/codex".to_string())])
        );
    }
    /// The load-bearing property from pocketshell-integration-plan.md 0.2: a
    /// custom engine's own (smaller/different) `env_unset` can only ADD to
    /// the forced provider-key union, never replace or shrink it.
    #[test]
    fn env_unset_union_is_forced() {
        let mut config = Config {
            default_engine: Some("custom".into()),
            ..Config::default()
        };
        config.engines.insert(
            "custom".into(),
            EngineConfig {
                command: vec!["true".into()],
                env: BTreeMap::new(),
                // deliberately includes a name already in the forced list
                // (to exercise dedup) plus one new name.
                env_unset: vec!["ANTHROPIC_API_KEY".into(), "MY_CUSTOM_VAR".into()],
                skip_permissions_argv: Vec::new(),
            },
        );
        let launch = config
            .resolve(
                Vec::new(),
                None,
                None,
                Path::new("/tmp"),
                None,
                &BTreeMap::new(),
                &Limits::default(),
                None,
            )
            .unwrap();
        for name in PROVIDER_ENV_UNSET_VARS {
            assert!(
                launch.env_unset.iter().any(|v| v == name),
                "forced provider var {name} missing from env_unset"
            );
        }
        assert!(launch.env_unset.iter().any(|v| v == "MY_CUSTOM_VAR"));
        let count = launch
            .env_unset
            .iter()
            .filter(|v| v.as_str() == "ANTHROPIC_API_KEY")
            .count();
        assert_eq!(count, 1, "ANTHROPIC_API_KEY must not be duplicated");
        assert_eq!(
            launch.env_unset.len(),
            PROVIDER_ENV_UNSET_VARS.len() + 1,
            "union must be exactly the forced list plus the one new custom name"
        );
    }
    #[test]
    fn skip_permissions_argv_ported_values() {
        let config = Config {
            engines: BTreeMap::from([(
                "claude".to_string(),
                EngineConfig {
                    command: vec!["claude".into()],
                    env: BTreeMap::new(),
                    env_unset: Vec::new(),
                    skip_permissions_argv: vec!["--dangerously-skip-permissions".into()],
                },
            )]),
            ..Config::default()
        };
        let launch = config
            .resolve(
                Vec::new(),
                Some("claude"),
                None,
                Path::new("/tmp"),
                None,
                &BTreeMap::new(),
                &Limits::default(),
                None,
            )
            .unwrap();
        assert_eq!(
            launch.skip_permissions_argv,
            vec!["--dangerously-skip-permissions".to_string()]
        );
    }
}
