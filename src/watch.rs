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
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

/// How often to re-scan session metadata. `list_records` is a directory scan
/// over small JSON files -- spec.md section 30 calls this "milliseconds on
/// tens of sessions" -- so polling a few times a second is cheap even for
/// dozens of sessions. Chosen in the middle of the task's suggested
/// 500ms-1s range: fast enough that lifecycle events and state flips show up
/// with sub-second-to-one-second latency, slow enough to stay negligible
/// background load for a long-lived stream.
const POLL_INTERVAL: Duration = Duration::from_millis(750);

/// How long a session's PTY must stay silent before its derived
/// `agent.state` flips from `running` to `waiting`. Set to 4x POLL_INTERVAL
/// so a single late/missed poll tick, or output landing right at a poll
/// boundary, cannot flap the state back and forth -- a few polls of margin.
///
/// This is a COARSE, HONEST PROXY, not real agent-semantic-state detection.
/// spec.md section 20 explicitly defers true per-engine state derivation
/// (parsing claude/codex/gemini's own output, native agent state sources) to
/// future work; "PTY went quiet" also describes a long compute-bound step
/// with no terminal output, which this heuristic cannot tell apart from an
/// agent genuinely waiting on user input. Treat `waiting` as "no PTY output
/// recently", not as "the agent is idle".
const ACTIVITY_THRESHOLD_MS: u64 = 3_000;

/// heru's `UnifiedEvent` envelope (docs/pocketshell-integration-plan.md
/// Part 2, section 2.1 -- found in heru's real `heru/types.py`, not
/// inferred). Serialized with null/empty fields omitted, matching heru's own
/// `model_dump_json(exclude_none=True)` convention and this codebase's
/// existing `skip_serializing_if` pattern (see `SessionRecord`).
///
/// aplexer only ever emits `kind: "status"` or `kind: "error"` (lifecycle
/// events mapped onto heru's closed `kind` literal via `metadata.event`,
/// per the integration plan's "Option 1" -- no heru schema change needed).
/// Fields with no heru equivalent (`session_id`, `workspace`, `tag`,
/// `profile`, `session_kind`, `state`, `reason`, `generation`) ride in
/// `metadata`, which is contract-legal there (a flat dict of scalars).
#[derive(Debug, Clone, Serialize, Default)]
pub struct UnifiedEvent {
    pub kind: &'static str,
    pub engine: String,
    pub sequence: u64,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub usage_delta: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_id: Option<String>,
    /// aplexer's own native event object, mirroring heru's "original
    /// provider payload" semantics -- here aplexer is the provider.
    pub raw: Value,
    pub metadata: BTreeMap<String, Value>,
}

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

fn session_kind(record: &SessionRecord) -> &'static str {
    if record.engine == "shell" {
        "shell"
    } else {
        "agent"
    }
}

/// `starting/running/waiting/idle/exited/oom/error/unknown` is spec.md
/// section 20's full vocabulary; this heuristic only ever produces
/// `starting` (Phase::Starting), `running`/`waiting` (the PTY-recency
/// heuristic, see ACTIVITY_THRESHOLD_MS), `exited`, and `oom`/`error` for
/// the terminal phases -- `idle`/`unknown` are not used by this v1 proxy.
fn derive_agent_state(record: &SessionRecord, now: u64) -> &'static str {
    match record.phase {
        Phase::Starting => "starting",
        Phase::Running | Phase::Exiting => match record.last_activity_ms {
            Some(ts) if now.saturating_sub(ts) < ACTIVITY_THRESHOLD_MS => "running",
            Some(_) => "waiting",
            // No PTY output observed yet (e.g. worker just flipped to
            // Running but the periodic activity-persist tick hasn't fired):
            // assume running rather than waiting, since the session just
            // started and there is no evidence yet of it going quiet.
            None => "running",
        },
        Phase::Exited => {
            if record.exit.as_ref().map(|e| e.oom_killed).unwrap_or(false) {
                "oom"
            } else {
                "exited"
            }
        }
        Phase::Failed => "error",
    }
}

fn exit_reason(exit: Option<&ExitInfo>) -> &'static str {
    match exit {
        Some(e) if e.oom_killed => "killed",
        Some(e) if e.signal.is_some() => "signal",
        Some(_) => "exit",
        // A worker that reached Phase::Failed without ever recording an
        // ExitInfo (e.g. startup failure, or the `a kill` fallback path
        // that retires a broken session whose worker died without
        // recording the workload's own exit) is closer to "killed" than a
        // clean "exit".
        None => "killed",
    }
}

fn common_metadata(record: &SessionRecord, generation: u64) -> BTreeMap<String, Value> {
    let mut metadata = BTreeMap::new();
    metadata.insert("session_id".into(), json!(record.id.to_string()));
    metadata.insert(
        "workspace".into(),
        json!(record.workspace.display().to_string()),
    );
    metadata.insert("tag".into(), json!(record.tag.clone()));
    metadata.insert("generation".into(), json!(generation));
    metadata
}

fn next_sequence(sequence: &mut u64) -> u64 {
    let value = *sequence;
    *sequence += 1;
    value
}

fn make_created_event(record: &SessionRecord, generation: u64, sequence: &mut u64) -> UnifiedEvent {
    let mut metadata = common_metadata(record, generation);
    metadata.insert("event".into(), json!("session.created"));
    metadata.insert("session_kind".into(), json!(session_kind(record)));
    if let Some(profile) = &record.profile {
        metadata.insert("profile".into(), json!(profile));
    }
    let engine_profile = match &record.profile {
        Some(p) => format!("{}/{p}", record.engine),
        None => record.engine.clone(),
    };
    UnifiedEvent {
        kind: "status",
        engine: record.engine.clone(),
        sequence: next_sequence(sequence),
        timestamp: iso8601_utc(record.created_at_ms),
        content: format!(
            "created {}:{} ({engine_profile})",
            record.workspace.display(),
            record.tag
        ),
        raw: json!({"type":"session.created","id":record.id.to_string()}),
        metadata,
        ..Default::default()
    }
}

