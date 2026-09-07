//! Product-path regressions for reclaiming a `workspace+tag` from a record
//! that no longer needs it (issue #7).
//!
//! Reported shape: a zombie -- worker SIGKILLed, so `phase` is stuck at
//! `running` because the worker never got to write a terminal one -- held
//! its `workspace+tag` forever, because `start_session`'s supersede check
//! was `worker_finished()` (`phase in {Exited, Failed} && !worker_alive()`):
//!
//! ```text
//! $ a --json start --workspace /tmp/ws --tag zt
//! a: workspace+tag already belongs to session 57e5a6ba-...; rename it or
//!    choose a different tag
//! exit=1
//! ```
//!
//! `a prune` learned to reap such a record, but only if something ran it
//! first -- so a caller that just runs `a start` still failed, and
//! pocketshell had to reap-before-start to work around it.
//!
//! The safety property this must never break is tested here alongside: a
//! record whose *workload* is still alive keeps its claim, because taking
//! the pair archives and then deletes the holder's durable state, which is
//! the last handle to that running process. A live *worker* whose leader
//! is already gone keeps its claim for the same reason; every other live
//! fixture here has both pids, so deleting `worker_alive()` from
//! `reap_verdict` used to stay green in this suite.
//!
//! Harness style follows tests/prune_dead_records.rs (direct CLI, real
//! sessions, real signals).

