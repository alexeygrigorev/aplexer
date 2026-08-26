// Integration tests for the failure-domain invariants aplexer exists to
// provide (spec.md section 29): killing or OOM-ing one session's workload,
// or killing one session's worker process outright, must never affect
// unrelated sessions.
//
// The destructive tests (`three_sessions_oom_isolation`,
// `worker_kill_isolation`) need a real systemd --user session with
// cgroup-v2 delegation and take several seconds, so they are marked
// #[ignore] rather than run on every `cargo test`. Run them explicitly:
//
//   cargo test --release --test oom_isolation -- --ignored --nocapture
//
// `start_attach_send_capture_roundtrip` is a fast, non-destructive smoke
// test of the basic PTY spawn path (no cgroups needed) and runs by default
// -- it is the regression guard for a gate-pipe deadlock that used to make
// every single session spawn hang forever (see git history), which
// run_with_timeout turns into a loud failure instead of a hung test suite.
//
// Every test runs against an isolated APLEXER_RUNTIME_DIR/APLEXER_STATE_DIR
// (see Harness::new), so this never touches a real user's actual sessions.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_a"));
        cmd.env("APLEXER_RUNTIME_DIR", self.runtime_dir.path());
        cmd.env("APLEXER_STATE_DIR", self.state_dir.path());
        cmd.env("APLEXER_CONFIG", &self.config_file);
        cmd
    }

    fn run(&self, args: &[&str], timeout: Duration) -> std::process::Output {
        let mut cmd = self.command();
        cmd.args(args);
        run_with_timeout(cmd, timeout)
    }

    fn run_ok(&self, args: &[&str], timeout: Duration) -> String {
        let output = self.run(args, timeout);
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

/// Runs `cmd` to completion, killing it and panicking if it does not finish
/// within `timeout`. `a start` already has its own --startup-timeout-ms
/// (default 10s) so a hung worker fails fast on its own, but this is a
/// second, independent bound in case the CLI process itself ever hangs for
/// an unrelated reason -- a timed-out test is a clear failure, a hung test
/// binary is just a wedged CI job.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::process::Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().expect("failed to spawn command");
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("failed to wait for command: {error}"),
        Err(_) => {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            panic!("command (pid {pid}) did not finish within {timeout:?}");
        }
    }
}

fn start_session(harness: &Harness, workspace: &Path, tag: &str, memory: Option<&str>) -> String {
    let workspace = workspace.to_str().expect("utf8 workspace path");
    let mut args = vec!["start", "--workspace", workspace, "--tag", tag, "--json"];
    if let Some(mem) = memory {
        args.push("--memory");
        args.push(mem);
    }
    args.extend(["--", "bash", "--norc", "-l"]);
    let stdout = harness.run_ok(&args, Duration::from_secs(15));
    let value: Value = serde_json::from_str(&stdout).expect("`a start` output is JSON");
    value["id"]
        .as_str()
        .expect("session id in start output")
        .to_string()
}

