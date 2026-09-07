//! An accepted kill persists `phase: exiting` BEFORE teardown starts
//! (issue #18).
//!
//! `a kill` returns ok once the worker accepts the Kill RPC; the durable
//! record is removed later by lifecycle finalization, with the CLI's
//! five-second record-removal wait bounding how long that may take -- and
//! finalization can wedge indefinitely on a containment domain it cannot
//! prove empty. Throughout that window the record used to keep its pre-kill
//! phase (`running`) with `worker_alive: true`, so a snapshot consumer could
//! not tell a dying session from a healthy one. This file pins the honest
//! dying signal: from kill acceptance until the record disappears, the row
//! reads `exiting` (state `exiting`, "stopping" in the terminal UI), and
//! nothing about kill's exit codes or clean-removal outcome changes.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use aplexer::{read_record, Phase};
use serde_json::Value;
use tempfile::TempDir;

/// How long a poll for something that must eventually become true is allowed
/// to run before the test gives up. The budget only bounds how long a broken
/// run takes to say so.
const LIVENESS_BACKSTOP: Duration = Duration::from_secs(15);

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

    fn record_path(&self, id: &str) -> PathBuf {
        self.state_dir
            .path()
            .join("sessions")
            .join(id)
            .join("session.json")
    }

    fn snapshot_row(&self, id: &str) -> Value {
        let stdout = self.run_ok(&["--json", "snapshot"], Duration::from_secs(5));
        let rows: Value = serde_json::from_str(&stdout).expect("snapshot JSON");
        rows.as_array()
            .expect("snapshot is a bare array")
            .iter()
            .find(|row| row["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("session {id} missing from snapshot: {stdout}"))
    }

    fn wait_until_record_gone(&self, id: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !self.record_path(id).exists() {
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
    let (tx, rx) = std::sync::mpsc::channel();
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

/// Spawn `a kill` without waiting for it: the test must be able to sample
/// the record while the kill's grace window is still open.
fn spawn_kill(harness: &Harness, id: &str, grace_ms: u64) -> Child {
    harness
        .command()
        .args([
            "kill",
            id,
            "--signal",
            "TERM",
            "--grace-ms",
            &grace_ms.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn a kill")
}

/// Wait for the record's phase to become `exiting` and hand it back. A
/// workload that died on the first TERM would leave `running` in place (the
/// pre-#18 behavior), so the backstop turns a missing transition into a
/// failure rather than a vacuous pass.
fn wait_for_exiting(path: &std::path::Path) -> aplexer::SessionRecord {
    let deadline = Instant::now() + LIVENESS_BACKSTOP;
    loop {
        let record = read_record(path).expect("read session record");
        if record.phase == Phase::Exiting {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "accepted kill never persisted phase exiting; record still says {:?}",
            record.phase
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// The workload must survive TERM for the whole grace window so the sample
/// points below land mid-teardown. A `while` loop cannot be exec-optimized
/// away by the shell, so the trapped-TERM leader stays alive (its `sleep`
/// children die on the group signal; the loop replaces them) and the
/// containment domain stays populated until the SIGKILL escalation.
const TERM_PROOF_WORKLOAD: &str = "trap \"\" TERM; while :; do /bin/sleep 1; done";

#[test]
fn kill_shows_exiting_in_record_snapshot_and_status_until_the_record_is_gone() {
    let harness = Harness::new();
    std::fs::create_dir_all(harness.workspace()).unwrap();
    let id = harness.start("dying", TERM_PROOF_WORKLOAD);
    let record_path = harness.record_path(&id);
    assert!(
        record_path.exists(),
        "sanity: durable record exists before the kill"
    );

    // 2s: long enough to hold the teardown window open while the test
    // samples it, short enough that the blocked-until-empty `runtime.kill`
    // response lands inside the client's 3s CONTROL_RPC_TIMEOUT (an
    // over-budget grace makes `a kill` itself time out reading the reply --
    // pre-existing client behavior, not this fix's concern).
    let grace_ms = 2_000;
    let mut kill = spawn_kill(&harness, &id, grace_ms);

    // The kill was accepted: the record now says `exiting` while the worker
    // that accepted it is still alive.
    let record = wait_for_exiting(&record_path);
    assert!(
        aplexer::process_alive(record.worker_pid.expect("worker pid")),
        "worker that accepted the kill is already gone mid-teardown"
    );
    assert!(
        kill.try_wait().expect("probe kill child").is_none(),
        "kill finished before sampling began; the TERM-proof workload did not hold the grace window"
    );

    // The snapshot a consumer runs right after an accepted kill must be
    // honest about the dying session, distinguishable from a healthy row.
    // Still mid-grace: the kill child must not have finished either.
    let row = harness.snapshot_row(&id);
    assert!(
        kill.try_wait().expect("probe kill child").is_none(),
        "kill finished during sampling; window closed early"
    );
    assert_eq!(row["phase"], "exiting", "{row}");
    assert_eq!(row["state"], "exiting", "{row}");
    assert_eq!(row["worker_alive"], true, "{row}");

    // `a status` derives from the same predicate, so the two commands
    // cannot disagree about the dying session.
    let status_stdout = harness.run_ok(&["status", &id, "--json"], Duration::from_secs(5));
    let status: Value = serde_json::from_str(&status_stdout).expect("status JSON");
    assert_eq!(status["phase"], "exiting", "{status}");
    assert_eq!(status["state"], "exiting", "{status}");
    assert_eq!(status["worker_alive"], true, "{status}");

    // The graceful signal is deliberately ignored, so this kill only ends
    // via the SIGKILL escalation after the full grace window -- but its
    // contract is unchanged: exit 0, no fallback stderr outcome, and the
    // worker removes the record itself. `still finalizing` (Pending) and
    // `kept the record` (Kept) are the two messages a kill prints when the
    // record survives; neither belongs on this clean path.
    let output = kill.wait_with_output().expect("reap a kill");
    assert!(
        output.status.success(),
        "accepted kill must still exit 0 (status {:?}), stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("still finalizing") && !stderr.contains("kept the record"),
        "clean kill must not report the fallback outcomes, got: {stderr}"
    );
    assert!(
        harness.wait_until_record_gone(&id, Duration::from_secs(10)),
        "record survived a completed kill"
    );
    let stdout = harness.run_ok(&["--json", "snapshot"], Duration::from_secs(5));
    assert!(
        !stdout.contains(&id),
        "killed session still appears in `a snapshot --json`: {stdout}"
    );
}

/// A kill accepted on a session whose workload dies instantly still removes
/// the record exactly as before -- the new exiting write must not slow the
/// path down, flip exit codes, or leave a record behind.
#[test]
fn fast_kill_still_exits_zero_and_removes_the_record() {
    let harness = Harness::new();
    std::fs::create_dir_all(harness.workspace()).unwrap();
    let id = harness.start("quick", "sleep 300");

    let output = run_with_timeout(
        {
            let mut command = harness.command();
            command.args(["kill", &id, "--signal", "KILL"]);
            command
        },
        Duration::from_secs(20),
    );
    assert!(
        output.status.success(),
        "kill --signal KILL must exit 0 (status {:?}), stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        harness.wait_until_record_gone(&id, Duration::from_secs(10)),
        "fast kill left the durable record behind"
    );
}
