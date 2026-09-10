use super::*;
use clap::Parser;

#[test]
fn attach_goodbye_distinguishes_detach_error_and_socket_loss() {
    let selector = "/ws:tag";
    assert_eq!(
        attach_goodbye_line(classify_attach_stop(false, true, false), selector, None),
        "Detached from /ws:tag."
    );
    assert_eq!(
        attach_goodbye_line(classify_attach_stop(false, false, true), selector, None),
        "Attach dropped: /ws:tag."
    );
    assert_eq!(
        attach_goodbye_line(classify_attach_stop(false, false, false), selector, None),
        "Connection to /ws:tag lost."
    );
    // Ctrl-b d shuts our own stream down, so the frame loop that follows
    // it usually sees a reset too. Client intent must still win, or every
    // deliberate detach would report a connection loss.
    assert_eq!(
        classify_attach_stop(false, true, true),
        AttachStop::ClientDetached
    );
    // Session end is unchanged, including the two-way split on whether a
    // record survived to be inspected.
    assert_eq!(
        attach_goodbye_line(classify_attach_stop(true, false, false), selector, None),
        "Session ended: /ws:tag."
    );
    assert_eq!(
        attach_goodbye_line(
            classify_attach_stop(true, true, true),
            selector,
            Some("0a1b2c3d")
        ),
        "Session ended: /ws:tag. Inspect output with `a capture 0a1b2c3d --screen --plain`."
    );
}

#[test]
fn control_deadline_bounds_a_silent_worker_and_streaming_can_clear_it() {
    let (mut client, _silent_worker) = UnixStream::pair().unwrap();
    set_control_deadlines(&client).unwrap();
    let started = Instant::now();
    let error = client.read(&mut [0u8; 1]).unwrap_err();
    assert!(matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ));
    assert!(started.elapsed() < Duration::from_secs(1));

    clear_streaming_deadlines(&client).unwrap();
    assert_eq!(client.read_timeout().unwrap(), None);
    assert_eq!(client.write_timeout().unwrap(), None);
}

#[test]
fn connect_deadline_bounds_a_saturated_unix_listener_backlog() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("saturated.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);

    // Linux permits one queued connection for backlog zero. Leave it
    // unaccepted so the next real AF_UNIX connect hits the saturated
    // backlog rather than a synthetic silent-response fixture.
    let _queued = connect_with_timeout(&path, Duration::from_millis(100)).unwrap();
    let started = Instant::now();
    let error = connect_with_timeout(&path, Duration::from_millis(100)).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}");
    assert!(started.elapsed() >= Duration::from_millis(75));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn persisted_history_tail_is_seeked_and_frame_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.bin");
    let mut file = fs::File::create(&path).unwrap();
    file.set_len(1024 * 1024 * 1024).unwrap();
    std::io::Seek::seek(&mut file, std::io::SeekFrom::End(-4)).unwrap();
    file.write_all(b"tail").unwrap();
    drop(file);

    assert_eq!(
        read_persisted_history_tail(&path, Some(4)).unwrap(),
        b"tail"
    );

    let bounded = read_persisted_history_tail(&path, Some(usize::MAX)).unwrap();
    assert_eq!(bounded.len(), MAX_FRAME_BYTES);
    assert_eq!(&bounded[bounded.len() - 4..], b"tail");
}

#[test]
fn parse_hex_rejects_non_ascii_without_panicking() {
    assert!(parse_hex("aéa".as_bytes()).is_err());
}

#[test]
fn ctrl_b_question_mark_flashes_help_without_forwarding() {
    let mut scanner = InputScanner::default();
    let actions = scanner.scan(&[0x02, b'?']);
    assert!(matches!(actions.as_slice(), [InputAction::Help]));
    assert!(bytes(&actions).is_empty());
    // Help is consumed wherever it appears in the stream, and the
    // withheld Ctrl-b of an unbound chord still forwards.
    let mut scanner = InputScanner::default();
    let actions = scanner.scan(&[b'x', 0x02, b'?', b'y']);
    assert_eq!(bytes(&actions), b"xy");
    assert_eq!(actions.len(), 3);
    assert!(matches!(actions[1], InputAction::Help));
}

#[test]
fn ctrl_b_r_redraws_without_forwarding() {
    let mut scanner = InputScanner::default();
    let actions = scanner.scan(&[0x02, b'r']);
    assert!(matches!(actions.as_slice(), [InputAction::Redraw]));
    assert!(bytes(&actions).is_empty());
    let mut scanner = InputScanner::default();
    let actions = scanner.scan(&[b'x', 0x02, b'r', b'y']);
    assert_eq!(bytes(&actions), b"xy");
    assert_eq!(actions.len(), 3);
    assert!(matches!(actions[1], InputAction::Redraw));
    // Capital R is not the chord -- tmux's refresh-client is lowercase.
    let mut scanner = InputScanner::default();
    let actions = scanner.scan(&[0x02, b'R']);
    assert_eq!(bytes(&actions), &[0x02, b'R']);
}

fn bytes(actions: &[InputAction]) -> Vec<u8> {
    let mut out = Vec::new();
    for a in actions {
        if let InputAction::Forward(b) = a {
            out.extend_from_slice(b);
        }
    }
    out
}

#[test]
fn human_commands_parse_as_real_clap_commands() {
    // The terminal-first vocabulary must be real Clap commands and
    // visible aliases -- not argv rewriting -- so generated completions
    // and `a help` know every name.
    let args = args_of(&["here", "codex", "review"]);
    match Cli::try_parse_from(args).unwrap().command {
        Some(Commands::Here(quick)) => {
            assert_eq!(quick.rest, vec!["codex".to_string(), "review".to_string()])
        }
        _ => panic!("expected `here` command"),
    }

    let args = args_of(&["open", "review"]);
    match Cli::try_parse_from(args).unwrap().command {
        Some(Commands::Attach(attach)) => {
            assert_eq!(attach.target.selector.as_deref(), Some("review"))
        }
        _ => panic!("expected `open` alias of attach"),
    }

    let args = args_of(&["attach", "review", "--no-status"]);
    match Cli::try_parse_from(args).unwrap().command {
        Some(Commands::Attach(attach)) => assert!(attach.no_status),
        _ => panic!("expected `attach --no-status` command"),
    }

    let args = args_of(&["new", "--engine", "shell"]);
    match Cli::try_parse_from(args).unwrap().command {
        Some(Commands::New(start)) => assert_eq!(start.engine.as_deref(), Some("shell")),
        _ => panic!("expected `new` command"),
    }

    for (argv, expected) in [
        (vec!["ps"], "list"),
        (vec!["show", "x"], "status"),
        (vec!["current"], "whoami"),
        (vec!["keys"], "hotkeys"),
        (vec!["check"], "doctor"),
    ] {
        let parsed = Cli::try_parse_from(args_of(&argv)).unwrap();
        let name = parsed
            .command
            .map(|command| command_name(&command).to_string())
            .unwrap_or_else(|| "<none>".to_string());
        assert_eq!(name, expected, "argv {argv:?}");
    }

    // `start`'s terminal-first default tag, shared with `a here`/`a -`.
    let args = args_of(&["start"]);
    match Cli::try_parse_from(args).unwrap().command {
        Some(Commands::Start(start)) => assert_eq!(start.tag, DEFAULT_HUMAN_TAG),
        _ => panic!("expected start command"),
    }

    let args = args_of(&["list", "--sort", "activity"]);
    match Cli::try_parse_from(args).unwrap().command {
        Some(Commands::List(list)) => assert_eq!(list.sort, Some(ListSort::Activity)),
        _ => panic!("expected list --sort activity"),
    }
}

fn args_of(argv: &[&str]) -> Vec<String> {
    std::iter::once("a")
        .chain(argv.iter().copied())
        .map(str::to_string)
        .collect()
}

fn command_name(command: &Commands) -> &'static str {
    match command {
        Commands::Start(_) => "start",
        Commands::New(_) => "new",
        Commands::Here(_) => "here",
        Commands::List(_) => "list",
        Commands::Snapshot(_) => "snapshot",
        Commands::Attach(_) => "attach",
        Commands::Send(_) => "send",
        Commands::Capture(_) => "capture",
        Commands::Status(_) => "status",
        Commands::Kill(_) => "kill",
        Commands::Forget(_) => "forget",
        Commands::Prune => "prune",
        Commands::Rename(_) => "rename",
        Commands::Engines => "engines",
        Commands::Profiles => "profiles",
        Commands::LaunchSpec(_) => "launch-spec",
        Commands::LaunchExec(_) => "launch-exec",
        Commands::Doctor => "doctor",
        Commands::Init(_) => "init",
        Commands::Whoami => "whoami",
        Commands::StateReport(_) => "state-report",
        Commands::Message(_) => "message",
        Commands::Watch(_) => "watch",
        Commands::Transcript(_) => "transcript",
        Commands::Completions(_) => "completions",
        Commands::Hotkeys => "hotkeys",
        Commands::QuickAttach(_) => "quick-attach",
        Commands::QuickLaunch(_) => "quick-launch",
    }
}

#[test]
fn uuid_like_selector_detection_does_not_consume_normal_tags() {
    assert!(looks_like_uuid_selector("01234567"));
    assert!(looks_like_uuid_selector(
        "01234567-89ab-cdef-0123-456789abcdef"
    ));
    assert!(!looks_like_uuid_selector("review"));
    assert!(!looks_like_uuid_selector("deadbee"));
    // Dashes alone carry no digits; a 7-digit quick index is not a UUID
    // prefix and stays with the quick-attach resolver.
    assert!(!looks_like_uuid_selector("-------"));
    assert!(!looks_like_uuid_selector("1234567"));
}

#[test]
fn compact_elapsed_and_fit_column_render_for_dense_terminals() {
    assert_eq!(compact_elapsed(0), "now");
    assert_eq!(compact_elapsed(4_000), "now");
    assert_eq!(compact_elapsed(59_000), "59s");
    assert_eq!(compact_elapsed(120_000), "2m");
    assert_eq!(compact_elapsed(7_200_000), "2h");
    assert_eq!(compact_elapsed(7_260_000), "2h 1m");
    assert_eq!(compact_elapsed(172_800_000), "2d");
    assert_eq!(compact_elapsed(190_800_000), "2d 5h");
    assert_eq!(human_age_phrase(7_260_000), "2h 1m ago");
    assert_eq!(human_age_phrase(0), "just now");

    assert_eq!(fit_column("abcdefgh", 5), "abcd…");
    // Wide glyphs count display cells, not chars: two CJK glyphs fit a
    // 5-cell column exactly with padding, three must truncate.
    assert_eq!(terminal_display_width(&fit_column("界界界", 5)), 5);
    assert_eq!(fit_column("界界", 5), "界界 ");
}

#[test]
fn group_by_workspace_sorts_by_name_created_accessed_and_activity() {
    let mut zebra = mk_record("/ws/zebra", "main", Phase::Running);
    zebra.created_at_ms = 10;
    zebra.last_accessed_ms = Some(100);
    zebra.last_activity_ms = Some(1);

    let mut apple = mk_record("/ws/apple", "main", Phase::Running);
    apple.created_at_ms = 30;
    apple.last_accessed_ms = Some(50);
    apple.last_activity_ms = Some(200);

    let mut mango = mk_record("/ws/mango", "review", Phase::Running);
    mango.created_at_ms = 20;
    mango.last_accessed_ms = None;
    mango.last_activity_ms = None;

    let records = vec![zebra, apple, mango];
    let names = |sort: ListSort, records: &[SessionRecord]| -> Vec<PathBuf> {
        group_by_workspace(records.to_vec(), sort)
            .into_iter()
            .map(|(ws, _)| ws)
            .collect()
    };

    assert_eq!(
        names(ListSort::Name, &records),
        vec![
            PathBuf::from("/ws/apple"),
            PathBuf::from("/ws/mango"),
            PathBuf::from("/ws/zebra")
        ]
    );
    assert_eq!(
        names(ListSort::Created, &records),
        vec![
            PathBuf::from("/ws/apple"),
            PathBuf::from("/ws/mango"),
            PathBuf::from("/ws/zebra")
        ]
    );
    assert_eq!(
        names(ListSort::Accessed, &records),
        vec![
            PathBuf::from("/ws/zebra"),
            PathBuf::from("/ws/apple"),
            PathBuf::from("/ws/mango")
        ]
    );
    assert_eq!(
        names(ListSort::Activity, &records),
        vec![
            PathBuf::from("/ws/apple"),
            PathBuf::from("/ws/zebra"),
            PathBuf::from("/ws/mango")
        ]
    );
}

#[test]
fn ui_state_is_semantic_when_reported_and_honest_when_inferred() {
    let now: u64 = 20_000;
    let mut record = mk_record("/ws/state", "agent", Phase::Running);
    record.engine = "codex".to_string();

    // A fresh state-report push is semantic fact.
    record.reported_state = Some("waiting".to_string());
    record.reported_state_at_ms = Some(now - 500);
    assert_eq!(session_ui_state(&record, now), ("waiting", "reported"));
    assert!(ui_state_needs_attention("waiting"));
    assert!(ui_state_is_active("waiting"));

    record.reported_state = Some("working".to_string());
    assert_eq!(session_ui_state(&record, now), ("working", "reported"));
    record.reported_state = Some("idle".to_string());
    assert_eq!(session_ui_state(&record, now), ("idle", "reported"));
    assert!(!ui_state_needs_attention("idle"));

    // Stale push: only PTY-recency evidence, so only activity words.
    record.reported_state = Some("waiting".to_string());
    record.reported_state_at_ms = Some(now.saturating_sub(60_000));
    record.last_activity_ms = Some(now - 500);
    assert_eq!(session_ui_state(&record, now), ("active", "activity"));
    record.last_activity_ms = Some(now - 5_000);
    assert_eq!(session_ui_state(&record, now), ("quiet", "activity"));
    // Quiet is deliberately not attention: silence is not a reported wait.
    assert!(!ui_state_needs_attention("quiet"));
}

#[test]
fn ui_state_does_not_guess_agent_semantics_for_shells_or_corpses() {
    let now: u64 = 20_000;
    // A plain shell (no agent-state push ever) stays `running` however
    // quiet its PTY is.
    let mut record = mk_record("/ws/state", "shell", Phase::Running);
    record.last_activity_ms = Some(now.saturating_sub(60_000));
    assert_eq!(session_ui_state(&record, now), ("running", "lifecycle"));

    // But a fresh hook push inside a shell session is fact, not a
    // guess: the agent was started by hand, `APLEXER_SESSION_ID` is
    // still injected, and without this an idle agent in a shell
    // session would read `running` forever.
    record.reported_state = Some("idle".to_string());
    record.reported_state_at_ms = Some(now);
    assert_eq!(session_ui_state(&record, now), ("idle", "reported"));
    record.reported_state = Some("waiting".to_string());
    assert_eq!(session_ui_state(&record, now), ("waiting", "reported"));
    record.reported_state = Some("working".to_string());
    assert_eq!(session_ui_state(&record, now), ("working", "reported"));

    // A stale push no longer falls back to plain `running`: a shell an
    // agent has lived in gets the same activity words as a first-class
    // engine once nothing is fresh. Its long-quiet PTY is an agent
    // resting at a prompt (or thinking), not a shell doing work.
    record.reported_state_at_ms = Some(now.saturating_sub(60_000));
    assert_eq!(session_ui_state(&record, now), ("quiet", "activity"));

    // And an idle push with no PTY output since it landed stays
    // authoritative however old it gets -- a rest has no follow-up
    // push to refresh it, so expiring it on the clock is what made
    // resting agents read RUNNING.
    record.reported_state = Some("idle".to_string());
    assert_eq!(session_ui_state(&record, now), ("idle", "reported"));

    // Non-terminal phase + dead worker = broken, regardless of what the
    // record still claims or what was last reported.
    let mut corpse = mk_record("/ws/state", "agent", Phase::Running);
    corpse.engine = "codex".to_string();
    corpse.worker_pid = None;
    corpse.reported_state = Some("working".to_string());
    corpse.reported_state_at_ms = Some(now);
    assert_eq!(session_ui_state(&corpse, now), ("broken", "lifecycle"));
    assert!(ui_state_needs_attention("broken"));
    // ... but a Starting record with no worker pid yet is the shape
    // every healthy `a start` persists first, and the TTY UI must not
    // paint that as a corpse (issue #9). Age is the only thing that
    // turns it into one.
    let mut creating = mk_record("/ws/state", "creating", Phase::Starting);
    creating.worker_pid = None;
    creating.created_at_ms = now;
    assert_eq!(
        session_ui_state(&creating, now + 1),
        ("starting", "lifecycle")
    );
    assert_eq!(
        session_ui_state(&creating, now + DEFAULT_STARTUP_TIMEOUT_MS - 1),
        ("starting", "lifecycle")
    );
    assert_eq!(
        session_ui_state(&creating, now + DEFAULT_STARTUP_TIMEOUT_MS),
        ("broken", "lifecycle")
    );
}

/// The default list's corpse filter: `exited` is the one state that is
/// neither active nor needs-attention, so it is the only one hidden.
/// The failure states a human may need to see (oom, failed, broken) and
/// a healthy Starting session that has not registered its worker pid
/// yet (issue #9's startup window) all stay listed.
#[test]
fn only_exited_sessions_drop_out_of_the_default_list() {
    let now = now_ms();
    let mut exited = mk_record("/ws/f", "done", Phase::Exited);
    exited.worker_pid = None;
    assert!(!session_is_listed(&exited, now));

    let mut oom = mk_record("/ws/f", "oomed", Phase::Exited);
    oom.worker_pid = None;
    oom.exit = Some(aplexer::ExitInfo {
        code: None,
        signal: None,
        oom_killed: true,
        exited_at_ms: now,
    });
    assert!(
        session_is_listed(&oom, now),
        "oom needs attention, not a corpse"
    );

    let failed = mk_record("/ws/f", "failed", Phase::Failed);
    assert!(session_is_listed(&failed, now));

    let mut broken = mk_record("/ws/f", "broken", Phase::Running);
    broken.worker_pid = None;
    assert!(session_is_listed(&broken, now));

    let mut creating = mk_record("/ws/f", "creating", Phase::Starting);
    creating.worker_pid = None;
    creating.created_at_ms = now;
    assert!(
        session_is_listed(&creating, now + 1),
        "the startup window must not read as a corpse (issue #9)"
    );
}

