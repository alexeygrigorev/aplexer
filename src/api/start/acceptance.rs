//! Startup acceptance: deciding what a worker that already exited during
//! startup means for the start, plus the lineage stamp and the opt-in test
//! hook that pins the fast-workload interleaving.

use super::*;

/// Forces the fast-workload startup interleaving that `start_session`'s
/// readiness poll can otherwise only lose by chance: block until the worker
/// has finished its job, unlinked its control socket and exited, so the poll
/// loop in `await_worker_ready` can never observe a live socket to Ping.
///
/// Timing alone does not reproduce this on an idle machine -- 25+ repetitions
/// of the fast-workload test pass locally -- which is exactly how the
/// readiness-Ping gate reached a release with this ordering unhandled. The
/// non-default `startup-test-hooks` feature is the authorization boundary for
/// pinning it; default and release builds do not contain this path.
#[cfg(feature = "startup-test-hooks")]
pub(super) fn await_worker_exit_before_readiness_poll(
    startup: &mut LaunchGuard<'_>,
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
pub(super) fn exited_worker_completed_startup(record: &SessionRecord) -> bool {
    matches!(record.phase, Phase::Exiting | Phase::Exited)
        && record.exit.is_some()
        && record.containment_empty == Some(true)
}

/// What a worker that has already exited during startup means for the
/// start: a clean exit whose record is gone is a session that finished and
/// auto-removed itself (that deletion is the proof the lifecycle
/// completed); a clean exit that left a completed record
/// (`exited_worker_completed_startup`) did not fail to start either; every
/// other shape -- a crashed worker, an unreadable-but-present record, a
/// non-terminal one -- is a startup failure named by the exit status
/// rather than by a read error.
pub(super) fn exited_worker_outcome(
    paths: &Paths,
    id: Uuid,
    status: std::process::ExitStatus,
    last_seen: SessionRecord,
) -> Result<SessionRecord> {
    if status.success() {
        if !paths.record(id).exists() {
            return Ok(auto_removed_completion(last_seen));
        }
        if let Ok(final_record) = read_session_record(paths, id) {
            if exited_worker_completed_startup(&final_record) {
                return Ok(final_record);
            }
        }
    }
    bail!("worker exited during startup: {status}")
}

/// Reconstruct a start response for a worker that finished cleanly and
/// deleted its own record (natural exit, Ctrl-D, or an in-startup kill).
/// The on-disk record is already gone; this is only what `a start --json`
/// returns to the client that launched it.
pub(super) fn auto_removed_completion(mut record: SessionRecord) -> SessionRecord {
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
pub(super) fn resolve_parent_session(paths: &Paths) -> Option<Uuid> {
    let parent = crate::discover_session_id()?;
    read_session_record(paths, parent).map(|_| parent).ok()
}

#[cfg(test)]
mod startup_acceptance_tests {
    use super::*;
    use crate::ExitInfo;

    pub(super) fn startup_record(
        phase: Phase,
        exit: Option<ExitInfo>,
        containment_empty: Option<bool>,
    ) -> SessionRecord {
        let mut record = SessionRecord::fixture("/ws", "main");
        record.phase = phase;
        record.worker_pid = Some(1);
        record.containment_empty = containment_empty;
        record.exit = exit;
        record
    }

    /// `(description, phase, exit, containment_empty, expected_accept)`.
    type AcceptanceCase = (&'static str, Phase, Option<ExitInfo>, Option<bool>, bool);

    pub(super) fn clean_exit() -> Option<ExitInfo> {
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
    pub(super) fn exited_worker_startup_acceptance_matrix() {
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
