use aplexer::api::{start_session, StartRequest};
use aplexer::{
    atomic_write_json, read_record, ExitInfo, Limits, Paths, Phase, SessionRecord,
    MAX_HISTORY_BYTES, SCHEMA_VERSION,
};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use tempfile::TempDir;
use uuid::Uuid;

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
        fresh: false,
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
    paths.ensure().unwrap();
    let id = Uuid::now_v7();
    let workspace = workspace
        .path()
        .canonicalize()
        .unwrap_or_else(|_| workspace.path().to_path_buf());
    let planted = SessionRecord {
        parent_session: None,
        schema_version: SCHEMA_VERSION,
        id,
        workspace,
        tag: "legacy".into(),
        engine: "shell".into(),
        profile: None,
        command: vec!["/bin/sh".into()],
        cwd: paths.state_root.clone(),
        env: BTreeMap::new(),
        env_unset: Vec::new(),
        limits: Limits::default(),
        history_bytes: 4096,
        created_at_ms: 1,
        updated_at_ms: 1,
        last_activity_ms: None,
        last_accessed_ms: None,
        reported_state: None,
        reported_state_at_ms: None,
        phase: Phase::Exited,
        worker_pid: None,
        workload_pid: None,
        containment_cgroup: None,
        containment_cgroup_identity: None,
        containment_empty: Some(true),
        socket_path: paths.socket(id),
        history_path: paths.history(id),
        exit: Some(ExitInfo {
            code: Some(0),
            signal: None,
            oom_killed: false,
            exited_at_ms: 1,
        }),
        error: None,
    };
    fs::create_dir_all(paths.state_session(id)).unwrap();
    fs::write(&planted.history_path, b"legacy-history\r\n").unwrap();
    atomic_write_json(&paths.record(id), &planted).unwrap();

    let mut record = read_record(&paths.record(id)).expect("read record");
    record.history_bytes = MAX_HISTORY_BYTES + 1;
    atomic_write_json(&paths.record(record.id), &record).expect("write legacy record");

    let snapshot = harness.run(&["snapshot"]);
    assert!(
        snapshot.status.success(),
        "legacy record hid registry: {}",
        String::from_utf8_lossy(&snapshot.stderr)
    );
    let id = id.to_string();
    let status = harness.run(&["status", &id, "--json"]);
    assert!(
        status.status.success(),
        "legacy status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let capture = harness.run(&["capture", &id]);
    assert!(
        capture.status.success(),
        "legacy capture failed: {}",
        String::from_utf8_lossy(&capture.stderr)
    );
    assert_eq!(capture.stdout, b"legacy-history\r\n");

    let forgotten = harness.run(&["forget", &id, "--force"]);
    assert!(
        forgotten.status.success(),
        "legacy forget failed: {}",
        String::from_utf8_lossy(&forgotten.stderr)
    );
    assert!(!paths.state_session(record.id).exists());
}