#[test]
fn overlay_reported_state_takes_the_live_activity_stamp_too() {
    let now: u64 = 200_000;
    let mut record = mk_record("/ws/state", "shell", Phase::Running);
    // Attach-time snapshot: output predates the attach; nothing has
    // been reported since.
    record.last_activity_ms = Some(now - 30_000);

    // The worker's live answer: the agent has since said `idle`, and
    // the PTY has been silent since the push. The rest is authoritative
    // even though the snapshot itself knows nothing of it.
    let raw = json!({
        "reported_state": "idle",
        "reported_state_at_ms": now - 1_000,
        "last_activity_ms": now - 2_000,
    });
    let overlaid = overlay_reported_state(&record, Some(&raw));
    assert_eq!(
        session_ui_state(&overlaid, now),
        ("idle", "reported"),
        "a rest with no output since the push is idle, however stale the attach snapshot"
    );

    // The same rest, but the live activity stamp says output arrived
    // after it: the snapshot's old stamp must not keep the idle claim
    // alive once the agent (or the user at the prompt) produced output.
    let raw = json!({
        "reported_state": "idle",
        "reported_state_at_ms": now - 5_000,
        "last_activity_ms": now - 500,
    });
    let overlaid = overlay_reported_state(&record, Some(&raw));
    assert_eq!(
        session_ui_state(&overlaid, now),
        ("active", "activity"),
        "newer live output retracts the rest even though the snapshot predates it"
    );

    // A failed Status RPC leaves the snapshot untouched...
    let untouched = overlay_reported_state(&record, None);
    assert_eq!(untouched.reported_state, None);
    assert_eq!(untouched.last_activity_ms, Some(now - 30_000));

    // ...and so does a worker too old to send the activity field.
    let old_worker = json!({ "reported_state": "idle" });
    let degraded = overlay_reported_state(&record, Some(&old_worker));
    assert_eq!(degraded.reported_state.as_deref(), Some("idle"));
    assert_eq!(degraded.last_activity_ms, Some(now - 30_000));
}

/// An accepted kill persists `phase: exiting` before teardown (issue
/// #18), so the whole finalization window must render as a dying
/// session, not as healthy -- and once the worker dies mid-finalization
/// it must fall through to `broken`, the shape prune reaps, never back
/// to a live-looking word.
#[test]
fn ui_state_shows_a_killed_session_as_stopping_while_it_dies() {
    let now: u64 = 20_000;
    // Worker still alive mid-finalization (the kill window, however long
    // finalization takes): "stopping", not "running".
    let dying = mk_record("/ws/state", "dying", Phase::Exiting);
    assert_eq!(session_ui_state(&dying, now), ("stopping", "lifecycle"));

    // Worker gone before finalization wrote a terminal phase: the
    // contradicted-phase rule owns the row now.
    let mut corpse = mk_record("/ws/state", "dying", Phase::Exiting);
    corpse.worker_pid = None;
    assert_eq!(session_ui_state(&corpse, now), ("broken", "lifecycle"));
}

/// Issue #9's second half. The worker persists the record, then its pid,
/// then binds the control socket -- so a client racing a healthy start
/// finds either no pid or no socket. Both used to be reported as
/// destroyed state with `a kill` as the remedy.
#[test]
fn check_attachable_does_not_advise_killing_a_still_starting_session() {
    let now = now_ms();

    // Before the worker registers a pid.
    let mut pre_pid = mk_record("/ws/a", "main", Phase::Starting);
    pre_pid.worker_pid = None;
    pre_pid.created_at_ms = now;
    let err = check_attachable(&pre_pid).unwrap_err().to_string();
    assert!(err.contains("still starting"), "{err}");
    assert!(!err.contains("a kill"), "{err}");

    // Pid registered, socket not bound yet.
    let mut pre_socket = mk_record("/ws/a", "main", Phase::Starting);
    pre_socket.created_at_ms = now;
    pre_socket.socket_path = PathBuf::from("/definitely/does/not/exist/control.sock");
    let err = check_attachable(&pre_socket).unwrap_err().to_string();
    assert!(err.contains("still starting"), "{err}");
    assert!(!err.contains("removed out from under it"), "{err}");
    assert!(!err.contains("a kill"), "{err}");

    // Past the startup budget these really are wreckage, and the
    // original advice is the right advice again.
    let mut expired_pre_pid = pre_pid.clone();
    expired_pre_pid.created_at_ms = now.saturating_sub(DEFAULT_STARTUP_TIMEOUT_MS);
    let err = check_attachable(&expired_pre_pid).unwrap_err().to_string();
    assert!(err.contains("worker is not running"), "{err}");
    assert!(err.contains("state: broken"), "{err}");

    let mut expired_pre_socket = pre_socket.clone();
    expired_pre_socket.created_at_ms = now.saturating_sub(DEFAULT_STARTUP_TIMEOUT_MS);
    let err = check_attachable(&expired_pre_socket)
        .unwrap_err()
        .to_string();
    assert!(err.contains("control socket is gone"), "{err}");
    assert!(
        err.contains(&format!("a kill {}", expired_pre_socket.id)),
        "{err}"
    );

    // The guard must not stand in the way of the ordinary path: a
    // Starting session whose worker is up and listening is attachable.
    let mut ready = mk_record("/ws/a", "main", Phase::Starting);
    ready.created_at_ms = now;
    assert!(check_attachable(&ready).is_ok());
}

#[test]
fn scan_ctrl_b_n_asks_for_a_new_session() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, b'n']);
    assert!(matches!(
        actions.as_slice(),
        [InputAction::Switch(SwitchTarget::New)]
    ));
    // Split across reads, like every other chord: the prefix state has to
    // survive the read() boundary.
    let mut split = InputScanner::default();
    assert!(split.scan(&[0x02]).is_empty());
    assert!(matches!(
        split.scan(b"n").as_slice(),
        [InputAction::Switch(SwitchTarget::New)]
    ));
    // And the chord bytes never reach the workload -- pressing it must
    // not type an `n` into whatever has the prompt.
    let mut mixed = InputScanner::default();
    let actions = mixed.scan(&[b'a', 0x02, b'n', b'z']);
    assert_eq!(actions.len(), 3);
    match &actions[0] {
        InputAction::Forward(b) => assert_eq!(b, b"a"),
        _ => panic!("expected the pre-chord byte forwarded"),
    }
    assert!(matches!(actions[1], InputAction::Switch(SwitchTarget::New)));
    match &actions[2] {
        InputAction::Forward(b) => assert_eq!(b, b"z"),
        _ => panic!("expected the post-chord byte forwarded"),
    }
}

/// Session navigation lives on Right/Left and workspace navigation on
/// Down/Up, in *both* encodings a terminal can send them in: CSI
/// (`ESC [ C`) in normal cursor mode and SS3 (`ESC O C`) in application
/// cursor mode, which a TUI in the session can turn on at any moment.
#[test]
fn scan_ctrl_b_arrows_navigate_in_both_cursor_modes() {
    for introducer in [b'[', b'O'] {
        for (final_byte, expected) in [
            (b'C', SwitchTarget::Next),
            (b'D', SwitchTarget::Prev),
            (b'B', SwitchTarget::NextWorkspace),
            (b'A', SwitchTarget::PrevWorkspace),
        ] {
            let mut s = InputScanner::default();
            let actions = s.scan(&[0x02, 0x1b, introducer, final_byte]);
            match actions.as_slice() {
                [InputAction::Switch(target)] => assert_eq!(
                    *target, expected,
                    "ESC {} {} should mean {expected:?}",
                    introducer as char, final_byte as char
                ),
                other => panic!(
                    "ESC {} {} was not consumed as a chord ({} action(s))",
                    introducer as char,
                    final_byte as char,
                    other.len()
                ),
            }
        }
    }
}

/// A terminal writes an escape sequence in one `write`, but a PTY read can
/// still split it anywhere -- every prefix has to survive the boundary,
/// including `Ctrl-b` and `ESC` landing in different reads.
#[test]
fn scan_ctrl_b_arrow_split_across_every_read_boundary() {
    let chord: &[u8] = &[0x02, 0x1b, b'[', b'C'];
    for split in 1..chord.len() {
        let mut s = InputScanner::default();
        let first = s.scan(&chord[..split]);
        assert!(
            first.is_empty(),
            "a partial chord split at {split} must emit nothing yet"
        );
        assert!(
            s.awaiting_escape() || split == 1,
            "split at {split} should leave the scanner holding escape bytes"
        );
        assert!(
            matches!(
                s.scan(&chord[split..]).as_slice(),
                [InputAction::Switch(SwitchTarget::Next)]
            ),
            "a chord split at {split} did not resolve"
        );
    }
}

/// `Ctrl-b` then a bare `ESC` is not a chord: once the input thread stops
/// waiting for the rest of an arrow (`CHORD_ESCAPE_TIMEOUT`), both
/// withheld bytes go to the workload, so an editor still gets its Escape.
#[test]
fn scan_ctrl_b_escape_flushes_when_no_arrow_follows() {
    let mut s = InputScanner::default();
    assert!(s.scan(&[0x02, 0x1b]).is_empty());
    assert!(s.awaiting_escape());
    assert_eq!(bytes(&s.flush_pending()), vec![0x02, 0x1b]);
    assert!(!s.awaiting_escape());
    // Nothing is left behind: the next key is scanned from scratch.
    assert!(matches!(
        s.scan(&[0x02, b'n']).as_slice(),
        [InputAction::Switch(SwitchTarget::New)]
    ));

    // A lone `Ctrl-b` is *not* flushed: waiting for its second key is the
    // keymap's contract, and only the multi-byte arrows are ambiguous.
    let mut lone = InputScanner::default();
    assert!(lone.scan(&[0x02]).is_empty());
    assert!(!lone.awaiting_escape());
    assert!(lone.flush_pending().is_empty());
}

/// An escape sequence after the prefix that is *not* an arrow (Home, F1,
/// ...) forwards every withheld byte in order rather than swallowing any.
#[test]
fn scan_ctrl_b_non_arrow_escape_forwards_every_byte() {
    let mut s = InputScanner::default();
    assert_eq!(bytes(&s.scan(&[0x02, 0x1b, b'[', b'H'])), {
        let mut expected = vec![0x02, 0x1b];
        expected.extend_from_slice(b"[H");
        expected
    });
    let mut alt = InputScanner::default();
    assert_eq!(
        bytes(&alt.scan(&[0x02, 0x1b, b'x'])),
        vec![0x02, 0x1b, b'x']
    );
}

/// `p` was only ever the other half of `n`/`p`. With `n` now meaning
/// "new", a lone "previous" on `p` would be a trap, so it is unbound and
/// forwards -- and `N`/`P` are untouched.
#[test]
fn scan_ctrl_b_p_is_unbound_but_capital_p_still_switches() {
    let mut s = InputScanner::default();
    assert_eq!(bytes(&s.scan(&[0x02, b'p'])), vec![0x02, b'p']);
    assert!(matches!(
        s.scan(&[0x02, b'N']).as_slice(),
        [InputAction::Switch(SwitchTarget::NextGlobal)]
    ));
    assert!(matches!(
        s.scan(&[0x02, b'P']).as_slice(),
        [InputAction::Switch(SwitchTarget::PrevGlobal)]
    ));
}

/// The arrow chords must not have eaten the bracket-ish keys next to
/// them: `[` is still the pager and the digits still jump.
#[test]
fn scan_ctrl_b_bracket_and_digits_survive_the_arrow_chords() {
    let mut s = InputScanner::default();
    assert!(matches!(
        s.scan(&[0x02, b'[']).as_slice(),
        [InputAction::Scroll]
    ));
    assert!(matches!(
        s.scan(&[0x02, b'4']).as_slice(),
        [InputAction::Switch(SwitchTarget::Index(4))]
    ));
    assert!(matches!(
        s.scan(&[0x02, b'l']).as_slice(),
        [InputAction::Switch(SwitchTarget::Last)]
    ));
    assert!(matches!(
        s.scan(&[0x02, b'r']).as_slice(),
        [InputAction::Redraw]
    ));
    assert!(matches!(
        s.scan(&[0x02, b'?']).as_slice(),
        [InputAction::Help]
    ));
}

/// The keymap has exactly one definition; the three renderings are views
/// of it. This is the guard on that: each must mention every bound key,
/// and the table must still spell the bindings the scanner implements.
#[test]
fn the_key_reference_is_generated_from_the_binding_table() {
    let help = attach_key_help();
    assert!(help.starts_with("Ctrl-b: "));
    // The overlay is the third view. Rendered at a size with room for
    // everything, it has to carry every key the table does -- a binding
    // that only reaches two of the three renderings is exactly the drift
    // this table exists to make impossible.
    let overlay = key_overlay_lines(40, 120).expect("40x120 fits the whole keymap");
    for binding in ATTACH_BINDINGS {
        assert!(
            overlay.iter().any(|line| line.contains(binding.keys)),
            "the key overlay dropped {:?}:\n{}",
            binding.keys,
            overlay.join("\n")
        );
        assert!(
            overlay
                .iter()
                .any(|line| line.contains(binding.description)),
            "the key overlay dropped {:?}:\n{}",
            binding.description,
            overlay.join("\n")
        );
    }
    for binding in ATTACH_BINDINGS {
        if let Some(brief) = binding.brief {
            assert!(
                help.contains(brief),
                "the status-bar reference dropped {brief:?}: {help}"
            );
        }
    }
    // The bindings the scanner actually implements, spelled as the table
    // spells them -- a binding added to the scanner and forgotten here (or
    // the reverse) fails this.
    let keys: Vec<&str> = ATTACH_BINDINGS.iter().map(|b| b.keys).collect();
    assert_eq!(
        keys,
        vec![
            "Right / Left",
            "Down / Up",
            "n",
            "d",
            "[",
            "N / P",
            "1-9",
            "l",
            "r",
            "?"
        ]
    );
}

/// The overlay is the third *view* of `ATTACH_BINDINGS`, not a third
/// copy of it: on a terminal with room for the whole table, every key and
/// every description the table holds is on screen verbatim. A binding
/// added to the scanner and the table shows up here for free; one written
/// out by hand could not.
#[test]
fn key_overlay_renders_every_binding_from_the_table() {
    let lines = key_overlay_lines(40, 100).expect("40x100 has room for the whole keymap");
    assert_eq!(
        lines.len(),
        ATTACH_BINDINGS.len() + KEY_OVERLAY_CHROME_ROWS,
        "every binding gets a row, plus two borders and the footer: {lines:#?}"
    );
    for (binding, line) in ATTACH_BINDINGS.iter().zip(&lines[1..]) {
        assert!(
            line.contains(binding.keys),
            "the overlay dropped the keys {:?}: {line}",
            binding.keys
        );
        assert!(
            line.contains(binding.description),
            "the overlay truncated {:?} on a terminal with room for it: {line}",
            binding.description
        );
    }
    assert!(
        lines[0].contains("Ctrl-b"),
        "the box has to name the key the user is waiting on: {}",
        lines[0]
    );
}

/// Every row is one uniform width that fits inside the terminal, and the
/// box never claims more rows than it was given. This is the "do not draw
/// outside the screen" guarantee stated over the layout rather than left
/// to the sequence writer, because the sequence addresses rows absolutely
/// and a box one row too tall would land on the status bar.
#[test]
fn key_overlay_lines_are_uniform_and_stay_inside_the_terminal() {
    for rows in [6usize, 9, 13, 23, 40, 200] {
        for cols in [32usize, 40, 46, 80, 100, 200] {
            let Some(lines) = key_overlay_lines(rows, cols) else {
                continue;
            };
            assert!(
                lines.len() <= rows,
                "a {rows}x{cols} box claimed {} rows",
                lines.len()
            );
            let width = terminal_display_width(&lines[0]);
            assert!(width <= cols, "a {rows}x{cols} box is {width} cells wide");
            for line in &lines {
                assert_eq!(
                    terminal_display_width(line),
                    width,
                    "ragged row in a {rows}x{cols} box: {line}"
                );
            }
        }
    }
}

/// Honest degradation, part one: a terminal with room for some of the
/// keymap gets some of it -- trimmed from the end, because the table is
/// ordered most-useful-first -- and is told how much it is not seeing.
#[test]
fn key_overlay_trims_from_the_end_and_says_how_much_it_dropped() {
    let rows = KEY_OVERLAY_CHROME_ROWS + 4;
    let lines = key_overlay_lines(rows, 80).expect("four bindings still fit");
    assert_eq!(lines.len(), rows);
    let dropped = ATTACH_BINDINGS.len() - 4;
    let footer = &lines[lines.len() - 2];
    assert!(
        footer.contains(&format!("{dropped} more")),
        "a trimmed box must say how many bindings it dropped: {footer}"
    );
    for binding in &ATTACH_BINDINGS[..4] {
        assert!(
            lines[1..5].iter().any(|l| l.contains(binding.keys)),
            "the first four table entries are the ones kept, missing {:?}",
            binding.keys
        );
    }
    let full = key_overlay_lines(40, 80).expect("40 rows fit everything");
    assert!(
        full[full.len() - 2].contains("Esc dismiss"),
        "an untrimmed box says how to get out, not how much is missing: {}",
        full[full.len() - 2]
    );
}

/// Honest degradation, part two: below a box worth drawing there is no
/// box. `show_key_overlay` reads this `None` as "flash the one-line
/// reference instead", which is the whole of the small-terminal story --
/// no half-drawn border, no writing past the last column.
#[test]
fn key_overlay_refuses_a_terminal_it_cannot_fit() {
    // Too short: chrome plus fewer than KEY_OVERLAY_MIN_BINDINGS rows.
    for rows in 0..KEY_OVERLAY_CHROME_ROWS + KEY_OVERLAY_MIN_BINDINGS {
        assert!(
            key_overlay_lines(rows, 200).is_none(),
            "{rows} rows is not enough for a box worth reading"
        );
    }
    assert!(key_overlay_lines(KEY_OVERLAY_CHROME_ROWS + KEY_OVERLAY_MIN_BINDINGS, 200).is_some());
    // Too narrow: the description column would stop being sentences.
    let keys_width = ATTACH_BINDINGS[..KEY_OVERLAY_MIN_BINDINGS]
        .iter()
        .map(|b| terminal_display_width(b.keys))
        .max()
        .unwrap();
    let narrowest = keys_width + 6 + KEY_OVERLAY_MIN_DESC;
    assert!(key_overlay_lines(40, narrowest - 1).is_none());
    assert!(key_overlay_lines(40, narrowest).is_some());
    assert!(key_overlay_lines(40, 0).is_none());
}

