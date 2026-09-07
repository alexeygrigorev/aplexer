//! `--fresh` starts -- `a new`'s "always creates" backend (product ask: add
//! another session to a workspace whose tree already has one).
//!
//! `a start` keys a session by `workspace+tag`, so a workspace with a live
//! `main` had no one-word way to say "start another one here": the exact-tag
//! start is refused, and `a here`/`a -` would just reattach. `--fresh` makes
//! the live holder a reason to move to the next free `<tag>-2` suffix rather
//! than an error, decided under the registry lock so two racing fresh starts
//! cannot claim the same suffix.
//!
//! The safety property from tests/reclaim_zombie_tag.rs is inherited, not
//! reimplemented: `--fresh` only bypasses the *error*, never the *bar* --
//! a holder that would be refused is skipped to the next suffix, and one
//! that would be reclaimed is reclaimed under its own name.
//!
//! Harness style follows tests/reclaim_zombie_tag.rs (direct CLI, real
//! sessions, real signals).

use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
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

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            .args(args);
        command
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.command(args).output().expect("run aplexer CLI");
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

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().expect("run aplexer CLI");
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}): stdout={} stderr={}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn start(&self, workspace: &TempDir, tag: &str, extra: &[&str]) -> Value {
        let mut args = vec![
            "--json",
            "start",
            "--workspace",
            workspace.path().to_str().expect("UTF-8 workspace"),
            "--tag",
            tag,
        ];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--", "/bin/sleep", "300"]);
        self.json(&args)
    }

    /// Live (worker-alive, non-terminal phase) rows for one workspace, sorted
    /// by tag so positional assertions read in tag order.
    fn live_rows(&self, workspace: &TempDir) -> Vec<Value> {
        let target = workspace
            .path()
            .canonicalize()
            .expect("canonical workspace")
            .to_str()
            .expect("UTF-8 workspace")
            .to_string();
        let mut rows: Vec<Value> = self
            .json(&["--json", "list"])
            .as_array()
            .expect("snapshot array")
            .iter()
            .filter(|row| {
                row["workspace"] == target
                    && row["worker_alive"] == true
                    && row["phase"] != "exited"
                    && row["phase"] != "failed"
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| a["tag"].as_str().unwrap().cmp(b["tag"].as_str().unwrap()));
        rows
    }

    fn tags(&self, workspace: &TempDir) -> Vec<String> {
        self.live_rows(workspace)
            .iter()
            .map(|row| row["tag"].as_str().expect("tag").to_string())
            .collect()
    }

    fn kill_ok(&self, id: &str) {
        self.run_ok(&["kill", id, "--signal", "KILL", "--grace-ms", "0"]);
    }
}

/// Kills anything a failing test may have orphaned, so an assertion failure
/// cannot leave a `sleep 300` behind on the box.
struct ProcessCleanup(Vec<i32>);

impl Drop for ProcessCleanup {
    fn drop(&mut self) {
        for pid in self.0.drain(..) {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

fn cleanup_handles(rows: &[Value]) -> ProcessCleanup {
    ProcessCleanup(
        rows.iter()
            .flat_map(|row| {
                [
                    row["worker_pid"].as_i64().unwrap_or(0) as i32,
                    row["workload_pid"].as_i64().unwrap_or(0) as i32,
                ]
            })
            .collect(),
    )
}

/// `a new` always attaches, so like the `a -` harness in
/// reclaim_zombie_tag.rs it needs stdin at EOF: the attach relay returns on
/// its own instead of wedging the suite (bounded at 30s regardless).
fn run_attaching(mut command: Command) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn aplexer CLI");
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let out_reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout.read_to_end(&mut buffer);
        buffer
    });
    let err_reader = thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr.read_to_end(&mut buffer);
        buffer
    });
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait());
    });
    let status = match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(status) => status.expect("wait for aplexer CLI"),
        Err(_) => {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            panic!("the attaching command did not return within 30s");
        }
    };
    Output {
        status,
        stdout: out_reader.join().expect("stdout reader"),
        stderr: err_reader.join().expect("stderr reader"),
    }
}

/// The product ask, end to end: a workspace whose tree already has a live
/// session grows another one, and the next `--fresh` start grows a third.
/// Each returned record carries the tag that was actually claimed.
#[test]
fn fresh_starts_grow_a_live_tree_one_session_at_a_time() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let first = harness.start(&workspace, "main", &[]);
    let _cleanup = cleanup_handles(std::slice::from_ref(&first));

    let second = harness.start(&workspace, "main", &["--fresh"]);
    assert_eq!(second["tag"], "main-2", "{}", second);
    assert_ne!(second["id"], first["id"], "{}", second);
    let _second_cleanup = cleanup_handles(std::slice::from_ref(&second));

    let third = harness.start(&workspace, "main", &["--fresh"]);
    assert_eq!(third["tag"], "main-3", "{}", third);
    let _third_cleanup = cleanup_handles(std::slice::from_ref(&third));

    assert_eq!(harness.tags(&workspace), vec!["main", "main-2", "main-3"]);
}

