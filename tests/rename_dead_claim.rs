//! `a rename` must answer "does this record still own its `workspace+tag`?"
//! exactly like `a start` does (issue #13).
//!
//! `98b5efc` (#7) routed `start_session`'s supersede check through
//! `reap_verdict`, so `a start` reclaims a pair held by a dead record.
//! `rename` was left with a bare conflict scan -- no liveness check at all --
//! so on the same box, with the same dead record holding the same pair,
//! `a start` succeeded while `a rename` failed with
//! "workspace+tag already belongs to session <uuid>", naming a session the
//! user cannot see, cannot attach to, and has no obvious way to get rid of.
//! The bug is the disagreement, so
//! `start_and_rename_agree_for_every_record_shape` asserts the agreement
//! directly, shape by shape, instead of testing each command in isolation.
//!
//! The dead-conflict decision taken here is the non-destructive one: rename
//! takes the pair and leaves the corpse in place for `a prune` -- it neither
//! archives nor deletes anything (`rename_takes_a_workspace_tag_held_by_a_dead_record`
//! pins that a successful rename leaves the dead record's durable state
//! behind). A pre-PID `Starting` holder is fenced on its worker lock exactly
//! like every other claim check (issue #9): a record in the
//! spawn-to-worker-lock gap is a healthy session coming up, and a rename
//! must not steal its tag.
//!
//! Harness style follows tests/reclaim_zombie_tag.rs (direct CLI, real
//! sessions, real signals).

use aplexer::{atomic_write_json, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
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

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run aplexer CLI")
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

    /// A session whose workload is a plain long sleep. Killing its worker
    /// closes the PTY and takes the leader with it -- exactly how the
    /// zombie records behind issue #13 were produced.
    fn start_sleeper(&self, workspace: &TempDir, tag: &str) -> Session {
        self.start(workspace, tag, &["/bin/sleep", "300"])
    }

    fn rename(&self, session: &str, workspace: &TempDir, tag: &str) -> Output {
        self.run(&[
            "--json",
            "rename",
            session,
            "--workspace",
            workspace.path().to_str().expect("UTF-8 workspace"),
            "--tag",
            tag,
        ])
    }

    fn snapshot(&self) -> Vec<Value> {
        self.json(&["--json", "list"])
            .as_array()
            .expect("snapshot array")
            .clone()
    }

    fn rows_for(&self, workspace: &TempDir, tag: &str) -> Vec<Value> {
        let workspace = workspace
            .path()
            .canonicalize()
            .expect("canonical workspace")
            .to_str()
            .expect("UTF-8 workspace")
            .to_string();
        self.snapshot()
            .into_iter()
            .filter(|row| row["workspace"] == workspace && row["tag"] == tag)
            .collect()
    }

    fn state_dir(&self, id: &str) -> PathBuf {
        self.state.path().join("sessions").join(id)
    }

    fn state_dir_exists(&self, id: &str) -> bool {
        self.state_dir(id).exists()
    }

    fn worker_lock(&self, id: &str) -> PathBuf {
        self.runtime
            .path()
            .join("sessions")
            .join(id)
            .join("worker.lock")
    }
}