/// The sequence addresses rows absolutely, so the rows it addresses are
/// the guarantee: never row 0, never the reserved status row, never past
/// the bottom of the terminal.
#[test]
fn key_overlay_sequence_never_addresses_the_status_bar_row() {
    for (rows, reserved) in [(24u16, true), (24, false), (13, true), (40, true)] {
        let geom = TermGeom {
            rows,
            cols: 80,
            reserved,
        };
        let usable = key_overlay_rows(geom);
        let lines = key_overlay_lines(usable as usize, 80).expect("80 columns fit a box");
        let seq = key_overlay_sequence(geom, &lines);
        let text = String::from_utf8(seq).expect("the sequence is utf-8");
        let mut addressed: Vec<u16> = Vec::new();
        let mut rest = text.as_str();
        while let Some(at) = rest.find("\x1b[") {
            rest = &rest[at + 2..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() && rest[digits.len()..].starts_with(";1H") {
                addressed.push(digits.parse().expect("a row number"));
            }
        }
        assert_eq!(
            addressed.len(),
            lines.len(),
            "one absolute address per row, got {addressed:?}"
        );
        assert_eq!(*addressed.first().unwrap(), usable - lines.len() as u16 + 1);
        assert_eq!(
            *addressed.last().unwrap(),
            usable,
            "the box sits directly above the status bar"
        );
        for row in addressed {
            assert!(
                (1..=usable).contains(&row),
                "the box addressed row {row} on a {rows}-row terminal (usable {usable})"
            );
        }
    }
}

/// The two deadlines a held prefix can be under -- `KEY_OVERLAY_DELAY`
/// and `CHORD_ESCAPE_TIMEOUT` -- are armed by mutually exclusive scanner
/// states, which is what stops them fighting: a partial arrow chord can
/// never pop the overlay, and a bare `Ctrl-b ESC` never waits for one
/// deadline plus the other.
#[test]
fn the_overlay_deadline_and_the_chord_deadline_are_never_armed_together() {
    let mut s = InputScanner::default();
    assert!(s.settled(), "an idle scanner is under neither deadline");
    assert!(!s.awaiting_key() && !s.awaiting_escape());

    assert!(s.scan(&[0x02]).is_empty());
    assert!(
        s.awaiting_key(),
        "a lone Ctrl-b arms the overlay's deadline"
    );
    assert!(!s.awaiting_escape());
    assert!(!s.settled());

    // The moment the arrow's ESC arrives the overlay's deadline is gone
    // and the chord's is the only one left.
    assert!(s.scan(&[0x1b]).is_empty());
    assert!(s.awaiting_escape());
    assert!(!s.awaiting_key());
    assert!(!s.settled());

    // ... and completing the chord leaves neither armed.
    assert!(matches!(
        s.scan(b"[C").as_slice(),
        [InputAction::Switch(SwitchTarget::Next)]
    ));
    assert!(s.settled());
    assert!(!s.awaiting_key() && !s.awaiting_escape());

    // A bound key resolves the prefix in one step, which is what makes
    // the input thread take the overlay down before running its action.
    let mut fast = InputScanner::default();
    assert!(matches!(
        fast.scan(&[0x02, b'd']).as_slice(),
        [InputAction::Detach]
    ));
    assert!(fast.settled());
    // So does an unbound one, which also still falls through.
    let mut through = InputScanner::default();
    assert_eq!(bytes(&through.scan(&[0x02, b'p'])), vec![0x02, b'p']);
    assert!(through.settled());
}

/// The delay has to be long enough that a chord typed from muscle memory
/// resolves first, and its whole point is that it is a *different* wait
/// from the arrow chord's -- long enough to read as hesitation where 100ms
/// reads as a split escape sequence.
#[test]
fn the_overlay_delay_is_a_hesitation_not_a_chord_gap() {
    assert!(
        KEY_OVERLAY_DELAY > CHORD_ESCAPE_TIMEOUT,
        "an overlay that can fire inside the arrow chord's own deadline \
             would flicker on every Ctrl-b Left"
    );
    assert!(
        KEY_OVERLAY_DELAY < FLASH_DURATION,
        "hesitation has to be answered faster than a message is read"
    );
}

/// `SwitchTarget::New` is created, never selected: `pick_switch_target`
/// must say so rather than quietly resolving somewhere.
#[test]
fn pick_switch_target_refuses_to_select_a_new_session() {
    let ws = PathBuf::from("/ws/new");
    let a = mk_record("/ws/new", "a", Phase::Running);
    let groups = vec![(ws.clone(), vec![a.clone()])];
    let error = pick_switch_target(&groups, &ws, a.id, SwitchTarget::New, None)
        .expect_err("New must not be selectable");
    assert!(
        format!("{error:#}").contains("created, not selected"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn scan_ctrl_q_through_ctrl_y_are_forwarded() {
    let mut s = InputScanner::default();
    for byte in 0x11u8..=0x19 {
        let actions = s.scan(&[byte]);
        assert_eq!(bytes(&actions), vec![byte]);
    }
}

#[test]
fn scan_control_bytes_preserve_input_order() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[b'a', 0x13, b'z']);
    assert_eq!(bytes(&actions), vec![b'a', 0x13, b'z']);
}

#[test]
fn scan_non_shortcut_control_bytes_still_forward() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x10, 0x1a, b'1', b'9']);
    assert_eq!(bytes(&actions), vec![0x10, 0x1a, b'1', b'9']);
    assert!(actions
        .iter()
        .all(|action| matches!(action, InputAction::Forward(_))));
}

#[test]
fn scan_split_across_reads() {
    let mut s = InputScanner::default();
    assert!(s.scan(&[0x02]).is_empty());
    let actions = s.scan(b"N");
    assert!(matches!(
        actions.as_slice(),
        [InputAction::Switch(SwitchTarget::NextGlobal)]
    ));
}

#[test]
fn scan_unbound_ctrl_b_forwards_both_bytes() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, b'x']);
    match actions.as_slice() {
        [InputAction::Forward(b)] => assert_eq!(b, &[0x02, b'x']),
        other => panic!("unexpected: {}", other.len()),
    }
}

#[test]
fn scan_forward_switch_forward() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[b'a', 0x02, b'3', b'z']);
    assert_eq!(actions.len(), 3);
    match &actions[0] {
        InputAction::Forward(b) => assert_eq!(b, b"a"),
        _ => panic!("expected Forward"),
    }
    assert!(matches!(
        &actions[1],
        InputAction::Switch(SwitchTarget::Index(3))
    ));
    match &actions[2] {
        InputAction::Forward(b) => assert_eq!(b, b"z"),
        _ => panic!("expected Forward"),
    }
}

#[test]
fn scan_ctrl_b_d_detaches() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, b'd']);
    assert!(matches!(actions.as_slice(), [InputAction::Detach]));
}

#[test]
fn scan_ctrl_bracket_forwards_rest() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[b'a', 0x1d, b'b', b'c']);
    match &actions[0] {
        InputAction::Forward(b) => assert_eq!(b, &[b'a', 0x1d, b'b', b'c']),
        _ => panic!("expected Forward"),
    }
}

#[test]
fn scan_double_ctrl_b_then_d() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, 0x02, b'd']);
    assert_eq!(actions.len(), 2);
    match &actions[0] {
        InputAction::Forward(b) => assert_eq!(b, &[0x02]),
        _ => panic!("expected Forward"),
    }
    assert!(matches!(&actions[1], InputAction::Detach));
}

#[test]
fn scan_ctrl_b_zero_forwards_both() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, b'0']);
    assert_eq!(bytes(&actions), vec![0x02, b'0']);
}

// -- scroll mode: key decoding and offset arithmetic -------------------

/// The property the whole mode rests on: in scroll mode `scroll_keys`
/// classifies every byte as either a command or `Ignored`, and both are
/// *consumed*. There is no third answer that could let a keystroke reach
/// the workload.
#[test]
fn scroll_mode_consumes_every_byte_it_is_given() {
    // Ordinary typing, control characters, a paste, an unknown CSI, a
    // non-wheel mouse report, UTF-8.
    for chunk in [
        b"hello world".as_slice(),
        b"\x7f\x0d\x0a\x09".as_slice(),
        b"\x1b[200~pasted text\x1b[201~".as_slice(),
        b"\x1b[1;2R".as_slice(),
        b"\x1b[<0;10;5M".as_slice(),
        "naïve — ünïcode".as_bytes(),
        b"\x1b[Z\x1bOP\x1b[15~".as_slice(),
    ] {
        let mut i = 0;
        let mut guard = 0;
        while i < chunk.len() {
            guard += 1;
            assert!(guard < 1000, "scroll_keys made no progress on {chunk:?}");
            match scroll_keys(&chunk[i..]) {
                ScrollKey::Command(_, n) | ScrollKey::Ignored(n) => {
                    assert!(n > 0, "a zero-length consume would spin forever");
                    i += n;
                }
                ScrollKey::Incomplete => panic!(
                    "a complete chunk must never be Incomplete: {:?} at {i}",
                    String::from_utf8_lossy(chunk)
                ),
            }
        }
        assert_eq!(i, chunk.len(), "consumed past the end of {chunk:?}");
    }
}

#[test]
fn scroll_keys_navigation_bindings() {
    use ScrollCommand::*;
    for (bytes, expected) in [
        (b"\x1b[5~".as_slice(), PageUp),
        (b"\x1b[6~".as_slice(), PageDown),
        (b"\x1b[5;2~".as_slice(), PageUp), // shifted PageUp
        (b"\x1b[A".as_slice(), Up(1)),
        (b"\x1b[B".as_slice(), Down(1)),
        (b"\x1bOA".as_slice(), Up(1)),
        (b"\x1bOB".as_slice(), Down(1)),
        (b"\x1b[H".as_slice(), Top),
        (b"\x1b[F".as_slice(), Bottom),
        (b"\x1b[1~".as_slice(), Top),
        (b"\x1b[4~".as_slice(), Bottom),
        (b"k".as_slice(), Up(1)),
        (b"j".as_slice(), Down(1)),
        (b" ".as_slice(), PageDown),
        (b"b".as_slice(), PageUp),
        (b"u".as_slice(), HalfUp),
        (b"d".as_slice(), HalfDown),
        (b"g".as_slice(), Top),
        (b"G".as_slice(), Bottom),
        (b"q".as_slice(), Exit),
        (b"\x03".as_slice(), Exit),
        (b"\x1b".as_slice(), Exit),
        (b"\x1b[<64;10;5M".as_slice(), Up(WHEEL_LINES)),
        (b"\x1b[<65;10;5M".as_slice(), Down(WHEEL_LINES)),
    ] {
        match scroll_keys(bytes) {
            ScrollKey::Command(command, consumed) => {
                assert_eq!(
                    command,
                    expected,
                    "for {:?}",
                    String::from_utf8_lossy(bytes)
                );
                assert_eq!(
                    consumed,
                    bytes.len(),
                    "for {:?}",
                    String::from_utf8_lossy(bytes)
                );
            }
            other => panic!("{:?} gave {other:?}", String::from_utf8_lossy(bytes)),
        }
    }
}

/// A wheel *release* report (`m`) must not move the view a second time,
/// or one notch would scroll twice as far as tmux's.
#[test]
fn scroll_keys_wheel_release_is_swallowed_not_repeated() {
    assert_eq!(
        scroll_keys(b"\x1b[<64;10;5m"),
        ScrollKey::Ignored(b"\x1b[<64;10;5m".len())
    );
}

/// An arrow key or mouse report arriving in two `read()`s is held, not
/// mistaken for something else.
#[test]
fn scroll_keys_split_sequences_are_incomplete_not_misread() {
    assert_eq!(scroll_keys(b"\x1b["), ScrollKey::Incomplete);
    assert_eq!(scroll_keys(b"\x1bO"), ScrollKey::Incomplete);
    assert_eq!(scroll_keys(b"\x1b[<64;10"), ScrollKey::Incomplete);
    // ...but not forever: a stray `ESC [` followed by junk is bounded.
    let long = [b"\x1b[".as_slice(), &[b'1'; 40]].concat();
    assert_eq!(scroll_keys(&long), ScrollKey::Ignored(long.len()));
}

/// The bar has to name the mode and the position at every width, because
/// it is the only thing telling the user where their keystrokes go.
#[test]
fn scroll_bar_text_keeps_the_mode_and_position_at_every_width() {
    let view = ScrollView {
        offset: 12,
        available: 2000,
    };
    for cols in [10usize, 20, 40, 80, 200] {
        let text = scroll_bar_text(view, cols, false);
        assert_eq!(
            terminal_display_width(&text),
            cols,
            "the bar must fill exactly its row at {cols} columns"
        );
        assert!(
            text.contains("SCROLL"),
            "the mode must be named at {cols} columns, got {text:?}"
        );
        if cols >= 20 {
            assert!(
                text.contains("12/2000"),
                "the position must survive at {cols} columns, got {text:?}"
            );
        }
    }
}

/// The pager must stay anchored to its content while the workload
/// streams behind it. `offset` counts lines above the live screen, so
/// without compensation every line the agent scrolls off drags the page
/// the user is reading that much closer to the bottom -- a streaming
/// reply yanks the reader back down mid-conversation, which is the
/// tmux-parity complaint that opened the pager in the first place.
#[test]
fn reanchor_view_grows_the_offset_by_lines_that_arrived_behind_the_pager() {
    let mut view = ScrollView {
        offset: 5,
        available: 10,
    };
    reanchor_view(&mut view, 17);
    assert_eq!(
        view.offset, 12,
        "seven new lines must push the view seven lines further back"
    );
    assert_eq!(view.available, 17);
}

/// The live screen is the anchor itself: at offset 0 the pager shows
/// whatever is newest, and no compensation is due.
#[test]
fn reanchor_view_leaves_the_live_offset_at_the_bottom() {
    let mut view = ScrollView {
        offset: 0,
        available: 10,
    };
    reanchor_view(&mut view, 25);
    assert_eq!(view.offset, 0);
    assert_eq!(view.available, 25);
}

/// `available` can also shrink under the view (a rebuild the guard
/// adopted, a resize); the baseline must follow it without inventing
/// compensation out of a negative growth. The offset is left alone --
/// `scrolled_frame` clamps it at render time.
#[test]
fn reanchor_view_tolerates_available_shrinking() {
    let mut view = ScrollView {
        offset: 5,
        available: 10,
    };
    reanchor_view(&mut view, 4);
    assert_eq!(view.offset, 5);
    assert_eq!(view.available, 4);
}

/// An empty pager must say *why* it is empty, in both of the two ways a
/// pager can be empty. The primary-screen case used to show the generic
/// "PgUp/PgDn" hint over a pager that could not move, which reads as a
/// broken feature rather than an answer -- and it is the case a
/// full-screen TUI that repaints in place produces, which is most of what
/// runs under aplexer.
#[test]
fn an_empty_pager_says_why_it_is_empty() {
    let empty = ScrollView {
        offset: 0,
        available: 0,
    };
    let alt = scroll_bar_text(empty, 120, true);
    assert!(
        alt.contains("no history: the workload owns the screen"),
        "an alt-screen workload's empty pager must name the reason: {alt:?}"
    );
    let primary = scroll_bar_text(empty, 120, false);
    assert!(
        primary.contains("no history"),
        "an empty pager on the primary screen must say so too, not offer \
             navigation keys that cannot do anything: {primary:?}"
    );
    assert!(
        !primary.contains("the workload owns the screen"),
        "...and must not blame the alternate screen when it is not in use: {primary:?}"
    );
    // The reason has to survive an ordinary terminal, not just a wide one.
    // It used to be the first thing the width ladder dropped, which left
    // exactly the row the user reported: `SCROLL 0/0 · q live`, with no
    // hint that the emptiness was the answer rather than a failure.
    for cols in [80usize, 100, 200] {
        for alt in [true, false] {
            let text = scroll_bar_text(empty, cols, alt);
            assert!(
                text.contains("no history"),
                "the reason must fit a {cols}-column terminal (alt={alt}): {text:?}"
            );
            assert!(
                !text.contains("PgUp"),
                "a pager that cannot move must not offer keys to move it: {text:?}"
            );
            assert_eq!(terminal_display_width(&text), cols);
        }
    }
    // A pager with history says nothing of the sort, at any width.
    for cols in [40usize, 80, 200] {
        let full = scroll_bar_text(
            ScrollView {
                offset: 0,
                available: 900,
            },
            cols,
            false,
        );
        assert!(
            !full.contains("no history"),
            "a pager with 900 lines behind it must not apologise: {full:?}"
        );
    }
}

/// A pager whose "back to live" gesture needs a second keystroke to
/// actually hand the keyboard back is the confusion this mode exists to
/// avoid, so scrolling down past the live screen leaves the mode.
#[test]
fn scrolling_down_past_the_live_screen_leaves_scroll_mode() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    ctx.scroll.active.store(true, Ordering::SeqCst);
    *ctx.scroll
        .view
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = ScrollView {
        offset: 2,
        available: 100,
    };
    apply_scroll_command(&ctx, ScrollCommand::Down(1));
    assert!(
        ctx.scroll.is_active(),
        "one line up from the bottom is still the pager"
    );
    apply_scroll_command(&ctx, ScrollCommand::Down(5));
    assert!(
        !ctx.scroll.is_active(),
        "hitting the bottom hands the keyboard back to the session"
    );
}

/// `Ctrl-b [` opens the pager without moving it, and without immediately
/// closing it again -- the `Stay` command exists for exactly that.
#[test]
fn ctrl_b_bracket_opens_the_pager_at_the_live_screen() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    enter_scroll_mode(&ctx, ScrollCommand::Stay);
    assert!(ctx.scroll.is_active());
    assert_eq!(
        ctx.scroll
            .view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .offset,
        0
    );
    apply_scroll_command(&ctx, ScrollCommand::Exit);
    assert!(!ctx.scroll.is_active());
}

#[test]
fn scan_ctrl_b_bracket_opens_scroll_mode() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, b'[']);
    assert!(matches!(actions.as_slice(), [InputAction::Scroll]));
}

/// The routing layer, not just the decoder: with the pager up, a chunk
/// of ordinary typing comes back empty -- nothing to send to the
/// workload.
#[test]
fn scroll_input_forwards_nothing_while_the_pager_is_up() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    let mut input = ScrollInput::default();
    assert_eq!(
        input.route(&ctx, b"ls -la\r"),
        b"ls -la\r".to_vec(),
        "with the pager down and no mouse borrowed, input is untouched"
    );
    enter_scroll_mode(&ctx, ScrollCommand::Stay);
    for chunk in [
        b"ls -la\r".as_slice(),
        b"\x1b[A".as_slice(),
        b"XXNOTINPUTXX".as_slice(),
        b"\x1b[<0;3;4M".as_slice(),
        b"rm -rf /\r".as_slice(),
    ] {
        assert!(
            input.route(&ctx, chunk).is_empty(),
            "{:?} must not reach the workload",
            String::from_utf8_lossy(chunk)
        );
        if !ctx.scroll.is_active() {
            // A chunk containing a downward move at the live screen
            // (Space is PageDown) legitimately closes the pager -- and
            // the assertion above is the important half: the *rest* of
            // that chunk is discarded rather than typed into the
            // session. Reopen for the next case.
            enter_scroll_mode(&ctx, ScrollCommand::Stay);
        }
    }
}

