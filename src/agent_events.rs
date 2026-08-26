//! Headless agent-invocation streaming (`a exec`) and native-transcript
//! tailing (`a transcript`) -- both emit the same `UnifiedEvent` envelope
//! `src/watch.rs` already defined for `a watch --jsonl`
//! (docs/pocketshell-integration-plan.md Part 2 / heru's `UnifiedEvent`).
//!
//! This is a DIFFERENT, complementary capability from `a watch`: `a watch`
//! derives coarse host-level session-lifecycle events (created/exited/
//! oom/a running-vs-waiting heuristic) by polling `session.json` records --
//! it never looks at what an agent is actually saying or doing. This module
//! ports heru's real, working Python adapters (`heru/base.py`,
//! `heru/adapters/{claude,codex}.py` + their `_*_impl.py` field-mapping
//! helpers, read in full at `/home/alexey/git/heru` commit `7278523`) so
//! aplexer can parse an agent's own conversation -- messages, tool calls,
//! tool results, usage, continuation ids -- into the SAME envelope.
//!
//! Two independent sources feed the same parsers:
//!
//! - **`a exec`**: invokes `claude`/`codex`/`grok` in their own documented
//!   HEADLESS/non-interactive mode (`codex exec --json`, `claude --print
//!   --output-format stream-json`, `grok -p --output-format
//!   streaming-messages-json`) as a plain child process (NOT a PTY -- see
//!   `run_exec`'s doc comment for why) and streams its stdout JSONL,
//!   translated live, until the run completes.
//! - **`a transcript`**: locates and parses the NATIVE, PERSISTED JSONL
//!   conversation log an agent CLI writes during a completely ordinary
//!   INTERACTIVE session (`~/.claude/projects/<encoded-cwd>/<session>.jsonl`,
//!   `~/.codex/sessions/<Y>/<M>/<D>/<session>.jsonl`) -- no special launch
//!   mode needed at all, since aplexer's actual common case is a long-lived
//!   `a start`/`a attach` PTY session, not a one-shot headless invocation.
//!   Ported from `/home/alexey/git/pocketshell/tools/pocketshell/src/
//!   pocketshell/agent_log.py`'s path-resolution rules (read in full).
//!
//! Claude's headless wire format and its native transcript format are
//! VERIFIED BYTE-IDENTICAL in shape (both are the raw `claude --print
//! --output-format stream-json` per-line event shape -- confirmed by
//! inspecting a real `~/.claude/projects/.../*.jsonl` file on this machine),
//! so one set of translate functions (`claude_wire_events`) serves `a exec
//! --engine claude`, `a exec --engine grok` (grok's `streaming-messages-json`
//! headless format is the Anthropic Messages API wire format too -- verified
//! with a real `grok -p ... --output-format streaming-messages-json`
//! invocation), AND `a transcript --engine claude`. Codex's headless
//! (`item.completed`/`agent_message`) and native (`response_item`/
//! `event_msg` rollout envelope) formats are GENUINELY DIFFERENT wire
//! shapes -- ported as two separate functions, `codex_exec_events` and
//! `codex_native_events`.
//!
//! Known, deliberate departures from heru's Python source (found while
//! validating against real CLI output on this machine, documented rather
//! than silently "fixed" out from under the port):
//!
//! - heru's claude `live_events()` has an unreachable `tool_result` branch:
//!   it matches a top-level `{"type":"tool_result",...}` event, but real
//!   `claude --print --output-format stream-json` output never emits that
//!   shape -- tool results arrive nested as `{"type":"user","message":
//!   {"content":[{"type":"tool_result",...}]}}` (the Anthropic Messages API
//!   convention). This port keeps heru's (dead) top-level branch for fidelity
//!   AND additionally unwraps the real `"user"` shape, so `a exec --engine
//!   claude`/`grok` actually surfaces `tool_result` events end to end.
//! - heru's codex `command_execution` mapping assumes `item.command` is
//!   always a JSON array and reads `command[0]` as the tool name; real
//!   `codex exec --json` output on this machine emits `item.command` as a
//!   plain string (`"/bin/bash -lc ls"`). This port accepts both shapes.
//! - Claude's partial `content_block_delta` text chunks AND the later,
//!   complete top-level `"assistant"` message BOTH translate to `message`-
//!   kind events -- this is heru's actual behavior (ported faithfully, not a
//!   bug here): a consumer wanting only the final text should read the
//!   larger, later `message` events per turn, not sum every delta.

use crate::watch::{iso8601_utc, UnifiedEvent};
use crate::{now_ms, Result};
use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

