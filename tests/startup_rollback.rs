use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime_dir: TempDir,
    state_dir: TempDir,
    config_file: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime_dir = TempDir::new().expect("runtime tempdir");
        let state_dir = TempDir::new().expect("state tempdir");
        let config_file = runtime_dir.path().join("config.toml");
        Self {
            runtime_dir,
            state_dir,
            config_file,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime_dir.path())
            .env("APLEXER_STATE_DIR", self.state_dir.path())
            .env("APLEXER_CONFIG", &self.config_file)
            .envs(environment.iter().copied())
            .args(args);
        command.output().expect("run aplexer CLI")
    }

    fn assert_no_session_artifacts(&self) {
        assert_directory_empty(&self.state_dir.path().join("sessions"));
        assert_directory_empty(&self.runtime_dir.path().join("sessions"));
    }
}

fn assert_directory_empty(path: &Path) {
    let entries = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(entries.is_empty(), "{} is not empty", path.display());
}

fn assert_process_exited(pid: u32) {
    let process_path = PathBuf::from(format!("/proc/{pid}"));
    let deadline = Instant::now() + Duration::from_secs(1);
    while process_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_path.exists(),
        "workload process {pid} survived startup rollback"
    );
}

#[test]
fn zero_timeout_rolls_back_and_same_tag_can_retry() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");

    let timed_out = harness.run(&[
        "start",
        "--workspace",
        workspace,
        "--tag",
        "retry",
        "--startup-timeout-ms",
        "0",
        "--",
        "/bin/sleep",
        "30",
    ]);
    assert!(!timed_out.status.success(), "zero-timeout start succeeded");
    assert!(
        String::from_utf8_lossy(&timed_out.stderr).contains("within 0 ms"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&timed_out.stderr)
    );
    harness.assert_no_session_artifacts();

    let retry = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace,
        "--tag",
        "retry",
        "--",
        "/bin/sleep",
        "30",
    ]);
    assert!(
        retry.status.success(),
        "same-tag retry failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let record: Value = serde_json::from_slice(&retry.stdout).expect("retry JSON record");
    let id = record["id"].as_str().expect("retry session id");

    let killed = harness.run(&["kill", id, "--signal", "KILL", "--grace-ms", "0"]);
    assert!(
        killed.status.success(),
        "cleanup kill failed: {}",
        String::from_utf8_lossy(&killed.stderr)
    );
}

#[test]
fn timeout_after_workload_spawn_does_not_orphan_workload() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let marker = harness.runtime_dir.path().join("spawned-workload-pid");
    let marker = marker.to_str().expect("UTF-8 marker path");

    let timed_out = harness.run_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "post-spawn-timeout",
            "--startup-timeout-ms",
            "3000",
            "--",
            "/bin/sleep",
            "30",
        ],
        &[
            (
                "APLEXER_TEST_PAUSE_WORKER_STARTUP_AT",
                "after_workload_spawn",
            ),
            ("APLEXER_TEST_WORKER_STARTUP_MARKER", marker),
        ],
    );
    assert!(!timed_out.status.success(), "paused startup succeeded");
    assert!(
        String::from_utf8_lossy(&timed_out.stderr).contains("within 3000 ms"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&timed_out.stderr)
    );

    let workload_pid: u32 = fs::read_to_string(marker)
        .expect("post-spawn marker was written")
        .parse()
        .expect("marker contains workload PID");
    assert_process_exited(workload_pid);
    harness.assert_no_session_artifacts();
}