struct Session {
    id: String,
    worker_pid: i32,
    workload_pid: i32,
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

/// Reproduce the reported state: a worker SIGKILLed without recording an
/// exit, and no workload left behind. `phase` stays at `running` forever --
/// the invisible corpse issue #13 is about.
fn make_zombie(harness: &Harness, workspace: &TempDir, tag: &str) -> Session {
    let session = harness.start_sleeper(workspace, tag);
    kill_and_wait(session.worker_pid);
    kill_and_wait(session.workload_pid);
    let rows = harness.rows_for(workspace, tag);
    assert_eq!(rows.len(), 1, "fixture lost its record: {rows:?}");
    assert_eq!(
        rows[0]["phase"], "running",
        "fixture no longer reproduces the reported stale phase: {}",
        rows[0]
    );
    assert_eq!(
        rows[0]["worker_alive"], false,
        "fixture no longer reproduces the reported dead worker: {}",
        rows[0]
    );
    assert_eq!(rows[0]["state"], "broken", "{}", rows[0]);
    session
}

/// Rewrite a started-then-killed session's record into the exact on-disk
/// shape `start_session` publishes before it spawns: active phase, no worker
/// pid, no workload pid -- a pre-PID `Starting` stub (issue #9's shape).
fn rewrite_as_pre_pid_stub(harness: &Harness, id: &str) {
    let record_path = harness.state_dir(id).join("session.json");
    let mut record: Value = serde_json::from_slice(&fs::read(&record_path).expect("read record"))
        .expect("parse record");
    record["phase"] = Value::String("starting".into());
    record["worker_pid"] = Value::Null;
    record["workload_pid"] = Value::Null;
    fs::write(&record_path, serde_json::to_vec(&record).unwrap()).expect("write record");
}

/// A live worker whose workload leader is already gone. `reap_verdict`
/// retains when *either* pid is alive, so this is the shape that dies if a
/// liveness predicate degenerates to "leader only".
///
/// Seeded, not spawned: a real aplexer worker exits shortly after its
/// leader, so the live worker here is a throwaway `sleep` that will not.
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
        worker_cgroup: None,
        workload_cgroup: None,
        socket_path: paths.socket(id),
        history_path: paths.history(id),
        exit: None,
        error: None,
    };
    fs::create_dir_all(paths.state_session(id)).expect("state session");
    atomic_write_json(&paths.record(id), &record).expect("write record");

    let rows = harness.rows_for(workspace, tag);
    assert_eq!(rows.len(), 1, "fixture lost its record: {rows:?}");
    assert_eq!(rows[0]["worker_alive"], true, "{}", rows[0]);
    assert_eq!(rows[0]["state"], "running", "{}", rows[0]);
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

/// One claim-check fixture, self-contained: its own harness, workspace and
/// tag (`claim`), in one of the record shapes both claim checks must agree
/// about. Dropping it kills everything it started, so a failing assertion
/// cannot strand a `sleep 300` or a held fence on the box.
struct ShapeFixture {
    harness: Harness,
    workspace: TempDir,
    tag: &'static str,
    /// Held for the `ComingUpFenced` shape only: the worker lock of a
    /// pre-PID stub, simulating a worker that was spawned but has not
    /// reached its first lock yet.
    _fence: Option<aplexer::FileLock>,
    /// The live stand-in worker of the `LiveWorkerDeadLeader` shape.
    _stand_in_worker: Option<std::process::Child>,
    /// Every pid this fixture started or stood in for.
    pids: Vec<i32>,
    /// Every live session id this fixture created (killed on drop).
    live_sessions: Vec<String>,
}

impl Drop for ShapeFixture {
    fn drop(&mut self) {
        for pid in self.pids.drain(..) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        if let Some(mut child) = self._stand_in_worker.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        for id in self.live_sessions.drain(..) {
            let _ = self
                .harness
                .command(&["kill", &id, "--signal", "KILL", "--grace-ms", "0"])
                .output();
        }
    }
}

/// The record shapes `start_session`'s reclaim and `rename`'s claim check
/// must agree about (issue #13 acceptance criterion).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Worker SIGKILLed, `phase` stuck at `running`: the invisible corpse.
    Zombie,
    /// A healthy running session.
    Live,
    /// Pre-PID `Starting` stub whose worker lock is held: a session coming up.
    ComingUpFenced,
    /// Pre-PID `Starting` stub with nobody behind it: a crashed start.
    CrashedStart,
    /// Live worker, dead workload leader: `reap_verdict` must retain.
    LiveWorkerDeadLeader,
}

const SHAPES: &[Shape] = &[
    Shape::Zombie,
    Shape::Live,
    Shape::ComingUpFenced,
    Shape::CrashedStart,
    Shape::LiveWorkerDeadLeader,
];

impl Shape {
    /// The answer both commands must give for this shape.
    fn claim_is_taken(self) -> bool {
        match self {
            Shape::Zombie | Shape::CrashedStart => true,
            Shape::Live | Shape::ComingUpFenced | Shape::LiveWorkerDeadLeader => false,
        }
    }

