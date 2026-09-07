//! Issue #9: a healthy mid-create session is `starting`, not `broken`.
//!
//! `start_session` persists `phase: starting, worker_pid: null` before the
//! worker registers, so `worker_alive` is false for tens of milliseconds of
//! every successful `a start`. These tests poll that record out of a live
//! start (and out of a start whose process group was SIGKILLed in the same
//! window) rather than synthesising one.

use aplexer::{
    atomic_write_json, now_ms, observed_state, Paths, SessionRecord, DEFAULT_STARTUP_TIMEOUT_MS,
};
use serde_json::Value;
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().expect("runtime tempdir");
        let state = TempDir::new().expect("state tempdir");
        let config = state.path().join("config.toml");
        Self {
            runtime,
            state,
            config,
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

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}): stdout={} stderr={}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "`a {}` did not print JSON ({error}): {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }
}

struct StartCleanup {
    child: Option<Child>,
    state: PathBuf,
}

impl Drop for StartCleanup {
    fn drop(&mut self) {
        kill_recorded_pids(&self.state);
        if let Some(mut child) = self.child.take() {
            let pid = child.id() as i32;
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
            let _ = child.wait();
        }
        kill_recorded_pids(&self.state);
    }
}

fn kill_recorded_pids(state: &Path) {
    let Ok(entries) = fs::read_dir(state.join("sessions")) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(bytes) = fs::read(entry.path().join("session.json")) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        for key in ["worker_pid", "workload_pid"] {
            if let Some(pid) = value[key].as_i64() {
                let pid = pid as i32;
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
}

fn spawn_start(harness: &Harness, workspace: &Path, tag: &str) -> Child {
    let mut command = harness.command();
    command
        .args([
            "start",
            "--workspace",
            workspace.to_str().expect("UTF-8 workspace"),
            "--tag",
            tag,
            "--",
            "/bin/sleep",
            "300",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().expect("spawn a start")
}

fn poll_pre_pid_record(state: &Path, timeout: Duration) -> Option<Value> {
    let sessions = state.join("sessions");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(entries) = fs::read_dir(&sessions) {
            for entry in entries.flatten() {
                let Ok(bytes) = fs::read(entry.path().join("session.json")) else {
                    continue;
                };
                let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
                    continue;
                };
                if value["phase"] == "starting" && value["worker_pid"].is_null() {
                    return Some(value);
                }
            }
        }
        thread::sleep(Duration::from_millis(1));
    }
    None
}

fn capture_pre_pid_record(kill_start: bool) -> Value {
    let workspace = TempDir::new().expect("workspace tempdir");
    for _ in 0..20 {
        let harness = Harness::new();
        let child = spawn_start(&harness, workspace.path(), "main");
        let mut cleanup = StartCleanup {
            child: Some(child),
            state: harness.state.path().to_path_buf(),
        };
        let Some(record) = poll_pre_pid_record(harness.state.path(), Duration::from_millis(500))
        else {
            continue;
        };
        assert_eq!(record["phase"], "starting", "{record}");
        assert!(record["worker_pid"].is_null(), "{record}");
        if kill_start {
            let child = cleanup.child.as_mut().expect("start child");
            let pid = child.id() as i32;
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
            let _ = child.wait();
            cleanup.child = None;
        }
        return record;
    }
    panic!("never polled a pre-PID Starting record out of a live `a start`");
}

fn without_identity_and_age(value: &Value) -> Value {
    let mut object = value
        .as_object()
        .unwrap_or_else(|| panic!("session record is not an object: {value}"))
        .clone();
    for key in [
        "id",
        "created_at_ms",
        "updated_at_ms",
        "socket_path",
        "history_path",
        "tag",
        "workspace",
        "cwd",
    ] {
        object.remove(key);
    }
    Value::Object(object)
}

fn plant(harness: &Harness, record: &mut SessionRecord) {
    let paths = Paths {
        runtime_root: harness.runtime.path().to_path_buf(),
        state_root: harness.state.path().to_path_buf(),
        config_file: harness.config.clone(),
    };
    paths.ensure().expect("aplexer paths");
    // The capture's socket/history paths name the original runtime/state
    // dirs; rewrite them so this registry will load the same record.
    record.socket_path = paths.socket(record.id);
    record.history_path = paths.history(record.id);
    atomic_write_json(&paths.record(record.id), record).expect("plant session.json");
}

fn doctor_sessions(harness: &Harness) -> Value {
    let output = harness.run(&["--json", "doctor"]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "doctor did not print JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    report["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == "sessions")
        .cloned()
        .expect("sessions check")
}

/// The two real captures are the same persisted shape: Starting, no worker
/// pid. Age (and the registry identity that comes with a second start) is
/// the only difference.
#[test]
fn captured_mid_create_and_crashed_start_records_differ_only_in_age() {
    let mid = capture_pre_pid_record(false);
    let crashed = capture_pre_pid_record(true);
    assert_eq!(
        without_identity_and_age(&mid),
        without_identity_and_age(&crashed),
        "mid-create={mid} crashed-start={crashed}"
    );
    assert_ne!(
        mid["id"], crashed["id"],
        "the two captures must be distinct starts, not the same bytes twice"
    );
}

#[test]
fn captured_mid_create_record_is_starting_not_broken() {
    let mid = capture_pre_pid_record(false);
    let record: SessionRecord = serde_json::from_value(mid.clone()).expect("session record");
    assert_eq!(
        observed_state(
            &record.phase,
            record.worker_alive(),
            record.created_at_ms,
            now_ms(),
        ),
        "starting",
        "{mid}"
    );

    let harness = Harness::new();
    // Re-stamp created_at so a slow capture retry cannot age the planted
    // fixture out of the startup window before we query it.
    let mut planted = record;
    planted.created_at_ms = now_ms();
    planted.updated_at_ms = planted.created_at_ms;
    plant(&harness, &mut planted);
    let status = harness.json(&["--json", "status", planted.id.to_string().as_str()]);
    assert_eq!(status["phase"], "starting", "{status}");
    assert_eq!(status["worker_alive"], false, "{status}");
    assert!(status["worker_pid"].is_null(), "{status}");
    assert_eq!(status["state"], "starting", "{status}");
    let listed = harness.json(&["--json", "list"]);
    let row = listed
        .as_array()
        .expect("list array")
        .iter()
        .find(|row| row["id"] == planted.id.to_string())
        .unwrap_or_else(|| panic!("list dropped the planted record: {listed}"));
    assert_eq!(row["state"], "starting", "{row}");
}

#[test]
fn captured_crashed_start_record_is_broken_once_past_the_startup_budget() {
    let crashed = capture_pre_pid_record(true);
    let mut record: SessionRecord =
        serde_json::from_value(crashed.clone()).expect("session record");
    // The captures differ only in age: rewind this one past the startup
    // budget and leave every other persisted field alone.
    record.created_at_ms = now_ms().saturating_sub(DEFAULT_STARTUP_TIMEOUT_MS + 1);
    record.updated_at_ms = record.created_at_ms;
    assert_eq!(
        observed_state(
            &record.phase,
            record.worker_alive(),
            record.created_at_ms,
            now_ms(),
        ),
        "broken",
        "{crashed}"
    );

    let harness = Harness::new();
    plant(&harness, &mut record);
    let status = harness.json(&["--json", "status", record.id.to_string().as_str()]);
    assert_eq!(status["phase"], "starting", "{status}");
    assert_eq!(status["worker_alive"], false, "{status}");
    assert!(status["worker_pid"].is_null(), "{status}");
    assert_eq!(status["state"], "broken", "{status}");
}

#[test]
fn doctor_does_not_list_a_captured_mid_create_session_as_broken() {
    let mid = capture_pre_pid_record(false);
    let harness = Harness::new();
    let mut planted: SessionRecord = serde_json::from_value(mid).expect("session record");
    planted.created_at_ms = now_ms();
    planted.updated_at_ms = planted.created_at_ms;
    plant(&harness, &mut planted);
    let sessions = doctor_sessions(&harness);
    assert_eq!(sessions["ok"], true, "{sessions}");
    assert!(
        sessions["broken_sessions"]
            .as_array()
            .is_some_and(|rows| rows.is_empty()),
        "doctor listed a mid-create session as broken: {sessions}"
    );
}

/// Issue #9's user-facing half, end to end. `a attach` on a real mid-create
/// record used to answer "the worker's runtime directory was removed out
/// from under it -- run `a kill`" for a session that was simply still
/// binding its control socket. Past the startup budget that same record is a
/// crashed start and the reclaim advice is correct again.
#[test]
fn attach_to_a_captured_mid_create_session_does_not_advise_killing_it() {
    let mid = capture_pre_pid_record(false);
    let record: SessionRecord = serde_json::from_value(mid).expect("session record");

    let harness = Harness::new();
    let mut planted = record.clone();
    planted.created_at_ms = now_ms();
    planted.updated_at_ms = planted.created_at_ms;
    plant(&harness, &mut planted);
    let id = planted.id.to_string();
    let output = harness.run(&["attach", id.as_str()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("still starting"), "{stderr}");
    assert!(
        !stderr.contains("a kill"),
        "attach told the user to kill a healthy starting session: {stderr}"
    );
    assert!(
        !stderr.contains("removed out from under it"),
        "attach blamed a destroyed runtime directory for the startup race: {stderr}"
    );

    let harness = Harness::new();
    let mut expired = record;
    expired.created_at_ms = now_ms().saturating_sub(DEFAULT_STARTUP_TIMEOUT_MS + 1);
    expired.updated_at_ms = expired.created_at_ms;
    plant(&harness, &mut expired);
    let id = expired.id.to_string();
    let output = harness.run(&["attach", id.as_str()]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("worker is not running"), "{stderr}");
    assert!(stderr.contains("state: broken"), "{stderr}");
    assert!(stderr.contains("a kill"), "{stderr}");
}
