fn mk_record(workspace: &str, tag: &str, phase: Phase) -> SessionRecord {
    let id = Uuid::new_v4();
    SessionRecord {
        parent_session: None,
        schema_version: SCHEMA_VERSION,
        id,
        workspace: PathBuf::from(workspace),
        tag: tag.to_string(),
        engine: "shell".to_string(),
        profile: None,
        command: vec![],
        cwd: PathBuf::from(workspace),
        env: Default::default(),
        env_unset: Default::default(),
        limits: Default::default(),
        history_bytes: 0,
        created_at_ms: 0,
        updated_at_ms: 0,
        last_activity_ms: None,
        last_accessed_ms: None,
        reported_state: None,
        reported_state_at_ms: None,
        phase,
        worker_pid: Some(std::process::id()), // our own pid: always "alive"
        workload_pid: None,
        worker_cgroup: None,
        workload_cgroup: None,
        containment_cgroup: None,
        containment_cgroup_identity: None,
        containment_empty: Some(false),
        // Must exist on disk: check_attachable now checks socket_path
        // (this test binary's own executable is a convenient stand-in
        // for "some file that's there"; only .exists() is probed, never
        // actually connected to).
        socket_path: std::env::current_exe().unwrap(),
        history_path: PathBuf::from("/nonexistent"),
        exit: None,
        error: None,
    }
}

#[test]
fn terminal_text_sanitizer_replaces_c0_del_and_c1_controls() {
    let unsafe_text = "plain\x00\x07\x1b\n\r\x7f\u{0085}\u{009b}tail";
    let safe = sanitize_terminal_text(unsafe_text);
    assert_eq!(safe, "plain????????tail");
    assert!(!safe.chars().any(char::is_control));
}

#[test]
fn cgroup_capability_requires_delegation_but_is_optional_when_missing() {
    let warning = cgroup_limits_check(CgroupLimitProbe {
        cgroup_v2: true,
        controllers: vec!["cpu".into(), "memory".into(), "pids".into()],
        delegated_scope: false,
        detail: "user manager unavailable".into(),
    });
    assert_eq!(warning["available"], false);
    assert_eq!(warning["ok"], false);
    assert_eq!(warning["severity"], "warning");
    assert_eq!(warning["required"], false);
    assert!(warning["detail"]
        .as_str()
        .unwrap()
        .contains("unlimited sessions still work"));
    assert!(doctor_checks_ok(&[warning]));

    let no_cgroup_v2 = cgroup_limits_check(CgroupLimitProbe {
        cgroup_v2: false,
        controllers: Vec::new(),
        delegated_scope: false,
        detail: "cgroup v2 unavailable".into(),
    });
    assert_eq!(no_cgroup_v2["severity"], "warning");
    assert!(doctor_checks_ok(&[no_cgroup_v2]));

    let supported = cgroup_limits_check(CgroupLimitProbe {
        cgroup_v2: true,
        controllers: vec!["cpu".into(), "memory".into(), "pids".into()],
        delegated_scope: true,
        detail: "verified".into(),
    });
    assert_eq!(supported["available"], true);
    assert_eq!(supported["ok"], true);
    assert_eq!(supported["severity"], "ok");
}

#[test]
fn status_json_value_preserves_runtime_diagnostics() {
    let current = mk_record("/ws", "status", Phase::Running);
    let status = StatusData {
        current,
        raw: json!({
            "cgroup": {"memory": "64M"},
            "foreground_command": "vim",
        }),
        worker_reachable: false,
        rpc_error: Some("connection refused".into()),
        cgroup_stats: Some(json!({"memory": "64M"})),
        history_persistence_error: Some("history warning".into()),
        record_persistence_error: Some("record warning".into()),
        foreground_command: Some("vim".into()),
    };

    let value = status.json_value().unwrap();
    assert_eq!(value["state"], "running");
    assert_eq!(value["worker_alive"], true);
    assert_eq!(value["worker_reachable"], false);
    assert_eq!(value["rpc_error"], "connection refused");
    assert_eq!(value["foreground_command"], "vim");
    assert_eq!(value["cgroup"]["memory"], "64M");
    assert_eq!(value["history_persistence_error"], "history warning");
    assert_eq!(value["record_persistence_error"], "record warning");
}