use aplexer::{atomic_write_json, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
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

    /// `a -` runs from the current directory and always attaches, so it can
    /// only be driven with a cwd and a bounded wait: with stdin at EOF the
    /// attach relay returns on its own, but a hang here must fail the test
    /// rather than wedge the suite.
    fn quick_launch(&self, workspace: &TempDir, rest: &[&str]) -> Output {
        let mut args = vec!["-"];
        args.extend_from_slice(rest);
        let mut command = self.command(&args);
        command
            .current_dir(workspace.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        run_with_timeout(command, Duration::from_secs(30))
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
    /// reported zombie records were produced.
    fn start_sleeper(&self, workspace: &TempDir, tag: &str) -> Session {
        self.start(workspace, tag, &["/bin/sleep", "300"])
    }

    /// A workload leader that deliberately survives its worker: it ignores
    /// the SIGHUP the closing PTY delivers, so killing the worker leaves a
    /// real orphaned process whose last durable handle is this record.
    fn start_hup_proof_sleeper(&self, workspace: &TempDir, tag: &str) -> Session {
        self.start(
            workspace,
            tag,
            &["/bin/sh", "-c", "trap \"\" HUP TERM; sleep 300"],
        )
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

    fn retired_dir(&self, id: &str) -> PathBuf {
        self.state.path().join("retired-sessions").join(id)
    }
}

struct Session {
    id: String,
    worker_pid: i32,
    workload_pid: i32,
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

fn run_with_timeout(mut command: Command, timeout: Duration) -> Output {
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
    let status = match rx.recv_timeout(timeout) {
        Ok(status) => status.expect("wait for aplexer CLI"),
        Err(_) => {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            panic!("`a` did not return within {timeout:?}");
        }
    };
    Output {
        status,
        stdout: out_reader.join().expect("stdout reader"),
        stderr: err_reader.join().expect("stderr reader"),
    }
}

/// Reproduce the reported state: a worker SIGKILLed without recording an
/// exit, and no workload left behind. `phase` stays at whatever the worker
/// last wrote (`running`) forever.
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

/// A live worker whose workload leader is already gone. `reap_verdict`
/// retains when *either* pid is alive; every other live fixture in this
/// file has both, so this is the only row that goes red if the
/// `worker_alive()` guard is deleted.
///
/// Seeded, not spawned: a real aplexer worker exits shortly after its
/// leader, so the live worker here is a throwaway `sleep` that will not.
/// No cgroup locator, so containment cannot retain on its own and hide
/// the same mutation.
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

/// The reported bug, end to end: `a start` on a pair held by a zombie had to
/// be preceded by something that pruned the zombie first. It must now
/// succeed on its own.
#[test]
fn start_reclaims_a_workspace_tag_held_by_a_broken_record() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let zombie = make_zombie(&harness, &workspace, "zt");

    let started = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "zt",
        "--",
        "/bin/sleep",
        "300",
    ]);
    assert!(
        started.status.success(),
        "start refused a pair held by a broken record: stderr={}",
        String::from_utf8_lossy(&started.stderr)
    );
    let record: Value = serde_json::from_slice(&started.stdout).expect("start JSON");
    let replacement = record["id"].as_str().expect("replacement id").to_string();
    let _cleanup = ProcessCleanup(vec![
        record["worker_pid"].as_i64().unwrap() as i32,
        record["workload_pid"].as_i64().unwrap() as i32,
    ]);
    assert_ne!(replacement, zombie.id, "start returned the zombie itself");

    // The zombie was archived and cleaned up by the same replacement
    // transaction a finished predecessor goes through -- not left behind as
    // a second record claiming the pair.
    let rows = harness.rows_for(&workspace, "zt");
    assert_eq!(rows.len(), 1, "reclaim left duplicate selectors: {rows:?}");
    assert_eq!(rows[0]["id"], replacement, "{}", rows[0]);
    assert_eq!(rows[0]["state"], "running", "{}", rows[0]);
    assert!(
        !harness.state_dir_exists(&zombie.id),
        "reclaim left the predecessor's durable state behind"
    );
    assert!(
        !harness.retired_dir(&zombie.id).exists(),
        "reclaim left the predecessor's archive behind"
    );

    // Reclaiming from a worker that never proved its containment domain
    // empty is reported, exactly as `a prune` reports the same class.
    let stderr = String::from_utf8_lossy(&started.stderr);
    assert!(
        stderr.contains("without a containment proof") && stderr.contains(&zombie.id),
        "unproven reclaim was reported as a clean one: {stderr}"
    );

    harness.run_ok(&["kill", &replacement, "--signal", "KILL", "--grace-ms", "0"]);
}

/// The safety property, absolutely: taking the pair archives and then
/// deletes the holder's durable state, which is the last handle to an
/// orphaned workload leader. A live workload always retains its claim.
#[test]
fn start_refuses_a_workspace_tag_whose_workload_is_still_alive() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let session = harness.start_hup_proof_sleeper(&workspace, "orphan");
    let _cleanup = ProcessCleanup(vec![session.worker_pid, session.workload_pid]);

    kill_and_wait(session.worker_pid);
    assert!(
        process_alive(session.workload_pid),
        "fixture needs a surviving workload leader"
    );

    let refused = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "orphan",
        "--",
        "/bin/sleep",
        "300",
    ]);
    assert!(
        !refused.status.success(),
        "start orphaned a live workload to take its tag: stdout={}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("workspace+tag already belongs"), "{stderr}");
    assert!(stderr.contains(&session.id), "{stderr}");

    let rows = harness.rows_for(&workspace, "orphan");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], session.id, "{}", rows[0]);
    assert!(
        harness.state_dir_exists(&session.id),
        "refused start still destroyed the live workload's last handle"
    );
    assert!(
        process_alive(session.workload_pid),
        "start signalled a workload it was refused permission to replace"
    );
}

/// No regression to the existing contract: a genuinely live session keeps
/// its pair.
#[test]
fn start_refuses_a_workspace_tag_held_by_a_live_session() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let session = harness.start_sleeper(&workspace, "live");
    let _cleanup = ProcessCleanup(vec![session.worker_pid, session.workload_pid]);

    let refused = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "live",
        "--",
        "/bin/sleep",
        "300",
    ]);
    assert!(
        !refused.status.success(),
        "start replaced a live session: stdout={}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("workspace+tag already belongs"), "{stderr}");
    assert!(stderr.contains(&session.id), "{stderr}");

    let rows = harness.rows_for(&workspace, "live");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], session.id, "{}", rows[0]);
    assert_eq!(rows[0]["state"], "running", "{}", rows[0]);
    assert!(
        process_alive(session.worker_pid) && process_alive(session.workload_pid),
        "a refused start must never signal the session it was refused"
    );

    harness.run_ok(&["kill", &session.id, "--signal", "KILL", "--grace-ms", "0"]);
}

