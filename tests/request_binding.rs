use aplexer::{
    atomic_write_json, frame_json, read_frame, write_json, Operation, Paths, Request, Response,
    SessionRecord,
};
use serde_json::Value;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
    sessions: Vec<String>,
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
            sessions: Vec::new(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("run aplexer CLI")
    }

    fn start(&mut self, workspace: &Path, tag: &str) -> SessionRecord {
        let output = self.run(&[
            "--json",
            "start",
            "--workspace",
            workspace.to_str().expect("UTF-8 workspace"),
            "--tag",
            tag,
            "--",
            "/bin/sleep",
            "300",
        ]);
        assert!(
            output.status.success(),
            "start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record = serde_json::from_slice::<SessionRecord>(&output.stdout)
            .expect("start returned record JSON");
        self.sessions.push(record.id.to_string());
        record
    }

    fn paths(&self) -> Paths {
        Paths {
            runtime_root: self.runtime.path().to_path_buf(),
            state_root: self.state.path().to_path_buf(),
            config_file: self.config.clone(),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for id in &self.sessions {
            let _ = self
                .command()
                .args(["kill", id, "--signal", "KILL", "--grace-ms", "0"])
                .output();
        }
    }
}

fn rpc(record: &SessionRecord, request: &Request) -> Response {
    let mut stream = UnixStream::connect(&record.socket_path).expect("connect worker socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write_json(&mut stream, request).expect("write request");
    frame_json(
        read_frame(&mut stream)
            .expect("read response")
            .expect("response frame"),
    )
    .expect("decode response")
}

#[test]
fn wrong_socket_metadata_and_mismatched_requests_cannot_route_to_another_session() {
    let mut harness = Harness::new();
    let first_workspace = TempDir::new().expect("first workspace");
    let second_workspace = TempDir::new().expect("second workspace");
    let first = harness.start(first_workspace.path(), "first");
    let second = harness.start(second_workspace.path(), "second");
    let paths = harness.paths();

    let mut displaced = first.clone();
    displaced.socket_path = second.socket_path.clone();
    atomic_write_json(&paths.record(first.id), &displaced).expect("write displaced socket path");
    let status = harness.run(&["status", &first.id.to_string()]);
    // Restore the registry before asserting so cleanup remains able to find
    // both live workers even if the assertion below fails.
    atomic_write_json(&paths.record(first.id), &first).expect("restore first record");
    assert!(!status.status.success(), "wrong-socket status succeeded");
    let error = String::from_utf8_lossy(&status.stderr);
    assert!(error.contains("contains socket path"), "{error}");
    assert!(error.contains("expected"), "{error}");

    let response = rpc(
        &second,
        &Request::new(
            first.id,
            Operation::Kill {
                signal: libc::SIGKILL,
                grace_ms: 0,
            },
        ),
    );
    assert!(!response.ok, "worker accepted another session's request");
    let error = response.error.expect("mismatch error");
    assert!(error.contains(&first.id.to_string()), "{error}");
    assert!(error.contains(&second.id.to_string()), "{error}");

    let mut unbound = Request::new(second.id, Operation::Status);
    unbound.session_id = None;
    let response = rpc(&second, &unbound);
    assert!(!response.ok, "worker accepted an unbound request");
    assert!(response
        .error
        .as_deref()
        .is_some_and(|error| error.contains("omitted session_id")));

    let status = harness.run(&["--json", "status", &second.id.to_string()]);
    assert!(
        status.status.success(),
        "second worker was affected: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).expect("status JSON");
    assert_eq!(status["worker_reachable"], true);
}
