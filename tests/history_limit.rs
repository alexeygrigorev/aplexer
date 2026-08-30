use aplexer::api::{start_session, StartRequest};
use aplexer::{Paths, MAX_HISTORY_BYTES};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
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
