use aplexer::{
    atomic_write_json, now_ms, Limits, Paths, Phase, SessionRecord, DEFAULT_STARTUP_TIMEOUT_MS,
    SCHEMA_VERSION,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use uuid::Uuid;

fn test_paths(temp: &TempDir) -> Paths {
    let paths = Paths {
        runtime_root: temp.path().join("runtime"),
        state_root: temp.path().join("state"),
        config_file: temp.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    paths
}

fn stale_running_record(paths: &Paths) -> SessionRecord {
    let id = Uuid::new_v4();
    SessionRecord {
        parent_session: None,
        schema_version: SCHEMA_VERSION,
        id,
        workspace: PathBuf::from("/tmp/doctor-workspace"),
        tag: "stale".into(),
        engine: "shell".into(),
        profile: None,
        command: vec!["sh".into()],
        cwd: PathBuf::from("/tmp"),
        env: BTreeMap::new(),
        env_unset: Vec::new(),
        limits: Limits::default(),
        history_bytes: 1024,
        created_at_ms: 1,
        updated_at_ms: 1,
        last_activity_ms: None,
        last_accessed_ms: None,
        reported_state: None,
        reported_state_at_ms: None,
        phase: Phase::Running,
        worker_pid: None,
        workload_pid: None,
        containment_cgroup: None,
        containment_cgroup_identity: None,
        containment_empty: None,
        socket_path: paths.socket(id),
        history_path: paths.history(id),
        exit: None,
        error: None,
    }
}

/// A stale record with no live worker and no live workload is exactly what
/// `a prune` now reaps, so that is the command doctor must name. It used to
/// print `a kill SESSION` (which exits 1 on this record: "no authoritative
/// containment locator") with `a forget --force` as the fallback, whose
/// "workload processes may survive" warning is not true here either.
#[test]
fn doctor_points_a_reapable_stale_record_at_prune() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let record = stale_running_record(&paths);
    std::fs::create_dir_all(paths.state_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), &record).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_a"))
        .args(["--json", "doctor"])
        .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "doctor should fail for stale records"
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "sessions")
        .unwrap();
    assert_eq!(sessions["ok"], false);
    assert_eq!(sessions["broken_sessions"][0]["id"], record.id.to_string());
    assert_eq!(sessions["broken_sessions"][0]["worker_alive"], false);
    assert_eq!(sessions["broken_sessions"][0]["worker_reachable"], false);
    assert!(sessions["broken_sessions"][0]["rpc_error"]
        .as_str()
        .is_some_and(|error| error.contains("connect")));
    assert_eq!(sessions["broken_sessions"][0]["state"], "broken");
    assert_eq!(
        sessions["broken_sessions"][0]["recovery"]["prune"],
        "a prune"
    );
    assert!(
        sessions["broken_sessions"][0]["recovery"]["kill"].is_null(),
        "doctor still suggests `a kill`, which hard-fails on this record"
    );
    assert!(
        sessions["broken_sessions"][0]["recovery"]["forget"].is_null(),
        "doctor still suggests the --force escape hatch this record does not need"
    );
    assert!(
        sessions["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("a prune")),
        "human advice must name prune too: {}",
        sessions["detail"]
    );

    // And the advice is honest: running it removes the record.
    let pruned = Command::new(env!("CARGO_BIN_EXE_a"))
        .args(["--json", "prune"])
        .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file)
        .output()
        .unwrap();
    assert!(pruned.status.success(), "{pruned:?}");
    let pruned: Value = serde_json::from_slice(&pruned.stdout).unwrap();
    assert_eq!(
        pruned["removed"],
        serde_json::json!([record.id.to_string()]),
        "doctor advised a command that does nothing: {pruned}"
    );
}

/// The counterpart: a broken record whose workload leader is still running
/// is NOT reapable, so the kill/forget advice stays exactly as it was.
#[test]
fn doctor_keeps_kill_and_forget_advice_for_a_record_prune_retains() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let mut record = stale_running_record(&paths);
    // This test process stands in for a workload leader that outlived its
    // worker: prune must retain the record, so doctor must not say `prune`.
    record.workload_pid = Some(std::process::id());
    std::fs::create_dir_all(paths.state_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), &record).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_a"))
        .args(["--json", "doctor"])
        .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "sessions")
        .unwrap();
    let broken = &sessions["broken_sessions"][0];
    assert_eq!(broken["id"], record.id.to_string());
    assert_eq!(broken["recovery"]["kill"], format!("a kill {}", record.id));
    assert_eq!(
        broken["recovery"]["forget"],
        format!("a forget {} --force", record.id)
    );
    assert!(
        broken["recovery"]["prune"].is_null(),
        "doctor advised prune for a record prune will not touch"
    );
    assert!(
        sessions["detail"]
            .as_str()
            .is_some_and(|detail| !detail.contains("a prune")),
        "human advice must not name prune here: {}",
        sessions["detail"]
    );
}

