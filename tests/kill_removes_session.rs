//! `a kill` and a clean natural exit both remove the session entirely.
//!
//! Finished sessions used to leave a durable `exited` row that every listing
//! client (PocketShell included) kept rendering until someone ran `a forget`
//! / `a prune`. The worker now removes the record itself during finalization
//! of a clean, proven-empty finish -- `a kill`, `exit`, Ctrl-D / shell EOF,
//! or a command that ran to completion -- so nothing remains to list: no
//! state dir, no runtime dir, and a follow-up kill/forget answers "no
//! matching session". Failed and OOM records are the leftover diagnostic
//! trail; this file pins the clean-exit removal.
//!
//! "Clean" here means the worker finalized without error and proved its
//! containment domain empty -- not that the workload succeeded. A non-zero
//! exit and a signalled workload are removed exactly like a zero exit; the
//! process is over either way, and its exit status goes to whoever was
//! watching rather than into a row nobody asked to keep. `keep_exited =
//! true` in the config is the escape hatch back to durable post-mortem
//! records.

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

    /// Write the config file this harness points `APLEXER_CONFIG` at. Must
    /// be called before `start`, since the worker reads the file the session
    /// was launched with.
    fn write_config(&self, text: &str) {
        std::fs::write(&self.config_file, text).expect("write config");
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
fn natural_exit_removes_the_record() {
    let harness = Harness::new();
    let workspace = harness.workspace();
    std::fs::create_dir_all(&workspace).unwrap();
    // `true` is the same lifecycle as typing `exit` or Ctrl-D in an
    // attached shell: the workload ends, the worker proves containment
    // empty, and the record must disappear instead of parking as `exited`.
    let id = harness.start("natural", "true");

    assert!(
        harness.wait_until_record_gone(&id, Duration::from_secs(10)),
        "a naturally exiting session left a durable record: {}",
        harness.state_session(&id).display()
    );
    assert!(
        !harness.list_ids().contains(&id),
        "a naturally exiting session still appears in `a list --json`"
    );
    let (stdout, stderr) = harness.run_failing(&["status", &id, "--json"], Duration::from_secs(5));
    let detail = format!("{stdout}{stderr}");
    assert!(
        detail.contains("no matching session"),
        "status of a naturally exiting session should report it gone, got: {detail}"
    );
}

#[test]
fn non_zero_exit_removes_the_record() {
    let harness = Harness::new();
    std::fs::create_dir_all(harness.workspace()).unwrap();
    // The reported lingering rows included an `exit code 1`. A command that
    // failed is still a command that is over: there is no process left to
    // manage, so there is nothing for `a list` to list.
    let id = harness.start("failed-cmd", "exit 1");

    assert!(
        harness.wait_until_record_gone(&id, Duration::from_secs(10)),
        "a session whose workload exited non-zero left a durable record: {}",
        harness.state_session(&id).display()
    );
    assert!(
        !harness.list_ids().contains(&id),
        "a non-zero exit still appears in `a list --json`"
    );
}

#[test]
fn signalled_workload_removes_the_record() {
    let harness = Harness::new();
    std::fs::create_dir_all(harness.workspace()).unwrap();
    // The reported lingering rows also included three SIGKILLs. A workload
    // killed from outside aplexer (`kill -9 <workload pid>`, a supervisor, a
    // crash) reaches the same finalization as `a kill`: the domain empties,
    // and the record goes with it.
    let id = harness.start("signalled", "kill -9 $$");

    assert!(
        harness.wait_until_record_gone(&id, Duration::from_secs(10)),
        "a signalled session left a durable record: {}",
        harness.state_session(&id).display()
    );
    assert!(
        !harness.list_ids().contains(&id),
        "a signalled session still appears in `a list --json`"
    );
}

#[test]
fn keep_exited_config_retains_the_terminal_record() {
    let harness = Harness::new();
    std::fs::create_dir_all(harness.workspace()).unwrap();
    // The documented escape hatch for anyone who wants the post-mortem
    // trail back. Everything else in this file runs with no config file at
    // all, which is the default (`keep_exited = false`).
    harness.write_config("version = 1\nkeep_exited = true\n");
    let id = harness.start("kept", "exit 3");

    // Wait for the worker to finish rather than sampling immediately: the
    // point is that the record is still there once finalization is over,
    // not that it exists during it. The worker unlinks its runtime dir on
    // the way out either way, so that is the "finished" signal.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while harness.runtime_session(&id).exists() && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !harness.runtime_session(&id).exists(),
        "worker did not finish within the timeout"
    );

    assert!(
        harness.state_session(&id).exists(),
        "keep_exited = true must retain the durable record"
    );
    let listed = harness.run_ok(&["--json", "list"], Duration::from_secs(5));
    let rows: Value = serde_json::from_str(&listed).expect("list JSON");
    let row = rows
        .as_array()
        .expect("list array")
        .iter()
        .find(|row| row["id"] == id.as_str())
        .unwrap_or_else(|| panic!("kept session missing from `a list`: {listed}"));
    assert_eq!(row["phase"], "exited", "{row}");
    assert_eq!(row["exit"]["code"], 3, "{row}");

    // Deliberately not asserting that `a prune` then reaps this record.
    // Prune's bar is pid liveness (`reap_verdict`), so what it decides here
    // depends on whether the just-exited worker has been reaped by its
    // parent yet -- and running the suite from inside an aplexer session
    // (the normal way on this project) leaves that worker a zombie, which
    // every `kill(pid, 0)` probe still calls alive. That is prune's
    // contract, not this escape hatch's, and it is already owned by
    // tests/prune_dead_records.rs under a fixture that controls process
    // liveness directly. What belongs here is what `keep_exited = true`
    // itself promises: the terminal record survives finalization and stays
    // addressable, asserted above.
}
