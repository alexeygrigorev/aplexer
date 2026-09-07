//! Product-path regressions for `a prune`.
//!
//! Reproduces the state reported on 2026-09-06: three ~10-day-old records
//! whose workers had been killed without recording an exit sat at
//! `phase: running, worker_alive: false` forever. `a status` already called
//! them `broken`, but `a prune` retained all three (`{"removed": [],
//! "retained_count": 3}`) because `worker_finished()` requires a terminal
//! phase a crashed worker never gets to write. Only `a forget --force`
//! could remove them, printing a "workload processes may survive" warning
//! that was not true for any of them.
//!
//! The safety property those retained records were supposed to protect is
//! tested here too: a record whose workload leader is still alive must
//! survive prune, because prune deletes the last durable handle to it.
//! The other retention arm is a live worker whose leader is already gone --
//! every other live fixture here starts a sleeper and leaves both pids
//! running, so deleting `worker_alive()` from `reap_verdict` used to stay
//! green in this suite.
//!
//! Harness style follows tests/containment_recovery.rs (direct CLI, real
//! sessions, real signals).

use aplexer::{atomic_write_json, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use uuid::Uuid;

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

    /// A session whose workload is a plain long sleep -- long enough that
    /// nothing in these tests can pass merely because it exited on its own.
    /// Killing this session's worker takes the workload with it (the PTY
    /// master closes and the leader takes the resulting SIGHUP), which is
    /// exactly how the reported zombie records were produced.
    fn start_sleeper(&self, workspace: &TempDir, tag: &str) -> Session {
        self.start(workspace, tag, &["/bin/sleep", "300"])
    }

    /// A workload leader that deliberately survives its worker: it ignores
    /// the SIGHUP the closing PTY delivers, so killing the worker leaves a
    /// real orphaned process the record is the last handle to.
    fn start_hup_proof_sleeper(&self, workspace: &TempDir, tag: &str) -> Session {
        self.start(
            workspace,
            tag,
            &["/bin/sh", "-c", "trap \"\" HUP TERM; sleep 300"],
        )
    }

    /// A workload whose leader forks a `setsid` descendant -- a process
    /// that escapes both the leader's process group and, once the worker
    /// dies, the subreaper tree that was the session's only containment
    /// boundary. The descendant writes its pid to `marker` so the test can
    /// watch it directly.
    fn start_setsid_escapee(&self, workspace: &TempDir, tag: &str, marker: &Path) -> Session {
        self.start(
            workspace,
            tag,
            &[
                "/bin/sh",
                "-c",
                "/usr/bin/setsid /bin/sh -c 'trap \"\" HUP TERM; echo $$ > \"$1\"; sleep 900' \
                 aplexer-descendant \"$1\" & wait",
                "aplexer-leader",
                marker.to_str().expect("UTF-8 marker path"),
            ],
        )
    }

    fn start(&self, workspace: &TempDir, tag: &str, command: &[&str]) -> Session {
        let mut args = vec![
            "--json",
            "start",
            "--workspace",
            workspace.path().to_str().expect("UTF-8 workspace"),
            "--tag",
            tag,
            "--",
        ];
        args.extend_from_slice(command);
        let record = self.json(&args);
        Session {
            id: record["id"].as_str().expect("session id").to_string(),
            worker_pid: record["worker_pid"].as_i64().expect("worker pid") as i32,
            workload_pid: record["workload_pid"].as_i64().expect("workload pid") as i32,
        }
    }

    fn snapshot(&self) -> Vec<Value> {
        self.json(&["--json", "list"])
            .as_array()
            .expect("snapshot array")
            .clone()
    }

    fn state_dir_exists(&self, id: &str) -> bool {
        self.state.path().join("sessions").join(id).exists()
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

fn process_alive(pid: i32) -> bool {
    // Deliberately the production predicate, not a bare `kill(pid, 0)`:
    // that call SUCCEEDS for a zombie, so a suite using it waits forever
    // for a process that is already dead and only awaiting a reaper.
    aplexer::process_alive(pid as u32)
}

/// SIGKILL a pid and wait for it to leave /proc. Tolerates a pid that is
/// already gone: killing a worker often takes its workload with it, and the
/// point of the call is the post-condition, not the signal.
fn kill_and_wait(pid: i32) {
    unsafe { libc::kill(pid, libc::SIGKILL) };
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_alive(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!process_alive(pid), "pid {pid} did not die");
}

fn row<'a>(snapshot: &'a [Value], id: &str) -> Option<&'a Value> {
    snapshot.iter().find(|row| row["id"] == id)
}