#[test]
fn status_bar_sanitizes_record_fields_and_flash_messages() {
    let ctx = status_ctx_for_test(true);
    {
        let mut record = ctx.record.lock().unwrap();
        record.workspace = PathBuf::from("/ws/\x1b[31mred\nline");
        record.tag = "tag\x07bell".to_string();
        record.engine = "engine\rreturn".to_string();
        record.profile = Some("profile\u{009b}2J".to_string());
    }

    let rendered = status_bar_text(&ctx, 256);
    assert!(!rendered.chars().any(char::is_control), "{rendered:?}");
    assert!(!rendered.contains("\x1b[31m"), "{rendered:?}");

    *ctx.flash.lock().unwrap() = Some(("failed\x1b[2J\x07\nnext".to_string(), Instant::now()));
    let flashed = status_bar_text(&ctx, 80);
    assert!(!flashed.chars().any(char::is_control), "{flashed:?}");
    assert!(!flashed.contains("\x1b[2J"), "{flashed:?}");
}

#[test]
fn agent_annotation_appears_only_when_it_adds_information() {
    let mut record = mk_record("/ws", "t", Phase::Running);
    assert_eq!(extra_agent_label(&record, None), None);

    // A claude-engine session running claude already says claude ...
    record.engine = "claude".to_string();
    assert_eq!(
        extra_agent_label(&record, Some(agent_kind::AgentKind::Claude)),
        None
    );
    // ... but the same session running codex does not.
    assert_eq!(
        extra_agent_label(&record, Some(agent_kind::AgentKind::Codex)),
        Some("codex")
    );

    // The engine cell shows the declared engine/profile when there is
    // nothing detected ...
    record.engine = "shell".to_string();
    record.profile = Some("default".to_string());
    assert_eq!(engine_label(&record, None), "shell/default");
    // ... but a shell workload running an agent is labeled by the agent
    // alone: "shell" is the absence of a choice, not a fact worth a
    // column. A shell workload running zcodex lands here too -- the
    // zcodex token classifies as the codex kind.
    assert_eq!(
        engine_label(&record, Some(agent_kind::AgentKind::Claude)),
        "claude"
    );
    assert_eq!(
        engine_label(&record, Some(agent_kind::AgentKind::Codex)),
        "codex"
    );

    // A declared zcodex engine running zcodex is already fully named --
    // zcodex is a codex variant, so the family comparison says codex
    // once, exactly like the claude-engine/claude case above.
    record.engine = "zcodex".to_string();
    record.profile = None;
    assert_eq!(
        extra_agent_label(&record, Some(agent_kind::AgentKind::Codex)),
        None
    );
    assert_eq!(
        engine_label(&record, Some(agent_kind::AgentKind::Codex)),
        "zcodex"
    );

    // A declared engine stays the base: there the annotation is a real
    // override, not noise.
    record.engine = "claude".to_string();
    record.profile = None;
    assert_eq!(
        engine_label(&record, Some(agent_kind::AgentKind::Codex)),
        "claude -> codex"
    );
}

/// A fake `claude` whose cmdline detection classifies, run as
/// `/bin/sh <script>` -- same script shape and same ETXTBSY reasoning as
/// `write_fake_claude` in tests/agent_detection.rs. The loop also
/// self-expires, so a test that panics before cleanup cannot leak an
/// immortal polling process.
fn spawn_fake_claude(dir: &Path) -> (PathBuf, std::process::Child) {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let script = bin.join("claude");
    fs::write(
        &script,
        "#!/bin/sh\n\
             echo running > \"$1\"\n\
             i=0\n\
             while [ -e \"$2\" ] && [ $i -lt 300 ]; do /bin/sleep 0.05; i=$((i+1)); done\n",
    )
    .unwrap();
    fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let ready = dir.join("claude.ready");
    let sentinel = dir.join("claude.keep-running");
    fs::write(&sentinel, b"run").unwrap();
    let child = Command::new("/bin/sh")
        .arg(&script)
        .arg(&ready)
        .arg(&sentinel)
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "fake claude never started");
        thread::sleep(Duration::from_millis(10));
    }
    (sentinel, child)
}

#[test]
fn status_bar_names_the_agent_running_in_a_shell_session() {
    let dir = tempfile::TempDir::new().unwrap();
    let (sentinel, mut child) = spawn_fake_claude(dir.path());
    let ctx = status_ctx_for_test(true);
    ctx.record.lock().unwrap().workload_pid = Some(child.id());

    // Full layout: the agent sits between the state and the engine cell.
    let full = status_bar_text(&ctx, 256);
    assert!(full.contains("\u{25cf} RUNNING  claude  shell"), "{full:?}");
    // Narrow layout: the agent outlives the engine cell, same as the tag.
    let compact = status_bar_text(&ctx, 32);
    assert!(compact.contains("claude"), "{compact:?}");

    fs::remove_file(&sentinel).unwrap();
    child.wait().unwrap();
}

