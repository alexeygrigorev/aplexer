//! Containment-reap verdicts: the one decision table every command that
//! destroys a record's durable state answers.

use super::{CgroupIdentity, SessionRecord};
use crate::{cgroup_path_populated, linux_boot_id, validate_recorded_cgroup};
use anyhow::Result;
use std::path::Path;
use uuid::Uuid;

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