/// The other retention arm of `reap_verdict`: a live worker whose recorded
/// workload leader is already gone. Doctor used to stay green if the
/// `worker_alive()` guard was deleted, because every other fixture that
/// pins kill/forget advice had a live *workload*. Recovery must follow
/// prune: not `a prune`, because prune will not touch this record while
/// the worker is in `/proc`.
#[test]
fn doctor_does_not_advise_prune_for_a_live_worker_whose_leader_is_gone() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let mut record = stale_running_record(&paths);
    let mut worker = Command::new("sleep").arg("30").spawn().unwrap();
    record.worker_pid = Some(worker.id());
    let mut gone = Command::new("sleep").arg("30").spawn().unwrap();
    let gone_pid = gone.id();
    gone.kill().unwrap();
    gone.wait().unwrap();
    record.workload_pid = Some(gone_pid);
    std::fs::create_dir_all(paths.state_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), &record).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_a"))
        .args(["--json", "doctor"])
        .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "sessions")
        .unwrap();
    let broken = &sessions["broken_sessions"][0];
    assert_eq!(broken["id"], record.id.to_string());
    assert_eq!(broken["worker_alive"], true);
    assert_eq!(broken["recovery"]["kill"], format!("a kill {}", record.id));
    assert_eq!(
        broken["recovery"]["forget"],
        format!("a forget {} --force", record.id)
    );
    assert!(
        broken["recovery"]["prune"].is_null(),
        "doctor advised prune for a live worker: {broken}"
    );
    assert!(
        sessions["detail"]
            .as_str()
            .is_some_and(|detail| !detail.contains("a prune")),
        "human advice must not name prune here: {}",
        sessions["detail"]
    );
    assert!(
        worker.try_wait().unwrap().is_none(),
        "doctor must never signal a live worker"
    );

    worker.kill().unwrap();
    worker.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(record.worker_pid.unwrap() as i32, 0) == 0 }
        && Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }

    let output = Command::new(env!("CARGO_BIN_EXE_a"))
        .args(["--json", "doctor"])
        .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "sessions")
        .unwrap();
    let broken = &sessions["broken_sessions"][0];
    assert_eq!(
        broken["recovery"]["prune"], "a prune",
        "a worker that has since died must be advised as reapable: {broken}"
    );
}

fn doctor_sessions_check(paths: &Paths) -> (std::process::Output, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_a"))
        .args(["--json", "doctor"])
        .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "sessions")
        .cloned()
        .unwrap();
    (output, sessions)
}

/// `phase: starting` with no worker pid is what `a start` persists before
/// its worker registers. Inside `--startup-timeout-ms` that is a healthy
/// create, and doctor must not offer it up as a broken record to reclaim --
/// nothing about it is wrong, and the advice would race a session that is
/// about to come up (issue #9).
#[test]
fn doctor_does_not_report_a_session_still_inside_the_startup_window() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let mut record = stale_running_record(&paths);
    record.phase = Phase::Starting;
    record.tag = "creating".into();
    record.created_at_ms = now_ms();
    record.updated_at_ms = record.created_at_ms;
    std::fs::create_dir_all(paths.state_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), &record).unwrap();

    let (output, sessions) = doctor_sessions_check(&paths);
    assert!(
        output.status.success(),
        "doctor treated a mid-create session as broken: {sessions}"
    );
    assert_eq!(sessions["ok"], true, "{sessions}");
    assert!(
        sessions["broken_sessions"]
            .as_array()
            .is_some_and(|rows| rows.is_empty()),
        "doctor listed a mid-create session as broken: {sessions}"
    );
}

/// The same record, differing only in age, is the crashed start `a prune`
/// exists to reap -- so doctor must still report it. This is the other half
/// of the pair: without it, the fix above would just hide broken records.
#[test]
fn doctor_reports_a_starting_record_past_the_startup_budget_as_broken() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let mut record = stale_running_record(&paths);
    record.phase = Phase::Starting;
    record.tag = "stuck".into();
    record.created_at_ms = now_ms().saturating_sub(DEFAULT_STARTUP_TIMEOUT_MS + 1);
    record.updated_at_ms = record.created_at_ms;
    std::fs::create_dir_all(paths.state_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), &record).unwrap();

    let (output, sessions) = doctor_sessions_check(&paths);
    assert!(!output.status.success(), "{sessions}");
    assert_eq!(sessions["ok"], false, "{sessions}");
    assert_eq!(sessions["broken_sessions"][0]["id"], record.id.to_string());
    assert_eq!(sessions["broken_sessions"][0]["state"], "broken");
}

