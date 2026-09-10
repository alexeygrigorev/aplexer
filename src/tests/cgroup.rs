//! Unit tests for cgroup-v2 containment.

use super::*;

/// The membership half of the real probe, without needing a real
/// cgroup: `cgroup.events` says `populated 1` while tasks remain, and a
/// collected cgroup loses the file entirely (ENOENT means empty).
#[test]
fn cgroup_path_populated_reads_the_kernel_counter_and_treats_enoent_as_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(
        !cgroup_path_populated(dir.path()).unwrap(),
        "a collected cgroup (no cgroup.events) is empty, not an error"
    );
    fs::write(dir.path().join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
    assert!(cgroup_path_populated(dir.path()).unwrap());
    fs::write(dir.path().join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
    assert!(!cgroup_path_populated(dir.path()).unwrap());
    fs::write(dir.path().join("cgroup.events"), "frozen 0\n").unwrap();
    assert!(
        cgroup_path_populated(dir.path()).is_err(),
        "a cgroup.events with no populated key must fail closed, not read as empty"
    );
}

#[test]
fn cgroup_counter_read_errors_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let events = dir.path().join("cgroup.events");

    assert!(read_counter(&events, "populated").is_err());
    fs::write(&events, "frozen 0\n").unwrap();
    assert!(read_counter(&events, "populated").is_err());
    fs::write(&events, "populated nope\n").unwrap();
    assert!(read_counter(&events, "populated").is_err());
    fs::write(&events, "populated 1\n").unwrap();
    assert_eq!(read_counter(&events, "populated").unwrap(), 1);
}

#[test]
fn recorded_cgroup_cleanup_checks_deadline_before_locator_io() {
    let id = Uuid::new_v4();
    let locator = PathBuf::from(format!("/sys/fs/cgroup/aplexer-workload-{id}.scope"));
    let error = cleanup_recorded_cgroup_until(
        id,
        &locator,
        None,
        libc::SIGKILL,
        Duration::ZERO,
        Instant::now(),
    )
    .expect_err("expired cleanup must stop before locator inspection");
    assert!(error.to_string().contains("timed out validating"));
}

/// A member that ignores the graceful signal must be SIGKILLed once the
/// grace period ends. The grace wait used to report its own expiry as
/// "timed out waiting for recorded cgroup grace", so `a kill` with any
/// non-zero grace failed on a stubborn member instead of escalating.
/// `#[ignore]`d like every test that needs a real delegated cgroup.
#[test]
#[ignore = "needs cgroup-v2 delegation to the running user; run explicitly"]
fn recorded_cgroup_cleanup_escalates_to_kill_after_grace() {
    let id = Uuid::new_v4();
    let mut cgroup =
        DelegatedCgroup::create(id).expect("this environment has no writable cgroup-v2 parent");
    cgroup.populate_ignoring(Some(libc::SIGTERM));
    let identity = current_cgroup_identity().unwrap();
    let grace = Duration::from_millis(100);

    let started = Instant::now();
    cleanup_recorded_cgroup(id, &cgroup.path, Some(&identity), libc::SIGTERM, grace)
        .expect("grace expiry must escalate to SIGKILL, not fail");
    assert!(started.elapsed() >= grace, "grace must be honoured first");
    assert!(
        !cgroup_path_populated(&cgroup.path).unwrap(),
        "the SIGTERM-ignoring member must be gone after escalation"
    );
}

#[test]
fn recorded_cgroup_cleanup_rejects_untrusted_locator() {
    let id = Uuid::new_v4();
    let identity = current_cgroup_identity().unwrap();
    let error = cleanup_recorded_cgroup_until(
        id,
        Path::new("/tmp/not-a-cgroup"),
        Some(&identity),
        libc::SIGKILL,
        Duration::ZERO,
        Instant::now() + Duration::from_secs(1),
    )
    .expect_err("untrusted locator must fail closed");
    assert!(error
        .to_string()
        .contains("untrusted recorded cgroup locator"));
}

#[test]
fn cgroup_identity_captures_current_v2_kernel_domain() {
    let identity = current_cgroup_identity().unwrap();
    assert_eq!(identity.boot_id, linux_boot_id().unwrap());
    assert_ne!(identity.cgroup_namespace_inode, 0);
    assert_ne!(identity.mount_namespace_inode, 0);
    assert_ne!(identity.cgroup_mount_id, 0);
    assert_ne!(identity.cgroup_root_inode, 0);
    ensure_cgroup2_filesystem(Path::new(CGROUP_V2_ROOT)).unwrap();
}

