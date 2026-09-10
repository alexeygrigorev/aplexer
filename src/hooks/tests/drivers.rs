use super::*;

#[test]
fn opencode_plugin_embeds_the_a_binary_and_maps_events() {
    let source = opencode_plugin_source(A_BIN);
    assert!(source.contains(A_BIN));
    for event in [
        "session.idle",
        "permission.asked",
        "session.error",
        "session.created",
    ] {
        assert!(source.contains(event), "missing {event}");
    }
    for state in ["idle", "waiting", "working"] {
        assert!(source.contains(state), "missing {state}");
    }
}

#[test]
fn normalize_engine_filter_maps_zcodex_onto_codex() {
    assert_eq!(normalize_engine_filter("zcodex").unwrap(), "codex");
    assert_eq!(normalize_engine_filter("codex").unwrap(), "codex");
    assert!(normalize_engine_filter("shell").is_err());
}

#[test]
fn engine_drivers_match_the_documented_engine_list() {
    let driven: Vec<&str> = ENGINE_DRIVERS.iter().map(|d| d.engine).collect();
    assert_eq!(driven, HOOK_ENGINES);
}

#[test]
fn install_check_uninstall_round_trip_in_a_throwaway_home() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path();
    let targets = resolve_targets(home, None, None, &[]);
    // check before install: nothing installed.
    let statuses = check(&targets, None);
    assert_eq!(statuses.len(), HOOK_ENGINES.len());
    assert!(statuses.iter().all(|s| !s.installed));
    // install: everything installs (fresh home, no foreign config).
    let installed = install(&targets, A_BIN, None);
    assert!(installed.iter().all(|s| s.installed), "{installed:?}");
    // check after install: fully initialized.
    let statuses = check(&targets, None);
    assert!(statuses.iter().all(|s| s.installed), "{statuses:?}");
    // install again: idempotent, nothing new.
    let again = install(&targets, A_BIN, None);
    assert!(again.iter().all(|s| s.action == "present"), "{again:?}");
    // uninstall: removes what install added.
    let removed = uninstall(&targets, None);
    assert!(removed.iter().all(|s| s.action != "error"), "{removed:?}");
    let statuses = check(&targets, None);
    assert!(statuses.iter().all(|s| !s.installed));
}

#[test]
fn engine_filter_limits_the_drivers() {
    let dir = tempfile::TempDir::new().unwrap();
    let targets = resolve_targets(dir.path(), None, None, &[]);
    let only = install(&targets, A_BIN, Some("opencode"));
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].engine, "opencode");
    let statuses = check(&targets, Some("opencode"));
    assert!(statuses.iter().all(|s| s.installed));
    let others = check(&targets, Some("claude"));
    assert!(others.iter().all(|s| !s.installed));
}

#[test]
fn install_writes_through_a_symlinked_settings_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let home = dir.path().join("home");
    let dotfiles = dir.path().join("dotfiles");
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::create_dir_all(home.join(".gemini")).unwrap();
    fs::create_dir_all(&dotfiles).unwrap();
    let real = dotfiles.join("claude-settings.json");
    fs::write(&real, "{\"permissions\": {}}\n").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
    let link = home.join(".claude").join("settings.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    // A dotfiles link whose target does not exist yet.
    let dangling_target = dotfiles.join("gemini").join("settings.json");
    let dangling = home.join(".gemini").join("settings.json");
    std::os::unix::fs::symlink(&dangling_target, &dangling).unwrap();

    let targets = resolve_targets(&home, None, None, &[]);
    let statuses = install(&targets, A_BIN, None);
    assert!(statuses.iter().all(|s| s.installed), "{statuses:?}");

    for path in [&link, &dangling] {
        assert!(
            fs::symlink_metadata(path).unwrap().file_type().is_symlink(),
            "{} is no longer a symlink",
            path.display()
        );
    }
    let doc: Value = serde_json::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
    assert!(missing_nested_hooks(&doc, &CLAUDE_EVENTS).is_empty());
    assert_eq!(doc["permissions"], json!({}));
    assert_eq!(
        fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o640,
        "mode of the real file was not preserved"
    );
    let doc: Value = serde_json::from_str(&fs::read_to_string(&dangling_target).unwrap()).unwrap();
    assert!(missing_nested_hooks(&doc, &GEMINI_EVENTS).is_empty());
    assert_eq!(
        fs::metadata(&dangling_target).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
