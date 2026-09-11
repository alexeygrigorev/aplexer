//! `a watch --jsonl` -- a client-side poller over `list_records`, per
//! spec.md section 15's own guidance ("Start with direct metadata
//! scanning... Add a control process only when profiling demonstrates a
//! meaningful benefit"). There is no new worker RPC, no new socket, and no
//! central daemon here: this reads the same durable per-session
//! `session.json` records every other command reads, on a timer, and emits
//! one JSON line per detected change.
//!
//! The event envelope adopts heru's `UnifiedEvent` schema, per
//! docs/pocketshell-integration-plan.md's "Part 2 -- Common event format:
//! adopting heru's UnifiedEvent" (read that section for the full mapping
//! rationale; this module follows its event-by-event table directly rather
//! than re-deriving it). See spec.md section 19 for the event stream
//! sketch this fills in, and section 20 for the agent-state vocabulary.

use crate::*;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

mod events;
mod state;

pub(crate) use events::iso8601_utc;
pub use events::UnifiedEvent;
use events::{
    make_agent_state_event, make_created_event, make_deleted_event, make_exited_event,
    make_oom_event,
};
#[cfg(test)]
use serde_json::json;
use state::derive_agent_state;
pub use state::derive_agent_state_with_source;
#[cfg(test)]
use state::{
    fresh_reported_state, ACTIVITY_THRESHOLD_MS, IDLE_ACTIVITY_GRACE_MS, REPORTED_STATE_STALE_MS,
};

/// How often to re-scan session metadata. `list_records` is a directory scan
/// over small JSON files -- spec.md section 30 calls this "milliseconds on
/// tens of sessions" -- so polling a few times a second is cheap even for
/// dozens of sessions. Chosen in the middle of the task's suggested
/// 500ms-1s range: fast enough that lifecycle events and state flips show up
/// with sub-second-to-one-second latency, slow enough to stay negligible
/// background load for a long-lived stream.
const POLL_INTERVAL: Duration = Duration::from_millis(750);

/// Per-session state `a watch` tracks between polls; never persisted.
struct KnownSession {
    record: SessionRecord,
    derived_state: &'static str,
    /// Whether `session.oom`/`session.exited` has already been emitted for
    /// this session's terminal transition, so a session sitting in
    /// Exited/Failed phase across many polls (it can linger on disk for a
    /// while, e.g. until reclaimed by `a start` or cleaned up by `a kill`)
    /// doesn't get the same lifecycle event re-emitted every poll.
    exit_emitted: bool,
}

/// Only sessions whose engine is an actual agent engine are watched by
/// default -- explicit user scope decision: "watch works only for agents".
/// There is no separate `kind` field on `SessionRecord` distinguishing
/// agent/shell/process sessions yet, so `engine != "shell"` stands in for
/// "this is an agent session" (see `Config::load`'s built-in engine list:
/// shell, codex, claude, gemini, grok). `--all` opts back into shell
/// sessions too.
fn matches_filter(record: &SessionRecord, all: bool, workspace: Option<&Path>) -> bool {
    if !all && record.engine == "shell" {
        return false;
    }
    if let Some(ws) = workspace {
        if record.workspace != ws {
            return false;
        }
    }
    true
}

/// Computes (without emitting) the events for one session's transition from
/// its previously-known state to `current`, updating `ks` in place.
/// `is_new` suppresses the initial `agent.state` event for a session `a
/// watch` has just started tracking -- there is no previous poll's state to
/// have "changed" from, so the first observation seeds `derived_state`
/// silently rather than emitting a change event for it.
fn transition_events(
    ks: &mut KnownSession,
    current: &SessionRecord,
    now: u64,
    generation: u64,
    sequence: &mut u64,
    is_new: bool,
) -> Vec<UnifiedEvent> {
    let mut events = Vec::new();
    if !ks.exit_emitted && matches!(current.phase, Phase::Exited | Phase::Failed) {
        if let Some(exit) = &current.exit {
            if exit.oom_killed {
                events.push(make_oom_event(current, exit, generation, sequence));
            }
        }
        events.push(make_exited_event(current, generation, sequence));
        ks.exit_emitted = true;
    }
    let (new_state, source) = derive_agent_state_with_source(current, now);
    if !is_new && new_state != ks.derived_state {
        events.push(make_agent_state_event(
            current, new_state, source, generation, sequence,
        ));
    }
    ks.derived_state = new_state;
    events
}

