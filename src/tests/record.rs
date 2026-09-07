//! Unit tests for the session record model.

use super::*;

#[test]
fn session_record_write_persists_worker_start_identity_once() {
    let root = tempfile::tempdir().unwrap();
    let record_path = root.path().join("session.json");
    let pid = std::process::id();
    atomic_write_json(
        &record_path,
        &serde_json::json!({"worker_pid": pid, "value": 1}),
    )
    .unwrap();
    let identity_path = root.path().join(WORKER_IDENTITY_FILE);
    let original: ProcessIdentity =
        serde_json::from_slice(&fs::read(&identity_path).unwrap()).unwrap();
    assert_eq!(original.pid, pid);
    assert_eq!(original.boot_id, linux_boot_id().unwrap());
    assert_eq!(
        original.start_time_ticks,
        process_start_time_ticks(pid).unwrap()
    );

    // A later write must not refresh the immutable registration.
    atomic_write_json(
        &record_path,
        &serde_json::json!({"worker_pid": pid, "value": 2}),
    )
    .unwrap();
    let after: ProcessIdentity =
        serde_json::from_slice(&fs::read(&identity_path).unwrap()).unwrap();
    assert_eq!(after.pid, original.pid);
    assert_eq!(after.start_time_ticks, original.start_time_ticks);
}

fn liveness_record(state_dir: &Path) -> SessionRecord {
    let pid = std::process::id();
    SessionRecord {
        parent_session: None,
        schema_version: SCHEMA_VERSION,
        id: Uuid::new_v4(),
        workspace: state_dir.to_path_buf(),
        tag: "identity-test".into(),
        engine: "shell".into(),
        profile: None,
        command: vec!["/bin/true".into()],
        cwd: state_dir.to_path_buf(),
        env: BTreeMap::new(),
        env_unset: Vec::new(),
        limits: Limits::default(),
        history_bytes: DEFAULT_HISTORY_BYTES,
        created_at_ms: 1,
        updated_at_ms: 1,
        last_activity_ms: None,
        last_accessed_ms: None,
        reported_state: None,
        reported_state_at_ms: None,
        phase: Phase::Running,
        worker_pid: Some(pid),
        workload_pid: None,
        worker_cgroup: None,
        workload_cgroup: None,
        containment_cgroup: None,
        containment_cgroup_identity: None,
        containment_empty: Some(false),
        socket_path: state_dir.join("control.sock"),
        history_path: state_dir.join("history.bin"),
        exit: None,
        error: None,
    }
}

/// The two proof shapes that short-circuit before the kernel is ever
/// consulted, and the no-locator shape that is the reported zombie.
#[test]
fn containment_reap_verdict_reads_durable_proof_without_probing() {
    let state = tempfile::tempdir().unwrap();
    let base = liveness_record(state.path());
    let refuse = |_: Uuid, _: &Path, _: Option<&CgroupIdentity>| -> Result<bool> {
        panic!("probe must not run when the record already answers the question")
    };

    let mut proven = base.clone();
    proven.containment_empty = Some(true);
    assert_eq!(
        containment_reap_verdict_with(&proven, refuse),
        ContainmentReap::Proven,
        "a worker's own durable proof must still be trusted"
    );

    let mut legacy_exit = base.clone();
    legacy_exit.containment_empty = None;
    legacy_exit.exit = Some(ExitInfo {
        code: Some(0),
        signal: None,
        oom_killed: false,
        exited_at_ms: 2,
    });
    assert_eq!(
        containment_reap_verdict_with(&legacy_exit, refuse),
        ContainmentReap::Proven,
        "the legacy pre-field ExitInfo proof must still be trusted"
    );

    // The reported zombie shape: unlimited session, worker SIGKILLed
    // before it could prove anything. No locator, so nothing to probe.
    let unlimited = base.clone();
    assert_eq!(unlimited.containment_cgroup, None);
    assert_eq!(unlimited.containment_empty, Some(false));
    assert_eq!(
        containment_reap_verdict_with(&unlimited, refuse),
        ContainmentReap::NoRemainingHandle
    );
}