#[test]
fn live_cgroup_disappearance_is_empty_only_in_matching_domain() {
    let identity = current_cgroup_identity().unwrap();
    let missing_path =
        Path::new(CGROUP_V2_ROOT).join(format!("aplexer-workload-{}.scope", Uuid::new_v4()));
    assert!(!missing_path.exists());
    let collected = Cgroup {
        path: missing_path,
        identity: identity.clone(),
        anchor: Arc::new(Mutex::new(None)),
        initial_oom_kill: 0,
    };
    assert!(!collected.populated().unwrap());

    assert!(!live_cgroup_populated_with(&identity, || {
        Err(io::Error::from(io::ErrorKind::NotFound).into())
    })
    .unwrap());

    let mut wrong_mount = identity.clone();
    wrong_mount.cgroup_mount_id ^= 1;
    let mismatch = live_cgroup_populated_with(&wrong_mount, || {
        panic!("membership must not be read in a mismatched kernel domain")
    })
    .expect_err("mismatched identity must fail closed");
    assert!(mismatch.to_string().contains("before reading membership"));

    let malformed =
        live_cgroup_populated_with(&identity, || Err(anyhow!("malformed cgroup.events")))
            .expect_err("non-ENOENT membership errors must fail closed");
    assert!(malformed
        .to_string()
        .contains("read live cgroup membership"));
}

#[test]
fn control_group_locator_is_uuid_bound_and_cannot_escape_root() {
    let id = Uuid::new_v4();
    let valid = format!("/user.slice/user-1000.slice/aplexer-workload-{id}.scope");
    assert_eq!(
        control_group_locator(id, &valid).unwrap(),
        Path::new(CGROUP_V2_ROOT).join(valid.trim_start_matches('/'))
    );
    assert!(
        control_group_locator(id, &format!("/user.slice/../aplexer-workload-{id}.scope")).is_err()
    );
    assert!(control_group_locator(id, "relative.scope").is_err());
    assert!(control_group_locator(
        id,
        &format!("/user.slice/aplexer-workload-{}.scope", Uuid::new_v4())
    )
    .is_err());
}

#[test]
fn scope_wait_retries_empty_control_group_then_accepts_valid_path() {
    let dir = tempfile::tempdir().unwrap();
    let systemctl = dir.path().join("systemctl");
    let id = Uuid::new_v4();
    let unit = format!("aplexer-workload-{id}");
    let reported = format!("/user.slice/{unit}.scope");
    fs::write(
        &systemctl,
        format!(
            "#!/bin/sh\nif [ ! -e \"$0.seen\" ]; then : > \"$0.seen\"; printf '\\n'; else printf '%s\\n' '{}'; fi\n",
            reported
        ),
    )
    .unwrap();
    fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();

    let mut validations = 0;
    let path = wait_for_scope_cgroup_with(
        id,
        &unit,
        &systemctl,
        "--user",
        Duration::from_secs(1),
        |path| {
            validations += 1;
            Ok(Some(path.to_path_buf()))
        },
    )
    .unwrap();

    assert_eq!(
        path,
        Path::new(CGROUP_V2_ROOT).join(reported.trim_start_matches('/'))
    );
    assert_eq!(validations, 1, "empty value must not reach validation");
    assert!(systemctl.with_extension("seen").exists());
}

