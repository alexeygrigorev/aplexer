#[cfg(feature = "startup-test-hooks")]
use aplexer::read_persisted_history_tail;
use serde_json::Value;
use std::fs;
#[cfg(feature = "startup-test-hooks")]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
#[cfg(feature = "startup-test-hooks")]
use std::process::Stdio;
use std::process::{Command, Output};
#[cfg(feature = "startup-test-hooks")]
use std::thread;
#[cfg(feature = "startup-test-hooks")]
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime_dir: TempDir,
    state_dir: TempDir,
    config_file: PathBuf,
}

#[cfg(feature = "startup-test-hooks")]
struct ProcessGroupCleanup(Option<i32>);

#[cfg(feature = "startup-test-hooks")]
impl ProcessGroupCleanup {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(feature = "startup-test-hooks")]
impl Drop for ProcessGroupCleanup {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

impl Harness {
    fn new() -> Self {
        let runtime_dir = TempDir::new().expect("runtime tempdir");
        let state_dir = TempDir::new().expect("state tempdir");
        let config_file = runtime_dir.path().join("config.toml");
        Self {
            runtime_dir,
            state_dir,
            config_file,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_env(args, &[])
    }

    fn run_with_env(&self, args: &[&str], environment: &[(&str, &str)]) -> Output {
        self.command_with_env(args, environment)
            .output()
            .expect("run aplexer CLI")
    }

    fn command_with_env(&self, args: &[&str], environment: &[(&str, &str)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime_dir.path())
            .env("APLEXER_STATE_DIR", self.state_dir.path())
            .env("APLEXER_CONFIG", &self.config_file)
            .envs(environment.iter().copied())
            .args(args);
        command
    }

    fn assert_no_session_artifacts(&self) {
        assert_directory_empty(&self.state_dir.path().join("sessions"));
        assert_directory_empty(&self.runtime_dir.path().join("sessions"));
    }
}

fn assert_directory_empty(path: &Path) {
    let entries = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(entries.is_empty(), "{} is not empty", path.display());
}

#[cfg(feature = "startup-test-hooks")]
fn assert_process_exited(pid: u32) {
    let process_path = PathBuf::from(format!("/proc/{pid}"));
    let deadline = Instant::now() + Duration::from_secs(1);
    while process_path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_path.exists(),
        "workload process {pid} survived startup rollback"
    );
}

#[cfg(feature = "startup-test-hooks")]
fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "{} did not appear", path.display());
}

#[cfg(feature = "startup-test-hooks")]
fn limit_open_files(command: &mut Command, soft_limit: libc::rlim_t) {
    unsafe {
        command.pre_exec(move || {
            let mut limit: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            limit.rlim_cur = limit.rlim_cur.min(soft_limit);
            if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(feature = "startup-test-hooks")]
fn process_state(pid: u32) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rfind(')')
        .and_then(|end| stat.get(end + 1..))?
        .split_whitespace()
        .next()?
        .chars()
        .next()
}

#[test]
fn zero_timeout_rolls_back_and_same_tag_can_retry() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");

    let timed_out = harness.run(&[
        "start",
        "--workspace",
        workspace,
        "--tag",
        "retry",
        "--startup-timeout-ms",
        "0",
        "--",
        "/bin/sleep",
        "30",
    ]);
    assert!(!timed_out.status.success(), "zero-timeout start succeeded");
    assert!(
        String::from_utf8_lossy(&timed_out.stderr).contains("within 0 ms"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&timed_out.stderr)
    );
    harness.assert_no_session_artifacts();

    let retry = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace,
        "--tag",
        "retry",
        "--",
        "/bin/sleep",
        "30",
    ]);
    assert!(
        retry.status.success(),
        "same-tag retry failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let record: Value = serde_json::from_slice(&retry.stdout).expect("retry JSON record");
    let id = record["id"].as_str().expect("retry session id");

    let killed = harness.run(&["kill", id, "--signal", "KILL", "--grace-ms", "0"]);
    assert!(
        killed.status.success(),
        "cleanup kill failed: {}",
        String::from_utf8_lossy(&killed.stderr)
    );
}

