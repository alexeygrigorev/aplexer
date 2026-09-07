#![cfg(target_os = "linux")]

pub mod agent_events;
pub mod agent_kind;
pub mod api;
pub mod hooks;
pub mod messaging;
pub mod placement;
pub mod screen;
pub mod watch;
pub mod worker;

mod registry;
pub use registry::{read_record, read_session_record, list_records, resolve_record};

mod paths;
pub use paths::{Paths, ensure_private_dir, canonical_workspace};
use paths::*;

mod persist;
pub use persist::{atomic_write_json, atomic_write_bytes, FileLock};
use persist::*;

#[cfg(feature = "python")]
mod python;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;
pub const PROTOCOL_VERSION: u16 = 1;
pub const DEFAULT_HISTORY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Global per-session raw-history ceiling. The ring is resident in every
/// worker and periodically copied for atomic persistence, while protocol and
/// post-mortem capture can expose at most one frame, so retaining more than a
/// maximum-sized frame adds memory/write amplification without a usable read
/// path.
pub const MAX_HISTORY_BYTES: usize = MAX_FRAME_BYTES;
const MAX_CGROUP_RECOVERY_MEMBERS: usize = 4096;
const MAX_CGROUP_PROCS_BYTES: u64 = 128 * 1024;
const CGROUP_RECOVERY_FD_RESERVE: u64 = 16;
/// Long enough for graceful shutdown, but bounded so an authenticated local
/// client cannot monopolize a worker's serialized kill path indefinitely.
pub const MAX_KILL_GRACE_MS: u64 = 30_000;
/// The default `--startup-timeout-ms` for `a start` / `a here`, and the same
/// bound query-time code uses to decide whether a `Starting` record that has
/// not registered a worker pid is still coming up or is a crashed start.
///
/// One constant on purpose: the startup contract and the rule that reads it
/// cannot drift apart, and no consumer has to invent an age bound of its own
/// (issue #9). `--startup-timeout-ms` is per-invocation and not persisted, so
/// a query-time reader can only use the default; a start given a longer
/// timeout than this simply reads `broken` for the remainder of its own
/// window, which is the conservative direction.
pub const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;

pub fn validate_history_bytes(value: usize) -> Result<usize> {
    if value > MAX_HISTORY_BYTES {
        bail!("history_bytes {value} exceeds the maximum of {MAX_HISTORY_BYTES} bytes (16 MiB)");
    }
    Ok(value)
}

/// Restore the standalone process contract needed by `std::process::Child`.
/// This changes a process-wide disposition and is therefore reserved for the
/// CLI and worker binaries, never the embeddable Rust/Python API.
pub fn normalize_sigchld_for_child_management() -> Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error()).context("restore SIGCHLD default disposition");
        }
    }
    Ok(())
}

/// Validate, without changing it, the embedding process's SIGCHLD contract.
/// Custom handlers remain installed. SIG_IGN and SA_NOCLDWAIT are rejected
/// because either may auto-reap a worker before the API can wait for it.
pub fn ensure_sigchld_compatible_for_child_management() -> Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) != 0 {
            return Err(io::Error::last_os_error()).context("inspect SIGCHLD disposition");
        }
        if action.sa_sigaction == libc::SIG_IGN {
            bail!(
                "SIGCHLD disposition is SIG_IGN; in-process session startup requires child wait ownership"
            );
        }
        if action.sa_flags & libc::SA_NOCLDWAIT != 0 {
            bail!(
                "SIGCHLD disposition uses SA_NOCLDWAIT; in-process session startup requires child wait ownership"
            );
        }
    }
    Ok(())
}

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

/// `a state-report <state>` vocabulary (docs/pocketshell-integration-plan.md
/// Open question #2, "Agent-state ingestion"). A Claude/Codex/OpenCode hook
/// running inside a session pushes one of these; `watch.rs`'s
/// `agent.state` merge logic treats a fresh push as authoritative over its
/// own PTY-recency heuristic (see `watch::fresh_reported_state`). Chosen to
/// match PocketShell's own `SessionAgentState` granularity
/// (Idle/WaitingForInput/Working) one-for-one rather than inventing a
/// fourth aplexer-only vocabulary for the same concept.
pub const REPORTED_AGENT_STATES: [&str; 3] = ["idle", "waiting", "working"];

pub fn validate_reported_state(state: &str) -> Result<()> {
    if !REPORTED_AGENT_STATES.contains(&state) {
        bail!("invalid reported state {state:?}; expected one of {REPORTED_AGENT_STATES:?}");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
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

fn validate_limits(limits: &Limits, context: &str) -> Result<()> {
    for (name, value) in [
        ("memory_bytes", limits.memory_bytes),
        ("pids", limits.pids),
        ("cpu_quota_us", limits.cpu_quota_us),
        ("cpu_period_us", limits.cpu_period_us),
    ] {
        if value == Some(0) {
            bail!("{context} {name} must be greater than zero");
        }
    }
    match (limits.cpu_quota_us, limits.cpu_period_us) {
        (None, Some(_)) => bail!("{context} cpu_period_us requires cpu_quota_us"),
        (Some(quota), period) => {
            let period = period.unwrap_or(100_000);
            let percent = (u128::from(quota) * 100).div_ceil(u128::from(period));
            if percent > u128::from(u64::MAX) {
                bail!("{context} cpu_quota_us/cpu_period_us ratio is too large");
            }
        }
        (None, None) => {}
    }
    Ok(())
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

impl Phase {
    /// The wire/display name, identical to this enum's serde representation.
    pub fn name(&self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Running => "running",
            Phase::Exiting => "exiting",
            Phase::Exited => "exited",
            Phase::Failed => "failed",
        }
    }
}

/// A persisted `phase` of Starting/Running/Exiting only means the worker
/// *said* it was in that phase the last time it wrote its record -- if the
/// worker process has since died (e.g. SIGKILL, which gives it no chance to
/// update the record), that phase is stale. `phase` and worker liveness are
/// different facts (spec.md 20: "session worker alive" vs "workload alive"
/// vs "agent semantic state" are different facts), so this derives a third
/// one from both instead of rewriting either: the persisted `phase` stays
/// exactly what the worker wrote, and `worker_alive` stays the live probe.
///
/// `start_session` persists a `Starting` record with `worker_pid: None`
/// before the worker can register its pid, so `worker_alive` is false for the
/// first tens of milliseconds of every healthy `a start`. Reporting that as
/// `broken` conflated two opposite situations that are byte-identical in the
/// record: **starting** (no worker *yet*, transient, resolves in
/// milliseconds) and **broken** (the worker died, permanent, needs
/// `a prune`). A consumer mapping `broken -> dead` therefore treated every
/// session as reclaimable for the first instants of its life (issue #9).
///
/// Age is what separates them, and `DEFAULT_STARTUP_TIMEOUT_MS` -- aplexer's
/// own startup contract, not a fresh magic number -- is the bound: a
/// `Starting` record younger than that is `starting`; older, it is the
/// crashed-start record `a prune` can reap, so it is `broken`.
/// `Running`/`Exiting` get no such grace: those phases are only ever written
/// by a worker that had already registered.
///
/// Both `a status`'s `state:` line and every `a list --json`/`a snapshot`
/// row's `state` field come from here, so the two commands cannot disagree
/// about whether a session is broken -- the disagreement that let a machine
/// consumer read a zombie record as an ordinary running session.
pub fn observed_state(
    phase: &Phase,
    worker_alive: bool,
    created_at_ms: u64,
    now_ms: u64,
) -> &'static str {
    if matches!(phase, Phase::Starting | Phase::Running | Phase::Exiting) && !worker_alive {
        if within_startup_window(phase, created_at_ms, now_ms) {
            phase.name()
        } else {
            "broken"
        }
    } else {
        phase.name()
    }
}

/// Whether a record is still inside `a start`'s own startup budget, i.e. a
/// worker may legitimately not have finished coming up yet.
///
/// The single expression of that rule: `observed_state` uses it to keep a
/// mid-create record out of `broken`, and `a attach`'s `check_attachable`
/// uses it to keep the "your runtime directory was destroyed, run `a kill`"
/// diagnosis off a session that is merely still binding its control socket.
/// Note that this is true for a live worker too -- `Starting` means the
/// worker has not published `phase: running` yet, whatever its pid says --
/// so a caller that cares about liveness must test that separately.
pub fn within_startup_window(phase: &Phase, created_at_ms: u64, now_ms: u64) -> bool {
    matches!(phase, Phase::Starting)
        && now_ms.saturating_sub(created_at_ms) < DEFAULT_STARTUP_TIMEOUT_MS
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub oom_killed: bool,
    pub exited_at_ms: u64,
}

/// Kernel identity of the cgroup-v2 domain in which a limited session was
/// created. A pathname alone is not durable authority: after reboot, or from
/// another container/cgroup/mount namespace, the same text can name an
/// unrelated domain. Recovery compares every field before inspecting or
/// signalling the recorded path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CgroupIdentity {
    pub boot_id: String,
    pub cgroup_namespace_device: u64,
    pub cgroup_namespace_inode: u64,
    pub mount_namespace_device: u64,
    pub mount_namespace_inode: u64,
    pub cgroup_mount_id: u64,
    pub cgroup_root_device: u64,
    pub cgroup_root_inode: u64,
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
    /// Session this one was started from: the `a start` client's ambient
    /// `APLEXER_SESSION_ID` (`discover_session_id`), recorded only when it
    /// named a session record that still existed at start time. Best-effort
    /// provenance, not an enforced hierarchy -- the parent may be killed or
    /// forgotten later, leaving this pointing at a removed record on
    /// purpose, so the lineage fact survives the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<Uuid>,
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
    /// Last time a client attached to this session's PTY. Distinct from
    /// `last_activity_ms` (workload output) and `updated_at_ms` (any record
    /// write): attach is the "I looked at this" event used to sort
    /// workspaces by recency of access. Absent on records created before
    /// the field existed, and on sessions that have never been attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_accessed_ms: Option<u64>,
    /// Semantic state a hook running inside the session pushed via
    /// `a state-report` (docs/pocketshell-integration-plan.md Open question
    /// #2), one of `REPORTED_AGENT_STATES`. `None` means no hook has ever
    /// reported for this session -- `a watch`'s heuristic runs unmodified.
    /// See `watch::fresh_reported_state` for how this is merged with
    /// `last_activity_ms`-derived state and for how long it stays
    /// authoritative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_state: Option<String>,
    /// When `reported_state` was written (`WorkerRuntime::report_state`),
    /// ms since epoch. Always `Some` when `reported_state` is `Some`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_state_at_ms: Option<u64>,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_pid: Option<u32>,
    /// The cgroup-v2 path (`/proc/<pid>/cgroup` `0::` form, e.g.
    /// `/user.slice/user-1000.slice/session-8.scope`) the worker process was
    /// actually in, read from `/proc` by the worker itself right after it
    /// published its pid (issue #1). `setsid()` gave the worker a new
    /// session but did NOT move it out of whichever service manager owns
    /// this subtree, so this path is the durable evidence of which manager
    /// can kill the session wholesale -- `a doctor` and the placement
    /// summary fields classify it via `placement::classify_cgroup_path`.
    /// `None` on records started before the field existed and when the
    /// read failed (recording nothing beats recording a guess).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_cgroup: Option<String>,
    /// Same evidence for the workload's leader process, read right after
    /// the spawn. For a resource-limited session this is cross-checked at
    /// launch against `containment_cgroup` (the scope systemd was asked to
    /// create): disagreement is logged to worker.log because it means the
    /// limits were not applied where the workload actually runs. An
    /// unlimited session shares the worker's cgroup, so the two fields
    /// normally agree there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_cgroup: Option<String>,
    /// Kernel containment domain for resource-limited sessions. Recovery
    /// code must validate this path against the session id before using it;
    /// an absent locator is never equivalent to an empty domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment_cgroup: Option<PathBuf>,
    /// Boot, namespace, and mount identity that makes
    /// `containment_cgroup` authoritative. Older records omit this field and
    /// remain readable, but destructive recovery from them fails closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment_cgroup_identity: Option<CgroupIdentity>,
    /// Durable proof that the worker (or an independent cgroup recovery)
    /// observed the complete containment domain empty. Numeric leader PIDs
    /// are not such proof because descendants may call `setsid` and outlive
    /// both the original process group and the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment_empty: Option<bool>,
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

    /// New records persist `containment_empty` explicitly, including `false`.
    /// Before that field existed, ExitInfo was written only after the lifecycle
    /// loop observed the full subreaper/cgroup domain empty, so an exit remains
    /// a valid proof only when the field is absent. Explicit `false` always
    /// wins: a newer failed lifecycle must not be upgraded by its ExitInfo.
    pub fn containment_proven_empty(&self) -> bool {
        self.containment_empty
            .unwrap_or_else(|| self.exit.is_some())
    }

    /// Whether the worker process is still running. This is a cheap,
    /// pessimistic check for legacy records.
    ///
    /// A worker that exited but has not been reaped by its parent is dead,
    /// not alive -- see `process_alive`. That is the common shape when the
    /// session was started from inside another aplexer session, whose worker
    /// is a child subreaper and therefore inherits the corpse.
    ///
    /// New records also pin the worker's boot and process start time, so a
    /// recycled numeric pid does not keep a dead session alive forever. An
    /// absent or unreadable identity sidecar deliberately falls back to the
    /// legacy pid check: uncertainty must not let prune/tag replacement
    /// delete a live worker.
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

    /// Whether the recorded workload leader is still running (an unreaped
    /// zombie leader is not: see `process_alive`). A record
    /// with no leader pid never had one recorded; that is not a liveness
    /// claim either way, only the absence of this particular handle.
    pub fn workload_leader_alive(&self) -> bool {
        self.workload_pid.map(process_alive).unwrap_or(false)
    }

    /// Whether the record itself says the session is on its way out, so a
    /// caller waiting for the worker to disappear is waiting for something
    /// that is actually going to happen. Either the worker wrote a
    /// terminal/exiting phase, or the workload leader it was supervising is
    /// already gone (the worker exits shortly after its workload does, and
    /// it may not have updated the phase yet -- the exact window that made
    /// `a kill` followed by `a prune` a no-op).
    pub fn worker_is_terminating(&self) -> bool {
        matches!(self.phase, Phase::Exiting | Phase::Exited | Phase::Failed)
            || (self.workload_pid.is_some() && !self.workload_leader_alive())
    }

    /// The derived liveness state, identical to `a status`'s `state:` line.
    pub fn observed_state(&self) -> &'static str {
        self.observed_state_at(now_ms())
    }

    /// `observed_state` with an injected clock, so a test can pin the
    /// startup-window boundary without sleeping out
    /// `DEFAULT_STARTUP_TIMEOUT_MS`.
    pub fn observed_state_at(&self, now_ms: u64) -> &'static str {
        observed_state(&self.phase, self.worker_alive(), self.created_at_ms, now_ms)
    }
}

/// What removing a record's durable state would cost, containment-wise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainmentReap {
    /// Nothing survives this session: either the worker durably proved its
    /// containment domain empty, or the domain was observed empty just now.
    Proven,
    /// No proof either way, but the record holds no handle to whatever might
    /// have survived, so keeping it protects nothing.
    NoRemainingHandle,
    /// Something may still be inside the containment domain AND this record
    /// is how it can still be reached. Keep it.
    Retain,
}

/// Decide whether prune may drop this record's durable state.
///
/// `containment_proven_empty()` alone is the wrong predicate for a broken
/// record: a worker that was SIGKILLed never wrote `containment_empty`, and
/// the legacy `exit.is_some()` fallback is false for exactly the same
/// reason, so a crashed session can never satisfy it no matter how long it
/// sits there. The question that actually matters is not "did somebody once
/// prove this empty" but "would removing this record lose the last handle to
/// a process that might still be running":
///
/// * durable proof -- trust it, unchanged;
/// * a recorded cgroup -- ask the kernel *now*. That is strictly stronger
///   evidence than the persisted bit, and it is also the handle at risk: a
///   populated (or unreadable, or unvalidatable) domain must keep its
///   locator, so anything short of an observed-empty answer retains;
/// * no recorded cgroup -- an unlimited session's only containment boundary
///   was its worker's subreaper tree, which died with the worker. A `setsid`
///   descendant may have escaped, and the record retains no *programmatic*
///   handle to it: `a kill` refuses such a record ("no authoritative
///   containment locator"), and the leader pid, if any, is already gone.
///   What the record does still hold is a manual-investigation trail --
///   `command`, `cwd`, `workspace`, the persisted transcript -- and reaping
///   it drops that trail. That is a deliberate trade, not an oversight: the
///   alternative is a record no command can ever remove except
///   `a forget --force`, whose "workload processes may survive" warning is
///   itself misleading here, sitting in `a list` forever (issue: three such
///   records, 234-254h old, on the maintainer's box). So it is reapable, and
///   the caller reports it as an unproven reap rather than pretending the
///   domain was clean.
///
/// ## Supersedes: `recover_broken_containment`'s preservation rule
///
/// `a kill`/`a forget` deliberately preserve a broken unlimited session's
/// durable and runtime evidence "for manual investigation rather than
/// reporting a false cleanup success"
/// (`a.rs::recover_broken_containment`, and
/// `tests/containment_recovery.rs::kill_preserves_evidence_when_dead_unlimited_worker_loses_setsid_descendant`,
/// which asserts exactly that). That rule is unchanged for `a kill` -- it
/// still refuses, and still preserves both directories, because it would be
/// claiming a cleanup it cannot perform.
///
/// `a prune` now supersedes it. The state that sibling test builds is, once
/// its worker is killed, precisely the class reaped here: closing the PTY
/// kills the leader `sh` with SIGHUP, leaving only the `setsid` descendant
/// that traps it. Running prune there removes both directories and the
/// descendant outlives the reap, unsignalled, with no record left naming the
/// session it came from -- measured, not inferred, and pinned end to end by
/// `tests/prune_dead_records.rs::prune_reaps_a_record_whose_setsid_descendant_escaped`.
/// The trade is deliberate: the alternative is a record that no routine
/// command can ever remove, which is the bug this exists to fix.
///
/// Caller-side guards, deliberately NOT folded in here: a live worker and a
/// live workload leader are checked separately and always retain.
pub fn containment_reap_verdict(record: &SessionRecord) -> ContainmentReap {
    containment_reap_verdict_with(record, recorded_cgroup_observed_empty)
}

