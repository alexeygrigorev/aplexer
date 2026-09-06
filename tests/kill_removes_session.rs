//! `a kill` removes the killed session entirely.
//!
//! Killing a session used to leave its durable record behind as an `exited`
//! row that every listing client (PocketShell included) kept rendering until
//! someone ran `a forget`/`a prune`. Since the worker that accepts the kill
//! RPC now removes the record itself during finalization, `a kill` must
//! leave nothing to list: no state dir, no runtime dir, and a follow-up
//! kill/forget answers "no matching session". A session that exits on its
//! own keeps its record -- post-mortem capture remains the natural-exit
//! behavior, and this file pins the distinction.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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

    /// Run a command expected to FAIL, returning (stdout, stderr).
    fn run_failing(&self, args: &[&str], timeout: Duration) -> (String, String) {
        let output = self.run(args, timeout);
        assert!(
            !output.status.success(),
            "`a {}` unexpectedly succeeded:\nstdout: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
        );
        (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    fn start(&self, tag: &str, script: &str) -> String {
        let stdout = self.run_ok(
            &[
                "--json",
                "start",
                "--workspace",
                self.workspace().to_str().unwrap(),
                "--tag",
                tag,
                "--",
                "/bin/sh",
                "-c",
                script,
            ],
            Duration::from_secs(15),
        );
        let value: Value = serde_json::from_str(&stdout).expect("start JSON");
        value["id"].as_str().expect("session id").to_string()
    }

    fn workspace(&self) -> std::path::PathBuf {
        self.runtime_dir.path().join("ws")
    }

    fn state_session(&self, id: &str) -> PathBuf {
        self.state_dir.path().join("sessions").join(id)
    }

    fn runtime_session(&self, id: &str) -> PathBuf {
        self.runtime_dir.path().join("sessions").join(id)
    }

    fn list_ids(&self) -> Vec<String> {
        let stdout = self.run_ok(&["--json", "list"], Duration::from_secs(5));
        let value: Value = serde_json::from_str(&stdout).expect("list JSON");
        value
            .as_array()
            .expect("list is a bare array")
            .iter()
            .filter_map(|record| record["id"].as_str().map(str::to_string))
            .collect()
    }

    fn wait_until_record_gone(&self, id: &str, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
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

#[test]
fn kill_removes_record_runtime_dir_and_listing() {
    let harness = Harness::new();
    let workspace = harness.workspace();
    std::fs::create_dir_all(&workspace).unwrap();
    let id = harness.start("gone", "sleep 60");

    assert!(
        harness.state_session(&id).exists(),
        "sanity: durable record exists before the kill"
    );

    harness.run_ok(&["kill", &id], Duration::from_secs(20));

    assert!(
        harness.wait_until_record_gone(&id, Duration::from_secs(10)),
        "durable state dir still present after kill: {}",
        harness.state_session(&id).display()
    );
    assert!(
        !harness.runtime_session(&id).exists(),
        "runtime dir still present after kill"
    );
    assert!(
        !harness.list_ids().contains(&id),
        "killed session still appears in `a list --json`"
    );

    // Both clients treat "already gone" as the goal state of a kill; the
    // error must stay exactly this phrase (pocketshell-electron
    // AplexerClient::isAplexerNotFound matches /no matching session/i).
    let (stdout, stderr) = harness.run_failing(&["kill", &id], Duration::from_secs(5));
    let detail = format!("{stdout}{stderr}");
    assert!(
        detail.contains("no matching session"),
        "second kill should report the record as gone, got: {detail}"
    );
}

#[test]
fn kill_escalating_past_ignored_term_still_removes() {
    let harness = Harness::new();
    let workspace = harness.workspace();
    std::fs::create_dir_all(&workspace).unwrap();
    let id = harness.start("stubborn", "trap \"\" TERM; sleep 60");

    harness.run_ok(
        &["kill", &id, "--signal", "TERM", "--grace-ms", "500"],
        Duration::from_secs(20),
    );

    assert!(
        harness.wait_until_record_gone(&id, Duration::from_secs(10)),
        "record survived a kill that had to escalate to SIGKILL"
    );
    assert!(!harness.list_ids().contains(&id));
}

#[test]
fn natural_exit_still_keeps_the_record() {
    let harness = Harness::new();
    let workspace = harness.workspace();
    std::fs::create_dir_all(&workspace).unwrap();
    let id = harness.start("natural", "true");

    // The worker finalizes (phase Exited) and exits; the record must remain
    // for post-mortem capture/status -- only kills remove it.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut exited = false;
    while std::time::Instant::now() < deadline {
        let stdout = harness.run_ok(&["status", &id, "--json"], Duration::from_secs(5));
        let value: Value = serde_json::from_str(&stdout).expect("status JSON");
        if value["phase"] == "exited" {
            exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(exited, "workload did not reach exited phase in time");
    assert!(
        harness.state_session(&id).exists(),
        "a naturally exiting session must keep its durable record"
    );
    assert!(
        harness.list_ids().contains(&id),
        "a naturally exiting session must stay listed until forgotten/pruned"
    );
}
