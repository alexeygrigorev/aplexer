//! End-to-end proof that `a list --json` / `a snapshot` / `a status --json`
//! name the agent actually running inside a session.
//!
//! Every pocketshell session is `engine: "shell"` with the agent launched by
//! hand inside it, so the answer cannot come from configuration -- it has to
//! come from the live workload process tree. These tests exercise that path
//! against a real session and a real `/proc`:
//!
//! * `agent_appears_and_clears_as_a_fake_claude_runs_inside_a_shell_session`
//!   starts `/bin/bash -l`, types a fake `claude` script into it, and watches
//!   the reported `agent` go `null` -> `"claude"` -> `null` as the script
//!   starts and exits. That is the acceptance scenario from issue #2580.
//! * `a_terminal_record_never_reports_an_agent_from_a_recycled_pid` pins the
//!   staleness guard: an exited record whose `workload_pid` now names some
//!   unrelated live agent process must report `null`, while the same pid on a
//!   running record reports the agent.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use aplexer::{atomic_write_json, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION};
use serde_json::Value;
use tempfile::TempDir;
use uuid::Uuid;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
    workspace: TempDir,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().expect("runtime tempdir");
        let state = TempDir::new().expect("state tempdir");
        let config = state.path().join("config.toml");
        let workspace = TempDir::new().expect("workspace tempdir");
        Self {
            runtime,
            state,
            config,
            workspace,
        }
    }

    fn paths(&self) -> Paths {
        Paths {
            runtime_root: self.runtime.path().to_path_buf(),
            state_root: self.state.path().to_path_buf(),
            config_file: self.config.clone(),
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

    /// The row for `id` from `a list --json`.
    fn list_row(&self, id: &str) -> Value {
        let stdout = self.run_ok(&["list", "--json"], Duration::from_secs(10));
        let rows: Value = serde_json::from_str(&stdout).expect("list JSON");
        rows.as_array()
            .expect("list JSON is an array")
            .iter()
            .find(|row| row["id"] == Value::String(id.to_owned()))
            .unwrap_or_else(|| panic!("no list row for session {id}: {stdout}"))
            .clone()
    }

    /// The `agent` field as `a list --json` reports it. Deliberately reads
    /// the raw `Value` so a MISSING key is distinguishable from `null`.
    fn list_agent(&self, id: &str) -> Value {
        let row = self.list_row(id);
        assert!(
            row.get("agent").is_some(),
            "`a list --json` row has no `agent` key: {row}"
        );
        row["agent"].clone()
    }

    fn snapshot_agent(&self, id: &str) -> Value {
        let stdout = self.run_ok(&["snapshot"], Duration::from_secs(10));
        let rows: Value = serde_json::from_str(&stdout).expect("snapshot JSON");
        let row = rows
            .as_array()
            .expect("snapshot JSON is an array")
            .iter()
            .find(|row| row["id"] == Value::String(id.to_owned()))
            .unwrap_or_else(|| panic!("no snapshot row for session {id}"))
            .clone();
        assert!(
            row.get("agent").is_some(),
            "`a snapshot` row has no `agent` key: {row}"
        );
        row["agent"].clone()
    }

    fn status_agent(&self, id: &str) -> Value {
        let stdout = self.run_ok(&["status", id, "--json"], Duration::from_secs(10));
        let value: Value = serde_json::from_str(&stdout).expect("status JSON");
        assert!(
            value.get("agent").is_some(),
            "`a status --json` has no `agent` key: {value}"
        );
        value["agent"].clone()
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

fn wait_until(mut condition: impl FnMut() -> bool, description: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// A shell script named `claude` that keeps running until `sentinel` is
/// removed -- a stand-in for the real CLI (whose comm/cmdline is what
/// detection reads), with no network, no config, and a deterministic exit.
///
/// Both call sites run it as `/bin/sh <script> …` rather than exec'ing the
/// freshly written file. Executing a file this process just created races
/// with any concurrent `fork` in the test binary: the child inherits the
/// still-open write descriptor, and the exec fails with `ETXTBSY` ("Text
/// file busy") -- roughly 1 run in 78 on a busy box. Going through
/// `/bin/sh` removes the race entirely: `sh` only ever *reads* the script.
/// Detection is unaffected, and in fact better exercised -- `comm` is then
/// `sh`, so the `claude` token has to be classified from the cmdline
/// (`/bin/sh /…/bin/claude …`), which is the node-wrapped shape real agents
/// present.
fn write_fake_claude(dir: &Path) -> PathBuf {
    let bin = dir.join("bin");
    fs::create_dir_all(&bin).expect("create fake bin dir");
    let script = bin.join("claude");
    fs::write(
        &script,
        "#!/bin/sh\n\
         # Fake `claude` for aplexer agent-detection tests.\n\
         echo running > \"$1\"\n\
         while [ -e \"$2\" ]; do /bin/sleep 0.05; done\n",
    )
    .expect("write fake claude");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod fake claude");
    script
}

fn wait_for_file(path: &Path, description: &str) {
    wait_until(|| path.exists(), description);
}

#[test]
fn agent_appears_and_clears_as_a_fake_claude_runs_inside_a_shell_session() {
    assert!(
        Path::new("/bin/bash").exists(),
        "/bin/bash is required by this test"
    );
    let harness = Harness::new();
    let workspace = harness
        .workspace
        .path()
        .to_str()
        .expect("utf8 workspace")
        .to_owned();
    let script = write_fake_claude(harness.workspace.path());
    let ready = harness.workspace.path().join("claude.ready");
    let sentinel = harness.workspace.path().join("claude.keep-running");
    fs::write(&sentinel, b"run").expect("write sentinel");

    // A login bash with the user's dotfiles suppressed: the session must be a
    // plain shell whose only agent is the one this test starts inside it.
    let home = format!("HOME={workspace}");
    let stdout = harness.run_ok(
        &[
            "start",
            "--workspace",
            &workspace,
            "--tag",
            "agent-detect",
            "--env",
            &home,
            "--json",
            "--",
            "/bin/bash",
            "--noprofile",
            "--norc",
            "-l",
        ],
        Duration::from_secs(20),
    );
    let started: Value = serde_json::from_str(&stdout).expect("start JSON");
    let id = started["id"].as_str().expect("session id").to_owned();

    // A bare shell session: the key is present and explicitly null.
    wait_until(
        || harness.list_agent(&id) == Value::Null,
        "a bare shell session to report agent: null",
    );

    // `/bin/sh <script>` for the same ETXTBSY reason as `write_fake_claude`
    // documents: the session's shell must not exec a file this test process
    // may still hold a write descriptor for in a forked child.
    let command = format!(
        "/bin/sh {} {} {}",
        script.display(),
        ready.display(),
        sentinel.display()
    );
    harness.run_ok(&["send", &id, &command, "--enter"], Duration::from_secs(10));
    wait_for_file(&ready, "the fake claude to start inside the session");

    wait_until(
        || harness.list_agent(&id) == Value::String("claude".into()),
        "`a list --json` to report agent: \"claude\"",
    );
    assert_eq!(harness.snapshot_agent(&id), Value::String("claude".into()));
    assert_eq!(harness.status_agent(&id), Value::String("claude".into()));

    // The agent exits; the shell session lives on and must stop claiming it.
    fs::remove_file(&sentinel).expect("remove sentinel");
    wait_until(
        || harness.list_agent(&id) == Value::Null,
        "`a list --json` to report agent: null after the fake claude exits",
    );
    assert_eq!(harness.snapshot_agent(&id), Value::Null);
    assert_eq!(harness.status_agent(&id), Value::Null);

    harness.run_ok(
        &["kill", &id, "--signal", "TERM", "--grace-ms", "200"],
        Duration::from_secs(15),
    );
}

/// Kill a spawned helper even if an assertion unwinds the test.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn a_terminal_record_never_reports_an_agent_from_a_recycled_pid() {
    let harness = Harness::new();
    let paths = harness.paths();
    paths.ensure().expect("ensure paths");
    let script = write_fake_claude(harness.workspace.path());
    let ready = harness.workspace.path().join("claude.ready");
    let sentinel = harness.workspace.path().join("claude.keep-running");
    fs::write(&sentinel, b"run").expect("write sentinel");

    // A live process that a `/proc` walk classifies as claude, standing in
    // for whatever a recycled pid could point at after a session ended.
    let child = Command::new("/bin/sh")
        .arg(&script)
        .arg(&ready)
        .arg(&sentinel)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn fake claude");
    let workload_pid = child.id();
    let _guard = ChildGuard(child);
    wait_for_file(&ready, "the standalone fake claude to start");

    for (phase, expected) in [
        (Phase::Running, Value::String("claude".into())),
        (Phase::Exited, Value::Null),
    ] {
        let id = Uuid::now_v7();
        let record = SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id,
            workspace: harness.workspace.path().to_path_buf(),
            tag: format!("agent-{id}"),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/bash".into()],
            cwd: harness.workspace.path().to_path_buf(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: 4096,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase,
            worker_pid: None,
            workload_pid: Some(workload_pid),
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: None,
            socket_path: paths.socket(id),
            history_path: paths.history(id),
            exit: None,
            error: None,
        };
        fs::create_dir_all(paths.state_session(id)).expect("session dir");
        fs::write(&record.history_path, b"").expect("history file");
        atomic_write_json(&paths.record(id), &record).expect("write record");

        assert_eq!(
            harness.list_agent(&id.to_string()),
            expected,
            "record in phase {:?} reported the wrong agent",
            record.phase,
        );
    }
}