/// Every outcome the kernel probe can return, including the one arm that
/// stands between `a prune` and deleting the last handle to a live
/// containment domain: a locator that validates and is still POPULATED
/// must retain. Injected rather than staged on a real cgroup so this
/// runs everywhere, on every `cargo test`, with no delegation needed;
/// `recorded_cgroup_observed_empty_tracks_a_real_delegated_cgroup`
/// covers the probe itself against a real one.
#[test]
fn containment_reap_verdict_maps_every_cgroup_probe_outcome() {
    let state = tempfile::tempdir().unwrap();
    let mut record = liveness_record(state.path());
    record.containment_empty = Some(false);
    record.containment_cgroup = Some(PathBuf::from(format!(
        "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
        record.id
    )));

    assert_eq!(
        containment_reap_verdict_with(&record, |_, _, _| Ok(true)),
        ContainmentReap::Proven,
        "an observed-empty domain is proof at least as strong as the persisted bit"
    );
    assert_eq!(
        containment_reap_verdict_with(&record, |_, _, _| Ok(false)),
        ContainmentReap::Retain,
        "a populated containment domain must keep its locator"
    );
    assert_eq!(
        containment_reap_verdict_with(&record, |_, _, _| bail!("cgroup inspection failed")),
        ContainmentReap::Retain,
        "an unreadable containment domain must fail closed"
    );

    // The probe is handed the record's own identity triple -- a mixed-up
    // locator would validate against the wrong domain.
    let mut seen = None;
    containment_reap_verdict_with(&record, |id, locator, identity| {
        seen = Some((id, locator.to_path_buf(), identity.cloned()));
        Ok(false)
    });
    let (id, locator, identity) = seen.expect("probe ran");
    assert_eq!(id, record.id);
    assert_eq!(Some(locator), record.containment_cgroup);
    assert!(identity.is_none());
}

/// A recorded cgroup with no identity cannot be validated, so it cannot
/// be declared empty either -- keep the locator.
#[test]
fn containment_reap_verdict_retains_an_unvalidatable_locator() {
    let state = tempfile::tempdir().unwrap();
    let mut record = liveness_record(state.path());
    record.containment_cgroup = Some(PathBuf::from(format!(
        "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
        record.id
    )));
    assert!(record.containment_cgroup_identity.is_none());
    assert_eq!(
        containment_reap_verdict(&record),
        ContainmentReap::Retain,
        "an unvalidatable containment locator must fail closed"
    );
}

/// A cgroup recorded under a different boot cannot hold a live process:
/// the hierarchy and every task in it ceased to exist at reboot. Without
/// this, `validate_recorded_cgroup`'s (correct, for destructive
/// recovery) refusal to touch a foreign-boot identity would make a
/// rebooted-away record permanently unreapable.
#[test]
fn containment_reap_verdict_treats_a_previous_boot_as_empty() {
    let state = tempfile::tempdir().unwrap();
    let mut record = liveness_record(state.path());
    record.containment_cgroup = Some(PathBuf::from(format!(
        "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
        record.id
    )));
    let mut identity = current_cgroup_identity().unwrap_or(CgroupIdentity {
        boot_id: String::new(),
        cgroup_namespace_device: 0,
        cgroup_namespace_inode: 0,
        mount_namespace_device: 0,
        mount_namespace_inode: 0,
        cgroup_mount_id: 0,
        cgroup_root_device: 0,
        cgroup_root_inode: 0,
    });
    identity.boot_id = "00000000-0000-0000-0000-000000000000".into();
    assert_ne!(identity.boot_id, linux_boot_id().unwrap());
    record.containment_cgroup_identity = Some(identity);
    assert_eq!(
        containment_reap_verdict(&record),
        ContainmentReap::Proven,
        "a cgroup from a previous boot cannot hold a live process"
    );
}