/// A failed history refresh must not take the pager down with it. The
/// model here has seen a DECSTBM sub-range (the gate fires) and the test
/// record's socket path is a regular file (every RPC fails fast), which
/// is exactly the "worker went away between the keystroke and the
/// rebuild" case: `Ctrl-b [` still opens the pager on whatever the live
/// model has, which is the pre-refresh behavior.
#[test]
fn pager_entry_survives_a_failed_history_refresh() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    feed_test_screen(&ctx.screen, b"\x1b[3;23r");
    assert!(
        ctx.screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .subregion_seen(),
        "precondition: the gate has to fire, or this tests nothing"
    );
    enter_scroll_mode(&ctx, ScrollCommand::Stay);
    assert!(ctx.scroll.is_active());
    apply_scroll_command(&ctx, ScrollCommand::Exit);
    assert!(!ctx.scroll.is_active());
}

/// `i` in the pager hands the keyboard to the workload -- the thing
/// tmux copy-mode cannot do: text forwards verbatim, mouse reports stay
/// swallowed (the client borrowed the mouse; the workload never asked
/// for it), and a lone Esc takes the keyboard back with the pager still
/// up, paging again.
#[test]
fn type_through_forwards_text_until_esc_returns_to_paging() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    enter_scroll_mode(&ctx, ScrollCommand::Stay);
    let mut input = ScrollInput::default();
    assert!(
        input.route(&ctx, b"i").is_empty(),
        "the i that opens type-through is consumed, not sent"
    );
    assert!(ctx.scroll.is_typing(), "i must enter type-through");
    assert!(ctx.scroll.is_active(), "typing must not close the pager");
    assert_eq!(
        input.route(&ctx, b"hi there\r"),
        b"hi there\r".to_vec(),
        "while typing, text goes to the workload"
    );
    assert!(
        input.route(&ctx, b"\x1b[<0;3;4M").is_empty(),
        "mouse reports stay swallowed during type-through"
    );
    assert!(
        input.route(&ctx, b"\x1b").is_empty(),
        "the Esc that ends type-through is consumed"
    );
    assert!(!ctx.scroll.is_typing());
    assert!(
        ctx.scroll.is_active(),
        "Esc returns to paging, not to the live screen"
    );
    assert!(
        input.route(&ctx, b"xx").is_empty(),
        "once back in the pager, keys are swallowed again"
    );
    apply_scroll_command(&ctx, ScrollCommand::Exit);
    assert!(!ctx.scroll.is_active());
}

/// The type-through bar keeps the pager's position readout (the offset is
/// still where the user left it) and names the mode; narrow widths
/// degrade without ever exceeding the row.
#[test]
fn typing_bar_names_the_mode_and_keeps_the_position() {
    let view = ScrollView {
        offset: 12,
        available: 240,
    };
    let text = scroll_bar_typing_text(view, 120);
    assert!(text.contains("SCROLL 12/240"), "{text:?}");
    assert!(text.contains("TYPE"), "{text:?}");
    assert!(text.chars().count() <= 120);
    let medium = scroll_bar_typing_text(view, 20);
    assert!(
        medium.contains("TYPE") && medium.chars().count() <= 20,
        "{medium:?}"
    );
    assert_eq!(scroll_bar_typing_text(view, 4), "TYPE");
}

#[test]
fn scroll_keys_binds_i_to_type_through() {
    assert_eq!(
        scroll_keys(b"i"),
        ScrollKey::Command(ScrollCommand::TypeThrough, 1)
    );
}

/// Pane delivery appends the return by default (the tmuxctl behavior) in
/// both framed and raw form, and `--no-enter` drops it in both.
#[test]
fn pane_delivery_appends_enter_by_default_and_no_enter_drops_it() {
    assert_eq!(
        pane_input_bytes("ship it", Some("review"), false, false),
        b"[aplexer message from review] ship it\r"
    );
    assert_eq!(
        pane_input_bytes("ship it", Some("review"), true, false),
        b"ship it\r"
    );
    assert_eq!(
        pane_input_bytes("hold", Some("review"), false, true),
        b"[aplexer message from review] hold"
    );
    assert_eq!(pane_input_bytes("hold", None, true, true), b"hold");
}

/// While the client holds the mouse and the pager is *down*, mouse
/// reports are swallowed (the workload never asked for them) and a wheel
/// roll up opens the pager -- with no `Ctrl-b` first, which is the
/// gesture the user actually reported as broken. Ordinary typing in the
/// same chunk still gets through.
#[test]
fn wheel_up_opens_the_pager_with_no_prefix_and_typing_still_passes() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    *ctx.mouse_owned
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(true);
    let mut input = ScrollInput::default();
    assert_eq!(input.route(&ctx, b"ab"), b"ab".to_vec());
    assert!(!ctx.scroll.is_active());
    // A left click: swallowed, never typed into the workload.
    assert!(input.route(&ctx, b"\x1b[<0;5;5M").is_empty());
    assert!(!ctx.scroll.is_active());
    // The wheel: straight into the pager.
    assert!(input.route(&ctx, b"\x1b[<64;5;5M").is_empty());
    assert!(ctx.scroll.is_active());
}

/// A mouse report split across two reads is reassembled rather than
/// leaking its tail into the workload as text.
#[test]
fn a_split_mouse_report_is_buffered_not_leaked_to_the_workload() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    *ctx.mouse_owned
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(true);
    let mut input = ScrollInput::default();
    assert!(input.route(&ctx, b"\x1b[<64;5").is_empty());
    assert!(!ctx.scroll.is_active());
    assert!(input.route(&ctx, b";5M").is_empty());
    assert!(ctx.scroll.is_active());
}

/// The counterpart guarantee: a bare `ESC` at the end of a chunk is
/// forwarded immediately while the pager is down, so pressing Escape in
/// an editor inside the session does not wait for the next keystroke.
#[test]
fn a_bare_escape_is_never_held_back_from_the_workload() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    *ctx.mouse_owned
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(true);
    let mut input = ScrollInput::default();
    assert_eq!(input.route(&ctx, b"\x1b"), b"\x1b".to_vec());
    assert_eq!(input.route(&ctx, b"\x1b["), b"\x1b[".to_vec());
}

// -- parse_sgr_mouse (docs/clickable-status-bar-design.md section 2) --

