use aplexer::api::{start_session, StartRequest};
use aplexer::{atomic_write_json, read_record, Paths, MAX_HISTORY_BYTES};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().expect("runtime tempdir");
        let state = TempDir::new().expect("state tempdir");
        let config = runtime.path().join("config.toml");
        Self {
            runtime,
            state,
            config,
        }
    }

    fn paths(&self) -> Paths {
        let paths = Paths {
            runtime_root: self.runtime.path().to_path_buf(),
            state_root: self.state.path().to_path_buf(),
            config_file: self.config.clone(),
        };
        paths.ensure().unwrap();
        paths
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_a"))
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            .args(args)
            .output()
            .expect("run CLI")
    }

    fn assert_no_session_artifacts(&self) {
        for root in [self.runtime.path(), self.state.path()] {
            let sessions = root.join("sessions");
            let entries = fs::read_dir(&sessions)
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert!(entries.is_empty(), "{} is not empty", sessions.display());
        }
    }
}

#[test]
fn cli_rejects_cap_plus_one_before_worker_spawn() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace");
    let paths = harness.paths();
    let output = Command::new(env!("CARGO_BIN_EXE_a"))
        .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
        .env("APLEXER_STATE_DIR", &paths.state_root)
        .env("APLEXER_CONFIG", &paths.config_file)
        .args([
            "start",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--history-bytes",
            &(MAX_HISTORY_BYTES + 1).to_string(),
            "--",
            "/bin/true",
        ])
        .output()
        .expect("run CLI");

    assert!(!output.status.success(), "oversized CLI launch succeeded");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("history_bytes"), "{error}");
    assert!(error.contains(&MAX_HISTORY_BYTES.to_string()), "{error}");
    harness.assert_no_session_artifacts();
}

#[test]
fn embedded_api_rejects_usize_max_before_worker_spawn() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace");
    let paths = harness.paths();
    let request = StartRequest {
        workspace: workspace.path().to_path_buf(),
        tag: "oversized".into(),
        engine: None,
        profile: None,
        cwd: None,
        env: BTreeMap::new(),
        command: vec!["/bin/true".into()],
        memory: None,
        pids: None,
        cpu_quota_us: None,
        cpu_period_us: 100_000,
        history_bytes: Some(usize::MAX),
        no_skip_permissions: false,
        startup_timeout_ms: 10_000,
        worker_rows: None,
        worker_cols: None,
        python: None,
    };

    let error = start_session(&paths, &request).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("history_bytes"), "{message}");
    assert!(
        message.contains(&MAX_HISTORY_BYTES.to_string()),
        "{message}"
    );
    harness.assert_no_session_artifacts();
}

#[test]
fn terminal_legacy_oversized_record_remains_recoverable() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace");
    let paths = harness.paths();
    let started = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "legacy",
        "--",
        "/bin/sh",
        "-c",
        "printf 'legacy-history\\n'",
    ]);
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let started: Value = serde_json::from_slice(&started.stdout).expect("start JSON");
    let id = started["id"].as_str().expect("session id");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = harness.run(&["snapshot"]);
        assert!(snapshot.status.success());
        let records: Value = serde_json::from_slice(&snapshot.stdout).expect("snapshot JSON");
        if records.as_array().is_some_and(|records| {
            records.iter().any(|record| {
                record["id"] == id && record["phase"] == "exited" && record["worker_alive"] == false
            })
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "session did not become terminal");
        thread::sleep(Duration::from_millis(25));
    }

    let mut record = read_record(&paths.record(id.parse().unwrap())).expect("read record");
    record.history_bytes = MAX_HISTORY_BYTES + 1;
    atomic_write_json(&paths.record(record.id), &record).expect("write legacy record");

    let snapshot = harness.run(&["snapshot"]);
    assert!(
        snapshot.status.success(),
        "legacy record hid registry: {}",
        String::from_utf8_lossy(&snapshot.stderr)
    );
    let status = harness.run(&["status", id, "--json"]);
    assert!(
        status.status.success(),
        "legacy status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let capture = harness.run(&["capture", id]);
    assert!(
        capture.status.success(),
        "legacy capture failed: {}",
        String::from_utf8_lossy(&capture.stderr)
    );
    assert_eq!(capture.stdout, b"legacy-history\r\n");

    let forgotten = harness.run(&["forget", id, "--force"]);
    assert!(
        forgotten.status.success(),
        "legacy forget failed: {}",
        String::from_utf8_lossy(&forgotten.stderr)
    );
    assert!(!paths.state_session(record.id).exists());
}