/// The decision table, with the kernel probe injected so every arm --
/// including `Ok(false) => Retain`, the one arm standing between prune and a
/// live containment domain -- is pinned by an ordinary unit test on any
/// machine, not only one with cgroup-v2 delegation. The real probe is
/// covered separately against a real cgroup (see
/// `recorded_cgroup_observed_empty_tracks_a_real_delegated_cgroup`).
fn containment_reap_verdict_with(
    record: &SessionRecord,
    probe: impl FnOnce(Uuid, &Path, Option<&CgroupIdentity>) -> Result<bool>,
) -> ContainmentReap {
    if record.containment_proven_empty() {
        return ContainmentReap::Proven;
    }
    let Some(locator) = record.containment_cgroup.as_deref() else {
        return ContainmentReap::NoRemainingHandle;
    };
    match probe(
        record.id,
        locator,
        record.containment_cgroup_identity.as_ref(),
    ) {
        Ok(true) => ContainmentReap::Proven,
        // Populated, or any inspection/validation error: fail closed and keep
        // the locator, exactly like `recover_broken_containment`'s preflight.
        Ok(false) | Err(_) => ContainmentReap::Retain,
    }
}

/// Read-only membership check for a durably recorded cgroup. Signals
/// nothing and removes nothing.
fn recorded_cgroup_observed_empty(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
) -> Result<bool> {
    // A cgroup recorded under a different boot cannot hold a live process:
    // the hierarchy is rebuilt empty at boot, so both the domain and every
    // task that was in it are gone. Checked before validate_recorded_cgroup,
    // which (correctly, for destructive recovery) refuses to act at all on a
    // foreign-boot identity and would otherwise make a rebooted-away record
    // permanently unreapable.
    if let Some(identity) = identity {
        if identity.boot_id != linux_boot_id()? {
            return Ok(true);
        }
    }
    let Some(path) = validate_recorded_cgroup(id, locator, identity)? else {
        // The directory is gone. cgroup v2 cannot remove a populated cgroup,
        // so a locator that was durably recorded and has since disappeared
        // is empty by construction.
        return Ok(true);
    };
    Ok(!cgroup_path_populated(&path)?)
}

/// Whether nothing that could still be running depends on this record, and
/// therefore whether its durable state may be destroyed. Three independent
/// facts, all required:
///
///  1. no live worker -- `worker_alive()` stays the authority, including its
///     deliberate fallback to the bare pid check when the identity sidecar
///     is missing or unreadable, so uncertainty retains rather than reaps;
///  2. no live workload leader -- the safety property `a forget --force`
///     exists to override, unchanged;
///  3. containment holds no remaining handle (see `containment_reap_verdict`).
///
/// Note what is deliberately NOT required: `worker_finished()`, a terminal
/// *phase*. A worker killed with SIGKILL never gets to write one, which is
/// why records at `phase: running, worker_alive: false` could never be
/// reaped by any number of `a prune` runs.
///
/// Every command that destroys a record's durable state answers this one
/// question, so they cannot drift apart about what "dead" means:
/// `a prune`'s `reap_session_state`, and `start_session`'s reclaim of a
/// `workspace+tag` its holder no longer needs (spec.md 32.1) -- a reclaim
/// archives and then deletes the predecessor, which is the same destruction
/// prune performs, so it must clear the same bar. `a kill` and
/// `a forget --force` deliberately do NOT use it: kill acts and must prove
/// its own cleanup, forget is the operator's explicit override of exactly
/// the safety this encodes.
///
/// `Some(verdict)` means removable, carrying whether containment was proven
/// empty or merely holds no remaining handle; `None` means retain.
///
/// Deliberately NOT startup-window aware, unlike `observed_state` (issue
/// #9). A pre-PID `Starting` stub has no live worker, no live leader and no
/// containment domain, so it is reapable here -- and it must be, or a
/// crashed start would be unprunable until its budget expired. What keeps
/// prune off a *healthy* start is not this predicate but a lock: the
/// destroying callers (`reap_session_state`, `a forget`, `start_session`'s
/// reclaim) take the worker lock for a pre-PID record via
/// `fence_pre_pid_worker`, and a worker already holding it is retained.
/// Pinned by `prune_fences_a_pre_pid_starting_record_against_its_worker_lock`
/// and `a_pre_pid_record_is_fenced_by_its_worker_lock`.
pub fn reap_verdict(record: &SessionRecord) -> Option<ContainmentReap> {
    if record.worker_alive() || record.workload_leader_alive() {
        return None;
    }
    match containment_reap_verdict(record) {
        ContainmentReap::Retain => None,
        verdict => Some(verdict),
    }
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

/// Whether `pid` names a process that can still run code.
///
/// `kill(pid, 0)` alone is NOT that question. It succeeds for a zombie: an
/// exited process whose parent has not yet reaped it still occupies its pid
/// slot and still accepts (and discards) signals. Every liveness decision in
/// aplexer -- `worker_alive`, `workload_leader_alive`, and through them
/// `reap_verdict` and `a prune` -- is really asking "is there anything left
/// that could act", and a zombie's answer is no.
///
/// This matters because aplexer workers are child subreapers
/// (`PR_SET_CHILD_SUBREAPER`), so a session started from inside another
/// aplexer session reparents to that outer worker when its own parent goes
/// away. If the outer worker does not reap it, the dead session's pid stays
/// signalable indefinitely and every probe here reported it alive forever:
/// `a prune` retained records it should have removed, and tests that wait
/// for a pid to die failed with "pid NNNN did not die".
///
/// Uncertainty still fails closed: an unreadable `/proc/<pid>/stat` (a
/// hardened procfs, a racing exit) leaves the answer at the signalable
/// result, so a live process is never mistaken for a dead one.
pub fn process_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    let signalable = rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    signalable && !process_is_zombie(pid)
}

/// The single-character run state from field 3 of `/proc/<pid>/stat`
/// (`R` running, `S`/`D` sleeping, `T` stopped, `Z` zombie, `X` dead).
pub fn process_state(pid: u32) -> Result<char> {
    process_state_in(Path::new(crate::agent_kind::DEFAULT_PROC_ROOT), pid)
}

/// `process_state` against an arbitrary `/proc` root, so the zombie rules
/// below can be pinned by ordinary unit tests on a synthetic tree instead of
/// requiring a real process in a specific state -- the same split
/// `direct_child_pids_in` uses.
fn process_state_in(proc_root: &Path, pid: u32) -> Result<char> {
    let stat_path = proc_root.join(pid.to_string()).join("stat");
    let stat =
        fs::read_to_string(&stat_path).with_context(|| format!("read {}", stat_path.display()))?;
    // The parenthesized comm field may itself contain spaces or `)`, so the
    // state is the first token after its final close-paren, never field 3 of
    // a naive whitespace split.
    stat.rfind(')')
        .and_then(|end| stat.get(end + 1..))
        .and_then(|after_comm| after_comm.split_whitespace().next())
        .and_then(|state| state.chars().next())
        .ok_or_else(|| anyhow!("malformed {}", stat_path.display()))
}

/// Whether `pid` has exited but has not been reaped by its parent.
///
/// The `Z` in `/proc/<pid>/stat` is necessary but not sufficient. A thread
/// group leader that called `pthread_exit` (or bare `exit(2)`) while its
/// sibling threads keep running also reads as `Z`, and that process is very
/// much still executing code -- measured, not assumed: such a leader shows
/// `state=Z` with two entries under `/proc/<pid>/task`. Calling it dead
/// would let a multi-threaded workload be declared contained while its
/// threads ran on. So the thread group must also be down to nothing but the
/// leader's corpse, which is exactly the state `waitpid` will return for.
///
/// Every unreadable answer is reported as "not a zombie": callers use this
/// to subtract the dead from a liveness answer, and an unknown state must
/// never subtract a process that may still be running.
pub fn process_is_zombie(pid: u32) -> bool {
    process_is_zombie_in(Path::new(crate::agent_kind::DEFAULT_PROC_ROOT), pid)
}

fn process_is_zombie_in(proc_root: &Path, pid: u32) -> bool {
    matches!(process_state_in(proc_root, pid), Ok('Z'))
        && thread_group_holds_only_the_leader(proc_root, pid)
}

/// Whether `<proc>/<pid>/task` contains exactly one entry, i.e. no sibling
/// thread of `pid` is left. Any read failure answers `false`, keeping the
/// caller on the "may still be running" side.
fn thread_group_holds_only_the_leader(proc_root: &Path, pid: u32) -> bool {
    let Ok(tasks) = fs::read_dir(proc_root.join(pid.to_string()).join("task")) else {
        return false;
    };
    let mut seen = 0_usize;
    for task in tasks {
        if task.is_err() {
            return false;
        }
        seen += 1;
        if seen > 1 {
            return false;
        }
    }
    seen == 1
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
    // Cached: the boot id cannot change without a reboot, and every
    // `worker_alive` probe (i.e. every `a list` row, twice per row in the old
    // plain rendering) read it from disk. Benchmark PLAN P1.1: `a list` with
    // dozens of sessions did dozens of redundant reads of this one tiny
    // file; cache it process-wide after the first successful read.
    static CACHED_BOOT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(cached) = CACHED_BOOT_ID.get() {
        return Ok(cached.clone());
    }
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("read Linux boot identity")?;
    let boot_id = boot_id.trim();
    if boot_id.is_empty() {
        bail!("Linux boot identity is empty");
    }
    let owned = boot_id.to_owned();
    let _ = CACHED_BOOT_ID.set(owned.clone());
    Ok(owned)
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Additional vars to unset at spawn time. Agent engines add these to the
    /// forced `PROVIDER_ENV_UNSET_VARS` union; the literal `shell` engine uses
    /// exactly this configured list so ordinary shell commands retain their
    /// ambient/provider environment unless the user explicitly opts out.
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
    /// Effective unset policy for one named engine. `shell` is the deliberate
    /// non-agent exception; all other built-in and custom engine ids retain
    /// the subscription-auth provider stripping policy.
    pub fn resolved_env_unset(&self, engine_name: &str) -> Vec<String> {
        if engine_name == "shell" {
            ordered_unique_env_names(std::iter::empty(), &self.env_unset)
        } else {
            ordered_unique_env_names(PROVIDER_ENV_UNSET_VARS.iter().copied(), &self.env_unset)
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
pub struct ShortcutConfig {
    pub engine: String,
    #[serde(default)]
    pub profile: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
    /// Keep a durable record for a session whose workload finished.
    ///
    /// Default `false`: a session that ends -- `exit`, Ctrl-D at a shell,
    /// a workload that returned non-zero, a signalled workload, or `a kill`
    /// -- removes its own record and history, so it leaves `a list` the
    /// moment it is over instead of parking an `exited` row there until
    /// somebody runs `a prune`. Set `keep_exited = true` to retain those
    /// post-mortem records (`a status`, `a capture --screen`, the durable
    /// terminal transition a polling `a watch` can observe) and go back to
    /// pruning them explicitly.
    ///
    /// Never suppresses evidence the operator did not choose to lose:
    /// worker-side finalization failures, a containment domain that was
    /// not proven empty, and OOM kills keep their record regardless (see
    /// `run_lifecycle` in src/worker.rs).
    #[serde(default)]
    pub keep_exited: bool,
}
fn default_config_version() -> u32 {
    1
}

/// Read the `keep_exited` policy without building a full [`Config`].
///
/// The worker consults this on its exit path, where [`Config::load`] would
/// be the wrong tool twice over: it walks the filesystem for engine profile
/// discovery that finalization has no use for, and it fails the whole load
/// on an unrelated invalid engine/profile/shortcut entry -- which would flip
/// the retention policy as a side effect of a typo elsewhere in the file.
/// Only the single field is parsed here, so an unreadable or unparsable
/// config file (and a missing one, the common case) means the documented
/// default: do not keep exited records.
///
/// The `Config` field above stays the source of truth for the name and the
/// default; `config_keep_exited_matches_full_config_load` pins the two
/// readers together.
pub fn config_keep_exited(paths: &Paths) -> bool {
    #[derive(Deserialize)]
    struct KeepExitedOnly {
        #[serde(default)]
        keep_exited: bool,
    }
    fs::read_to_string(&paths.config_file)
        .ok()
        .and_then(|text| toml::from_str::<KeepExitedOnly>(&text).ok())
        .map(|parsed| parsed.keep_exited)
        .unwrap_or(false)
}

/// Transcript-family normalization: a variant engine -- a fork of a built-in
/// engine CLI with the same wire format and the same native conversation-log
/// location -- is identified with that engine's family for parsing, while
/// sessions and emitted events keep the variant's own id. `zcodex` is a
/// codex-rs fork (same `-c` overrides, same rollout JSONL under
/// `CODEX_HOME`/`~/.codex`), so it rides the codex machinery; everything
/// else is its own family.
pub fn engine_family(engine: &str) -> &str {
    match engine {
        "zcodex" => "codex",
        other => other,
    }
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
/// this with each agent engine's own `EngineConfig.env_unset`; the union is
/// forced for every engine id except the literal `shell` engine (see that
/// function's doc comment) -- an agent config can only add to this list,
/// never remove from it.
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

/// Preserve first-seen order and drop blanks/duplicates across the policy
/// prefix and configured additions.
fn ordered_unique_env_names<'a>(
    prefix: impl IntoIterator<Item = &'a str>,
    extra: &[String],
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |name: &str| {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    };
    for name in prefix {
        push(name);
    }
    for name in extra {
        push(name);
    }
    out
}

impl Config {
    fn validate(&self) -> Result<()> {
        if let Some(name) = &self.default_engine {
            if !self.engines.contains_key(name) {
                bail!("default_engine {name:?} does not reference a configured engine");
            }
        }
        if let Some(name) = &self.default_profile {
            if !self.profiles.contains_key(name) {
                bail!("default_profile {name:?} does not reference a configured profile");
            }
        }

        for (name, engine) in &self.engines {
            if name.is_empty() {
                bail!("engine name must not be empty");
            }
            if engine.command.is_empty() {
                bail!("engine {name:?} command must not be empty");
            }
            if engine.command[0].is_empty() {
                bail!("engine {name:?} command executable must not be empty");
            }
        }

        for (name, profile) in &self.profiles {
            if name.is_empty() {
                bail!("profile name must not be empty");
            }
            if let Some(engine) = &profile.engine {
                if !self.engines.contains_key(engine) {
                    bail!(
                        "profile {name:?} engine {engine:?} does not reference a configured engine"
                    );
                }
            }
            if profile.executable.as_deref() == Some("") {
                bail!("profile {name:?} executable must not be empty");
            }
            if let Some(command) = &profile.command {
                if command.is_empty() {
                    bail!("profile {name:?} command must not be empty");
                }
                if command[0].is_empty() {
                    bail!("profile {name:?} command executable must not be empty");
                }
                if profile.executable.is_some() {
                    bail!("profile {name:?} cannot set both command and executable");
                }
                if !profile.args.is_empty() {
                    bail!("profile {name:?} cannot set both command and args");
                }
            }
            if let Some(history_bytes) = profile.history_bytes {
                validate_history_bytes(history_bytes)
                    .with_context(|| format!("profile {name:?} history_bytes"))?;
            }
            validate_limits(&profile.limits, &format!("profile {name:?} limits"))?;
        }

        for (name, shortcut) in &self.shortcuts {
            if name.is_empty() {
                bail!("shortcut name must not be empty");
            }
            if !self.engines.contains_key(&shortcut.engine) {
                bail!(
                    "shortcut {name:?} engine {:?} does not reference a configured engine",
                    shortcut.engine
                );
            }
            if let Some(profile_name) = &shortcut.profile {
                let profile = self.profiles.get(profile_name).ok_or_else(|| {
                    anyhow!(
                        "shortcut {name:?} profile {profile_name:?} does not reference a configured profile"
                    )
                })?;
                if let Some(profile_engine) = &profile.engine {
                    if profile_engine != &shortcut.engine {
                        bail!(
                            "shortcut {name:?} selects engine {:?}, but profile {profile_name:?} selects engine {profile_engine:?}",
                            shortcut.engine
                        );
                    }
                }
            }
        }
        Ok(())
    }

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
        // `zcodex` is a codex variant (see `engine_family`): a codex-rs fork
        // with the same CLI surface and the same rollout log, so its launch
        // spec mirrors codex's exactly, with the fork's own binary name.
        config.engines.insert(
            "zcodex".into(),
            EngineConfig {
                command: vec![
                    "zcodex".into(),
                    "-c".into(),
                    "check_for_update_on_startup=false".into(),
                ],
                env: BTreeMap::new(),
                env_unset: Vec::new(),
                skip_permissions_argv: vec!["--dangerously-bypass-approvals-and-sandbox".into()],
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
            // A bool has no "unset" value to test the way the options above
            // do, and the built-in default is `false`, so the user's parsed
            // value simply is the answer.
            config.keep_exited = user.keep_exited;
        }
        // Profile-specific built-ins are useful only when discovery or user
        // config supplied their target profile. Insert them after merging so
        // they never create dangling references, while an explicit user
        // shortcut with the same id still wins.
        for (shortcut, engine, profile) in [
            ("clz", "claude", "zlaude"),
            ("coz", "codex", "zodex"),
            ("cog", "codex", "godex"),
        ] {
            if config.profiles.contains_key(profile) {
                config
                    .shortcuts
                    .entry(shortcut.into())
                    .or_insert_with(|| ShortcutConfig {
                        engine: engine.into(),
                        profile: Some(profile.into()),
                    });
            }
        }
        config.validate()?;
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
        validate_limits(&merged_limits, "resolved launch limits")?;
        let launch_cwd = cwd
            .map(Path::to_path_buf)
            .or_else(|| profile.and_then(|p| p.cwd.clone()))
            .unwrap_or_else(|| workspace.to_path_buf());
        // Forced provider-key union (pocketshell-integration-plan.md 1.4/0.2)
        // for agent engines: `PROVIDER_ENV_UNSET_VARS` comes first regardless
        // of a custom engine's additions. The exact `shell` engine id is the
        // deliberate exception: literal shells use only their configured
        // env_unset list, so explicit shell --env values are not silently
        // removed. Callers apply this after env_set at workload spawn.
        let env_unset = engine.resolved_env_unset(&selected_engine);
        let history_bytes = history_bytes
            .or_else(|| profile.and_then(|p| p.history_bytes))
            .unwrap_or(DEFAULT_HISTORY_BYTES);
        validate_history_bytes(history_bytes)?;
        Ok(ResolvedLaunch {
            engine: selected_engine,
            profile: selected_profile,
            command,
            cwd: launch_cwd,
            env: merged_env,
            env_unset,
            skip_permissions_argv: engine.skip_permissions_argv.clone(),
            limits: merged_limits,
            history_bytes,
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
    /// Effective vars that must be absent from the spawned workload, applied
    /// AFTER `env` at spawn time. Agent engines receive the forced provider
    /// union; the `shell` engine receives only its explicitly configured
    /// `env_unset` entries.
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
    fn is_executable_file(path: &Path) -> bool {
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
    }

    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return is_executable_file(candidate);
    }
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| is_executable_file(&dir.join(program))))
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
    /// Additive protocol binding for daemonless workers that can outlive a
    /// client upgrade. New clients always send it; older workers ignore the
    /// unknown field and remain controllable. New workers reject `None` with
    /// an explicit upgrade error, so outdated clients cannot issue unbound
    /// operations to a newly-created session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(flatten)]
    pub operation: Operation,
}
impl Request {
    pub fn new(session_id: Uuid, operation: Operation) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4().to_string(),
            session_id: Some(session_id),
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
    /// `a state-report <state>` (docs/pocketshell-integration-plan.md Open
    /// question #2, "Agent-state ingestion"): a hook running inside the
    /// session pushes its own semantic state, the missing half of `a watch
    /// --jsonl`'s `agent.state` PTY-recency heuristic. `state` must be one
    /// of `REPORTED_AGENT_STATES`; the worker validates and rejects
    /// anything else (`WorkerRuntime::report_state`) rather than writing an
    /// unrecognised value the watch merge logic would then have to guess
    /// at.
    ReportState {
        state: String,
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

const HISTORY_FORMAT_VERSION: u32 = 2;
const HISTORY_BANK_MAGIC: &[u8; 8] = b"APLXH2D\0";
const HISTORY_BANK_HEADER_PREFIX_BYTES: usize = 72;
const HISTORY_BANK_HEADER_BYTES: usize = HISTORY_BANK_HEADER_PREFIX_BYTES + 32;
const HISTORY_COMMIT_MAX_BYTES: usize = 4096;
const HISTORY_MARKER_MAX_BYTES: usize = 4096;
const HISTORY_BANK_COUNT: u8 = 2;
const HISTORY_COMMIT_COUNT: u8 = 2;

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn history_sidecar_path(path: &Path, kind: &str, slot: u8) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("history.bin"))
        .to_os_string();
    name.push(format!(".v2.{kind}.{slot}"));
    path.with_file_name(name)
}

fn history_data_path(path: &Path, slot: u8) -> PathBuf {
    history_sidecar_path(path, "data", slot)
}

fn history_commit_path(path: &Path, slot: u8) -> PathBuf {
    history_sidecar_path(path, "commit", slot)
}

fn history_marker_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("history.bin"))
        .to_os_string();
    name.push(".v2.marker");
    path.with_file_name(name)
}

fn history_session_id(path: &Path) -> Uuid {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse().ok())
        .unwrap_or_else(Uuid::nil)
}

