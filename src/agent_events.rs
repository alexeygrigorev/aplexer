//! Parse an aplexer session's native engine conversation log into heru
//! `UnifiedEvent` JSONL (`a transcript`). PocketShell's conversation pane
//! is the intended consumer -- last-N for the initial view, `--before` for
//! older pages, `--after` plus `--follow` for live tail.
//!
//! This is a different layer from `a watch`: `a watch` derives coarse
//! host-level session-lifecycle events (created/exited/oom / a
//! running-vs-waiting heuristic) by polling `session.json`. It never looks
//! at what an agent is saying. This module parses the conversation the
//! agent CLI already writes to disk during an ordinary interactive
//! `a start`/`a attach` session.
//!
//! Capture and keep (deliberate, not a copy of the PTY history):
//!
//! - **Source of truth** is the engine's own append-only JSONL, not aplexer
//!   state. We do not duplicate conversation bytes under the session dir
//!   (that would go stale, double disk, and fight the engine's own log).
//!   PTY `history.bin` stays the raw terminal capture; this is the
//!   structured conversation.
//! - **Location** is a heuristic the first time: aplexer has the session's
//!   cwd + created_at, not the engine-native session id. Rules are ported
//!   from pocketshell's `agent_log.py` (`~/.claude/projects/<encoded-cwd>/`,
//!   `~/.codex/sessions/<Y>/<M>/<D>/`, `$GROK_HOME/sessions/<urlencoded-cwd>/`).
//! - **Bind**: once located, the path is written to
//!   `<state>/sessions/<id>/transcript.json` so later reads and `--follow`
//!   hit the same file even if a second session shares the cwd. The bind
//!   is a sidecar, not a `SessionRecord` field, so the worker's periodic
//!   `last_activity_ms` writes cannot race it away.
//! - **Re-locate** if the bound path disappears (agent rotated the log).
//!
//! Supported engines: claude, codex, grok. `shell`/`gemini`/`opencode` have
//! no reader here yet. Claude's native `.jsonl` is the Anthropic Messages
//! API event shape (same functions as heru's claude adapter). Codex's
//! native rollout (`response_item`) is a different shape from `codex exec
//! --json` -- only the native shape is parsed, because this is not a
//! headless launcher. Grok Build writes ACP `updates.jsonl`.
//!
//! Known, deliberate departures from heru's Python source:
//!
//! - heru's claude `live_events()` has an unreachable top-level
//!   `tool_result` branch; real logs nest tool results inside
//!   `{"type":"user","message":{"content":[{"type":"tool_result",...}]}}`.
//!   This keeps the dead branch and unwraps the real `"user"` shape, and
//!   also emits user **text** turns from that same `"user"` event (needed
//!   for PocketShell display; heru's live adapter was assistant-centric).
//! - Claude `content_block_delta` chunks AND the later complete
//!   `"assistant"` message both become `message` events -- heru's behavior,
//!   ported faithfully. A consumer that wants only the final text should
//!   skip the small deltas.

use crate::watch::{iso8601_utc, UnifiedEvent};
use crate::{atomic_write_json, now_ms, Result, SessionRecord};
use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

/// Byte-identical to PocketShell's `LINE_TRUNCATION_SENTINEL` so a client
/// that already recognises the marker can render a truncation chip instead
/// of feeding an oversized line to a parser.
const LINE_TRUNCATION_SENTINEL: &str = "@@PS_LINE_TRUNCATED@@";

const FOLLOW_POLL: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------
// JSONL payload assembly -- tolerant of one-JSON-object-per-line AND of a
// JSON object split across multiple lines (heru's `_codex_impl.py::
// iter_codex_payloads` brace/bracket/string balance tracking).
// ---------------------------------------------------------------------

#[derive(Default)]
struct JsonAssembler {
    buffer: String,
    braces: i64,
    brackets: i64,
    in_string: bool,
    escaped: bool,
}

impl JsonAssembler {
    /// Feed one line (no trailing newline). Returns a complete JSON value
    /// once enough lines have been buffered to balance braces/brackets
    /// outside of any string, or `None` while still accumulating.
    fn feed(&mut self, line: &str) -> Option<Value> {
        let trimmed = line.trim();
        if trimmed.is_empty() && self.buffer.is_empty() {
            return None;
        }
        if !self.buffer.is_empty() {
            self.buffer.push('\n');
        }
        self.buffer.push_str(line);
        self.update_balance(line);
        if self.is_complete() {
            let text = std::mem::take(&mut self.buffer);
            self.braces = 0;
            self.brackets = 0;
            self.in_string = false;
            self.escaped = false;
            return serde_json::from_str::<Value>(text.trim()).ok();
        }
        None
    }

    fn is_complete(&self) -> bool {
        !self.in_string && !self.escaped && self.braces == 0 && self.brackets == 0
    }

    fn update_balance(&mut self, text: &str) {
        for ch in text.chars() {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if ch == '\\' {
                    self.escaped = true;
                } else if ch == '"' {
                    self.in_string = false;
                }
                continue;
            }
            match ch {
                '"' => self.in_string = true,
                '{' => self.braces += 1,
                '}' => self.braces -= 1,
                '[' => self.brackets += 1,
                ']' => self.brackets -= 1,
                _ => {}
            }
        }
    }
}