#[test]
fn parse_sgr_mouse_left_click_press() {
    let buf = b"\x1b[<0;10;5M";
    match parse_sgr_mouse(buf) {
        MouseParse::Complete(report, consumed) => {
            assert_eq!(
                report,
                MouseReport {
                    button: 0,
                    press: true,
                    col: 10,
                    row: 5,
                }
            );
            assert_eq!(consumed, buf.len());
        }
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[test]
fn parse_sgr_mouse_release() {
    let buf = b"\x1b[<0;10;5m";
    match parse_sgr_mouse(buf) {
        MouseParse::Complete(report, consumed) => {
            assert!(!report.press);
            assert_eq!(consumed, buf.len());
        }
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[test]
fn parse_sgr_mouse_large_coordinates_no_1006_overflow() {
    // The entire point of SGR (?1006h) over legacy (?1000h alone) mode:
    // no 223-column/row ceiling.
    let buf = b"\x1b[<2;9999;500M";
    match parse_sgr_mouse(buf) {
        MouseParse::Complete(report, _) => {
            assert_eq!(report.col, 9999);
            assert_eq!(report.row, 500);
        }
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[test]
fn parse_sgr_mouse_trailing_bytes_only_consumes_the_report() {
    let buf = b"\x1b[<0;10;5Mrest-of-buffer";
    match parse_sgr_mouse(buf) {
        MouseParse::Complete(_, consumed) => assert_eq!(consumed, 10),
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[test]
fn parse_sgr_mouse_incomplete_at_every_prefix_length() {
    let full = b"\x1b[<0;10;5M";
    for split in 1..full.len() {
        let partial = &full[..split];
        assert_eq!(
            parse_sgr_mouse(partial),
            MouseParse::Incomplete,
            "prefix of length {split} should be Incomplete"
        );
    }
}

#[test]
fn parse_sgr_mouse_rejects_ordinary_csi_sequences() {
    // Arrow keys, cursor reports, colors, etc. -- none start with the
    // `ESC [ <` mouse prefix, so these must be an immediate NotMouse,
    // never treated as "keep buffering".
    assert_eq!(parse_sgr_mouse(b"\x1b[A"), MouseParse::NotMouse); // up arrow
    assert_eq!(parse_sgr_mouse(b"\x1b[31m"), MouseParse::NotMouse); // SGR color
    assert_eq!(parse_sgr_mouse(b"hello"), MouseParse::NotMouse);
}

#[test]
fn parse_sgr_mouse_empty_buffer_is_incomplete_not_rejected() {
    // Zero bytes seen yet can't be ruled out as the start of a mouse
    // report -- a caller with nothing buffered should keep reading,
    // not treat an empty read as "definitely not a mouse sequence".
    assert_eq!(parse_sgr_mouse(b""), MouseParse::Incomplete);
}

#[test]
fn parse_sgr_mouse_malformed_after_prefix_is_not_mouse_not_incomplete() {
    // A non-digit, non-';' byte right where a field is expected can
    // never resolve into a valid report -- must not be reported
    // Incomplete (that would make a caller buffer forever).
    assert_eq!(parse_sgr_mouse(b"\x1b[<x;10;5M"), MouseParse::NotMouse);
    assert_eq!(parse_sgr_mouse(b"\x1b[<0;;5M"), MouseParse::NotMouse);
    assert_eq!(parse_sgr_mouse(b"\x1b[<0;10;5X"), MouseParse::NotMouse);
}

#[test]
fn parse_sgr_mouse_split_across_two_reads_reassembles() {
    // Mirrors the Ctrl-b split-read tests above: a caller buffering
    // bytes across scan() calls must see Incomplete on the first half
    // and Complete once the second half is appended.
    let full: &[u8] = b"\x1b[<0;10;5M";
    let split = 5;
    assert_eq!(parse_sgr_mouse(&full[..split]), MouseParse::Incomplete);
    let mut buffered = full[..split].to_vec();
    buffered.extend_from_slice(&full[split..]);
    match parse_sgr_mouse(&buffered) {
        MouseParse::Complete(_, consumed) => assert_eq!(consumed, full.len()),
        other => panic!("expected Complete, got {other:?}"),
    }
}

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

#[test]
fn next_prev_wrap_and_skip_dead() {
    let groups = sample_groups();
    let a1 = groups[0].1[0].id;
    let a2 = groups[0].1[1].id;
    let next =
        pick_switch_target(&groups, Path::new("/ws/a"), a1, SwitchTarget::Next, None).unwrap();
    assert_eq!(next.id, a2); // dead a3 skipped
    let prev =
        pick_switch_target(&groups, Path::new("/ws/a"), a1, SwitchTarget::Prev, None).unwrap();
    assert_eq!(prev.id, a2); // wraps backward past dead a3 too
}

/// `Ctrl-b Down`/`Up` move a *workspace* at a time and enter the one they
/// land on at its most recently accessed session -- the session a
/// returning user means by "that workspace".
#[test]
fn workspace_hop_enters_at_the_most_recently_accessed_session() {
    let ws_a = "/ws/a";
    let ws_b = "/ws/b";
    let mut a1 = mk_record(ws_a, "main", Phase::Running);
    a1.worker_pid = Some(std::process::id());
    let mut b1 = mk_record(ws_b, "first", Phase::Running);
    let mut b2 = mk_record(ws_b, "second", Phase::Running);
    b1.worker_pid = Some(std::process::id());
    b2.worker_pid = Some(std::process::id());
    // `b2` is listed second but was attached to more recently.
    b1.last_accessed_ms = Some(1_000);
    b2.last_accessed_ms = Some(2_000);
    let (b1_id, b2_id) = (b1.id, b2.id);
    let groups = vec![
        (PathBuf::from(ws_a), vec![a1.clone()]),
        (PathBuf::from(ws_b), vec![b1, b2]),
    ];
    for target in [SwitchTarget::NextWorkspace, SwitchTarget::PrevWorkspace] {
        // Two workspaces, so next and previous are the same one; both
        // must enter it at b2, not at list position 1.
        let picked = pick_switch_target(&groups, Path::new(ws_a), a1.id, target, None).unwrap();
        assert_eq!(picked.id, b2_id, "{target:?} entered the wrong session");
    }

    // Never attached: fall back to `a list` order, i.e. the session the
    // status bar numbers 1.
    let mut c1 = mk_record(ws_b, "first", Phase::Running);
    let mut c2 = mk_record(ws_b, "second", Phase::Running);
    c1.worker_pid = Some(std::process::id());
    c2.worker_pid = Some(std::process::id());
    c1.id = b1_id;
    let fresh = vec![
        (PathBuf::from(ws_a), vec![a1.clone()]),
        (PathBuf::from(ws_b), vec![c1, c2]),
    ];
    let picked = pick_switch_target(
        &fresh,
        Path::new(ws_a),
        a1.id,
        SwitchTarget::NextWorkspace,
        None,
    )
    .unwrap();
    assert_eq!(picked.id, b1_id);
}

/// A workspace with nothing attachable left in it is stepped over, not
/// turned into an error the user has to press through; when every other
/// workspace is like that, the error says so and (via `perform_switch`)
/// the attach is left alone.
#[test]
fn workspace_hop_skips_dead_workspaces_and_reports_when_none_remain() {
    let ws_a = "/ws/a";
    let ws_dead = "/ws/dead";
    let ws_c = "/ws/c";
    let mut a1 = mk_record(ws_a, "main", Phase::Running);
    a1.worker_pid = Some(std::process::id());
    let mut corpse = mk_record(ws_dead, "gone", Phase::Exited);
    corpse.worker_pid = None;
    let mut c1 = mk_record(ws_c, "live", Phase::Running);
    c1.worker_pid = Some(std::process::id());
    let c1_id = c1.id;
    let groups = vec![
        (PathBuf::from(ws_a), vec![a1.clone()]),
        (PathBuf::from(ws_dead), vec![corpse.clone()]),
        (PathBuf::from(ws_c), vec![c1]),
    ];
    let picked = pick_switch_target(
        &groups,
        Path::new(ws_a),
        a1.id,
        SwitchTarget::NextWorkspace,
        None,
    )
    .unwrap();
    assert_eq!(picked.id, c1_id, "the dead workspace was not skipped");

    let alone = vec![
        (PathBuf::from(ws_a), vec![a1.clone()]),
        (PathBuf::from(ws_dead), vec![corpse]),
    ];
    let error = pick_switch_target(
        &alone,
        Path::new(ws_a),
        a1.id,
        SwitchTarget::PrevWorkspace,
        None,
    )
    .expect_err("nowhere to go");
    assert!(
        format!("{error:#}").contains("no other workspace"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn index_returns_dead_session_without_skipping() {
    let groups = sample_groups();
    let a1 = groups[0].1[0].id;
    let dead = pick_switch_target(
        &groups,
        Path::new("/ws/a"),
        a1,
        SwitchTarget::Index(3),
        None,
    )
    .unwrap();
    assert_eq!(dead.phase, Phase::Exited);
}

#[test]
fn index_out_of_range_errors() {
    let groups = sample_groups();
    let a1 = groups[0].1[0].id;
    let err = pick_switch_target(
        &groups,
        Path::new("/ws/a"),
        a1,
        SwitchTarget::Index(9),
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("no session 9"));
}

#[test]
fn index_nine_selects_the_ninth_session_and_bounds_are_one_based() {
    let sessions: Vec<SessionRecord> = (1..=9)
        .map(|i| mk_record("/ws/a", &format!("session-{i}"), Phase::Running))
        .collect();
    let current = sessions[0].id;
    let groups = vec![(PathBuf::from("/ws/a"), sessions)];

    let ninth = pick_switch_target(
        &groups,
        Path::new("/ws/a"),
        current,
        SwitchTarget::Index(9),
        None,
    )
    .unwrap();
    assert_eq!(ninth.tag, "session-9");

    let zero = pick_switch_target(
        &groups,
        Path::new("/ws/a"),
        current,
        SwitchTarget::Index(0),
        None,
    )
    .unwrap_err();
    assert!(zero.to_string().contains("no session 0"));

    let tenth = pick_switch_target(
        &groups,
        Path::new("/ws/a"),
        current,
        SwitchTarget::Index(10),
        None,
    )
    .unwrap_err();
    assert!(tenth.to_string().contains("no session 10"));
}

#[test]
fn next_global_crosses_workspace_boundary() {
    let groups = sample_groups();
    let a2 = groups[0].1[1].id; // last live session in ws/a
    let next = pick_switch_target(
        &groups,
        Path::new("/ws/a"),
        a2,
        SwitchTarget::NextGlobal,
        None,
    )
    .unwrap();
    assert_eq!(next.workspace, PathBuf::from("/ws/b"));
}

#[test]
fn last_resolves_by_id() {
    let groups = sample_groups();
    let a1 = groups[0].1[0].id;
    let a2 = groups[0].1[1].id;
    let found = pick_switch_target(
        &groups,
        Path::new("/ws/a"),
        a1,
        SwitchTarget::Last,
        Some(a2),
    )
    .unwrap();
    assert_eq!(found.id, a2);
}

#[test]
fn single_live_session_workspace_errors_on_next() {
    let groups = sample_groups();
    let b1 = groups[1].1[0].id;
    let err =
        pick_switch_target(&groups, Path::new("/ws/b"), b1, SwitchTarget::Next, None).unwrap_err();
    assert!(err.to_string().contains("no other running session"));
}

/// The orphaned-session bug this test guards against: `phase: Running`
/// plus an alive `worker_pid` used to sail straight through
/// `check_attachable` and hit a raw `UnixStream::connect` OS error deep
/// inside `rpc_simple`/`attach` if the socket file was gone. Now it's
/// caught up front with a clear diagnostic.
#[test]
fn check_attachable_reports_missing_socket() {
    let mut r = mk_record("/ws/a", "main", Phase::Running);
    r.socket_path = PathBuf::from("/definitely/does/not/exist/control.sock");
    let err = check_attachable(&r).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("control socket is gone"), "{msg}");
    assert!(msg.contains(&format!("a kill {}", r.id)), "{msg}");
}

/// `check_attachable`'s other two cases (terminal phase, dead
/// worker_pid) must be unaffected by the new socket check -- they bail
/// before ever looking at `socket_path`.
#[test]
fn check_attachable_unchanged_for_terminal_and_dead_worker() {
    let mut exited = mk_record("/ws/a", "main", Phase::Exited);
    exited.socket_path = PathBuf::from("/does/not/matter");
    let err = check_attachable(&exited).unwrap_err().to_string();
    assert!(err.contains("has already exited"), "{err}");

    let mut dead_worker = mk_record("/ws/a", "main", Phase::Running);
    dead_worker.worker_pid = None;
    dead_worker.socket_path = PathBuf::from("/does/not/matter");
    let err = check_attachable(&dead_worker).unwrap_err().to_string();
    assert!(err.contains("worker is not running"), "{err}");
}

/// A registry containing exactly one record, with its paths wired to
/// the throwaway state/runtime roots so `read_session_record`'s identity
/// checks accept it.
fn seeded_registry(record: &mut SessionRecord) -> (Paths, tempfile::TempDir, tempfile::TempDir) {
    let state_dir = tempfile::tempdir().unwrap();
    let runtime_dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: runtime_dir.path().to_path_buf(),
        state_root: state_dir.path().to_path_buf(),
        config_file: state_dir.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    record.socket_path = paths.socket(record.id);
    record.history_path = paths.history(record.id);
    fs::create_dir_all(paths.state_session(record.id)).unwrap();
    fs::create_dir_all(paths.runtime_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), record).unwrap();
    (paths, state_dir, runtime_dir)
}

/// `mk_record` + `seeded_registry`-style write for a second session in
/// an already-seeded paths triple, without `seeded_registry`'s identity
/// sidecar wiring (the nesting guard only does a plain `read_record`).
fn write_inner_record(paths: &Paths, record: &SessionRecord) {
    fs::create_dir_all(paths.state_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), record).unwrap();
}

#[test]
fn nested_attach_allowed_without_a_resolvable_live_inner_session() {
    let mut outer = mk_record("/ws/outer", "outer", Phase::Running);
    let (paths, _state_dir, _runtime_dir) = seeded_registry(&mut outer);
    // No inner id at all: the common attach, one getenv.
    nested_attach_conflict_for(&paths, None).unwrap();
    // A stale or foreign id (another runtime dir, a test harness that
    // inherited the real user's env) resolves to no record here.
    nested_attach_conflict_for(&paths, Some(Uuid::new_v4())).unwrap();
    // A record whose worker is gone (mk_record with the pid stripped)
    // is a corpse, not a session to refuse for.
    let mut corpse = mk_record("/ws/inner", "dead", Phase::Exited);
    corpse.worker_pid = None;
    write_inner_record(&paths, &corpse);
    nested_attach_conflict_for(&paths, Some(corpse.id)).unwrap();
}

#[test]
fn nested_attach_refuses_a_live_inner_session_and_names_the_way_out() {
    let mut outer = mk_record("/ws/outer", "outer", Phase::Running);
    let (paths, _state_dir, _runtime_dir) = seeded_registry(&mut outer);
    // mk_record pins worker_pid to our own pid, so this inner record is
    // worker-alive by the same `a list` liveness check.
    let inner = mk_record("/ws/inner", "peeked", Phase::Running);
    write_inner_record(&paths, &inner);
    let error = nested_attach_conflict_for(&paths, Some(inner.id)).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("/ws/inner"), "message: {message}");
    assert!(message.contains("peeked"), "message: {message}");
    assert!(message.contains("Ctrl-]"), "message: {message}");
    assert!(message.contains("--force"), "message: {message}");
}

/// The default list sweeps before it renders: a corpse `a prune` would
/// take (worker gone, proven-empty containment) is removed from the
/// registry by one bare `a list`, not parked behind the hide filter.
/// `--all` skips the sweep -- it exists to show post-mortems.
#[test]
fn default_list_sweeps_what_prune_would_take() {
    let now = now_ms();
    let mut corpse = mk_record("/ws/sweep", "gone", Phase::Exited);
    corpse.worker_pid = None;
    corpse.workload_pid = None;
    corpse.containment_empty = Some(true);
    corpse.exit = Some(aplexer::ExitInfo {
        code: Some(0),
        signal: None,
        oom_killed: false,
        exited_at_ms: now,
    });
    let (paths, _state_dir, _runtime_dir) = seeded_registry(&mut corpse);

    cmd_list_tty(
        &paths,
        ListArgs {
            running: false,
            all: true,
            sort: None,
        },
    )
    .unwrap();
    assert!(
        paths.state_session(corpse.id).exists(),
        "--all must not sweep; it exists to show post-mortems"
    );

    cmd_list_tty(
        &paths,
        ListArgs {
            running: false,
            all: false,
            sort: None,
        },
    )
    .unwrap();
    assert!(
        !paths.state_session(corpse.id).exists(),
        "the default list left a prunable corpse on disk"
    );
}

/// resolve_quick_index shares session_is_listed with the default list,
/// so the numbers `a 1` understands stay the numbers `a list` prints: a
/// corpse cannot take a row number even when it is the newest session
/// in its workspace, and a workspace whose only session has exited is
/// simply not there (the list drops it whole).
#[test]
fn quick_index_skips_exited_corpses_like_the_default_list() {
    let now = now_ms();
    // Newest-created-first row order (registry.rs), so unfiltered this
    // corpse would be row 1 of the workspace.
    let mut corpse = mk_record("/ws/only", "gone", Phase::Exited);
    corpse.worker_pid = None;
    corpse.created_at_ms = now + 5_000;
    let (paths, _state_dir, _runtime_dir) = seeded_registry(&mut corpse);

    let err =
        resolve_quick_index(&paths, 1, None).expect_err("a corpse-only workspace has no rows");
    assert!(
        format!("{err:#}").contains("no sessions found"),
        "unexpected error: {err:#}"
    );

    // A live session behind the corpse: with the filter, index 1
    // resolves to the live session, never to the newer corpse.
    let mut live = mk_record("/ws/only", "main", Phase::Running);
    live.created_at_ms = now;
    live.socket_path = paths.socket(live.id);
    live.history_path = paths.history(live.id);
    fs::create_dir_all(paths.state_session(live.id)).unwrap();
    fs::create_dir_all(paths.runtime_session(live.id)).unwrap();
    atomic_write_json(&paths.record(live.id), &live).unwrap();

    let picked = resolve_quick_index(&paths, 1, None).unwrap();
    assert_eq!(picked.id, live.id, "the corpse took the live session's row");
}

/// `worker_alive()` deliberately falls back to the bare pid check when
/// the worker identity sidecar cannot be read, so an unreadable sidecar
/// can never let prune delete a live worker's session. Prune must
/// inherit that conservatism instead of re-deriving liveness: while the
/// recorded pid exists and the sidecar is garbage, the record stays --
/// even though every other reap condition (non-terminal phase, dead
/// workload, no containment proof) is satisfied.
#[test]
fn prune_retains_a_record_whose_worker_identity_is_unreadable() {
    let mut record = mk_record("/ws/uncertain", "main", Phase::Running);
    // A real throwaway process standing in for the recorded worker,
    // never this test process's own pid.
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    record.worker_pid = Some(child.id());
    record.workload_pid = None;
    record.containment_empty = Some(false);
    let (paths, _state_dir, _runtime_dir) = seeded_registry(&mut record);
    // Unparseable, so read_worker_identity errors rather than returning
    // None: the "we cannot tell" case, not the "legacy record" case.
    fs::write(
        paths.state_session(record.id).join("worker.identity.json"),
        b"{not json",
    )
    .unwrap();

    let outcome = prune_dead_sessions(&paths).unwrap();
    assert!(
        outcome.removed.is_empty(),
        "prune reaped a session whose worker liveness was unknown"
    );
    assert_eq!(outcome.retained_count, 1);
    assert!(
        paths.state_session(record.id).exists(),
        "durable state was removed under an unreadable identity"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "prune must never signal anything"
    );

    // Same unreadable sidecar, worker genuinely gone: now it is reapable.
    // Proves the retention above came from the liveness fallback and not
    // from some blanket refusal to touch this record.
    child.kill().unwrap();
    child.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(record.worker_pid.unwrap()) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let outcome = prune_dead_sessions(&paths).unwrap();
    assert_eq!(outcome.removed, vec![record.id]);
    assert_eq!(outcome.removed_without_containment_proof, vec![record.id]);
    assert!(!paths.state_session(record.id).exists());
}

/// Prune no longer requires a terminal phase, so it can now see a
/// `Starting` record with no worker pid -- which is exactly what
/// `start_session` writes before its worker registers itself. That
/// worker holds the session's worker lock, so prune must fence on it
/// the same way `a forget` does rather than deleting a session that is
/// coming up.
#[test]
fn prune_fences_a_pre_pid_starting_record_against_its_worker_lock() {
    let mut record = mk_record("/ws/starting", "main", Phase::Starting);
    record.worker_pid = None;
    record.workload_pid = None;
    record.containment_empty = Some(false);
    let (paths, _state_dir, _runtime_dir) = seeded_registry(&mut record);

    let held = FileLock::exclusive(&paths.worker_lock(record.id), true).unwrap();
    let outcome = prune_dead_sessions(&paths).unwrap();
    assert!(
        outcome.removed.is_empty(),
        "prune removed a session whose worker still holds its lock"
    );
    assert_eq!(outcome.retained_count, 1);
    assert!(paths.state_session(record.id).exists());

    drop(held);
    let outcome = prune_dead_sessions(&paths).unwrap();
    assert_eq!(
        outcome.removed,
        vec![record.id],
        "an unfenced pre-PID stub must still be reapable"
    );
    assert!(!paths.state_session(record.id).exists());
}

/// `a kill` against a record whose worker_pid is alive but whose control
/// socket is gone may stop that verified worker, but without a cgroup it
/// cannot prove that a setsid descendant did not escape. It must preserve
/// the worker and record, keeping the only remaining subreaper boundary.
/// Uses a real throwaway child process as the stand-in worker_pid
/// (never the test process's own pid, which `mk_record` defaults to --
/// this test also proves the containment preflight happens before any
/// signal is sent.
#[test]
fn cmd_kill_preserves_stale_socket_record_without_containment_proof() {
    let state_dir = tempfile::tempdir().unwrap();
    let runtime_dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: runtime_dir.path().to_path_buf(),
        state_root: state_dir.path().to_path_buf(),
        config_file: state_dir.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let mut record = mk_record("/ws/stale", "main", Phase::Running);
    // A real, throwaway process standing in for the orphaned worker --
    // long-lived enough that if `a kill` did nothing, it would still be
    // alive when we check.
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    record.worker_pid = Some(child.id());
    record.history_path = paths.history(record.id);
    // Preserve runtime evidence while leaving the control socket absent.
    record.socket_path = paths.socket(record.id);
    fs::create_dir_all(paths.state_session(record.id)).unwrap();
    fs::create_dir_all(paths.runtime_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), &record).unwrap();
    let start_time = process_start_time_ticks(child.id()).unwrap();
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    let boot_id = boot_id.trim();
    fs::write(
        paths.state_session(record.id).join("worker.identity.json"),
        format!(
            "{{\"pid\":{},\"start_time_ticks\":{start_time},\"boot_id\":\"{boot_id}\"}}\n",
            child.id(),
        ),
    )
    .unwrap();

    let args = KillArgs {
        target: TargetArgs {
            selector: Some(record.id.to_string()),
            workspace: None,
            tag: None,
        },
        signal: "TERM".to_string(),
        grace_ms: 50,
    };
    let error = cmd_kill(&paths, args, false)
        .expect_err("missing containment proof must prevent cleanup success");
    assert!(
        format!("{error:#}").contains("no authoritative containment locator"),
        "{error:#}"
    );
    assert!(
        paths.state_session(record.id).exists(),
        "ambiguous stale record must be preserved"
    );
    assert!(
        paths.runtime_session(record.id).exists(),
        "ambiguous runtime evidence must be preserved"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "unreachable worker was destroyed before recovery was proven"
    );
    assert!(
        process_alive(record.worker_pid.unwrap()),
        "unreachable worker must remain the subreaper boundary"
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn force_kill_stale_worker_refuses_legacy_record_without_identity() {
    let state_dir = tempfile::tempdir().unwrap();
    let runtime_dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: runtime_dir.path().to_path_buf(),
        state_root: state_dir.path().to_path_buf(),
        config_file: state_dir.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let mut record = mk_record("/ws/legacy", "main", Phase::Running);
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    record.worker_pid = Some(child.id());
    record.history_path = paths.history(record.id);
    fs::create_dir_all(paths.state_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), &record).unwrap();

    let error = force_kill_stale_worker(&record).unwrap_err();
    assert!(
        format!("{error:#}").contains("no trustworthy recorded worker identity"),
        "{error:#}"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "legacy pid was signalled"
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn force_kill_stale_worker_refuses_start_time_mismatch() {
    let state_dir = tempfile::tempdir().unwrap();
    let runtime_dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: runtime_dir.path().to_path_buf(),
        state_root: state_dir.path().to_path_buf(),
        config_file: state_dir.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let mut record = mk_record("/ws/reused", "main", Phase::Running);
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = child.id();
    record.worker_pid = Some(pid);
    record.history_path = paths.history(record.id);
    fs::create_dir_all(paths.state_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), &record).unwrap();
    let wrong_start = process_start_time_ticks(pid).unwrap().saturating_add(1);
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap();
    let boot_id = boot_id.trim();
    fs::write(
        paths.state_session(record.id).join("worker.identity.json"),
        format!("{{\"pid\":{pid},\"start_time_ticks\":{wrong_start},\"boot_id\":\"{boot_id}\"}}\n"),
    )
    .unwrap();

    let error = force_kill_stale_worker(&record).unwrap_err();
    assert!(
        format!("{error:#}").contains("has been reused"),
        "{error:#}"
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "mismatched pid identity was signalled"
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

/// The safety property this whole feature must preserve: a session
/// whose worker is genuinely alive AND reachable (real socket on disk)
/// must still be refused, exactly as before -- only the narrower
/// socket-missing case gets the new force-kill behavior.
#[test]
fn cmd_kill_still_refuses_live_reachable_worker() {
    let state_dir = tempfile::tempdir().unwrap();
    let runtime_dir = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: runtime_dir.path().to_path_buf(),
        state_root: state_dir.path().to_path_buf(),
        config_file: state_dir.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let record = mk_record("/ws/live", "main", Phase::Running);
    // worker_pid defaults (via mk_record) to this test process's own
    // pid, i.e. genuinely alive. socket_path defaults to this test
    // binary's own executable path, i.e. genuinely exists on disk --
    // so `socket_missing` must be false and the RPC-failure path below
    // must hit the unchanged `return Err(error)` refusal, never the
    // force-kill/remove branch.
    fs::create_dir_all(paths.state_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), &record).unwrap();

    let args = KillArgs {
        target: TargetArgs {
            selector: Some(record.id.to_string()),
            workspace: None,
            tag: None,
        },
        signal: "TERM".to_string(),
        grace_ms: 50,
    };
    let err = cmd_kill(&paths, args, false)
        .expect_err("a live, reachable worker must not be force-killed by `a kill`");
    // The error is the raw RPC/connect failure (there's no real worker
    // listening on that socket path), not a "removed" success -- and
    // the record and the still-alive pid must both be untouched.
    drop(err);
    assert!(
        paths.state_session(record.id).exists(),
        "a live/reachable session's record must not be removed"
    );
    assert!(
        process_alive(record.worker_pid.unwrap()),
        "a live/reachable session's worker must not be signalled"
    );
}

// -- draw_status_bar's "did it actually write" contract, which the
// status thread's STATUS_BAR_MAX_INTERVAL overdue-timer depends on to
// avoid the timer-starvation bug: resetting `last_draw` on every tick
// regardless of whether draw_status_bar performed a real write would
// let a frequent-but-unchanging redraw (a spinner, streamed tokens with
// pauses) keep the overdue timer perpetually "recently fired" without
// ever actually re-writing a margin/row a full-screen erase clobbered.
// See draw_status_bar's and the status thread's doc comments.

/// `draw_status_bar` writes straight to the real `io::Stdout` (no
/// injectable writer to swap in for a test), so exercising its
/// real-write path here would otherwise leak raw DECSTBM/reverse-video
/// escape sequences into whatever terminal happens to be running
/// `cargo test` interactively -- and leave that terminal's scroll
/// region permanently narrowed, since nothing in this test ever runs
/// the reset-on-detach path that would restore it. Redirecting the
/// process's real fd 1 to `/dev/null` for the guard's lifetime (and
/// restoring the original fd on drop) makes the write land somewhere
/// harmless instead.
struct StdoutToDevNull {
    saved_fd: i32,
}
impl StdoutToDevNull {
    fn new() -> Self {
        let saved_fd = unsafe { libc::dup(1) };
        assert!(saved_fd >= 0, "dup(1) failed");
        let devnull = std::ffi::CString::new("/dev/null").unwrap();
        let devnull_fd = unsafe { libc::open(devnull.as_ptr(), libc::O_WRONLY) };
        assert!(devnull_fd >= 0, "open /dev/null failed");
        let rc = unsafe { libc::dup2(devnull_fd, 1) };
        unsafe { libc::close(devnull_fd) };
        assert!(rc >= 0, "dup2 to /dev/null failed");
        Self { saved_fd }
    }
}
impl Drop for StdoutToDevNull {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.saved_fd, 1);
            libc::close(self.saved_fd);
        }
    }
}

/// Feeds bytes into a test context's model the way the client's relay
/// path does, without a terminal to write them to.
fn feed_test_screen(screen: &Arc<Mutex<aplexer::screen::ClientScreen>>, data: &[u8]) {
    screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .feed(data);
}

fn status_ctx_for_test(reserved: bool) -> StatusBarCtx {
    StatusBarCtx {
        stdout: Arc::new(Mutex::new(io::stdout())),
        term: Arc::new(Mutex::new(TermGeom {
            rows: 24,
            cols: 80,
            reserved,
        })),
        paths: Paths {
            runtime_root: PathBuf::from("/nonexistent-aplexer-test-runtime"),
            state_root: PathBuf::from("/nonexistent-aplexer-test-state"),
            config_file: PathBuf::from("/nonexistent-aplexer-test-state/config.toml"),
        },
        record: Arc::new(Mutex::new(mk_record(
            "/ws/status-bar-test",
            "t",
            Phase::Running,
        ))),
        flash: Arc::new(Mutex::new(None)),
        last_drawn: Arc::new(Mutex::new(None)),
        screen: Arc::new(Mutex::new(
            aplexer::screen::ClientScreen::try_new(23, 80).unwrap(),
        )),
        pending: Arc::new(AtomicBool::new(false)),
        pending_refresh: Arc::new(AtomicBool::new(false)),
        pending_layout: Arc::new(Mutex::new(None)),
        sync_deferred_since: Arc::new(Mutex::new(None)),
        scroll: Arc::new(ScrollMode::new()),
        overlay: Arc::new(KeyOverlay::default()),
        mouse_owned: Arc::new(Mutex::new(None)),
        mouse_capture: false,
    }
}

/// Serializes every test that redirects the process-wide fd 1. Without
/// it the default multi-threaded test harness lets one such test's
/// `dup(1)` capture another's pipe write end and hold it open, so the
/// reader blocks forever waiting for an EOF that never comes.
static FD1_GUARD: Mutex<()> = Mutex::new(());

/// Like `StdoutToDevNull`, but keeps the bytes: redirects fd 1 to a pipe
/// so a test can assert on the exact escape sequences `draw_status_bar`
/// emitted, rather than only on its `bool` return.
struct StdoutToPipe {
    saved_fd: i32,
    read_fd: i32,
}
impl StdoutToPipe {
    fn new() -> Self {
        let saved_fd = unsafe { libc::dup(1) };
        assert!(saved_fd >= 0, "dup stdout failed");
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
        // Non-blocking read end: everything of interest is flushed by
        // `write_locked` before we read, so "no more data" must surface
        // as EAGAIN rather than an indefinite block.
        assert_eq!(
            unsafe { libc::fcntl(fds[0], libc::F_SETFL, libc::O_NONBLOCK) },
            0,
            "set O_NONBLOCK failed"
        );
        assert!(unsafe { libc::dup2(fds[1], 1) } >= 0, "dup2 to pipe failed");
        unsafe { libc::close(fds[1]) };
        Self {
            saved_fd,
            read_fd: fds[0],
        }
    }
    /// Restores stdout and returns everything written while redirected.
    fn take(self) -> Vec<u8> {
        // Restore first so the write end is fully closed before reading,
        // otherwise the read below blocks on a still-open pipe.
        unsafe {
            libc::dup2(self.saved_fd, 1);
            libc::close(self.saved_fd);
        }
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::read(
                    self.read_fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        unsafe { libc::close(self.read_fd) };
        out
    }
}

/// Regression test for the DECSTBM clobber described on
/// `StatusBarCtx::screen`: the bar's defensive scroll-region
/// re-assert used to write `\x1b[1;{rows-1}r` unconditionally, which
/// destroyed a workload's own sub-range -- including the one the attach
/// snapshot had just restored (docs/terminal-state-design.md section 6.2
/// step 3) -- and left the host terminal scrolling the wrong rows.
#[test]
fn draw_status_bar_reasserts_the_workload_scroll_region_not_its_own() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = status_ctx_for_test(true);
    // No workload region: the bar reserves the bottom row for itself, as
    // it always has.
    let pipe = StdoutToPipe::new();
    draw_status_bar(&ctx, true);
    let default_margins = String::from_utf8_lossy(&pipe.take()).into_owned();
    assert!(
            default_margins.contains("\x1b[1;23r"),
            "with a full-screen workload the bar must reserve row 24 for itself, got {default_margins:?}"
        );

    // Workload sets a DECSTBM sub-range (as an attach snapshot's trailing
    // bytes do, and as a margin-using TUI does live).
    feed_test_screen(&ctx.screen, b"\x1b[5;15r");
    let pipe = StdoutToPipe::new();
    draw_status_bar(&ctx, true);
    let sub_range = String::from_utf8_lossy(&pipe.take()).into_owned();
    assert!(
        sub_range.contains("\x1b[5;15r"),
        "the workload's own scroll region must be the one re-asserted, got {sub_range:?}"
    );
    assert!(
        !sub_range.contains("\x1b[1;23r"),
        "the bar must not clobber the workload's sub-range, got {sub_range:?}"
    );

    // Workload releases its region (`\x1b[r`): the bar's own reservation
    // must come straight back, or the reserved row stops being protected.
    feed_test_screen(&ctx.screen, b"\x1b[r");
    let pipe = StdoutToPipe::new();
    draw_status_bar(&ctx, true);
    let released = String::from_utf8_lossy(&pipe.take()).into_owned();
    assert!(
        released.contains("\x1b[1;23r"),
        "releasing the workload region must restore the bar's reservation, got {released:?}"
    );
}

/// A workload margin change with otherwise-identical bar text must not be
/// swallowed by the dirty-check -- that would leave the wrong scroll
/// region in force on the host until some unrelated text change happened.
#[test]
fn draw_status_bar_dirty_check_notices_a_workload_margin_change() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let _guard = StdoutToDevNull::new();
    let ctx = status_ctx_for_test(true);
    assert!(
        draw_status_bar(&ctx, false),
        "first draw must be a real write"
    );
    assert!(
        !draw_status_bar(&ctx, false),
        "unchanged state must be a skip"
    );
    feed_test_screen(&ctx.screen, b"\x1b[5;15r");
    assert!(
        draw_status_bar(&ctx, false),
        "a workload margin change must defeat the dirty-check even when the text is unchanged"
    );
}

// ---------------------------------------------------------------------
// Issue #5: the status bar must not be able to corrupt a workload frame.
//
// Everything below asserts on the *rendered screen* of a real
// `vt100::Parser` standing in for the user's terminal, compared against a
// second parser standing in for what the workload believes it drew. A
// byte-level assertion would pass while the screen was still wrong, which
// is exactly the trap this class of bug sets.
// ---------------------------------------------------------------------

/// The user's real terminal (`host`, full physical geometry) beside the
/// workload's own screen (`workload`, one row shorter -- its PTY is
/// resized to leave the status row free). The client is a byte relay
/// between them, so for every row the workload can reach the two must
/// render identically, character for character, and agree on the cursor.
struct Harness {
    ctx: StatusBarCtx,
    host: vt100::Parser,
    workload: vt100::Parser,
    rows: u16,
    cols: u16,
    /// Every status redraw that actually reached the terminal, with the
    /// stream state it was written at.
    redraws: Vec<Redraw>,
}

struct Redraw {
    at_escape_boundary: bool,
    in_synchronized_update: bool,
}

impl Harness {
    fn new() -> Self {
        let (rows, cols) = (24u16, 80u16);
        let ctx = status_ctx_for_test(true);
        let mut host = vt100::Parser::new(rows, cols, 0);
        // What `apply_terminal_layout` puts on the wire at attach time.
        host.process(format!("\x1b[1;{}r", rows - 1).as_bytes());
        Self {
            ctx,
            host,
            workload: vt100::Parser::new(rows - 1, cols, 0),
            rows,
            cols,
            redraws: Vec::new(),
        }
    }

    /// One PTY chunk: the workload's own screen sees it, and so does the
    /// client's model on its way to the host terminal.
    fn workload_emits(&mut self, data: &[u8]) {
        self.workload.process(data);
        let rewritten = {
            let mut screen = self
                .ctx
                .screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            screen.relay(data)
        };
        self.host.process(rewritten.as_deref().unwrap_or(data));
        // What the main frame loop does after every Data frame.
        if self.ctx.pending.load(Ordering::Relaxed) {
            self.status_redraw();
        }
    }

    /// What the status thread's timer does. Returns whether the redraw
    /// actually reached the terminal (false = deferred to `ctx.pending`).
    fn status_redraw(&mut self) -> bool {
        let (at_escape_boundary, in_synchronized_update) = {
            let screen = self
                .ctx
                .screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (screen.at_escape_boundary(), screen.in_synchronized_update())
        };
        match status_bar_redraw(&self.ctx, true) {
            Some(bytes) => {
                assert!(!bytes.is_empty(), "a reported write must emit bytes");
                self.host.process(&bytes);
                self.redraws.push(Redraw {
                    at_escape_boundary,
                    in_synchronized_update,
                });
                true
            }
            None => false,
        }
    }

    /// Simulates `STATUS_BAR_SYNC_DEFER_LIMIT` having elapsed, so the
    /// synchronized-output deferral stops holding the redraw back and the
    /// escape-boundary gate is the only thing standing between the
    /// injection and the workload's half-emitted sequence. That is the
    /// production worst case (a frame longer than the limit, or a block
    /// the workload never closes) and the case issue #5 was reported
    /// from, so the tests drive it directly rather than hiding behind the
    /// softer gate.
    fn expire_sync_deferral(&self) {
        let mut since = self
            .ctx
            .sync_deferred_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *since = Instant::now().checked_sub(STATUS_BAR_SYNC_DEFER_LIMIT * 2);
    }

    fn row(parser: &vt100::Parser, row: u16, cols: u16) -> String {
        parser.screen().contents_between(row, 0, row, cols)
    }

    /// The load-bearing assertion: every row the workload can reach must
    /// render on the host exactly as the workload drew it, and the cursor
    /// must agree.
    fn assert_screens_agree(&self, label: &str) {
        for row in 0..self.rows - 1 {
            assert_eq!(
                Self::row(&self.host, row, self.cols),
                Self::row(&self.workload, row, self.cols),
                "{label}: host row {} diverged from the workload's screen",
                row + 1
            );
        }
        assert_eq!(
            self.host.screen().cursor_position(),
            self.workload.screen().cursor_position(),
            "{label}: host and workload disagree on the cursor position"
        );
    }

    fn assert_bar_drawn(&self, label: &str) {
        let bar = Self::row(&self.host, self.rows - 1, self.cols);
        assert!(
            bar.contains("status-bar-test"),
            "{label}: the reserved row must still carry the status bar, got {bar:?}"
        );
    }
}

/// One opencode/opentui-shaped frame: a synchronized-output block wrapping
/// a per-cell diff repaint, every run absolutely positioned with its own
/// SGR. Captured verbatim from a real `opencode` session for issue #5:
/// `\x1b[?2026h\x1b[?25l\x1b[15;64H\x1b[38;5;237m\x1b[48;5;234m\xc2\xb7...`
fn opencode_shaped_frame(generation: usize) -> Vec<u8> {
    let words = [
        "Replace",
        "with",
        "ValueError",
        "capture-warnings.html",
        "docs.pytest.org",
        "session",
        "passed",
    ];
    let mut frame = b"\x1b[?2026h\x1b[?25l".to_vec();
    for row in 1..=23usize {
        let mut col = 1usize;
        let mut k = 0usize;
        while col < 66 {
            let word = words[(generation + row + k) % words.len()];
            frame.extend_from_slice(
                format!(
                    "\x1b[{row};{col}H\x1b[38;5;{}m\x1b[48;5;234m{word}\x1b[0m",
                    16 + ((generation + row + k) % 200)
                )
                .as_bytes(),
            );
            col += word.len() + 1;
            k += 1;
        }
    }
    frame.extend_from_slice(b"\x1b[13;17H\x1b[?25h\x1b[?2026l");
    frame
}

/// **The issue #5 reproduction.** A status redraw requested while the
/// relayed stream sits inside a half-emitted CSI sequence -- which is
/// where a PTY read boundary lands about half the time under a
/// continuously-streaming TUI (measured: 5 of 10 redraws on a real
/// `a attach`) -- used to be written there anyway. The host terminal
/// abandons the workload's partial sequence when our `ESC` arrives and
/// prints its remaining parameter bytes as literal text into the frame:
/// `\x1b[38;5;` + our redraw + `91m...` renders a stray `91m` welded into
/// the row and shifts everything after it along, which is exactly the
/// reported `Rep69ce with Val` / `Doos` / `hetps` corruption.
///
/// The split point here is not hand-picked to be convenient: the test
/// walks *every* byte offset inside the frame, so it covers splits inside
/// CSI parameters, inside intermediate bytes, between an `ESC` and its
/// `[`, and inside the multi-byte characters the frame draws with.
#[test]
fn status_redraw_never_splices_into_a_workload_frame() {
    let frame = opencode_shaped_frame(0);
    // Every offset would be ~3500 harnesses; step through it densely
    // enough to hit every sequence position class many times over while
    // keeping the test fast.
    let mut deferred = 0usize;
    let mut written = 0usize;
    for split in (1..frame.len()).step_by(7) {
        let mut h = Harness::new();
        h.workload_emits(&frame[..split]);
        // The whole frame is inside a `?2026` block, so without this the
        // softer synchronized-output gate would defer every redraw and the
        // escape-boundary gate -- the one that actually prevents the
        // corruption -- would never be exercised.
        h.expire_sync_deferral();
        if h.status_redraw() {
            written += 1;
        } else {
            deferred += 1;
        }
        h.workload_emits(&frame[split..]);
        h.assert_screens_agree(&format!("split at byte {split}"));
        h.assert_bar_drawn(&format!("split at byte {split}"));
        for r in &h.redraws {
            assert!(
                r.at_escape_boundary,
                "split at byte {split}: a redraw was written mid-escape-sequence"
            );
        }
    }
    assert!(
        deferred > 0,
        "the frame must contain unsafe split points for this test to mean anything"
    );
    assert!(
        written > 0,
        "and safe ones, so the bar is not simply never drawn"
    );
}

/// One Claude-Code-shaped frame: **no `?2026` anywhere**. Ink-based TUIs
/// (Claude Code, and codex before it adopted synchronized output) repaint
/// with a full-screen erase followed by absolutely-positioned SGR runs,
/// and Claude Code opens with the DEC save/restore-cursor idiom
/// (`\x1b7\x1b[r\x1b8`) that the status bar used to clobber. Measured from
/// a real 24x100 capture: 1 `ESC 7`, 1 `ESC 8`, 0 `CSI ?2026h`.
fn claude_code_shaped_frame(generation: usize) -> Vec<u8> {
    let words = [
        "Removed",
        "InDjango70Warning",
        "category",
        "capture-warnings.html",
        "docs.pytest.org",
        "8 passed, 3 warnings",
    ];
    // The exact opening idiom, plus a save the workload restores later.
    let mut frame = b"\x1b7\x1b[r\x1b8\x1b[2J\x1b[H".to_vec();
    for row in 1..=23usize {
        frame.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
        let mut k = 0usize;
        let mut col = 1usize;
        while col < 70 {
            let word = words[(generation + row + k) % words.len()];
            frame.extend_from_slice(
                format!(
                    "\x1b[38;5;{}m\x1b[1m{word}\x1b[0m ",
                    16 + ((generation + row + k) % 200)
                )
                .as_bytes(),
            );
            col += word.len() + 1;
            k += 1;
        }
    }
    frame.extend_from_slice(b"\x1b[9;5H\x1b7\x1b[23;1H\x1b[2Kfooter\x1b8ANCHORED");
    frame
}

/// `flash_status` is a *new* forced-redraw caller (the terminal-first CLI
/// merge routed the attach hint, `Ctrl-b ?` help and switch failures
/// through it, replacing an `eprintln!` banner and two direct
/// `draw_status_bar` calls). It must not be a hole in the boundary gate.
///
/// It is not, by construction rather than by discipline: the gate lives
/// inside `draw_status_bar`, which is the single funnel every bar write
/// goes through, so a caller cannot opt out of it -- `flash_status` passes
/// `force: true` and `force` deliberately does not bypass the boundary
/// check. This pins that: a flash raised while the workload is
/// mid-escape-sequence must be deferred rather than spliced, and must
/// still reach the terminal at the next boundary with its message intact.
///
/// This matters more since the maintainer's "let's not hide status"
/// decision (aplexer#12 closed won't-do): there is no suppression flag, so
/// the injected path has to be correct on its own for every caller.
#[test]
fn flash_status_cannot_bypass_the_boundary_gate() {
    let frame = opencode_shaped_frame(3);
    // A split inside a CSI parameter list -- the shape a real capture put
    // 11 of 54 status writes into.
    let needle = b"\x1b[38;5;";
    let split = frame
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap()
        + needle.len();
    let mut h = Harness::new();
    h.workload_emits(&frame[..split]);
    h.expire_sync_deferral();
    assert!(
        !h.ctx
            .screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at_escape_boundary(),
        "the harness must actually be mid-sequence for this test to mean anything"
    );

    flash_status(&h.ctx, "FLASHED-MESSAGE");
    assert!(
        h.ctx.pending.load(Ordering::Relaxed),
        "a flash raised mid-sequence must be deferred, not written"
    );
    assert!(
        !Harness::row(&h.host, h.rows - 1, h.cols).contains("FLASHED-MESSAGE"),
        "nothing may reach the terminal while the stream is mid-sequence"
    );

    // The rest of the frame arrives; the frame loop flushes the deferral.
    h.workload_emits(&frame[split..]);
    h.assert_screens_agree("flash deferred across a mid-sequence split");
    assert!(
        Harness::row(&h.host, h.rows - 1, h.cols).contains("FLASHED-MESSAGE"),
        "the deferred flash must still be delivered, got {:?}",
        Harness::row(&h.host, h.rows - 1, h.cols)
    );
    for r in &h.redraws {
        assert!(
            r.at_escape_boundary,
            "a flash was written mid-escape-sequence"
        );
    }
}

/// **The fix must not depend on synchronized-output mode.** opencode and
/// codex bracket every frame in `CSI ?2026 h/l`, but Claude Code -- the
/// most-used agent here -- emits none at all, so if the boundary detection
/// leaned on `?2026` it would be a strictly weaker code path for exactly
/// the workload that matters most.
///
/// It does not. `?2026` is a *soft* preference layered on top: it keeps a
/// redraw out of a frame the workload declared, and is bounded by
/// `STATUS_BAR_SYNC_DEFER_LIMIT` precisely so nothing can depend on it.
/// The protection is `ClientScreen::at_escape_boundary()`, which is
/// derived from the stream's own parser state and knows nothing about
/// `?2026`.
///
/// This test is the same all-offsets split walk as
/// `status_redraw_never_splices_into_a_workload_frame`, over a frame that
/// provably contains no synchronized-output markers, with the additional
/// assertion that the synchronized-update gate never once fired -- so a
/// green result here can only come from the escape-boundary gate.
#[test]
fn escape_boundary_gate_protects_a_workload_that_never_uses_synchronized_output() {
    let frame = claude_code_shaped_frame(0);
    assert!(
        !frame
            .windows(8)
            .any(|w| w == b"\x1b[?2026h" || w == b"\x1b[?2026l"),
        "this test is only meaningful on a frame with no ?2026 markers"
    );
    let mut deferred = 0usize;
    let mut written = 0usize;
    let mut redraws = 0usize;
    for split in (1..frame.len()).step_by(7) {
        let mut h = Harness::new();
        h.workload_emits(&frame[..split]);
        if h.status_redraw() {
            written += 1;
        } else {
            deferred += 1;
        }
        h.workload_emits(&frame[split..]);
        h.assert_screens_agree(&format!("no-sync split at byte {split}"));
        h.assert_bar_drawn(&format!("no-sync split at byte {split}"));
        // The workload's own `\x1b7`/`\x1b8` pair must have survived every
        // redraw: `ANCHORED` belongs at row 9 col 5, not wherever the bar
        // last left the cursor.
        assert!(
                Harness::row(&h.host, 8, h.cols).contains("ANCHORED"),
                "no-sync split at byte {split}: the workload's DECRC must land where it saved, row 9 was {:?}",
                Harness::row(&h.host, 8, h.cols)
            );
        for r in &h.redraws {
            assert!(
                r.at_escape_boundary,
                "no-sync split at byte {split}: a redraw was written mid-escape-sequence"
            );
            assert!(
                !r.in_synchronized_update,
                "no-sync split at byte {split}: this workload has no synchronized-output \
                     blocks, so the sync gate must never be what protected it"
            );
            redraws += 1;
        }
    }
    assert!(
        deferred > 0,
        "the frame must contain unsafe split points for this test to mean anything"
    );
    assert!(
        written > 0 && redraws > 0,
        "and the bar must still be drawn"
    );
}

/// A workload that uses the DEC save/restore-cursor register itself --
/// Claude Code opens with exactly `\x1b7\x1b[r\x1b8`, opencode uses the
/// same register via `CSI s`/`CSI u`, and every `tput sc`-style progress
/// line inside a session does too. A terminal has one such register, so
/// the status bar's old `\x1b7 ... \x1b8` bracket overwrote the workload's
/// saved position and its own later restore jumped to *ours*.
#[test]
fn workload_saved_cursor_survives_a_status_redraw() {
    let mut h = Harness::new();
    h.workload_emits(b"\x1b[2J\x1b[5;1HHEADER");
    h.workload_emits(b"\x1b7"); // workload saves its cursor at row 5
    h.workload_emits(b"\x1b[20;1Hfooter drawn elsewhere");
    assert!(h.status_redraw(), "a ground-state redraw must be written");
    h.workload_emits(b"\x1b8TAIL"); // workload restores -- must be row 5
    h.assert_screens_agree("workload DECSC/DECRC");
    assert!(
        Harness::row(&h.host, 4, h.cols).contains("HEADERTAIL"),
        "the workload's own restore must land where it saved, got {:?}",
        Harness::row(&h.host, 4, h.cols)
    );
    h.assert_bar_drawn("workload DECSC/DECRC");
}

/// A workload that never goes idle -- an agent CLI mid-generation, the
/// workload aplexer exists for -- never opens `STATUS_BAR_IDLE_GAP`, so
/// every redraw it ever gets is the `STATUS_BAR_MAX_INTERVAL` forced one.
/// This drives that worst case directly: a redraw requested after *every*
/// chunk of a continuous multi-frame stream chopped at pseudo-random
/// offsets. The bar must stay fresh, and no redraw may be written at an
/// unsafe point or inside a declared frame.
#[test]
fn continuously_streaming_workload_redraws_only_at_frame_boundaries() {
    let mut stream = Vec::new();
    for generation in 0..6 {
        stream.extend_from_slice(&opencode_shaped_frame(generation));
    }
    // Phase A: the synchronized-output deferral in force. Every redraw
    // that reaches the terminal must be both at an escape boundary and
    // outside a declared frame.
    let mut h = Harness::new();
    // A deterministic LCG stands in for PTY read boundaries, which fall at
    // arbitrary byte offsets rather than on sequence boundaries.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut at = 0usize;
    while at < stream.len() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = (((seed >> 33) % 900) + 100) as usize;
        let end = (at + len).min(stream.len());
        h.workload_emits(&stream[at..end]);
        h.status_redraw();
        at = end;
    }
    h.assert_screens_agree("continuous stream, sync respected");
    h.assert_bar_drawn("continuous stream, sync respected");
    assert!(
        !h.redraws.is_empty(),
        "a never-idle workload must still get its bar refreshed"
    );
    for (i, r) in h.redraws.iter().enumerate() {
        assert!(
            r.at_escape_boundary,
            "redraw {i} was written mid-escape-sequence"
        );
        assert!(
            !r.in_synchronized_update,
            "redraw {i} was written inside a synchronized-output frame"
        );
    }

    // Phase B: the deferral bounded out (a frame longer than
    // `STATUS_BAR_SYNC_DEFER_LIMIT`, or a block the workload never
    // closes). Redraws are now allowed inside a frame, so the
    // escape-boundary gate is the only protection left -- and it has to be
    // enough, which is the whole claim of this fix.
    let mut h = Harness::new();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut at = 0usize;
    while at < stream.len() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = (((seed >> 33) % 900) + 100) as usize;
        let end = (at + len).min(stream.len());
        h.workload_emits(&stream[at..end]);
        h.expire_sync_deferral();
        h.status_redraw();
        at = end;
    }
    h.assert_screens_agree("continuous stream, deferral bounded out");
    h.assert_bar_drawn("continuous stream, deferral bounded out");
    assert!(
        h.redraws.len() >= 10,
        "with the deferral bounded out the bar must refresh often; got {} redraws",
        h.redraws.len()
    );
    assert!(
        h.redraws.iter().any(|r| r.in_synchronized_update),
        "this phase must actually exercise redraws inside a frame"
    );
    for (i, r) in h.redraws.iter().enumerate() {
        assert!(
            r.at_escape_boundary,
            "redraw {i} was written mid-escape-sequence"
        );
    }
}

/// docs/terminal-state-design.md section 7.1's reserved-row walk, which
/// this replaces the old characterization test for. While the client
/// re-asserts a workload's own DECSTBM sub-range, the host's bottom row is
/// the screen bottom rather than a margin boundary, so a line feed on the
/// workload's last row walked the host cursor onto the reserved row and
/// left it there -- the workload's screen model and the host permanently
/// one row apart. The client's model now detects that at the byte that
/// causes it and splices in an absolute reposition.
#[test]
fn workload_line_feed_no_longer_reaches_the_reserved_row_under_a_sub_range() {
    let mut h = Harness::new();
    h.workload_emits(b"\x1b[5;15r");
    assert!(
        h.status_redraw(),
        "the bar re-asserts the workload sub-range"
    );
    h.workload_emits(b"\x1b[23;1HWORKLOAD-LAST-ROW\nWALKED");
    h.assert_screens_agree("sub-range line feed");
    assert_eq!(
        h.host.screen().cursor_position().0 + 1,
        23,
        "the cursor must stay on the workload's last row"
    );
    h.assert_bar_drawn("sub-range line feed");
}

/// The client must never write DECSC/DECRC into a stream it is only
/// relaying -- not from the status bar and not from the layout code. This
/// is a hard cut, not a preference: there is one save-cursor register and
/// it belongs to the workload.
#[test]
fn client_never_writes_the_shared_save_cursor_register() {
    let ctx = status_ctx_for_test(true);
    let restore = ctx
        .screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .cursor_restore();
    let mut bytes = status_bar_redraw(&ctx, true).expect("a ground-state redraw writes");
    bytes.extend_from_slice(&terminal_layout_sequence(24, &restore));
    bytes.extend_from_slice(TERMINAL_RESET_SEQUENCE);
    assert!(!bytes.is_empty());
    for pair in [&b"\x1b7"[..], b"\x1b8"] {
        assert!(
            !bytes.windows(2).any(|w| w == pair),
            "client emitted {pair:?}: {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }
}

#[test]
fn draw_status_bar_not_reserved_never_writes() {
    let ctx = status_ctx_for_test(false);
    assert!(!draw_status_bar(&ctx, false));
    assert!(!draw_status_bar(&ctx, true));
}

#[test]
fn live_screen_refresh_repaints_model_contents_and_the_bar() {
    let ctx = status_ctx_for_test(true);
    feed_test_screen(&ctx.screen, b"recover-me\r\n");
    let bytes = live_screen_refresh_locked(&ctx).expect("ground-state refresh writes");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        bytes.windows(4).any(|w| w == b"\x1b[2J") || bytes.windows(3).any(|w| w == b"\x1b[J"),
        "refresh must clear before repainting: {text:?}"
    );
    assert!(
        text.contains("recover-me"),
        "refresh must include the live screen: {text:?}"
    );
    assert!(
        bytes.windows(4).any(|w| w == b"\x1b[7m"),
        "refresh must restore the status bar the snapshot's clear wiped: {text:?}"
    );
    assert!(!ctx.pending_refresh.load(Ordering::Relaxed));
}

#[test]
fn live_screen_refresh_defers_mid_escape_sequence() {
    let ctx = status_ctx_for_test(true);
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");
    assert!(live_screen_refresh_locked(&ctx).is_none());
    assert!(
        ctx.pending_refresh.load(Ordering::Relaxed),
        "a deferred refresh must be retried at the next safe boundary"
    );
}

#[test]
fn draw_status_bar_dirty_check_reports_skip_vs_real_write() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let _guard = StdoutToDevNull::new();
    let ctx = status_ctx_for_test(true);
    // Nothing drawn yet: even a non-forced call must actually write
    // (there's no `last_drawn` to compare against).
    assert!(
        draw_status_bar(&ctx, false),
        "first draw must be a real write"
    );
    // Same record/geometry, so the rendered text is unchanged: a
    // non-forced call must be a dirty-check no-op, not a real write --
    // this is exactly the case the timer-starvation bug got wrong by
    // treating a no-op the same as a real write for timer-reset
    // purposes.
    assert!(
        !draw_status_bar(&ctx, false),
        "unchanged text must be a dirty-check skip, not a real write"
    );
    // `force: true` must bypass the dirty-check unconditionally, since
    // that's the self-heal guarantee the overdue timer and every
    // switch/flash redraw rely on.
    assert!(
        draw_status_bar(&ctx, true),
        "force=true must always be a real write, even with unchanged text"
    );
}

#[test]
fn real_zero_sized_pty_uses_conventional_geometry() {
    use std::os::fd::AsRawFd;

    let (_master, slave) = aplexer::open_pty(0, 0).unwrap();
    assert_eq!(
        terminal_size(slave.as_raw_fd()),
        Some((
            aplexer::screen::DEFAULT_TERMINAL_ROWS,
            aplexer::screen::DEFAULT_TERMINAL_COLS,
        ))
    );
}

// -- The typing bar is a live-stream writer, not a suspended one -------
//
// `i` (type-through) hands the keyboard back while the pager stays up,
// and the relay streams workload bytes to the host again. The typing bar
// is then client-originated output spliced into a live stream, with the
// same two obligations as the live bar: never splice mid-sequence, and
// repair the row when the workload's Erase-in-Display -- which ignores
// scroll margins -- takes it out. Reproduced against a real zcodex
// session: after wheel-up + `i`, the workload's first `CSI ... J` wiped
// the bar and nothing ever rewrote it, because the frame loop `continue`d
// past every bar path while scroll mode was active and the status tick's
// dirty check saw unchanged text.

fn ctx_in_typing_mode() -> StatusBarCtx {
    let ctx = status_ctx_for_test(true);
    ctx.scroll.active.store(true, Ordering::SeqCst);
    ctx.scroll.typing.store(true, Ordering::SeqCst);
    ctx
}

#[test]
fn typing_bar_waits_for_an_escape_boundary_and_parks_until_one() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = ctx_in_typing_mode();
    // Half a CSI sequence: the relayed stream is mid-escape, so the bar
    // write must be refused, parked for the frame loop, and nothing may
    // reach the terminal.
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");
    let pipe = StdoutToPipe::new();
    assert!(
        !refresh_scroll_bar(&ctx),
        "a mid-sequence typing-bar write must be deferred"
    );
    assert!(
        pipe.take().is_empty(),
        "a deferred typing-bar write must not reach the terminal"
    );
    assert!(
        ctx.pending.load(Ordering::Relaxed),
        "a deferred typing-bar write must be parked for the frame loop"
    );
    // Complete the sequence: the parked write goes out at the boundary,
    // and `pending` (which forced the write past the dirty check) is
    // cleared so the tick's dirty check is honest again.
    feed_test_screen(&ctx.screen, b"m");
    let pipe = StdoutToPipe::new();
    assert!(refresh_scroll_bar(&ctx), "the parked write flushes");
    let text = String::from_utf8_lossy(&pipe.take()).into_owned();
    assert!(
        text.contains("TYPE"),
        "the typing wording must reach the bar row: {text:?}"
    );
    assert!(
        !ctx.pending.load(Ordering::Relaxed),
        "a delivered write must clear the parking flag"
    );
}

#[test]
fn layout_erase_while_typing_repairs_an_unchanged_bar_row() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = ctx_in_typing_mode();
    let pipe = StdoutToPipe::new();
    assert!(refresh_scroll_bar(&ctx), "first draw writes the bar");
    assert!(!pipe.take().is_empty());
    // The dirty check must skip when nothing changed -- this is what
    // kept the erased row blank forever before the fix: the text was
    // unchanged, so every later refresh saw "already drawn" and stopped.
    assert!(
        !refresh_scroll_bar(&ctx),
        "unchanged text must be a dirty-check skip"
    );
    // What the `Layout` arm does when the workload erased the screen
    // while typing: invalidate `last_drawn`, then refresh. The text is
    // byte-identical; the write must happen anyway, because the row the
    // text lives on no longer holds it.
    *ctx.last_drawn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    let pipe = StdoutToPipe::new();
    assert!(
        refresh_scroll_bar(&ctx),
        "an invalidated dirty check must rewrite the bar row"
    );
    assert!(
        !pipe.take().is_empty(),
        "the repair must be a real write, not a bookkeeping update"
    );
}

#[test]
fn pager_bar_without_typing_still_writes_unconditionally() {
    // Deliberate asymmetry, pinned so it reads as decided rather than
    // forgotten: with the pager up but NOT typing, the relay is
    // suspended, so there is no live stream to splice into and the bar
    // does not wait for a boundary -- a workload stopped mid-sequence
    // must not freeze the bar the user is actively reading against.
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = status_ctx_for_test(true);
    ctx.scroll.active.store(true, Ordering::SeqCst);
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");
    let pipe = StdoutToPipe::new();
    assert!(
        refresh_scroll_bar(&ctx),
        "a stream-suspended write goes out at once"
    );
    assert!(!pipe.take().is_empty());
    assert!(
        !ctx.pending.load(Ordering::Relaxed),
        "a stream-suspended write never parks"
    );
}

// -- Issue #14: the resize path is a client-originated writer too ------
//
// `2db19d0` put the escape-boundary gate inside `draw_status_bar`, which
// covered its eight callers and silently did not cover
// `apply_terminal_layout` -- a ninth writer, driven by the resize
// poller's wall clock, that wrote DECSTBM straight to stdout. Nothing
// failed; a reviewer found it by reading. These tests are what fails
// instead, and the last one is what fails for writer eleven.

/// Runs the real resize path into a buffer instead of fd 1. Nothing here
/// re-implements production's decision -- `apply_terminal_layout_to` is
/// what `apply_terminal_layout` calls under the stdout lock -- and
/// keeping the process's fd 1 out of it means these tests neither
/// serialize on `FD1_GUARD` nor can catch another thread's stray write.
fn resize_capturing(ctx: &StatusBarCtx, rows: u16, cols: u16) -> (bool, Vec<u8>) {
    let mut sink = Vec::new();
    let wrote = apply_terminal_layout_to(&mut sink, ctx, rows, cols);
    assert_eq!(
        wrote,
        !sink.is_empty(),
        "the resize path's return value must agree with what it actually wrote"
    );
    (wrote, sink)
}

fn flush_capturing(ctx: &StatusBarCtx) -> (bool, Vec<u8>) {
    let mut sink = Vec::new();
    let wrote = flush_pending_layout_to(&mut sink, ctx);
    (wrote, sink)
}

/// Backdates the parked resize's deadline, standing in for
/// `LAYOUT_DEFER_LIMIT` having elapsed without the stream ever reaching
/// a boundary -- a workload that stopped mid-escape-sequence.
fn expire_layout_deferral(ctx: &StatusBarCtx) {
    let mut pending = ctx
        .pending_layout
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(p) = pending.as_mut() {
        p.since = Instant::now()
            .checked_sub(LAYOUT_DEFER_LIMIT * 2)
            .expect("backdate the layout deadline");
    }
}

/// A resize raised while the workload is mid-escape-sequence must put
/// nothing on the wire. This is the splice the issue describes: the host
/// terminal would abandon the workload's half-emitted CSI and print its
/// remaining parameter bytes as literal text.
///
/// The geometry is still recorded on the spot, deliberately: the
/// physical terminal has already changed size, and `TermGeom` is
/// internal state rather than output, so the status bar must start
/// targeting the real last row immediately.
#[test]
fn resize_mid_escape_sequence_defers_decstbm_instead_of_splicing() {
    let ctx = status_ctx_for_test(true);

    // Control: at a boundary the same call writes, so a later "nothing
    // was written" assertion means the gate, not a broken fixture.
    let (wrote, bytes) = resize_capturing(&ctx, 24, 80);
    assert!(wrote, "a resize at an escape boundary must be written");
    assert!(
        String::from_utf8_lossy(&bytes).contains("\x1b[1;23r"),
        "expected the row reservation for a 24-row terminal, got {:?}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(
        ctx.pending_layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none(),
        "a resize that was written must leave nothing parked"
    );

    // Now mid-CSI, exactly as a PTY read boundary leaves the stream
    // about half the time under a streaming TUI.
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");
    assert!(
        !ctx.screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at_escape_boundary(),
        "the fixture must actually be mid-sequence for this test to mean anything"
    );
    let (wrote, bytes) = resize_capturing(&ctx, 30, 100);
    assert!(!wrote, "a resize raised mid-sequence must not be written");
    assert!(
        bytes.is_empty(),
        "nothing may reach the terminal mid-sequence, got {:?}",
        String::from_utf8_lossy(&bytes)
    );
    let parked = *ctx
        .pending_layout
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let parked = parked.expect("a deferred resize must be parked, not dropped");
    assert_eq!((parked.rows, parked.cols), (30, 100));
    let geom = *ctx.term.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(
        (geom.rows, geom.cols),
        (30, 100),
        "the new physical geometry must be recorded even while the bytes wait"
    );
}

/// Deferral is not dropping. The parked resize must reach the terminal
/// at the next boundary -- and must carry the *latest* geometry, since a
/// superseded size would leave the workload rendering at the wrong
/// geometry just as surely as dropping it would.
#[test]
fn deferred_resize_is_delivered_at_the_next_boundary_with_the_latest_geometry() {
    let ctx = status_ctx_for_test(true);
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");

    assert!(!resize_capturing(&ctx, 28, 80).0);
    // A second resize while the first is still parked: the user kept
    // dragging the window edge.
    assert!(!resize_capturing(&ctx, 32, 100).0);
    let (wrote, bytes) = flush_capturing(&ctx);
    assert!(
        !wrote && bytes.is_empty(),
        "a flush while still mid-sequence must stay silent, got {:?}",
        String::from_utf8_lossy(&bytes)
    );

    // The workload completes its sequence: the stream is at a boundary
    // again, which is exactly what the frame loop's flush waits for.
    feed_test_screen(&ctx.screen, b"91m");
    let (wrote, bytes) = flush_capturing(&ctx);
    assert!(wrote, "the deferred resize must be delivered, not dropped");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(
        text.contains("\x1b[1;31r"),
        "the latest geometry (32 rows -> DECSTBM 1;31) must be the one delivered, got {text:?}"
    );
    assert!(
        !text.contains("\x1b[1;27r"),
        "a superseded deferred resize must not be the one delivered, got {text:?}"
    );
    assert!(
        ctx.pending_layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none(),
        "a delivered resize must clear the parking slot"
    );
    // Idempotent: nothing parked, nothing written.
    let (wrote, bytes) = flush_capturing(&ctx);
    assert!(
        !wrote && bytes.is_empty(),
        "flushing with nothing parked must be a no-op, got {:?}",
        String::from_utf8_lossy(&bytes)
    );
}

/// The one case the boundary gate cannot wait out: a workload that stops
/// emitting part-way through an escape sequence. There is no next
/// boundary, so `LAYOUT_DEFER_LIMIT` writes anyway -- one spliced frame
/// beats a host terminal left scrolling the old geometry for the rest of
/// the attach, which is the failure the issue calls worse than the
/// splice. This asserts the exemption rather than describing it.
#[test]
fn deferred_resize_is_written_once_the_defer_limit_expires() {
    let ctx = status_ctx_for_test(true);
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");
    assert!(!resize_capturing(&ctx, 20, 80).0);

    expire_layout_deferral(&ctx);
    assert!(
        !ctx.screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at_escape_boundary(),
        "the stream must still be mid-sequence: the point is that the deadline, \
             not a recovered boundary, is what delivers this"
    );
    let (wrote, bytes) = flush_capturing(&ctx);
    assert!(
        wrote,
        "past LAYOUT_DEFER_LIMIT the resize must go out rather than be stranded"
    );
    assert!(
        String::from_utf8_lossy(&bytes).contains("\x1b[1;19r"),
        "expected the row reservation for a 20-row terminal, got {:?}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(
        ctx.pending_layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none(),
        "the deadline write must also clear the parking slot"
    );
}

/// A physical grow changes the bar's target row, but DECSTBM only changes
/// scrolling behavior; it does not erase the bar that was painted at the
/// old bottom. The production wrapper must therefore repaint the model so
/// the old row is cleared and the only visible bar is on the new bottom.
#[test]
fn resize_repaints_the_status_bar_at_the_new_bottom() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = status_ctx_for_test(true);
    let mut host = vt100::Parser::new(34, 80, 0);

    let old_bar = status_bar_redraw(&ctx, true).expect("the old bar must render");
    host.process(&old_bar);
    assert!(
        host.screen()
            .contents_between(23, 0, 23, 80)
            .contains("RUNNING"),
        "the fixture must start with a bar on the old bottom row"
    );

    ctx.screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .set_size(33, 80);
    let pipe = StdoutToPipe::new();
    assert!(apply_terminal_layout(&ctx, 34, 80));
    let repaint = pipe.take();
    host.process(&repaint);

    assert!(
        !host
            .screen()
            .contents_between(23, 0, 23, 80)
            .contains("RUNNING"),
        "the old status-bar row must be cleared after a grow"
    );
    assert!(
        host.screen()
            .contents_between(33, 0, 33, 80)
            .contains("RUNNING"),
        "the status bar must be painted on the physical bottom row"
    );
}