fn make_oom_event(
    record: &SessionRecord,
    exit: &ExitInfo,
    generation: u64,
    sequence: &mut u64,
) -> UnifiedEvent {
    let mut metadata = common_metadata(record, generation);
    metadata.insert("event".into(), json!("session.oom"));
    metadata.insert("resource".into(), json!("memory"));
    UnifiedEvent {
        kind: "error",
        engine: record.engine.clone(),
        sequence: next_sequence(sequence),
        timestamp: iso8601_utc(exit.exited_at_ms),
        error: Some("workload killed: cgroup memory limit".to_string()),
        raw: json!({"type":"session.oom","id":record.id.to_string()}),
        metadata,
        ..Default::default()
    }
}

fn make_exited_event(record: &SessionRecord, generation: u64, sequence: &mut u64) -> UnifiedEvent {
    let mut metadata = common_metadata(record, generation);
    metadata.insert("event".into(), json!("session.exited"));
    let reason = exit_reason(record.exit.as_ref());
    metadata.insert("reason".into(), json!(reason));
    let timestamp = record
        .exit
        .as_ref()
        .map(|e| e.exited_at_ms)
        .unwrap_or_else(now_ms);
    if let Some(exit) = &record.exit {
        if let Some(code) = exit.code {
            metadata.insert("exit_code".into(), json!(code));
        }
    }
    UnifiedEvent {
        kind: "status",
        engine: record.engine.clone(),
        sequence: next_sequence(sequence),
        timestamp: iso8601_utc(timestamp),
        content: format!("exited ({reason})"),
        raw: json!({"type":"session.exited","id":record.id.to_string(),"exit":record.exit}),
        metadata,
        ..Default::default()
    }
}

fn make_deleted_event(record: &SessionRecord, generation: u64, sequence: &mut u64) -> UnifiedEvent {
    let mut metadata = common_metadata(record, generation);
    metadata.insert("event".into(), json!("session.deleted"));
    UnifiedEvent {
        kind: "status",
        engine: record.engine.clone(),
        sequence: next_sequence(sequence),
        timestamp: iso8601_utc(now_ms()),
        content: format!("deleted {}:{}", record.workspace.display(), record.tag),
        raw: json!({"type":"session.deleted","id":record.id.to_string()}),
        metadata,
        ..Default::default()
    }
}

fn make_agent_state_event(
    record: &SessionRecord,
    state: &'static str,
    generation: u64,
    sequence: &mut u64,
) -> UnifiedEvent {
    let mut metadata = common_metadata(record, generation);
    metadata.insert("event".into(), json!("agent.state"));
    metadata.insert("state".into(), json!(state));
    UnifiedEvent {
        kind: "status",
        engine: record.engine.clone(),
        sequence: next_sequence(sequence),
        timestamp: iso8601_utc(now_ms()),
        content: state.to_string(),
        raw: json!({"type":"agent.state","id":record.id.to_string(),"state":state}),
        metadata,
        ..Default::default()
    }
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
    let new_state = derive_agent_state(current, now);
    if !is_new && new_state != ks.derived_state {
        events.push(make_agent_state_event(
            current, new_state, generation, sequence,
        ));
    }
    ks.derived_state = new_state;
    events
}

fn emit(out: &mut impl Write, event: &UnifiedEvent) -> Result<()> {
    let line = serde_json::to_string(event)?;
    writeln!(out, "{line}")?;
    out.flush()?;
    Ok(())
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
                emit(
                    &mut stdout,
                    &make_deleted_event(&ks.record, generation, &mut sequence),
                )?;
            }
        }

        for record in current {
            match known.get_mut(&record.id) {
                None => {
                    emit(
                        &mut stdout,
                        &make_created_event(&record, generation, &mut sequence),
                    )?;
                    let mut ks = KnownSession {
                        record: record.clone(),
                        derived_state: "starting",
                        exit_emitted: false,
                    };
                    for event in
                        transition_events(&mut ks, &record, now, generation, &mut sequence, true)
                    {
                        emit(&mut stdout, &event)?;
                    }
                    known.insert(record.id, ks);
                }
                Some(ks) => {
                    for event in
                        transition_events(ks, &record, now, generation, &mut sequence, false)
                    {
                        emit(&mut stdout, &event)?;
                    }
                    ks.record = record;
                }
            }
        }
    }
}

/// ISO-8601 UTC, second precision (e.g. `2026-08-26T12:00:00+00:00`), per
/// heru's `UnifiedEvent.timestamp` convention. Implemented from scratch
/// (Howard Hinnant's `civil_from_days` algorithm) rather than adding a
/// chrono/time dependency for one formatting function.
///
/// `pub(crate)` so `src/agent_events.rs` (native transcript parsing, a
/// different producer of the same `UnifiedEvent` envelope) can reuse it
/// instead of re-deriving the same formatting logic.
pub(crate) fn iso8601_utc(epoch_ms: u64) -> String {
    let secs = (epoch_ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}+00:00")
}

/// Days-since-unix-epoch to (year, month, day), UTC civil calendar. Public
/// domain algorithm by Howard Hinnant
/// (http://howardhinnant.github.io/date_algorithms.html#civil_from_days).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
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
        assert!(["starting", "running", "waiting", "exited", "oom", "error"]
            .contains(&derive_agent_state(&sample_record(Phase::Starting), 0)));
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
            phase,
            worker_pid: None,
            workload_pid: None,
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
