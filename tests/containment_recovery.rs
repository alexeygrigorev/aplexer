//! Product-path regressions for fail-closed broken-session recovery.

use aplexer::{atomic_write_json, FileLock, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION};
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
}

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

fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "{} did not appear", path.display());
}

fn process_alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Scope note -- this test asserts `a kill`'s and `a forget`'s contract
/// only: kill refuses a cleanup it cannot prove and preserves both evidence
/// directories, and only `--force` removes them. It is NOT a claim about
/// `a prune`, which deliberately supersedes that preservation for exactly
/// this state.
///
/// Measured, not assumed: killing the worker closes the PTY, so the leader
/// `sh` takes the resulting SIGHUP and dies with it -- only the `setsid`
/// descendant, which traps HUP, survives. Running `a prune` at the point
/// where this test checks the preserved directories therefore REAPS the
/// record (verified: `{"removed":["<id>"],
/// "removed_without_containment_proof":["<id>"],"retained_count":0}`),
/// leaving the descendant alive and unsignalled. That trade-off is the
/// subject of `aplexer::containment_reap_verdict`'s "Supersedes" note and is
/// pinned end to end by
/// `tests/prune_dead_records.rs::prune_reaps_a_record_whose_setsid_descendant_escaped`;
/// this test stays on kill/forget so the two contracts remain separable.
#[test]
fn kill_preserves_evidence_when_dead_unlimited_worker_loses_setsid_descendant() {
    assert!(Path::new("/usr/bin/setsid").is_file(), "setsid is required");
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let descendant_marker = harness.runtime.path().join("setsid-descendant-pid");
    let descendant_marker_text = descendant_marker.to_str().expect("UTF-8 marker path");
    let workspace_text = workspace.path().to_str().expect("UTF-8 workspace");

    let started = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace_text,
        "--tag",
        "broken-setsid",
        "--",
        "/bin/sh",
        "-c",
        "/usr/bin/setsid /bin/sh -c 'trap \"\" HUP TERM; echo $$ > \"$1\"; sleep 30' aplexer-descendant \"$1\" & wait",
        "aplexer-leader",
        descendant_marker_text,
    ]);
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let record: Value = serde_json::from_slice(&started.stdout).expect("start JSON");
    let id = record["id"].as_str().expect("session id");
    let worker_pid = record["worker_pid"].as_i64().expect("worker pid") as i32;
    wait_for_path(&descendant_marker);
    let descendant_pid = fs::read_to_string(&descendant_marker)
        .expect("read descendant marker")
        .trim()
        .parse::<i32>()
        .expect("descendant pid");
    let mut cleanup = ProcessCleanup(vec![descendant_pid, worker_pid]);

    let live_forget = harness.run(&["forget", id, "--force"]);
    assert!(
        !live_forget.status.success(),
        "forget must refuse a verified-live worker"
    );
    assert!(
        String::from_utf8_lossy(&live_forget.stderr).contains("still has a live worker"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&live_forget.stderr)
    );

    assert_eq!(unsafe { libc::kill(worker_pid, libc::SIGKILL) }, 0);
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_alive(worker_pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }

    let killed = harness.run(&["kill", id, "--signal", "KILL", "--grace-ms", "0"]);
    assert!(
        !killed.status.success(),
        "ambiguous cleanup reported success"
    );
    assert!(
        String::from_utf8_lossy(&killed.stderr).contains("no authoritative containment locator"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&killed.stderr)
    );
    assert!(
        process_alive(descendant_pid),
        "setsid descendant was not preserved"
    );
    assert!(
        harness.state.path().join("sessions").join(id).exists(),
        "durable evidence was removed"
    );
    assert!(
        harness.runtime.path().join("sessions").join(id).exists(),
        "runtime evidence was removed"
    );

    // A SIGKILL'd worker adopted by an outer aplexer subreaper can linger
    // as a zombie (`worker_alive` stays true). This test is about kill's
    // refusal and forget's force-removal of evidence, not about waiting
    // for that zombie to be reaped.
    let record_path = harness
        .state
        .path()
        .join("sessions")
        .join(id)
        .join("session.json");
    let mut persisted: Value =
        serde_json::from_slice(&fs::read(&record_path).expect("read record before forget"))
            .expect("parse record before forget");
    persisted["worker_pid"] = Value::Null;
    fs::write(
        &record_path,
        serde_json::to_vec(&persisted).expect("serialize record"),
    )
    .expect("clear zombie worker pid");

    let forgotten = harness.run(&["--json", "forget", id, "--force"]);
    assert!(
        forgotten.status.success(),
        "force-forget failed: {}",
        String::from_utf8_lossy(&forgotten.stderr)
    );
    let report: Value = serde_json::from_slice(&forgotten.stdout).expect("forget JSON");
    assert_eq!(report["id"], id);
    assert_eq!(report["forgotten"], true);
    assert_eq!(report["signalled"], false);
    assert_eq!(report["containment_proven_empty"], false);
    assert_eq!(report["workload_may_survive"], true);
    assert!(
        String::from_utf8_lossy(&forgotten.stderr).contains("workload processes may survive"),
        "missing survival warning: {}",
        String::from_utf8_lossy(&forgotten.stderr)
    );
    assert!(
        !harness.state.path().join("sessions").join(id).exists(),
        "durable evidence was not forgotten"
    );
    assert!(
        !harness.runtime.path().join("sessions").join(id).exists(),
        "runtime evidence was not forgotten"
    );
    assert!(
        process_alive(descendant_pid),
        "forget must not signal a possibly surviving workload"
    );
    let snapshot = harness.run(&["snapshot"]);
    assert!(snapshot.status.success(), "snapshot failed");
    let snapshot: Value =
        serde_json::from_slice(&snapshot.stdout).expect("snapshot is always JSON without --json");
    assert_eq!(snapshot, serde_json::json!([]));

    let restarted = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace_text,
        "--tag",
        "broken-setsid",
        "--",
        "/bin/sh",
        "-c",
        "sleep 30",
    ]);
    assert!(
        restarted.status.success(),
        "forgotten tag was not reusable: {}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    let restarted: Value = serde_json::from_slice(&restarted.stdout).expect("restart JSON");
    cleanup.0.push(
        restarted["worker_pid"]
            .as_i64()
            .expect("restarted worker pid") as i32,
    );
}