/// The inherited safety property: a live holder is skipped, never taken --
/// the original session stays live with its own identity.
#[test]
fn fresh_start_never_touches_the_live_holder_it_skips() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let holder = harness.start(&workspace, "main", &[]);
    let _cleanup = cleanup_handles(std::slice::from_ref(&holder));

    let sibling = harness.start(&workspace, "main", &["--fresh"]);
    assert_eq!(sibling["tag"], "main-2", "{}", sibling);
    let _sibling_cleanup = cleanup_handles(std::slice::from_ref(&sibling));

    let rows = harness.live_rows(&workspace);
    assert_eq!(rows.len(), 2, "{rows:?}");
    let original = rows
        .iter()
        .find(|row| row["tag"] == "main")
        .expect("original row");
    assert_eq!(original["id"], holder["id"], "{}", original);
    assert_eq!(original["state"], "running", "{}", original);
}

/// `--fresh` changes the *error*, not the *bar*: a dead holder of the exact
/// requested tag is reclaimable, so the exact name is taken (reclaimed)
/// rather than skipped to a suffix -- the same verdict plain `start` applies.
///
/// The holder's record is rewritten into its real dead shape (no live pids)
/// rather than waited for: a worker that exits on its own may linger as an
/// unreaped zombie when the suite runs nested inside another aplexer worker
/// (the subreaper that adopts it does not reap promptly), which would make
/// "wait for worker_alive false" an environment property instead of a test
/// one. Everything else about the record is real.
#[test]
fn fresh_start_reclaims_a_dead_holder_under_its_exact_name() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let corpse = harness.json(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().expect("UTF-8 workspace"),
        "--tag",
        "main",
        "--",
        "/bin/true",
    ]);

    // Wait for the workload's exit to be durably recorded (workload long
    // gone by then), then drop the worker pids the zombie may still hold.
    let id = corpse["id"].as_str().expect("session id").to_string();
    let record_path = harness
        .state
        .path()
        .join("sessions")
        .join(&id)
        .join("session.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "the /bin/true session never recorded its exit"
        );
        if let Ok(bytes) = std::fs::read(&record_path) {
            if let Ok(record) = serde_json::from_slice::<Value>(&bytes) {
                if record["exit"].is_object() && record["containment_empty"] == true {
                    break;
                }
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    let mut record: Value =
        serde_json::from_slice(&std::fs::read(&record_path).expect("read record for rewrite"))
            .expect("parse record for rewrite");
    record["worker_pid"] = Value::Null;
    record["workload_pid"] = Value::Null;
    std::fs::write(&record_path, serde_json::to_vec(&record).unwrap())
        .expect("write rewritten record");

    let fresh = harness.start(&workspace, "main", &["--fresh"]);
    let _cleanup = cleanup_handles(std::slice::from_ref(&fresh));
    assert_eq!(
        fresh["tag"], "main",
        "a dead holder was not reused under its own name: {}",
        fresh
    );
    assert_ne!(
        fresh["id"], corpse["id"],
        "the reclaimed session kept the corpse's identity"
    );
    assert!(
        !harness.state.path().join("sessions").join(&id).exists(),
        "the reclaim left the predecessor's durable state behind"
    );
}

/// `a new` itself: the human path -- attach forced, fresh forced. Run in a
/// workspace that already has a live `main`, it must come back attached to a
/// brand-new `main-2` (its two stdout lines: id, then workspace:tag
/// selector), leaving `main` alone.
#[test]
fn a_new_grows_the_current_workspace_and_attaches() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let holder = harness.start(&workspace, "main", &[]);
    let _cleanup = cleanup_handles(std::slice::from_ref(&holder));

    let mut command = harness.command(&[
        "new",
        "--workspace",
        workspace.path().to_str().expect("UTF-8 workspace"),
        "--tag",
        "main",
        "--",
        "/bin/sleep",
        "300",
    ]);
    command.current_dir(workspace.path());
    let launched = run_attaching(command);
    assert!(
        launched.status.success(),
        "`a new` failed: stderr={}",
        String::from_utf8_lossy(&launched.stderr)
    );
    let stdout = String::from_utf8_lossy(&launched.stdout);
    assert!(
        stdout.contains("main-2"),
        "`a new` did not report the fresh tag: stdout={stdout}"
    );

    assert_eq!(harness.tags(&workspace), vec!["main", "main-2"]);
    let rows = harness.live_rows(&workspace);
    for row in &rows {
        harness.kill_ok(row["id"].as_str().expect("id"));
    }
}