fn validate_optional_history_node(path: &Path, label: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => bail!("{label} {} is not a regular file", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {label} {}", path.display())),
    }
}

fn open_optional_history_file(path: &Path, label: &str, write: bool) -> Result<Option<File>> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("open {label} {}", path.display()))
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("{label} {} is not a trusted regular file", path.display());
    }
    Ok(Some(file))
}

fn validate_history_artifacts(path: &Path) -> Result<()> {
    validate_optional_history_node(path, "legacy history")?;
    validate_optional_history_node(&history_marker_path(path), "history marker")?;
    for slot in 0..HISTORY_BANK_COUNT {
        let data_path = history_data_path(path, slot);
        if validate_optional_history_node(&data_path, "history data bank")? {
            let length = fs::symlink_metadata(&data_path)?.len();
            let hard_cap = HISTORY_BANK_HEADER_BYTES as u64 + 2 * MAX_HISTORY_BYTES as u64;
            if length > hard_cap {
                bail!(
                    "history data bank {} exceeds the {}-byte hard cap",
                    data_path.display(),
                    hard_cap
                );
            }
        }
    }
    for slot in 0..HISTORY_COMMIT_COUNT {
        validate_optional_history_node(&history_commit_path(path, slot), "history commit")?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HistoryMarker {
    format_version: u32,
    store_id: Uuid,
    session_id: Uuid,
    metadata_sha256: String,
}

impl HistoryMarker {
    fn seal(mut self) -> Result<Self> {
        self.metadata_sha256.clear();
        self.metadata_sha256 = sha256_hex(&serde_json::to_vec(&self)?);
        Ok(self)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        if self.format_version != HISTORY_FORMAT_VERSION {
            bail!("unsupported history marker format {}", self.format_version);
        }
        if self.store_id.is_nil() {
            bail!("history marker has a nil store id");
        }
        if self.session_id != history_session_id(path) {
            bail!("history marker belongs to a different session");
        }
        let mut unsigned = self.clone();
        let supplied = std::mem::take(&mut unsigned.metadata_sha256);
        let expected = sha256_hex(&serde_json::to_vec(&unsigned)?);
        if supplied != expected {
            bail!("history marker metadata checksum mismatch");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HistoryCommit {
    format_version: u32,
    store_id: Uuid,
    session_id: Uuid,
    commit_generation: u64,
    bank_generation: u64,
    data_slot: u8,
    capacity: u64,
    committed_len: u64,
    stream_end: u64,
    data_sha256: String,
    metadata_sha256: String,
}

impl HistoryCommit {
    fn seal(mut self) -> Result<Self> {
        self.metadata_sha256.clear();
        self.metadata_sha256 = sha256_hex(&serde_json::to_vec(&self)?);
        Ok(self)
    }

    fn validate(&self, path: &Path, metadata_slot: u8) -> Result<()> {
        if self.format_version != HISTORY_FORMAT_VERSION {
            bail!("unsupported history format {}", self.format_version);
        }
        if self.data_slot >= HISTORY_BANK_COUNT {
            bail!("history commit names invalid data slot {}", self.data_slot);
        }
        if self.commit_generation % HISTORY_COMMIT_COUNT as u64 != metadata_slot as u64 {
            bail!("history commit is stored in the wrong metadata slot");
        }
        if self.session_id != history_session_id(path) {
            bail!("history commit belongs to a different session");
        }
        if self.commit_generation == 0 || self.bank_generation == 0 {
            bail!("history generation counters must be positive");
        }
        let capacity = usize::try_from(self.capacity).context("history capacity does not fit")?;
        validate_history_bytes(capacity)?;
        let max_payload = self
            .capacity
            .checked_mul(2)
            .ok_or_else(|| anyhow!("history bank size overflow"))?;
        if self.committed_len > max_payload {
            bail!("history committed length exceeds its bounded bank size");
        }
        if self.committed_len > self.stream_end {
            bail!("history committed length exceeds its logical stream position");
        }
        if self.data_sha256.len() != 64
            || !self
                .data_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("history commit has an invalid data checksum");
        }
        let mut unsigned = self.clone();
        let supplied = std::mem::take(&mut unsigned.metadata_sha256);
        let expected = sha256_hex(&serde_json::to_vec(&unsigned)?);
        if supplied != expected {
            bail!("history commit metadata checksum mismatch");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct HistoryBankHeader {
    store_id: Uuid,
    session_id: Uuid,
    bank_generation: u64,
    data_slot: u8,
    capacity: u64,
}

impl HistoryBankHeader {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HISTORY_BANK_HEADER_BYTES);
        bytes.extend_from_slice(HISTORY_BANK_MAGIC);
        bytes.extend_from_slice(&HISTORY_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(HISTORY_BANK_HEADER_BYTES as u32).to_le_bytes());
        bytes.extend_from_slice(self.store_id.as_bytes());
        bytes.extend_from_slice(self.session_id.as_bytes());
        bytes.extend_from_slice(&self.bank_generation.to_le_bytes());
        bytes.push(self.data_slot);
        bytes.extend_from_slice(&[0; 7]);
        bytes.extend_from_slice(&self.capacity.to_le_bytes());
        debug_assert_eq!(bytes.len(), HISTORY_BANK_HEADER_PREFIX_BYTES);
        let checksum = Sha256::digest(&bytes);
        bytes.extend_from_slice(&checksum);
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != HISTORY_BANK_HEADER_BYTES {
            bail!("history bank header has the wrong length");
        }
        if &bytes[..8] != HISTORY_BANK_MAGIC {
            bail!("history bank magic mismatch");
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != HISTORY_FORMAT_VERSION {
            bail!("unsupported history bank format {version}");
        }
        let header_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        if header_len != HISTORY_BANK_HEADER_BYTES {
            bail!("history bank header length is invalid");
        }
        let expected = Sha256::digest(&bytes[..HISTORY_BANK_HEADER_PREFIX_BYTES]);
        if expected.as_slice() != &bytes[HISTORY_BANK_HEADER_PREFIX_BYTES..] {
            bail!("history bank header checksum mismatch");
        }
        let store_id = Uuid::from_slice(&bytes[16..32]).context("parse history store id")?;
        let session_id = Uuid::from_slice(&bytes[32..48]).context("parse history session id")?;
        let bank_generation = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
        let data_slot = bytes[56];
        if bytes[57..64].iter().any(|byte| *byte != 0) {
            bail!("history bank reserved header bytes are nonzero");
        }
        let capacity = u64::from_le_bytes(bytes[64..72].try_into().unwrap());
        Ok(Self {
            store_id,
            session_id,
            bank_generation,
            data_slot,
            capacity,
        })
    }
}

struct RecoveredHistory {
    commit: HistoryCommit,
    tail: Vec<u8>,
    data_hasher: Sha256,
}

fn read_history_commit(path: &Path, slot: u8) -> Result<Option<HistoryCommit>> {
    let commit_path = history_commit_path(path, slot);
    let Some(file) = open_optional_history_file(&commit_path, "history commit", false)? else {
        return Ok(None);
    };
    let length = file.metadata()?.len();
    if length > HISTORY_COMMIT_MAX_BYTES as u64 {
        bail!(
            "history commit {} exceeds the {}-byte cap",
            commit_path.display(),
            HISTORY_COMMIT_MAX_BYTES
        );
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(HISTORY_COMMIT_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read history commit {}", commit_path.display()))?;
    if bytes.len() > HISTORY_COMMIT_MAX_BYTES {
        bail!("history commit {} exceeds its cap", commit_path.display());
    }
    let commit: HistoryCommit = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse history commit {}", commit_path.display()))?;
    commit
        .validate(path, slot)
        .with_context(|| format!("validate history commit {}", commit_path.display()))?;
    Ok(Some(commit))
}

fn read_history_marker(path: &Path) -> Result<Option<HistoryMarker>> {
    let marker_path = history_marker_path(path);
    let Some(file) = open_optional_history_file(&marker_path, "history marker", false)? else {
        return Ok(None);
    };
    let length = file.metadata()?.len();
    if length > HISTORY_MARKER_MAX_BYTES as u64 {
        bail!(
            "history marker {} exceeds the {}-byte cap",
            marker_path.display(),
            HISTORY_MARKER_MAX_BYTES
        );
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(HISTORY_MARKER_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read history marker {}", marker_path.display()))?;
    if bytes.len() > HISTORY_MARKER_MAX_BYTES {
        bail!("history marker {} exceeds its cap", marker_path.display());
    }
    let marker: HistoryMarker = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse history marker {}", marker_path.display()))?;
    marker
        .validate(path)
        .with_context(|| format!("validate history marker {}", marker_path.display()))?;
    Ok(Some(marker))
}

fn publish_history_marker(path: &Path, commit: &HistoryCommit) -> Result<()> {
    if let Some(marker) = read_history_marker(path)? {
        if marker.store_id != commit.store_id || marker.session_id != commit.session_id {
            bail!("history marker does not match the committed history store");
        }
        return Ok(());
    }
    let marker = HistoryMarker {
        format_version: HISTORY_FORMAT_VERSION,
        store_id: commit.store_id,
        session_id: commit.session_id,
        metadata_sha256: String::new(),
    }
    .seal()?;
    let serialized = serde_json::to_vec(&marker)?;
    if serialized.len() > HISTORY_MARKER_MAX_BYTES {
        bail!("history marker exceeds its bounded metadata size");
    }
    let marker_path = history_marker_path(path);
    atomic_write_json(&marker_path, &marker)
        .with_context(|| format!("publish history marker {}", marker_path.display()))
}

fn recover_history_candidate(
    path: &Path,
    commit: HistoryCommit,
    tail_limit: usize,
) -> Result<RecoveredHistory> {
    let data_path = history_data_path(path, commit.data_slot);
    let mut file = open_optional_history_file(&data_path, "history data bank", false)?
        .ok_or_else(|| anyhow!("history data bank is missing: {}", data_path.display()))?;
    let physical_len = file.metadata()?.len();
    let max_payload = commit
        .capacity
        .checked_mul(2)
        .ok_or_else(|| anyhow!("history bank size overflow"))?;
    let max_physical = (HISTORY_BANK_HEADER_BYTES as u64)
        .checked_add(max_payload)
        .ok_or_else(|| anyhow!("history physical size overflow"))?;
    let committed_physical = (HISTORY_BANK_HEADER_BYTES as u64)
        .checked_add(commit.committed_len)
        .ok_or_else(|| anyhow!("history committed size overflow"))?;
    if physical_len < committed_physical {
        bail!("history data bank is shorter than its committed prefix");
    }
    if physical_len > max_physical {
        bail!("history data bank exceeds its bounded physical size");
    }
    let mut header_bytes = vec![0; HISTORY_BANK_HEADER_BYTES];
    file.read_exact(&mut header_bytes)
        .context("read history bank header")?;
    let header = HistoryBankHeader::decode(&header_bytes)?;
    if header.store_id != commit.store_id
        || header.session_id != commit.session_id
        || header.bank_generation != commit.bank_generation
        || header.data_slot != commit.data_slot
        || header.capacity != commit.capacity
    {
        bail!("history data bank does not match its commit metadata");
    }
    let committed_len = usize::try_from(commit.committed_len)
        .context("history committed length does not fit memory")?;
    let mut payload = Vec::with_capacity(committed_len);
    file.take(commit.committed_len)
        .read_to_end(&mut payload)
        .context("read committed history payload")?;
    if payload.len() != committed_len {
        bail!("history data bank ended inside its committed prefix");
    }
    let mut hasher = Sha256::new();
    hasher.update(&payload);
    if format!("{:x}", hasher.clone().finalize()) != commit.data_sha256 {
        bail!("history data checksum mismatch");
    }
    let count = tail_limit.min(commit.capacity as usize).min(payload.len());
    let tail = payload[payload.len() - count..].to_vec();
    Ok(RecoveredHistory {
        commit,
        tail,
        data_hasher: hasher,
    })
}

fn recover_v2_history(path: &Path, tail_limit: usize) -> Result<(Option<RecoveredHistory>, bool)> {
    validate_history_artifacts(path)?;
    let marker = read_history_marker(path)?;
    let mut metadata_seen = false;
    let mut valid = Vec::new();
    let mut failures = Vec::new();
    for slot in 0..HISTORY_COMMIT_COUNT {
        match read_history_commit(path, slot) {
            Ok(Some(commit)) => {
                metadata_seen = true;
                if marker.as_ref().is_some_and(|marker| {
                    marker.store_id != commit.store_id || marker.session_id != commit.session_id
                }) {
                    failures.push(format!(
                        "commit slot {slot}: history commit does not match the history marker"
                    ));
                    continue;
                }
                match recover_history_candidate(path, commit, tail_limit) {
                    Ok(candidate) => valid.push(candidate),
                    Err(error) => failures.push(format!("commit slot {slot}: {error:#}")),
                }
            }
            Ok(None) => {}
            Err(error) => {
                metadata_seen = true;
                failures.push(format!("commit slot {slot}: {error:#}"));
            }
        }
    }
    if valid.is_empty() {
        if metadata_seen || marker.is_some() {
            bail!(
                "no valid committed history generation remains: {}",
                failures.join("; ")
            );
        }
        return Ok((None, false));
    }
    if marker.is_none()
        && valid
            .iter()
            .any(|candidate| candidate.commit.store_id != valid[0].commit.store_id)
    {
        bail!("valid history commits belong to different stores");
    }
    valid.sort_by_key(|candidate| candidate.commit.commit_generation);
    if valid.len() >= 2 {
        let newest = &valid[valid.len() - 1];
        let previous = &valid[valid.len() - 2];
        if newest.commit.commit_generation == previous.commit.commit_generation
            && newest.commit != previous.commit
        {
            bail!("conflicting history commits have the same generation");
        }
    }
    Ok((valid.pop(), marker.is_some()))
}

struct LegacyHistory {
    tail: Vec<u8>,
    total_len: u64,
    present: bool,
}

fn read_legacy_history_tail(path: &Path, limit: usize) -> Result<LegacyHistory> {
    let Some(mut file) = open_optional_history_file(path, "legacy history", false)? else {
        return Ok(LegacyHistory {
            tail: Vec::new(),
            total_len: 0,
            present: false,
        });
    };
    let total_len = file.metadata()?.len();
    let count = total_len.min(limit as u64);
    file.seek(SeekFrom::Start(total_len - count))
        .with_context(|| format!("seek legacy history {}", path.display()))?;
    let mut tail = Vec::with_capacity(count as usize);
    file.take(count)
        .read_to_end(&mut tail)
        .with_context(|| format!("read legacy history tail {}", path.display()))?;
    Ok(LegacyHistory {
        tail,
        total_len,
        present: true,
    })
}

/// Read a byte-exact, frame-bounded persisted tail from either the v2
/// generation store or a legacy raw `history.bin`. Once the v2 presence
/// marker is published, corruption fails closed instead of falling back to
/// potentially stale raw bytes.
pub fn read_persisted_history_tail(path: &Path, requested: Option<usize>) -> Result<Vec<u8>> {
    let limit = requested.unwrap_or(MAX_FRAME_BYTES).min(MAX_FRAME_BYTES);
    if let (Some(recovered), _) = recover_v2_history(path, limit)? {
        return Ok(recovered.tail);
    }
    Ok(read_legacy_history_tail(path, limit)?.tail)
}

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
    pending: VecDeque<u8>,
    dirty: bool,
    observed_end: u64,
    durable_end: u64,
    compatibility_end: u64,
    compatibility_len: u64,
    compatibility_known: bool,
    persisted: Option<RecoveredHistory>,
    #[cfg(test)]
    data_bytes_written: u64,
    #[cfg(test)]
    append_failure: Option<i32>,
}
impl History {
    pub fn open(path: PathBuf, cap: usize) -> Result<Self> {
        validate_history_bytes(cap)?;
        let (recovered, marker_present) = recover_v2_history(&path, cap)?;
        let had_v2 = recovered.is_some();
        let legacy = if recovered.is_none() {
            Some(read_legacy_history_tail(&path, cap)?)
        } else {
            None
        };
        let (bytes, observed_end, persisted, legacy_present, legacy_total_len) =
            if let Some(recovered) = recovered {
                let stream_end = recovered.commit.stream_end;
                (
                    recovered.tail.iter().copied().collect(),
                    stream_end,
                    Some(recovered),
                    false,
                    0,
                )
            } else {
                let legacy = legacy.expect("legacy state is loaded without v2 metadata");
                (
                    legacy.tail.iter().copied().collect(),
                    legacy.total_len,
                    None,
                    legacy.present,
                    legacy.total_len,
                )
            };
        let mut history = Self {
            path,
            cap,
            bytes,
            pending: VecDeque::new(),
            dirty: false,
            observed_end,
            durable_end: observed_end,
            compatibility_end: if legacy_present { observed_end } else { 0 },
            compatibility_len: if legacy_present { legacy_total_len } else { 0 },
            compatibility_known: !had_v2,
            persisted,
            #[cfg(test)]
            data_bytes_written: 0,
            #[cfg(test)]
            append_failure: None,
        };
        if had_v2 && !marker_present {
            let commit = &history
                .persisted
                .as_ref()
                .expect("v2 recovery has a committed generation")
                .commit;
            publish_history_marker(&history.path, commit)?;
        }
        let needs_capacity_migration = history
            .persisted
            .as_ref()
            .is_some_and(|persisted| persisted.commit.capacity != cap as u64);
        if legacy_present && legacy_total_len > cap as u64 {
            history.repair_legacy_compatibility()?;
        }
        if legacy_present || needs_capacity_migration {
            history.publish_compaction()?;
        }
        if had_v2 {
            // V2 is authoritative after a crash or partial compatibility
            // write. Rebuild the raw view once at worker restart so an old
            // client cannot observe a duplicated or stale suffix.
            history.repair_legacy_compatibility()?;
        }
        Ok(history)
    }
    #[cfg(test)]
    pub(crate) fn inject_append_failure(&mut self, errno: i32) {
        self.append_failure = Some(errno);
    }
    /// Appends only to the in-memory ring. Persisting from this hot path can
    /// both throttle PTY output and turn a disk failure into a PTY failure,
    /// so the worker owns periodic and final `flush()` attempts separately.
    pub fn append(&mut self, data: &[u8]) -> Result<()> {
        #[cfg(test)]
        if let Some(errno) = self.append_failure {
            return Err(io::Error::from_raw_os_error(errno)).context("append history");
        }
        let added = u64::try_from(data.len()).context("history append length does not fit u64")?;
        let next_end = self
            .observed_end
            .checked_add(added)
            .ok_or_else(|| anyhow!("history stream position overflow"))?;
        if self.cap > 0 {
            if data.len() >= self.cap {
                self.bytes.clear();
                self.bytes
                    .extend(data[data.len() - self.cap..].iter().copied());
                self.pending.clear();
                self.pending
                    .extend(data[data.len() - self.cap..].iter().copied());
            } else {
                self.bytes.extend(data.iter().copied());
                self.pending.extend(data.iter().copied());
                while self.bytes.len() > self.cap {
                    self.bytes.pop_front();
                }
                while self.pending.len() > self.cap {
                    self.pending.pop_front();
                }
            }
        }
        self.observed_end = next_end;
        self.dirty = true;
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        // Publish and sync the old-client raw view before committing v2. If
        // this fails, v2 remains at the prior generation and dirty state is
        // retained for retry. If v2 then fails, compatibility_end prevents a
        // retry from appending the same bytes twice.
        self.flush_legacy_compatibility()?;
        let Some(persisted) = self.persisted.as_ref() else {
            return self.publish_compaction();
        };
        let pending_span = self
            .observed_end
            .checked_sub(self.durable_end)
            .ok_or_else(|| anyhow!("history durable position exceeds observed position"))?;
        let pending_is_contiguous = self.cap == 0 || pending_span == self.pending.len() as u64;
        let resulting_len = persisted
            .commit
            .committed_len
            .checked_add(self.pending.len() as u64)
            .ok_or_else(|| anyhow!("history committed length overflow"))?;
        if !pending_is_contiguous || resulting_len > (self.cap as u64).saturating_mul(2) {
            return self.publish_compaction();
        }
        self.publish_append()
    }

    /// Finalize v2 first, then publish one raw tail snapshot for older clients
    /// that know only `history.bin`. This full-tail write happens once at a
    /// clean terminal transition, never on the 500 ms periodic path.
    pub fn flush_final(&mut self) -> Result<()> {
        self.flush()?;
        self.repair_legacy_compatibility()
    }

    fn repair_legacy_compatibility(&mut self) -> Result<()> {
        let present = validate_optional_history_node(&self.path, "legacy history")?;
        if self.cap == 0 {
            if present {
                fs::remove_file(&self.path).with_context(|| {
                    format!("remove disabled legacy history {}", self.path.display())
                })?;
                if let Some(parent) = self.path.parent() {
                    File::open(parent)?.sync_all()?;
                }
            }
            self.compatibility_end = self.observed_end;
            self.compatibility_len = 0;
            self.compatibility_known = true;
            return Ok(());
        }
        let contiguous: Vec<u8> = self.bytes.iter().copied().collect();
        let result = atomic_write_bytes(&self.path, &contiguous)
            .with_context(|| format!("publish legacy history tail {}", self.path.display()));
        match result {
            Ok(()) => {
                self.compatibility_end = self.observed_end;
                self.compatibility_len = contiguous.len() as u64;
                self.compatibility_known = true;
                Ok(())
            }
            Err(error) => {
                self.compatibility_known = false;
                Err(error)
            }
        }
    }

    fn flush_legacy_compatibility(&mut self) -> Result<()> {
        let delta = self
            .observed_end
            .checked_sub(self.compatibility_end)
            .ok_or_else(|| anyhow!("legacy history position exceeds observed history"))?;
        if delta == 0 && self.compatibility_known {
            return Ok(());
        }
        if self.cap == 0 {
            return self.repair_legacy_compatibility();
        }
        let delta_len = usize::try_from(delta).unwrap_or(usize::MAX);
        let resulting_len = self.compatibility_len.checked_add(delta);
        if !self.compatibility_known
            || delta_len > self.bytes.len()
            || resulting_len.is_none_or(|length| length > (self.cap as u64).saturating_mul(2))
            || !validate_optional_history_node(&self.path, "legacy history")?
        {
            return self.repair_legacy_compatibility();
        }
        let mut file = open_optional_history_file(&self.path, "legacy history", true)?
            .ok_or_else(|| anyhow!("legacy history disappeared: {}", self.path.display()))?;
        if file.metadata()?.len() != self.compatibility_len {
            return self.repair_legacy_compatibility();
        }
        let bytes: Vec<u8> = self
            .bytes
            .iter()
            .skip(self.bytes.len() - delta_len)
            .copied()
            .collect();
        file.seek(SeekFrom::End(0))?;
        let result = file
            .write_all(&bytes)
            .and_then(|()| file.sync_data())
            .with_context(|| format!("append legacy history {}", self.path.display()));
        match result {
            Ok(()) => {
                self.compatibility_end = self.observed_end;
                self.compatibility_len += delta;
                Ok(())
            }
            Err(error) => {
                self.compatibility_known = false;
                Err(error)
            }
        }
    }

    fn next_commit_generation(&self) -> Result<u64> {
        self.persisted
            .as_ref()
            .map(|persisted| {
                persisted
                    .commit
                    .commit_generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("history commit generation overflow"))
            })
            .unwrap_or(Ok(1))
    }

    fn publish_commit(&self, commit: HistoryCommit) -> Result<HistoryCommit> {
        let commit = commit.seal()?;
        let slot = (commit.commit_generation % HISTORY_COMMIT_COUNT as u64) as u8;
        let path = history_commit_path(&self.path, slot);
        validate_optional_history_node(&path, "history commit")?;
        let serialized = serde_json::to_vec(&commit)?;
        if serialized.len() > HISTORY_COMMIT_MAX_BYTES {
            bail!("history commit exceeds its bounded metadata size");
        }
        atomic_write_json(&path, &commit)
            .with_context(|| format!("publish history commit {}", path.display()))?;
        Ok(commit)
    }

    fn publish_append(&mut self) -> Result<()> {
        let persisted = self
            .persisted
            .as_ref()
            .ok_or_else(|| anyhow!("history append has no committed bank"))?;
        let data_path = history_data_path(&self.path, persisted.commit.data_slot);
        let mut file = open_optional_history_file(&data_path, "history data bank", true)?
            .ok_or_else(|| anyhow!("history data bank disappeared: {}", data_path.display()))?;
        let committed_physical = (HISTORY_BANK_HEADER_BYTES as u64)
            .checked_add(persisted.commit.committed_len)
            .ok_or_else(|| anyhow!("history append offset overflow"))?;
        let physical_len = file.metadata()?.len();
        if physical_len < committed_physical {
            bail!("history data bank is shorter than its committed prefix");
        }
        let max_physical = (HISTORY_BANK_HEADER_BYTES as u64)
            .checked_add((self.cap as u64).saturating_mul(2))
            .ok_or_else(|| anyhow!("history bank bound overflow"))?;
        if physical_len > max_physical {
            bail!("history data bank exceeds its bounded physical size");
        }
        if physical_len != committed_physical {
            file.set_len(committed_physical)
                .context("discard uncommitted history suffix")?;
        }
        file.seek(SeekFrom::Start(committed_physical))?;
        let pending: Vec<u8> = self.pending.iter().copied().collect();
        file.write_all(&pending).context("append history payload")?;
        file.sync_data().context("sync appended history payload")?;

        let mut hasher = persisted.data_hasher.clone();
        hasher.update(&pending);
        let committed_len = persisted
            .commit
            .committed_len
            .checked_add(pending.len() as u64)
            .ok_or_else(|| anyhow!("history committed length overflow"))?;
        let commit = HistoryCommit {
            format_version: HISTORY_FORMAT_VERSION,
            store_id: persisted.commit.store_id,
            session_id: persisted.commit.session_id,
            commit_generation: self.next_commit_generation()?,
            bank_generation: persisted.commit.bank_generation,
            data_slot: persisted.commit.data_slot,
            capacity: self.cap as u64,
            committed_len,
            stream_end: self.observed_end,
            data_sha256: format!("{:x}", hasher.clone().finalize()),
            metadata_sha256: String::new(),
        };
        let commit = self.publish_commit(commit)?;
        publish_history_marker(&self.path, &commit)?;
        #[cfg(test)]
        {
            self.data_bytes_written = self.data_bytes_written.saturating_add(pending.len() as u64);
        }
        self.persisted = Some(RecoveredHistory {
            commit,
            tail: Vec::new(),
            data_hasher: hasher,
        });
        self.durable_end = self.observed_end;
        self.pending.clear();
        self.dirty = false;
        Ok(())
    }

    fn publish_compaction(&mut self) -> Result<()> {
        let store_id = self
            .persisted
            .as_ref()
            .map(|persisted| persisted.commit.store_id)
            .unwrap_or_else(Uuid::new_v4);
        let bank_generation = self
            .persisted
            .as_ref()
            .map(|persisted| {
                persisted
                    .commit
                    .bank_generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("history bank generation overflow"))
            })
            .unwrap_or(Ok(1))?;
        let data_slot = self
            .persisted
            .as_ref()
            .map(|persisted| (persisted.commit.data_slot + 1) % HISTORY_BANK_COUNT)
            .unwrap_or(0);
        let header = HistoryBankHeader {
            store_id,
            session_id: history_session_id(&self.path),
            bank_generation,
            data_slot,
            capacity: self.cap as u64,
        };
        let snapshot: Vec<u8> = self.bytes.iter().copied().collect();
        let mut bank = header.encode();
        bank.extend_from_slice(&snapshot);
        let data_path = history_data_path(&self.path, data_slot);
        validate_optional_history_node(&data_path, "history data bank")?;
        atomic_write_bytes(&data_path, &bank)
            .with_context(|| format!("publish history data bank {}", data_path.display()))?;
        let mut hasher = Sha256::new();
        hasher.update(&snapshot);
        let commit = HistoryCommit {
            format_version: HISTORY_FORMAT_VERSION,
            store_id,
            session_id: header.session_id,
            commit_generation: self.next_commit_generation()?,
            bank_generation,
            data_slot,
            capacity: self.cap as u64,
            committed_len: snapshot.len() as u64,
            stream_end: self.observed_end,
            data_sha256: format!("{:x}", hasher.clone().finalize()),
            metadata_sha256: String::new(),
        };
        let commit = self.publish_commit(commit)?;
        publish_history_marker(&self.path, &commit)?;
        #[cfg(test)]
        {
            self.data_bytes_written = self
                .data_bytes_written
                .saturating_add(snapshot.len() as u64);
        }
        self.persisted = Some(RecoveredHistory {
            commit,
            tail: Vec::new(),
            data_hasher: hasher,
        });
        self.durable_end = self.observed_end;
        self.pending.clear();
        self.dirty = false;
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

const CGROUP_V2_ROOT: &str = "/sys/fs/cgroup";
const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;
const TRUSTED_HELPER_DIRS: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/local/bin",
    "/run/current-system/sw/bin",
];

fn namespace_coordinates(path: &Path, label: &str) -> Result<(u64, u64)> {
    let metadata = fs::metadata(path).with_context(|| format!("inspect {label} namespace"))?;
    Ok((metadata.dev(), metadata.ino()))
}

fn ensure_cgroup2_filesystem(path: &Path) -> Result<()> {
    let encoded = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("encode cgroup path {}", path.display()))?;
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(encoded.as_ptr(), &mut stats) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("inspect filesystem for {}", path.display()));
    }
    if stats.f_type != CGROUP2_SUPER_MAGIC {
        bail!("{} is not on a cgroup-v2 filesystem", path.display());
    }
    Ok(())
}

fn mount_id_for_file(file: &File) -> Result<u64> {
    let path = format!("/proc/self/fdinfo/{}", file.as_raw_fd());
    let info = fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .map(str::trim)
        .ok_or_else(|| anyhow!("{path} has no mount identity"))?
        .parse()
        .with_context(|| format!("parse mount identity from {path}"))
}

/// Capture the kernel domain that gives a persisted cgroup locator meaning.
/// The root checks also keep resource-limit setup from accepting a lookalike
/// directory mounted at `/sys/fs/cgroup`.
pub fn current_cgroup_identity() -> Result<CgroupIdentity> {
    let root = Path::new(CGROUP_V2_ROOT);
    let root_handle = File::open(root).context("open cgroup-v2 root")?;
    let root_metadata = root_handle.metadata().context("inspect cgroup-v2 root")?;
    if !root_metadata.is_dir() {
        bail!("{CGROUP_V2_ROOT} is not a directory");
    }
    ensure_cgroup2_filesystem(root)?;
    let controllers = root.join("cgroup.controllers");
    if !fs::metadata(&controllers)
        .with_context(|| format!("inspect {}", controllers.display()))?
        .is_file()
    {
        bail!(
            "{} is not a cgroup-v2 controllers file",
            controllers.display()
        );
    }
    let (cgroup_namespace_device, cgroup_namespace_inode) =
        namespace_coordinates(Path::new("/proc/self/ns/cgroup"), "cgroup")?;
    let (mount_namespace_device, mount_namespace_inode) =
        namespace_coordinates(Path::new("/proc/self/ns/mnt"), "mount")?;
    Ok(CgroupIdentity {
        boot_id: linux_boot_id()?,
        cgroup_namespace_device,
        cgroup_namespace_inode,
        mount_namespace_device,
        mount_namespace_inode,
        cgroup_mount_id: mount_id_for_file(&root_handle)?,
        cgroup_root_device: root_metadata.dev(),
        cgroup_root_inode: root_metadata.ino(),
    })
}

fn verify_recorded_cgroup_identity(recorded: Option<&CgroupIdentity>) -> Result<CgroupIdentity> {
    let recorded = recorded.ok_or_else(|| {
        anyhow!(
            "recorded cgroup has no boot/namespace/mount identity; refusing legacy destructive recovery"
        )
    })?;
    let current = current_cgroup_identity()?;
    if recorded != &current {
        bail!(
            "recorded cgroup identity does not match the current boot, cgroup namespace, mount namespace, or cgroup-v2 root"
        );
    }
    Ok(current)
}

fn validate_trusted_helper(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("helper path is not absolute: {}", path.display());
    }
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("resolve helper executable {}", path.display()))?;
    let metadata = fs::metadata(&canonical)
        .with_context(|| format!("inspect helper executable {}", canonical.display()))?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
    {
        bail!(
            "untrusted helper executable {} (must be root-owned, executable, and not group/world writable)",
            canonical.display()
        );
    }
    let mut parent = canonical.parent();
    while let Some(directory) = parent {
        let metadata = fs::metadata(directory)
            .with_context(|| format!("inspect helper directory {}", directory.display()))?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!(
                "untrusted helper directory {} (must be root-owned and not group/world writable)",
                directory.display()
            );
        }
        parent = directory.parent();
    }
    Ok(canonical)
}

fn trusted_system_helper(name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains('/') {
        bail!("invalid system helper name {name:?}");
    }
    let mut failures = Vec::new();
    for directory in TRUSTED_HELPER_DIRS {
        let candidate = Path::new(directory).join(name);
        match validate_trusted_helper(&candidate) {
            Ok(path) => return Ok(path),
            Err(error) => failures.push(format!("{}: {error:#}", candidate.display())),
        }
    }
    bail!(
        "no trusted absolute {name} helper was found; {}",
        failures.join("; ")
    )
}

/// End-to-end probe of the system-manager scope backend behind the
/// `APLEXER_LAUNCH_SYSTEM_SCOPE=system` escape (issue #1): resolves the same
/// trusted helpers the real launch path resolves, then creates and collects
/// one trivial transient scope (`-- true`) on the system manager. This is
/// the only way to know the backend actually works -- as a regular user it
/// usually does not (`org.freedesktop.systemd1.manage-units` needs root or
/// a polkit authorization), and guessing would turn the opt-in escape into
/// a broken start. Probing creates no lasting state: the scope runs `true`,
/// exits, and `--collect` garbage-collects it. Never called unless the env
/// opt-in is set.
pub fn probe_system_scope_backend() -> Result<()> {
    let systemd_run = trusted_system_helper("systemd-run")?;
    let true_binary = trusted_system_helper("true")?;
    let unit = format!("aplexer-escape-probe-{}", Uuid::new_v4().simple());
    let mut command = Command::new(systemd_run);
    command
        .args([
            "--system",
            "--scope",
            "--collect",
            "--quiet",
            &format!("--unit={unit}"),
            "--",
        ])
        .arg(&true_binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut probe = command
        .spawn()
        .with_context(|| format!("spawn system-scope probe from {}", true_binary.display()))?;
    // Same discipline as every other helper child: register the pid so the
    // worker's descendant reaper cannot consume its status, and always reap
    // it here before dropping.
    let probe_pid = probe.id();
    crate::worker::own_child_pid(probe_pid);
    let probe_result = (|| -> Result<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match probe.try_wait()? {
                Some(status) => return Ok(status),
                None if Instant::now() >= deadline => {
                    let _ = probe.kill();
                    let _ = probe.wait();
                    bail!("systemd-run system-scope probe timed out after 10s");
                }
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
    })();
    crate::worker::disown_child_pid(probe_pid);
    let status = probe_result?;
    if !status.success() {
        bail!(
            "systemd-run --system --scope probe exited with {status}; creating system \
             manager scopes needs root or polkit authorization \
             (org.freedesktop.systemd1.manage-units)"
        );
    }
    Ok(())
}

/// Decide, once per launch, whether the opt-in escape backend is usable.
/// `Ok(true)`/`Ok(false)` mean "system scope"/"user manager fallback" for
/// the caller's placement decision; the error is the probe failure, for the
/// caller to report honestly instead of silently pretending the escape
/// happened (issue #1: warn or fail clearly).
pub fn system_scope_escape_decision() -> Result<bool> {
    if !crate::placement::system_scope_requested() {
        return Ok(false);
    }
    match probe_system_scope_backend() {
        Ok(()) => Ok(true),
        Err(error) => Err(error),
    }
}

/// Rewrite `worker` (already carrying the worker program and its initial
/// argv from `worker_command`) so spawning it creates the worker inside a
/// system-manager scope (`systemd-run --system --scope --collect
/// --unit=aplexer-worker-<id>`) instead of bare `setsid()` in the ambient
/// cgroup (issue #1). Everything configured on the command afterwards --
/// per-session env, `--rows/--cols` -- lands on the systemd-run wrapper and
/// is passed through to the worker child: systemd-run shares its own
/// environment with the scope's process, and argv after `--` is the
/// worker's argv verbatim.
///
/// The `pre_exec` closure the caller installs afterwards (setsid + signal
/// blocking) then applies to systemd-run itself; the worker inherits the
/// blocked-signal baseline it unblocks at startup and needs no session of
/// its own (the workload's spawn does its own `setsid` + `TIOCSCTTY`).
/// The wrapper stays the worker's parent for the worker's whole life --
/// one extra small process per escaped session -- and `--collect` removes
/// the scope as soon as the worker exits.
///
/// `Ok(())` means the command now spawns into the escape scope; the error
/// is the reason the escape was not applied, for the caller to surface.
/// The caller falls back to the plain (setsid-only, ambient-cgroup) spawn
/// in that case: the issue asks for honest degradation with a warning,
/// never for a broken start.
fn wrap_worker_in_system_scope(id: Uuid, worker: &mut Command) -> Result<()> {
    let systemd_run = trusted_system_helper("systemd-run")?;
    let program = worker.get_program().to_os_string();
    let worker_args: Vec<OsString> = worker.get_args().map(|arg| arg.to_os_string()).collect();
    let mut command = Command::new(systemd_run);
    command.args([
        "--system",
        "--scope",
        "--collect",
        "--quiet",
        &format!("--unit=aplexer-worker-{id}"),
        "--",
    ]);
    command.arg(&program);
    command.args(&worker_args);
    *worker = command;
    Ok(())
}

fn control_group_locator(id: Uuid, value: &str) -> Result<PathBuf> {
    let value = value.trim();
    let reported = Path::new(value);
    if value.is_empty() || value == "/" || !reported.is_absolute() {
        bail!("systemd returned invalid ControlGroup value {value:?}");
    }
    let mut relative = PathBuf::new();
    for component in reported.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => relative.push(value),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                bail!("systemd ControlGroup escapes cgroup root: {value:?}")
            }
        }
    }
    let expected = format!("aplexer-workload-{id}.scope");
    if relative.file_name() != Some(OsStr::new(&expected)) {
        bail!("systemd ControlGroup does not belong to session {id}: {value:?}");
    }
    Ok(Path::new(CGROUP_V2_ROOT).join(relative))
}