/// Reproduce the reported state: a worker SIGKILLed without recording an
/// exit, and no workload left behind. `phase` stays at whatever the worker
/// last wrote (`running`) forever, because a killed worker never gets to
/// write a terminal one.
fn make_zombie(harness: &Harness, workspace: &TempDir, tag: &str) -> Session {
    let session = harness.start_sleeper(workspace, tag);
    // Worker first, so it never gets to record an exit. Killing it closes
    // the PTY, which usually takes the plain `sleep` leader with it; kill
    // it explicitly anyway so both pids are provably absent from /proc.
    kill_and_wait(session.worker_pid);
    kill_and_wait(session.workload_pid);
    let snapshot = harness.snapshot();
    let listed = row(&snapshot, &session.id).expect("record still listed");
    assert_eq!(
        listed["phase"], "running",
        "fixture no longer reproduces the reported stale phase: {listed}"
    );
    assert_eq!(
        listed["worker_alive"], false,
        "fixture no longer reproduces the reported dead worker: {listed}"
    );
    session
}

/// A live worker whose workload leader is already gone. `reap_verdict`
/// retains when *either* pid is alive; every other live fixture in this
/// file has both, so this is the only row that goes red if the
/// `worker_alive()` guard is deleted.
///
/// Seeded, not spawned: a real aplexer worker exits shortly after its
/// leader (the window `settle_terminating_record` waits out), so the
/// live worker here is a throwaway `sleep` that will not. No cgroup
/// locator, so containment cannot retain on its own and hide the same
/// mutation.
fn make_live_worker_dead_leader(
    harness: &Harness,
    workspace: &TempDir,
    tag: &str,
) -> (Session, std::process::Child) {
    let paths = Paths {
        runtime_root: harness.runtime.path().to_path_buf(),
        state_root: harness.state.path().to_path_buf(),
        config_file: harness.config.clone(),
    };
    paths.ensure().expect("session roots");

    let mut gone = Command::new("/bin/true").spawn().expect("dead leader");
    let workload_pid = gone.id() as i32;
    gone.wait().expect("reap dead leader");
    assert!(
        !process_alive(workload_pid),
        "reaped leader {workload_pid} still in /proc"
    );

    let stand_in = Command::new("/bin/sleep")
        .arg("300")
        .spawn()
        .expect("live worker stand-in");
    let worker_pid = stand_in.id() as i32;

    let id = Uuid::new_v4();
    let record = SessionRecord {
        parent_session: None,
        schema_version: SCHEMA_VERSION,
        id,
        workspace: workspace
            .path()
            .canonicalize()
            .expect("canonical workspace"),
        tag: tag.into(),
        engine: "shell".into(),
        profile: None,
        command: vec!["/bin/sleep".into(), "300".into()],
        cwd: PathBuf::from("/tmp"),
        env: BTreeMap::new(),
        env_unset: Vec::new(),
        limits: Limits::default(),
        history_bytes: 1024,
        created_at_ms: 1,
        updated_at_ms: 1,
        last_activity_ms: None,
        last_accessed_ms: None,
        reported_state: None,
        reported_state_at_ms: None,
        phase: Phase::Running,
        worker_pid: Some(worker_pid as u32),
        workload_pid: Some(workload_pid as u32),
        containment_cgroup: None,
        containment_cgroup_identity: None,
        containment_empty: Some(false),
        socket_path: paths.socket(id),
        history_path: paths.history(id),
        exit: None,
        error: None,
    };
    fs::create_dir_all(paths.state_session(id)).expect("state session");
    atomic_write_json(&paths.record(id), &record).expect("write record");

    let snapshot = harness.snapshot();
    let listed = row(&snapshot, &record.id.to_string()).expect("record still listed");
    assert_eq!(listed["worker_alive"], true, "{listed}");
    assert_eq!(listed["state"], "running", "{listed}");
    assert!(process_alive(worker_pid), "fixture lost its live worker");
    assert!(
        !process_alive(workload_pid),
        "fixture still has a live workload leader"
    );
    (
        Session {
            id: record.id.to_string(),
            worker_pid,
            workload_pid,
        },
        stand_in,
    )
}