#[test]
fn system_helpers_resolve_without_ambient_path() {
    for helper in ["systemd-run", "systemctl", "sleep"] {
        let path = trusted_system_helper(helper).unwrap();
        assert!(path.is_absolute());
        assert_eq!(fs::metadata(path).unwrap().uid(), 0);
    }

    let dir = tempfile::tempdir().unwrap();
    let shadow = dir.path().join("systemctl");
    fs::write(&shadow, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&shadow, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(validate_trusted_helper(&shadow).is_err());
}

#[test]
fn missing_cgroup_leaf_requires_matching_persisted_identity() {
    let id = Uuid::new_v4();
    let locator = PathBuf::from(format!("{CGROUP_V2_ROOT}/aplexer-workload-{id}.scope"));
    let missing = validate_recorded_cgroup(id, &locator, None)
        .expect_err("legacy locator must not prove emptiness");
    assert!(missing
        .to_string()
        .contains("no boot/namespace/mount identity"));

    let mut wrong_boot = current_cgroup_identity().unwrap();
    wrong_boot.boot_id = Uuid::new_v4().to_string();
    let mismatch = validate_recorded_cgroup(id, &locator, Some(&wrong_boot))
        .expect_err("cross-boot locator must not prove emptiness");
    assert!(mismatch.to_string().contains("does not match"));

    let mut wrong_mount = current_cgroup_identity().unwrap();
    wrong_mount.cgroup_mount_id = wrong_mount.cgroup_mount_id.saturating_add(1);
    let mismatch = validate_recorded_cgroup(id, &locator, Some(&wrong_mount))
        .expect_err("replacement mount must not prove emptiness");
    assert!(mismatch.to_string().contains("does not match"));

    let identity = current_cgroup_identity().unwrap();
    assert_eq!(
        validate_recorded_cgroup(id, &locator, Some(&identity)).unwrap(),
        None,
        "same-domain missing cgroup is empty"
    );
}

#[test]
fn cgroup_recovery_pidfds_preserve_descriptor_reserve() {
    assert_eq!(
        cgroup_recovery_pidfd_capacity_from_counts(CGROUP_RECOVERY_FD_RESERVE, 0),
        0
    );
    assert_eq!(
        cgroup_recovery_pidfd_capacity_from_counts(CGROUP_RECOVERY_FD_RESERVE + 7, 3),
        4
    );
    assert_eq!(
        cgroup_recovery_pidfd_capacity_from_counts(u64::MAX, 0),
        MAX_CGROUP_RECOVERY_MEMBERS
    );
}

#[test]
fn cgroup_member_fallback_uses_identity_pinned_signal() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("cgroup.procs"),
        format!("{}\n", std::process::id()),
    )
    .unwrap();
    signal_cgroup_path_until(dir.path(), 0, Instant::now() + Duration::from_secs(1))
        .expect("pidfd signal-zero probe");
}

#[test]
fn cgroup_setup_helper_obeys_wall_clock_deadline() {
    let mut command = Command::new("/bin/sleep");
    command.arg("30");
    let started = Instant::now();
    let error = command_output_until(
        &mut command,
        Instant::now() + Duration::from_millis(50),
        "exercise setup timeout",
    )
    .expect_err("wedged setup helper must time out");
    assert!(error.to_string().contains("timed out waiting"));
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn cgroup_setup_helper_collects_bounded_output() {
    let mut command = Command::new("/bin/printf");
    command.arg("/user.slice/example.scope\n");
    let output = command_output_until(
        &mut command,
        Instant::now() + Duration::from_secs(1),
        "exercise setup output",
    )
    .expect("short-lived setup helper");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"/user.slice/example.scope\n");
}

#[test]
fn cgroup_setup_helper_pipe_cannot_outlive_deadline() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 0.2 & exit 0"]);
    let started = Instant::now();
    let error = command_output_until(
        &mut command,
        Instant::now() + Duration::from_millis(50),
        "exercise inherited output pipe",
    )
    .expect_err("inherited helper pipe must not defeat deadline");
    assert!(error.to_string().contains("timed out waiting"));
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[test]
fn cgroup_anchor_release_owns_child_through_kill_and_reap() {
    let anchor = Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let anchor_pid = anchor.id();
    let cgroup = Cgroup {
        path: PathBuf::from("/does/not/exist"),
        identity: current_cgroup_identity().unwrap(),
        anchor: Arc::new(Mutex::new(Some(anchor))),
        initial_oom_kill: 0,
    };
    let clone = cgroup.clone();

    cgroup.release_anchor().unwrap();
    assert!(cgroup.anchor.lock().unwrap().is_none());
    clone.release_anchor().unwrap();

    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(anchor_pid as libc::pid_t, &mut status, libc::WNOHANG) },
        -1,
        "anchor must already be reaped exactly once"
    );
    assert_eq!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn cgroup_anchor_release_retains_handle_when_reaping_fails() {
    let mut slot = Some(7_u8);
    let error = release_anchor_slot(&mut slot, |_| bail!("injected release failure"))
        .expect_err("release must fail");
    assert!(error.to_string().contains("injected release failure"));
    assert_eq!(slot, Some(7), "failed release must preserve ownership");
}
