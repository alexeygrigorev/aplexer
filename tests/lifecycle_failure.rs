#![cfg(feature = "startup-test-hooks")]

//! Product-path regression for a failed workload waiter. The worker must
//! empty its whole subreaper domain before it exits, including a detached
//! descendant which ignores the graceful signals normally used by shells.

use aplexer::{process_alive, process_start_time_ticks, read_record, Phase};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
        let config = state.path().join("config.toml");
        Self {
            runtime,
            state,
            config,
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
}

struct PidGuard {
    pid: u32,
    start_time: u64,
}

impl PidGuard {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            start_time: process_start_time_ticks(pid).expect("process start time"),
        }
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        if process_start_time_ticks(self.pid).ok() == Some(self.start_time) {
            unsafe {
                libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

fn wait_for_pid_file(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "{} was not written",
            path.display()
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn waiter_failure_kills_detached_descendant_before_worker_exits() {
    assert!(Path::new("/usr/bin/setsid").is_file(), "setsid is required");
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let marker = harness.runtime.path().join("detached-descendant.pid");
    let script = "/bin/sleep 1; /usr/bin/setsid /bin/sh -c \
        'trap \"\" HUP TERM; echo $$ > \"$1\"; exec /bin/sleep 300' \
        detached \"$1\" & wait";
    let output = harness
        .command()
        .env("APLEXER_TEST_FAIL_WAITER_AFTER_FILE", &marker)
        .args([
            "--json",
            "start",
            "--workspace",
            workspace.path().to_str().expect("UTF-8 workspace"),
            "--tag",
            "waiter-failure",
            "--",
            "/bin/sh",
            "-c",
            script,
            "aplexer-leader",
            marker.to_str().expect("UTF-8 marker"),
        ])
        .output()
        .expect("run a start");
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let started: Value = serde_json::from_slice(&output.stdout).expect("start JSON");
    let id = started["id"].as_str().expect("session id");
    let worker_pid = started["worker_pid"].as_u64().expect("worker pid") as u32;
    let _worker_guard = PidGuard::new(worker_pid);
    let descendant_pid = wait_for_pid_file(&marker);
    let _descendant_guard = PidGuard::new(descendant_pid);

    let record_path = harness
        .state
        .path()
        .join("sessions")
        .join(id)
        .join("session.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    let failed = loop {
        let record = read_record(&record_path).expect("read session record");
        if record.phase == Phase::Failed
            && record.containment_empty == Some(true)
            && !record.worker_alive()
        {
            break record;
        }
        assert!(
            Instant::now() < deadline,
            "worker did not finalize: {record:?}"
        );
        thread::sleep(Duration::from_millis(25));
    };

    assert!(
        failed
            .error
            .as_deref()
            .is_some_and(|error| error.contains("injected workload waiter failure")),
        "unexpected lifecycle error: {:?}",
        failed.error
    );
    assert!(
        !process_alive(descendant_pid),
        "detached descendant survived waiter failure cleanup"
    );
}
