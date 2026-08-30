use aplexer::{atomic_write_json, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
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

#[test]
fn doctor_reports_stale_active_records_with_recovery_commands() {
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
    assert_eq!(
        sessions["broken_sessions"][0]["recovery"]["kill"],
        format!("a kill {}", record.id)
    );
    assert_eq!(
        sessions["broken_sessions"][0]["recovery"]["forget"],
        format!("a forget {} --force", record.id)
    );
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