#[test]
fn status_bar_does_not_repeat_an_agent_the_engine_already_names() {
    let dir = tempfile::TempDir::new().unwrap();
    let (sentinel, mut child) = spawn_fake_claude(dir.path());
    let ctx = status_ctx_for_test(true);
    {
        let mut record = ctx.record.lock().unwrap();
        record.engine = "claude".to_string();
        record.workload_pid = Some(child.id());
    }

    let full = status_bar_text(&ctx, 256);
    assert!(full.contains("  claude  |  ^b ?"), "{full:?}");
    assert!(!full.contains("claude  claude"), "{full:?}");

    fs::remove_file(&sentinel).unwrap();
    child.wait().unwrap();
}

#[test]
fn spinner_frame_animates_only_the_reported_working_state() {
    // `working` is the one state that means "the agent said it is
    // running right now" (a fresh state-report push) -- the only one the
    // spinner may run for. `active` is deliberately absent: it is a
    // PTY-recency guess that also fires while the user types at a
    // prompt, and for records with no activity sample at all.
    for t in [0, 123_456_789] {
        assert!(
            spinner_frame("working", t).is_some(),
            "working should animate"
        );
    }
    // Everything else stays on `state_glyph`'s static glyph -- the bar
    // must be motionless for an idle, waiting, or dead session, which is
    // the "only when it's running" half of the feature.
    for state in [
        "active", "running", "waiting", "idle", "quiet", "starting", "stopping", "broken",
        "exited", "oom", "failed",
    ] {
        assert_eq!(spinner_frame(state, 0), None, "{state} must not animate");
        assert_eq!(
            spinner_frame(state, 123_456_789),
            None,
            "{state} must not animate"
        );
    }
}

#[test]
fn spinner_frame_is_a_pure_function_of_the_wall_clock() {
    // The status thread, the frame loop's pending flush, and the input
    // thread's flash redraw all render the bar independently; they must
    // agree on the frame within one SPINNER_FRAME_MS window without any
    // shared counter state.
    let base = 1_000_100; // not a multiple of SPINNER_FRAME_MS
    let window = base / SPINNER_FRAME_MS;
    assert_eq!(
        spinner_frame("working", base),
        Some(SPINNER_FRAMES[(window as usize) % SPINNER_FRAMES.len()])
    );
    // Both ends of the same window land on the same frame...
    assert_eq!(
        spinner_frame("working", window * SPINNER_FRAME_MS),
        spinner_frame("working", window * SPINNER_FRAME_MS + SPINNER_FRAME_MS - 1)
    );
    // ...and one full revolution later the frame wraps back around.
    assert_eq!(
        spinner_frame("working", base),
        spinner_frame(
            "working",
            base + SPINNER_FRAME_MS * SPINNER_FRAMES.len() as u64
        )
    );
}

#[test]
fn state_derivation_sees_the_worker_s_fresh_push_not_the_attach_snapshot() {
    let ctx = status_ctx_for_test(true);
    let now = now_ms();
    let record = {
        let mut record = ctx.record.lock().unwrap().clone();
        record.reported_state = Some("working".to_string());
        // A push from a minute before attach: stale now, so the
        // snapshot alone must not read as working.
        record.reported_state_at_ms = Some(now.saturating_sub(60_000));
        record
    };
    // No Status answer (worker briefly unreachable): snapshot stands.
    // The push is stale, so the word is only ever an activity guess --
    // for this occupied shell (no PTY sample at all) the heuristic's
    // just-started arm says `active`, never a semantic `working`.
    assert_eq!(
        session_ui_state(&overlay_reported_state(&record, None), now).0,
        "active"
    );
    // The worker's live copy says the agent started working *after*
    // attach -- the case the spinner exists for.
    let raw = serde_json::json!({"reported_state": "working", "reported_state_at_ms": now});
    assert_eq!(
        session_ui_state(&overlay_reported_state(&record, Some(&raw)), now).0,
        "working"
    );
    // An older worker that omits the fields leaves the snapshot alone.
    let overlay = overlay_reported_state(&record, Some(&serde_json::json!({"cgroup": {}})));
    assert_eq!(overlay.reported_state.as_deref(), Some("working"));
    assert_eq!(
        overlay.reported_state_at_ms,
        Some(now.saturating_sub(60_000))
    );
}