#[test]
fn corrupt_registry_entry_blocks_new_session_start() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let corrupt_id = uuid::Uuid::new_v4();
    let corrupt_dir = harness
        .state_dir
        .path()
        .join("sessions")
        .join(corrupt_id.to_string());
    fs::create_dir_all(&corrupt_dir).unwrap();
    fs::write(corrupt_dir.join("session.json"), b"{truncated").unwrap();

    let output = harness.run(&[
        "start",
        "--workspace",
        workspace,
        "--tag",
        "must-not-bypass-corruption",
        "--",
        "/bin/sleep",
        "30",
    ]);

    assert!(
        !output.status.success(),
        "startup bypassed corrupt registry"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(&corrupt_id.to_string()), "{stderr}");
    assert!(stderr.contains("parse"), "{stderr}");
    assert_eq!(
        fs::read_dir(harness.state_dir.path().join("sessions"))
            .unwrap()
            .count(),
        1,
        "startup published a second session despite registry corruption"
    );
    assert_directory_empty(&harness.runtime_dir.path().join("sessions"));
}

#[test]
fn failed_replacement_preserves_finished_session_evidence() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");

    let old = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace,
        "--tag",
        "daily",
        "--",
        "/bin/sh",
        "-c",
        "printf 'important-old-history\\n'",
    ]);
    assert!(
        old.status.success(),
        "old session failed: {}",
        String::from_utf8_lossy(&old.stderr)
    );
    let old: Value = serde_json::from_slice(&old.stdout).expect("old session JSON");
    let old_id = old["id"].as_str().expect("old session id");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let snapshot = harness.run(&["snapshot"]);
        let records: Value = serde_json::from_slice(&snapshot.stdout).expect("snapshot JSON");
        let finished = records.as_array().is_some_and(|records| {
            records.iter().any(|record| {
                record["id"] == old_id
                    && record["phase"] == "exited"
                    && record["worker_alive"] == false
            })
        });
        if finished {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "old session did not finish"
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let capture_before = harness.run(&["capture", old_id]);
    assert_eq!(capture_before.stdout, b"important-old-history\r\n");

    let replacement = harness.run(&[
        "start",
        "--startup-timeout-ms",
        "0",
        "--workspace",
        workspace,
        "--tag",
        "daily",
        "--",
        "/bin/sleep",
        "30",
    ]);
    assert!(
        !replacement.status.success(),
        "replacement unexpectedly started"
    );
    assert!(String::from_utf8_lossy(&replacement.stderr).contains("within 0 ms"));

    let capture_after = harness.run(&["capture", old_id]);
    assert!(
        capture_after.status.success(),
        "old capture was lost: {}",
        String::from_utf8_lossy(&capture_after.stderr)
    );
    assert_eq!(capture_after.stdout, capture_before.stdout);
    assert!(
        harness
            .state_dir
            .path()
            .join("sessions")
            .join(old_id)
            .exists(),
        "failed replacement removed old durable state"
    );

    let successful = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace,
        "--tag",
        "daily",
        "--",
        "/bin/sleep",
        "30",
    ]);
    assert!(
        successful.status.success(),
        "successful replacement failed: {}",
        String::from_utf8_lossy(&successful.stderr)
    );
    let successful: Value = serde_json::from_slice(&successful.stdout).expect("replacement JSON");
    assert!(
        !harness
            .state_dir
            .path()
            .join("sessions")
            .join(old_id)
            .exists(),
        "ready replacement did not retire old durable state"
    );
    let replacement_id = successful["id"].as_str().expect("replacement id");
    let killed = harness.run(&[
        "kill",
        replacement_id,
        "--signal",
        "KILL",
        "--grace-ms",
        "0",
    ]);
    assert!(killed.status.success(), "replacement cleanup failed");
}

