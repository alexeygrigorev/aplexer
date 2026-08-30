use aplexer::{atomic_write_json, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;
use uuid::Uuid;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
    workspace: TempDir,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let config = state.path().join("config.toml");
        let workspace = TempDir::new().unwrap();
        Self {
            runtime,
            state,
            config,
            workspace,
        }
    }

    fn paths(&self) -> Paths {
        Paths {
            runtime_root: self.runtime.path().to_path_buf(),
            state_root: self.state.path().to_path_buf(),
            config_file: self.config.clone(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config);
        command
    }

    fn record(&self, phase: Phase, worker_pid: Option<u32>, history: &[u8]) -> SessionRecord {
        let paths = self.paths();
        paths.ensure().unwrap();
        let id = Uuid::now_v7();
        let record = SessionRecord {
            schema_version: SCHEMA_VERSION,
            id,
            workspace: self.workspace.path().to_path_buf(),
            tag: format!("capture-{id}"),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/sh".into()],
            cwd: self.workspace.path().to_path_buf(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: 4096,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_activity_ms: None,
            phase,
            worker_pid,
            workload_pid: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: None,
            socket_path: paths.socket(id),
            history_path: paths.history(id),
            exit: None,
            error: None,
        };
        std::fs::create_dir_all(paths.state_session(id)).unwrap();
        std::fs::write(&record.history_path, history).unwrap();
        atomic_write_json(&paths.record(id), &record).unwrap();
        record
    }

    fn capture(&self, record: &SessionRecord) -> std::process::Output {
        self.command()
            .args(["capture", &record.id.to_string()])
            .output()
            .unwrap()
    }

    fn capture_json(&self, record: &SessionRecord) -> std::process::Output {
        self.command()
            .args(["--json", "capture", &record.id.to_string()])
            .output()
            .unwrap()
    }
}

#[test]
fn live_worker_rpc_failure_does_not_return_stale_history() {
    let harness = Harness::new();
    let record = harness.record(
        Phase::Running,
        Some(std::process::id()),
        b"stale-history-must-not-escape",
    );

    let output = harness.capture(&record);
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing to return potentially stale persisted history"),
        "{stderr}"
    );
    assert!(stderr.contains("connect"), "{stderr}");
}

#[test]
fn persisted_history_remains_available_for_terminal_and_dead_workers() {
    let harness = Harness::new();
    for (phase, label) in [(Phase::Exited, "terminal"), (Phase::Running, "dead")] {
        let history = format!("{label}-history");
        let record = harness.record(phase, None, history.as_bytes());
        let output = harness.capture(&record);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, history.as_bytes());
    }
}

#[test]
fn json_capture_base64_preserves_arbitrary_bytes_without_lossy_utf8() {
    let harness = Harness::new();
    let record = harness.record(Phase::Exited, None, &[0x00, 0xff, b'A', 0x80]);

    let output = harness.capture_json(&record);
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["id"], record.id.to_string());
    assert_eq!(value["bytes"], 4);
    assert_eq!(value["encoding"], "base64");
    assert_eq!(value["data"], "AP9BgA==");
    assert!(value.get("utf8").is_none(), "{value}");
}

#[test]
fn json_capture_keeps_exact_utf8_as_an_optional_convenience() {
    let harness = Harness::new();
    let record = harness.record(Phase::Exited, None, "hello, 世界\n".as_bytes());

    let output = harness.capture_json(&record);
    assert!(output.status.success(), "{output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["encoding"], "base64");
    assert_eq!(value["data"], "aGVsbG8sIOS4lueVjAo=");
    assert_eq!(value["utf8"], "hello, 世界\n");
}