/// The other claim-check arm: the worker is still in `/proc` (so `a status`
/// says `state: running`) even though its workload leader is already gone.
/// Taking the pair would archive and then delete that worker's durable
/// state. Once the worker is gone too, the same pair is reclaimable; that
/// proves the refusal came from `worker_alive()`, not from a blanket
/// refusal to touch this shape.
#[test]
fn start_refuses_a_workspace_tag_whose_worker_is_alive_even_if_the_leader_is_gone() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let (session, mut worker) = make_live_worker_dead_leader(&harness, &workspace, "worker-only");
    let _cleanup = ProcessCleanup(vec![session.worker_pid, session.workload_pid]);

    let refused = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "worker-only",
        "--",
        "/bin/sleep",
        "300",
    ]);
    assert!(
        !refused.status.success(),
        "start replaced a live worker to take its tag: stdout={}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("workspace+tag already belongs"), "{stderr}");
    assert!(stderr.contains(&session.id), "{stderr}");
    assert!(
        stderr.contains("state: running"),
        "refusal must name the live worker, not a broken record: {stderr}"
    );

    let rows = harness.rows_for(&workspace, "worker-only");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], session.id, "{}", rows[0]);
    assert_eq!(rows[0]["state"], "running", "{}", rows[0]);
    assert!(
        harness.state_dir_exists(&session.id),
        "refused start still destroyed the live worker's last handle"
    );
    assert!(
        process_alive(session.worker_pid),
        "start signalled a worker it was refused permission to replace"
    );

    worker.kill().expect("kill stand-in worker");
    worker.wait().expect("reap stand-in worker");
    let started = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "worker-only",
        "--",
        "/bin/sleep",
        "300",
    ]);
    assert!(
        started.status.success(),
        "a worker that has since died must no longer hold the pair: stderr={}",
        String::from_utf8_lossy(&started.stderr)
    );
    let record: Value = serde_json::from_slice(&started.stdout).expect("start JSON");
    let _replacement = ProcessCleanup(vec![
        record["worker_pid"].as_i64().unwrap() as i32,
        record["workload_pid"].as_i64().unwrap() as i32,
    ]);
    assert_ne!(record["id"], session.id, "start returned the corpse itself");
    assert!(!harness.state_dir_exists(&session.id));
    let replacement = record["id"].as_str().expect("replacement id");
    harness.run_ok(&["kill", replacement, "--signal", "KILL", "--grace-ms", "0"]);
}

/// A record that `start_session` can no longer see as blocking, because it
/// has no terminal phase requirement any more: a `Starting` stub whose
/// worker was spawned but has not registered its pid yet reads as
/// `worker_alive: false` while being a perfectly healthy session coming up.
/// The worker holds `worker.lock` from its first startup action, so start
/// must fence on it instead of archiving the session out from under it.
#[test]
fn start_refuses_a_workspace_tag_whose_pre_pid_worker_holds_its_lock() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let zombie = make_zombie(&harness, &workspace, "coming-up");

    // Rewrite the corpse into the exact on-disk shape `start_session`
    // publishes before it spawns: active phase, no worker pid, no workload
    // pid. Everything else about the record is real.
    let record_path = harness.state_dir(&zombie.id).join("session.json");
    let mut record: Value = serde_json::from_slice(&fs::read(&record_path).expect("read record"))
        .expect("parse record");
    record["phase"] = Value::String("starting".into());
    record["worker_pid"] = Value::Null;
    record["workload_pid"] = Value::Null;
    fs::write(&record_path, serde_json::to_vec(&record).unwrap()).expect("write record");

    let lock_path = harness
        .runtime
        .path()
        .join("sessions")
        .join(&zombie.id)
        .join("worker.lock");
    let held = aplexer::FileLock::exclusive(&lock_path, true).expect("hold the worker lock");

    let args = [
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "coming-up",
        "--",
        "/bin/sleep",
        "300",
    ];
    let refused = harness.run(&args);
    assert!(
        !refused.status.success(),
        "start took a pair from a session whose worker was still coming up: stdout={}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("workspace+tag already belongs"), "{stderr}");
    assert!(stderr.contains("worker.lock"), "{stderr}");
    assert!(
        harness.state_dir_exists(&zombie.id),
        "start destroyed the state of a session that was coming up"
    );

    // Same stub, no worker behind it: now reclaimable. Proves the refusal
    // above came from the fence and not from some blanket refusal to touch
    // a `Starting` record.
    drop(held);
    let started = harness.run(&args);
    assert!(
        started.status.success(),
        "an unfenced pre-PID stub must still be reclaimable: stderr={}",
        String::from_utf8_lossy(&started.stderr)
    );
    let record: Value = serde_json::from_slice(&started.stdout).expect("start JSON");
    let _cleanup = ProcessCleanup(vec![
        record["worker_pid"].as_i64().unwrap() as i32,
        record["workload_pid"].as_i64().unwrap() as i32,
    ]);
    assert!(!harness.state_dir_exists(&zombie.id));
    let replacement = record["id"].as_str().expect("replacement id");
    harness.run_ok(&["kill", replacement, "--signal", "KILL", "--grace-ms", "0"]);
}

