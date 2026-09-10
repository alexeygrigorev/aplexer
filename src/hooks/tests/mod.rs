//! Unit tests for `a init`, one file per submodule; the hook-command and
//! target-resolution tests and the shared fixtures live here.

mod codex_notify;
mod drivers;
mod nested;

use super::*;

use serde_json::json;

const A_BIN: &str = "/home/test/.local/bin/a";

fn merged(events: &[(&str, &str)], start: Value) -> Value {
    let mut doc = start;
    merge_nested_hooks(&mut doc, events, A_BIN).unwrap();
    doc
}

#[test]
fn state_report_command_never_blocks() {
    let cmd = state_report_command("/home/u/.local/bin/a", "idle");
    assert!(cmd.ends_with("|| true"), "{cmd}");
    // Boring paths stay unquoted and readable.
    assert_eq!(cmd, "/home/u/.local/bin/a state-report idle || true");
    // Weird paths are quoted, never interpolated raw.
    let quoted = state_report_command("/home/u/my dir/a", "waiting");
    assert!(quoted.starts_with("'/home/u/my dir/a'"), "{quoted}");
}

#[test]
fn resolve_targets_covers_profile_config_dirs() {
    let home = Path::new("/home/u");
    let mut codex_profile = BTreeMap::new();
    codex_profile.insert("CODEX_HOME".to_string(), "/home/u/.zodex".to_string());
    let mut claude_profile = BTreeMap::new();
    claude_profile.insert(
        "CLAUDE_CONFIG_DIR".to_string(),
        "/home/u/.zlaude".to_string(),
    );
    let targets = resolve_targets(home, None, None, &[codex_profile, claude_profile]);
    assert!(targets
        .claude_settings
        .contains(&PathBuf::from("/home/u/.claude/settings.json")));
    assert!(targets
        .claude_settings
        .contains(&PathBuf::from("/home/u/.zlaude/settings.json")));
    assert!(targets
        .codex_dirs
        .contains(&PathBuf::from("/home/u/.codex")));
    assert!(targets
        .codex_dirs
        .contains(&PathBuf::from("/home/u/.zodex")));
    assert_eq!(targets.grok_dir, PathBuf::from("/home/u/.grok"));
    assert_eq!(
        targets.gemini_settings,
        PathBuf::from("/home/u/.gemini/settings.json")
    );
    assert_eq!(
        targets.opencode_plugin_dir,
        PathBuf::from("/home/u/.config/opencode/plugin")
    );
}

#[test]
fn reports_state_matches_the_state_argument_not_the_path() {
    let cmd = state_report_command("/home/u/idle-tools/a", "working");
    assert!(reports_state(&cmd, "working"));
    assert!(!reports_state(&cmd, "idle"), "{cmd}");
    assert!(!reports_state(
        "/home/u/idle-tools/a status || true",
        "idle"
    ));
    assert!(reports_state("a state-report idle", "idle"));
    assert!(!reports_state("a state-report", "idle"));
}
