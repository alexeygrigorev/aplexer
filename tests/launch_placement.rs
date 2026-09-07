//! Worker/workload cgroup recording and placement classification, end to
//! end (issue #1).
//!
//! The 2026-08-27 user-manager exit proved two things at once: aplexer had
//! no record of *where* its workers lived (the dead `yolo` session's
//! cgroups could not even be named after the fact), and it did not tell
//! anyone when the only placement available was one `systemctl --user exit`
//! can destroy. These tests pin the fix at the wire level: a real session
//! started through the real binary carries its actual `/proc` cgroups in
//! the durable record and in every machine-visible row, and `a doctor`
//! reports the placement of the context commands are launched from.
//!
//! Harness style follows tests/fresh_start.rs (direct CLI, real sessions,
//! isolated state/runtime dirs).

use aplexer::placement;
use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
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

    fn json(&self, args: &[&str]) -> Value {
        let output = self.command().args(args).output().expect("run aplexer CLI");
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

    fn start(&self, workspace: &TempDir, tag: &str) -> Value {
        self.json(&[
            "--json",
            "start",
            "--workspace",
            workspace.path().to_str().expect("UTF-8 workspace"),
            "--tag",
            tag,
            "--",
            "/bin/sleep",
            "300",
        ])
    }

    fn kill(&self, id: &str) {
        let _ = self.command().args(["kill", id]).output();
    }
}

/// This test process's cgroup, which every process below it (the `a` CLI,
/// the worker, the workload) inherits: the exact value the record must
/// carry for an unlimited session launched from here.
fn own_cgroup() -> String {
    placement::read_process_cgroup(std::process::id())
        .expect("this test process must have a readable /proc/<pid>/cgroup")
}