/// The twin claim check: `a -` attaches to a live session and hands
/// everything else to start. Against a zombie it used to inherit start's
/// refusal, so the one command whose whole promise is "create or attach"
/// could do neither.
#[test]
fn quick_launch_reclaims_a_workspace_tag_held_by_a_broken_record() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let zombie = make_zombie(&harness, &workspace, "sleep");

    let launched = harness.quick_launch(&workspace, &["/bin/sleep", "300"]);
    assert!(
        launched.status.success(),
        "`a -` refused a pair held by a broken record: stderr={}",
        String::from_utf8_lossy(&launched.stderr)
    );

    let rows = harness.rows_for(&workspace, "sleep");
    assert_eq!(rows.len(), 1, "`a -` left duplicate selectors: {rows:?}");
    assert_ne!(rows[0]["id"], zombie.id.as_str(), "{}", rows[0]);
    assert_eq!(rows[0]["state"], "running", "{}", rows[0]);
    assert!(
        !harness.state_dir_exists(&zombie.id),
        "`a -` left the predecessor's durable state behind"
    );

    let replacement = rows[0]["id"].as_str().expect("replacement id").to_string();
    let _cleanup = ProcessCleanup(vec![
        rows[0]["worker_pid"].as_i64().unwrap() as i32,
        rows[0]["workload_pid"].as_i64().unwrap() as i32,
    ]);
    harness.run_ok(&["kill", &replacement, "--signal", "KILL", "--grace-ms", "0"]);
}

/// `a -` must not route around the safety property either: with the worker
/// dead it cannot attach, and with the workload alive it must not create a
/// second session for the pair.
#[test]
fn quick_launch_refuses_a_workspace_tag_whose_workload_is_still_alive() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let session = harness.start(
        &workspace,
        "sleep",
        &["/bin/sh", "-c", "trap \"\" HUP TERM; sleep 300"],
    );
    let _cleanup = ProcessCleanup(vec![session.worker_pid, session.workload_pid]);
    kill_and_wait(session.worker_pid);
    assert!(
        process_alive(session.workload_pid),
        "fixture needs a surviving workload leader"
    );

    let refused = harness.quick_launch(&workspace, &["/bin/sleep", "300"]);
    assert!(
        !refused.status.success(),
        "`a -` orphaned a live workload to take its tag: stdout={}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("workspace+tag already belongs"), "{stderr}");

    let rows = harness.rows_for(&workspace, "sleep");
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["id"], session.id, "{}", rows[0]);
    assert!(harness.state_dir_exists(&session.id));
    assert!(
        process_alive(session.workload_pid),
        "`a -` signalled a workload it was refused permission to replace"
    );
}