/// The real kernel probe, end to end, against a genuinely delegated
/// cgroup: empty, then POPULATED (the arm that must retain), then empty
/// again, then collected. `#[ignore]`d for the same reason
/// `tests/oom_isolation.rs`'s destructive tests are -- it needs a
/// cgroup-v2 tree with delegation to the running user, which a CI
/// container generally lacks. Run it explicitly:
///
///   cargo test --lib recorded_cgroup_observed_empty -- --ignored --nocapture
///
/// The decision arms it feeds are pinned unconditionally by
/// `containment_reap_verdict_maps_every_cgroup_probe_outcome`, and the
/// membership read by
/// `cgroup_path_populated_reads_the_kernel_counter_and_treats_enoent_as_empty`;
/// this test is what proves those two meet reality.
#[test]
#[ignore = "needs cgroup-v2 delegation to the running user; run explicitly"]
fn recorded_cgroup_observed_empty_tracks_a_real_delegated_cgroup() {
    let state = tempfile::tempdir().unwrap();
    let mut record = liveness_record(state.path());
    record.containment_empty = Some(false);
    let mut cgroup = DelegatedCgroup::create(record.id)
        .expect("this environment has no writable cgroup-v2 parent");
    record.containment_cgroup = Some(cgroup.path.clone());
    record.containment_cgroup_identity = Some(current_cgroup_identity().unwrap());
    let probe = || {
        recorded_cgroup_observed_empty(
            record.id,
            record.containment_cgroup.as_deref().unwrap(),
            record.containment_cgroup_identity.as_ref(),
        )
        .unwrap()
    };

    assert!(probe(), "a freshly created cgroup is empty");
    assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);

    let pid = cgroup.populate();
    assert!(
        !probe(),
        "a cgroup holding a live process must not read as empty"
    );
    assert_eq!(
        containment_reap_verdict(&record),
        ContainmentReap::Retain,
        "prune must keep the locator of a populated containment domain"
    );
    assert!(process_alive(pid), "probing must not signal anything");

    cgroup.drain_members();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !probe() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(probe(), "an emptied cgroup must read as empty again");
    assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);

    // Collected: cgroup v2 cannot remove a populated cgroup, so a
    // durably recorded locator that has since disappeared is empty by
    // construction.
    fs::remove_dir(&cgroup.path).expect("remove the now-empty cgroup");
    assert!(probe(), "a collected cgroup is empty by construction");
    assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);
}

/// `state` is derived from both facts and rewrites neither.
#[test]
fn observed_state_reports_broken_only_for_a_contradicted_phase() {
    let now = DEFAULT_STARTUP_TIMEOUT_MS * 2;
    let aged = |age: u64| now - age;
    // A `Starting` record with no live worker is the shape
    // `start_session` persists before the worker registers its pid, and
    // also the shape a crashed start leaves behind. Only age tells them
    // apart, and the boundary is exactly the startup budget.
    assert_eq!(
        observed_state(&Phase::Starting, false, aged(0), now),
        "starting"
    );
    assert_eq!(
        observed_state(
            &Phase::Starting,
            false,
            aged(DEFAULT_STARTUP_TIMEOUT_MS - 1),
            now
        ),
        "starting"
    );
    assert_eq!(
        observed_state(
            &Phase::Starting,
            false,
            aged(DEFAULT_STARTUP_TIMEOUT_MS),
            now
        ),
        "broken",
        "past the startup budget a pre-PID record is a crashed start"
    );
    // A record whose clock ran backwards (or was written by a machine
    // with a different clock) must not become permanently `starting`.
    assert_eq!(
        observed_state(&Phase::Starting, false, now + 1_000, now),
        "starting"
    );
    assert_eq!(observed_state(&Phase::Starting, true, 0, now), "starting");
    // Running/Exiting are only ever written by a worker that already
    // registered, so a dead worker there is broken at any age.
    for phase in [Phase::Running, Phase::Exiting] {
        assert_eq!(observed_state(&phase, false, aged(0), now), "broken");
        assert_eq!(observed_state(&phase, false, aged(1), now), "broken");
        assert_eq!(observed_state(&phase, true, aged(0), now), phase.name());
    }
    for phase in [Phase::Exited, Phase::Failed] {
        assert_eq!(observed_state(&phase, false, aged(0), now), phase.name());
        assert_eq!(observed_state(&phase, true, aged(0), now), phase.name());
    }
}