// ---------------------------------------------------------------------
// JSONL payload assembly -- tolerant of one-JSON-object-per-line (the
// common case for all three engines observed on this machine) AND of a
// JSON object split across multiple lines (heru's `_codex_impl.py::
// iter_codex_payloads` defends against this with brace/bracket/string
// balance tracking; ported here as `JsonAssembler` and used uniformly
// for all engines/sources since it is a strict superset of "one line is
// already valid JSON").
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
        // =dict)`), not null -- `Value`'s own `Default` is `Null`, so this
        // is set explicitly here rather than left to `..Default::default()`.
        // Callers that have a real native payload overwrite this before the
        // event is emitted (see `run_exec`/`read_transcript_events`); the
        // batched-at-the-end `continuation` event is the one case with no
        // single native payload to attach, and keeps this `{}`.
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
// Claude wire format (heru's `_claude_impl.py::live_events` +
// `claude_continuation`) -- also used for grok's headless
// `streaming-messages-json`, verified byte-shape-identical, and for
// `a transcript --engine claude`'s native `.jsonl` (verified identical).
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

/// Ported `_claude_impl.py::live_events`, PLUS the real-shape `"user"`
/// tool_result unwrap documented in the module doc comment above.
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
        // Ported as-is from heru (unreachable against real output on this
        // machine, kept for fidelity -- see module doc comment).
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
        // Deliberate addition over heru: real headless/native output nests
        // tool_result blocks inside a top-level "user" message.
        Some("user") => {
            if let Some(blocks) = unwrapped
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for block in blocks {
                    if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
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
                        out.push(e);
                    }
                }
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
// Codex headless (`codex exec --json`) -- ported `_codex_impl.py::
// codex_live_events` + `codex_continuation`.
// ---------------------------------------------------------------------

fn codex_command_tool_name(item: &Value) -> Option<String> {
    match item.get("command") {
        Some(Value::Array(arr)) => arr.first().and_then(|v| v.as_str()).map(str::to_string),
        // Deliberate addition over heru: real `codex exec --json` output on
        // this machine emits `command` as a plain string, not an array.
        Some(Value::String(s)) => Some(s.clone()),
        _ => None,
    }
}

fn codex_exec_events(payload: &Value) -> Vec<UnifiedEvent> {
    let event_type = payload.get("type").and_then(|t| t.as_str());
    let mut out = Vec::new();
    match event_type {
        Some("item.completed") => {
            if let Some(item) = payload.get("item") {
                match item.get("type").and_then(|t| t.as_str()) {
                    Some("agent_message") => {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                let mut e = ev("message");
                                e.engine = "codex".to_string();
                                e.role = Some("assistant".to_string());
                                e.content = text.to_string();
                                out.push(e);
                            }
                        }
                    }
                    Some("command_execution") => {
                        let mut e = ev("tool_result");
                        e.engine = "codex".to_string();
                        e.tool_name = codex_command_tool_name(item);
                        e.tool_output = item
                            .get("aggregated_output")
                            .and_then(|o| o.as_str())
                            .map(str::to_string);
                        if let Some(code) = item.get("exit_code").and_then(|c| c.as_i64()) {
                            e.metadata.insert("exit_code".into(), json!(code));
                        }
                        out.push(e);
                    }
                    _ => {}
                }
            }
        }
        Some("turn.completed") => {
            if let Some(usage) = payload.get("usage").filter(|u| u.is_object()) {
                let mut meta = BTreeMap::new();
                int_meta(&mut meta, "input_tokens", usage);
                int_meta(&mut meta, "output_tokens", usage);
                int_meta(&mut meta, "total_tokens", usage);
                let mut e = ev("usage");
                e.engine = "codex".to_string();
                e.usage_delta = meta;
                out.push(e);
            }
        }
        Some("error") | Some("turn.failed") => {
            if let Some(message) = payload.get("message").and_then(|m| m.as_str()) {
                if !message.trim().is_empty() {
                    let mut e = ev("error");
                    e.engine = "codex".to_string();
                    e.error = Some(message.trim().to_string());
                    out.push(e);
                }
            }
        }
        _ => {}
    }
    out
}

fn codex_exec_continuation(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("thread.started") {
        return None;
    }
    str_field(payload, "thread_id")
}

// ---------------------------------------------------------------------
// Codex NATIVE rollout transcript (`~/.codex/sessions/.../<id>.jsonl`) --
// no heru equivalent; genuinely different wire shape from `exec --json`
// (see module doc comment). Parses only `response_item` rows (the raw
// per-turn model log) to avoid double-counting against the separate
// `event_msg` progress-notification rows, which mirror the same content.
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
// `a exec` -- headless one-shot invocation.
// ---------------------------------------------------------------------