/// The screen-level statement of the same thing, through a real `vt100`
/// host terminal: after a resize raised in the middle of a workload's
/// absolute-positioning sequence, every row the workload can reach must
/// still render exactly what the workload drew.
///
/// The second half is the control. It replays the identical resize the
/// *ungated* code would have written at the same offset, and asserts the
/// host screen is then wrong -- so this test fails if the gate is
/// removed, rather than passing because the splice happened to be
/// harmless.
#[test]
fn resize_across_a_mid_sequence_split_leaves_the_host_screen_intact() {
    let ctx = status_ctx_for_test(true);
    let (rows, cols) = (24u16, 80u16);
    let mut host = vt100::Parser::new(rows, cols, 0);
    let mut ungated = vt100::Parser::new(rows, cols, 0);
    let mut workload = vt100::Parser::new(rows - 1, cols, 0);
    for p in [&mut host, &mut ungated] {
        p.process(format!("\x1b[1;{}r", rows - 1).as_bytes());
    }

    // Ink-shaped output: words painted at absolute columns, which is
    // what makes a misaligned injection weld two frames onto one row.
    let frame = b"\x1b[2;1H\x1b[0m\x1b[2GQuick\x1b[8Gsafety\x1b[16Gcheck";
    // Split inside `\x1b[16G` -- a CSI with its parameters half emitted,
    // which is what a PTY read boundary looks like about half the time
    // under a streaming TUI.
    let split = frame.len() - 7;
    for p in [&mut host, &mut ungated] {
        p.process(&frame[..split]);
    }
    workload.process(&frame[..split]);
    feed_test_screen(&ctx.screen, &frame[..split]);

    // What the resize poller does at this instant.
    let (wrote, _) = resize_capturing(&ctx, 20, cols);
    assert!(!wrote, "the resize must be deferred here, not written");
    // What it used to do: DECSTBM straight onto the wire, mid-CSI.
    let restore = ctx
        .screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .cursor_restore();
    ungated.process(&terminal_layout_sequence(20, &restore));

    for p in [&mut host, &mut ungated] {
        p.process(&frame[split..]);
    }
    workload.process(&frame[split..]);
    feed_test_screen(&ctx.screen, &frame[split..]);
    let (wrote, bytes) = flush_capturing(&ctx);
    assert!(wrote, "the deferred resize must still be delivered");
    host.process(&bytes);

    let row_of = |p: &vt100::Parser| p.screen().contents_between(1, 0, 1, cols);
    assert_eq!(
        row_of(&host),
        row_of(&workload),
        "the gated resize must leave the host row exactly as the workload drew it"
    );
    assert_ne!(
        row_of(&ungated),
        row_of(&workload),
        "control: the ungated resize must actually corrupt this row, otherwise \
             this test would pass with the gate removed"
    );
}

