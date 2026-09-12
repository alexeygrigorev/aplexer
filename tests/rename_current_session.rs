//! `a rename` with no SESSION argument renames the session the command runs
//! inside (`APLEXER_SESSION_ID`) -- the CLI twin of the attach client's
//! `Ctrl-b R` prompt. These tests drive the real binary against a real
//! worker, the way `tests/rename_uniqueness.rs` does.

use aplexer::{list_records, Paths};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
    sessions: Vec<String>,
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
            sessions: Vec::new(),
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

    fn run(&self, args: &[&str]) -> Output {
        let mut command = self.command();
        command.args(args);
        run_with_timeout(command, Duration::from_secs(10))
    }

    fn run_in_session(&self, session_id: &str, args: &[&str]) -> Output {
        let mut command = self.command();
        command.env("APLEXER_SESSION_ID", session_id).args(args);
        run_with_timeout(command, Duration::from_secs(10))
    }

    /// An invocation that must not accidentally resolve to a session through
    /// the ancestor-/proc fallback: the test runner itself may well be
    /// running inside an aplexer session, whose stamp would otherwise be
    /// inherited.
    fn run_outside_any_session(&self, args: &[&str]) -> Output {
        let mut command = self.command();
        command.env_remove("APLEXER_SESSION_ID").args(args);
        run_with_timeout(command, Duration::from_secs(10))
    }

    fn start(&mut self, workspace: &Path, tag: &str) -> aplexer::SessionRecord {
        let output = self.run(&[
            "--json",
            "start",
            "--workspace",
            workspace.to_str().expect("UTF-8 workspace"),
            "--tag",
            tag,
            "--",
            "/bin/sleep",
            "300",
        ]);
        assert!(
            output.status.success(),
            "start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record = serde_json::from_slice::<aplexer::SessionRecord>(&output.stdout)
            .expect("start record JSON");
        self.sessions.push(record.id.to_string());
        record
    }

    fn paths(&self) -> Paths {
        Paths {
            runtime_root: self.runtime.path().to_path_buf(),
            state_root: self.state.path().to_path_buf(),
            config_file: self.config.clone(),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for id in &self.sessions {
            let _ = self
                .command()
                .args(["kill", id, "--signal", "KILL", "--grace-ms", "0"])
                .output();
        }
    }
}

#[test]
fn rename_without_selector_renames_the_session_it_runs_inside() {
    let mut harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let record = harness.start(workspace.path(), "original");

    let output = harness.run_in_session(
        &record.id.to_string(),
        &["--json", "rename", "--tag", "renamed"],
    );
    assert!(
        output.status.success(),
        "in-session rename failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let renamed = serde_json::from_slice::<aplexer::SessionRecord>(&output.stdout)
        .expect("rename reply is the updated record JSON");
    assert_eq!(renamed.tag, "renamed");
    assert_eq!(
        renamed.workspace,
        workspace
            .path()
            .canonicalize()
            .expect("canonical workspace"),
        "the in-session form must not move the workspace"
    );

    let records = list_records(&harness.paths()).expect("registry after rename");
    assert!(
        records
            .iter()
            .any(|r| r.id == record.id && r.tag == "renamed"),
        "renamed record missing from the registry: {records:?}"
    );
    assert!(
        records.iter().all(|r| r.tag != "original"),
        "the old tag must be gone: {records:?}"
    );
}

#[test]
fn rename_without_selector_refuses_to_guess_a_tag() {
    let mut harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let record = harness.start(workspace.path(), "original");

    let output = harness.run_in_session(&record.id.to_string(), &["--json", "rename"]);
    assert!(
        !output.status.success(),
        "a tagless in-session rename must not succeed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--tag is required"),
        "the refusal must say what is missing: {stderr}"
    );
    let records = list_records(&harness.paths()).expect("registry after refusal");
    assert!(
        records
            .iter()
            .any(|r| r.id == record.id && r.tag == "original"),
        "a refused rename must not touch the record: {records:?}"
    );
}

#[test]
fn rename_without_selector_outside_a_session_says_so() {
    let harness = Harness::new();
    let output = harness.run_outside_any_session(&["--json", "rename", "--tag", "somewhere"]);
    assert!(
        !output.status.success(),
        "a sessionless rename must not succeed"
    );
    // Two honest failure shapes, environment-dependent because
    // discover_session_id also walks ancestor /proc environ (the test
    // runner itself may run inside an aplexer session, whose stamp is then
    // found but resolves to nothing in this test's runtime): no identity at
    // all, or an identity with no record in this runtime dir. Both must name
    // APLEXER_SESSION_ID rather than a selector parse failure.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("APLEXER_SESSION_ID"),
        "the error must name the missing identity, not a selector parse: {stderr}"
    );
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Output {
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
            panic!("command pid {pid} exceeded {timeout:?}");
        }
    }
}
