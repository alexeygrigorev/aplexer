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