// -- The structural guard --------------------------------------------
//
// Issue #14 was a missed *caller*, not a subtle race, and the next one
// will be too: someone adds a writer, does not know the gate exists,
// and no test notices. So the rule is enforced over the source text --
// every function in this file that can put bytes on the host terminal
// is enumerated here, with how it is allowed to do so.

/// Every function in src/bin/a.rs, outside this test module, that calls
/// `write_all` -- i.e. that reaches the terminal without going through
/// the funnel -- and the reason it is allowed to. A new one fails
/// `every_client_terminal_write_site_is_gated_or_explicitly_exempt`.
const RAW_TERMINAL_WRITERS: &[(&str, &str)] = &[
    (
        "write_client_locked",
        "the gate itself: it performs the boundary check it is named for",
    ),
    (
        "write_locked",
        "attach start and detach only, pinned by WRITE_LOCKED_CALLERS below -- there is \
             no relayed stream to splice before the first workload byte or after the last, \
             and neither may be deferrable",
    ),
    (
        "relay_to_terminal",
        "relays the workload's own bytes; it *is* the stream, not an injection",
    ),
    (
        "feed_and_write",
        "the attach snapshot and a switch's replayed screen: a full repaint that replaces \
             the stream rather than splicing into it, fed to the model under the same lock",
    ),
    (
        "cmd_capture",
        "`a capture` on a plain stdout; no attach, no relayed stream",
    ),
];

