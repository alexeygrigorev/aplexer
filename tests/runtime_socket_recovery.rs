use serde_json::Value;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
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
        run_with_timeout(command, Duration::from_secs(5))
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
        Ok(Err(error)) => panic!("wait for command: {error}"),
        Err(_) => {
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            panic!("command {pid} exceeded {timeout:?}");
        }
    }
}

#[test]
fn live_worker_recreates_deleted_private_runtime_socket() {
    let mut harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let start = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "socket-recovery",
        "--",
        "/bin/bash",
        "--norc",
    ]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    let record: Value = serde_json::from_slice(&start.stdout).unwrap();
    let id = record["id"].as_str().unwrap().to_string();
    let socket = PathBuf::from(record["socket_path"].as_str().unwrap());
    let runtime_session = socket.parent().unwrap().to_path_buf();
    let durable_session = harness.state.path().join("sessions").join(&id);
    let identity_path = durable_session.join("worker.identity.json");
    let identity_before = fs::read(&identity_path).unwrap();
    harness.id = Some(id.clone());

    // A healthy listener must remain published under the exact same socket
    // node across several idle health checks. Linux gives the open listener
    // fd and its filesystem pathname different inode numbers, so comparing
    // fstat(listener) with lstat(path) falsely treats every healthy socket as
    // displaced and continuously rebinds it.
    let healthy_metadata = fs::symlink_metadata(&socket).unwrap();
    let healthy_identity = (healthy_metadata.dev(), healthy_metadata.ino());
    thread::sleep(Duration::from_millis(1_200));
    for _ in 0..3 {
        let status = harness.run(&["status", &id, "--json"]);
        assert!(status.status.success(), "healthy status RPC failed");
    }
    let still_healthy = fs::symlink_metadata(&socket).unwrap();
    assert_eq!(
        (still_healthy.dev(), still_healthy.ino()),
        healthy_identity,
        "idle health checks replaced an untouched control socket"
    );

    fs::remove_dir_all(&runtime_session).unwrap();

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let status = harness.run(&["status", &id, "--json"]);
        if socket.exists()
            && runtime_session.join("worker.lock").exists()
            && status.status.success()
        {
            let value: Value = serde_json::from_slice(&status.stdout).unwrap();
            if value["worker_alive"] == true {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "worker did not recover its deleted control socket; stderr={:?}",
            String::from_utf8_lossy(&status.stderr)
        );
        thread::sleep(Duration::from_millis(50));
    }

    let directory_mode = fs::metadata(&runtime_session).unwrap().permissions().mode() & 0o777;
    assert_eq!(directory_mode, 0o700);
    let lock_mode = fs::metadata(runtime_session.join("worker.lock"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(lock_mode, 0o600);
    let socket_metadata = fs::symlink_metadata(&socket).unwrap();
    assert!(socket_metadata.file_type().is_socket());
    assert_eq!(socket_metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(fs::read(&identity_path).unwrap(), identity_before);

    let marker = "runtime-socket-recovered";
    let send = harness.run(&["send", &id, &format!("echo {marker}"), "--enter"]);
    assert!(
        send.status.success(),
        "send after recovery failed: {}",
        String::from_utf8_lossy(&send.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let capture = harness.run(&["capture", &id, "--bytes", "4096"]);
        if String::from_utf8_lossy(&capture.stdout).contains(marker) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "shell did not respond after recovery"
        );
        thread::sleep(Duration::from_millis(25));
    }

    // Losing only the pathname must also heal. In this case the original
    // worker.lock inode is still held by this worker, so recovery must retain
    // it rather than deadlocking against its own flock.
    fs::remove_file(&socket).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "socket-only deletion did not heal"
        );
        thread::sleep(Duration::from_millis(25));
    }
    let second_marker = "runtime-socket-recovered-again";
    let send = harness.run(&["send", &id, &format!("echo {second_marker}"), "--enter"]);
    assert!(
        send.status.success(),
        "send after socket-only recovery failed: {}",
        String::from_utf8_lossy(&send.stderr)
    );

    // The durable identity is the authorization boundary for republishing
    // reachability. A mismatch must leave the socket missing even though the
    // worker process itself is still alive.
    let mut mismatched_identity: Value = serde_json::from_slice(&identity_before).unwrap();
    mismatched_identity["pid"] = Value::from(1);
    fs::write(
        &identity_path,
        serde_json::to_vec(&mismatched_identity).unwrap(),
    )
    .unwrap();
    fs::remove_file(&socket).unwrap();
    thread::sleep(Duration::from_millis(1_200));
    assert!(
        !socket.exists(),
        "worker republished a socket with mismatched durable identity proof"
    );

    // Restore the exact trusted evidence so the worker can heal once more and
    // the harness can perform normal contained cleanup.
    fs::write(&identity_path, &identity_before).unwrap();
    fs::set_permissions(&identity_path, fs::Permissions::from_mode(0o600)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(8);
    while !socket.exists() {
        assert!(
            Instant::now() < deadline,
            "socket did not recover after identity proof was restored"
        );
        thread::sleep(Duration::from_millis(25));
    }
}
