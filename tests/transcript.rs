// CLI tests for `a transcript`: last-N / before / after pagination,
// `$APLEXER_SESSION_ID` addressing, and the bind sidecar. Isolated
// APLEXER_* dirs + HOME so this never touches a real user's sessions
// or engine logs.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

struct Harness {
    runtime_dir: TempDir,
    state_dir: TempDir,
    home: TempDir,
    config_file: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime_dir = TempDir::new().expect("runtime tempdir");
        let state_dir = TempDir::new().expect("state tempdir");
        let home = TempDir::new().expect("home tempdir");
        let config_file = runtime_dir.path().join("config.toml");
        Self {
            runtime_dir,
            state_dir,
            home,
            config_file,
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_a"));
        cmd.env("APLEXER_RUNTIME_DIR", self.runtime_dir.path());
        cmd.env("APLEXER_STATE_DIR", self.state_dir.path());
        cmd.env("APLEXER_CONFIG", &self.config_file);
        cmd.env("HOME", self.home.path());
        cmd.env_remove("APLEXER_SESSION_ID");
        cmd
    }

    fn run(&self, args: &[&str], timeout: Duration) -> std::process::Output {
        let mut cmd = self.command();
        cmd.args(args);
        run_with_timeout(cmd, timeout)
    }

    fn run_ok(&self, args: &[&str]) -> String {
        let output = self.run(args, Duration::from_secs(5));
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::process::Output {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let child = cmd.spawn().expect("spawn a");
    let (tx, rx) = mpsc::channel();
    let pid_hint = child.id();
    thread::spawn(move || {
        let output = child.wait_with_output();
        let _ = tx.send(output);
    });
    match rx.recv_timeout(timeout) {
        Ok(output) => output.expect("wait a"),
        Err(_) => {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid_hint.to_string()])
                .status();
            panic!("`a` timed out after {timeout:?}");
        }
    }
}

fn write_session(h: &Harness, id: &str, cwd: &Path, engine: &str) {
    let session_dir = h.state_dir.path().join("sessions").join(id);
    fs::create_dir_all(&session_dir).unwrap();
    let record = json!({
        "schema_version": 1,
        "id": id,
        "workspace": cwd.display().to_string(),
        "tag": "review",
        "engine": engine,
        "command": [engine],
        "cwd": cwd.display().to_string(),
        "env": {},
        "env_unset": [],
        "limits": {},
        "history_bytes": 0,
        "created_at_ms": 1_000,
        "updated_at_ms": 1_000,
        "phase": "running",
        "socket_path": "/tmp/s",
        "history_path": "/tmp/h",
    });
    fs::write(
        session_dir.join("session.json"),
        serde_json::to_vec_pretty(&record).unwrap(),
    )
    .unwrap();
}

fn write_claude_log(h: &Harness, cwd: &Path, body: &str) -> PathBuf {
    let encoded = cwd.display().to_string().replace(['/', '.'], "-");
    let dir = h.home.path().join(".claude/projects").join(encoded);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("abc.jsonl");
    fs::write(&path, body).unwrap();
    path
}

fn parse_jsonl(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|l| !l.is_empty() && l.starts_with('{'))
        .map(|l| serde_json::from_str(l).expect(l))
        .collect()
}

#[test]
fn transcript_last_and_whoami() {
    let h = Harness::new();
    let cwd = h.home.path().join("proj");
    fs::create_dir_all(&cwd).unwrap();
    let id = "00000000-0000-0000-0000-000000000001";
    write_session(&h, id, &cwd, "claude");
    write_claude_log(
        &h,
        &cwd,
        concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"one"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"two"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"three"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"four"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"five"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"six"}]}}"#,
            "\n",
        ),
    );

    let cwd_str = cwd.display().to_string();
    let stdout = h.run_ok(&[
        "--json",
        "transcript",
        "--workspace",
        &cwd_str,
        "--tag",
        "review",
        "--last",
        "5",
    ]);
    let events = parse_jsonl(&stdout);
    assert_eq!(events.len(), 5, "{stdout}");
    assert_eq!(events[0]["content"], "two");
    assert_eq!(events[4]["content"], "six");
    assert_eq!(events[0]["sequence"], 1);
    assert_eq!(events[4]["metadata"]["session_id"], id);
    assert_eq!(events[4]["metadata"]["tag"], "review");

    let older = h.run_ok(&[
        "--json",
        "transcript",
        "--workspace",
        &cwd_str,
        "--tag",
        "review",
        "--before",
        "4",
        "--last",
        "2",
        "--kind",
        "message",
    ]);
    let events = parse_jsonl(&older);
    assert_eq!(
        events
            .iter()
            .map(|e| e["content"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["three", "four"]
    );

    let after = h.run_ok(&[
        "--json",
        "transcript",
        "--workspace",
        &cwd_str,
        "--tag",
        "review",
        "--after",
        "4",
    ]);
    let events = parse_jsonl(&after);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["content"], "six");

    let mut cmd = h.command();
    cmd.env("APLEXER_SESSION_ID", id);
    cmd.args(["--json", "transcript", "--last", "1"]);
    let output = run_with_timeout(cmd, Duration::from_secs(5));
    assert!(output.status.success(), "{:?}", output);
    let events = parse_jsonl(&String::from_utf8_lossy(&output.stdout));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["content"], "six");

    let bind = h
        .state_dir
        .path()
        .join("sessions")
        .join(id)
        .join("transcript.json");
    assert!(bind.is_file(), "expected bind sidecar at {}", bind.display());
}

#[test]
fn whoami_inside_session_survives_cleared_env() {
    let h = Harness::new();
    let ws = h.home.path().join("proj");
    fs::create_dir_all(&ws).unwrap();
    let ws_str = ws.display().to_string();
    let stdout = {
        let output = h.run(
            &[
                "start",
                "--json",
                "--workspace",
                &ws_str,
                "--cwd",
                &ws_str,
                "--tag",
                "me",
                "--engine",
                "shell",
                "--startup-timeout-ms",
                "15000",
                "--",
                "/bin/bash",
                "--norc",
                "-i",
            ],
            Duration::from_secs(20),
        );
        assert!(
            output.status.success(),
            "start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    };
    let record: Value = serde_json::from_str(&stdout).expect("start json");
    let id = record["id"].as_str().expect("id").to_string();

    h.run_ok(&[
        "send",
        &id,
        "--enter",
        // Keep the completion marker out of the echoed input line; otherwise
        // capture() can mistake the command itself for its output and the
        // following kill races the whoami subprocess.
        "env -u APLEXER_SESSION_ID a whoami; printf 'WHOAMI_'; printf 'DONE\\n'",
    ]);
    h.run_ok(&["send", &id, "--hex", "0d"]);
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let mut captured = String::new();
    while std::time::Instant::now() < deadline {
        let output = h.run(&["capture", &id, "--bytes", "4000"], Duration::from_secs(5));
        captured = String::from_utf8_lossy(&output.stdout).into_owned();
        if captured.contains("WHOAMI_DONE") {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = h.run(&["kill", &id], Duration::from_secs(8));
    assert!(
        captured.contains(&id),
        "whoami inside session should print {id} even with APLEXER_SESSION_ID unset; capture:\n{captured}"
    );
    assert!(
        captured.contains("engine: shell"),
        "whoami should report engine; capture:\n{captured}"
    );
}

#[test]
fn exec_subcommand_is_gone() {
    let h = Harness::new();
    let output = h.run(&["exec", "--help"], Duration::from_secs(5));
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("unrecognized subcommand") || err.contains("unexpected"),
        "stderr: {err}"
    );
}
