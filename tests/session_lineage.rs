//! Session lineage: a session started from inside another session records
//! its parent.
//!
//! `a start` (and every path that funnels into it) runs with the caller's
//! environment, and a caller inside an aplexer session carries that
//! session's `APLEXER_SESSION_ID` stamp (which the worker also inherits and
//! then overrides for its own workload). When that stamp names a record
//! that still exists, the new record keeps `parent_session` so lineage
//! survives in `a list`/`a status`/JSON; anything else (no stamp, stale
//! stamp) must never fail or fabricate a start.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

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

    /// `a start` as if the caller lived inside `inside`: the aplexer stamp
    /// is exactly what a session's environment carries.
    fn start_inside(&self, inside: Option<&str>, tag: &str) -> String {
        let mut command = self.command();
        command.args([
            "--json",
            "start",
            "--workspace",
            self.workspace().to_str().unwrap(),
            "--tag",
            tag,
            "--",
            "/bin/sh",
            "-c",
            "sleep 60",
        ]);
        if let Some(parent) = inside {
            command.env("APLEXER_SESSION_ID", parent);
        }
        let output = run_with_timeout(command, Duration::from_secs(15));
        assert!(
            output.status.success(),
            "`a start` (inside {inside:?}) failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        let value: Value =
            serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("start JSON");
        value["id"].as_str().expect("session id").to_string()
    }

    fn record(&self, id: &str) -> Value {
        let stdout = self.run_ok(&["status", id, "--json"], Duration::from_secs(5));
        serde_json::from_str(&stdout).expect("status JSON")
    }

    fn workspace(&self) -> PathBuf {
        self.runtime_dir.path().join("ws")
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

#[test]
fn start_records_existing_parent_and_chains_lineage() {
    let harness = Harness::new();
    let workspace = harness.workspace();
    std::fs::create_dir_all(&workspace).unwrap();

    let parent = harness.start_inside(None, "parent");
    let child = harness.start_inside(Some(&parent), "child");
    let grandchild = harness.start_inside(Some(&child), "grandchild");

    let child_record = harness.record(&child);
    assert_eq!(
        child_record["parent_session"].as_str(),
        Some(parent.as_str()),
        "child must record the session it was started from"
    );
    let grandchild_record = harness.record(&grandchild);
    assert_eq!(
        grandchild_record["parent_session"].as_str(),
        Some(child.as_str()),
        "lineage chains one hop per start, not straight to the root"
    );

    // The parent record itself was started from outside: no lineage.
    assert!(harness.record(&parent)["parent_session"].is_null());
}

#[test]
fn stale_or_absent_stamp_records_no_lineage() {
    let harness = Harness::new();
    let workspace = harness.workspace();
    std::fs::create_dir_all(&workspace).unwrap();

    // A stamp naming a record that does not exist (killed/forgotten parent,
    // an unrelated uuid) must be dropped, never fabricated -- and must not
    // fail the start.
    let stale = harness.start_inside(Some(&Uuid::new_v4().to_string()), "stale");
    assert!(harness.record(&stale)["parent_session"].is_null());

    let bare = harness.start_inside(None, "bare");
    assert!(harness.record(&bare)["parent_session"].is_null());
}
