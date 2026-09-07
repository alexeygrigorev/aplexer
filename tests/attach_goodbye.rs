//! `a attach`'s goodbye line must tell the truth about why the client left.
//!
//! Ctrl-b d and stdin EOF stay `Detached from X.`. A worker that dies under an
//! attached client is `Connection to X lost.`, never Detached -- the message is
//! the user's diagnosis of which layer to look at, and blaming their own
//! detach for a worker-side drop sends them debugging the client.
//!
//! The worker-sent `ServerEvent::Error` classification (`Attach dropped: X.`)
//! is pinned by `src/bin/a.rs`'s
//! `attach_goodbye_distinguishes_detach_error_and_socket_loss` unit test:
//! since issue #16 a live-screen subscriber coalesces rather than being
//! evicted, so forcing a real one through a live PTY needs a PTY/waiter
//! failure this file does not try to synthesize.
//!
//! The goodbye is gated on a tty stdout, so these spawn `a attach` on a real
//! PTY the same way tests/terminal_signal_cleanup.rs does.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let config = runtime.path().join("config.toml");
        Self {
            runtime,
            state,
            config,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command.env("APLEXER_RUNTIME_DIR", self.runtime.path());
        command.env("APLEXER_STATE_DIR", self.state.path());
        command.env("APLEXER_CONFIG", &self.config);
        command
    }

    fn output(&self, args: &[&str]) -> std::process::Output {
        let mut command = self.command();
        command.args(args);
        command.output().unwrap()
    }

    fn start(&self, workspace: &Path, tag: &str) -> String {
        let output = self.output(&[
            "start",
            "--workspace",
            workspace.to_str().unwrap(),
            "--tag",
            tag,
            "--json",
            "--",
            "bash",
            "--norc",
        ]);
        assert!(output.status.success(), "start failed: {output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        value["id"].as_str().unwrap().to_owned()
    }

    fn status(&self, id: &str) -> Value {
        let output = self.output(&["status", id, "--json"]);
        assert!(output.status.success(), "status failed: {output:?}");
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn selector(&self, id: &str) -> String {
        let value = self.status(id);
        format!(
            "{}:{}",
            value["workspace"].as_str().unwrap(),
            value["tag"].as_str().unwrap()
        )
    }

    fn kill_session(&self, id: &str) {
        let _ = self.output(&["kill", id, "--signal", "KILL", "--grace-ms", "0"]);
    }
}

struct PtyAttach {
    child: Child,
    master: File,
    captured: Arc<Mutex<Vec<u8>>>,
}

impl PtyAttach {
    fn spawn(harness: &Harness, id: &str, stdin_tty: bool) -> Self {
        let (master, slave) = aplexer::open_pty(24, 80).unwrap();
        let mut command = harness.command();
        command.args(["attach", id]);
        if stdin_tty {
            command.stdin(Stdio::from(slave.try_clone().unwrap()));
        } else {
            command.stdin(Stdio::null());
        }
        command.stdout(Stdio::from(slave.try_clone().unwrap()));
        command.stderr(Stdio::from(slave.try_clone().unwrap()));
        let child = command.spawn().unwrap();
        drop(slave);

        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        let mut reader = master.try_clone().unwrap();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => sink.lock().unwrap().extend_from_slice(&buf[..n]),
                }
            }
        });
        Self {
            child,
            master,
            captured,
        }
    }

    fn wait_for(&self, needle: &[u8], what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let out = self.captured.lock().unwrap().clone();
            if out.windows(needle.len()).any(|window| window == needle) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {what} ({:?}); captured:\n{}",
                String::from_utf8_lossy(needle),
                String::from_utf8_lossy(&out).replace('\x1b', "<ESC>")
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).unwrap();
        self.master.flush().unwrap();
    }

    fn wait_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                // Let the reader thread drain the last bytes after hangup.
                thread::sleep(Duration::from_millis(50));
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("attach did not exit after its termination condition");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.captured.lock().unwrap()).into_owned()
    }
}

/// Asserts the expected goodbye and, just as importantly, that none of the
/// other three leaked: the bug was one line standing in for four outcomes.
fn assert_goodbye(text: &str, expected: &str) {
    assert!(
        text.contains(expected),
        "missing goodbye {expected:?} in:\n{}",
        text.replace('\x1b', "<ESC>")
    );
    for other in [
        "Detached from",
        "Attach dropped:",
        "Connection to",
        "Session ended:",
    ] {
        if expected.starts_with(other) {
            continue;
        }
        assert!(
            !text.contains(other),
            "goodbye {expected:?} must not also print {other:?}:\n{}",
            text.replace('\x1b', "<ESC>")
        );
    }
}

#[test]
fn stdin_eof_on_tty_stdout_says_detached() {
    let harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let id = harness.start(workspace.path(), "eof-bye");
    let selector = harness.selector(&id);

    let mut attach = PtyAttach::spawn(&harness, &id, false);
    let status = attach.wait_exit();
    assert!(status.success(), "attach failed after stdin EOF: {status}");
    assert_goodbye(&attach.text(), &format!("Detached from {selector}."));

    let value = harness.status(&id);
    assert_eq!(value["phase"], "running");
    assert_eq!(value["worker_alive"], true);
    harness.kill_session(&id);
}

#[test]
fn ctrl_b_d_says_detached() {
    let harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let id = harness.start(workspace.path(), "chord-bye");
    let selector = harness.selector(&id);

    let mut attach = PtyAttach::spawn(&harness, &id, true);
    attach.wait_for(b"attached to", "attach hint");
    attach.send(&[0x02, b'd']);
    let status = attach.wait_exit();
    assert!(status.success(), "attach failed after Ctrl-b d: {status}");
    assert_goodbye(&attach.text(), &format!("Detached from {selector}."));

    let value = harness.status(&id);
    assert_eq!(value["phase"], "running");
    assert_eq!(value["worker_alive"], true);
    harness.kill_session(&id);
}

#[test]
fn worker_killed_under_attach_says_connection_lost() {
    let harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let id = harness.start(workspace.path(), "lost-bye");
    let selector = harness.selector(&id);
    let worker_pid = harness.status(&id)["worker_pid"]
        .as_u64()
        .expect("worker_pid") as i32;

    let mut attach = PtyAttach::spawn(&harness, &id, true);
    attach.wait_for(b"attached to", "attach hint");
    assert_eq!(unsafe { libc::kill(worker_pid, libc::SIGKILL) }, 0);

    let status = attach.wait_exit();
    assert!(
        status.success(),
        "attach should return Ok after socket loss: {status}"
    );
    assert_goodbye(&attach.text(), &format!("Connection to {selector} lost."));
}