    fn build(self) -> ShapeFixture {
        let harness = Harness::new();
        let workspace = TempDir::new().expect("shape workspace tempdir");
        let tag = "claim";
        match self {
            Shape::Zombie => {
                let corpse = make_zombie(&harness, &workspace, tag);
                ShapeFixture {
                    harness,
                    workspace,
                    tag,
                    _fence: None,
                    _stand_in_worker: None,
                    pids: vec![corpse.worker_pid, corpse.workload_pid],
                    live_sessions: Vec::new(),
                }
            }
            Shape::Live => {
                let session = harness.start_sleeper(&workspace, tag);
                ShapeFixture {
                    harness,
                    workspace,
                    tag,
                    _fence: None,
                    _stand_in_worker: None,
                    pids: vec![session.worker_pid, session.workload_pid],
                    live_sessions: vec![session.id],
                }
            }
            Shape::ComingUpFenced | Shape::CrashedStart => {
                let stub = make_zombie(&harness, &workspace, tag);
                rewrite_as_pre_pid_stub(&harness, &stub.id);
                let fence = match self {
                    Shape::ComingUpFenced => Some(
                        aplexer::FileLock::exclusive(&harness.worker_lock(&stub.id), true)
                            .expect("hold the stub's worker lock"),
                    ),
                    _ => None,
                };
                ShapeFixture {
                    harness,
                    workspace,
                    tag,
                    _fence: fence,
                    _stand_in_worker: None,
                    pids: vec![stub.worker_pid, stub.workload_pid],
                    live_sessions: Vec::new(),
                }
            }
            Shape::LiveWorkerDeadLeader => {
                let (session, stand_in) = make_live_worker_dead_leader(&harness, &workspace, tag);
                ShapeFixture {
                    harness,
                    workspace,
                    tag,
                    _fence: None,
                    _stand_in_worker: Some(stand_in),
                    pids: vec![session.worker_pid, session.workload_pid],
                    live_sessions: Vec::new(),
                }
            }
        }
    }
}

/// The reported bug, end to end: `a start` reclaims a pair held by a dead
/// record, so `a rename` must be able to take it too. Rename is the
/// non-destructive claimant: it takes the name and leaves the corpse's
/// durable state in place for `a prune`.
#[test]
fn rename_takes_a_workspace_tag_held_by_a_dead_record() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let corpse = make_zombie(&harness, &workspace, "zt");
    let helper_workspace = TempDir::new().expect("helper workspace tempdir");
    let helper = harness.start_sleeper(&helper_workspace, "helper");
    let _cleanup = ProcessCleanup(vec![helper.worker_pid, helper.workload_pid]);

    let renamed = harness.rename(&helper.id, &workspace, "zt");
    assert!(
        renamed.status.success(),
        "rename refused a pair held by a dead record: stderr={}",
        String::from_utf8_lossy(&renamed.stderr)
    );

    // The renamed session owns the pair now.
    let rows = harness.rows_for(&workspace, "zt");
    assert_eq!(rows.len(), 2, "rename lost a record: {rows:?}");
    let live = rows
        .iter()
        .find(|row| row["id"] == helper.id.as_str())
        .expect("renamed session missing from the registry");
    assert_eq!(live["state"], "running", "{}", live);

    // ...and the corpse is untouched: rename destroys nothing, `a prune`
    // remains its cleanup path.
    let dead = rows
        .iter()
        .find(|row| row["id"] == corpse.id.as_str())
        .expect("rename must not remove the dead record");
    assert_eq!(dead["state"], "broken", "{}", dead);
    assert!(
        harness.state_dir_exists(&corpse.id),
        "rename destroyed the dead record's durable state"
    );
}

