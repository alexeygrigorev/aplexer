//! Codex NATIVE rollout transcript (`~/.codex/sessions/.../<id>.jsonl`).
//! Parses only `response_item` rows (the raw per-turn model log) to avoid
//! double-counting against the separate `event_msg` progress-notification
//! rows, which mirror the same content.

use super::*;

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

pub(crate) fn codex_native_events(payload: &Value) -> Vec<UnifiedEvent> {
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
                    e.role = Some(role.to_string());
                    e.content = text;
                    out.push(e);
                }
            }
        }
        Some("custom_tool_call") => {
            let mut e = ev("tool_call");
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
pub(crate) fn codex_native_continuation(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    payload.get("payload").and_then(|p| str_field(p, "id"))
}

/// The codex rollout's own working directory, from the same `session_meta`
/// row -- used by `locate_codex_transcript` to disambiguate candidate files
/// beyond the mtime heuristic.
pub(crate) fn codex_native_cwd(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    payload.get("payload").and_then(|p| str_field(p, "cwd"))
}
