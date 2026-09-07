//! A running worker must reap the descendants it adopts.
//!
//! Workers set `PR_SET_CHILD_SUBREAPER`, so every process in the workload's
//! tree that loses its own parent reparents to the worker. Before this
//! behaviour existed the worker only waited for those adoptees on its way
//! out, so a long-lived session accumulated `Z` zombies for its entire
//! lifetime -- thousands of them on a busy machine, each holding a pid and,
//! worse, each still answering `kill(pid, 0)` and so being counted as a
//! *live* descendant by the containment and prune probes.
//!
//! Each test here builds a real orphan: an intermediate shell backgrounds a
//! process and exits immediately, so the kernel reparents the survivor onto
//! the worker.

use aplexer::{process_alive, process_is_zombie, process_state};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    workspace: TempDir,
    config: PathBuf,
    ids: Vec<String>,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().expect("runtime tempdir");
        let state = TempDir::new().expect("state tempdir");
        let workspace = TempDir::new().expect("workspace tempdir");
        let config = runtime.path().join("config.toml");
        Self {
            runtime,
            state,
            workspace,
            config,
            ids: Vec::new(),
        }
    }

    fn keep_exited(self) -> Self {
        fs::write(&self.config, "version = 1\nkeep_exited = true\n").expect("write config");
        self
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config);
        command
    }

    fn run(&self, args: &[&str], timeout: Duration) -> Output {
        let mut command = self.command();
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().expect("spawn a");
        let pid = child.id();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        match rx.recv_timeout(timeout) {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => panic!("wait for `a {}`: {error}", args.join(" ")),
            Err(_) => {
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                panic!("`a {}` exceeded {timeout:?}", args.join(" "));
            }
        }
    }

    fn start(&mut self, tag: &str, script: &str, marker: &Path) -> Value {
        let workspace = self.workspace.path().to_str().unwrap().to_owned();
        let marker = marker.to_str().unwrap().to_owned();
        let output = self.run(
            &[
                "--json",
                "start",
                "--workspace",
                &workspace,
                "--tag",
                tag,
                "--",
                "/bin/sh",
                "-c",
                script,
                "aplexer-leader",
                &marker,
            ],
            Duration::from_secs(20),
        );
        assert!(
            output.status.success(),
            "start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let started: Value = serde_json::from_slice(&output.stdout).expect("start JSON");
        self.ids
            .push(started["id"].as_str().expect("session id").to_owned());
        started
    }

    fn record_path(&self, id: &str) -> PathBuf {
        self.state
            .path()
            .join("sessions")
            .join(id)
            .join("session.json")
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for id in &self.ids {
            let _ = self
                .command()
                .args(["kill", id, "--signal", "KILL", "--grace-ms", "0"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// Background a `sleep` from a shell that exits immediately. The `sleep`
/// survives its parent and is reparented onto the nearest subreaper, which
/// is the aplexer worker.
const ORPHAN_ONE_AND_WAIT: &str = r#"
/bin/sh -c '/bin/sleep 300 & echo $! > "$1"' orphan "$1"
exec /bin/sleep 300
"#;

fn wait_for_pid_file(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        assert!(
            Instant::now() < deadline,
            "orphan never published its pid to {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn parent_pid(pid: u32) -> Option<u32> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The comm field may contain spaces and ')', so ppid is the second token
    // after its final close-paren.
    stat.rfind(')')
        .and_then(|end| stat.get(end + 1..))
        .and_then(|rest| rest.split_whitespace().nth(1))
        .and_then(|ppid| ppid.parse().ok())
}

/// Wait until the kernel has actually reparented the orphan onto the worker.
/// Without this the rest of the test would be measuring the intermediate
/// shell's exit, not the subreaper's adoption.
fn wait_for_adoption(orphan: u32, worker_pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if parent_pid(orphan) == Some(worker_pid) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "orphan {orphan} was never adopted by worker {worker_pid} \
             (parent is {:?})",
            parent_pid(orphan)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_running_worker_reaps_a_descendant_it_adopted() {
    let mut harness = Harness::new();
    let marker = harness.runtime.path().join("orphan.pid");
    let started = harness.start("adopted", ORPHAN_ONE_AND_WAIT, &marker);
    let worker_pid = started["worker_pid"].as_u64().expect("worker pid") as u32;
    let orphan = wait_for_pid_file(&marker);
    wait_for_adoption(orphan, worker_pid);

    // Kill only the process this test created. The worker's workload leader
    // is deliberately left running: this is about a *running* worker reaping,
    // not about its exit path.
    assert_eq!(
        unsafe { libc::kill(orphan as libc::pid_t, libc::SIGKILL) },
        0,
        "kill orphan {orphan}: {}",
        std::io::Error::last_os_error()
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if !Path::new(&format!("/proc/{orphan}")).exists() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "orphan {orphan} was still in /proc after 10s in state {:?}; \
             a `Z` here means the worker adopted it and never reaped it",
            process_state(orphan).ok()
        );
        thread::sleep(Duration::from_millis(10));
    }

    // The whole point: the pid is gone, not merely dead-but-unreaped.
    assert!(
        !process_is_zombie(orphan),
        "orphan {orphan} is still a zombie"
    );
    assert!(
        !process_alive(orphan),
        "liveness probe still reports reaped orphan {orphan} as alive"
    );

    // And the session it came from is untouched by the reaping.
    let id = started["id"].as_str().unwrap().to_owned();
    let status = harness.run(&["--json", "status", &id], Duration::from_secs(10));
    assert!(
        status.status.success(),
        "status after reaping failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status["phase"], "running", "{status}");
}

/// A zombie is not a live descendant. `kill_descendants` loops until its
/// procfs scan finds nothing left to signal, and SIGKILL to a zombie changes
/// nothing -- so counting one as live made `a kill --signal KILL` spin for
/// the full `DESCENDANT_KILL_TIMEOUT` and then fail with "timed out killing
/// contained workload descendants".
#[test]
fn a_dead_descendant_does_not_block_the_kill_path() {
    let mut harness = Harness::new();
    let marker = harness.runtime.path().join("orphan.pid");
    let started = harness.start("kill-path", ORPHAN_ONE_AND_WAIT, &marker);
    let id = started["id"].as_str().expect("session id").to_owned();
    let worker_pid = started["worker_pid"].as_u64().expect("worker pid") as u32;
    let orphan = wait_for_pid_file(&marker);
    wait_for_adoption(orphan, worker_pid);

    // Stop the orphan without reaping it from here: the worker is its parent
    // now, so this creates exactly the zombie the kill path used to trip on.
    assert_eq!(
        unsafe { libc::kill(orphan as libc::pid_t, libc::SIGKILL) },
        0
    );

    let began = Instant::now();
    let killed = harness.run(
        &["kill", &id, "--signal", "KILL", "--grace-ms", "0"],
        Duration::from_secs(15),
    );
    assert!(
        killed.status.success(),
        "kill failed after {:?}: {}",
        began.elapsed(),
        String::from_utf8_lossy(&killed.stderr)
    );
    // The old failure mode was a full 2 s `DESCENDANT_KILL_TIMEOUT` spin
    // ending in an error; a healthy kill is far under that.
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "kill took {:?}",
        began.elapsed()
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(worker_pid) {
        assert!(
            Instant::now() < deadline,
            "worker {worker_pid} outlived its killed session"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Reaping adoptees must never touch the workload's own exit status. The
/// leader orphans a burst of short-lived processes -- every one of them a
/// SIGCHLD that drives the reaper -- immediately before exiting with a
/// distinctive code, so a reaper that reached for `waitpid(-1)` would very
/// likely consume the leader's status and leave the session with none.
#[test]
fn reaping_adoptees_preserves_the_workload_exit_status() {
    let mut harness = Harness::new().keep_exited();
    let marker = harness.runtime.path().join("unused.pid");
    let script = r#"
i=0
while [ $i -lt 12 ]; do
    /bin/sh -c '/bin/sleep 0.05 &'
    i=$((i + 1))
done
/bin/sleep 1
exit 42
"#;
    let started = harness.start("exit-status", script, &marker);
    let id = started["id"].as_str().expect("session id").to_owned();
    let record_path = harness.record_path(&id);

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = fs::read_to_string(&record_path) {
            if let Ok(record) = serde_json::from_str::<Value>(&text) {
                if !record["exit"].is_null() {
                    assert_eq!(
                        record["exit"]["code"], 42,
                        "the workload's exit status was lost or replaced: {record}"
                    );
                    assert!(record["exit"]["signal"].is_null(), "{record}");
                    assert_eq!(record["phase"], "exited", "{record}");
                    assert_eq!(record["error"], Value::Null, "{record}");
                    return;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "session never recorded an exit status"
        );
        thread::sleep(Duration::from_millis(25));
    }
}