#[test]
fn worker_liveness_rejects_recycled_pid_identity() {
    let state = tempfile::tempdir().unwrap();
    let mut record = liveness_record(state.path());
    let pid = record.worker_pid.unwrap();
    let identity = ProcessIdentity {
        pid,
        start_time_ticks: process_start_time_ticks(pid).unwrap() + 1,
        boot_id: linux_boot_id().unwrap(),
    };
    fs::write(
        state.path().join(WORKER_IDENTITY_FILE),
        serde_json::to_vec(&identity).unwrap(),
    )
    .unwrap();

    assert!(!record.worker_alive());
    record.phase = Phase::Failed;
    assert!(record.worker_finished());
}

#[test]
fn worker_liveness_uses_safe_legacy_fallback_for_missing_or_corrupt_identity() {
    let state = tempfile::tempdir().unwrap();
    let record = liveness_record(state.path());
    assert!(record.worker_alive(), "missing sidecar uses numeric pid");

    fs::write(state.path().join(WORKER_IDENTITY_FILE), b"not-json").unwrap();
    assert!(record.worker_alive(), "corrupt sidecar fails closed");

    let identity = ProcessIdentity {
        pid: record.worker_pid.unwrap() + 1,
        start_time_ticks: 0,
        boot_id: "corrupt".into(),
    };
    fs::write(
        state.path().join(WORKER_IDENTITY_FILE),
        serde_json::to_vec(&identity).unwrap(),
    )
    .unwrap();
    assert!(record.worker_alive(), "pid mismatch fails closed");
}

#[test]
fn legacy_exit_info_remains_a_containment_proof() {
    let state = tempfile::tempdir().unwrap();
    let mut value = serde_json::to_value(liveness_record(state.path())).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("containment_cgroup");
    object.remove("containment_cgroup_identity");
    object.remove("containment_empty");
    object.insert("phase".into(), serde_json::json!("exited"));
    object.insert(
        "exit".into(),
        serde_json::json!({
            "code": 0,
            "signal": null,
            "oom_killed": false,
            "exited_at_ms": 2
        }),
    );
    let terminal: SessionRecord = serde_json::from_value(value.clone()).unwrap();
    assert!(terminal.containment_proven_empty());

    value
        .as_object_mut()
        .unwrap()
        .insert("containment_empty".into(), serde_json::json!(false));
    let explicit_failure: SessionRecord = serde_json::from_value(value.clone()).unwrap();
    assert!(!explicit_failure.containment_proven_empty());

    value.as_object_mut().unwrap().remove("exit");
    value
        .as_object_mut()
        .unwrap()
        .insert("phase".into(), serde_json::json!("failed"));
    let ambiguous: SessionRecord = serde_json::from_value(value).unwrap();
    assert!(!ambiguous.containment_proven_empty());
}

#[test]
fn session_metadata_keeps_only_transcript_roots() {
    let env = BTreeMap::from([
        ("CODEX_HOME".to_string(), "/profiles/codex".to_string()),
        ("API_TOKEN".to_string(), "secret".to_string()),
    ]);
    assert_eq!(
        session_metadata_env(&env),
        BTreeMap::from([("CODEX_HOME".to_string(), "/profiles/codex".to_string())])
    );
}