/// The reported bug: a record stuck at `phase: running` with both pids gone
/// was structurally unreapable -- `a prune` returned `{"removed": [],
/// "retained_count": N}` no matter how often it ran. Nothing about that
/// state is recoverable and nothing survives it, so prune must reap it.
#[test]
fn prune_reaps_broken_record_whose_worker_and_workload_are_gone() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let session = make_zombie(&harness, &workspace, "zombie");

    let pruned = harness.json(&["--json", "prune"]);
    assert_eq!(
        pruned["removed"],
        serde_json::json!([session.id]),
        "broken record was not reaped: {pruned}"
    );
    assert_eq!(pruned["retained_count"], 0, "{pruned}");
    // No worker ever proved its containment empty, so prune must say so
    // rather than silently claiming a clean reap.
    assert_eq!(
        pruned["removed_without_containment_proof"],
        serde_json::json!([session.id]),
        "{pruned}"
    );
    assert!(
        !harness.state_dir_exists(&session.id),
        "durable state survived the reap"
    );
    assert!(harness.snapshot().is_empty(), "record still listed");
}

/// `a status` has always called this record `broken`; `a list --json` kept
/// reporting `"phase": "running"` with no derived field at all, so a machine
/// consumer (pocketshell's session tree) could not tell a zombie from a live
/// session and rendered it as an attachable row.
#[test]
fn list_json_reports_the_same_state_as_status_for_a_broken_record() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let session = make_zombie(&harness, &workspace, "zombie");

    let status = String::from_utf8_lossy(&harness.run_ok(&["status", &session.id]).stdout)
        .lines()
        .find(|line| line.starts_with("state: "))
        .map(str::to_string)
        .expect("status prints a state line");
    assert_eq!(status, "state: broken", "status stopped saying broken");

    let snapshot = harness.snapshot();
    let listed = row(&snapshot, &session.id).expect("record still listed");
    assert_eq!(
        listed["state"], "broken",
        "`a list --json` must not disagree with `a status` about liveness: {listed}"
    );
    // The persisted facts stay exactly as they were: `state` is derived on
    // top of them, it does not rewrite them.
    assert_eq!(listed["phase"], "running", "{listed}");
    assert_eq!(listed["worker_alive"], false, "{listed}");
}

/// The safety property the old guard was protecting, and the reason
/// `forget --force` exists: an orphaned workload leader is still running,
/// and the record is the last durable handle to it. Prune must not touch it.
#[test]
fn prune_retains_broken_record_whose_workload_is_still_alive() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let session = harness.start_hup_proof_sleeper(&workspace, "orphan");
    let _cleanup = ProcessCleanup(vec![session.worker_pid, session.workload_pid]);

    kill_and_wait(session.worker_pid);
    assert!(
        process_alive(session.workload_pid),
        "fixture needs a surviving workload leader"
    );

    let pruned = harness.json(&["--json", "prune"]);
    assert_eq!(
        pruned["removed"],
        serde_json::json!([]),
        "prune deleted the last handle to a live workload: {pruned}"
    );
    assert_eq!(pruned["retained_count"], 1, "{pruned}");
    assert!(
        harness.state_dir_exists(&session.id),
        "durable evidence for a live workload was removed"
    );
    assert!(
        process_alive(session.workload_pid),
        "prune must never signal anything"
    );
}

