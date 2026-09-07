//! Workspace ordering for `a list --sort` and `last_accessed_ms` on attach.
//!
//! Human `a list` numbers workspaces in the chosen order, and `a N` uses
//! the same remembered sort, so this file pins: name / created / accessed
//! actually reorder the tree, attach stamps `last_accessed_ms`, and a later
//! bare `a list` keeps the last `--sort`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime_dir.path())
            .env("APLEXER_STATE_DIR", self.state_dir.path())
            .env("APLEXER_CONFIG", &self.config_file);
        command
    }

    fn run(&self, args: &[&str], timeout: Duration) -> std::process::Output {
        let mut command = self.command();
        command.args(args);
        run_with_timeout(command, timeout)
    }

    fn run_ok(&self, args: &[&str], timeout: Duration) -> String {
        let output = self.run(args, timeout);
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

    fn start(&self, workspace: &Path, tag: &str) -> String {
        let stdout = self.run_ok(
            &[
                "--json",
                "start",
                "--workspace",
                workspace.to_str().unwrap(),
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
        value["id"].as_str().expect("session id").to_string()
    }

    fn status(&self, id: &str) -> Value {
        let stdout = self.run_ok(&["status", id, "--json"], Duration::from_secs(5));
        serde_json::from_str(&stdout).expect("status JSON")
    }

    fn attach_and_detach(&self, id: &str) {
        let mut command = self.command();
        command.args(["attach", id]).stdin(Stdio::null());
        let output = run_with_timeout(command, Duration::from_secs(10));
        assert!(
            output.status.success(),
            "`a attach {id}` failed (status {:?}):\nstdout: {}\nstderr: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn list_workspace_order(&self, extra: &[&str]) -> Vec<String> {
        let mut args = vec!["list"];
        args.extend_from_slice(extra);
        let stdout = self.run_ok(&args, Duration::from_secs(5));
        stdout
            .lines()
            .filter_map(|line| {
                // `[N] /path (...)` -- the numbered workspace headers.
                let rest = line.strip_prefix('[')?;
                let (_index, rest) = rest.split_once(']')?;
                let rest = rest.trim_start();
                let path = rest.split(" (").next()?.trim();
                if path.is_empty() {
                    None
                } else {
                    Some(path.to_string())
                }
            })
            .collect()
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

#[test]
fn list_sorts_workspaces_and_attach_stamps_last_accessed() {
    let harness = Harness::new();
    let ws_apple = harness.runtime_dir.path().join("apple");
    let ws_zebra = harness.runtime_dir.path().join("zebra");
    std::fs::create_dir_all(&ws_apple).unwrap();
    std::fs::create_dir_all(&ws_zebra).unwrap();

    // Start zebra first so created-at order is the reverse of name order.
    let zebra_id = harness.start(&ws_zebra, "main");
    thread::sleep(Duration::from_millis(50));
    let apple_id = harness.start(&ws_apple, "main");

    let apple = ws_apple.to_string_lossy().into_owned();
    let zebra = ws_zebra.to_string_lossy().into_owned();

    assert_eq!(
        harness.list_workspace_order(&["--sort", "name"]),
        vec![apple.clone(), zebra.clone()],
        "name sort is alphabetical"
    );
    assert_eq!(
        harness.list_workspace_order(&["--sort", "created"]),
        vec![apple.clone(), zebra.clone()],
        "created sort is newest session first"
    );

    let before = harness.status(&zebra_id);
    assert!(
        before.get("last_accessed_ms").is_none() || before["last_accessed_ms"].is_null(),
        "unattached session has no last_accessed_ms: {before}"
    );

    harness.attach_and_detach(&zebra_id);

    let after = harness.status(&zebra_id);
    assert!(
        after["last_accessed_ms"].as_u64().is_some_and(|ms| ms > 0),
        "attach must stamp last_accessed_ms: {after}"
    );
    assert!(
        harness.status(&apple_id).get("last_accessed_ms").is_none()
            || harness.status(&apple_id)["last_accessed_ms"].is_null(),
        "unattached sibling stays unstamped"
    );

    assert_eq!(
        harness.list_workspace_order(&["--sort", "accessed"]),
        vec![zebra.clone(), apple.clone()],
        "accessed sort puts the just-opened workspace first"
    );
    assert_eq!(
        harness.list_workspace_order(&[]),
        vec![zebra, apple],
        "bare `a list` remembers --sort accessed"
    );

    let _ = harness.run_ok(&["kill", &zebra_id], Duration::from_secs(10));
    let _ = harness.run_ok(&["kill", &apple_id], Duration::from_secs(10));
}