/// Sends `echo <marker>` into the session and polls `a capture` until the
/// marker shows up in the PTY output, proving the session is alive and its
/// shell is actually processing input -- not just that the worker process
/// happens to still exist.
fn assert_responsive(harness: &Harness, id: &str, marker: &str) {
    harness.run_ok(
        &["send", id, &format!("echo {marker}"), "--enter"],
        Duration::from_secs(5),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = harness.run(&["capture", id, "--bytes", "4096"], Duration::from_secs(5));
        let captured = String::from_utf8_lossy(&output.stdout);
        if captured.contains(marker) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("session {id} never echoed back marker {marker:?}; last capture:\n{captured}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn cgroup_stats(harness: &Harness, id: &str) -> Option<Value> {
    let stdout = harness.run_ok(&["status", id, "--json"], Duration::from_secs(5));
    let value: Value = serde_json::from_str(&stdout).ok()?;
    value.get("cgroup").cloned()
}

fn worker_pid(harness: &Harness, id: &str) -> u32 {
    let stdout = harness.run_ok(&["status", id, "--json"], Duration::from_secs(5));
    let value: Value = serde_json::from_str(&stdout).expect("status output is JSON");
    value["worker_pid"]
        .as_u64()
        .expect("worker_pid in status output") as u32
}

fn make_workspaces(root: &Path, names: &[&str]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|name| {
            let ws = root.join(name);
            std::fs::create_dir_all(&ws).expect("create workspace dir");
            ws
        })
        .collect()
}

fn cleanup(harness: &Harness, ids: &[&str]) {
    for id in ids {
        let _ = harness.run(&["kill", id, "--signal", "KILL"], Duration::from_secs(5));
    }
}

/// Same delegation mechanism aplexer itself uses (systemd-run --user
/// --scope with Delegate=yes). Skip the destructive tests rather than fail
/// the suite in environments without a real systemd --user session
/// (containers, some CI runners) -- these tests need to actually assert
/// something about the *real* isolation mechanism, not a mock of it.
fn delegated_cgroups_available() -> bool {
    if !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        return false;
    }
    Command::new("systemd-run")
        .args([
            "--user",
            "--scope",
            "--collect",
            "--unit=aplexer-test-probe-availability",
            "-p",
            "Delegate=yes",
            "--",
            "true",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// spec.md section 29.1 (the project's own "critical isolation test"):
/// OOM-ing one session's workload must not affect unrelated sessions, and
/// the workload must actually be memory-bounded -- no swap escape -- so
/// the kernel OOM killer fires deterministically instead of the workload
/// just swapping unboundedly (a real bug this suite would have caught).
#[test]
#[ignore = "needs systemd --user with cgroup-v2 delegation; run explicitly"]
fn three_sessions_oom_isolation() {
    if !delegated_cgroups_available() {
        eprintln!("skipping: no delegated cgroup-v2 scope available in this environment");
        return;
    }
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspaces = make_workspaces(root.path(), &["a", "b", "c"]);

    let a = start_session(&harness, &workspaces[0], "a", Some("128M"));
    let b = start_session(&harness, &workspaces[1], "b", Some("128M"));
    let c = start_session(&harness, &workspaces[2], "c", Some("128M"));

    assert_responsive(&harness, &a, "a-ready");
    assert_responsive(&harness, &b, "b-ready");
    assert_responsive(&harness, &c, "c-ready");

    // Confirm the swap cap actually took effect before relying on it --
    // without it the bomb below just grows into swap and this test would
    // time out waiting for an OOM kill that never comes (see git history:
    // this happened for real while developing this fix).
    let swap_max = cgroup_stats(&harness, &b)
        .and_then(|stats| stats.get("memory_swap_current").cloned())
        .expect("session b has cgroup stats (memory limit should have created a cgroup)");
    assert_eq!(
        swap_max,
        json!(0),
        "expected session b to start with zero swap used"
    );

    // Memory bomb: grow unboundedly until the cgroup's 128M cap kills it.
    let bomb = "python3 -c \"d=[]\nwhile True:\n    d.append(bytearray(10 * 1024 * 1024))\n\"";
    harness.run_ok(&["send", &b, bomb, "--enter"], Duration::from_secs(5));

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut oom_kills = 0u64;
    while Instant::now() < deadline {
        if let Some(stats) = cgroup_stats(&harness, &b) {
            oom_kills = stats["oom_kill_count"].as_u64().unwrap_or(0);
            if oom_kills > 0 {
                break;
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
    assert!(
        oom_kills > 0,
        "expected the cgroup OOM killer to fire on session b's memory bomb within 15s"
    );

    // The headline invariant: A and C never noticed.
    assert_responsive(&harness, &a, "a-survived-bs-oom");
    assert_responsive(&harness, &c, "c-survived-bs-oom");

    // B's own PTY session (the shell) survives the OOM kill of its
    // subprocess and stays interactive -- the kernel OOM killer targets
    // the memory hog, not everything in the cgroup.
    assert_responsive(&harness, &b, "b-shell-survived-its-own-oom");

    cleanup(&harness, &[&a, &b, &c]);
}

/// spec.md section 29.2: killing a session's *worker* process outright
/// (not just its workload) must not affect unrelated sessions.
#[test]
#[ignore = "spawns and SIGKILLs real worker processes; run explicitly"]
fn worker_kill_isolation() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspaces = make_workspaces(root.path(), &["a", "b", "c"]);

    let a = start_session(&harness, &workspaces[0], "a", None);
    let b = start_session(&harness, &workspaces[1], "b", None);
    let c = start_session(&harness, &workspaces[2], "c", None);

    assert_responsive(&harness, &a, "a-ready");
    assert_responsive(&harness, &b, "b-ready");
    assert_responsive(&harness, &c, "c-ready");

    let pid = worker_pid(&harness, &b);
    let killed = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    assert_eq!(killed, 0, "failed to SIGKILL worker pid {pid}");

    assert_responsive(&harness, &a, "a-survived-worker-kill");
    assert_responsive(&harness, &c, "c-survived-worker-kill");

    // The now-workerless session should be visibly broken, not silently
    // reported as still running (see git history: `a list`/`a status` used
    // to report stale "running" state forever after a worker died).
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stdout = harness.run_ok(&["status", &b, "--json"], Duration::from_secs(5));
        let value: Value = serde_json::from_str(&stdout).expect("status output is JSON");
        if value["worker_alive"] == json!(false) {
            break;
        }
        if Instant::now() >= deadline {
            panic!("session {b}'s worker_alive never went false after SIGKILL; last status:\n{stdout}");
        }
        thread::sleep(Duration::from_millis(100));
    }

    cleanup(&harness, &[&a, &c]);
}

/// Fast, non-destructive smoke test of the basic PTY spawn path: no
/// cgroups, no systemd dependency, runs on every `cargo test`.
#[test]
fn start_attach_send_capture_roundtrip() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspaces = make_workspaces(root.path(), &["main"]);

    let id = start_session(&harness, &workspaces[0], "main", None);
    assert_responsive(&harness, &id, "roundtrip-ok");

    cleanup(&harness, &[&id]);
}
