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

/// How long a poll for something that must eventually become true is allowed
/// to run before the test gives up.
///
/// Neither loop below asserts anything about *when* its condition holds, only
/// that it does: both exit the instant it is true, so a generous budget cannot
/// turn a real failure into a pass -- a pid file the workload never writes
/// never appears, and a worker that never finalizes never reaches
/// `Failed`+`containment_empty`+exited. The budget only decides how long a
/// genuinely broken run takes to report itself, so it is sized for a saturated
/// CI runner rather than an idle laptop. The 6s and 10s budgets it replaces
/// were a lottery on a loaded box for no gain in strictness.
const LIVENESS_BACKSTOP: Duration = Duration::from_secs(60);

fn wait_for_pid_file(path: &Path) -> u32 {
    let deadline = Instant::now() + LIVENESS_BACKSTOP;
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

/// `process_alive` is `kill(pid, 0)`, which still reports a process that has
/// exited but not yet been reaped by its new parent. The worker empties its
/// domain before exiting, so the descendant is dead by the time the record says
/// so -- but "dead" and "gone from the process table" are two different
/// instants, and only the second one `kill(pid, 0)` can see. Poll rather than
/// assert at a point: a descendant that genuinely survived stays alive forever,
/// so this cannot pass by waiting.
fn assert_process_gone(pid: u32, what: &str) {
    let deadline = Instant::now() + LIVENESS_BACKSTOP;
    while process_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(!process_alive(pid), "{what}");
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
    let deadline = Instant::now() + LIVENESS_BACKSTOP;
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
    assert_process_gone(
        descendant_pid,
        "detached descendant survived waiter failure cleanup",
    );
}