/// A healthy, live session is not prune's business at all.
#[test]
fn prune_retains_a_live_session() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let session = harness.start_sleeper(&workspace, "live");
    let _cleanup = ProcessCleanup(vec![session.worker_pid, session.workload_pid]);

    let pruned = harness.json(&["--json", "prune"]);
    assert_eq!(pruned["removed"], serde_json::json!([]), "{pruned}");
    assert_eq!(pruned["retained_count"], 1, "{pruned}");
    assert!(harness.state_dir_exists(&session.id));
    assert!(
        process_alive(session.worker_pid) && process_alive(session.workload_pid),
        "prune must never signal a live session"
    );
    let snapshot = harness.snapshot();
    let listed = row(&snapshot, &session.id).expect("live record still listed");
    assert_eq!(listed["state"], "running", "{listed}");
    assert_eq!(listed["worker_alive"], true, "{listed}");
}

/// The other retention arm: the worker is still in `/proc`, the leader is
/// not. Prune must keep the record -- it is the last durable handle to a
/// live worker -- and must never signal that worker. Once the worker is
/// gone too, the same record becomes an ordinary zombie and is reaped;
/// that proves the retention came from `worker_alive()`, not from a
/// blanket refusal to touch this shape.
#[test]
fn prune_retains_a_live_worker_whose_workload_leader_is_gone() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let (session, mut worker) = make_live_worker_dead_leader(&harness, &workspace, "worker-only");
    let _cleanup = ProcessCleanup(vec![session.worker_pid, session.workload_pid]);

    let pruned = harness.json(&["--json", "prune"]);
    assert_eq!(
        pruned["removed"],
        serde_json::json!([]),
        "prune deleted the last handle to a live worker: {pruned}"
    );
    assert_eq!(pruned["retained_count"], 1, "{pruned}");
    assert!(
        harness.state_dir_exists(&session.id),
        "durable evidence for a live worker was removed"
    );
    assert!(
        process_alive(session.worker_pid),
        "prune must never signal a live worker"
    );

    worker.kill().expect("kill stand-in worker");
    worker.wait().expect("reap stand-in worker");
    let pruned = harness.json(&["--json", "prune"]);
    assert_eq!(
        pruned["removed"],
        serde_json::json!([session.id]),
        "a worker that has since died must be reapable: {pruned}"
    );
    assert!(!harness.state_dir_exists(&session.id));
}

/// Second reported defect: `a kill` left the record behind at
/// `phase: exited, worker_alive: true` while the worker wound down, so a
/// caller that kills and then prunes (pocketshell's Stop) used to get
/// `{"removed": [], "retained_count": N}` and a row that never went away.
///
/// The fix moved further than "prune can now reap it": a session that ends
/// removes its own record during finalization, so the row is gone with no
/// prune at all (`tests/kill_removes_session.rs` pins that directly). What
/// this test still owns is the *interaction* -- prune running right behind a
/// kill must reach the same end state and must report it honestly, whichever
/// of the two did the removing.
///
/// `removed` is therefore no longer asserted to name the session: the worker
/// usually wins that race and prune correctly finds nothing left to reap.
/// The assertions that carry the defect are the end state (no durable state,
/// nothing listed, retained_count 0) plus the report never claiming an
/// unproven reap for a session whose worker finalized cleanly -- and those
/// are checked in both orders below, so neither outcome of the race can pass
/// by accident.
#[test]
fn prune_immediately_after_kill_removes_the_record() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let session = harness.start_sleeper(&workspace, "stopped");
    let _cleanup = ProcessCleanup(vec![session.worker_pid, session.workload_pid]);

    harness.run_ok(&["kill", &session.id]);
    let pruned = harness.json(&["--json", "prune"]);
    let removed = pruned["removed"].as_array().expect("removed array").clone();
    assert!(
        removed.is_empty() || removed == [Value::String(session.id.clone())],
        "prune reaped something other than the killed session: {pruned}"
    );
    assert_eq!(pruned["retained_count"], 0, "{pruned}");
    // The worker got to finish its own lifecycle, so if prune did reap this
    // record it was a proven-clean reap -- never an unproven one.
    assert_eq!(
        pruned["removed_without_containment_proof"],
        serde_json::json!([]),
        "{pruned}"
    );
    // The end state is the point of the test, and it must hold whether the
    // worker or prune got there first. A short wait covers the third
    // possibility -- prune ran while the worker was still finalizing, saw a
    // record it correctly refused to touch, and the worker removed it a
    // moment later.
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness.state_dir_exists(&session.id) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !harness.state_dir_exists(&session.id),
        "durable state survived both the kill and the prune"
    );
    assert!(harness.snapshot().is_empty(), "stopped record still listed");
    // ... and a prune run after the dust settles is a clean no-op rather
    // than an error or a phantom row.
    let again = harness.json(&["--json", "prune"]);
    assert_eq!(again["removed"], serde_json::json!([]), "{again}");
    assert_eq!(again["retained_count"], 0, "{again}");
}