fn ev(kind: &'static str) -> UnifiedEvent {
    UnifiedEvent {
        kind,
        // heru's `UnifiedEvent.raw` defaults to `{}` (`Field(default_factory
        // =dict)`), not null -- `Value`'s own `Default` is `Null`.
        raw: json!({}),
        ..Default::default()
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

fn int_meta(map: &mut BTreeMap<String, Value>, key: &str, v: &Value) {
    if let Some(n) = v.get(key).and_then(|x| x.as_i64()) {
        map.insert(key.to_string(), json!(n));
    }
}

// ---------------------------------------------------------------------
// Claude native JSONL (Anthropic Messages API event shape).
// ---------------------------------------------------------------------

/// `unwrap_stream_event`: partial-delta lines arrive wrapped as
/// `{"type":"stream_event","event":{...}}`; unwrap to the inner event.
/// Full-message lines (`assistant`/`user`/`result`/`system`/`error`) are
/// not wrapped and pass through unchanged.
fn claude_unwrap(payload: &Value) -> &Value {
    payload.get("event").filter(|e| e.is_object()).unwrap_or(payload)
}

fn claude_final_messages(payload: &Value) -> Vec<String> {
    let event_type = payload.get("type").and_then(|t| t.as_str());
    let mut out = Vec::new();
    match event_type {
        Some("assistant") => {
            if let Some(content) = payload.get("message").and_then(|m| m.get("content")) {
                if let Some(blocks) = content.as_array() {
                    for block in blocks {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                if !text.is_empty() {
                                    out.push(text.to_string());
                                }
                            }
                        }
                    }
                } else if let Some(text) = content.as_str() {
                    if !text.is_empty() {
                        out.push(text.to_string());
                    }
                }
            }
        }
        Some("result") => {
            if let Some(text) = payload.get("result").and_then(|r| r.as_str()) {
                if !text.is_empty() {
                    out.push(text.to_string());
                }
            }
        }
        _ => {}
    }
    out
}

fn claude_tool_result_event(engine: &str, block: &Value) -> UnifiedEvent {
    let tool_output = match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    };
    let mut e = ev("tool_result");
    e.engine = engine.to_string();
    e.role = Some("user".to_string());
    e.tool_output = Some(tool_output);
    if let Some(id) = str_field(block, "tool_use_id") {
        e.metadata.insert("tool_use_id".into(), json!(id));
    }
    e
}

fn claude_user_text_event(engine: &str, text: &str) -> UnifiedEvent {
    let mut e = ev("message");
    e.engine = engine.to_string();
    e.role = Some("user".to_string());
    e.content = text.to_string();
    e
}

