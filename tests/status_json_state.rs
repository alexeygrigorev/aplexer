//! `a status --json` must report the same derived liveness `state` that
//! `a list --json` and `a status`'s own human output report (issue #6).
//!
//! The property under test is *agreement between commands*, not the shape
//! of one command's output: `state` exists because a machine consumer
//! reading a SIGKILLed worker's record saw `"phase": "running"` and could
//! not tell a zombie from a live session. Adding the field to `list` but
//! not to `status --json` leaves a consumer of `status` with exactly the
//! ambiguity the field was introduced to remove, and leaves the same
//! command telling a human and a machine two different stories.
//!
//! Harness style follows tests/prune_dead_records.rs (direct CLI, real
//! sessions, real signals, isolated state/runtime dirs).

use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// The exact `a status --json` key set of the released CLI (`origin/main`
/// @ c548ace, the commit this change branched from), for the broken record
/// `broken_session()` below builds.
///
/// Captured from the real binary's real output rather than transcribed
/// from the source: the pre-change `a` was built from that commit, run
/// against an isolated instance on the same fixture, and its printed JSON
/// object's keys sorted. That makes this an on-the-wire baseline -- the
/// thing a downstream consumer actually parses -- so a key silently
/// renamed, retyped away or dropped fails here even if the code still
/// looks like it emits it.
const BASELINE_STATUS_JSON_KEYS: &[&str] = &[
    "command",
    "containment_empty",
    "created_at_ms",
    "cwd",
    "engine",
    "env",
    "env_unset",
    "history_bytes",
    "history_path",
    "id",
    "limits",
    "phase",
    "rpc_error",
    "schema_version",
    "socket_path",
    "tag",
    "updated_at_ms",
    "worker_alive",
    "worker_pid",
    "worker_reachable",
    "workload_pid",
    "workspace",
];

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

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_a"))
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            .args(args)
            .output()
            .expect("run aplexer CLI")
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
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

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run_ok(args);
        serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "`a {}` did not print JSON ({error}): {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    fn start_sleeper(&self, workspace: &TempDir, tag: &str) -> Session {
        let ws = workspace.path().to_str().expect("UTF-8 workspace");
        let record = self.json(&[
            "--json",
            "start",
            "--workspace",
            ws,
            "--tag",
            tag,
            "--",
            "/bin/sleep",
            "300",
        ]);
        Session {
            id: record["id"].as_str().expect("session id").to_string(),
            worker_pid: record["worker_pid"].as_i64().expect("worker pid") as i32,
            workload_pid: record["workload_pid"].as_i64().expect("workload pid") as i32,
        }
    }

    /// What `a status --json <id>` puts on the wire.
    fn status_json(&self, id: &str) -> Value {
        self.json(&["--json", "status", id])
    }

    /// The `state:` value `a status <id>` prints for a human.
    fn status_human_state(&self, id: &str) -> String {
        let stdout = self.run_ok(&["status", id]).stdout;
        String::from_utf8_lossy(&stdout)
            .lines()
            .find_map(|line| line.strip_prefix("state: ").map(str::to_string))
            .unwrap_or_else(|| {
                panic!(
                    "`a status {id}` printed no state line: {}",
                    String::from_utf8_lossy(&stdout)
                )
            })
    }

    /// The `state` value this record carries in `a list --json`.
    fn list_json_state(&self, id: &str) -> String {
        let rows = self.json(&["--json", "list"]);
        let row = rows
            .as_array()
            .expect("list prints an array")
            .iter()
            .find(|row| row["id"] == id)
            .unwrap_or_else(|| panic!("`a list --json` no longer lists {id}: {rows}"))
            .clone();
        row["state"]
            .as_str()
            .unwrap_or_else(|| panic!("`a list --json` row carries no state: {row}"))
            .to_string()
    }
}

struct Session {
    id: String,
    worker_pid: i32,
    workload_pid: i32,
}

/// Kills anything the test may have orphaned, so a failing assertion cannot
/// leave a `sleep 300` behind on the box.
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

/// `kill(pid, 0)` alone is not a liveness test: it succeeds for a zombie.
/// A worker SIGKILLed by this suite reparents onto whatever child subreaper
/// the suite itself runs under (another aplexer worker, when the tests are
/// run from inside a session), and stays signalable until that subreaper
/// reaps it -- so the bare signal probe reported the pid alive forever and
/// this suite failed with "pid NNNN did not die". `aplexer::process_alive`
/// subtracts the zombie state, which is the same answer `a prune` and
/// `a status` now give.
fn process_alive(pid: i32) -> bool {
    aplexer::process_alive(pid as u32)
}

fn kill_and_wait(pid: i32) {
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!process_alive(pid), "pid {pid} did not die");
}

