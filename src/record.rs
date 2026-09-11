//! The session record data model: schema and contract constants, tag/state
//! validation, `Phase` and the derived observed state, `SessionRecord` with
//! its write path and worker-start identity persistence, exit/containment
//! verdicts, and the recorded-worker signal path.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use uuid::Uuid;

use crate::{now_ms, process_alive};

mod identity;
mod reap;

pub use identity::signal_recorded_worker;
pub(crate) use identity::{
    persist_worker_identity_once, read_worker_identity, verify_worker_identity, WorkerIdentity,
};
pub use reap::{containment_reap_verdict, reap_verdict, ContainmentReap};
// The rest of the submodules' crate-visible surface is consumed by the
// crate's own test modules only, so a non-test build would otherwise call
// these re-exports unused.
#[allow(unused_imports)]
pub(crate) use identity::{ProcessIdentity, WORKER_IDENTITY_FILE};
#[allow(unused_imports)]
pub(crate) use reap::{containment_reap_verdict_with, recorded_cgroup_observed_empty};

pub const SCHEMA_VERSION: u32 = 1;

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

pub(crate) fn validate_limits(limits: &Limits, context: &str) -> Result<()> {
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

#[cfg(test)]
impl SessionRecord {
    /// The minimal valid record tests start from: a fresh id, `Running`,
    /// no pids, no containment proof, placeholder paths, a zero clock. A
    /// test sets the few fields it is actually about instead of restating
    /// all thirty in every fixture.
    pub(crate) fn fixture(workspace: impl Into<PathBuf>, tag: &str) -> Self {
        let workspace = workspace.into();
        Self {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id: Uuid::new_v4(),
            workspace: workspace.clone(),
            tag: tag.to_string(),
            engine: "shell".to_string(),
            profile: None,
            command: Vec::new(),
            cwd: workspace,
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
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
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(false),
            socket_path: PathBuf::from("/nonexistent"),
            history_path: PathBuf::from("/nonexistent"),
            exit: None,
            error: None,
        }
    }
}

/// Environment entries that are session metadata rather than launch
/// secrets. Transcript discovery needs these profile-specific roots after
/// the worker exits; every other launch value remains one-shot/private.
pub(crate) const SESSION_METADATA_ENV_KEYS: &[&str] =
    &["CLAUDE_CONFIG_DIR", "CODEX_HOME", "GROK_HOME"];

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
        match verify_worker_identity(&identity) {
            Ok(WorkerIdentity::Verified) => true,
            Ok(
                WorkerIdentity::Gone
                | WorkerIdentity::DifferentBoot
                | WorkerIdentity::PidReused { .. },
            ) => false,
            // Uncertainty fails closed: never let prune/tag replacement
            // delete a live worker over an unreadable probe.
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
