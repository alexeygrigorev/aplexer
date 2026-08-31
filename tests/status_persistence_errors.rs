use aplexer::{
    atomic_write_json, frame_json, read_frame, write_json, Limits, Paths, Phase, Request, Response,
    SessionRecord,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use uuid::Uuid;

fn record(paths: &Paths, workspace: &Path, id: Uuid, tag: &str) -> SessionRecord {
    SessionRecord {
        schema_version: 1,
        id,
        workspace: workspace.to_path_buf(),
        tag: tag.into(),
        engine: "shell".into(),
        profile: None,
        command: vec!["/bin/bash".into(), "--norc".into()],
        cwd: workspace.to_path_buf(),
        env: BTreeMap::new(),
        env_unset: Vec::new(),
        limits: Limits::default(),
        history_bytes: 4096,
        created_at_ms: 1,
        updated_at_ms: 1,
        last_activity_ms: None,
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

fn command(runtime: &TempDir, state: &TempDir, config: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
    command
        .env("APLEXER_RUNTIME_DIR", runtime.path())
        .env("APLEXER_STATE_DIR", state.path())
        .env("APLEXER_CONFIG", config);
    command
}

#[test]
fn status_preserves_both_live_persistence_errors() {
    let runtime = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let paths = Paths {
        runtime_root: runtime.path().to_path_buf(),
        state_root: state.path().to_path_buf(),
        config_file: state.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let id = Uuid::now_v7();
    fs::create_dir_all(paths.runtime_session(id)).unwrap();
    fs::create_dir_all(paths.state_session(id)).unwrap();
    let record = record(&paths, workspace.path(), id, "errors");
    atomic_write_json(&paths.record(id), &record).unwrap();
    let listener = UnixListener::bind(&record.socket_path).unwrap();
    let response_record = record.clone();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let request: Request = frame_json(read_frame(&mut stream).unwrap().unwrap()).unwrap();
            let mut value = serde_json::to_value(&response_record).unwrap();
            value["history_persistence_error"] = json!("history disk unavailable");
            value["record_persistence_error"] = json!("record disk unavailable");
            write_json(&mut stream, &Response::ok(request.request_id, value)).unwrap();
        }
    });

    let json_output = command(&runtime, &state, &paths.config_file)
        .args(["status", &id.to_string(), "--json"])
        .output()
        .unwrap();
    assert!(json_output.status.success(), "{json_output:?}");
    let value: Value = serde_json::from_slice(&json_output.stdout).unwrap();
    assert_eq!(
        value["history_persistence_error"],
        "history disk unavailable"
    );
    assert_eq!(value["record_persistence_error"], "record disk unavailable");

    let human = command(&runtime, &state, &paths.config_file)
        .args(["status", &id.to_string()])
        .output()
        .unwrap();
    assert!(human.status.success(), "{human:?}");
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.contains("history_persistence_error: history disk unavailable"));
    assert!(human.contains("record_persistence_error: record disk unavailable"));
    server.join().unwrap();
}

#[test]
fn live_history_degradation_is_visible_without_stopping_the_workload() {
    let runtime = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let config = runtime.path().join("config.toml");
    let mut start = command(&runtime, &state, &config);
    start.args([
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "degraded",
        "--",
        "/bin/bash",
        "--norc",
    ]);
    let started = start.output().unwrap();
    assert!(started.status.success(), "{started:?}");
    let started_record: Value = serde_json::from_slice(&started.stdout).unwrap();
    let id = started_record["id"].as_str().unwrap().to_string();
    let history = PathBuf::from(started_record["history_path"].as_str().unwrap());
    if let Err(error) = fs::remove_file(&history) {
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }
    fs::create_dir(&history).unwrap();
    for slot in 0..2 {
        let commit = history.with_file_name(format!("history.bin.v2.commit.{slot}"));
        if let Err(error) = fs::remove_file(&commit) {
            assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        }
        fs::create_dir(&commit).unwrap();
    }

    let marker = "still-responsive-after-history-error";
    let sent = command(&runtime, &state, &config)
        .args(["send", &id, &format!("echo {marker}"), "--enter"])
        .output()
        .unwrap();
    assert!(sent.status.success(), "{sent:?}");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = command(&runtime, &state, &config)
            .args(["status", &id, "--json"])
            .output()
            .unwrap();
        let value: Value = serde_json::from_slice(&status.stdout).unwrap();
        if value.get("history_persistence_error").is_some() {
            assert_eq!(value["worker_alive"], true);
            break;
        }
        assert!(Instant::now() < deadline, "degradation was not reported");
        thread::sleep(Duration::from_millis(50));
    }

    let capture: Output = command(&runtime, &state, &config)
        .args(["capture", &id, "--bytes", "4096"])
        .output()
        .unwrap();
    assert!(capture.status.success(), "{capture:?}");
    assert!(String::from_utf8_lossy(&capture.stdout).contains(marker));
    let _ = command(&runtime, &state, &config)
        .args(["kill", &id, "--signal", "KILL", "--grace-ms", "0"])
        .output();
}
