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
    assert_eq!(
        sessions["broken_sessions"][0]["recovery"]["kill"],
        format!("a kill {}", record.id)
    );
    assert_eq!(
        sessions["broken_sessions"][0]["recovery"]["forget"],
        format!("a forget {} --force", record.id)
    );
}