/// The core contract: the worker records where it actually is while it can
/// still read it, and the fact survives into `a status --json` /
/// `a list --json` with its derived classification -- so a post-mortem of a
/// manager-wide kill can name the failure domain that did it (issue #1's
/// `yolo` session could not).
#[test]
fn record_and_queries_carry_the_real_worker_and_workload_cgroups() {
    let harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let own = own_cgroup();
    let expected_placement = placement::classify_cgroup_path(&own);

    let started = harness.start(&workspace, "placement");
    let id = started["id"].as_str().expect("session id").to_string();

    // The durable record on disk: worker and workload read their cgroups
    // from /proc at launch. An unlimited session moves nothing, so both are
    // the ambient cgroup of this test process.
    let record_path = harness
        .state
        .path()
        .join("sessions")
        .join(&id)
        .join("session.json");
    let record: Value = serde_json::from_slice(
        &std::fs::read(&record_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", record_path.display())),
    )
    .expect("parse session record");
    assert_eq!(
        record["worker_cgroup"], own,
        "worker must record the cgroup it actually runs in"
    );
    assert_eq!(
        record["workload_cgroup"], own,
        "an unlimited workload shares its worker's cgroup"
    );

    // `a status --json`: recorded paths plus the derived placement summary,
    // classified identically to this test's own classification of the same
    // path -- the command and the classifier cannot disagree.
    let status = harness.json(&["--json", "status", &id]);
    assert_eq!(status["worker_cgroup"], own);
    assert_eq!(status["workload_cgroup"], own);
    assert_eq!(
        status["worker_placement"]["placement"],
        expected_placement.name()
    );
    assert_eq!(
        status["worker_placement"]["vulnerable_to_user_manager_exit"],
        expected_placement.vulnerable_to_user_manager_exit()
    );
    assert_eq!(status["workload_placement"]["cgroup"], own);

    // `a list --json` rows carry the same facts from the same helper.
    let list = harness.json(&["--json", "list"]);
    let row = list
        .as_array()
        .expect("list rows")
        .iter()
        .find(|row| row["id"] == id.as_str())
        .expect("started session listed");
    assert_eq!(
        row["worker_placement"]["placement"],
        expected_placement.name()
    );

    harness.kill(&id);
}

/// Doctor's `launch_placement` check: reports the placement of the context
/// the commands run in, names the escape, and stays warning-severity --
/// a vulnerable placement is a risk to explain, never a host that fails
/// checkup (issue #1: "warn or fail clearly"; aplexer warns).
#[test]
fn doctor_reports_launch_placement_as_an_advisory_check() {
    let harness = Harness::new();
    let own = own_cgroup();
    let expected = placement::classify_cgroup_path(&own);

    let report = harness.json(&["--json", "doctor"]);
    let check = report["checks"]
        .as_array()
        .expect("doctor checks")
        .iter()
        .find(|check| check["name"] == "launch_placement")
        .expect("doctor must carry a launch_placement check");
    assert_eq!(check["own_cgroup"], own);
    assert_eq!(check["own_placement"], expected.name());
    assert_eq!(
        check["vulnerable_to_user_manager_exit"],
        expected.vulnerable_to_user_manager_exit()
    );
    // Warning-severity in the vulnerable case, plain ok otherwise; either
    // way doctor's overall verdict stays green and the escape is named.
    if expected.vulnerable_to_user_manager_exit() {
        assert_eq!(check["severity"], "warning");
        assert_eq!(check["ok"], false);
        assert!(check["advice"].is_null(), "vulnerable placement advises");
    } else {
        assert_eq!(check["ok"], true);
    }
    assert_eq!(
        check["escape"]["env"],
        placement::LAUNCH_SYSTEM_SCOPE_ENV,
        "doctor must name the opt-in escape"
    );
    assert_eq!(report["ok"], true, "advisory checks never fail doctor");
}

/// Whether the opted-in escape can actually work here: creating a system
/// manager scope needs root or polkit authorization, which stock hosts
/// deny to regular users. Probe once, exactly like the launch path does.
fn system_scope_escape_works() -> bool {
    aplexer::probe_system_scope_backend().is_ok()
}

/// The opt-in escape (APLEXER_LAUNCH_SYSTEM_SCOPE=system) must never turn
/// into a broken start. When the backend works, the worker is re-parented
/// into a system-manager scope outside the per-user manager's subtree;
/// when it does not (the stock case for a regular user), the session still
/// starts exactly as before, in the ambient cgroup, and `a start` says why
/// on stderr. Skipped where systemd-run itself is missing, mirroring how
/// the repo quarantines live-systemd coverage (README Validation).
#[test]
fn system_scope_opt_in_escapes_or_degrades_with_a_warning() {
    let systemd_run = Command::new("systemd-run").arg("--version").output();
    if systemd_run.is_err() {
        eprintln!("skipping: systemd-run is not installed");
        return;
    }
    let escape_works = system_scope_escape_works();

    let harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let own = own_cgroup();
    let output = harness
        .command()
        .env(
            placement::LAUNCH_SYSTEM_SCOPE_ENV,
            placement::LAUNCH_SYSTEM_SCOPE_VALUE,
        )
        .args([
            "--json",
            "start",
            "--workspace",
            workspace.path().to_str().expect("UTF-8 workspace"),
            "--tag",
            "escape",
            "--",
            "/bin/sleep",
            "300",
        ])
        .output()
        .expect("run a start");
    assert!(
        output.status.success(),
        "the opt-in escape must never break a start: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let started: Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = started["id"].as_str().expect("session id").to_string();

    let record_path = harness
        .state
        .path()
        .join("sessions")
        .join(&id)
        .join("session.json");
    let record: Value = serde_json::from_slice(&std::fs::read(&record_path).expect("read record"))
        .expect("parse session record");

    let stderr = String::from_utf8_lossy(&output.stderr);
    if escape_works {
        let worker_cgroup = record["worker_cgroup"].as_str().expect("worker cgroup");
        let placement = placement::classify_cgroup_path(worker_cgroup);
        assert_eq!(
            placement,
            placement::CgroupPlacement::SystemSlice,
            "an escaped worker lands in a system-manager scope, not {worker_cgroup}"
        );
        assert_ne!(
            worker_cgroup, own,
            "the escape must actually move the worker out of the ambient cgroup"
        );
    } else {
        assert_eq!(
            record["worker_cgroup"], own,
            "without a working backend the worker stays in the ambient cgroup"
        );
        assert!(
            stderr.contains("APLEXER_LAUNCH_SYSTEM_SCOPE"),
            "the degraded escape must say why on stderr: {stderr}"
        );
    }

    harness.kill(&id);
}
