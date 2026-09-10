//! Codex legacy notify (top-level `notify = [...]` in config.toml).

use super::*;

// ---------------------------------------------------------------------------
// Codex legacy notify (top-level `notify = [...]` in config.toml)
// ---------------------------------------------------------------------------

/// Render our notify argv for `config.toml`: a `sh -c` wrapper so the hook
/// always exits 0 (same non-blocking guarantee as the `|| true` commands).
pub fn codex_notify_line(a_bin: &str) -> String {
    let inner = state_report_command(a_bin, "idle");
    let mut rendered = String::from("notify = [\"sh\", \"-c\", ");
    rendered.push_str(&serde_json::to_string(&inner).unwrap_or_else(|_| format!("{inner:?}")));
    rendered.push(']');
    rendered
}

/// Classify the top-level `notify` in a `config.toml` text: the byte span
/// of its line plus whether it is ours (`Absend`/`Ours`/`Foreign`).
fn find_top_level_notify(text: &str) -> Option<(usize, usize)> {
    // A top-level `notify` lives before the first `[table]` header; anything
    // at or after that belongs to a table and is not Codex's `notify`.
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let line_start = pos;
        pos += line.len();
        let body = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = body.trim_start();
        if trimmed.starts_with('[') {
            break;
        }
        if trimmed.starts_with("notify") && trimmed["notify".len()..].trim_start().starts_with('=')
        {
            return Some((line_start, line_start + body.len()));
        }
    }
    None
}

fn parse_notify_line(line: &str) -> Option<toml::Value> {
    line.parse::<toml::Value>()
        .ok()
        .and_then(|v| v.get("notify").cloned())
}

fn notify_value_is_ours(value: &toml::Value) -> bool {
    let args: Vec<&str> = match value.as_array() {
        Some(items) => items.iter().filter_map(toml::Value::as_str).collect(),
        None => return false,
    };
    args.iter().any(|a| is_state_report_command(a))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyState {
    Absent,
    Ours,
    Foreign,
}

/// Status of the legacy `notify` line (informational only — `hooks.json`
/// is the primary codex channel; see the module docs).
pub fn codex_notify_state(text: &str) -> NotifyState {
    match find_top_level_notify(text) {
        None => NotifyState::Absent,
        Some((start, end)) => match parse_notify_line(&text[start..end]) {
            Some(value) if notify_value_is_ours(&value) => NotifyState::Ours,
            _ => NotifyState::Foreign,
        },
    }
}

/// Ensure our `notify` line exists. Returns the new text and whether it
/// changed. A foreign `notify` is never clobbered (returns unchanged).
pub fn ensure_codex_notify(text: &str, a_bin: &str) -> (String, bool) {
    match find_top_level_notify(text) {
        None => {
            let mut out = codex_notify_line(a_bin);
            out.push('\n');
            out.push_str(text);
            (out, true)
        }
        Some((start, end)) => match parse_notify_line(&text[start..end]) {
            Some(value) if notify_value_is_ours(&value) => (text.to_string(), false),
            _ => (text.to_string(), false),
        },
    }
}

/// Remove our `notify` line. Returns the new text and whether it changed.
/// A foreign `notify` is left alone.
pub fn remove_codex_notify(text: &str) -> (String, bool) {
    let Some((start, end)) = find_top_level_notify(text) else {
        return (text.to_string(), false);
    };
    let is_ours = parse_notify_line(&text[start..end])
        .map(|v| notify_value_is_ours(&v))
        .unwrap_or(false);
    if !is_ours {
        return (text.to_string(), false);
    }
    let drop_end = if text.as_bytes().get(end) == Some(&b'\n') {
        end + 1
    } else {
        end
    };
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&text[drop_end..]);
    (out, true)
}