#[test]
#[cfg(feature = "startup-test-hooks")]
fn replacement_cleanup_failure_keeps_one_usable_session_and_archived_evidence() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");

    let old = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace,
        "--tag",
        "daily",
        "--",
        "/bin/sh",
        "-c",
        "printf 'archived-evidence\\n'",
    ]);
    assert!(
        old.status.success(),
        "old session failed: {}",
        String::from_utf8_lossy(&old.stderr)
    );
    let old: Value = serde_json::from_slice(&old.stdout).expect("old session JSON");
    let old_id = old["id"].as_str().expect("old session id");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = harness.run(&["snapshot"]);
        let records: Value = serde_json::from_slice(&snapshot.stdout).expect("snapshot JSON");
        if records.as_array().is_some_and(|records| {
            records.iter().any(|record| {
                record["id"] == old_id
                    && record["phase"] == "exited"
                    && record["worker_alive"] == false
            })
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "old session did not finish");
        thread::sleep(Duration::from_millis(25));
    }

    let replacement = harness.run_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "daily",
            "--",
            "/bin/sleep",
            "30",
        ],
        &[("APLEXER_TEST_FAIL_SUPERSEDED_CLEANUP", "1")],
    );
    assert!(
        !replacement.status.success(),
        "replacement ignored cleanup failure"
    );
    let stderr = String::from_utf8_lossy(&replacement.stderr);
    assert!(stderr.contains("ready and manageable by UUID"), "{stderr}");
    assert!(stderr.contains(old_id), "{stderr}");
    assert!(stderr.contains("retired-sessions"), "{stderr}");

    let snapshot = harness.run(&["snapshot"]);
    assert!(
        snapshot.status.success(),
        "registry became unreadable: {}",
        String::from_utf8_lossy(&snapshot.stderr)
    );
    let records: Value = serde_json::from_slice(&snapshot.stdout).expect("snapshot JSON");
    let matching = records
        .as_array()
        .expect("snapshot array")
        .iter()
        .filter(|record| record["workspace"] == workspace && record["tag"] == "daily")
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "replacement left duplicate selectors");
    let replacement_id = matching[0]["id"].as_str().expect("replacement session id");
    assert_ne!(replacement_id, old_id);

    let status = harness.run(&["status", replacement_id, "--json"]);
    assert!(
        status.status.success(),
        "replacement is not manageable: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let archived = harness
        .state_dir
        .path()
        .join("retired-sessions")
        .join(old_id);
    assert!(archived.join("session.json").exists());
    assert_eq!(
        fs::read(archived.join("history.bin")).expect("read archived history"),
        b"archived-evidence\r\n"
    );
    assert_eq!(
        read_persisted_history_tail(&archived.join("history.bin"), None)
            .expect("recover archived v2 history"),
        b"archived-evidence\r\n"
    );
    assert!(!harness
        .state_dir
        .path()
        .join("sessions")
        .join(old_id)
        .exists());

    let killed = harness.run(&[
        "kill",
        replacement_id,
        "--signal",
        "KILL",
        "--grace-ms",
        "0",
    ]);
    assert!(killed.status.success(), "replacement cleanup failed");
}

#[test]
#[cfg(feature = "startup-test-hooks")]
fn persisted_running_without_verified_ping_is_not_startup_success() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");

    let output = harness.run_with_env(
        &[
            "start",
            "--startup-timeout-ms",
            "3000",
            "--workspace",
            workspace,
            "--tag",
            "not-ready",
            "--",
            "/bin/sleep",
            "30",
        ],
        &[(
            "APLEXER_TEST_FAIL_WORKER_STARTUP_AT",
            "after_running_record",
        )],
    );

    assert!(
        !output.status.success(),
        "launcher accepted Running without a successful Ping"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("after_running_record"),
        "unexpected error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    harness.assert_no_session_artifacts();
}

