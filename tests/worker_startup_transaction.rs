#![cfg(feature = "startup-test-hooks")]

use aplexer::{
    atomic_write_json, now_ms, process_alive, Limits, Paths, Phase, SessionRecord,
    DEFAULT_HISTORY_BYTES,
};
use std::collections::BTreeMap;
use std::fs;
use std::process::Command;
use tempfile::TempDir;
use uuid::Uuid;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    workspace: TempDir,
    paths: Paths,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().expect("runtime tempdir");
        let state = TempDir::new().expect("state tempdir");
        let workspace = TempDir::new().expect("workspace tempdir");
        let paths = Paths {
            runtime_root: runtime.path().to_path_buf(),
            state_root: state.path().to_path_buf(),
            config_file: runtime.path().join("config.toml"),
        };
        paths.ensure().expect("create aplexer paths");
        Self {
            runtime,
            state,
            workspace,
            paths,
        }
    }

    fn starting_record(&self, id: Uuid) -> SessionRecord {
        SessionRecord {
            schema_version: 1,
            id,
            workspace: self.workspace.path().to_path_buf(),
            tag: format!("startup-{id}"),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/sleep".into(), "300".into()],
            cwd: self.workspace.path().to_path_buf(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: DEFAULT_HISTORY_BYTES,
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
            last_activity_ms: None,
            phase: Phase::Starting,
            worker_pid: None,
            workload_pid: None,
            containment_cgroup: None,
            containment_empty: false,
            socket_path: self.paths.socket(id),
            history_path: self.paths.history(id),
            exit: None,
            error: None,
        }
    }

    fn fail_at(&self, point: &str) -> SessionRecord {
        let id = Uuid::new_v4();
        let record = self.starting_record(id);
        atomic_write_json(&self.paths.record(id), &record).expect("write starting record");
        let mut launch_environment = BTreeMap::new();
        launch_environment.insert(
            "APLEXER_TEST_SECRET".to_string(),
            "do-not-retain".to_string(),
        );
        atomic_write_json(
            &self
                .paths
                .runtime_session(id)
                .join("launch-environment.json"),
            &launch_environment,
        )
        .expect("write launch environment");

        let output = Command::new(env!("CARGO_BIN_EXE_aplexer"))
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.paths.config_file)
            .env("APLEXER_TEST_FAIL_WORKER_STARTUP_AT", point)
            .args(["worker", "--id", &id.to_string()])
            .output()
            .expect("run worker");
        assert!(
            !output.status.success(),
            "worker unexpectedly succeeded at {point}"
        );

        let failed: SessionRecord =
            aplexer::read_record(&self.paths.record(id)).expect("read failed record");
        assert_eq!(failed.phase, Phase::Failed);
        assert!(
            failed
                .error
                .as_deref()
                .is_some_and(|error| error.contains(point)),
            "unexpected failure: {:?}",
            failed.error
        );
        assert!(
            !self.paths.runtime_session(id).exists(),
            "runtime transaction leaked at {point}"
        );
        assert!(!self.paths.socket(id).exists(), "socket leaked at {point}");
        failed
    }
}

#[test]
fn post_spawn_and_partial_thread_failures_kill_workload_and_clean_runtime() {
    let harness = Harness::new();
    for point in ["after_workload_spawn", "thread_3"] {
        let failed = harness.fail_at(point);
        let pid = failed
            .workload_pid
            .expect("post-spawn failure records workload pid");
        assert!(!process_alive(pid), "workload {pid} leaked at {point}");
        assert!(
            !fs::read_to_string(harness.paths.record(failed.id))
                .expect("read record text")
                .contains("do-not-retain"),
            "launch secret leaked into record at {point}"
        );
    }
}