#[test]
fn status_bar_spins_only_while_the_agent_is_working() {
    let ctx = status_ctx_for_test(true);
    let now = now_ms();
    {
        let mut record = ctx.record.lock().unwrap();
        record.reported_state = Some("working".to_string());
        record.reported_state_at_ms = Some(now);
    }
    let full = status_bar_text(&ctx, 256);
    assert!(full.contains(" WORKING"), "{full:?}");
    assert!(
        full.chars().any(|c| SPINNER_FRAMES.contains(&c)),
        "a working session's bar should carry a spinner frame: {full:?}"
    );
    assert!(
        !full.contains('\u{25cf}'),
        "the static dot must yield to the spinner: {full:?}"
    );

    // Once the push goes stale the occupied shell falls back to the
    // honest activity word (`active`: no PTY sample at all, so the
    // heuristic's just-started arm) -- the bar freezes back to the
    // static dot, no motion. Only a *reported* working push may spin.
    {
        let mut record = ctx.record.lock().unwrap();
        record.reported_state_at_ms = Some(now.saturating_sub(8_001));
    }
    let full = status_bar_text(&ctx, 256);
    assert!(
        full.contains("\u{25cf} ACTIVE"),
        "a stale push falls back to the static glyph: {full:?}"
    );
    assert!(
        !full.chars().any(|c| SPINNER_FRAMES.contains(&c)),
        "no spinner may survive the stale push: {full:?}"
    );
}

#[test]
fn status_padding_uses_display_cells_and_preserves_graphemes() {
    let combining = "e\u{301}";
    let emoji = "👩‍💻";

    assert_eq!(pad_or_truncate("界x", 1), " ");
    assert_eq!(pad_or_truncate("界x", 2), "界");
    assert_eq!(pad_or_truncate("界x", 3), "界x");
    assert_eq!(pad_or_truncate(&format!("{combining}x"), 1), combining);
    assert_eq!(pad_or_truncate(&format!("{emoji}x"), 2), emoji);

    for (text, cols) in [("界x", 4), (combining, 3), (emoji, 5)] {
        let rendered = pad_or_truncate(text, cols);
        assert_eq!(terminal_display_width(&rendered), cols, "{rendered:?}");
    }
}

#[test]
fn terminal_reset_disables_every_snapshot_input_mode_variant() {
    let mouse_modes: &[&[u8]] = &[b"\x1b[?9h", b"\x1b[?1000h", b"\x1b[?1002h", b"\x1b[?1003h"];
    let mouse_encodings: &[&[u8]] = &[b"\x1b[?1005h", b"\x1b[?1006h"];

    for mode in mouse_modes {
        for encoding in mouse_encodings {
            let mut parser = vt100::Parser::new(24, 80, 0);
            parser.process(b"\x1b[?1049h\x1b=\x1b[?1h\x1b[?2004h\x1b[?25l");
            parser.process(mode);
            parser.process(encoding);
            parser.process(TERMINAL_RESET_SEQUENCE);

            let screen = parser.screen();
            assert!(!screen.alternate_screen());
            assert!(!screen.application_keypad());
            assert!(!screen.application_cursor());
            assert!(!screen.bracketed_paste());
            assert_eq!(screen.mouse_protocol_mode(), vt100::MouseProtocolMode::None);
            assert_eq!(
                screen.mouse_protocol_encoding(),
                vt100::MouseProtocolEncoding::Default
            );
            assert!(!screen.hide_cursor());
        }
    }
}

/// A switch resets the outgoing session's input modes but must leave the
/// two host modes alone: `?1049l` is stripped by the alt-screen hold, but
/// `?1007h` is not, and writing the full detach sequence between sessions
/// re-enabled `alternateScroll` -- the wheel typing arrow keys into the
/// agent -- for the rest of the attach whenever the client did not hold
/// the mouse.
#[test]
fn switch_reset_keeps_the_hosts_own_modes() {
    assert!(
        TERMINAL_RESET_SEQUENCE.starts_with(ATTACH_ALT_SCREEN_EXIT),
        "the switch sequence is derived as the detach sequence's tail"
    );
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = status_ctx_for_test(true);
    let pipe = StdoutToPipe::new();
    feed_and_write(&ctx.stdout, &ctx.screen, SWITCH_RESET_SEQUENCE, b"", None).unwrap();
    let text = String::from_utf8_lossy(&pipe.take()).into_owned();
    assert!(
        !text.contains("\x1b[?1007h") && !text.contains("\x1b[?1049l"),
        "a switch must not touch the host's alt-screen hold or alternateScroll: {text:?}"
    );
    assert!(
        text.contains("\x1b[?1000l") && text.contains("\x1b[?2004l") && text.contains("\x1b[2J"),
        "a switch must still reset the outgoing session's input modes and screen: {text:?}"
    );
}

