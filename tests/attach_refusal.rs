//! A refused attach must say why. The worker used to return from
//! `handle_attach` before writing any `Response` frame when the attach
//! could not be established -- the attached-client cap, an oversized
//! geometry, a PTY that already closed -- so every cause reached the client
//! as a closed socket and "missing attach response". These drive the
//! control socket directly with the wire protocol so the assertion is on
//! the frame the worker writes, not on any client's rendering of it.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use aplexer::{frame_json, read_frame, write_json, FrameKind, Operation, Request, Response};
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

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

    fn run_ok(&self, args: &[&str], timeout: Duration) -> String {
        let mut command = self.command();
        command.args(args);
        let output = run_with_timeout(command, timeout);
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

    fn workspace(&self) -> PathBuf {
        self.runtime_dir.path().join("ws")
    }

    fn start(&self, tag: &str) -> (Uuid, PathBuf) {
        std::fs::create_dir_all(self.workspace()).unwrap();
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
                "sleep 60",
            ],
            Duration::from_secs(15),
        );
        let value: Value = serde_json::from_str(&stdout).expect("start JSON");
        let id = value["id"].as_str().expect("session id").parse().unwrap();
        let socket = PathBuf::from(value["socket_path"].as_str().expect("socket path"));
        (id, socket)
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

struct SessionGuard<'a> {
    harness: &'a Harness,
    id: Uuid,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        let mut command = self.harness.command();
        command.args(["kill", &self.id.to_string(), "--signal", "KILL"]);
        let _ = run_with_timeout(command, Duration::from_secs(10));
    }
}

/// Send one Attach request and return the worker's response frame. The
/// stream is handed back so a caller can keep the attach established.
fn attach(socket: &Path, id: Uuid, rows: Option<u16>, cols: Option<u16>) -> (UnixStream, Response) {
    let mut stream = UnixStream::connect(socket).expect("connect control socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let request = Request::new(
        id,
        Operation::Attach {
            history_bytes: Some(0),
            want_screen: false,
            rows,
            cols,
        },
    );
    write_json(&mut stream, &request).expect("write attach request");
    let frame = read_frame(&mut stream)
        .expect("read attach response")
        .expect("worker closed the socket before answering the attach");
    let response: Response = frame_json(frame).expect("attach response is a JSON frame");
    assert_eq!(response.request_id, request.request_id);
    (stream, response)
}

#[test]
fn attach_beyond_the_client_cap_is_refused_with_a_reason() {
    let harness = Harness::new();
    let (id, socket) = harness.start("cap");
    let _guard = SessionGuard {
        harness: &harness,
        id,
    };

    // Fill the subscriber table: every one of these stays attached.
    let mut attached = Vec::new();
    for n in 0..64 {
        let (mut stream, response) = attach(&socket, id, None, None);
        assert!(
            response.ok,
            "attach {n} below the cap was refused: {:?}",
            response.error
        );
        // Drain the initial payload so the worker's handshake completes.
        let payload = read_frame(&mut stream).unwrap().expect("initial payload");
        assert_eq!(payload.kind, FrameKind::Data);
        attached.push(stream);
    }

    let (_stream, response) = attach(&socket, id, None, None);
    assert!(
        !response.ok,
        "the 65th attach must be refused: {response:?}"
    );
    let error = response.error.expect("refusal carries its reason");
    assert!(
        error.contains("too many attached clients"),
        "refusal must name the cap, got: {error}"
    );
}

#[test]
fn attach_with_an_oversized_geometry_is_refused_with_a_reason() {
    let harness = Harness::new();
    let (id, socket) = harness.start("geometry");
    let _guard = SessionGuard {
        harness: &harness,
        id,
    };

    let (_stream, response) = attach(&socket, id, Some(u16::MAX), Some(u16::MAX));
    assert!(
        !response.ok,
        "an oversized geometry must be refused: {response:?}"
    );
    let error = response.error.expect("refusal carries its reason");
    assert!(
        error.contains("exceeds the maximum"),
        "refusal must name the geometry limit, got: {error}"
    );
}
