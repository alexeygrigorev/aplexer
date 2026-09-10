//! Nested-hook JSON merge (Claude settings.json, Codex hooks.json,
//! Grok aplexer.json, Gemini settings.json share one shape).

use super::*;

// ---------------------------------------------------------------------------
// Nested-hook JSON merge (Claude settings.json, Codex hooks.json,
// Grok aplexer.json, Gemini settings.json share one shape)
// ---------------------------------------------------------------------------

/// One hook group in the nested format.
fn our_group(command: String) -> Value {
    serde_json::json!({"hooks": [{"type": "command", "command": command}]})
}

/// Does a hook group already contain a state-report entry for `state`?
pub(crate) fn group_reports(group: &Value, state: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| reports_state(c, state))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Merge our `(event, state)` wirings into a nested-hooks document.
/// Returns the number of events changed. Idempotent: a second merge with
/// the same commands changes nothing.
///
/// Shape errors (non-object root, non-object `hooks`) are refused rather
/// than clobbered — the file may hold something newer than this tool
/// understands. A non-array event slot is schema-invalid in every engine,
/// so it is replaced (there is nothing meaningful to preserve).
pub fn merge_nested_hooks(doc: &mut Value, events: &[(&str, &str)], a_bin: &str) -> Result<usize> {
    let root = doc.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!("hooks document root is not a JSON object; left untouched")
    })?;
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Default::default()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("\"hooks\" key is not an object; left untouched"))?;
    let mut changed = 0;
    for (event, state) in events {
        let slot = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let groups = match slot.as_array_mut() {
            Some(groups) => groups,
            None => {
                *slot = Value::Array(Vec::new());
                slot.as_array_mut().expect("just set to array")
            }
        };
        if !groups.iter().any(|g| group_reports(g, state)) {
            groups.push(our_group(state_report_command(a_bin, state)));
            changed += 1;
        }
    }
    Ok(changed)
}

/// Removes every `state-report` hook from a nested hooks document, keyed by
/// the ours-by-content command match alone, across ALL event groups -- not
/// just the events the current install tables name. The event tables change
/// between releases (SubagentStop was unmapped from `idle` in 2026-09); an
/// uninstall that iterated only the current names would leave a retired
/// event's entry behind forever, pushing state from an event this version no
/// longer believes in. Foreign commands (no `state-report` in them) are
/// never touched, whatever the event. Drops emptied groups, events, and the
/// top-level `hooks` object. Returns true when anything changed.
pub fn unmerge_nested_hooks(doc: &mut Value) -> bool {
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(groups) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        let mut kept = Vec::with_capacity(groups.len());
        for group in groups.drain(..) {
            match group {
                Value::Object(mut map) => {
                    let inner = map.get_mut("hooks").and_then(Value::as_array_mut);
                    match inner {
                        Some(inner) => {
                            let inner_before = inner.len();
                            inner.retain(|h| {
                                h.get("command")
                                    .and_then(Value::as_str)
                                    .map(|c| !is_state_report_command(c))
                                    .unwrap_or(true)
                            });
                            if inner.len() != inner_before {
                                changed = true;
                            }
                            if !inner.is_empty() {
                                kept.push(Value::Object(map));
                            } else {
                                changed = true;
                            }
                        }
                        None => kept.push(Value::Object(map)),
                    }
                }
                other => kept.push(other),
            }
        }
        if kept.is_empty() {
            hooks.remove(&event);
        } else {
            hooks.insert(event, Value::Array(kept));
        }
    }
    if hooks.is_empty() {
        if let Some(root) = doc.as_object_mut() {
            root.remove("hooks");
        }
    }
    changed
}

/// Which required `(event, state)` wirings are missing from a document.
/// Empty means installed. A missing/unparseable file counts as all
/// missing (callers treat absent files as "not installed", not as errors).
pub fn missing_nested_hooks(doc: &Value, events: &[(&str, &str)]) -> Vec<String> {
    let mut missing = Vec::new();
    let hooks = doc.get("hooks").and_then(Value::as_object);
    for (event, state) in events {
        let present = hooks
            .and_then(|h| h.get(*event))
            .and_then(Value::as_array)
            .map(|groups| groups.iter().any(|g| group_reports(g, state)))
            .unwrap_or(false);
        if !present {
            missing.push((*event).to_string());
        }
    }
    missing
}