#[test]
#[cfg(feature = "startup-test-hooks")]
fn timeout_after_workload_spawn_does_not_orphan_workload() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let marker = harness.runtime_dir.path().join("spawned-workload-pid");
    let marker = marker.to_str().expect("UTF-8 marker path");
    let descendant_marker = harness.runtime_dir.path().join("spawned-descendant-pids");
    let descendant_marker = descendant_marker
        .to_str()
        .expect("UTF-8 descendant marker path");

    let started = Instant::now();
    let timed_out = harness.run_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "hard-hang",
            "--startup-timeout-ms",
            "1000",
            "--",
            "/bin/sh",
            "-c",
            ": > \"$1\"; i=0; while [ \"$i\" -lt 64 ]; do sleep 10 & echo $! >> \"$1\"; i=$((i + 1)); done; wait",
            "aplexer-startup-test",
            descendant_marker,
        ],
        &[
            (
                "APLEXER_TEST_HANG_WORKER_STARTUP_AT",
                "after_workload_spawn",
            ),
            ("APLEXER_TEST_WORKER_STARTUP_MARKER", marker),
        ],
    );
    let workload_pid: u32 = fs::read_to_string(marker)
        .expect("post-spawn marker was written")
        .trim()
        .parse()
        .expect("marker contains workload PID");
    assert!(!timed_out.status.success(), "paused startup succeeded");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "bounded rollback exceeded its single hard-cleanup deadline"
    );
    assert!(
        String::from_utf8_lossy(&timed_out.stderr).contains("within 1000 ms"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&timed_out.stderr)
    );

    let descendant_pids = fs::read_to_string(descendant_marker)
        .expect("workload descendant marker was written")
        .split_whitespace()
        .map(|pid| pid.parse::<u32>().expect("descendant marker contains PIDs"))
        .collect::<Vec<_>>();
    assert_eq!(descendant_pids.len(), 64, "all descendants were spawned");
    assert_process_exited(workload_pid);
    for descendant_pid in descendant_pids {
        assert_process_exited(descendant_pid);
    }
    harness.assert_no_session_artifacts();

    let retry = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace,
        "--tag",
        "hard-hang",
        "--",
        "/bin/sleep",
        "30",
    ]);
    assert!(
        retry.status.success(),
        "same-tag retry failed: {}",
        String::from_utf8_lossy(&retry.stderr)
    );
    let record: Value = serde_json::from_slice(&retry.stdout).expect("retry JSON record");
    let id = record["id"].as_str().expect("retry session id");
    let killed = harness.run(&["kill", id, "--signal", "KILL", "--grace-ms", "0"]);
    assert!(
        killed.status.success(),
        "retry cleanup kill failed: {}",
        String::from_utf8_lossy(&killed.stderr)
    );
}

