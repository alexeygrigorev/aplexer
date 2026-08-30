use aplexer::{atomic_write_json, Limits, Paths, Phase, SessionRecord};
use serde_json::Value;
use std::collections::BTreeMap;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn status_cli_bounds_connect_to_a_saturated_control_backlog() {
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
    std::fs::create_dir_all(paths.runtime_session(id)).unwrap();
    std::fs::create_dir_all(paths.state_session(id)).unwrap();
    let socket_path = paths.socket(id);
    let listener = UnixListener::bind(&socket_path).unwrap();
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
    let _queued = UnixStream::connect(&socket_path).unwrap();

    let record = SessionRecord {
        schema_version: 1,
        id,
        workspace: workspace.path().to_path_buf(),
        tag: "saturated".into(),
        engine: "shell".into(),
        profile: None,
        command: vec!["/bin/sh".into()],
        cwd: workspace.path().to_path_buf(),
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
        socket_path,
        history_path: paths.history(id),
        exit: None,
        error: None,
    };
    atomic_write_json(&paths.record(id), &record).unwrap();

    let started = Instant::now();
    let output = Command::new(env!("CARGO_BIN_EXE_a"))
        .env("APLEXER_RUNTIME_DIR", runtime.path())
        .env("APLEXER_STATE_DIR", state.path())
        .env("APLEXER_CONFIG", &paths.config_file)
        .args(["status", &id.to_string(), "--json"])
        .output()
        .unwrap();
    let elapsed = started.elapsed();

    assert!(output.status.success(), "status failed: {output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["worker_alive"], false);
    assert_eq!(value["worker_reachable"], false);
    assert!(value["rpc_error"]
        .as_str()
        .is_some_and(|error| error.contains("timed out")));
    assert!(elapsed >= Duration::from_millis(2_500), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}
