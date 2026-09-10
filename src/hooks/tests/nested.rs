use super::*;

#[test]
fn merge_creates_hooks_object_from_empty() {
    let doc = merged(&CLAUDE_EVENTS, json!({}));
    for (event, state) in CLAUDE_EVENTS {
        let groups = doc["hooks"][event].as_array().unwrap();
        assert_eq!(groups.len(), 1, "event {event}");
        assert!(group_reports(&groups[0], state));
    }
    assert!(missing_nested_hooks(&doc, &CLAUDE_EVENTS).is_empty());
}

#[test]
fn merge_is_idempotent() {
    let once = merged(&CLAUDE_EVENTS, json!({}));
    let mut twice = once.clone();
    let changed = merge_nested_hooks(&mut twice, &CLAUDE_EVENTS, A_BIN).unwrap();
    assert_eq!(changed, 0);
    assert_eq!(once, twice);
}

#[test]
fn merge_preserves_existing_hooks_and_keys() {
    let start = json!({
        "permissions": {"deny": ["AskUserQuestion"]},
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "my-linter"}]}],
            "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "check"}]}]
        }
    });
    let doc = merged(&CLAUDE_EVENTS, start);
    // Unrelated top-level keys survive.
    assert_eq!(doc["permissions"]["deny"], json!(["AskUserQuestion"]));
    // Pre-existing Stop group survives alongside ours.
    let stop = doc["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2);
    assert!(stop
        .iter()
        .any(|g| g["hooks"][0]["command"] == json!("my-linter")));
    // Untouched events survive byte-for-byte in value.
    assert_eq!(
        doc["hooks"]["PreToolUse"],
        json!([{"matcher": "Bash", "hooks": [{"type": "command", "command": "check"}]}])
    );
}

#[test]
fn merge_accepts_a_foreign_state_report_hook_as_installed() {
    // A hand-written `a state-report` hook (different binary path)
    // already feeds ingestion; do not add a duplicate group.
    let start = json!({
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "a state-report idle || true"}]}]
        }
    });
    let mut doc = start;
    let changed = merge_nested_hooks(&mut doc, &[("Stop", "idle")], A_BIN).unwrap();
    assert_eq!(changed, 0);
    assert!(missing_nested_hooks(&doc, &[("Stop", "idle")]).is_empty());
}

#[test]
fn merge_replaces_a_schema_invalid_event_slot() {
    let doc = merged(&CODEX_EVENTS, json!({"hooks": {"Stop": "bogus"}}));
    assert!(missing_nested_hooks(&doc, &CODEX_EVENTS).is_empty());
}

#[test]
fn merge_refuses_a_non_object_root() {
    let mut doc = json!([1, 2, 3]);
    assert!(merge_nested_hooks(&mut doc, &CODEX_EVENTS, A_BIN).is_err());
    assert_eq!(doc, json!([1, 2, 3]));
}

#[test]
fn unmerge_removes_only_ours_and_drops_emptied_keys() {
    let mut doc = merged(&CLAUDE_EVENTS, json!({}));
    assert!(unmerge_nested_hooks(&mut doc));
    // Whole `hooks` object is gone: we created every key in it.
    assert_eq!(doc, json!({}));
}

#[test]
fn unmerge_keeps_user_hooks_in_shared_groups() {
    let start = json!({
        "hooks": {
            "Stop": [{
                "matcher": "x",
                "hooks": [
                    {"type": "command", "command": "my-linter"},
                    {"type": "command", "command": "a state-report idle || true"}
                ]
            }]
        }
    });
    let mut doc = start;
    assert!(unmerge_nested_hooks(&mut doc));
    assert_eq!(
        doc["hooks"]["Stop"],
        json!([{
            "matcher": "x",
            "hooks": [{"type": "command", "command": "my-linter"}]
        }])
    );
}

#[test]
fn unmerge_sweeps_retired_events_but_leaves_foreign_hooks_there() {
    // SubagentStop was unmapped from `idle` in 2026-09. An uninstall
    // keyed on the current install tables would never visit its group,
    // leaving our entry pushing idle from an event this version no
    // longer believes in -- while a foreign program's SubagentStop hook
    // in the same document must survive untouched.
    let start = json!({
        "hooks": {
            "Stop": [{
                "hooks": [{"type": "command", "command": "a state-report idle || true"}]
            }],
            "SubagentStop": [{
                "hooks": [
                    {"type": "command", "command": "python3 /opt/pocketshell/hooks/claude_hook.py"},
                    {"type": "command", "command": "/usr/local/bin/a state-report idle || true"}
                ]
            }]
        }
    });
    let mut doc = start;
    assert!(unmerge_nested_hooks(&mut doc));
    assert!(doc["hooks"].get("Stop").is_none());
    assert_eq!(
        doc["hooks"]["SubagentStop"],
        json!([{
            "hooks": [
                {"type": "command", "command": "python3 /opt/pocketshell/hooks/claude_hook.py"}
            ]
        }])
    );
}

#[test]
fn missing_reports_every_absent_event() {
    let missing = missing_nested_hooks(&json!({}), &GEMINI_EVENTS);
    assert_eq!(missing.len(), GEMINI_EVENTS.len());
    let doc = merged(&GEMINI_EVENTS, json!({}));
    assert!(missing_nested_hooks(&doc, &GEMINI_EVENTS).is_empty());
}
