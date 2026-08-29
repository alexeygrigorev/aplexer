//! Linux descendant-containment regressions.
//!
//! Each test starts a short-lived shell leader which launches a `setsid`
//! descendant that ignores TERM/HUP. The leader exits before `a kill`, so a
//! process-group-only implementation silently leaves the child behind. One
//! descendant closes all PTY descriptors; the other deliberately retains
//! stdout/stderr on the PTY and therefore also delays PTY EOF.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use aplexer::{process_alive, process_start_time_ticks};
use serde_json::Value;
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

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime_dir.path())
            .env("APLEXER_STATE_DIR", self.state_dir.path())
            .env("APLEXER_CONFIG", &self.config_file);
        command
    }

    fn run(&self, args: &[&str], timeout: Duration) -> std::process::Output {
        let mut command = self.command();
        command.args(args);
        run_with_timeout(command, timeout)
    }

    fn run_ok(&self, args: &[&str], timeout: Duration) -> String {
        let output = self.run(args, timeout);
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn status(&self, id: &str) -> Value {
        let stdout = self.run_ok(&["status", id, "--json"], Duration::from_secs(5));
        serde_json::from_str(&stdout).expect("status JSON")
    }
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> std::process::Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("spawn command");
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("wait for command: {error}"),
        Err(_) => {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            panic!("command pid {pid} exceeded timeout {timeout:?}");
        }
    }
}

struct PidGuard {
    pid: u32,
    start_time: u64,
    armed: bool,
}

impl PidGuard {
    fn new(pid: u32) -> Self {
        Self {
            pid,
            start_time: process_start_time_ticks(pid).expect("process start time"),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        if self.armed && process_start_time_ticks(self.pid).ok() == Some(self.start_time) {
            unsafe {
                libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
            }
        }
    }
}

fn wait_for_pid_file(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "pid file was not written: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_until(mut condition: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn run_detached_descendant_case(retain_pty: bool) {
    assert!(Path::new("/usr/bin/setsid").is_file(), "setsid is required");
    let harness = Harness::new();
    let workspace_dir = TempDir::new().expect("workspace tempdir");
    let pid_file = workspace_dir.path().join("descendant.pid");
    let redirections = if retain_pty {
        "</dev/null"
    } else {
        "</dev/null >/dev/null 2>&1"
    };
    let script = format!(
        "/usr/bin/setsid /bin/sh -c 'trap \"\" HUP TERM; echo $$ > \"$1\"; \
         exec /bin/sleep 300' descendant '{}' {redirections} & \
         while [ ! -s '{}' ]; do /bin/sleep 0.01; done",
        pid_file.display(),
        pid_file.display(),
    );
    let workspace = workspace_dir.path().to_str().expect("utf8 workspace");
    let stdout = harness.run_ok(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            if retain_pty {
                "retain-pty"
            } else {
                "close-pty"
            },
            "--json",
            "--",
            "/bin/sh",
            "-c",
            &script,
        ],
        Duration::from_secs(15),
    );
    let started: Value = serde_json::from_str(&stdout).expect("start JSON");
    let id = started["id"].as_str().expect("session id").to_owned();
    let worker_pid = started["worker_pid"].as_u64().expect("worker pid") as u32;
    let mut worker_guard = PidGuard::new(worker_pid);
    let child_pid = wait_for_pid_file(&pid_file);
    let mut child_guard = PidGuard::new(child_pid);

    wait_until(
        || harness.status(&id)["phase"] == Value::String("exiting".into()),
        "leader exit while descendant remains",
    );
    assert!(
        process_alive(child_pid),
        "detached descendant exited unexpectedly"
    );
    assert!(
        process_alive(worker_pid),
        "worker finalized before its descendant"
    );
    let stdout_target =
        fs::read_link(format!("/proc/{child_pid}/fd/1")).expect("read descendant stdout target");
    if retain_pty {
        assert!(
            stdout_target.to_string_lossy().contains("/dev/pts/"),
            "descendant should retain the PTY, got {}",
            stdout_target.display(),
        );
    } else {
        assert_eq!(stdout_target, Path::new("/dev/null"));
    }

    harness.run_ok(
        &["kill", &id, "--signal", "TERM", "--grace-ms", "50"],
        Duration::from_secs(8),
    );
    wait_until(
        || !process_alive(child_pid),
        "detached descendant termination",
    );
    child_guard.disarm();
    wait_until(
        || {
            let status = harness.status(&id);
            status["phase"] == Value::String("exited".into())
                && status["worker_alive"] == Value::Bool(false)
        },
        "worker finalization after descendants drain",
    );
    worker_guard.disarm();
}

#[test]
fn kill_contains_detached_descendant_after_leader_exit() {
    run_detached_descendant_case(false);
}

#[test]
fn kill_contains_detached_descendant_that_retains_pty() {
    run_detached_descendant_case(true);
}