fn sample_groups() -> Vec<(PathBuf, Vec<SessionRecord>)> {
    let ws_a = "/ws/a";
    let ws_b = "/ws/b";
    let mut a1 = mk_record(ws_a, "main", Phase::Running);
    let mut a2 = mk_record(ws_a, "review", Phase::Running);
    let mut a3 = mk_record(ws_a, "dead", Phase::Exited);
    a1.worker_pid = Some(std::process::id());
    a2.worker_pid = Some(std::process::id());
    a3.worker_pid = None; // exited, unattachable regardless
    let b1 = mk_record(ws_b, "only", Phase::Running);
    vec![
        (PathBuf::from(ws_a), vec![a1, a2, a3]),
        (PathBuf::from(ws_b), vec![b1]),
    ]
}

// -- workspace_summary_regions (docs/clickable-status-bar-design.md
// section 4.2) --

#[test]
fn summary_regions_matches_workspace_summary_text() {
    let groups = sample_groups();
    let siblings = &groups[0].1; // main, review, dead(Exited)
    let current = siblings[0].id;
    let (text, regions) = workspace_summary_regions(siblings, current);
    assert_eq!(text, "1:main* 2:review 3:dead(exited)");
    assert_eq!(regions.len(), 3);
    assert_eq!(regions[0].action, BarClick::Sibling(1));
    assert_eq!(regions[1].action, BarClick::Sibling(2));
    assert_eq!(regions[2].action, BarClick::Sibling(3));
}

#[test]
fn summary_regions_column_ranges_slice_out_the_right_token() {
    let groups = sample_groups();
    let siblings = &groups[0].1;
    let current = siblings[0].id;
    let (text, regions) = workspace_summary_regions(siblings, current);
    let chars: Vec<char> = text.chars().collect();
    for region in &regions {
        let slice: String = chars[region.cols.clone()].iter().collect();
        match region.action {
            BarClick::Sibling(1) => assert_eq!(slice, "1:main*"),
            BarClick::Sibling(2) => assert_eq!(slice, "2:review"),
            BarClick::Sibling(3) => assert_eq!(slice, "3:dead(exited)"),
            _ => panic!("unexpected region {region:?}"),
        }
    }
}

#[test]
fn summary_regions_unicode_tag_uses_display_cells_not_byte_offsets() {
    // A multi-byte tag must not desync the column map -- offsets are
    // display cells (matching pad_or_truncate), not byte counts.
    let a = mk_record("/ws/u", "café", Phase::Running);
    let b = mk_record("/ws/u", "b", Phase::Running);
    let current = a.id;
    let siblings = vec![a, b];
    let (text, regions) = workspace_summary_regions(&siblings, current);
    assert_eq!(text, "1:café* 2:b");
    let chars: Vec<char> = text.chars().collect();
    let second: String = chars[regions[1].cols.clone()].iter().collect();
    assert_eq!(second, "2:b");
}

#[test]
fn summary_regions_count_wide_tags_in_terminal_cells() {
    let a = mk_record("/ws/u", "界", Phase::Running);
    let b = mk_record("/ws/u", "b", Phase::Running);
    let current = a.id;
    let (text, regions) = workspace_summary_regions(&[a, b], current);
    assert_eq!(text, "1:界* 2:b");
    assert_eq!(regions[0].cols, 0..5);
    assert_eq!(regions[1].cols, 6..9);
}

#[test]
fn summary_regions_single_session_still_renders_one_region() {
    // Unlike workspace_summary (which returns "" for a lone session,
    // since there's nothing to switch *to*), the pure builder here
    // doesn't special-case count -- callers decide whether to show the
    // segment at all, same as workspace_summary's caller does today.
    let a = mk_record("/ws/solo", "only", Phase::Running);
    let current = a.id;
    let siblings = vec![a];
    let (text, regions) = workspace_summary_regions(&siblings, current);
    assert_eq!(text, "1:only*");
    assert_eq!(regions.len(), 1);
}
