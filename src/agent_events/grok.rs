//! Grok Build ACP `updates.jsonl` (`session/update` rows). Field mapping
//! follows pocketshell's `GrokBuildParser.kt`.

use super::*;

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

pub(crate) fn grok_native_events(payload: &Value) -> Vec<UnifiedEvent> {
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
                e.role = Some("user".to_string());
                e.content = text;
                out.push(e);
            }
        }
        "agent_message_chunk" => {
            if let Some(text) = grok_chunk_text(update) {
                let mut e = ev("message");
                e.role = Some("assistant".to_string());
                e.content = text;
                out.push(e);
            }
        }
        "tool_call" => {
            let mut e = ev("tool_call");
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

pub(crate) fn grok_native_continuation(payload: &Value) -> Option<String> {
    payload
        .get("params")
        .and_then(|p| str_field(p, "sessionId"))
}

/// `timestamp` may be seconds or milliseconds since the epoch; anything
/// negative or overflowing is not a time and falls through.
fn grok_epoch_ms(n: i64) -> Option<u64> {
    let n = u64::try_from(n).ok()?;
    if n < 10_000_000_000 {
        n.checked_mul(1000)
    } else {
        Some(n)
    }
}

pub(crate) fn grok_row_timestamp(payload: &Value) -> String {
    if let Some(ms) = payload
        .get("timestamp")
        .and_then(|t| t.as_i64())
        .and_then(grok_epoch_ms)
    {
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
