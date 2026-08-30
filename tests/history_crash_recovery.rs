use aplexer::{read_persisted_history_tail, Paths};
use serde_json::Value;
use std::path::PathBuf;
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
        let config = runtime.path().join("config.toml");
        Self {
            runtime,
            state,
            config,
        }
    }

    fn paths(&self) -> Paths {
        Paths {
            runtime_root: self.runtime.path().to_path_buf(),
            state_root: self.state.path().to_path_buf(),
            config_file: self.config.clone(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_a"))
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            .args(args)
            .output()
            .expect("run CLI")
    }
}

#[test]
fn worker_crash_recovers_the_latest_committed_byte_exact_tail() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace");
    let started = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "crash-history",
        "--history-bytes",
        "1024",
        "--",
        "/bin/bash",
        "--norc",
    ]);
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let record: Value = serde_json::from_slice(&started.stdout).expect("start JSON");
    let id = record["id"].as_str().expect("session id");
    let worker_pid = record["worker_pid"].as_u64().expect("worker pid") as i32;
    let workload_pid = record["workload_pid"].as_u64().expect("workload pid") as i32;
    let history_path = harness.paths().history(id.parse().unwrap());
    let marker = "committed-before-worker-crash";

    let sent = harness.run(&["send", id, &format!("printf '{marker}\\n'"), "--enter"]);
    assert!(
        sent.status.success(),
        "send failed: {}",
        String::from_utf8_lossy(&sent.stderr)
    );
    let persistence_deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if read_persisted_history_tail(&history_path, None).is_ok_and(|bytes| {
            bytes
                .windows(marker.len())
                .any(|window| window == marker.as_bytes())
        }) {
            break;
        }
        assert!(
            Instant::now() < persistence_deadline,
            "periodic history checkpoint did not commit the marker"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        std::fs::read(&history_path)
            .expect("read old-client raw history before crash")
            .windows(marker.len())
            .any(|window| window == marker.as_bytes()),
        "raw compatibility history lagged the committed v2 generation"
    );

    assert_eq!(unsafe { libc::kill(worker_pid, libc::SIGKILL) }, 0);
    let exit_deadline = Instant::now() + Duration::from_secs(2);
    while PathBuf::from(format!("/proc/{worker_pid}")).exists() && Instant::now() < exit_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !PathBuf::from(format!("/proc/{worker_pid}")).exists(),
        "worker did not exit after SIGKILL"
    );

    let captured = harness.run(&["capture", id]);
    assert!(
        captured.status.success(),
        "dead capture failed: {}",
        String::from_utf8_lossy(&captured.stderr)
    );
    assert!(
        captured
            .stdout
            .windows(marker.len())
            .any(|window| window == marker.as_bytes()),
        "dead capture lost the committed marker: {:?}",
        captured.stdout
    );
    assert!(
        std::fs::read(&history_path)
            .expect("read old-client raw history after crash")
            .windows(marker.len())
            .any(|window| window == marker.as_bytes()),
        "worker crash left the old-client history stale"
    );

    unsafe {
        libc::kill(-workload_pid, libc::SIGKILL);
        libc::kill(workload_pid, libc::SIGKILL);
    }
}
