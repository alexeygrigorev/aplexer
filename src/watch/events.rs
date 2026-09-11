//! The `UnifiedEvent` envelope and the constructors the poll loop folds
//! each session lifecycle/state transition into.

use crate::*;
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;

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

fn session_kind(record: &SessionRecord) -> &'static str {
    if record.engine == "shell" {
        "shell"
    } else {
        "agent"
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
    let mut metadata = UnifiedEvent::session_metadata(record);
    metadata.insert("generation".into(), json!(generation));
    metadata
}

fn next_sequence(sequence: &mut u64) -> u64 {
    let value = *sequence;
    *sequence += 1;
    value
}

pub(super) fn make_created_event(
    record: &SessionRecord,
    generation: u64,
    sequence: &mut u64,
) -> UnifiedEvent {
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

pub(super) fn make_oom_event(
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

pub(super) fn make_exited_event(
    record: &SessionRecord,
    generation: u64,
    sequence: &mut u64,
) -> UnifiedEvent {
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

pub(super) fn make_deleted_event(
    record: &SessionRecord,
    generation: u64,
    sequence: &mut u64,
) -> UnifiedEvent {
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

pub(super) fn make_agent_state_event(
    record: &SessionRecord,
    state: &'static str,
    source: &'static str,
    generation: u64,
    sequence: &mut u64,
) -> UnifiedEvent {
    let mut metadata = common_metadata(record, generation);
    metadata.insert("event".into(), json!("agent.state"));
    metadata.insert("state".into(), json!(state));
    // "reported" (a fresh `a state-report` push, see fresh_reported_state)
    // or "heuristic" (the PTY-recency proxy) -- lets a consumer trust a
    // `waiting`/`idle` chip more when it knows a hook actually said so,
    // without hard-coding REPORTED_STATE_STALE_MS itself.
    metadata.insert("state_source".into(), json!(source));
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

impl UnifiedEvent {
    /// The session identity every aplexer-produced event carries in
    /// `metadata`: `session_id`, `workspace`, `tag`.
    pub fn session_metadata(record: &SessionRecord) -> BTreeMap<String, Value> {
        let mut metadata = BTreeMap::new();
        metadata.insert("session_id".into(), json!(record.id.to_string()));
        metadata.insert(
            "workspace".into(),
            json!(record.workspace.display().to_string()),
        );
        metadata.insert("tag".into(), json!(record.tag));
        metadata
    }

    /// Writes one line -- the JSON envelope, or the compact human
    /// rendering -- and flushes, so a consumer tailing the stream sees
    /// each event as soon as it exists.
    pub fn emit(&self, out: &mut impl Write, json_output: bool) -> Result<()> {
        if json_output {
            writeln!(out, "{}", serde_json::to_string(self)?)?;
        } else {
            writeln!(out, "{}", self.render_human())?;
        }
        out.flush()?;
        Ok(())
    }

    /// Compact one-line human rendering, used when `--json` is not passed
    /// -- matches the existing dual JSON/human convention (`a launch-spec`,
    /// `a status`, ...) rather than always forcing raw JSONL on a human
    /// reader.
    pub fn render_human(&self) -> String {
        match self.kind {
            "message" => format!(
                "[{}] {}",
                self.role.as_deref().unwrap_or(&self.engine),
                self.content
            ),
            "tool_call" => format!(
                "[tool_call] {}{}",
                self.tool_name.as_deref().unwrap_or("?"),
                self.tool_input
                    .as_deref()
                    .map(|i| format!(" {i}"))
                    .unwrap_or_default()
            ),
            "tool_result" => format!(
                "[tool_result] {}{}",
                self.tool_name.as_deref().unwrap_or(""),
                self.tool_output
                    .as_deref()
                    .map(|o| format!(" {}", truncate(o, 300)))
                    .unwrap_or_default()
            ),
            "usage" => format!("[usage] {:?}", self.usage_delta),
            "error" => format!("[error] {}", self.error.as_deref().unwrap_or("")),
            "continuation" => format!(
                "[continuation] {}",
                self.continuation_id.as_deref().unwrap_or("")
            ),
            other => format!("[{other}] {}", self.content),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
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