#[derive(Debug, Clone)]
pub struct Cgroup {
    path: PathBuf,
    identity: CgroupIdentity,
    /// Keep exclusive ownership of the unreaped child until release. An
    /// unreaped child reserves its pid, so Child::kill cannot be redirected
    /// to a recycled process; clones serialize the single kill+wait through
    /// this shared slot.
    anchor: Arc<Mutex<Option<std::process::Child>>>,
    initial_oom_kill: u64,
}

fn release_anchor_child(anchor: &mut std::process::Child) -> Result<()> {
    let pid = anchor.id();
    match anchor.kill() {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
        Err(error) => return Err(error).context("kill systemd-run anchor"),
    }
    let waited = anchor.wait().context("reap systemd-run anchor");
    // Released only after the wait has returned, so the worker's descendant
    // reaper can never consume this status first.
    crate::worker::disown_child_pid(pid);
    waited?;
    Ok(())
}

fn release_anchor_slot<T>(
    slot: &mut Option<T>,
    release: impl FnOnce(&mut T) -> Result<()>,
) -> Result<()> {
    if let Some(anchor) = slot.as_mut() {
        release(anchor)?;
        *slot = None;
    }
    Ok(())
}

fn cleanup_anchor_after_failure(
    anchor: &mut std::process::Child,
    error: anyhow::Error,
) -> anyhow::Error {
    match release_anchor_child(anchor) {
        Ok(()) => error,
        Err(cleanup_error) => {
            anyhow!("{error:#}; systemd-run anchor cleanup failed: {cleanup_error:#}")
        }
    }
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
    // scope (a sibling, not a nested child, of the ambient cgroup) and hold
    // it open with a placeholder process until the real workload can be
    // moved in.
    //
    // Which manager owns the new scope is a placement decision with a real
    // failure-domain consequence (issue #1): the default `--user` scope
    // lives beneath user@UID.service and dies with the per-user manager's
    // exit.target; the opt-in `--system` scope (APLEXER_LAUNCH_SYSTEM_SCOPE
    // = system, probed first via `system_scope_escape_decision`) lives under
    // the system manager and survives it. Probe failure downgrades to the
    // user manager with a printed warning -- limits still apply either way;
    // only the survival domain differs. A failure *after* a successful probe
    // (spawn, scope wait, controller delegation) fails closed exactly as the
    // `--user` path always has: a validated backend that then breaks is a
    // real error, not a placement preference to silently swap.
    pub fn create<F>(id: Uuid, limits: &Limits, setup_started: F) -> Result<Option<Self>>
    where
        F: FnOnce(),
    {
        ensure_sigchld_compatible_for_child_management()?;
        if !limits.requested() {
            return Ok(None);
        }
        let system_scope = match system_scope_escape_decision() {
            Ok(system_scope) => system_scope,
            Err(error) => {
                eprintln!(
                    "warning: APLEXER_LAUNCH_SYSTEM_SCOPE=system requested, but the \
                     system-scope backend is unavailable ({error:#}); the workload scope \
                     falls back to the per-user manager and inherits its exit.target \
                     failure domain"
                );
                false
            }
        };
        let bus_flag = if system_scope { "--system" } else { "--user" };
        let identity = current_cgroup_identity()?;
        // Resolve every executable before starting the scope. Ambient PATH is
        // intentionally irrelevant: a user-controlled shadow helper must not
        // choose or fabricate the containment domain we later trust.
        let systemd_run = trusted_system_helper("systemd-run")?;
        let systemctl = trusted_system_helper("systemctl")?;
        let sleep = trusted_system_helper("sleep")?;
        let unit = format!("aplexer-workload-{id}");
        let mut command = Command::new(systemd_run);
        command
            .arg(bus_flag)
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
            .arg(sleep)
            .arg("infinity")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut anchor = command
            .spawn()
            .context("spawn systemd-run anchor; limits fail closed")?;
        // The worker waits on this pid itself (`release_anchor_child`), so
        // register it before anything else in the process can observe it as
        // a child. See `worker::OWNED_CHILD_PIDS`.
        crate::worker::own_child_pid(anchor.id());
        // From this point, systemd may own a scope member outside the worker's
        // procfs descendant tree. Let the caller preserve recovery evidence
        // until an authoritative cgroup path has been recorded.
        setup_started();
        let path = match wait_for_scope_cgroup(
            id,
            &unit,
            &identity,
            &systemctl,
            bus_flag,
            Duration::from_secs(5),
        ) {
            Ok(path) => path,
            Err(error) => {
                return Err(cleanup_anchor_after_failure(
                    &mut anchor,
                    error.context("limits fail closed"),
                ));
            }
        };
        if limits.memory_bytes.is_some() && !path.join("memory.max").exists() {
            return Err(cleanup_anchor_after_failure(
                &mut anchor,
                anyhow!("systemd did not delegate the memory controller; limits fail closed"),
            ));
        }
        if limits.pids.is_some() && !path.join("pids.max").exists() {
            return Err(cleanup_anchor_after_failure(
                &mut anchor,
                anyhow!("systemd did not delegate the pids controller; limits fail closed"),
            ));
        }
        let initial_oom_kill = read_counter(&path.join("memory.events"), "oom_kill").unwrap_or(0);
        Ok(Some(Self {
            path,
            identity,
            anchor: Arc::new(Mutex::new(Some(anchor))),
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
    /// The same cgroup in `/proc/<pid>/cgroup` form (`/<relative>` under the
    /// cgroup-v2 root), so launch-time validation can compare what systemd
    /// was asked to create against what the workload actually reports being
    /// in (issue #1).
    pub fn proc_path(&self) -> String {
        let relative = self
            .path
            .strip_prefix(CGROUP_V2_ROOT)
            .unwrap_or(&self.path)
            .to_string_lossy()
            .to_string();
        format!("/{}", relative.trim_start_matches('/'))
    }
    pub fn identity(&self) -> &CgroupIdentity {
        &self.identity
    }
    /// Kills the placeholder process that was keeping the delegated scope
    /// alive. Call this only after the real workload pid has been added to
    /// the cgroup, so the cgroup never goes empty (and gets garbage
    /// collected by systemd) before the real workload takes residence.
    pub fn release_anchor(&self) -> Result<()> {
        let mut slot = self
            .anchor
            .lock()
            .map_err(|_| anyhow!("systemd-run anchor lock poisoned"))?;
        release_anchor_slot(&mut slot, release_anchor_child)
    }
    pub fn signal_all_until(&self, signal: i32, deadline: Instant) -> Result<()> {
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        verify_recorded_cgroup_identity(Some(&self.identity))?;
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        signal_cgroup_path_until(&self.path, signal, deadline)
    }
    pub fn kill_all_until(&self, deadline: Instant) -> Result<()> {
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        verify_recorded_cgroup_identity(Some(&self.identity))?;
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        kill_cgroup_path_until(&self.path, deadline)
    }
    pub fn populated(&self) -> Result<bool> {
        live_cgroup_populated_with(&self.identity, || {
            read_counter(&self.path.join("cgroup.events"), "populated")
        })
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
        let _ = self.release_anchor();
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
    identity: Option<&CgroupIdentity>,
    signal: i32,
    grace: Duration,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(grace)
        .and_then(|deadline| deadline.checked_add(Duration::from_secs(2)))
        .ok_or_else(|| anyhow!("cgroup cleanup deadline overflow"))?;
    cleanup_recorded_cgroup_until(id, locator, identity, signal, grace, deadline)
}

/// Preflight a durable locator before destroying a broken session's worker
/// subreaper. This performs no signalling; it only establishes that later
/// cgroup recovery will operate inside the expected kernel domain.
pub fn validate_recorded_cgroup_locator(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
) -> Result<()> {
    validate_recorded_cgroup(id, locator, identity).map(|_| ())
}

/// Deadline-sharing variant for startup rollback, where cgroup recovery must
/// consume the same wall-clock budget as procfs discovery and pidfd cleanup.
pub fn cleanup_recorded_cgroup_until(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
    signal: i32,
    grace: Duration,
    deadline: Instant,
) -> Result<()> {
    check_cgroup_cleanup_deadline(deadline, "validating recorded cgroup")?;
    let Some(path) = validate_recorded_cgroup(id, locator, identity)? else {
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

fn validate_recorded_cgroup(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
) -> Result<Option<PathBuf>> {
    // This comparison intentionally precedes canonicalizing the leaf. A
    // missing leaf proves emptiness only inside the exact kernel domain in
    // which it was durably recorded.
    let current_identity = verify_recorded_cgroup_identity(identity)?;
    let root = Path::new(CGROUP_V2_ROOT);
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
    ensure_cgroup2_filesystem(&canonical)?;
    let metadata = fs::metadata(&canonical)?;
    if metadata.dev() != current_identity.cgroup_root_device {
        bail!(
            "recorded cgroup {} is on a different cgroup-v2 mount",
            canonical.display()
        );
    }
    let procs = canonical.join("cgroup.procs");
    if !fs::metadata(&procs)
        .with_context(|| format!("inspect {}", procs.display()))?
        .is_file()
    {
        bail!("{} is not a cgroup member file", procs.display());
    }
    Ok(Some(canonical))
}

fn cgroup_path_populated(path: &Path) -> Result<bool> {
    match read_counter(&path.join("cgroup.events"), "populated") {
        Ok(value) => Ok(value != 0),
        Err(error) if error_is_not_found(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn error_is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
    })
}

/// Read live membership only while the cgroup pathname still belongs to the
/// exact kernel domain captured at creation. A collected scope loses both its
/// directory and `cgroup.events`; ENOENT therefore means empty, but only when
/// the domain matches both before and after that observation.
fn live_cgroup_populated_with(
    identity: &CgroupIdentity,
    read_populated: impl FnOnce() -> Result<u64>,
) -> Result<bool> {
    verify_recorded_cgroup_identity(Some(identity))
        .context("validate live cgroup identity before reading membership")?;
    let populated = match read_populated() {
        Ok(value) => Some(value != 0),
        Err(error) if error_is_not_found(&error) => None,
        Err(error) => return Err(error).context("read live cgroup membership"),
    };
    verify_recorded_cgroup_identity(Some(identity))
        .context("validate live cgroup identity after reading membership")?;
    Ok(populated.unwrap_or(false))
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
fn wait_for_scope_cgroup(
    id: Uuid,
    unit: &str,
    identity: &CgroupIdentity,
    systemctl: &Path,
    bus_flag: &str,
    timeout: Duration,
) -> Result<PathBuf> {
    wait_for_scope_cgroup_with(id, unit, systemctl, bus_flag, timeout, |path| {
        validate_recorded_cgroup(id, path, Some(identity))
    })
}

fn wait_for_scope_cgroup_with(
    id: Uuid,
    unit: &str,
    systemctl: &Path,
    bus_flag: &str,
    timeout: Duration,
    mut validate: impl FnMut(&Path) -> Result<Option<PathBuf>>,
) -> Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut command = Command::new(systemctl);
        command.args([
            bus_flag,
            "show",
            &format!("{unit}.scope"),
            "-p",
            "ControlGroup",
            "--value",
        ]);
        let output = command_output_until(&mut command, deadline, "query systemd scope")?;
        if output.status.success() {
            let value = std::str::from_utf8(&output.stdout)
                .context("decode systemd ControlGroup output")?;
            // systemd may publish the unit before assigning its ControlGroup.
            // Empty and root are transient "not assigned yet" values; every
            // other malformed, escaping, or wrong-session value is hostile
            // evidence and must fail closed rather than being retried.
            if !matches!(value.trim(), "" | "/") {
                let path = control_group_locator(id, value)?;
                if let Some(path) = validate(&path)? {
                    return Ok(path);
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for systemd scope {unit}.scope to appear");
        }
        thread::sleep(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

/// Reap a startup helper off the caller's critical path. The pid stays
/// registered as worker-owned (see `worker::OWNED_CHILD_PIDS`) until this
/// thread's `wait` returns, so the worker's descendant reaper cannot take
/// the status out from under it and cannot be handed a recycled pid early.
fn reap_helper_child_async(mut child: std::process::Child) {
    let pid = child.id();
    thread::spawn(move || {
        let _ = child.wait();
        crate::worker::disown_child_pid(pid);
    });
}

/// Run a small setup query without allowing a wedged helper to defeat the
/// caller's wall-clock timeout. Stdout is intentionally bounded: systemctl's
/// ControlGroup value is one short path, and anything larger is malformed.
fn command_output_until(
    command: &mut Command,
    deadline: Instant,
    operation: &str,
) -> Result<std::process::Output> {
    if Instant::now() >= deadline {
        bail!("timed out before {operation}");
    }
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn helper to {operation}"))?;
    // This helper's status belongs to this function (or to the detached
    // waiter `reap_helper_child_async` starts), never to the worker's
    // descendant reaper.
    let helper_pid = child.id();
    crate::worker::own_child_pid(helper_pid);
    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("{operation} helper has no stdout"))?;
    let flags = unsafe { libc::fcntl(child_stdout.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe {
            libc::fcntl(
                child_stdout.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        } < 0
    {
        let error = io::Error::last_os_error();
        let _ = child.kill();
        reap_helper_child_async(child);
        return Err(error).with_context(|| format!("make {operation} output nonblocking"));
    }
    let mut stdout = Vec::new();
    let mut stdout_eof = false;
    let mut status = None;
    loop {
        loop {
            let mut buffer = [0_u8; 4096];
            match child_stdout.read(&mut buffer) {
                Ok(0) => {
                    stdout_eof = true;
                    break;
                }
                Ok(count) => {
                    stdout.extend_from_slice(&buffer[..count]);
                    if stdout.len() > 64 * 1024 {
                        let _ = child.kill();
                        reap_helper_child_async(child);
                        bail!("output from {operation} exceeds 64 KiB");
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = child.kill();
                    reap_helper_child_async(child);
                    return Err(error).with_context(|| format!("read output from {operation}"));
                }
            }
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(Some(result)) => status = Some(result),
                Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = child.kill();
                    reap_helper_child_async(child);
                    return Err(error).with_context(|| format!("wait for {operation}"));
                }
            }
        }
        if let (Some(status), true) = (status, stdout_eof) {
            crate::worker::disown_child_pid(helper_pid);
            return Ok(std::process::Output {
                status,
                stdout,
                stderr: Vec::new(),
            });
        }

        if Instant::now() >= deadline {
            if status.is_none() {
                let _ = child.kill();
                // A helper stuck in uninterruptible sleep must not extend the
                // startup deadline. Reap asynchronously once the kernel permits.
                reap_helper_child_async(child);
            } else {
                crate::worker::disown_child_pid(helper_pid);
            }
            bail!("timed out waiting to {operation}");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(Duration::from_millis(20).min(remaining));
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

    fn load_config_text(text: &str) -> Result<Config> {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        fs::write(&paths.config_file, text).unwrap();
        Config::load(&paths)
    }

    fn registry_record(paths: &Paths, id: Uuid) -> SessionRecord {
        SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id,
            workspace: paths.state_root.clone(),
            tag: "registry-test".into(),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/true".into()],
            cwd: paths.state_root.clone(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: DEFAULT_HISTORY_BYTES,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase: Phase::Exited,
            worker_pid: None,
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(true),
            socket_path: paths.socket(id),
            history_path: paths.history(id),
            exit: None,
            error: None,
        }
    }

    #[test]
    fn zcodex_is_a_built_in_codex_variant() {
        let config = load_config_text("").unwrap();
        let zcodex = config
            .engines
            .get("zcodex")
            .expect("built-in zcodex engine");
        assert_eq!(
            zcodex.command,
            vec![
                "zcodex".to_string(),
                "-c".to_string(),
                "check_for_update_on_startup=false".to_string(),
            ]
        );
        assert_eq!(
            zcodex.skip_permissions_argv,
            vec!["--dangerously-bypass-approvals-and-sandbox".to_string()]
        );
        assert_eq!(engine_family("zcodex"), "codex");
        assert_eq!(engine_family("codex"), "codex");
        assert_eq!(engine_family("claude"), "claude");
    }

    /// `config_keep_exited` is a second reader of the same setting, chosen
    /// so the worker's exit path does not depend on the whole config file
    /// validating. It must agree with `Config::load` on every shape that
    /// matters, or the escape hatch would silently mean different things to
    /// `a` and to the worker that acts on it.
    #[test]
    fn config_keep_exited_matches_full_config_load() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };

        // No config file at all: the documented default.
        assert!(!config_keep_exited(&paths));

        for (text, expected) in [
            ("version = 1\n", false),
            ("version = 1\nkeep_exited = false\n", false),
            ("version = 1\nkeep_exited = true\n", true),
        ] {
            fs::write(&paths.config_file, text).unwrap();
            assert_eq!(
                Config::load(&paths).unwrap().keep_exited,
                expected,
                "Config::load disagreed for {text:?}"
            );
            assert_eq!(
                config_keep_exited(&paths),
                expected,
                "config_keep_exited disagreed for {text:?}"
            );
        }

        // An unrelated invalid entry fails `Config::load` outright. The
        // worker's reader must not treat that as "keep records": a typo in
        // an engine definition is not a retention decision.
        fs::write(
            &paths.config_file,
            "version = 1\nkeep_exited = true\ndefault_engine = \"nope\"\n",
        )
        .unwrap();
        assert!(Config::load(&paths).is_err());
        assert!(config_keep_exited(&paths));

        // Unparsable or unreadable config: default, never a panic.
        fs::write(&paths.config_file, "this is not toml {{{").unwrap();
        assert!(!config_keep_exited(&paths));
    }

    #[test]
    fn registry_enumeration_reports_corrupt_records() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();
        fs::write(paths.record(id), b"{truncated").unwrap();

        let error = list_records(&paths).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&id.to_string()), "{message}");
        assert!(message.contains("parse"), "{message}");
    }

    /// The window `start_session` opens between creating a session directory
    /// and writing that session's first record. Any reader that does not hold
    /// the registry lock can land in it, and treating it as corruption killed
    /// `a watch` outright (see `list_records`). The same fixture must still be
    /// reported once the record appears, so the entry is skipped, not
    /// blacklisted.
    #[test]
    fn registry_enumeration_skips_a_session_whose_record_is_not_written_yet() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let pending = Uuid::new_v4();
        fs::create_dir(paths.state_session(pending)).unwrap();
        let written = Uuid::new_v4();
        fs::create_dir(paths.state_session(written)).unwrap();
        atomic_write_json(&paths.record(written), &registry_record(&paths, written)).unwrap();

        let records = list_records(&paths).unwrap();
        assert_eq!(
            records.iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![written],
            "a session mid-creation must be skipped, not reported and not fatal"
        );

        // ... and picked up as soon as its record lands.
        atomic_write_json(&paths.record(pending), &registry_record(&paths, pending)).unwrap();
        let mut ids = list_records(&paths)
            .unwrap()
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>();
        ids.sort();
        let mut expected = vec![pending, written];
        expected.sort();
        assert_eq!(ids, expected);
    }

    /// The complement of the test above: skipping a missing record must not
    /// weaken the fail-closed contract for a record that is present and wrong.
    #[test]
    fn registry_enumeration_still_fails_closed_on_an_empty_record_file() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();
        fs::write(paths.record(id), b"").unwrap();

        let error = list_records(&paths).unwrap_err();
        assert!(format!("{error:#}").contains("parse"), "{error:#}");
    }

    #[test]
    fn registry_enumeration_reports_unsupported_schema() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();
        let mut record = registry_record(&paths, id);
        record.schema_version = SCHEMA_VERSION + 1;
        atomic_write_json(&paths.record(id), &record).unwrap();

        let error = list_records(&paths).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported session schema"));
    }

    #[test]
    fn registry_enumeration_validates_directory_id_and_paths() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();

        let mut record = registry_record(&paths, id);
        record.id = Uuid::new_v4();
        atomic_write_json(&paths.record(id), &record).unwrap();
        assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("directory id"));

        record = registry_record(&paths, id);
        record.socket_path = paths.socket(Uuid::new_v4());
        atomic_write_json(&paths.record(id), &record).unwrap();
        assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("socket path"));

        record = registry_record(&paths, id);
        record.history_path = paths.history(Uuid::new_v4());
        atomic_write_json(&paths.record(id), &record).unwrap();
        assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("history path"));
    }

    #[test]
    fn registry_enumeration_grandfathers_legacy_history_capacity() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();

        let mut record = registry_record(&paths, id);
        record.history_bytes = MAX_HISTORY_BYTES + 1;
        atomic_write_json(&paths.record(id), &record).unwrap();

        let records = list_records(&paths).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].history_bytes, MAX_HISTORY_BYTES + 1);
    }

    #[test]
    fn registry_enumeration_rejects_unexpected_entries() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let unexpected = paths.state_root.join("sessions").join("leftover");
        fs::write(&unexpected, b"not a session directory").unwrap();

        let error = list_records(&paths).unwrap_err();
        assert!(format!("{error:#}").contains("is not a directory"));
    }

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
            parent_session: None,
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
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase: Phase::Running,
            worker_pid: Some(pid),
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(false),
            socket_path: state_dir.join("control.sock"),
            history_path: state_dir.join("history.bin"),
            exit: None,
            error: None,
        }
    }

    /// The two proof shapes that short-circuit before the kernel is ever
    /// consulted, and the no-locator shape that is the reported zombie.
    #[test]
    fn containment_reap_verdict_reads_durable_proof_without_probing() {
        let state = tempfile::tempdir().unwrap();
        let base = liveness_record(state.path());
        let refuse = |_: Uuid, _: &Path, _: Option<&CgroupIdentity>| -> Result<bool> {
            panic!("probe must not run when the record already answers the question")
        };

        let mut proven = base.clone();
        proven.containment_empty = Some(true);
        assert_eq!(
            containment_reap_verdict_with(&proven, refuse),
            ContainmentReap::Proven,
            "a worker's own durable proof must still be trusted"
        );

        let mut legacy_exit = base.clone();
        legacy_exit.containment_empty = None;
        legacy_exit.exit = Some(ExitInfo {
            code: Some(0),
            signal: None,
            oom_killed: false,
            exited_at_ms: 2,
        });
        assert_eq!(
            containment_reap_verdict_with(&legacy_exit, refuse),
            ContainmentReap::Proven,
            "the legacy pre-field ExitInfo proof must still be trusted"
        );

        // The reported zombie shape: unlimited session, worker SIGKILLed
        // before it could prove anything. No locator, so nothing to probe.
        let unlimited = base.clone();
        assert_eq!(unlimited.containment_cgroup, None);
        assert_eq!(unlimited.containment_empty, Some(false));
        assert_eq!(
            containment_reap_verdict_with(&unlimited, refuse),
            ContainmentReap::NoRemainingHandle
        );
    }

    /// Every outcome the kernel probe can return, including the one arm that
    /// stands between `a prune` and deleting the last handle to a live
    /// containment domain: a locator that validates and is still POPULATED
    /// must retain. Injected rather than staged on a real cgroup so this
    /// runs everywhere, on every `cargo test`, with no delegation needed;
    /// `recorded_cgroup_observed_empty_tracks_a_real_delegated_cgroup`
    /// covers the probe itself against a real one.
    #[test]
    fn containment_reap_verdict_maps_every_cgroup_probe_outcome() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_empty = Some(false);
        record.containment_cgroup = Some(PathBuf::from(format!(
            "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
            record.id
        )));

        assert_eq!(
            containment_reap_verdict_with(&record, |_, _, _| Ok(true)),
            ContainmentReap::Proven,
            "an observed-empty domain is proof at least as strong as the persisted bit"
        );
        assert_eq!(
            containment_reap_verdict_with(&record, |_, _, _| Ok(false)),
            ContainmentReap::Retain,
            "a populated containment domain must keep its locator"
        );
        assert_eq!(
            containment_reap_verdict_with(&record, |_, _, _| bail!("cgroup inspection failed")),
            ContainmentReap::Retain,
            "an unreadable containment domain must fail closed"
        );

        // The probe is handed the record's own identity triple -- a mixed-up
        // locator would validate against the wrong domain.
        let mut seen = None;
        containment_reap_verdict_with(&record, |id, locator, identity| {
            seen = Some((id, locator.to_path_buf(), identity.cloned()));
            Ok(false)
        });
        let (id, locator, identity) = seen.expect("probe ran");
        assert_eq!(id, record.id);
        assert_eq!(Some(locator), record.containment_cgroup);
        assert!(identity.is_none());
    }

    /// A recorded cgroup with no identity cannot be validated, so it cannot
    /// be declared empty either -- keep the locator.
    #[test]
    fn containment_reap_verdict_retains_an_unvalidatable_locator() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_cgroup = Some(PathBuf::from(format!(
            "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
            record.id
        )));
        assert!(record.containment_cgroup_identity.is_none());
        assert_eq!(
            containment_reap_verdict(&record),
            ContainmentReap::Retain,
            "an unvalidatable containment locator must fail closed"
        );
    }

    /// A cgroup recorded under a different boot cannot hold a live process:
    /// the hierarchy and every task in it ceased to exist at reboot. Without
    /// this, `validate_recorded_cgroup`'s (correct, for destructive
    /// recovery) refusal to touch a foreign-boot identity would make a
    /// rebooted-away record permanently unreapable.
    #[test]
    fn containment_reap_verdict_treats_a_previous_boot_as_empty() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_cgroup = Some(PathBuf::from(format!(
            "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
            record.id
        )));
        let mut identity = current_cgroup_identity().unwrap_or(CgroupIdentity {
            boot_id: String::new(),
            cgroup_namespace_device: 0,
            cgroup_namespace_inode: 0,
            mount_namespace_device: 0,
            mount_namespace_inode: 0,
            cgroup_mount_id: 0,
            cgroup_root_device: 0,
            cgroup_root_inode: 0,
        });
        identity.boot_id = "00000000-0000-0000-0000-000000000000".into();
        assert_ne!(identity.boot_id, linux_boot_id().unwrap());
        record.containment_cgroup_identity = Some(identity);
        assert_eq!(
            containment_reap_verdict(&record),
            ContainmentReap::Proven,
            "a cgroup from a previous boot cannot hold a live process"
        );
    }

    /// The membership half of the real probe, without needing a real
    /// cgroup: `cgroup.events` says `populated 1` while tasks remain, and a
    /// collected cgroup loses the file entirely (ENOENT means empty).
    #[test]
    fn cgroup_path_populated_reads_the_kernel_counter_and_treats_enoent_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !cgroup_path_populated(dir.path()).unwrap(),
            "a collected cgroup (no cgroup.events) is empty, not an error"
        );
        fs::write(dir.path().join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert!(cgroup_path_populated(dir.path()).unwrap());
        fs::write(dir.path().join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        assert!(!cgroup_path_populated(dir.path()).unwrap());
        fs::write(dir.path().join("cgroup.events"), "frozen 0\n").unwrap();
        assert!(
            cgroup_path_populated(dir.path()).is_err(),
            "a cgroup.events with no populated key must fail closed, not read as empty"
        );
    }

    /// A cgroup created inside the caller's own delegated subtree, named
    /// exactly the way a real session's containment scope is named, so
    /// `validate_recorded_cgroup`'s full chain (locator shape, cgroup-v2
    /// filesystem, mount device, identity triple) runs for real. Returns
    /// None when the environment has no writable cgroup-v2 parent.
    struct DelegatedCgroup {
        path: PathBuf,
        members: Vec<std::process::Child>,
    }

    impl DelegatedCgroup {
        fn create(id: Uuid) -> Option<Self> {
            let own = fs::read_to_string("/proc/self/cgroup").ok()?;
            let relative = own
                .lines()
                .find_map(|line| line.strip_prefix("0::"))?
                .trim()
                .trim_start_matches('/')
                .to_string();
            let mut candidate = Path::new(CGROUP_V2_ROOT).join(&relative);
            let leaf = format!("aplexer-workload-{id}.scope");
            // Walk up until a parent accepts a new child cgroup: the leaf a
            // test process sits in is usually not delegated, its user@.service
            // ancestor is.
            loop {
                let path = candidate.join(&leaf);
                if fs::create_dir(&path).is_ok() {
                    return Some(Self {
                        path,
                        members: Vec::new(),
                    });
                }
                candidate = candidate.parent()?.to_path_buf();
                if !candidate.starts_with(CGROUP_V2_ROOT) || candidate == Path::new(CGROUP_V2_ROOT)
                {
                    return None;
                }
            }
        }

        fn populate(&mut self) -> u32 {
            let child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn cgroup member");
            let pid = child.id();
            self.members.push(child);
            fs::write(self.path.join("cgroup.procs"), format!("{pid}\n"))
                .expect("move member into the delegated cgroup");
            pid
        }

        /// Stop every member and reap it, so the cgroup can be collected and
        /// no `sleep` outlives the test.
        fn drain_members(&mut self) {
            for mut member in self.members.drain(..) {
                let _ = member.kill();
                let _ = member.wait();
            }
        }
    }

    impl Drop for DelegatedCgroup {
        fn drop(&mut self) {
            self.drain_members();
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.path.exists() && Instant::now() < deadline {
                if fs::remove_dir(&self.path).is_ok() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    /// The real kernel probe, end to end, against a genuinely delegated
    /// cgroup: empty, then POPULATED (the arm that must retain), then empty
    /// again, then collected. `#[ignore]`d for the same reason
    /// `tests/oom_isolation.rs`'s destructive tests are -- it needs a
    /// cgroup-v2 tree with delegation to the running user, which a CI
    /// container generally lacks. Run it explicitly:
    ///
    ///   cargo test --lib recorded_cgroup_observed_empty -- --ignored --nocapture
    ///
    /// The decision arms it feeds are pinned unconditionally by
    /// `containment_reap_verdict_maps_every_cgroup_probe_outcome`, and the
    /// membership read by
    /// `cgroup_path_populated_reads_the_kernel_counter_and_treats_enoent_as_empty`;
    /// this test is what proves those two meet reality.
    #[test]
    #[ignore = "needs cgroup-v2 delegation to the running user; run explicitly"]
    fn recorded_cgroup_observed_empty_tracks_a_real_delegated_cgroup() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_empty = Some(false);
        let mut cgroup = DelegatedCgroup::create(record.id)
            .expect("this environment has no writable cgroup-v2 parent");
        record.containment_cgroup = Some(cgroup.path.clone());
        record.containment_cgroup_identity = Some(current_cgroup_identity().unwrap());
        let probe = || {
            recorded_cgroup_observed_empty(
                record.id,
                record.containment_cgroup.as_deref().unwrap(),
                record.containment_cgroup_identity.as_ref(),
            )
            .unwrap()
        };

        assert!(probe(), "a freshly created cgroup is empty");
        assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);

        let pid = cgroup.populate();
        assert!(
            !probe(),
            "a cgroup holding a live process must not read as empty"
        );
        assert_eq!(
            containment_reap_verdict(&record),
            ContainmentReap::Retain,
            "prune must keep the locator of a populated containment domain"
        );
        assert!(process_alive(pid), "probing must not signal anything");

        cgroup.drain_members();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !probe() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(probe(), "an emptied cgroup must read as empty again");
        assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);

        // Collected: cgroup v2 cannot remove a populated cgroup, so a
        // durably recorded locator that has since disappeared is empty by
        // construction.
        fs::remove_dir(&cgroup.path).expect("remove the now-empty cgroup");
        assert!(probe(), "a collected cgroup is empty by construction");
        assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);
    }

    /// `state` is derived from both facts and rewrites neither.
    #[test]
    fn observed_state_reports_broken_only_for_a_contradicted_phase() {
        let now = DEFAULT_STARTUP_TIMEOUT_MS * 2;
        let aged = |age: u64| now - age;
        // A `Starting` record with no live worker is the shape
        // `start_session` persists before the worker registers its pid, and
        // also the shape a crashed start leaves behind. Only age tells them
        // apart, and the boundary is exactly the startup budget.
        assert_eq!(
            observed_state(&Phase::Starting, false, aged(0), now),
            "starting"
        );
        assert_eq!(
            observed_state(
                &Phase::Starting,
                false,
                aged(DEFAULT_STARTUP_TIMEOUT_MS - 1),
                now
            ),
            "starting"
        );
        assert_eq!(
            observed_state(
                &Phase::Starting,
                false,
                aged(DEFAULT_STARTUP_TIMEOUT_MS),
                now
            ),
            "broken",
            "past the startup budget a pre-PID record is a crashed start"
        );
        // A record whose clock ran backwards (or was written by a machine
        // with a different clock) must not become permanently `starting`.
        assert_eq!(
            observed_state(&Phase::Starting, false, now + 1_000, now),
            "starting"
        );
        assert_eq!(observed_state(&Phase::Starting, true, 0, now), "starting");
        // Running/Exiting are only ever written by a worker that already
        // registered, so a dead worker there is broken at any age.
        for phase in [Phase::Running, Phase::Exiting] {
            assert_eq!(observed_state(&phase, false, aged(0), now), "broken");
            assert_eq!(observed_state(&phase, false, aged(1), now), "broken");
            assert_eq!(observed_state(&phase, true, aged(0), now), phase.name());
        }
        for phase in [Phase::Exited, Phase::Failed] {
            assert_eq!(observed_state(&phase, false, aged(0), now), phase.name());
            assert_eq!(observed_state(&phase, true, aged(0), now), phase.name());
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
    fn bound_request_remains_readable_by_legacy_workers() {
        #[derive(Deserialize)]
        struct LegacyRequest {
            version: u16,
            request_id: String,
            #[serde(flatten)]
            operation: Operation,
        }

        let request = Request::new(Uuid::new_v4(), Operation::Ping);
        let legacy: LegacyRequest =
            serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert_eq!(legacy.version, PROTOCOL_VERSION);
        assert_eq!(legacy.request_id, request.request_id);
        assert!(matches!(legacy.operation, Operation::Ping));
    }

    #[test]
    fn bounded_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::open(dir.path().join("h"), 4).unwrap();
        h.append(b"abcdef").unwrap();
        assert_eq!(h.snapshot(None), b"cdef");
    }

    #[test]
    fn history_incremental_flush_writes_only_delta_and_recovers_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 8).unwrap();

        history.append(b"abcdef").unwrap();
        history.flush().unwrap();
        assert_eq!(history.data_bytes_written, 6);
        history.append(b"\0g").unwrap();
        history.flush().unwrap();
        assert_eq!(history.data_bytes_written, 8);
        history.append(b"hi").unwrap();
        history.flush().unwrap();
        assert_eq!(history.data_bytes_written, 10);
        assert_eq!(history.snapshot(None), b"cdef\0ghi");

        let reopened = History::open(path.clone(), 8).unwrap();
        assert_eq!(reopened.snapshot(None), b"cdef\0ghi");
        assert_eq!(
            read_persisted_history_tail(&path, None).unwrap(),
            b"cdef\0ghi"
        );
    }

    #[test]
    fn history_uncommitted_suffix_is_ignored_and_truncated_on_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"safe").unwrap();
        history.flush().unwrap();

        let data_path = history_data_path(&path, 0);
        OpenOptions::new()
            .append(true)
            .open(&data_path)
            .unwrap()
            .write_all(b"torn")
            .unwrap();
        let mut reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"safe");
        reopened.append(b"-next").unwrap();
        reopened.flush().unwrap();
        assert_eq!(
            History::open(path, 16).unwrap().snapshot(None),
            b"safe-next"
        );
    }

    #[test]
    fn history_corrupt_newest_commit_recovers_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"prior").unwrap();
        history.flush().unwrap();
        history.append(b"-newest").unwrap();
        history.flush().unwrap();

        fs::write(history_commit_path(&path, 0), b"{torn").unwrap();
        let reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"prior");
        assert_eq!(read_persisted_history_tail(&path, None).unwrap(), b"prior");
    }

    #[test]
    fn history_corrupt_v2_pair_never_falls_back_to_stale_raw_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"prior").unwrap();
        history.flush().unwrap();
        history.append(b"-newest").unwrap();
        history.flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"prior-newest");

        fs::write(history_commit_path(&path, 0), b"{torn-newest").unwrap();
        fs::write(history_commit_path(&path, 1), b"{torn-prior").unwrap();
        let read_error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{read_error:#}").contains("no valid committed history generation"),
            "{read_error:#}"
        );
        assert!(History::open(path.clone(), 16).is_err());
        assert_eq!(
            fs::read(path).unwrap(),
            b"prior-newest",
            "fail-closed v2 recovery mutated the raw compatibility evidence"
        );
    }

    #[test]
    fn history_marker_prevents_raw_fallback_when_all_commits_disappear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"v2-authoritative").unwrap();
        history.flush().unwrap();
        assert!(history_marker_path(&path).is_file());
        assert!(history_data_path(&path, 0).is_file());

        fs::write(&path, b"stale-raw").unwrap();
        for slot in 0..HISTORY_COMMIT_COUNT {
            let commit = history_commit_path(&path, slot);
            if commit.exists() {
                fs::remove_file(commit).unwrap();
            }
        }

        let read_error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{read_error:#}").contains("no valid committed history generation"),
            "{read_error:#}"
        );
        assert!(History::open(path.clone(), 16).is_err());
        assert_eq!(fs::read(path).unwrap(), b"stale-raw");
    }

    #[test]
    fn history_unpublished_first_bank_without_marker_still_recovers_legacy_raw() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"raw-precommit").unwrap();
        let blocked_commit = history_commit_path(&path, 1);
        fs::create_dir(&blocked_commit).unwrap();

        assert!(history.flush().is_err());
        assert!(history_data_path(&path, 0).is_file());
        assert!(!history_marker_path(&path).exists());
        assert_eq!(fs::read(&path).unwrap(), b"raw-precommit");
        drop(history);
        fs::remove_dir(blocked_commit).unwrap();

        assert_eq!(
            read_persisted_history_tail(&path, None).unwrap(),
            b"raw-precommit"
        );
        let reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"raw-precommit");
        assert!(history_marker_path(&path).is_file());
    }

    #[test]
    fn history_markerless_v2_is_readable_and_next_writable_open_publishes_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"pre-marker-v2").unwrap();
        history.flush().unwrap();
        fs::remove_file(history_marker_path(&path)).unwrap();

        assert_eq!(
            read_persisted_history_tail(&path, None).unwrap(),
            b"pre-marker-v2"
        );
        assert!(!history_marker_path(&path).exists());
        let reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"pre-marker-v2");
        assert!(history_marker_path(&path).is_file());
    }

    #[test]
    fn history_marker_is_bounded_checksummed_and_a_safe_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let marker_path = history_marker_path(&path);
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"committed").unwrap();
        history.flush().unwrap();
        let valid_marker = fs::read(&marker_path).unwrap();

        let mut bad_checksum: HistoryMarker = serde_json::from_slice(&valid_marker).unwrap();
        bad_checksum.store_id = Uuid::new_v4();
        fs::write(&marker_path, serde_json::to_vec(&bad_checksum).unwrap()).unwrap();
        let error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("checksum mismatch"),
            "{error:#}"
        );

        let wrong_store = bad_checksum.seal().unwrap();
        fs::write(&marker_path, serde_json::to_vec(&wrong_store).unwrap()).unwrap();
        let error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("no valid committed history generation"),
            "{error:#}"
        );

        fs::write(&marker_path, vec![b'x'; HISTORY_MARKER_MAX_BYTES + 1]).unwrap();
        let error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds the"), "{error:#}");

        fs::remove_file(&marker_path).unwrap();
        let target = dir.path().join("marker-target");
        fs::write(&target, b"unrelated").unwrap();
        symlink(&target, &marker_path).unwrap();
        assert!(read_persisted_history_tail(&path, None).is_err());
        fs::remove_file(&marker_path).unwrap();

        let marker_c = CString::new(marker_path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(marker_c.as_ptr(), 0o600) }, 0);
        assert!(read_persisted_history_tail(&path, None).is_err());
        assert!(History::open(path.clone(), 16).is_err());
        assert_eq!(fs::read(target).unwrap(), b"unrelated");
    }

    #[test]
    fn history_compaction_is_bounded_and_amortized_by_new_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 4).unwrap();
        history.append(b"abcd").unwrap();
        history.flush().unwrap();
        history.append(b"efgh").unwrap();
        history.flush().unwrap();
        history.append(b"i").unwrap();
        history.flush().unwrap();

        assert_eq!(history.snapshot(None), b"fghi");
        assert_eq!(history.data_bytes_written, 12);
        assert_eq!(fs::read(&path).unwrap(), b"fghi");
        for slot in 0..HISTORY_BANK_COUNT {
            let data_path = history_data_path(&path, slot);
            if let Ok(metadata) = fs::metadata(data_path) {
                assert!(
                    metadata.len() <= HISTORY_BANK_HEADER_BYTES as u64 + 2 * history.cap as u64
                );
            }
        }
        assert_eq!(History::open(path, 4).unwrap().snapshot(None), b"fghi");
    }

    #[test]
    fn history_legacy_migration_and_capacity_changes_keep_only_exact_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        fs::write(&path, b"0123456789").unwrap();

        let mut migrated = History::open(path.clone(), 4).unwrap();
        assert_eq!(migrated.snapshot(None), b"6789");
        assert_eq!(fs::read(&path).unwrap(), b"6789");
        migrated.append(b"AB").unwrap();
        migrated.flush().unwrap();
        assert_eq!(read_persisted_history_tail(&path, None).unwrap(), b"89AB");
        assert_eq!(fs::read(&path).unwrap(), b"6789AB");
        migrated.flush_final().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"89AB");

        let shrunk = History::open(path.clone(), 3).unwrap();
        assert_eq!(shrunk.snapshot(None), b"9AB");
        let mut grown = History::open(path.clone(), 6).unwrap();
        assert_eq!(grown.snapshot(None), b"9AB");
        grown.append(b"CD").unwrap();
        grown.flush().unwrap();
        assert_eq!(History::open(path, 6).unwrap().snapshot(None), b"9ABCD");
    }

    #[test]
    fn history_special_files_fail_without_becoming_persistence_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"unrelated").unwrap();
        let legacy = dir.path().join("history.bin");
        symlink(&target, &legacy).unwrap();
        assert!(History::open(legacy.clone(), 8).is_err());
        assert!(read_persisted_history_tail(&legacy, None).is_err());
        fs::remove_file(&legacy).unwrap();

        let commit = history_commit_path(&legacy, 0);
        let commit_c = CString::new(commit.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(commit_c.as_ptr(), 0o600) }, 0);
        assert!(History::open(legacy.clone(), 8).is_err());
        assert!(read_persisted_history_tail(&legacy, None).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unrelated");
    }

    #[test]
    fn history_capacity_has_one_global_limit_and_zero_stays_disabled() {
        for value in [0, 1, DEFAULT_HISTORY_BYTES, MAX_HISTORY_BYTES] {
            assert_eq!(validate_history_bytes(value).unwrap(), value);
        }
        for value in [MAX_HISTORY_BYTES + 1, usize::MAX] {
            let error = validate_history_bytes(value).unwrap_err().to_string();
            assert!(error.contains("history_bytes"), "{error}");
            assert!(error.contains(&MAX_HISTORY_BYTES.to_string()), "{error}");
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disabled-history.bin");
        let mut history = History::open(path.clone(), 0).unwrap();
        history.append(b"not retained").unwrap();
        history.flush().unwrap();
        assert!(history.snapshot(None).is_empty());
        assert!(!path.exists());
        assert!(read_persisted_history_tail(&path, None).unwrap().is_empty());
        assert!(History::open(dir.path().join("too-large"), MAX_HISTORY_BYTES + 1).is_err());
    }

    #[test]
    fn config_rejects_oversized_profile_history() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        fs::write(
            &paths.config_file,
            format!(
                "version = 1\n[profiles.too_large]\nhistory_bytes = {}\n",
                MAX_HISTORY_BYTES + 1
            ),
        )
        .unwrap();

        let error = Config::load(&paths).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("profile \"too_large\" history_bytes"),
            "{message}"
        );
        assert!(
            message.contains(&MAX_HISTORY_BYTES.to_string()),
            "{message}"
        );
    }

    #[test]
    fn config_schema_rejects_unknown_fields_at_every_level() {
        for (label, text, unknown) in [
            (
                "root",
                "version = 1\ndefualt_engine = \"shell\"\n",
                "defualt_engine",
            ),
            (
                "engine",
                "version = 1\n[engines.custom]\ncommand = [\"true\"]\ncomand = [\"false\"]\n",
                "comand",
            ),
            (
                "profile",
                "version = 1\n[profiles.review]\nhistroy_bytes = 1024\n",
                "histroy_bytes",
            ),
            (
                "profile limits",
                "version = 1\n[profiles.review.limits]\nmemroy_bytes = 1024\n",
                "memroy_bytes",
            ),
            (
                "shortcut",
                "version = 1\n[shortcuts.review]\nengine = \"shell\"\nprofiel = \"review\"\n",
                "profiel",
            ),
        ] {
            let message = format!("{:#}", load_config_text(text).unwrap_err());
            assert!(message.contains("unknown field"), "{label}: {message}");
            assert!(message.contains(unknown), "{label}: {message}");
        }
    }

    #[test]
    fn config_semantics_reject_dangling_references_and_invalid_commands() {
        for (label, text, expected) in [
            (
                "default engine",
                "version = 1\ndefault_engine = \"missing\"\n",
                "default_engine \"missing\"",
            ),
            (
                "default profile",
                "version = 1\ndefault_profile = \"missing\"\n",
                "default_profile \"missing\"",
            ),
            (
                "profile engine",
                "version = 1\n[profiles.review]\nengine = \"missing\"\n",
                "profile \"review\" engine \"missing\"",
            ),
            (
                "shortcut engine",
                "version = 1\n[shortcuts.review]\nengine = \"missing\"\n",
                "shortcut \"review\" engine \"missing\"",
            ),
            (
                "shortcut profile",
                "version = 1\n[shortcuts.review]\nengine = \"shell\"\nprofile = \"missing\"\n",
                "shortcut \"review\" profile \"missing\"",
            ),
            (
                "shortcut profile engine",
                "version = 1\n[profiles.review]\nengine = \"claude\"\n[shortcuts.review]\nengine = \"codex\"\nprofile = \"review\"\n",
                "selects engine \"codex\", but profile \"review\" selects engine \"claude\"",
            ),
            (
                "empty engine command",
                "version = 1\n[engines.shell]\ncommand = []\n",
                "engine \"shell\" command must not be empty",
            ),
            (
                "empty profile command",
                "version = 1\n[profiles.review]\ncommand = []\n",
                "profile \"review\" command must not be empty",
            ),
            (
                "empty profile executable",
                "version = 1\n[profiles.review]\nexecutable = \"\"\n",
                "profile \"review\" executable must not be empty",
            ),
            (
                "ignored profile executable",
                "version = 1\n[profiles.review]\ncommand = [\"true\"]\nexecutable = \"false\"\n",
                "cannot set both command and executable",
            ),
            (
                "ignored profile args",
                "version = 1\n[profiles.review]\ncommand = [\"true\"]\nargs = [\"--ignored\"]\n",
                "cannot set both command and args",
            ),
        ] {
            let message = format!("{:#}", load_config_text(text).unwrap_err());
            assert!(message.contains(expected), "{label}: {message}");
        }
    }

    #[test]
    fn config_semantics_reject_invalid_numeric_limits() {
        for (label, field, expected) in [
            ("memory", "memory_bytes = 0", "memory_bytes must be greater"),
            ("pids", "pids = 0", "pids must be greater"),
            ("quota", "cpu_quota_us = 0", "cpu_quota_us must be greater"),
            (
                "period",
                "cpu_quota_us = 1\ncpu_period_us = 0",
                "cpu_period_us must be greater",
            ),
            (
                "orphan period",
                "cpu_period_us = 100000",
                "cpu_period_us requires cpu_quota_us",
            ),
        ] {
            let text = format!("version = 1\n[profiles.review.limits]\n{field}\n");
            let message = format!("{:#}", load_config_text(&text).unwrap_err());
            assert!(message.contains(expected), "{label}: {message}");
        }

        let config = load_config_text("version = 1\n").unwrap();
        let error = config
            .resolve(
                vec!["/bin/true".into()],
                Some("shell"),
                None,
                Path::new("/tmp"),
                None,
                &BTreeMap::new(),
                &Limits {
                    pids: Some(0),
                    ..Limits::default()
                },
                None,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("resolved launch limits pids"),
            "{error:#}"
        );
    }

    #[test]
    fn valid_config_references_commands_and_limits_still_load() {
        let config = load_config_text(
            "version = 1\n\
             default_engine = \"custom\"\n\
             default_profile = \"review\"\n\
             [engines.custom]\n\
             command = [\"/bin/sh\", \"-l\"]\n\
             env_unset = [\"CUSTOM_SECRET\"]\n\
             [profiles.review]\n\
             engine = \"custom\"\n\
             args = [\"--review\"]\n\
             history_bytes = 0\n\
             [profiles.review.limits]\n\
             memory_bytes = 1048576\n\
             pids = 4\n\
             cpu_quota_us = 50000\n\
             cpu_period_us = 100000\n\
             [shortcuts.rev]\n\
             engine = \"custom\"\n\
             profile = \"review\"\n",
        )
        .unwrap();

        assert_eq!(config.default_engine.as_deref(), Some("custom"));
        assert_eq!(config.default_profile.as_deref(), Some("review"));
        for (name, shortcut) in &config.shortcuts {
            assert!(config.engines.contains_key(&shortcut.engine), "{name}");
            if let Some(profile) = &shortcut.profile {
                assert!(config.profiles.contains_key(profile), "{name}");
            }
        }
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
            None,
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
        let identity = current_cgroup_identity().unwrap();
        let error = cleanup_recorded_cgroup_until(
            id,
            Path::new("/tmp/not-a-cgroup"),
            Some(&identity),
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
    fn cgroup_identity_captures_current_v2_kernel_domain() {
        let identity = current_cgroup_identity().unwrap();
        assert_eq!(identity.boot_id, linux_boot_id().unwrap());
        assert_ne!(identity.cgroup_namespace_inode, 0);
        assert_ne!(identity.mount_namespace_inode, 0);
        assert_ne!(identity.cgroup_mount_id, 0);
        assert_ne!(identity.cgroup_root_inode, 0);
        ensure_cgroup2_filesystem(Path::new(CGROUP_V2_ROOT)).unwrap();
    }

    #[test]
    fn live_cgroup_disappearance_is_empty_only_in_matching_domain() {
        let identity = current_cgroup_identity().unwrap();
        let missing_path =
            Path::new(CGROUP_V2_ROOT).join(format!("aplexer-workload-{}.scope", Uuid::new_v4()));
        assert!(!missing_path.exists());
        let collected = Cgroup {
            path: missing_path,
            identity: identity.clone(),
            anchor: Arc::new(Mutex::new(None)),
            initial_oom_kill: 0,
        };
        assert!(!collected.populated().unwrap());

        assert!(!live_cgroup_populated_with(&identity, || {
            Err(io::Error::from(io::ErrorKind::NotFound).into())
        })
        .unwrap());

        let mut wrong_mount = identity.clone();
        wrong_mount.cgroup_mount_id ^= 1;
        let mismatch = live_cgroup_populated_with(&wrong_mount, || {
            panic!("membership must not be read in a mismatched kernel domain")
        })
        .expect_err("mismatched identity must fail closed");
        assert!(mismatch.to_string().contains("before reading membership"));

        let malformed =
            live_cgroup_populated_with(&identity, || Err(anyhow!("malformed cgroup.events")))
                .expect_err("non-ENOENT membership errors must fail closed");
        assert!(malformed
            .to_string()
            .contains("read live cgroup membership"));
    }

    #[test]
    fn control_group_locator_is_uuid_bound_and_cannot_escape_root() {
        let id = Uuid::new_v4();
        let valid = format!("/user.slice/user-1000.slice/aplexer-workload-{id}.scope");
        assert_eq!(
            control_group_locator(id, &valid).unwrap(),
            Path::new(CGROUP_V2_ROOT).join(valid.trim_start_matches('/'))
        );
        assert!(
            control_group_locator(id, &format!("/user.slice/../aplexer-workload-{id}.scope"))
                .is_err()
        );
        assert!(control_group_locator(id, "relative.scope").is_err());
        assert!(control_group_locator(
            id,
            &format!("/user.slice/aplexer-workload-{}.scope", Uuid::new_v4())
        )
        .is_err());
    }

    #[test]
    fn scope_wait_retries_empty_control_group_then_accepts_valid_path() {
        let dir = tempfile::tempdir().unwrap();
        let systemctl = dir.path().join("systemctl");
        let id = Uuid::new_v4();
        let unit = format!("aplexer-workload-{id}");
        let reported = format!("/user.slice/{unit}.scope");
        fs::write(
            &systemctl,
            format!(
                "#!/bin/sh\nif [ ! -e \"$0.seen\" ]; then : > \"$0.seen\"; printf '\\n'; else printf '%s\\n' '{}'; fi\n",
                reported
            ),
        )
        .unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();

        let mut validations = 0;
        let path = wait_for_scope_cgroup_with(
            id,
            &unit,
            &systemctl,
            "--user",
            Duration::from_secs(1),
            |path| {
                validations += 1;
                Ok(Some(path.to_path_buf()))
            },
        )
        .unwrap();

        assert_eq!(
            path,
            Path::new(CGROUP_V2_ROOT).join(reported.trim_start_matches('/'))
        );
        assert_eq!(validations, 1, "empty value must not reach validation");
        assert!(systemctl.with_extension("seen").exists());
    }

    #[test]
    fn system_helpers_resolve_without_ambient_path() {
        for helper in ["systemd-run", "systemctl", "sleep"] {
            let path = trusted_system_helper(helper).unwrap();
            assert!(path.is_absolute());
            assert_eq!(fs::metadata(path).unwrap().uid(), 0);
        }

        let dir = tempfile::tempdir().unwrap();
        let shadow = dir.path().join("systemctl");
        fs::write(&shadow, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&shadow, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_trusted_helper(&shadow).is_err());
    }

    #[test]
    fn missing_cgroup_leaf_requires_matching_persisted_identity() {
        let id = Uuid::new_v4();
        let locator = PathBuf::from(format!("{CGROUP_V2_ROOT}/aplexer-workload-{id}.scope"));
        let missing = validate_recorded_cgroup(id, &locator, None)
            .expect_err("legacy locator must not prove emptiness");
        assert!(missing
            .to_string()
            .contains("no boot/namespace/mount identity"));

        let mut wrong_boot = current_cgroup_identity().unwrap();
        wrong_boot.boot_id = Uuid::new_v4().to_string();
        let mismatch = validate_recorded_cgroup(id, &locator, Some(&wrong_boot))
            .expect_err("cross-boot locator must not prove emptiness");
        assert!(mismatch.to_string().contains("does not match"));

        let mut wrong_mount = current_cgroup_identity().unwrap();
        wrong_mount.cgroup_mount_id = wrong_mount.cgroup_mount_id.saturating_add(1);
        let mismatch = validate_recorded_cgroup(id, &locator, Some(&wrong_mount))
            .expect_err("replacement mount must not prove emptiness");
        assert!(mismatch.to_string().contains("does not match"));

        let identity = current_cgroup_identity().unwrap();
        assert_eq!(
            validate_recorded_cgroup(id, &locator, Some(&identity)).unwrap(),
            None,
            "same-domain missing cgroup is empty"
        );
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
    fn cgroup_setup_helper_obeys_wall_clock_deadline() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let started = Instant::now();
        let error = command_output_until(
            &mut command,
            Instant::now() + Duration::from_millis(50),
            "exercise setup timeout",
        )
        .expect_err("wedged setup helper must time out");
        assert!(error.to_string().contains("timed out waiting"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cgroup_setup_helper_collects_bounded_output() {
        let mut command = Command::new("/bin/printf");
        command.arg("/user.slice/example.scope\n");
        let output = command_output_until(
            &mut command,
            Instant::now() + Duration::from_secs(1),
            "exercise setup output",
        )
        .expect("short-lived setup helper");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"/user.slice/example.scope\n");
    }

    #[test]
    fn cgroup_setup_helper_pipe_cannot_outlive_deadline() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 0.2 & exit 0"]);
        let started = Instant::now();
        let error = command_output_until(
            &mut command,
            Instant::now() + Duration::from_millis(50),
            "exercise inherited output pipe",
        )
        .expect_err("inherited helper pipe must not defeat deadline");
        assert!(error.to_string().contains("timed out waiting"));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    /// `kill(pid, 0)` succeeds for a zombie, so the raw signalability test
    /// reported exited-but-unreaped processes as alive. Under a worker's
    /// child subreaper that is not a corner case: a session started inside
    /// another session reparents onto the outer worker, and until it is
    /// reaped every liveness answer about it -- `worker_alive`,
    /// `workload_leader_alive`, and therefore `reap_verdict` and `a prune` --
    /// was wrong in the direction of "still running, keep it".
    #[test]
    fn process_alive_reports_an_unreaped_zombie_as_dead() {
        let mut child = Command::new("/bin/true").spawn().unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process_is_zombie(pid) {
            assert!(
                Instant::now() < deadline,
                "child {pid} never became a zombie"
            );
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(
            process_state(pid).unwrap(),
            'Z',
            "the test needs a real unreaped zombie"
        );
        assert_eq!(
            unsafe { libc::kill(pid as libc::pid_t, 0) },
            0,
            "a zombie is still signalable, which is exactly the trap"
        );
        assert!(
            !process_alive(pid),
            "zombie {pid} must not be reported alive"
        );

        child.wait().unwrap();
        assert!(!process_alive(pid));
        assert!(
            !process_is_zombie(pid),
            "a reaped pid has no state to read, so it is not a zombie either"
        );
    }

    /// A `Z` in `/proc/<pid>/stat` is not by itself proof that a process is
    /// finished: a thread group leader that exited while its siblings kept
    /// running reads exactly the same (verified against a real process --
    /// `state=Z` with two entries under `/proc/<pid>/task`). Treating that
    /// as dead would let a multi-threaded workload be declared contained
    /// while it was still executing, so the thread group must be down to the
    /// leader's corpse alone.
    #[test]
    fn zombie_detection_requires_an_empty_thread_group() {
        let root = tempfile::tempdir().unwrap();
        let write_process = |pid: u32, state: char, threads: &[u32]| {
            let dir = root.path().join(pid.to_string());
            fs::create_dir_all(&dir).unwrap();
            // A comm containing spaces and a ')' is legal and must not shift
            // the state field.
            fs::write(
                dir.join("stat"),
                format!("{pid} (od d) ba) {state} 1 {pid} 0 -1 4194304 0 0\n"),
            )
            .unwrap();
            for tid in threads {
                fs::create_dir_all(dir.join("task").join(tid.to_string())).unwrap();
            }
        };

        write_process(11, 'Z', &[11]);
        write_process(12, 'Z', &[12, 13]);
        write_process(14, 'S', &[14]);
        write_process(15, 'R', &[15, 16]);

        assert_eq!(process_state_in(root.path(), 11).unwrap(), 'Z');
        assert_eq!(process_state_in(root.path(), 12).unwrap(), 'Z');

        assert!(
            process_is_zombie_in(root.path(), 11),
            "a Z leader alone in its thread group is a reapable zombie"
        );
        assert!(
            !process_is_zombie_in(root.path(), 12),
            "a Z leader with a live sibling thread is still running code"
        );
        assert!(!process_is_zombie_in(root.path(), 14));
        assert!(!process_is_zombie_in(root.path(), 15));
        assert!(
            !process_is_zombie_in(root.path(), 99),
            "an unreadable process must not be subtracted from liveness"
        );
    }

    /// A live process must never be mistaken for a zombie by the state read.
    #[test]
    fn process_alive_still_reports_a_running_child_as_alive() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        assert!(process_alive(pid));
        assert!(!process_is_zombie(pid));
        assert!(matches!(process_state(pid).unwrap(), 'R' | 'S' | 'D'));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn cgroup_anchor_release_owns_child_through_kill_and_reap() {
        let anchor = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let anchor_pid = anchor.id();
        let cgroup = Cgroup {
            path: PathBuf::from("/does/not/exist"),
            identity: current_cgroup_identity().unwrap(),
            anchor: Arc::new(Mutex::new(Some(anchor))),
            initial_oom_kill: 0,
        };
        let clone = cgroup.clone();

        cgroup.release_anchor().unwrap();
        assert!(cgroup.anchor.lock().unwrap().is_none());
        clone.release_anchor().unwrap();

        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(anchor_pid as libc::pid_t, &mut status, libc::WNOHANG) },
            -1,
            "anchor must already be reaped exactly once"
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn cgroup_anchor_release_retains_handle_when_reaping_fails() {
        let mut slot = Some(7_u8);
        let error = release_anchor_slot(&mut slot, |_| bail!("injected release failure"))
            .expect_err("release must fail");
        assert!(error.to_string().contains("injected release failure"));
        assert_eq!(slot, Some(7), "failed release must preserve ownership");
    }

    #[test]
    fn legacy_exit_info_remains_a_containment_proof() {
        let state = tempfile::tempdir().unwrap();
        let mut value = serde_json::to_value(liveness_record(state.path())).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("containment_cgroup");
        object.remove("containment_cgroup_identity");
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

        value
            .as_object_mut()
            .unwrap()
            .insert("containment_empty".into(), serde_json::json!(false));
        let explicit_failure: SessionRecord = serde_json::from_value(value.clone()).unwrap();
        assert!(!explicit_failure.containment_proven_empty());

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
    fn shell_env_unset_preserves_provider_overrides_and_configured_removals() {
        let config = load_config_text(
            "version = 1\n\
             [engines.shell]\n\
             command = [\"/bin/sh\", \"-l\"]\n\
             env_unset = [\"SHELL_SECRET\", \"SHELL_SECRET\"]\n",
        )
        .unwrap();
        let overrides = BTreeMap::from([
            ("OPENAI_API_KEY".into(), "literal-shell-value".into()),
            ("SHELL_SECRET".into(), "remove-me".into()),
        ]);
        let launch = config
            .resolve(
                vec!["/bin/true".into()],
                Some("shell"),
                None,
                Path::new("/tmp"),
                None,
                &overrides,
                &Limits::default(),
                None,
            )
            .unwrap();

        assert_eq!(
            launch.env.get("OPENAI_API_KEY").map(String::as_str),
            Some("literal-shell-value")
        );
        assert_eq!(launch.env_unset, vec!["SHELL_SECRET"]);
        assert!(!launch.env_unset.iter().any(|name| name == "OPENAI_API_KEY"));

        let agent = config
            .resolve(
                vec!["/bin/true".into()],
                Some("codex"),
                None,
                Path::new("/tmp"),
                None,
                &overrides,
                &Limits::default(),
                None,
            )
            .unwrap();
        assert!(agent.env_unset.iter().any(|name| name == "OPENAI_API_KEY"));
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

    #[test]
    fn executable_available_requires_execute_permission() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("tool");
        fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(!executable_available(program.to_str().unwrap()));

        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(executable_available(program.to_str().unwrap()));
    }

    #[test]
    fn explicit_relative_path_overrides_are_resolved_once() {
        let resolved = absolute_override_path(PathBuf::from("state"), "APLEXER_STATE_DIR").unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, env::current_dir().unwrap().join("state"));
    }

    #[test]
    fn xdg_paths_must_be_absolute() {
        let error = absolute_xdg_path(PathBuf::from("runtime"), "XDG_RUNTIME_DIR").unwrap_err();
        assert!(error.to_string().contains("must be an absolute path"));
        assert_eq!(
            absolute_xdg_path(PathBuf::from("/run/user/1000"), "XDG_RUNTIME_DIR").unwrap(),
            PathBuf::from("/run/user/1000")
        );
    }
}
