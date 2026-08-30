//! Product-path regressions for fail-closed broken-session recovery.

use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_a"))
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            .args(args)
            .output()
            .expect("run aplexer CLI")
    }
}

struct ProcessCleanup(Vec<i32>);

impl Drop for ProcessCleanup {
    fn drop(&mut self) {
        for pid in self.0.drain(..) {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "{} did not appear", path.display());
}

fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn kill_preserves_evidence_when_dead_unlimited_worker_loses_setsid_descendant() {
    assert!(Path::new("/usr/bin/setsid").is_file(), "setsid is required");
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let descendant_marker = harness.runtime.path().join("setsid-descendant-pid");
    let descendant_marker_text = descendant_marker.to_str().expect("UTF-8 marker path");
    let workspace_text = workspace.path().to_str().expect("UTF-8 workspace");

    let started = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace_text,
        "--tag",
        "broken-setsid",
        "--",
        "/bin/sh",
        "-c",
        "/usr/bin/setsid /bin/sh -c 'trap \"\" HUP TERM; echo $$ > \"$1\"; sleep 30' aplexer-descendant \"$1\" & wait",
        "aplexer-leader",
        descendant_marker_text,
    ]);
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let record: Value = serde_json::from_slice(&started.stdout).expect("start JSON");
    let id = record["id"].as_str().expect("session id");
    let worker_pid = record["worker_pid"].as_i64().expect("worker pid") as i32;
    wait_for_path(&descendant_marker);
    let descendant_pid = fs::read_to_string(&descendant_marker)
        .expect("read descendant marker")
        .trim()
        .parse::<i32>()
        .expect("descendant pid");
    let _cleanup = ProcessCleanup(vec![descendant_pid, worker_pid]);

    assert_eq!(unsafe { libc::kill(worker_pid, libc::SIGKILL) }, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_alive(worker_pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    let killed = harness.run(&["kill", id, "--signal", "KILL", "--grace-ms", "0"]);
    assert!(
        !killed.status.success(),
        "ambiguous cleanup reported success"
    );
    assert!(
        String::from_utf8_lossy(&killed.stderr).contains("no authoritative containment locator"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&killed.stderr)
    );
    assert!(
        process_alive(descendant_pid),
        "setsid descendant was not preserved"
    );
    assert!(
        harness.state.path().join("sessions").join(id).exists(),
        "durable evidence was removed"
    );
    assert!(
        harness.runtime.path().join("sessions").join(id).exists(),
        "runtime evidence was removed"
    );
}