#[test]
fn forget_fences_pre_pid_startup_and_refuses_held_worker_lock() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace_text = workspace.path().to_str().expect("UTF-8 workspace");
    let paths = Paths {
        runtime_root: harness.runtime.path().to_path_buf(),
        state_root: harness.state.path().to_path_buf(),
        config_file: harness.config.clone(),
    };
    paths.ensure().unwrap();
    let id = Uuid::now_v7();
    let workspace_path = workspace
        .path()
        .canonicalize()
        .unwrap_or_else(|_| workspace.path().to_path_buf());
    let record = SessionRecord {
        parent_session: None,
        schema_version: SCHEMA_VERSION,
        id,
        workspace: workspace_path,
        tag: "pre-pid".into(),
        engine: "shell".into(),
        profile: None,
        command: vec!["/bin/sh".into()],
        cwd: workspace.path().to_path_buf(),
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
        phase: Phase::Starting,
        worker_pid: None,
        workload_pid: None,
        worker_cgroup: None,
        workload_cgroup: None,
        containment_cgroup: None,
        containment_cgroup_identity: None,
        containment_empty: Some(false),
        socket_path: paths.socket(id),
        history_path: paths.history(id),
        exit: None,
        error: None,
    };
    fs::create_dir_all(paths.state_session(id)).unwrap();
    atomic_write_json(&paths.record(id), &record).expect("write stale Starting record");
    fs::create_dir_all(paths.runtime_session(id)).expect("recreate stale runtime dir");
    fs::write(paths.worker_lock(id), b"").expect("create historical worker lock");

    let held_lock = FileLock::exclusive(&paths.worker_lock(id), true).expect("hold worker lock");
    let refused = harness.run(&["forget", &id.to_string(), "--force"]);
    assert!(!refused.status.success(), "held worker lock was ignored");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("still has a worker holding"),
        "unexpected refusal: {}",
        String::from_utf8_lossy(&refused.stderr)
    );
    drop(held_lock);
    fs::remove_file(paths.worker_lock(id)).expect("remove historical worker lock");

    let forgotten = harness.run(&["forget", &id.to_string(), "--force"]);
    assert!(
        forgotten.status.success(),
        "missing lock could not be safely fenced and reclaimed: {}",
        String::from_utf8_lossy(&forgotten.stderr)
    );
    let restarted = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace_text,
        "--tag",
        "pre-pid",
        "--",
        "/bin/sh",
        "-c",
        "sleep 30",
    ]);
    assert!(
        restarted.status.success(),
        "reclaimed pre-PID tag was not reusable: {}",
        String::from_utf8_lossy(&restarted.stderr)
    );
    let restarted: Value = serde_json::from_slice(&restarted.stdout).expect("restart JSON");
    let restarted_worker = restarted["worker_pid"].as_i64().expect("worker pid") as i32;
    let _cleanup = ProcessCleanup(vec![restarted_worker]);
}