/// Ported `_claude_impl.py::live_events`, PLUS the real-shape `"user"`
/// unwrap (tool_result AND user text -- PocketShell needs both).
fn claude_wire_events(engine: &str, payload: &Value) -> Vec<UnifiedEvent> {
    let unwrapped = claude_unwrap(payload);
    let event_type = unwrapped.get("type").and_then(|t| t.as_str());
    let mut out = Vec::new();
    match event_type {
        Some("content_block_delta") => {
            if let Some(delta) = unwrapped.get("delta") {
                if delta.get("type").and_then(|t| t.as_str()) == Some("text_delta") {
                    if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            let mut e = ev("message");
                            e.engine = engine.to_string();
                            e.role = Some("assistant".to_string());
                            e.content = text.to_string();
                            out.push(e);
                        }
                    }
                }
            }
        }
        Some("content_block_start") => {
            if let Some(block) = unwrapped.get("content_block") {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    if let Some(name) = str_field(block, "name") {
                        let mut e = ev("tool_call");
                        e.engine = engine.to_string();
                        e.role = Some("assistant".to_string());
                        e.tool_name = Some(name);
                        e.tool_input = block
                            .get("input")
                            .filter(|i| i.is_object() || i.is_array())
                            .map(|i| i.to_string());
                        out.push(e);
                    }
                }
            }
        }
        Some("tool_result") => {
            let content = unwrapped.get("content");
            let tool_output = match content {
                Some(Value::String(s)) => s.clone(),
                Some(v) if !v.is_null() => v.to_string(),
                _ => String::new(),
            };
            let mut e = ev("tool_result");
            e.engine = engine.to_string();
            e.role = Some("user".to_string());
            e.tool_output = Some(tool_output);
            out.push(e);
        }
        Some("user") => {
            match unwrapped.get("message").and_then(|m| m.get("content")) {
                Some(Value::String(s)) if !s.is_empty() => {
                    out.push(claude_user_text_event(engine, s));
                }
                Some(Value::Array(blocks)) => {
                    for block in blocks {
                        match block.get("type").and_then(|t| t.as_str()) {
                            Some("tool_result") => out.push(claude_tool_result_event(engine, block)),
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    if !text.is_empty() {
                                        out.push(claude_user_text_event(engine, text));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        Some("assistant") => {
            for message in claude_final_messages(unwrapped) {
                let mut e = ev("message");
                e.engine = engine.to_string();
                e.role = Some("assistant".to_string());
                e.content = message;
                out.push(e);
            }
        }
        Some("result") => {
            for message in claude_final_messages(unwrapped) {
                let mut e = ev("message");
                e.engine = engine.to_string();
                e.role = Some("assistant".to_string());
                e.content = message;
                out.push(e);
            }
            if let Some(usage) = unwrapped.get("usage").filter(|u| u.is_object()) {
                let mut meta = BTreeMap::new();
                int_meta(&mut meta, "input_tokens", usage);
                int_meta(&mut meta, "output_tokens", usage);
                let mut e = ev("usage");
                e.engine = engine.to_string();
                e.usage_delta = meta;
                out.push(e);
            }
        }
        Some("error") => {
            let message = unwrapped
                .get("data")
                .and_then(|d| d.get("message"))
                .and_then(|m| m.as_str())
                .or_else(|| unwrapped.get("message").and_then(|m| m.as_str()));
            if let Some(message) = message {
                if !message.is_empty() {
                    let mut e = ev("error");
                    e.engine = engine.to_string();
                    e.error = Some(message.to_string());
                    out.push(e);
                }
            }
        }
        _ => {}
    }
    out
}

/// Ported `claude_continuation`: `{"type":"system","subtype":"init",
/// "session_id":"..."}`.
fn claude_wire_continuation(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("system") {
        return None;
    }
    if payload.get("subtype").and_then(|t| t.as_str()) != Some("init") {
        return None;
    }
    str_field(payload, "session_id")
}

// ---------------------------------------------------------------------
// Codex NATIVE rollout transcript (`~/.codex/sessions/.../<id>.jsonl`).
// Parses only `response_item` rows (the raw per-turn model log) to avoid
// double-counting against the separate `event_msg` progress-notification
// rows, which mirror the same content.
// ---------------------------------------------------------------------

fn codex_native_text_parts(content: &Value, allowed: &[&str]) -> Vec<String> {
    match content {
        Value::String(s) => {
            let t = s.trim();
            if t.is_empty() {
                Vec::new()
            } else {
                vec![t.to_string()]
            }
        }
        Value::Object(_) => {
            let block_type = content.get("type").and_then(|t| t.as_str());
            if let Some(bt) = block_type {
                if !allowed.contains(&bt) {
                    return Vec::new();
                }
            }
            if let Some(text) = content.get("text").and_then(|t| t.as_str()) {
                let t = text.trim();
                if !t.is_empty() {
                    return vec![t.to_string()];
                }
            }
            content
                .get("content")
                .map(|c| codex_native_text_parts(c, allowed))
                .unwrap_or_default()
        }
        Value::Array(items) => items
            .iter()
            .flat_map(|i| codex_native_text_parts(i, allowed))
            .collect(),
        _ => Vec::new(),
    }
}

fn codex_native_events(payload: &Value) -> Vec<UnifiedEvent> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return Vec::new();
    }
    let Some(item) = payload.get("payload") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match item.get("type").and_then(|t| t.as_str()) {
        Some("message") => {
            let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("");
            if role == "user" || role == "assistant" {
                let parts = codex_native_text_parts(
                    item.get("content").unwrap_or(&Value::Null),
                    &["input_text", "output_text", "text"],
                );
                let text = parts.join("\n\n");
                if !text.is_empty() {
                    let mut e = ev("message");
                    e.engine = "codex".to_string();
                    e.role = Some(role.to_string());
                    e.content = text;
                    out.push(e);
                }
            }
        }
        Some("custom_tool_call") => {
            let mut e = ev("tool_call");
            e.engine = "codex".to_string();
            e.role = Some("assistant".to_string());
            e.tool_name = str_field(item, "name");
            e.tool_input = item
                .get("input")
                .and_then(|i| i.as_str())
                .map(str::to_string)
                .or_else(|| item.get("input").map(|i| i.to_string()));
            out.push(e);
        }
        Some("custom_tool_call_output") => {
            let parts = codex_native_text_parts(
                item.get("output").unwrap_or(&Value::Null),
                &["input_text", "output_text", "text"],
            );
            let text = parts.join("\n\n");
            if !text.is_empty() {
                let mut e = ev("tool_result");
                e.engine = "codex".to_string();
                e.tool_output = Some(text);
                out.push(e);
            }
        }
        // "reasoning" and other response_item shapes: no stable text field
        // to surface (codex's reasoning items ship only encrypted content
        // on this CLI version) -- deliberately skipped, not an omission bug.
        _ => {}
    }
    out
}

/// `{"type":"session_meta","payload":{"id":"<thread-id>",...}}` -- the
/// codex rollout's own thread/session identifier (matches the `thread_id`
/// carried by every later `event_msg` row in the same file).
fn codex_native_continuation(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    payload.get("payload").and_then(|p| str_field(p, "id"))
}

/// The codex rollout's own working directory, from the same `session_meta`
/// row -- used by `locate_codex_transcript` to disambiguate candidate files
/// beyond the mtime heuristic.
fn codex_native_cwd(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    payload.get("payload").and_then(|p| str_field(p, "cwd"))
}

// ---------------------------------------------------------------------
// Grok Build ACP `updates.jsonl` (`session/update` rows). Field mapping
// follows pocketshell's `GrokBuildParser.kt`.
// ---------------------------------------------------------------------

fn grok_chunk_text(update: &Value) -> Option<String> {
    match update.get("content") {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Object(obj)) => obj
            .get("text")
            .and_then(|t| t.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

fn grok_tool_result_text(update: &Value) -> String {
    if let Some(items) = update.get("content").and_then(|c| c.as_array()) {
        let mut parts = Vec::new();
        for item in items {
            let inner = item.get("content").unwrap_or(item);
            if let Some(text) = inner.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    match update.get("rawOutput") {
        Some(Value::String(s)) => s.clone(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    }
}

fn grok_native_events(payload: &Value) -> Vec<UnifiedEvent> {
    let Some(params) = payload.get("params") else {
        return Vec::new();
    };
    let Some(update) = params.get("update") else {
        return Vec::new();
    };
    let Some(kind) = update.get("sessionUpdate").and_then(|k| k.as_str()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    match kind {
        "user_message_chunk" => {
            if let Some(text) = grok_chunk_text(update) {
                let mut e = ev("message");
                e.engine = "grok".to_string();
                e.role = Some("user".to_string());
                e.content = text;
                out.push(e);
            }
        }
        "agent_message_chunk" => {
            if let Some(text) = grok_chunk_text(update) {
                let mut e = ev("message");
                e.engine = "grok".to_string();
                e.role = Some("assistant".to_string());
                e.content = text;
                out.push(e);
            }
        }
        "tool_call" => {
            let mut e = ev("tool_call");
            e.engine = "grok".to_string();
            e.role = Some("assistant".to_string());
            e.tool_name = str_field(update, "title").or_else(|| Some("tool".into()));
            e.tool_input = update.get("rawInput").map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            });
            if let Some(id) = str_field(update, "toolCallId") {
                e.metadata.insert("tool_call_id".into(), json!(id));
            }
            out.push(e);
        }
        "tool_call_update" => {
            let status = update
                .get("status")
                .and_then(|s| s.as_str())
                .map(|s| s.to_ascii_lowercase());
            if status.as_deref() != Some("completed") && status.is_some() {
                return out;
            }
            let output = grok_tool_result_text(update);
            if output.is_empty() && status.as_deref() != Some("completed") {
                return out;
            }
            let mut e = ev("tool_result");
            e.engine = "grok".to_string();
            e.tool_output = Some(output);
            if let Some(id) = str_field(update, "toolCallId") {
                e.metadata.insert("tool_call_id".into(), json!(id));
            }
            out.push(e);
        }
        _ => {}
    }
    out
}

fn grok_native_continuation(payload: &Value) -> Option<String> {
    payload
        .get("params")
        .and_then(|p| str_field(p, "sessionId"))
}

fn grok_row_timestamp(payload: &Value) -> String {
    if let Some(n) = payload.get("timestamp").and_then(|t| t.as_i64()) {
        let ms = if n < 10_000_000_000 { (n as u64) * 1000 } else { n as u64 };
        return iso8601_utc(ms);
    }
    if let Some(n) = payload
        .get("params")
        .and_then(|p| p.get("_meta"))
        .and_then(|m| m.get("agentTimestampMs"))
        .and_then(|t| t.as_u64())
    {
        return iso8601_utc(n);
    }
    String::new()
}

// ---------------------------------------------------------------------
// Wire format dispatch (native logs only -- there is no headless exec).
// ---------------------------------------------------------------------

#[derive(Clone, Copy)]
enum WireFormat {
    Claude,
    CodexNative,
    Grok,
}

fn wire_format_for(engine: &str) -> Result<WireFormat> {
    match engine {
        "claude" => Ok(WireFormat::Claude),
        "codex" => Ok(WireFormat::CodexNative),
        "grok" => Ok(WireFormat::Grok),
        other => bail!(
            "a transcript supports claude, codex, and grok only (got engine {other})"
        ),
    }
}

fn translate(format: WireFormat, engine: &str, payload: &Value) -> (Vec<UnifiedEvent>, Option<String>) {
    match format {
        WireFormat::Claude => (
            claude_wire_events(engine, payload),
            claude_wire_continuation(payload),
        ),
        WireFormat::CodexNative => (codex_native_events(payload), codex_native_continuation(payload)),
        WireFormat::Grok => (grok_native_events(payload), grok_native_continuation(payload)),
    }
}

fn payload_to_raw(payload: &Value) -> Value {
    if payload.is_object() {
        payload.clone()
    } else {
        json!({"value": payload})
    }
}

fn row_timestamp(format: WireFormat, payload: &Value) -> String {
    match format {
        WireFormat::Grok => grok_row_timestamp(payload),
        WireFormat::Claude | WireFormat::CodexNative => payload
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

fn stamp_session(event: &mut UnifiedEvent, record: &SessionRecord) {
    event
        .metadata
        .insert("session_id".into(), json!(record.id.to_string()));
    event.metadata.insert(
        "workspace".into(),
        json!(record.workspace.display().to_string()),
    );
    event.metadata.insert("tag".into(), json!(record.tag));
    if let Some(profile) = &record.profile {
        event.metadata.insert("profile".into(), json!(profile));
    }
}

fn emit(out: &mut impl Write, event: &UnifiedEvent, json_output: bool) -> Result<()> {
    if json_output {
        writeln!(out, "{}", serde_json::to_string(event)?)?;
    } else {
        writeln!(out, "{}", render_human(event))?;
    }
    out.flush()?;
    Ok(())
}

/// Compact one-line human rendering, used when `--json` is not passed --
/// matches the existing dual JSON/human convention (`a launch-spec`,
/// `a status`, ...) rather than always forcing raw JSONL on a human reader.
pub fn render_human(event: &UnifiedEvent) -> String {
    match event.kind {
        "message" => format!(
            "[{}] {}",
            event.role.as_deref().unwrap_or(&event.engine),
            event.content
        ),
        "tool_call" => format!(
            "[tool_call] {}{}",
            event.tool_name.as_deref().unwrap_or("?"),
            event
                .tool_input
                .as_deref()
                .map(|i| format!(" {i}"))
                .unwrap_or_default()
        ),
        "tool_result" => format!(
            "[tool_result] {}{}",
            event.tool_name.as_deref().unwrap_or(""),
            event
                .tool_output
                .as_deref()
                .map(|o| format!(" {}", truncate(o, 300)))
                .unwrap_or_default()
        ),
        "usage" => format!("[usage] {:?}", event.usage_delta),
        "error" => format!("[error] {}", event.error.as_deref().unwrap_or("")),
        "continuation" => format!(
            "[continuation] {}",
            event.continuation_id.as_deref().unwrap_or("")
        ),
        other => format!("[{other}] {}", event.content),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

// ---------------------------------------------------------------------
// Location + bind sidecar.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TranscriptBind {
    path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    engine_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LocatedTranscript {
    pub path: PathBuf,
    pub engine_session_id: Option<String>,
}

/// Locate (or reuse the bound path of) the native log for `record`.
/// Writes `<state>/sessions/<id>/transcript.json` on a successful first
/// find so `--follow` and later pages do not re-run the cwd/mtime heuristic.
pub fn resolve_transcript(record: &SessionRecord, bind_path: &Path) -> Result<LocatedTranscript> {
    if let Ok(bytes) = fs::read(bind_path) {
        if let Ok(bind) = serde_json::from_slice::<TranscriptBind>(&bytes) {
            if bind.path.is_file() {
                return Ok(LocatedTranscript {
                    path: bind.path,
                    engine_session_id: bind.engine_session_id,
                });
            }
        }
    }
    let path = locate_transcript(
        &record.engine,
        &record.cwd,
        record.created_at_ms,
        &record.env,
    )
    .ok_or_else(|| {
        anyhow!(
            "no {} transcript found for session {} (cwd {}); the agent may not have written anything yet",
            record.engine,
            record.id,
            record.cwd.display()
        )
    })?;
    let engine_session_id = peek_continuation(&record.engine, &path);
    let bind = TranscriptBind {
        path: path.clone(),
        engine_session_id: engine_session_id.clone(),
    };
    // Best-effort: a bind write failing must not hide a successful locate.
    let _ = atomic_write_json(bind_path, &bind);
    Ok(LocatedTranscript {
        path,
        engine_session_id,
    })
}

pub fn locate_transcript(
    engine: &str,
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    match engine {
        "claude" => locate_claude_transcript(cwd, created_at_ms, env),
        "codex" => locate_codex_transcript(cwd, created_at_ms, env),
        "grok" => locate_grok_transcript(cwd, created_at_ms, env),
        _ => None,
    }
}

fn peek_continuation(engine: &str, path: &Path) -> Option<String> {
    let format = wire_format_for(engine).ok()?;
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut assembler = JsonAssembler::default();
    for line in reader.lines().map_while(std::io::Result::ok).take(64) {
        let Some(payload) = assembler.feed(&line) else {
            continue;
        };
        let (_events, continuation) = translate(format, engine, &payload);
        if continuation.is_some() {
            return continuation;
        }
    }
    None
}

/// Claude Code: `~/.claude/projects/<encoded-cwd>/<session>.jsonl`, where
/// `<encoded-cwd>` replaces every `/` with `-` (`agent_log.py::
/// _encode_claude_cwd`). aplexer has no direct handle on the underlying
/// claude session id, only the aplexer session's own `cwd` and
/// `created_at_ms` -- so this picks the most-recently-modified `*.jsonl`
/// directly under that cwd's project directory whose mtime is not earlier
/// than the aplexer session's creation (with a few seconds of slack for
/// startup ordering). This is a heuristic, not an exact session-id match:
/// if two aplexer claude sessions share the exact same cwd and are both
/// live, the bind sidecar is what keeps later reads on the first-found
/// file. Documented, not silently assumed.
pub fn locate_claude_transcript(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    let config_dir = env
        .get("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))?;
    let encoded = cwd.display().to_string().replace('/', "-");
    let dir = config_dir.join("projects").join(encoded);
    if !dir.is_dir() {
        return None;
    }
    let since = created_at_ms.saturating_sub(5_000);
    best_candidate(&dir, since, false)
}

/// Codex: `~/.codex/sessions/<YYYY>/<MM>/<DD>/<session>.jsonl`, date-
/// partitioned so the tree is walked rather than computed directly
/// (`agent_log.py::_resolve_codex_path`). Each rollout file's first line is
/// a `session_meta` row carrying its own `cwd`, which lets this disambiguate
/// candidates precisely rather than relying on mtime alone.
pub fn locate_codex_transcript(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    let root = env
        .get("CODEX_HOME")
        .map(|h| PathBuf::from(h).join("sessions"))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex/sessions")))?;
    if !root.is_dir() {
        return None;
    }
    let since = created_at_ms.saturating_sub(5_000);
    let cwd_str = cwd.display().to_string();
    let mut best: Option<(u64, PathBuf)> = None;
    walk_jsonl(&root, &mut |path, mtime_ms| {
        if mtime_ms < since {
            return;
        }
        if let Ok(file) = File::open(path) {
            let mut reader = BufReader::new(file);
            let mut first_line = String::new();
            if std::io::BufRead::read_line(&mut reader, &mut first_line).unwrap_or(0) > 0 {
                if let Ok(payload) = serde_json::from_str::<Value>(first_line.trim()) {
                    if codex_native_cwd(&payload).as_deref() != Some(cwd_str.as_str()) {
                        return;
                    }
                }
            }
        }
        if best.as_ref().map(|(m, _)| mtime_ms > *m).unwrap_or(true) {
            best = Some((mtime_ms, path.to_path_buf()));
        }
    });
    best.map(|(_, p)| p)
}

/// Grok Build: `$GROK_HOME/sessions/<urlencoded-cwd>/<session-id>/updates.jsonl`
/// (default `GROK_HOME` is `~/.grok`). Percent-encoding matches
/// `urllib.parse.quote(cwd, safe="")` in pocketshell's `agent_log.py`.
pub fn locate_grok_transcript(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    let root = grok_sessions_root(env)?;
    if !root.is_dir() {
        return None;
    }
    let since = created_at_ms.saturating_sub(5_000);
    let encoded = encode_grok_cwd(&cwd.display().to_string());
    let project = root.join(&encoded);
    // Stay inside this cwd's encoded directory. Walking every grok session
    // tree would bind an unrelated live session (this agent's own
    // updates.jsonl is the usual false match).
    if project.is_dir() {
        return best_grok_updates(&project, since);
    }
    None
}

fn grok_sessions_root(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(home) = env.get("GROK_HOME").cloned().or_else(|| std::env::var("GROK_HOME").ok()) {
        return Some(PathBuf::from(home).join("sessions"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".grok/sessions"))
}

fn encode_grok_cwd(cwd: &str) -> String {
    // urllib.parse.quote(cwd, safe="") -- RFC 3986 unreserved
    // (ALPHA / DIGIT / "-" / "." / "_" / "~") stay literal; everything
    // else, including `/`, is percent-encoded. `safe=""` only *adds*
    // extra unencoded bytes; it does not encode `-_.~`.
    let trimmed = cwd.trim();
    let trimmed = if trimmed.is_empty() { "/" } else { trimmed };
    let mut out = String::new();
    for b in trimmed.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn best_grok_updates(project_dir: &Path, since_ms: u64) -> Option<PathBuf> {
    let entries = fs::read_dir(project_dir).ok()?;
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in entries.flatten() {
        let candidate = entry.path().join("updates.jsonl");
        if !candidate.is_file() {
            continue;
        }
        let Some(mtime) = file_mtime_ms(&candidate) else {
            continue;
        };
        if mtime < since_ms {
            continue;
        }
        if best.as_ref().map(|(m, _)| mtime > *m).unwrap_or(true) {
            best = Some((mtime, candidate));
        }
    }
    best.map(|(_, p)| p)
}

fn file_mtime_ms(path: &Path) -> Option<u64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as u64)
}

fn walk_jsonl(dir: &Path, visit: &mut impl FnMut(&Path, u64)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, visit);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Some(mtime_ms) = file_mtime_ms(&path) {
                visit(&path, mtime_ms);
            }
        }
    }
}

/// Picks the most-recently-modified direct `*.jsonl` child of `dir` with
/// mtime `>= since_ms`. `recurse` is unused today (claude's project dirs
/// also contain a `subagents/` subdirectory which is deliberately NOT
/// walked -- those are sub-agent transcripts, not the top-level session).
fn best_candidate(dir: &Path, since_ms: u64, recurse: bool) -> Option<PathBuf> {
    let _ = recurse;
    let entries = fs::read_dir(dir).ok()?;
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(mtime_ms) = file_mtime_ms(&path) else {
            continue;
        };
        if mtime_ms < since_ms {
            continue;
        }
        if best.as_ref().map(|(m, _)| mtime_ms > *m).unwrap_or(true) {
            best = Some((mtime_ms, path));
        }
    }
    best.map(|(_, p)| p)
}

// ---------------------------------------------------------------------
// Incremental native-log reader, pagination, follow.
// ---------------------------------------------------------------------

/// PocketShell-facing query: last-N initial view, `--before` older page,
/// `--after` catch-up cursor, `--follow` live tail.
#[derive(Debug, Clone, Default)]
pub struct TranscriptQuery {
    pub last: Option<usize>,
    pub kind: Option<String>,
    pub after: Option<u64>,
    pub before: Option<u64>,
    pub follow: bool,
    pub max_line_bytes: Option<usize>,
}

struct NativeLogReader {
    path: PathBuf,
    engine: String,
    format: WireFormat,
    assembler: JsonAssembler,
    sequence: u64,
    byte_offset: u64,
    pending: String,
    max_line_bytes: Option<usize>,
}

impl NativeLogReader {
    fn open(engine: &str, path: &Path, max_line_bytes: Option<usize>) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            engine: engine.to_string(),
            format: wire_format_for(engine)?,
            assembler: JsonAssembler::default(),
            sequence: 0,
            byte_offset: 0,
            pending: String::new(),
            max_line_bytes,
        })
    }

    fn reset(&mut self) {
        self.assembler = JsonAssembler::default();
        self.sequence = 0;
        self.byte_offset = 0;
        self.pending.clear();
    }

    /// Read newly appended complete lines. `consume_tail` is true for a
    /// one-shot snapshot (last line may lack a trailing newline) and false
    /// for `--follow` (wait for a newline so a mid-write row is not parsed
    /// as truncated JSON).
    fn read_available(
        &mut self,
        record: &SessionRecord,
        consume_tail: bool,
    ) -> Result<Vec<UnifiedEvent>> {
        let mut file = File::open(&self.path)
            .with_context(|| format!("read {}", self.path.display()))?;
        let len = file.metadata()?.len();
        if len < self.byte_offset {
            self.reset();
        }
        file.seek(SeekFrom::Start(self.byte_offset))?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        if buf.is_empty() && self.pending.is_empty() {
            return Ok(Vec::new());
        }
        let chunk = String::from_utf8_lossy(&buf);
        let mut data = std::mem::take(&mut self.pending);
        data.push_str(&chunk);

        let has_trailing_newline = data.ends_with('\n') || data.ends_with('\r');
        let mut lines: Vec<&str> = data.split('\n').collect();
        let remainder = if !has_trailing_newline && !consume_tail {
            lines.pop().unwrap_or("").to_string()
        } else {
            if let Some(last) = lines.last() {
                if last.is_empty() {
                    lines.pop();
                }
            }
            String::new()
        };
        let consumed = data.len() - remainder.len();
        self.byte_offset += consumed as u64;
        self.pending = remainder;

        let mut events = Vec::new();
        for line in lines {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            if let Some(max) = self.max_line_bytes {
                let byte_len = line.len();
                if byte_len > max {
                    let mut e = ev("error");
                    e.engine = self.engine.clone();
                    e.error = Some(format!("{LINE_TRUNCATION_SENTINEL}{byte_len}"));
                    e.sequence = self.sequence;
                    self.sequence += 1;
                    e.timestamp = iso8601_utc(now_ms());
                    stamp_session(&mut e, record);
                    events.push(e);
                    continue;
                }
            }
            let Some(payload) = self.assembler.feed(line) else {
                continue;
            };
            let ts = row_timestamp(self.format, &payload);
            let (drafted, _continuation) = translate(self.format, &self.engine, &payload);
            for mut event in drafted {
                event.engine = self.engine.clone();
                event.sequence = self.sequence;
                self.sequence += 1;
                event.timestamp = ts.clone();
                event.raw = payload_to_raw(&payload);
                stamp_session(&mut event, record);
                events.push(event);
            }
        }
        Ok(events)
    }
}

pub fn paginate(mut events: Vec<UnifiedEvent>, query: &TranscriptQuery) -> Vec<UnifiedEvent> {
    if let Some(kind) = &query.kind {
        events.retain(|e| e.kind == kind);
    }
    if let Some(after) = query.after {
        events.retain(|e| e.sequence > after);
    }
    if let Some(before) = query.before {
        events.retain(|e| e.sequence < before);
    }
    if let Some(n) = query.last {
        if events.len() > n {
            events = events.split_off(events.len() - n);
        }
    }
    events
}

/// One-shot page, or a page followed by a live tail of the same file.
pub fn run_transcript(
    record: &SessionRecord,
    path: &Path,
    query: TranscriptQuery,
    json_output: bool,
) -> Result<()> {
    let mut reader = NativeLogReader::open(&record.engine, path, query.max_line_bytes)?;
    let mut stdout = std::io::stdout();
    let snapshot = reader.read_available(record, true)?;
    let page = paginate(snapshot, &query);
    for event in &page {
        if emit(&mut stdout, event, json_output).is_err() {
            return Ok(());
        }
    }
    if !query.follow {
        return Ok(());
    }
    let mut after = page.last().map(|e| e.sequence).or(query.after);
    loop {
        thread::sleep(FOLLOW_POLL);
        let more = reader.read_available(record, false)?;
        let follow_query = TranscriptQuery {
            last: None,
            before: None,
            after,
            kind: query.kind.clone(),
            follow: true,
            max_line_bytes: query.max_line_bytes,
        };
        let page = paginate(more, &follow_query);
        for event in &page {
            if emit(&mut stdout, event, json_output).is_err() {
                return Ok(());
            }
            after = Some(event.sequence);
        }
    }
}

/// Reads and parses one transcript file into `UnifiedEvent`s, in file
/// order, sequence-numbered from 0. Kept for unit tests and any in-process
/// caller that does not need `--follow`.
pub fn read_transcript_events(engine: &str, path: &Path) -> Result<Vec<UnifiedEvent>> {
    let dummy = dummy_record(engine);
    let mut reader = NativeLogReader::open(engine, path, None)?;
    reader.read_available(&dummy, true)
}

fn dummy_record(engine: &str) -> SessionRecord {
    SessionRecord {
        schema_version: crate::SCHEMA_VERSION,
        id: uuid::Uuid::nil(),
        workspace: PathBuf::from("/"),
        tag: "t".into(),
        engine: engine.to_string(),
        profile: None,
        command: vec![engine.to_string()],
        cwd: PathBuf::from("/"),
        env: Default::default(),
        env_unset: Default::default(),
        limits: Default::default(),
        history_bytes: 0,
        created_at_ms: 0,
        updated_at_ms: 0,
        last_activity_ms: None,
        phase: crate::Phase::Running,
        worker_pid: None,
        workload_pid: None,
        socket_path: PathBuf::from("/"),
        history_path: PathBuf::from("/"),
        exit: None,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn json_assembler_single_line() {
        let mut a = JsonAssembler::default();
        let v = a.feed(r#"{"type":"assistant"}"#).unwrap();
        assert_eq!(v.get("type").unwrap(), "assistant");
    }

    #[test]
    fn json_assembler_multi_line() {
        let mut a = JsonAssembler::default();
        assert!(a.feed("{\"type\":\"assistant\",").is_none());
        let v = a.feed("\"x\":1}").unwrap();
        assert_eq!(v.get("x").unwrap(), 1);
    }

    #[test]
    fn claude_content_block_delta_maps_to_message() {
        let payload: Value = serde_json::from_str(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}"#,
        )
        .unwrap();
        let events = claude_wire_events("claude", &payload);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "message");
        assert_eq!(events[0].content, "hi");
        assert_eq!(events[0].role.as_deref(), Some("assistant"));
    }

    #[test]
    fn claude_tool_use_maps_to_tool_call() {
        let payload: Value = serde_json::from_str(
            r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Bash","input":{"command":"ls"}}}}"#,
        )
        .unwrap();
        let events = claude_wire_events("claude", &payload);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "tool_call");
        assert_eq!(events[0].tool_name.as_deref(), Some("Bash"));
        assert!(events[0].tool_input.as_deref().unwrap().contains("ls"));
    }

    #[test]
    fn claude_user_tool_result_unwrap() {
        let payload: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"output text"}]}}"#,
        )
        .unwrap();
        let events = claude_wire_events("claude", &payload);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "tool_result");
        assert_eq!(events[0].tool_output.as_deref(), Some("output text"));
    }

    #[test]
    fn claude_user_text_message() {
        let payload: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"please review"}]}}"#,
        )
        .unwrap();
        let events = claude_wire_events("claude", &payload);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "message");
        assert_eq!(events[0].role.as_deref(), Some("user"));
        assert_eq!(events[0].content, "please review");
    }

    #[test]
    fn claude_continuation_from_system_init() {
        let payload: Value = serde_json::from_str(
            r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#,
        )
        .unwrap();
        assert_eq!(claude_wire_continuation(&payload).as_deref(), Some("abc-123"));
    }

    #[test]
    fn codex_native_message_response_item() {
        let payload: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}"#,
        )
        .unwrap();
        let events = codex_native_events(&payload);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "message");
        assert_eq!(events[0].role.as_deref(), Some("assistant"));
        assert_eq!(events[0].content, "hello");
    }

    #[test]
    fn codex_native_tool_call_and_result() {
        let call: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"ls"}}"#,
        )
        .unwrap();
        let events = codex_native_events(&call);
        assert_eq!(events[0].kind, "tool_call");
        assert_eq!(events[0].tool_name.as_deref(), Some("exec"));

        let output: Value = serde_json::from_str(
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":[{"type":"input_text","text":"ok"}]}}"#,
        )
        .unwrap();
        let events = codex_native_events(&output);
        assert_eq!(events[0].kind, "tool_result");
        assert_eq!(events[0].tool_output.as_deref(), Some("ok"));
    }

    #[test]
    fn codex_native_continuation_from_session_meta() {
        let payload: Value = serde_json::from_str(
            r#"{"type":"session_meta","payload":{"id":"thread-abc","cwd":"/tmp/x"}}"#,
        )
        .unwrap();
        assert_eq!(
            codex_native_continuation(&payload).as_deref(),
            Some("thread-abc")
        );
        assert_eq!(codex_native_cwd(&payload).as_deref(), Some("/tmp/x"));
    }

    #[test]
    fn grok_user_and_agent_chunks() {
        let user: Value = serde_json::from_str(
            r#"{"params":{"sessionId":"s1","update":{"sessionUpdate":"user_message_chunk","content":{"text":"hi"}}}}"#,
        )
        .unwrap();
        let events = grok_native_events(&user);
        assert_eq!(events[0].kind, "message");
        assert_eq!(events[0].role.as_deref(), Some("user"));
        assert_eq!(events[0].content, "hi");

        let agent: Value = serde_json::from_str(
            r#"{"params":{"update":{"sessionUpdate":"agent_message_chunk","content":"hello back"}}}"#,
        )
        .unwrap();
        let events = grok_native_events(&agent);
        assert_eq!(events[0].role.as_deref(), Some("assistant"));
        assert_eq!(events[0].content, "hello back");
    }

    #[test]
    fn grok_tool_call_and_completed_result() {
        let call: Value = serde_json::from_str(
            r#"{"params":{"update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Read","rawInput":{"path":"a.rs"}}}}"#,
        )
        .unwrap();
        let events = grok_native_events(&call);
        assert_eq!(events[0].kind, "tool_call");
        assert_eq!(events[0].tool_name.as_deref(), Some("Read"));

        let result: Value = serde_json::from_str(
            r#"{"params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"content":{"text":"ok"}}]}}}"#,
        )
        .unwrap();
        let events = grok_native_events(&result);
        assert_eq!(events[0].kind, "tool_result");
        assert_eq!(events[0].tool_output.as_deref(), Some("ok"));
    }

    #[test]
    fn paginate_last_after_before() {
        let mk = |seq: u64, kind: &'static str| UnifiedEvent {
            kind,
            sequence: seq,
            engine: "claude".into(),
            timestamp: String::new(),
            raw: json!({}),
            ..Default::default()
        };
        let events = vec![
            mk(0, "message"),
            mk(1, "tool_call"),
            mk(2, "message"),
            mk(3, "message"),
            mk(4, "usage"),
        ];
        let last = paginate(
            events.clone(),
            &TranscriptQuery {
                last: Some(2),
                ..Default::default()
            },
        );
        assert_eq!(last.iter().map(|e| e.sequence).collect::<Vec<_>>(), vec![3, 4]);

        let after = paginate(
            events.clone(),
            &TranscriptQuery {
                after: Some(1),
                kind: Some("message".into()),
                ..Default::default()
            },
        );
        assert_eq!(after.iter().map(|e| e.sequence).collect::<Vec<_>>(), vec![2, 3]);

        let older = paginate(
            events,
            &TranscriptQuery {
                before: Some(4),
                last: Some(2),
                kind: Some("message".into()),
                ..Default::default()
            },
        );
        assert_eq!(older.iter().map(|e| e.sequence).collect::<Vec<_>>(), vec![2, 3]);
    }

    #[test]
    fn encode_grok_cwd_matches_python_quote_safe_empty() {
        assert_eq!(
            encode_grok_cwd("/home/alexey/git/aplexer"),
            "%2Fhome%2Falexey%2Fgit%2Faplexer"
        );
        assert_eq!(
            encode_grok_cwd("/tmp/aplexer-tx-test"),
            "%2Ftmp%2Faplexer-tx-test"
        );
    }

    #[test]
    fn read_transcript_events_claude_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"one"}]}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"two"}]}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"three"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let events = read_transcript_events("claude", &path).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].role.as_deref(), Some("user"));
        assert_eq!(events[0].content, "one");
        assert_eq!(events[2].content, "three");
        assert_eq!(events[2].sequence, 2);
    }

    #[test]
    fn bind_sidecar_reuses_path() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        std::fs::write(&log, "{}\n").unwrap();
        let bind = dir.path().join("transcript.json");
        let record = dummy_record("claude");
        // First locate would fail (HOME is not this dir); write a bind first.
        atomic_write_json(
            &bind,
            &TranscriptBind {
                path: log.clone(),
                engine_session_id: Some("x".into()),
            },
        )
        .unwrap();
        let located = resolve_transcript(&record, &bind).unwrap();
        assert_eq!(located.path, log);
        assert_eq!(located.engine_session_id.as_deref(), Some("x"));
    }

    #[test]
    fn max_line_bytes_emits_truncation_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        let huge = format!(r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#, "x".repeat(200));
        std::fs::write(&path, format!("{huge}\n")).unwrap();
        let record = dummy_record("claude");
        let mut reader = NativeLogReader::open("claude", &path, Some(50)).unwrap();
        let events = reader.read_available(&record, true).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "error");
        assert!(events[0]
            .error
            .as_deref()
            .unwrap()
            .starts_with(LINE_TRUNCATION_SENTINEL));
    }

    #[test]
    fn follow_reader_picks_up_appended_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let record = dummy_record("claude");
        let mut reader = NativeLogReader::open("claude", &path, None).unwrap();
        let first = reader.read_available(&record, true).unwrap();
        assert_eq!(first.len(), 1);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(
                concat!(
                    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"yo"}]}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .unwrap();
        let second = reader.read_available(&record, false).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].content, "yo");
        assert_eq!(second[0].sequence, 1);
    }
}
