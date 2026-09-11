
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
fn attach_handshake_treats_deadline_expiries_as_transient_and_nothing_else() {
    // A worker busy past the control deadline -- a history fsync holding
    // the hub lock a subscribe waits behind -- surfaces as a bare
    // SO_RCVTIMEO WouldBlock, possibly under context layers. Those must be
    // retryable, or every such stall reports "disconnect on attach".
    assert!(is_deadline_expiry(&anyhow::Error::from(io::Error::from(
        io::ErrorKind::WouldBlock
    ))));
    assert!(is_deadline_expiry(&anyhow::Error::from(io::Error::from(
        io::ErrorKind::TimedOut
    ))));
    assert!(is_deadline_expiry(
        &anyhow::Error::from(io::Error::from(io::ErrorKind::WouldBlock))
            .context("set control response deadline")
    ));
    // A genuine answer must reach the user instead of being retried into.
    assert!(!is_deadline_expiry(&anyhow::Error::msg("worker closed connection")));
    assert!(!is_deadline_expiry(&anyhow::Error::from(io::Error::from(
        io::ErrorKind::ConnectionRefused
    ))));
    // The handshake budget strictly exceeds the control budget: a plain
    // RPC stays snappy while Attach waits out a busy disk.
    assert!(ATTACH_HANDSHAKE_TIMEOUT > CONTROL_RPC_TIMEOUT);
}