/// Every client-originated injection into a live relayed stream. Each
/// goes through `write_client_locked`, so each is boundary-gated by
/// construction rather than by remembering. Listed so the census is
/// visible: `apply_terminal_layout` was the ninth writer that nobody had
/// written down (issue #14).
const FUNNELLED_WRITERS: &[&str] = &[
    "apply_terminal_layout_to",
    "draw_status_bar",
    "redraw_live_screen",
    "paint_scroll_view",
    "refresh_scroll_bar",
    "paint_live_screen",
    "sync_client_mouse",
    "paint_key_overlay",
    "dismiss_key_overlay",
];

/// `write_locked` writes unconditionally, so it is a second route to the
/// terminal and would be a hole in the funnel if it could be called from
/// anywhere. These are the only two places allowed to.
const WRITE_LOCKED_CALLERS: &[&str] = &["attach", "reset_terminal"];

/// The production source slices, in the same order that `app.rs` includes
/// them. Compiled in, so the write census checks the exact source used by the
/// binary rather than a hand-maintained copy.
const PRODUCTION_SOURCE: &str = concat!(
    include_str!("a/cli.rs"),
    "\n",
    include_str!("a/commands.rs"),
    "\n",
    include_str!("a/list_plain.rs"),
    "\n",
    include_str!("a/list_tty.rs"),
    "\n",
    include_str!("a/list_helpers.rs"),
    "\n",
    include_str!("a/session_commands.rs"),
    "\n",
    include_str!("a/diagnostics.rs"),
    "\n",
    include_str!("a/rpc.rs"),
    "\n",
    include_str!("a/terminal.rs"),
    "\n",
    include_str!("a/status_bar.rs"),
    "\n",
    include_str!("a/scroll.rs"),
    "\n",
    include_str!("a/scroll_input.rs"),
    "\n",
    include_str!("a/switching.rs"),
    "\n",
    include_str!("a/input_scanner.rs"),
    "\n",
    include_str!("a/attach.rs"),
    "\n",
    include_str!("a/attach_input.rs"),
    "\n",
    include_str!("a/attach_session.rs"),
    "\n",
    include_str!("a/attach_threads.rs"),
    "\n",
    include_str!("a/system.rs"),
);

fn production_source_lines() -> Vec<&'static str> {
    PRODUCTION_SOURCE.lines().collect()
}

/// Maps each line matching `needle` to the name of the nearest
/// preceding `fn` declaration. Comment lines are skipped so a doc
/// comment mentioning a call is not mistaken for one.
fn enclosing_fns_of(lines: &[&str], needle: &str) -> Vec<(String, String)> {
    let mut current = String::new();
    let mut hits = Vec::new();
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        for prefix in ["fn ", "pub fn ", "pub(crate) fn ", "unsafe fn "] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                current = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                break;
            }
        }
        if trimmed.contains(needle) {
            hits.push((current.clone(), trimmed.to_string()));
        }
    }
    hits
}

/// The body of a top-level `fn`, from its declaration to the `}` that
/// closes it at column 0.
fn top_level_fn_body(lines: &[&str], name: &str) -> String {
    let private_decl = format!("fn {name}(");
    let crate_decl = format!("pub(crate) fn {name}(");
    let start = lines
        .iter()
        .position(|l| l.starts_with(&private_decl) || l.starts_with(&crate_decl))
        .unwrap_or_else(|| panic!("no top-level `fn {name}` in the production source slices"));
    let end = start
        + 1
        + lines[start + 1..]
            .iter()
            .position(|l| *l == "}")
            .unwrap_or_else(|| panic!("`fn {name}` is not closed at column 0"));
    lines[start..=end].join("\n")
}

/// The criterion the issue calls the most valuable one: a *new* ungated
/// writer fails here, instead of shipping and being found by a reviewer
/// reading the file (which is how #14 was found, after #5 declared the
/// class fixed).
///
/// Four things are pinned:
///
/// 1. the exact set of functions that can write to the terminal;
/// 2. that each one either goes through `write_client_locked` or carries
///    a written reason why it is not an injection;
/// 3. that `write_client_locked` really is the boundary check, and that
///    `write_locked` -- the unconditional second route -- is reachable
///    only from attach start and detach;
/// 4. that the deadline exemption stays a single call site.
#[test]
fn every_client_terminal_write_site_is_gated_or_explicitly_exempt() {
    use std::collections::BTreeSet;

    let lines = production_source_lines();

    // 1. Nobody new may reach `write_all` directly.
    let raw: BTreeSet<String> = enclosing_fns_of(&lines, "write_all")
        .into_iter()
        .map(|(f, _)| f)
        .collect();
    let declared_raw: BTreeSet<String> = RAW_TERMINAL_WRITERS
        .iter()
        .map(|(n, _)| (*n).to_string())
        .collect();
    let undeclared: Vec<&String> = raw.difference(&declared_raw).collect();
    assert!(
        undeclared.is_empty(),
        "new raw write(s) to the host terminal, not listed in RAW_TERMINAL_WRITERS: \
             {undeclared:?}. Client-originated bytes must go through `write_client_locked` \
             (the escape-boundary gate) instead; if the write genuinely cannot splice a \
             relayed stream, add it to RAW_TERMINAL_WRITERS with that reason. This is issue \
             #14: an ungated writer corrupts a workload's half-emitted escape sequences."
    );
    let stale: Vec<&String> = declared_raw.difference(&raw).collect();
    assert!(
        stale.is_empty(),
        "RAW_TERMINAL_WRITERS lists function(s) that no longer call write_all: {stale:?}. \
             Drop them, so the list stays an accurate census rather than folklore."
    );

    // 2. The injection census: gated by construction, but written down,
    //    because #14 was a writer nobody had written down.
    let funnelled: BTreeSet<String> = enclosing_fns_of(&lines, "write_client_locked(")
        .into_iter()
        .map(|(f, _)| f)
        .filter(|f| f != "write_client_locked")
        .collect();
    let declared_funnelled: BTreeSet<String> =
        FUNNELLED_WRITERS.iter().map(|n| (*n).to_string()).collect();
    assert_eq!(
        funnelled, declared_funnelled,
        "the set of client-originated injections changed. A new one is already \
             boundary-gated (that is what `write_client_locked` is for) -- add its name to \
             FUNNELLED_WRITERS so the census stays true, and check that it parks a refused \
             write for a later boundary instead of dropping it."
    );
    for name in FUNNELLED_WRITERS {
        let body = top_level_fn_body(&lines, name);
        assert!(
            body.contains("write_client_locked("),
            "`{name}` no longer goes through `write_client_locked`, so its bytes are not \
                 boundary-gated (issue #14)"
        );
    }

    let gate = top_level_fn_body(&lines, "write_client_locked");
    assert!(
        gate.contains("at_escape_boundary()"),
        "`write_client_locked` must be the escape-boundary check; every Funnelled writer \
             above relies on it being one"
    );

    let raw_callers: Vec<String> = enclosing_fns_of(&lines, "write_locked(")
        .into_iter()
        .map(|(f, _)| f)
        .filter(|f| f != "write_locked" && f != "write_client_locked")
        .collect();
    for caller in &raw_callers {
        assert!(
            WRITE_LOCKED_CALLERS.contains(&caller.as_str()),
            "`{caller}` calls `write_locked`, which writes without consulting the escape \
                 boundary. Only attach start and detach may (there is no relayed stream to \
                 splice at either); a live injection must use `write_client_locked`."
        );
    }

    let past_deadline: Vec<(String, String)> =
        enclosing_fns_of(&lines, "BoundaryPolicy::PastDeadline")
            .into_iter()
            .filter(|(f, _)| f != "write_client_locked")
            .collect();
    assert_eq!(
            past_deadline.len(),
            1,
            "`BoundaryPolicy::PastDeadline` is the client's only exemption from the boundary \
             gate and must stay one narrow call site (the resize deadline), found: {past_deadline:?}"
        );
    assert_eq!(
        past_deadline[0].0, "apply_terminal_layout_to",
        "the deadline exemption belongs to the resize path and nothing else"
    );
}

/// `BoundaryPolicy::StreamSuspended` says "the relay is not writing to
/// the host at all right now", which is true of exactly one class of
/// thing: a *client modal* that has taken the host terminal away from the
/// relay entirely -- the scroll-mode pager, and the `Ctrl-b` key overlay,
/// which suspends the relay the same way and for the same reason (see
/// `KeyOverlay`). Pinned the same way the deadline exemption is, so it
/// cannot quietly become a general-purpose way around the gate.
///
/// Two properties, both of which the variant's correctness rests on:
/// only a modal's writers may pass it, and every write that is the *first*
/// one after the suspension must lead with `SCROLL_CANCEL` -- the `CAN`
/// that ends whatever sequence the host was part-way through when the
/// relay was suspended.
#[test]
fn scroll_mode_writes_are_the_only_stream_suspended_ones() {
    use std::collections::BTreeSet;

    /// Every function allowed to write while the relay is suspended, and
    /// where its `SCROLL_CANCEL` prefix comes from.
    const SUSPENDED_WRITERS: &[(&str, bool)] = &[
        // (name, must build its own SCROLL_CANCEL-led sequence)
        ("paint_scroll_view", true),
        ("paint_live_screen", true),
        // The overlay's two frames. Either can be the first write after
        // the relay was suspended -- `paint_key_overlay` always is, and
        // `dismiss_key_overlay` is whenever nothing was repainted in
        // between (a resize, say) -- so both build their own.
        ("paint_key_overlay", true),
        ("dismiss_key_overlay", true),
        // The bar row is drawn *into* a screen the pager already owns
        // and already cancelled; it is not the first write after the
        // suspension, so it needs no CAN of its own.
        ("refresh_scroll_bar", false),
        // Hands the mouse over while the pager is up; same reasoning.
        ("sync_client_mouse", false),
    ];

    let lines = production_source_lines();
    let found: BTreeSet<String> = enclosing_fns_of(&lines, "BoundaryPolicy::StreamSuspended")
        .into_iter()
        .map(|(f, _)| f)
        .filter(|f| f != "write_client_locked")
        .collect();
    let declared: BTreeSet<String> = SUSPENDED_WRITERS
        .iter()
        .map(|(n, _)| (*n).to_string())
        .collect();
    assert_eq!(
        found, declared,
        "`BoundaryPolicy::StreamSuspended` bypasses the escape-boundary gate on the \
             grounds that scroll mode has suspended the relay entirely. Only scroll mode may \
             claim that. If a new writer genuinely runs with the relay suspended, add it here \
             with whether it must lead with SCROLL_CANCEL; otherwise use BoundaryPolicy::Defer."
    );
    for (name, needs_cancel) in SUSPENDED_WRITERS {
        if !needs_cancel {
            continue;
        }
        let body = top_level_fn_body(&lines, name);
        assert!(
            body.contains("SCROLL_CANCEL"),
            "`{name}` writes the first bytes after the relay is suspended, so it must lead \
                 with SCROLL_CANCEL (CAN) to end whatever escape sequence the host was \
                 part-way through -- that prefix is what makes skipping the boundary gate safe"
        );
    }
}