/// Which wire format a payload should be run through. `a exec` always uses
/// the `*Exec` variants; `a transcript` always uses the `*Native` variants
/// (Claude shares one variant for both, since the shapes are identical).
#[derive(Clone, Copy)]
pub enum WireFormat {
    ClaudeOrGrok,
    CodexExec,
    CodexNative,
}

fn translate(format: WireFormat, engine: &str, payload: &Value) -> (Vec<UnifiedEvent>, Option<String>) {
    match format {
        WireFormat::ClaudeOrGrok => (
            claude_wire_events(engine, payload),
            claude_wire_continuation(payload),
        ),
        WireFormat::CodexExec => (codex_exec_events(payload), codex_exec_continuation(payload)),
        WireFormat::CodexNative => (codex_native_events(payload), codex_native_continuation(payload)),
    }
}

/// Builds the headless/structured-output argv for one engine. `base` is the
/// engine's own configured command (normally just `["codex"]`/`["claude"]`/
/// `["grok"]`, but may carry extra leading args from a user's profile
/// override) -- reused from `Config::resolve` rather than hard-coding the
/// binary name, matching `a launch-spec`/`a launch-exec`'s reuse pattern.
///
/// Each engine's flags are the ones its own `--help` documents for
/// non-interactive structured output, confirmed working on this machine:
/// `codex exec --json`, `claude --print --output-format stream-json`,
/// `grok -p --output-format streaming-messages-json`. None of these need a
/// PTY -- see `run_exec`'s doc comment.
pub fn build_headless_argv(
    engine: &str,
    base: &[String],
    prompt: &str,
    resume: Option<&str>,
    model: Option<&str>,
) -> Result<Vec<String>> {
    if base.is_empty() {
        bail!("engine {engine} has no command");
    }
    let mut argv: Vec<String> = base.to_vec();
    match engine {
        "codex" => {
            argv.push("exec".into());
            if let Some(id) = resume {
                argv.push("resume".into());
                argv.push(id.into());
            }
            argv.push("--json".into());
            argv.push("--dangerously-bypass-approvals-and-sandbox".into());
            argv.push("--skip-git-repo-check".into());
            if let Some(m) = model {
                argv.push("--model".into());
                argv.push(m.into());
            }
            argv.push(prompt.into());
        }
        "claude" => {
            if let Some(id) = resume {
                argv.push("--resume".into());
                argv.push(id.into());
            }
            argv.push("-p".into());
            argv.push(prompt.into());
            argv.push("--output-format".into());
            argv.push("stream-json".into());
            argv.push("--include-partial-messages".into());
            argv.push("--verbose".into());
            argv.push("--dangerously-skip-permissions".into());
            if let Some(m) = model {
                argv.push("--model".into());
                argv.push(m.into());
            }
        }
        "grok" => {
            if let Some(id) = resume {
                argv.push("--resume".into());
                argv.push(id.into());
            }
            argv.push("-p".into());
            argv.push(prompt.into());
            argv.push("--output-format".into());
            argv.push("streaming-messages-json".into());
            argv.push("--include-partial-messages".into());
            argv.push("--always-approve".into());
            if let Some(m) = model {
                argv.push("--model".into());
                argv.push(m.into());
            }
        }
        other => bail!("a exec supports claude, codex, and grok only (got engine {other})"),
    }
    Ok(argv)
}

fn wire_format_for(engine: &str) -> Result<WireFormat> {
    match engine {
        "claude" | "grok" => Ok(WireFormat::ClaudeOrGrok),
        "codex" => Ok(WireFormat::CodexExec),
        other => bail!("a exec supports claude, codex, and grok only (got engine {other})"),
    }
}

