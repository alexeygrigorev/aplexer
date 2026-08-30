use serde_json::Value;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
    id: Option<String>,
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
            id: None,
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
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            let _ = self
                .command()
                .args(["kill", id, "--signal", "KILL", "--grace-ms", "0"])
                .output();
        }
    }
}

#[test]
fn worker_recovers_after_accept_hits_file_descriptor_limit() {
    let mut harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let mut start = harness.command();
    start.args([
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "fd-pressure",
        "--",
        "/bin/bash",
        "--norc",
    ]);
    unsafe {
        start.pre_exec(|| {
            let limit = libc::rlimit {
                rlim_cur: 48,
                rlim_max: 48,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = run_with_timeout(start, Duration::from_secs(15));
    assert!(
        output.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = record["id"].as_str().unwrap().to_string();
    let socket = PathBuf::from(record["socket_path"].as_str().unwrap());
    harness.id = Some(id.clone());

    // Each silent connection consumes a worker descriptor and a blocked
    // request thread. Exceed the inherited limit, then release everything so
    // a retrying accept loop can recover.
    let mut held = Vec::new();
    for _ in 0..64 {
        held.push(UnixStream::connect(&socket).expect("queue pressure connection"));
    }
    thread::sleep(Duration::from_millis(200));
    drop(held);

    let marker = "fd-pressure-recovered";
    let send = harness.run(&["send", &id, &format!("echo {marker}"), "--enter"]);
    assert!(
        send.status.success(),
        "worker did not recover: {}",
        String::from_utf8_lossy(&send.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let capture = harness.run(&["capture", &id, "--bytes", "4096"]);
        if String::from_utf8_lossy(&capture.stdout).contains(marker) {
            break;
        }
        assert!(Instant::now() < deadline, "shell stopped responding after EMFILE");
        thread::sleep(Duration::from_millis(50));
    }
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("wait command: {error}"),
        Err(_) => {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            panic!("command {pid} exceeded {timeout:?}");
        }
    }
}
