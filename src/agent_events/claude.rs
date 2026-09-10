//! Claude native JSONL (Anthropic Messages API event shape).

use super::*;

// ---------------------------------------------------------------------
// Claude native JSONL (Anthropic Messages API event shape).
// ---------------------------------------------------------------------

/// `unwrap_stream_event`: partial-delta lines arrive wrapped as
/// `{"type":"stream_event","event":{...}}`; unwrap to the inner event.
/// Full-message lines (`assistant`/`user`/`result`/`system`/`error`) are
/// not wrapped and pass through unchanged.
fn claude_unwrap(payload: &Value) -> &Value {
    payload
        .get("event")
        .filter(|e| e.is_object())
        .unwrap_or(payload)
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

fn claude_tool_result_event(block: &Value) -> UnifiedEvent {
    let tool_output = match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    };
    let mut e = ev("tool_result");
    e.role = Some("user".to_string());
    e.tool_output = Some(tool_output);
    if let Some(id) = str_field(block, "tool_use_id") {
        e.metadata.insert("tool_use_id".into(), json!(id));
    }
    e
}

fn claude_user_text_event(text: &str) -> UnifiedEvent {
    let mut e = ev("message");
    e.role = Some("user".to_string());
    e.content = text.to_string();
    e
}

/// Ported `_claude_impl.py::live_events`, PLUS the real-shape `"user"`
/// unwrap (tool_result AND user text -- PocketShell needs both).
pub(crate) fn claude_wire_events(payload: &Value) -> Vec<UnifiedEvent> {
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
            e.role = Some("user".to_string());
            e.tool_output = Some(tool_output);
            out.push(e);
        }
        Some("user") => match unwrapped.get("message").and_then(|m| m.get("content")) {
            Some(Value::String(s)) if !s.is_empty() => {
                out.push(claude_user_text_event(s));
            }
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    match block.get("type").and_then(|t| t.as_str()) {
                        Some("tool_result") => out.push(claude_tool_result_event(block)),
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                if !text.is_empty() {
                                    out.push(claude_user_text_event(text));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        },
        Some("assistant") => {
            for message in claude_final_messages(unwrapped) {
                let mut e = ev("message");
                e.role = Some("assistant".to_string());
                e.content = message;
                out.push(e);
            }
        }
        Some("result") => {
            for message in claude_final_messages(unwrapped) {
                let mut e = ev("message");
                e.role = Some("assistant".to_string());
                e.content = message;
                out.push(e);
            }
            if let Some(usage) = unwrapped.get("usage").filter(|u| u.is_object()) {
                let mut meta = BTreeMap::new();
                int_meta(&mut meta, "input_tokens", usage);
                int_meta(&mut meta, "output_tokens", usage);
                let mut e = ev("usage");
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
pub(crate) fn claude_wire_continuation(payload: &Value) -> Option<String> {
    if payload.get("type").and_then(|t| t.as_str()) != Some("system") {
        return None;
    }
    if payload.get("subtype").and_then(|t| t.as_str()) != Some("init") {
        return None;
    }
    str_field(payload, "session_id")
}