/// Runs `a watch --jsonl` until interrupted (Ctrl-C / SIGINT). There is no
/// bounded/`--once` mode -- default SIGINT handling (immediate process
/// termination) is sufficient here since this holds no resources that need
/// cleanup on exit (no raw terminal mode, no PTY, just a poll loop writing
/// to stdout).
///
/// Startup behavior: sessions that already existed before `a watch` started
/// are seeded into the tracker WITHOUT emitting a synthetic
/// `session.created` for them -- `a watch` is a live tail of what happens
/// WHILE it runs, not a replay of history, mirroring how the event
/// generation/sequence counters here are scoped per-stream rather than
/// global (docs/pocketshell-integration-plan.md 2.3). A client that wants
/// the full existing inventory first should call `a snapshot --json`/`a list
/// --json`, then layer this incremental stream on top of that baseline --
/// the same snapshot-fallback pattern spec.md section 19 already asks for
/// gap detection to use.
pub fn run(paths: &Paths, all: bool, workspace: Option<&Path>) -> Result<()> {
    let mut known: BTreeMap<Uuid, KnownSession> = BTreeMap::new();
    let now = now_ms();
    for record in list_records(paths)?
        .into_iter()
        .filter(|r| matches_filter(r, all, workspace))
    {
        let derived_state = derive_agent_state(&record, now);
        let exit_emitted = matches!(record.phase, Phase::Exited | Phase::Failed);
        known.insert(
            record.id,
            KnownSession {
                record,
                derived_state,
                exit_emitted,
            },
        );
    }

    let mut sequence: u64 = 0;
    let mut generation: u64 = 0;
    let mut stdout = io::stdout();
    loop {
        std::thread::sleep(POLL_INTERVAL);
        generation += 1;
        let now = now_ms();
        let current: Vec<SessionRecord> = list_records(paths)?
            .into_iter()
            .filter(|r| matches_filter(r, all, workspace))
            .collect();
        let current_ids: BTreeSet<Uuid> = current.iter().map(|r| r.id).collect();

        let deleted_ids: Vec<Uuid> = known
            .keys()
            .filter(|id| !current_ids.contains(id))
            .copied()
            .collect();
        for id in deleted_ids {
            if let Some(ks) = known.remove(&id) {
                make_deleted_event(&ks.record, generation, &mut sequence)
                    .emit(&mut stdout, true)?;
            }
        }

        for record in current {
            match known.get_mut(&record.id) {
                None => {
                    make_created_event(&record, generation, &mut sequence)
                        .emit(&mut stdout, true)?;
                    let mut ks = KnownSession {
                        record: record.clone(),
                        derived_state: "starting",
                        exit_emitted: false,
                    };
                    for event in
                        transition_events(&mut ks, &record, now, generation, &mut sequence, true)
                    {
                        event.emit(&mut stdout, true)?;
                    }
                    known.insert(record.id, ks);
                }
                Some(ks) => {
                    for event in
                        transition_events(ks, &record, now, generation, &mut sequence, false)
                    {
                        event.emit(&mut stdout, true)?;
                    }
                    ks.record = record;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_epoch() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00+00:00");
    }

    #[test]
    fn iso8601_known_date() {
        // 2026-08-26T12:00:00Z
        let ms = 1_787_745_600_u64 * 1000;
        assert_eq!(iso8601_utc(ms), "2026-08-26T12:00:00+00:00");
    }

    #[test]
    fn agent_state_vocabulary() {
        assert!(
            ["starting", "running", "waiting", "idle", "exited", "oom", "error"]
                .contains(&derive_agent_state(&sample_record(Phase::Starting), 0))
        );
    }

    // -- a state-report merge/priority logic (fresh_reported_state /
    // derive_agent_state_with_source) --

    #[test]
    fn fresh_reported_state_covers_every_worker_validated_value() {
        // Ties REPORTED_AGENT_STATES (validated worker-side on write) to
        // fresh_reported_state's match (read watch-side) so the two cannot
        // silently drift -- a new value added to one without the other
        // would either be rejected at write time or silently ignored at
        // read time, and this test fails on either.
        for state in REPORTED_AGENT_STATES {
            let mut record = sample_record(Phase::Running);
            record.reported_state = Some(state.to_string());
            record.reported_state_at_ms = Some(1_000);
            assert!(
                fresh_reported_state(&record, 1_000).is_some(),
                "fresh_reported_state does not handle {state:?}, but the worker accepts it"
            );
        }
    }

    #[test]
    fn reported_working_state_wins_over_a_heuristic_that_would_say_waiting() {
        let mut record = sample_record(Phase::Running);
        // PTY has been silent well past ACTIVITY_THRESHOLD_MS -- the
        // heuristic alone would say "waiting".
        record.last_activity_ms = Some(0);
        record.reported_state = Some("working".to_string());
        record.reported_state_at_ms = Some(1_000);
        let now = 1_000 + ACTIVITY_THRESHOLD_MS + 1;
        assert_eq!(
            derive_agent_state_with_source(&record, now),
            ("running", "reported")
        );
    }

    #[test]
    fn reported_idle_state_is_a_new_value_the_heuristic_alone_never_produces() {
        let mut record = sample_record(Phase::Running);
        record.last_activity_ms = Some(0);
        record.reported_state = Some("idle".to_string());
        record.reported_state_at_ms = Some(1_000);
        assert_eq!(
            derive_agent_state_with_source(&record, 1_000),
            ("idle", "reported")
        );
    }

    #[test]
    fn reported_idle_outlasts_the_stale_window_while_the_pty_stays_quiet() {
        let mut record = sample_record(Phase::Running);
        // The rest began with output just before the push, and the PTY has
        // been silent since. However far past the working/waiting window
        // the poll lands, a rest has no follow-up push to refresh it --
        // expiring it anyway is what made resting sessions read RUNNING.
        record.last_activity_ms = Some(900);
        record.reported_state = Some("idle".to_string());
        record.reported_state_at_ms = Some(1_000);
        let much_later = 1_000 + REPORTED_STATE_STALE_MS + 60_000;
        assert_eq!(
            derive_agent_state_with_source(&record, much_later),
            ("idle", "reported")
        );
    }

    #[test]
    fn output_landing_within_the_grace_does_not_retract_a_reported_idle() {
        let mut record = sample_record(Phase::Running);
        // The turn's tail render racing the hook process: stamped after
        // the push but inside IDLE_ACTIVITY_GRACE_MS.
        record.reported_state = Some("idle".to_string());
        record.reported_state_at_ms = Some(1_000);
        record.last_activity_ms = Some(1_000 + IDLE_ACTIVITY_GRACE_MS);
        assert_eq!(
            derive_agent_state_with_source(&record, 1_000 + IDLE_ACTIVITY_GRACE_MS),
            ("idle", "reported")
        );
    }

    #[test]
    fn newer_pty_output_retracts_a_reported_idle() {
        let mut record = sample_record(Phase::Running);
        record.reported_state = Some("idle".to_string());
        record.reported_state_at_ms = Some(1_000);
        record.last_activity_ms = Some(1_000 + IDLE_ACTIVITY_GRACE_MS + 1);
        // Fresh output: the heuristic says running (the agent is producing
        // again, or the user just typed); either way not idle.
        assert_eq!(
            derive_agent_state_with_source(&record, 1_000 + IDLE_ACTIVITY_GRACE_MS + 1),
            ("running", "heuristic")
        );
        // The same retracted rest, once the output has itself gone quiet:
        // the heuristic's honest "waiting", never a stale idle claim.
        let quiet_again = 1_000 + IDLE_ACTIVITY_GRACE_MS + 1 + ACTIVITY_THRESHOLD_MS;
        assert_eq!(
            derive_agent_state_with_source(&record, quiet_again),
            ("waiting", "heuristic")
        );
    }

    #[test]
    fn reported_waiting_state_wins_even_over_fresh_pty_activity() {
        let mut record = sample_record(Phase::Running);
        // The heuristic alone would say "running": output just now.
        record.last_activity_ms = Some(1_000);
        record.reported_state = Some("waiting".to_string());
        record.reported_state_at_ms = Some(1_000);
        assert_eq!(
            derive_agent_state_with_source(&record, 1_000),
            ("waiting", "reported")
        );
    }

    #[test]
    fn reported_state_falls_back_to_heuristic_once_the_stale_window_elapses() {
        let mut record = sample_record(Phase::Running);
        record.last_activity_ms = Some(0); // heuristic: "waiting"
        record.reported_state = Some("working".to_string());
        record.reported_state_at_ms = Some(1_000);
        let still_fresh = 1_000 + REPORTED_STATE_STALE_MS;
        assert_eq!(
            derive_agent_state_with_source(&record, still_fresh),
            ("running", "reported"),
            "must still be authoritative at exactly the window boundary"
        );
        let now_stale = 1_000 + REPORTED_STATE_STALE_MS + 1;
        assert_eq!(
            derive_agent_state_with_source(&record, now_stale),
            ("waiting", "heuristic"),
            "must fall back to the PTY-recency heuristic once stale"
        );
    }

    #[test]
    fn reported_state_never_overrides_a_terminal_phase() {
        let mut record = sample_record(Phase::Exited);
        record.reported_state = Some("working".to_string());
        record.reported_state_at_ms = Some(1_000);
        // Fresh by every measure, but the session already exited.
        assert_eq!(
            derive_agent_state_with_source(&record, 1_000),
            ("exited", "heuristic")
        );
    }

    #[test]
    fn unrecognised_reported_state_falls_back_to_heuristic() {
        // Simulates a foreign/hand-edited session.json -- the worker
        // itself never writes anything outside REPORTED_AGENT_STATES.
        let mut record = sample_record(Phase::Running);
        record.last_activity_ms = Some(1_000);
        record.reported_state = Some("bogus".to_string());
        record.reported_state_at_ms = Some(1_000);
        assert_eq!(
            derive_agent_state_with_source(&record, 1_000),
            ("running", "heuristic")
        );
    }

    #[test]
    fn no_reported_state_runs_the_heuristic_unmodified() {
        let mut record = sample_record(Phase::Running);
        record.last_activity_ms = Some(0);
        assert_eq!(
            derive_agent_state_with_source(&record, ACTIVITY_THRESHOLD_MS + 1),
            ("waiting", "heuristic")
        );
    }

    /// A real cgroup-level OOM kill on the workload's own PTY-owning process
    /// (as opposed to a subprocess inside a surviving shell) is a narrow
    /// kernel-timing race to reproduce on demand -- confirmed by hand while
    /// validating this feature: the cgroup's own oom_kill_count reliably
    /// increments (verified live via `a status`'s cgroup stats, same as
    /// tests/oom_isolation.rs's own methodology), but the session's
    /// ExitInfo::oom_killed flag did not reliably flip true in that
    /// particular environment before the tracked workload was reaped. That
    /// race lives in worker.rs's pre-existing Cgroup::oom_killed()/
    /// spawn_lifecycle exit-detection path, not in this module. This test
    /// instead pins the *mapping* deterministically: whenever a session's
    /// persisted record does say `exit.oom_killed == true`, `a watch` must
    /// emit `session.oom` (kind "error") immediately before `session.exited`,
    /// per docs/pocketshell-integration-plan.md's event table.
    #[test]
    fn oom_exit_emits_error_kind_before_exited() {
        let mut record = sample_record(Phase::Exited);
        record.exit = Some(ExitInfo {
            code: None,
            signal: Some(9),
            oom_killed: true,
            exited_at_ms: 1_787_745_600_000,
        });
        let mut ks = KnownSession {
            record: record.clone(),
            derived_state: "running",
            exit_emitted: false,
        };
        let mut sequence = 0;
        let events =
            transition_events(&mut ks, &record, 1_787_745_601_000, 1, &mut sequence, false);
        assert_eq!(
            events.len(),
            3,
            "expected oom + exited + agent.state: {events:?}"
        );
        assert_eq!(events[0].kind, "error");
        assert_eq!(
            events[0].metadata.get("event").unwrap(),
            &json!("session.oom")
        );
        assert_eq!(
            events[0].error.as_deref(),
            Some("workload killed: cgroup memory limit")
        );
        assert_eq!(events[1].kind, "status");
        assert_eq!(
            events[1].metadata.get("event").unwrap(),
            &json!("session.exited")
        );
        assert_eq!(events[1].metadata.get("reason").unwrap(), &json!("killed"));
        assert_eq!(events[2].metadata.get("state").unwrap(), &json!("oom"));
        assert!(ks.exit_emitted);
    }

    fn sample_record(phase: Phase) -> SessionRecord {
        SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id: Uuid::nil(),
            workspace: "/tmp".into(),
            tag: "t".into(),
            engine: "claude".into(),
            profile: None,
            command: vec!["claude".into()],
            cwd: "/tmp".into(),
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
            phase,
            worker_pid: None,
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(false),
            socket_path: "/tmp/s".into(),
            history_path: "/tmp/h".into(),
            exit: None,
            error: None,
        }
    }
}
