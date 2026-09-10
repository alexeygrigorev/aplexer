//! `api::kill_session` with a grace window longer than the ordinary control
//! deadline. The worker holds the `Kill` response until the kill has run to
//! completion (the grace window, then the SIGKILL sweep), so a
//! TERM-trapping workload with `grace_ms = 5000` answers after more than
//! 5 s. The client used to read that reply under the 3 s control deadline,
//! time out, and report an error for a kill the worker went on to finish.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use aplexer::Paths;
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

    fn paths(&self) -> Paths {
        Paths {
            runtime_root: self.runtime_dir.path().to_path_buf(),
            state_root: self.state_dir.path().to_path_buf(),
            config_file: self.config_file.clone(),
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

    fn start(&self, tag: &str, script: &str) -> String {
        let workspace = self.runtime_dir.path().join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut command = self.command();
        command.args([
            "--json",
            "start",
            "--workspace",
            workspace.to_str().unwrap(),
            "--tag",
            tag,
            "--",
            "/bin/sh",
            "-c",
            script,
        ]);
        let output = run_with_timeout(command, Duration::from_secs(15));
        assert!(
            output.status.success(),
            "`a start` failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let value: Value = serde_json::from_slice(&output.stdout).expect("start JSON");
        value["id"].as_str().expect("session id").to_string()
    }

    fn state_session(&self, id: &str) -> PathBuf {
        self.state_dir.path().join("sessions").join(id)
    }

    fn wait_until_record_gone(&self, id: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !self.state_session(id).exists() {
                return true;
            }
            thread::sleep(Duration::from_millis(25));
        }
        false
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

/// The workload survives TERM for the whole grace window: the loop
/// replaces every `sleep` the group signal kills, so the containment
/// domain stays populated until the SIGKILL escalation.
const TERM_PROOF_WORKLOAD: &str = "trap \"\" TERM; while :; do /bin/sleep 1; done";

#[test]
fn kill_with_a_long_grace_window_waits_for_the_worker_to_finish() {
    let harness = Harness::new();
    let id = harness.start("stubborn", TERM_PROOF_WORKLOAD);
    let grace_ms = 5_000;

    let started = Instant::now();
    aplexer::api::kill_session(&harness.paths(), &id, libc::SIGTERM, grace_ms)
        .expect("a kill whose grace outlasts the control deadline must still be answered");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(grace_ms),
        "the worker answered before the grace window ran out ({elapsed:?}); the workload did not hold it open"
    );

    assert!(
        harness.wait_until_record_gone(&id, Duration::from_secs(10)),
        "record survived a kill that had to escalate to SIGKILL"
    );
}