/// Runs one headless agent invocation to completion, streaming normalized
/// `UnifiedEvent`s as they arrive and returning the child's exit code.
///
/// Deliberately a plain piped subprocess, NOT a PTY: heru's own
/// `ExternalCLIAdapter.run`/`run_live` (`heru/base.py`, read in full) spawns
/// every engine adapter with `subprocess.Popen(..., stdout=PIPE,
/// stderr=PIPE, ...)` -- no pty allocation anywhere in that file. This
/// matches the actual contract: `codex exec --json`/`claude --print
/// --output-format stream-json`/`grok -p --output-format
/// streaming-messages-json` are documented to write structured JSONL
/// directly to stdout in headless mode, not to a terminal -- there is
/// nothing for a PTY to capture that a pipe doesn't already give directly,
/// and a PTY would add line-buffering/echo/terminal-control-sequence noise
/// that would have to be stripped back out before JSON parsing. This is
/// also why `a exec` does not go through aplexer's worker/PTY session
/// machinery at all (see the module doc comment): it is a bounded,
/// one-shot call, not a persistent interactive session.
pub fn run_exec(
    engine: &str,
    argv: &[String],
    cwd: &Path,
    env_set: &BTreeMap<String, String>,
    env_unset: &[String],
    json_output: bool,
) -> Result<i32> {
    let format = wire_format_for(engine)?;
    let program = argv.first().ok_or_else(|| anyhow!("empty argv"))?;
    let mut command = Command::new(program);
    command
        .args(&argv[1..])
        .current_dir(cwd)
        .envs(env_set)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in env_unset {
        command.env_remove(name);
    }
    let mut child: Child = command
        .spawn()
        .with_context(|| format!("spawn {}", argv.join(" ")))?;

    // Forward the child's stderr live (auth errors, warnings) rather than
    // swallowing it -- the JSONL contract only covers stdout.
    let stderr = child.stderr.take();
    let stderr_thread = stderr.map(|s| {
        std::thread::spawn(move || {
            let reader = BufReader::new(s);
            let mut err = std::io::stderr();
            for line in reader.lines().map_while(std::io::Result::ok) {
                let _ = writeln!(err, "{line}");
            }
        })
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("child has no stdout"))?;
    let reader = BufReader::new(stdout);
    let mut assembler = JsonAssembler::default();
    let mut sequence: u64 = 0;
    let mut last_continuation: Option<String> = None;
    let mut stdout_out = std::io::stdout();

    for line in reader.lines() {
        let line = line.context("read child stdout")?;
        let Some(payload) = assembler.feed(&line) else {
            continue;
        };
        let (events, continuation) = translate(format, engine, &payload);
        if let Some(id) = continuation {
            last_continuation = Some(id);
        }
        for mut event in events {
            event.engine = engine.to_string();
            event.sequence = sequence;
            sequence += 1;
            event.timestamp = iso8601_utc(now_ms());
            event.raw = payload_to_raw(&payload);
            emit(&mut stdout_out, event, json_output)?;
        }
    }

    let status = child.wait().context("wait for child")?;
    if let Some(t) = stderr_thread {
        let _ = t.join();
    }
    if let Some(id) = last_continuation {
        let mut e = ev("continuation");
        e.engine = engine.to_string();
        e.sequence = sequence;
        e.timestamp = iso8601_utc(now_ms());
        e.continuation_id = Some(id);
        emit(&mut stdout_out, e, json_output)?;
    }
    Ok(status.code().unwrap_or(-1))
}

fn payload_to_raw(payload: &Value) -> Value {
    if payload.is_object() {
        payload.clone()
    } else {
        json!({"value": payload})
    }
}

fn emit(out: &mut impl Write, event: UnifiedEvent, json_output: bool) -> Result<()> {
    if json_output {
        writeln!(out, "{}", serde_json::to_string(&event)?)?;
    } else {
        writeln!(out, "{}", render_human(&event))?;
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
// `a transcript` -- native persisted-log location + parsing.
// Ported from pocketshell's `agent_log.py` path-resolution rules (read in
// full). Claude/codex only in this pass -- see module doc comment.
// ---------------------------------------------------------------------

/// Claude Code: `~/.claude/projects/<encoded-cwd>/<session>.jsonl`, where
/// `<encoded-cwd>` replaces every `/` with `-` (`agent_log.py::
/// _encode_claude_cwd`). aplexer has no direct handle on the underlying
/// claude session id, only the aplexer session's own `cwd` and
/// `created_at_ms` -- so this picks the most-recently-modified `*.jsonl`
/// directly under that cwd's project directory whose mtime is not earlier
/// than the aplexer session's creation (with a few seconds of slack for
/// startup ordering). This is a heuristic, not an exact session-id match:
/// if two aplexer claude sessions share the exact same cwd and are both
/// live, this can pick the wrong one. Documented, not silently assumed.
pub fn locate_claude_transcript(cwd: &Path, created_at_ms: u64) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let encoded = cwd.display().to_string().replace('/', "-");
    let dir = PathBuf::from(home).join(".claude/projects").join(encoded);
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
pub fn locate_codex_transcript(cwd: &Path, created_at_ms: u64) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let root = PathBuf::from(home).join(".codex/sessions");
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
        if let Ok(file) = std::fs::File::open(path) {
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

fn walk_jsonl(dir: &Path, visit: &mut impl FnMut(&Path, u64)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, visit);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Ok(meta) = entry.metadata() {
                if let Ok(modified) = meta.modified() {
                    if let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) {
                        visit(&path, dur.as_millis() as u64);
                    }
                }
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
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(u64, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else { continue };
        let Ok(dur) = modified.duration_since(std::time::UNIX_EPOCH) else {
            continue;
        };
        let mtime_ms = dur.as_millis() as u64;
        if mtime_ms < since_ms {
            continue;
        }
        if best.as_ref().map(|(m, _)| mtime_ms > *m).unwrap_or(true) {
            best = Some((mtime_ms, path));
        }
    }
    best.map(|(_, p)| p)
}

/// Reads and parses one transcript file into `UnifiedEvent`s, in file
/// order, sequence-numbered from 0. `engine` selects the wire format
/// (`WireFormat::ClaudeOrGrok` for claude, `WireFormat::CodexNative` for
/// codex's rollout shape).
pub fn read_transcript_events(engine: &str, path: &Path) -> Result<Vec<UnifiedEvent>> {
    let format = match engine {
        "claude" => WireFormat::ClaudeOrGrok,
        "codex" => WireFormat::CodexNative,
        other => bail!("a transcript supports claude and codex only (got engine {other})"),
    };
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read {}", path.display()))?;
    let mut assembler = JsonAssembler::default();
    let mut events = Vec::new();
    let mut sequence: u64 = 0;
    for line in text.lines() {
        let Some(payload) = assembler.feed(line) else {
            continue;
        };
        // Both claude's native rows and codex's rollout envelope carry their
        // own real top-level `"timestamp"` string (unlike `a exec`, where
        // there is no such field and `now_ms()` at translate time is the
        // right answer) -- prefer the row's own historical timestamp so a
        // paginated transcript reads in real wall-clock order, not "now".
        let row_timestamp = payload
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string();
        let (drafted, _continuation) = translate(format, engine, &payload);
        for mut event in drafted {
            event.engine = engine.to_string();
            event.sequence = sequence;
            sequence += 1;
            event.timestamp = row_timestamp.clone();
            event.raw = payload_to_raw(&payload);
            events.push(event);
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn claude_continuation_from_system_init() {
        let payload: Value = serde_json::from_str(
            r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#,
        )
        .unwrap();
        assert_eq!(claude_wire_continuation(&payload).as_deref(), Some("abc-123"));
    }

    #[test]
    fn codex_agent_message_maps_to_message() {
        let payload: Value = serde_json::from_str(
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"done"}}"#,
        )
        .unwrap();
        let events = codex_exec_events(&payload);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "message");
        assert_eq!(events[0].content, "done");
    }

    #[test]
    fn codex_command_execution_string_command() {
        let payload: Value = serde_json::from_str(
            r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"/bin/bash -lc ls","aggregated_output":"a\nb","exit_code":0}}"#,
        )
        .unwrap();
        let events = codex_exec_events(&payload);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "tool_result");
        assert_eq!(events[0].tool_name.as_deref(), Some("/bin/bash -lc ls"));
        assert_eq!(events[0].tool_output.as_deref(), Some("a\nb"));
    }

    #[test]
    fn codex_thread_started_continuation() {
        let payload: Value =
            serde_json::from_str(r#"{"type":"thread.started","thread_id":"th-1"}"#).unwrap();
        assert_eq!(codex_exec_continuation(&payload).as_deref(), Some("th-1"));
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
    fn build_headless_argv_codex_fresh() {
        let argv = build_headless_argv("codex", &["codex".into()], "hi", None, None).unwrap();
        assert_eq!(argv[0], "codex");
        assert_eq!(argv[1], "exec");
        assert!(argv.contains(&"--json".to_string()));
        assert_eq!(argv.last().unwrap(), "hi");
    }

    #[test]
    fn build_headless_argv_claude_resume() {
        let argv =
            build_headless_argv("claude", &["claude".into()], "hi", Some("sess-1"), None).unwrap();
        assert!(argv.windows(2).any(|w| w == ["--resume", "sess-1"]));
        assert!(argv.windows(2).any(|w| w == ["-p", "hi"]));
    }

    #[test]
    fn build_headless_argv_unknown_engine() {
        assert!(build_headless_argv("gemini", &["gemini".into()], "hi", None, None).is_err());
    }
}
