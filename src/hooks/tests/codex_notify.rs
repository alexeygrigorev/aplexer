use super::*;

#[test]
fn codex_notify_round_trip() {
    let (text, changed) = ensure_codex_notify("", A_BIN);
    assert!(changed);
    assert_eq!(codex_notify_state(&text), NotifyState::Ours);
    let (same, changed) = ensure_codex_notify(&text, A_BIN);
    assert!(!changed);
    assert_eq!(same, text);
    let (removed, changed) = remove_codex_notify(&text);
    assert!(changed);
    assert_eq!(removed, "");
    assert_eq!(codex_notify_state(&removed), NotifyState::Absent);
}

#[test]
fn codex_notify_never_clobbers_a_foreign_program() {
    let foreign = "notify = [\"notify-send\", \"Codex\"]\n[model]\nname = \"x\"\n";
    assert_eq!(codex_notify_state(foreign), NotifyState::Foreign);
    let (same, changed) = ensure_codex_notify(foreign, A_BIN);
    assert!(!changed);
    assert_eq!(same, foreign);
    let (same, changed) = remove_codex_notify(foreign);
    assert!(!changed);
    assert_eq!(same, foreign);
}

#[test]
fn codex_notify_ignores_table_scoped_keys() {
    // A `notify` under a [table] is not the top-level notify.
    let text = "[tui]\nnotify = [\"x\"]\n";
    assert_eq!(codex_notify_state(text), NotifyState::Absent);
    let (out, changed) = ensure_codex_notify(text, A_BIN);
    assert!(changed);
    assert!(out.starts_with("notify = "));
    assert!(out.contains("[tui]\nnotify = [\"x\"]\n"));
}
