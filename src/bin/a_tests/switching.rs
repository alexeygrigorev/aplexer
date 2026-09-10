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
