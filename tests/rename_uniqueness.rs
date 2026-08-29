use aplexer::{
    frame_json, list_records, read_frame, write_json, Operation, Paths, Request, Response,
};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{mpsc, Arc, Barrier};
use std::thread;
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
        let mut command = self.command();
        command.args(args);
        run_with_timeout(command, Duration::from_secs(10))
    }

    fn start(&mut self, workspace: &Path, tag: &str) -> aplexer::SessionRecord {
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
        let record = serde_json::from_slice::<aplexer::SessionRecord>(&output.stdout)
            .expect("start record JSON");
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

fn direct_rename(record: aplexer::SessionRecord, workspace: PathBuf, tag: &str) -> Response {
    let mut stream = UnixStream::connect(&record.socket_path).expect("connect worker socket");
    write_json(
        &mut stream,
        &Request::new(Operation::Rename {
            workspace,
            tag: tag.to_string(),
        }),
    )
    .expect("write direct rename RPC");
    frame_json(
        read_frame(&mut stream)
            .expect("read response")
            .expect("response frame"),
    )
    .expect("decode response")
}

#[test]
fn concurrent_direct_renames_cannot_claim_the_same_workspace_and_tag() {
    let mut harness = Harness::new();
    let first_workspace = TempDir::new().expect("first workspace");
    let second_workspace = TempDir::new().expect("second workspace");
    let target_workspace = TempDir::new().expect("target workspace");
    let first = harness.start(first_workspace.path(), "first");
    let second = harness.start(second_workspace.path(), "second");

    let barrier = Arc::new(Barrier::new(3));
    let spawn_rename = |record: aplexer::SessionRecord| {
        let barrier = Arc::clone(&barrier);
        let target = target_workspace.path().to_path_buf();
        thread::spawn(move || {
            barrier.wait();
            direct_rename(record, target, "shared")
        })
    };
    let first_rename = spawn_rename(first);
    let second_rename = spawn_rename(second);
    barrier.wait();
    let responses = [
        first_rename.join().expect("first rename thread"),
        second_rename.join().expect("second rename thread"),
    ];

    assert_eq!(responses.iter().filter(|response| response.ok).count(), 1);
    let rejected = responses
        .iter()
        .find(|response| !response.ok)
        .expect("one rename must be rejected");
    assert!(
        rejected
            .error
            .as_deref()
            .is_some_and(|error| error.contains("workspace+tag already belongs")),
        "unexpected direct-RPC rejection: {:?}",
        rejected.error
    );
    let target = target_workspace.path().canonicalize().unwrap();
    let claimed = list_records(&harness.paths())
        .unwrap()
        .into_iter()
        .filter(|record| record.workspace == target && record.tag == "shared")
        .count();
    assert_eq!(claimed, 1, "registry contains a duplicate identity");
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Output {
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
            panic!("command pid {pid} exceeded {timeout:?}");
        }
    }
}