/// The reported fixture: a worker SIGKILLed before it could record an exit,
/// with no workload left behind. Its persisted `phase` stays `running`
/// forever, so anything reading `phase` alone sees a healthy session --
/// this is the state the derived `state` field exists to describe.
fn broken_session(harness: &Harness, workspace: &TempDir, tag: &str) -> Session {
    let session = harness.start_sleeper(workspace, tag);
    kill_and_wait(session.worker_pid);
    kill_and_wait(session.workload_pid);
    let status = harness.status_json(&session.id);
    assert_eq!(
        status["phase"], "running",
        "fixture no longer reproduces the reported stale phase: {status}"
    );
    assert_eq!(
        status["worker_alive"], false,
        "fixture no longer reproduces the reported dead worker: {status}"
    );
    session
}

/// Issue #6: `a status`'s human output said `broken`, `a list --json` said
/// `broken`, and `a status --json` -- the machine face of the very command
/// printing that line -- said nothing at all. All three read the same
/// record, so all three must report the same state.
#[test]
fn status_json_agrees_with_list_json_and_human_output_for_a_broken_record() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let session = broken_session(&harness, &workspace, "zombie");

    let status_json = harness.status_json(&session.id);
    let json_state = status_json["state"]
        .as_str()
        .unwrap_or_else(|| panic!("`a status --json` carries no state: {status_json}"));
    let human_state = harness.status_human_state(&session.id);
    let list_state = harness.list_json_state(&session.id);

    assert_eq!(
        human_state, "broken",
        "fixture is not the reported broken record any more"
    );
    assert_eq!(
        json_state, human_state,
        "`a status --json` and `a status` disagree about the same record's state"
    );
    assert_eq!(
        json_state, list_state,
        "`a status --json` and `a list --json` disagree about the same record's state"
    );

    // The derived field sits on top of the persisted facts; it does not
    // rewrite them. A consumer still sees exactly what the worker wrote.
    assert_eq!(status_json["phase"], "running", "{status_json}");
    assert_eq!(status_json["worker_alive"], false, "{status_json}");
}

/// The agreement must hold for a healthy session too, otherwise a fix could
/// satisfy the test above by hardcoding "broken" -- and a consumer would
/// then have to special-case which sessions `status --json` tells the truth
/// about.
#[test]
fn status_json_agrees_with_list_json_and_human_output_for_a_live_record() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let session = harness.start_sleeper(&workspace, "live");
    let _cleanup = ProcessCleanup(vec![session.worker_pid, session.workload_pid]);

    let status_json = harness.status_json(&session.id);
    let json_state = status_json["state"]
        .as_str()
        .unwrap_or_else(|| panic!("`a status --json` carries no state: {status_json}"));
    let human_state = harness.status_human_state(&session.id);
    let list_state = harness.list_json_state(&session.id);

    assert_eq!(
        human_state, "running",
        "fixture is not a live session any more"
    );
    assert_eq!(
        json_state, human_state,
        "`a status --json` and `a status` disagree about the same record's state"
    );
    assert_eq!(
        json_state, list_state,
        "`a status --json` and `a list --json` disagree about the same record's state"
    );

    harness.run_ok(&["kill", &session.id]);
}

/// Additive-only wire check: every key the released CLI put on the wire for
/// this fixture is still there, and the only things added are the two
/// query-time derived fields -- `state` (liveness, see `observed_state`) and
/// `agent` (which agent is live in the workload's process tree,
/// pocketshell issue #2580). The baseline is the released binary's real output (see
/// `BASELINE_STATUS_JSON_KEYS`), so this fails on a removed or renamed key
/// even though nothing in the source says "these keys are load-bearing", and
/// it fails again the moment a THIRD field appears without a deliberate
/// decision to widen the wire.
#[test]
fn status_json_adds_only_the_derived_state_and_agent_fields() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let session = broken_session(&harness, &workspace, "zombie");

    let status_json = harness.status_json(&session.id);
    let observed: BTreeSet<String> = status_json
        .as_object()
        .unwrap_or_else(|| panic!("`a status --json` printed a non-object: {status_json}"))
        .keys()
        .cloned()
        .collect();
    let baseline: BTreeSet<String> = BASELINE_STATUS_JSON_KEYS
        .iter()
        .map(|k| (*k).to_string())
        .collect();

    let removed: Vec<&String> = baseline.difference(&observed).collect();
    assert!(
        removed.is_empty(),
        "`a status --json` dropped keys the released CLI emitted: {removed:?}"
    );
    let added: Vec<&String> = observed.difference(&baseline).collect();
    assert_eq!(
        added,
        vec!["agent", "state"],
        "`a status --json` changed its wire shape by more than the additive `agent`/`state` fields"
    );
}
