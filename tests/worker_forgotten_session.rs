//! A worker launched for a session whose durable state is already gone must
//! not bring that session back.
//!
//! `a forget`, `a prune`, and `a start`'s reclaim all fence a pre-PID worker
//! through `runtime/sessions/<id>/worker.lock` and remove the session's
//! state while holding it. A late worker recreates the runtime dir, takes a
//! fresh lock inode unopposed, and -- when it had read the record before
//! locking -- rewrote `session.json` from that stale copy, resurrecting a
//! forgotten session. The worker now reads the record only under its lock
//! and, finding none, refuses to start and takes the runtime dir it
//! recreated back out.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tempfile::TempDir;
use uuid::Uuid;

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
fn worker_refuses_a_session_whose_record_is_gone_and_leaves_nothing_behind() {
    let runtime_dir = TempDir::new().expect("runtime tempdir");
    let state_dir = TempDir::new().expect("state tempdir");
    let id = Uuid::new_v4();
    // The shape a destroyer leaves for a moment after removing the state
    // dir, or that a late worker recreates itself: a runtime dir carrying
    // the one-shot launch environment, and no durable record anywhere.
    let runtime_session: PathBuf = runtime_dir.path().join("sessions").join(id.to_string());
    std::fs::create_dir_all(&runtime_session).unwrap();
    std::fs::write(runtime_session.join("launch-environment.json"), "{}\n").unwrap();
    let state_session = state_dir.path().join("sessions").join(id.to_string());
    assert!(!state_session.exists());

    let mut worker = Command::new(env!("CARGO_BIN_EXE_aplexer"));
    worker
        .args(["worker", "--id", &id.to_string()])
        .env("APLEXER_RUNTIME_DIR", runtime_dir.path())
        .env("APLEXER_STATE_DIR", state_dir.path())
        .env("APLEXER_CONFIG", runtime_dir.path().join("config.toml"));
    let output = run_with_timeout(worker, Duration::from_secs(15));
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "a worker with no durable record must refuse to start; stderr: {stderr}"
    );
    assert!(
        stderr.contains("no durable record"),
        "refusal must name the missing record, got: {stderr}"
    );
    assert!(
        !state_session.exists(),
        "the worker recreated durable state for a session that no longer exists"
    );
    assert!(
        !runtime_session.exists(),
        "the worker left its runtime dir behind for a session that no longer exists"
    );
}
