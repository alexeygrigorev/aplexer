use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
#[cfg(feature = "startup-test-hooks")]
use std::process::Stdio;
#[cfg(feature = "startup-test-hooks")]
use std::thread;
#[cfg(feature = "startup-test-hooks")]
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime_dir: TempDir,
    state_dir: TempDir,
    config_file: PathBuf,
}

#[cfg(feature = "startup-test-hooks")]
struct ProcessGroupCleanup(Option<i32>);

#[cfg(feature = "startup-test-hooks")]
impl ProcessGroupCleanup {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(feature = "startup-test-hooks")]
impl Drop for ProcessGroupCleanup {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
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
        self.command_with_env(args, environment)
            .output()
            .expect("run aplexer CLI")
    }

    fn command_with_env(&self, args: &[&str], environment: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime_dir.path())
            .env("APLEXER_STATE_DIR", self.state_dir.path())
            .env("APLEXER_CONFIG", &self.config_file)
            .envs(environment.iter().copied())
            .args(args);
        command
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

#[cfg(feature = "startup-test-hooks")]
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

#[cfg(feature = "startup-test-hooks")]
fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "{} did not appear", path.display());
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
#[cfg(feature = "startup-test-hooks")]
fn timeout_after_workload_spawn_does_not_orphan_workload() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let marker = harness.runtime_dir.path().join("spawned-workload-pid");
    let marker = marker.to_str().expect("UTF-8 marker path");
    let descendant_marker = harness.runtime_dir.path().join("spawned-descendant-pid");
    let descendant_marker = descendant_marker
        .to_str()
        .expect("UTF-8 descendant marker path");

    let timed_out = harness.run_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "hard-hang",
            "--startup-timeout-ms",
            "1000",
            "--",
            "/bin/sh",
            "-c",
            "sleep 10 & echo $! > \"$1\"; wait",
            "aplexer-startup-test",
            descendant_marker,
        ],
        &[
            (
                "APLEXER_TEST_HANG_WORKER_STARTUP_AT",
                "after_workload_spawn",
            ),
            ("APLEXER_TEST_WORKER_STARTUP_MARKER", marker),
        ],
    );
    let workload_pid: u32 = fs::read_to_string(marker)
        .expect("post-spawn marker was written")
        .trim()
        .parse()
        .expect("marker contains workload PID");
    assert!(!timed_out.status.success(), "paused startup succeeded");
    assert!(
        String::from_utf8_lossy(&timed_out.stderr).contains("within 1000 ms"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&timed_out.stderr)
    );

    let descendant_pid: u32 = fs::read_to_string(descendant_marker)
        .expect("workload descendant marker was written")
        .trim()
        .parse()
        .expect("descendant marker contains PID");
    assert_process_exited(workload_pid);
    assert_process_exited(descendant_pid);
    harness.assert_no_session_artifacts();

    let retry = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace,
        "--tag",
        "hard-hang",
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
        "retry cleanup kill failed: {}",
        String::from_utf8_lossy(&killed.stderr)
    );
}

#[test]
#[cfg(feature = "startup-test-hooks")]
fn externally_reaped_worker_preserves_ambiguous_containment_evidence() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let marker = harness
        .runtime_dir
        .path()
        .join("externally-killed-workload-pid");
    let marker_text = marker.to_str().expect("UTF-8 marker path");
    let mut command = harness.command_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "ambiguous-exit",
            "--startup-timeout-ms",
            "5000",
            "--",
            "/bin/sleep",
            "30",
        ],
        &[
            (
                "APLEXER_TEST_HANG_WORKER_STARTUP_AT",
                "after_workload_spawn",
            ),
            ("APLEXER_TEST_WORKER_STARTUP_MARKER", marker_text),
        ],
    );
    let start = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn start command");
    wait_for_path(&marker);
    let workload_pid: i32 = fs::read_to_string(&marker)
        .expect("read workload marker")
        .trim()
        .parse()
        .expect("workload marker contains PID");
    let mut workload_cleanup = ProcessGroupCleanup(Some(workload_pid));

    let session_dir = fs::read_dir(harness.state_dir.path().join("sessions"))
        .expect("read state sessions")
        .next()
        .expect("startup session directory")
        .expect("read startup session entry")
        .path();
    let record_path = session_dir.join("session.json");
    let record: Value = serde_json::from_slice(
        &fs::read(&record_path).expect("read startup record before killing worker"),
    )
    .expect("parse startup record");
    let worker_pid = record["worker_pid"].as_u64().expect("recorded worker pid") as i32;
    assert_eq!(unsafe { libc::kill(worker_pid, libc::SIGKILL) }, 0);

    let output = start.wait_with_output().expect("wait for failed start");
    assert!(
        !output.status.success(),
        "externally killed start succeeded"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not be confirmed"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        record_path.exists(),
        "ambiguous durable containment evidence was deleted"
    );
    assert!(
        harness
            .runtime_dir
            .path()
            .join("sessions")
            .read_dir()
            .unwrap()
            .next()
            .is_some(),
        "ambiguous runtime containment evidence was deleted"
    );

    let result = unsafe { libc::kill(-workload_pid, libc::SIGKILL) };
    assert!(
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
        "cleanup surviving workload failed: {}",
        std::io::Error::last_os_error()
    );
    assert_process_exited(workload_pid as u32);
    workload_cleanup.disarm();
}