#[test]
fn status_and_doctor_report_alive_but_unreachable_workers_separately() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let mut record = stale_running_record(&paths);
    record.worker_pid = Some(std::process::id());
    std::fs::create_dir_all(paths.state_session(record.id)).unwrap();
    atomic_write_json(&paths.record(record.id), &record).unwrap();

    let base_command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
            .env("APLEXER_STATE_DIR", &paths.state_root)
            .env("APLEXER_CONFIG", &paths.config_file);
        command
    };

    let json_status = base_command()
        .args(["--json", "status", &record.id.to_string()])
        .output()
        .unwrap();
    assert!(json_status.status.success(), "{json_status:?}");
    let status: Value = serde_json::from_slice(&json_status.stdout).unwrap();
    assert_eq!(status["worker_alive"], true);
    assert_eq!(status["worker_reachable"], false);
    assert!(status["rpc_error"]
        .as_str()
        .is_some_and(|error| error.contains("connect")));

    let human_status = base_command()
        .args(["status", &record.id.to_string()])
        .output()
        .unwrap();
    assert!(human_status.status.success(), "{human_status:?}");
    let human = String::from_utf8_lossy(&human_status.stdout);
    assert!(human.contains("worker_alive: true"), "{human}");
    assert!(human.contains("worker_reachable: false"), "{human}");
    assert!(human.contains("rpc_error: "), "{human}");

    let doctor_output = base_command().args(["--json", "doctor"]).output().unwrap();
    assert!(!doctor_output.status.success(), "{doctor_output:?}");
    let report: Value = serde_json::from_slice(&doctor_output.stdout).unwrap();
    let sessions = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "sessions")
        .unwrap();
    let broken = &sessions["broken_sessions"][0];
    assert_eq!(broken["id"], record.id.to_string());
    assert_eq!(broken["worker_alive"], true);
    assert_eq!(broken["worker_reachable"], false);
    assert!(broken["rpc_error"]
        .as_str()
        .is_some_and(|error| error.contains("connect")));
}

#[test]
fn doctor_reports_strict_config_errors_with_field_context() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    std::fs::write(
        &paths.config_file,
        "version = 1\n[profiles.review]\nhistroy_bytes = 1024\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_a"))
        .args(["--json", "doctor"])
        .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file)
        .output()
        .unwrap();

    assert!(!output.status.success(), "doctor hid invalid config");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let config = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "config")
        .unwrap();
    assert_eq!(config["ok"], false);
    let detail = config["detail"].as_str().unwrap();
    assert!(detail.contains("unknown field"), "{detail}");
    assert!(detail.contains("histroy_bytes"), "{detail}");
}

#[test]
fn doctor_reports_corrupt_registry_entry_with_its_path() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let id = Uuid::new_v4();
    std::fs::create_dir_all(paths.state_session(id)).unwrap();
    std::fs::write(paths.record(id), b"{truncated").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_a"))
        .args(["--json", "doctor"])
        .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file)
        .output()
        .unwrap();

    assert!(
        !output.status.success(),
        "doctor hid corrupt registry state"
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let sessions = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "sessions")
        .unwrap();
    let detail = sessions["detail"].as_str().unwrap();
    assert_eq!(sessions["ok"], false);
    assert!(detail.contains(&id.to_string()), "{detail}");
    assert!(detail.contains("session.json"), "{detail}");
    assert!(detail.contains("parse"), "{detail}");
}

#[test]
fn optional_cgroup_capability_is_explicit_and_never_makes_clean_doctor_fatal() {
    let temp = TempDir::new().unwrap();
    let paths = test_paths(&temp);
    let output = Command::new(env!("CARGO_BIN_EXE_a"))
        .args(["--json", "doctor"])
        .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ok"], true);
    let check = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "cgroup_limits")
        .expect("doctor must report optional limit capability");
    assert_eq!(check["required"], false);
    assert!(check["available"].is_boolean());
    assert!(check["prerequisites"]["cgroup_v2"].is_boolean());
    assert!(check["prerequisites"]["controllers"]["ok"].is_boolean());
    assert!(check["prerequisites"]["controllers"]["required"].is_array());
    assert!(check["prerequisites"]["controllers"]["available"].is_array());
    assert!(check["prerequisites"]["delegated_systemd_user_scope"]["ok"].is_boolean());
    assert_eq!(
        check["prerequisites"]["delegated_systemd_user_scope"]["method"],
        "temporary_scope_via_launch_path"
    );
    assert!(check["prerequisites"]["delegated_systemd_user_scope"]["verifies"].is_array());

    if check["available"] == true {
        assert_eq!(check["ok"], true);
        assert_eq!(check["severity"], "ok");
        assert_eq!(report["warnings"], 0);
        assert_eq!(check["prerequisites"]["cgroup_v2"], true);
        assert_eq!(check["prerequisites"]["controllers"]["ok"], true);
        assert_eq!(
            check["prerequisites"]["delegated_systemd_user_scope"]["ok"],
            true
        );
    } else {
        assert_eq!(check["ok"], false);
        assert_eq!(check["severity"], "warning");
        assert!(report["warnings"].as_u64().unwrap() >= 1);
        assert!(check["detail"]
            .as_str()
            .unwrap()
            .contains("unlimited sessions still work"));
    }
}