/// The boundary that must never move: a genuinely live session keeps its
/// claim, and the refusal names it, its derived state and a next step --
/// not an invisible uuid.
#[test]
fn rename_refuses_a_workspace_tag_held_by_a_live_session() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let holder = harness.start_sleeper(&workspace, "live");
    let _holder_cleanup = ProcessCleanup(vec![holder.worker_pid, holder.workload_pid]);
    let helper_workspace = TempDir::new().expect("helper workspace tempdir");
    let helper = harness.start_sleeper(&helper_workspace, "helper");
    let _helper_cleanup = ProcessCleanup(vec![helper.worker_pid, helper.workload_pid]);

    let refused = harness.rename(&helper.id, &workspace, "live");
    assert!(
        !refused.status.success(),
        "rename stole a live session's pair: stdout={}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("workspace+tag already belongs"), "{stderr}");
    assert!(stderr.contains(&holder.id), "{stderr}");
    assert!(
        stderr.contains("state: running"),
        "refusal must name the holder's derived state: {stderr}"
    );
    assert!(
        stderr.contains("rename it or choose a different tag"),
        "refusal must name a next step: {stderr}"
    );

    let rows = harness.rows_for(&workspace, "live");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], holder.id, "{}", rows[0]);
    assert_eq!(rows[0]["state"], "running", "{}", rows[0]);
    assert!(
        process_alive(holder.worker_pid) && process_alive(holder.workload_pid),
        "a refused rename must never signal the session it was refused"
    );

    // The helper keeps its old name: a refused rename changed nothing.
    let helper_rows = harness.rows_for(&helper_workspace, "helper");
    assert_eq!(helper_rows.len(), 1, "{helper_rows:?}");
    assert_eq!(helper_rows[0]["id"], helper.id, "{}", helper_rows[0]);
}

/// The criterion that matters (issue #13): for every record shape, `a start`
/// and `a rename` answer "who owns this `workspace+tag`?" identically. Each
/// shape also pins its expected answer directly, so an accident that breaks
/// both commands the same way cannot fake agreement.
#[test]
fn start_and_rename_agree_for_every_record_shape() {
    for shape in SHAPES {
        let (rename_ok, rename_detail) = rename_leg(*shape);
        let (start_ok, start_detail) = start_leg(*shape);
        assert_eq!(
            rename_ok,
            shape.claim_is_taken(),
            "rename answered wrong for {shape:?}: {rename_detail}"
        );
        assert_eq!(
            start_ok,
            shape.claim_is_taken(),
            "start answered wrong for {shape:?}: {start_detail}"
        );
        assert_eq!(
            rename_ok,
            start_ok,
            "start and rename disagree for {shape:?}: rename said {}, start said {}",
            if rename_ok { "yes" } else { "no" },
            if start_ok { "yes" } else { "no" },
        );
    }
}

/// `a rename` a live helper session onto a shape-held pair; report whether
/// the rename succeeded.
fn rename_leg(shape: Shape) -> (bool, String) {
    let fixture = shape.build();
    let helper_workspace = TempDir::new().expect("helper workspace tempdir");
    let helper = fixture.harness.start_sleeper(&helper_workspace, "helper");
    let _cleanup = ProcessCleanup(vec![helper.worker_pid, helper.workload_pid]);

    let renamed = fixture
        .harness
        .rename(&helper.id, &fixture.workspace, fixture.tag);
    (
        renamed.status.success(),
        format!("stderr={}", String::from_utf8_lossy(&renamed.stderr)),
    )
}

/// `a start` onto a shape-held pair; report whether the start succeeded.
fn start_leg(shape: Shape) -> (bool, String) {
    let fixture = shape.build();
    let started = fixture.harness.run(&[
        "--json",
        "start",
        "--workspace",
        fixture.workspace.path().to_str().expect("UTF-8 workspace"),
        "--tag",
        fixture.tag,
        "--",
        "/bin/sleep",
        "300",
    ]);
    if started.status.success() {
        let record: Value = serde_json::from_slice(&started.stdout).expect("start JSON");
        let _cleanup = ProcessCleanup(vec![
            record["worker_pid"].as_i64().expect("worker pid") as i32,
            record["workload_pid"].as_i64().expect("workload pid") as i32,
        ]);
        let replacement = record["id"].as_str().expect("replacement id").to_string();
        fixture
            .harness
            .run_ok(&["kill", &replacement, "--signal", "KILL", "--grace-ms", "0"]);
    }
    (
        started.status.success(),
        format!("stderr={}", String::from_utf8_lossy(&started.stderr)),
    )
}
