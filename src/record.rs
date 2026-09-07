//! The session record data model: schema and contract constants, tag/state
//! validation, `Phase` and the derived observed state, `SessionRecord` with
//! its write path and worker-start identity persistence, exit/containment
//! verdicts, and the recorded-worker signal path.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use uuid::Uuid;

use crate::persist::TEMP_COUNTER;
use crate::{
    cgroup_path_populated, linux_boot_id, now_ms, pidfd_open, process_alive,
    process_start_time_ticks, validate_recorded_cgroup,
};

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
pub(crate) fn containment_reap_verdict_with(
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
pub(crate) fn recorded_cgroup_observed_empty(
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
pub(crate) struct ProcessIdentity {
    pub(crate) pid: u32,
    pub(crate) start_time_ticks: u64,
    pub(crate) boot_id: String,
}

pub(crate) const WORKER_IDENTITY_FILE: &str = "worker.identity.json";

pub(crate) fn read_worker_identity(record: &SessionRecord) -> Result<Option<ProcessIdentity>> {
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
pub(crate) fn persist_worker_identity_once(path: &Path, value: &Value) -> Result<()> {
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