/// The invariant this change deliberately supersedes, pinned so it stays a
/// decision rather than a surprise.
///
/// `a kill`/`a forget` preserve a broken unlimited session's evidence
/// "for manual investigation rather than reporting a false cleanup success"
/// (`recover_broken_containment`; asserted by
/// `tests/containment_recovery.rs::kill_preserves_evidence_when_dead_unlimited_worker_loses_setsid_descendant`).
/// That rule is untouched for `a kill`, and it still holds under `a prune`
/// while the workload leader is alive -- the state that sibling test builds,
/// which the retention test above covers. Once the leader is gone too,
/// `a prune` reaps: the escaped descendant outlives it, unsignalled, with no
/// record left naming the session it came from. That is the accepted cost of
/// making a permanently unreapable record reapable; the alternative was a row
/// that only `a forget --force` could ever remove.
#[test]
fn prune_reaps_a_record_whose_setsid_descendant_escaped() {
    assert!(Path::new("/usr/bin/setsid").is_file(), "setsid is required");
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let marker = harness.runtime.path().join("setsid-descendant-pid");
    let session = harness.start_setsid_escapee(&workspace, "escapee", &marker);

    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let descendant: i32 = fs::read_to_string(&marker)
        .expect("descendant marker")
        .trim()
        .parse()
        .expect("descendant pid");
    let _cleanup = ProcessCleanup(vec![session.worker_pid, session.workload_pid, descendant]);

    // Worker first (no exit recorded), then the leader. The `setsid`
    // descendant survives both: it is in its own session and ignores HUP.
    kill_and_wait(session.worker_pid);
    kill_and_wait(session.workload_pid);
    assert!(
        process_alive(descendant),
        "fixture needs an escaped descendant that outlived worker and leader"
    );

    // `a kill` still refuses this record -- it cannot prove any cleanup.
    let killed = harness.run(&["kill", &session.id]);
    assert!(
        !killed.status.success(),
        "a kill claimed a cleanup it cannot do"
    );
    assert!(
        String::from_utf8_lossy(&killed.stderr).contains("no authoritative containment locator"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&killed.stderr)
    );
    assert!(
        harness.state_dir_exists(&session.id),
        "a kill must still preserve the evidence it refused to act on"
    );

    // `a prune` supersedes that preservation once nothing addressable is
    // left, and reports the reap as unproven rather than claiming it clean.
    let pruned = harness.json(&["--json", "prune"]);
    assert_eq!(
        pruned["removed"],
        serde_json::json!([session.id]),
        "{pruned}"
    );
    assert_eq!(
        pruned["removed_without_containment_proof"],
        serde_json::json!([session.id]),
        "an escaped descendant must never be reported as a proven-clean reap: {pruned}"
    );
    assert!(!harness.state_dir_exists(&session.id));
    assert!(
        process_alive(descendant),
        "prune must never signal anything, including a descendant it is abandoning"
    );
}