#[test]
#[cfg(feature = "startup-test-hooks")]
fn pidfd_budget_failure_resumes_tree_and_preserves_evidence() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let marker = harness.runtime_dir.path().join("budget-workload-pid");
    let marker_text = marker.to_str().expect("UTF-8 marker path");
    let mut command = harness.command_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "pidfd-budget",
            "--startup-timeout-ms",
            "1000",
            "--",
            "/bin/sh",
            "-c",
            "i=0; while [ \"$i\" -lt 32 ]; do sleep 30 & i=$((i + 1)); done; wait",
        ],
        &[
            (
                "APLEXER_TEST_HANG_WORKER_STARTUP_AT",
                "after_workload_spawn",
            ),
            ("APLEXER_TEST_WORKER_STARTUP_MARKER", marker_text),
        ],
    );
    // This leaves room for normal startup and the worker pidfd, but not for
    // pinning the hostile tree. The cleanup path must fail closed and resume
    // everything it stopped instead of exhausting RLIMIT_NOFILE.
    limit_open_files(&mut command, 24);
    let output = command.output().expect("run descriptor-limited start");
    assert!(!output.status.success(), "limited startup succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("safe descendant limit") && stderr.contains("resumed"),
        "unexpected stderr: {stderr}"
    );

    let record_path = fs::read_dir(harness.state_dir.path().join("sessions"))
        .expect("read preserved state sessions")
        .next()
        .expect("preserved startup session")
        .expect("read startup session entry")
        .path()
        .join("session.json");
    let record: Value =
        serde_json::from_slice(&fs::read(record_path).expect("read record")).expect("parse record");
    let worker_pid = record["worker_pid"].as_u64().expect("worker pid") as i32;
    let workload_pid = fs::read_to_string(marker)
        .expect("read workload marker")
        .trim()
        .parse::<i32>()
        .expect("workload pid");
    let mut worker_cleanup = ProcessGroupCleanup(Some(worker_pid));
    let mut workload_cleanup = ProcessGroupCleanup(Some(workload_pid));
    assert!(!matches!(process_state(worker_pid as u32), Some('T' | 't')));
    assert!(!matches!(
        process_state(workload_pid as u32),
        Some('T' | 't')
    ));

    unsafe {
        libc::kill(-workload_pid, libc::SIGKILL);
        libc::kill(-worker_pid, libc::SIGKILL);
    }
    assert_process_exited(workload_pid as u32);
    assert_process_exited(worker_pid as u32);
    workload_cleanup.disarm();
    worker_cleanup.disarm();
}

#[test]
#[cfg(feature = "startup-test-hooks")]
fn externally_reaped_worker_preserves_ambiguous_containment_evidence() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let marker = harness
        .runtime_dir
        .path()
        .join("externally-killed-workload-pid");
    let marker_text = marker.to_str().expect("UTF-8 marker path");
    let mut command = harness.command_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "ambiguous-exit",
            "--startup-timeout-ms",
            "5000",
            "--",
            "/bin/sleep",
            "30",
        ],
        &[
            (
                "APLEXER_TEST_HANG_WORKER_STARTUP_AT",
                "after_workload_spawn",
            ),
            ("APLEXER_TEST_WORKER_STARTUP_MARKER", marker_text),
        ],
    );
    let start = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn start command");
    wait_for_path(&marker);
    let workload_pid: i32 = fs::read_to_string(&marker)
        .expect("read workload marker")
        .trim()
        .parse()
        .expect("workload marker contains PID");
    let mut workload_cleanup = ProcessGroupCleanup(Some(workload_pid));

    let session_dir = fs::read_dir(harness.state_dir.path().join("sessions"))
        .expect("read state sessions")
        .next()
        .expect("startup session directory")
        .expect("read startup session entry")
        .path();
    let record_path = session_dir.join("session.json");
    let record: Value = serde_json::from_slice(
        &fs::read(&record_path).expect("read startup record before killing worker"),
    )
    .expect("parse startup record");
    let worker_pid = record["worker_pid"].as_u64().expect("recorded worker pid") as i32;
    assert_eq!(unsafe { libc::kill(worker_pid, libc::SIGKILL) }, 0);

    let output = start.wait_with_output().expect("wait for failed start");
    assert!(
        !output.status.success(),
        "externally killed start succeeded"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not be confirmed"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        record_path.exists(),
        "ambiguous durable containment evidence was deleted"
    );
    assert!(
        harness
            .runtime_dir
            .path()
            .join("sessions")
            .read_dir()
            .unwrap()
            .next()
            .is_some(),
        "ambiguous runtime containment evidence was deleted"
    );

    let result = unsafe { libc::kill(-workload_pid, libc::SIGKILL) };
    assert!(
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH),
        "cleanup surviving workload failed: {}",
        std::io::Error::last_os_error()
    );
    assert_process_exited(workload_pid as u32);
    workload_cleanup.disarm();
}
