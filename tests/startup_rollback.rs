#[cfg(feature = "startup-test-hooks")]
use aplexer::read_persisted_history_tail;
use aplexer::{atomic_write_json, ExitInfo, Limits, Paths, Phase, SessionRecord, SCHEMA_VERSION};
use serde_json::Value;
use std::collections::BTreeMap;
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
use uuid::Uuid;

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

    /// A dead-but-listed predecessor: clean natural exits now delete their
    /// record, so reclaim/archive tests plant a finished leftover (the same
    /// shape Failed/OOM keep) instead of waiting for `true` to linger.
    fn plant_finished_session(&self, workspace: &Path, tag: &str, history: &[u8]) -> String {
        let paths = Paths {
            runtime_root: self.runtime_dir.path().to_path_buf(),
            state_root: self.state_dir.path().to_path_buf(),
            config_file: self.config_file.clone(),
        };
        paths.ensure().unwrap();
        let id = Uuid::now_v7();
        let workspace = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        let record = SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id,
            workspace,
            tag: tag.into(),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/sh".into()],
            cwd: self.state_dir.path().to_path_buf(),
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
            phase: Phase::Exited,
            worker_pid: None,
            workload_pid: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(true),
            socket_path: paths.socket(id),
            history_path: paths.history(id),
            exit: Some(ExitInfo {
                code: Some(0),
                signal: None,
                oom_killed: false,
                exited_at_ms: 1,
            }),
            error: None,
        };
        fs::create_dir_all(paths.state_session(id)).unwrap();
        fs::write(&record.history_path, history).unwrap();
        atomic_write_json(&paths.record(id), &record).unwrap();
        id.to_string()
    }
}

fn assert_directory_empty(path: &Path) {
    let entries = fs::read_dir(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(entries.is_empty(), "{} is not empty", path.display());
}

/// How long a poll for something that must eventually become true is allowed
/// to run before the test gives up.
///
/// This is a liveness backstop, not an assertion: every loop below exits the
/// instant its condition holds, so a generous budget cannot turn a real
/// failure into a pass -- a process that genuinely leaked never disappears,
/// and a marker that is genuinely never written never appears. All the budget
/// decides is how long a broken run takes to report itself, which is why it is
/// sized for a heavily contended CI runner rather than for an idle laptop. It
/// is deliberately NOT used for `timeout_after_workload_spawn_does_not_orphan_workload`'s
/// rollback bound, which is a real assertion about a product deadline.
#[cfg(feature = "startup-test-hooks")]
const LIVENESS_BACKSTOP: Duration = Duration::from_secs(60);

/// True when `pid` is gone from the table or is only a zombie.
///
/// `kill(pid, 0)` and `/proc/<pid>` both still see a zombie, and this suite
/// often runs nested inside another aplexer worker -- a subreaper that
/// adopts the test's children and does not reap them promptly. A zombie is
/// an exited process, so treating it as still alive makes "did rollback
/// kill the tree?" an environment property instead of a test one.
#[cfg(feature = "startup-test-hooks")]
fn process_has_exited(pid: u32) -> bool {
    matches!(process_state(pid), None | Some('Z'))
}

#[cfg(feature = "startup-test-hooks")]
fn assert_process_exited(pid: u32) {
    let deadline = Instant::now() + LIVENESS_BACKSTOP;
    while !process_has_exited(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        process_has_exited(pid),
        "workload process {pid} survived startup rollback (state {:?})",
        process_state(pid)
    );
}

#[cfg(feature = "startup-test-hooks")]
fn wait_for_path(path: &Path) {
    let deadline = Instant::now() + LIVENESS_BACKSTOP;
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "{} did not appear", path.display());
}

/// Descriptors a process launched from here holds immediately after `exec`:
/// its three standard streams, plus every descriptor this test process itself
/// inherited without `FD_CLOEXEC`. CI runners routinely leak a couple of those
/// in, and the launcher's containment budget is computed against whatever it
/// finds open, so a descriptor budget that ignores them is really a budget for
/// one particular machine.
///
/// Descriptors this process opens for its own work (temp dirs, records, the
/// `/proc/self/fd` handle below) are all `FD_CLOEXEC`, so they are excluded:
/// they never reach the child, and excluding them also makes the count immune
/// to whatever sibling tests are doing on other threads.
#[cfg(feature = "startup-test-hooks")]
fn inherited_descriptor_count() -> libc::rlim_t {
    let mut inherited = std::collections::BTreeSet::from([0, 1, 2]);
    for entry in fs::read_dir("/proc/self/fd").expect("read open descriptors") {
        let entry = entry.expect("enumerate open descriptors");
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 && flags & libc::FD_CLOEXEC == 0 {
            inherited.insert(fd);
        }
    }
    inherited.len() as libc::rlim_t
}

/// Whether the launcher's stderr proves that its readiness loop caught the
/// worker being reaped out from under it, rather than any other startup
/// failure that also ends in preserved-but-unconfirmed containment.
///
/// `"could not be confirmed"` alone does not identify a code path: the
/// launcher emits it from `StartupGuard::rollback` for EVERY failure whose
/// cleanup could not be proven, including the ordinary startup-timeout path
/// (`pidfd_budget_failure_resumes_tree_and_preserves_evidence` ends with the
/// same words). Anchoring on it let `externally_reaped_worker_...` pass while
/// exercising the timeout path instead -- a silent false green, measurably
/// reproducible by having the launcher time out and letting the test's own
/// SIGKILL land inside the SIGTERM grace:
///
/// ```text
/// a: startup failed: worker did not become ready within 1000 ms; rollback also
/// failed: worker N exited before independent containment cleanup and left no
/// conclusive cleanup record; startup containment for <id> could not be
/// confirmed; preserved runtime and durable state
/// ```
///
/// Only the readiness loop's own `worker exited during startup: <status>` arm
/// reports the worker's termination status, and only an external reap makes
/// that status SIGKILL, so the pair identifies this path uniquely.
fn observed_external_worker_reap(stderr: &str) -> bool {
    stderr.contains("worker exited during startup") && stderr.contains("signal: 9")
}

/// The descendant budget the launcher reported refusing to exceed, parsed back
/// out of its own error text so the test can prove which budget it exercised.
#[cfg(feature = "startup-test-hooks")]
fn reported_descendant_limit(stderr: &str) -> Option<usize> {
    let tail = stderr.split_once("safe descendant limit of ")?.1;
    let digits = tail
        .split(|character: char| !character.is_ascii_digit())
        .next()?;
    digits.parse().ok()
}

/// `STARTUP_FD_RESERVE` plus the single descriptor the launcher keeps for the
/// worker pidfd: everything its containment budget subtracts before any
/// descendant can be pinned. Shared by both descriptor-budget tests so the two
/// cannot drift apart from each other or from `src/api.rs`.
#[cfg(feature = "startup-test-hooks")]
const LAUNCHER_FD_OVERHEAD: libc::rlim_t = 16 + 1;

/// A soft `RLIMIT_NOFILE` below every budget `pin_open_files` is asked for.
/// Dropping to this first forces the subsequent pin to *raise*, so a
/// clamp-only regression (`rlim_cur.min(soft_limit)`) cannot stay green on a
/// runner whose inherited soft limit is already larger than the target --
/// the GitHub-runner default of 1024, which is how finding 2 stayed latent.
#[cfg(feature = "startup-test-hooks")]
const STARVED_OPEN_FILES: libc::rlim_t = 8;

/// Set the current process's soft `RLIMIT_NOFILE` to `soft_limit`, raising
/// or lowering as needed, never above the hard limit.
#[cfg(feature = "startup-test-hooks")]
fn set_open_files_soft_limit(soft_limit: libc::rlim_t) -> std::io::Result<()> {
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    limit.rlim_cur = soft_limit.min(limit.rlim_max);
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Pin the launched process's soft `RLIMIT_NOFILE` to exactly `soft_limit`,
/// raising it as well as lowering it.
///
/// Clamping downward only (`rlim_cur.min(soft_limit)`) means the effective
/// budget is "whatever the runner had, if that happens to be smaller", so a
/// test that needs a *known* descriptor budget silently inherits the machine's
/// instead. Pinning cancels the ambient term in both directions: a test that
/// needs room to contain a large descendant tree still gets it under
/// `ulimit -n 64`, and a test that needs a starved budget still gets one under
/// `ulimit -n 1048576`.
///
/// The hard limit is checked here in the parent so an impossible request names
/// itself instead of surfacing as an opaque `spawn` failure from `pre_exec`.
/// The child first drops to `STARVED_OPEN_FILES` so the pin must raise even
/// when this process inherited a large soft limit; without that, CI only
/// ever exercises the lowering direction.
#[cfg(feature = "startup-test-hooks")]
fn pin_open_files(command: &mut Command, soft_limit: libc::rlim_t) {
    let mut current: libc::rlimit = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut current) },
        0,
        "read RLIMIT_NOFILE: {}",
        std::io::Error::last_os_error()
    );
    assert!(
        current.rlim_max >= soft_limit,
        "this test needs a soft RLIMIT_NOFILE of {soft_limit}, but the hard limit is {}",
        current.rlim_max
    );
    unsafe {
        command.pre_exec(move || {
            set_open_files_soft_limit(STARVED_OPEN_FILES)?;
            set_open_files_soft_limit(soft_limit)
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

    let old_id =
        harness.plant_finished_session(Path::new(workspace), "daily", b"important-old-history\r\n");
    let capture_before = harness.run(&["capture", &old_id]);
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

    let capture_after = harness.run(&["capture", &old_id]);
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
            .join(&old_id)
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
            .join(&old_id)
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

    let old_id =
        harness.plant_finished_session(Path::new(workspace), "daily", b"archived-evidence\r\n");

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
    assert!(stderr.contains(&old_id), "{stderr}");
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
        .join(&old_id);
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
        .join(&old_id)
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

/// A zombie predecessor: a session whose worker was SIGKILLed before it
/// could record an exit, so `phase` stays at `running` forever, holding
/// durable evidence (`marker` in its transcript) that a failed reclaim must
/// not destroy. Returns its session id.
///
/// The two tests below are the reclaim twins of the two replacement
/// transaction tests above: `start_session` reclaiming a `workspace+tag`
/// from a broken record runs the SAME archive -> start ->
/// restore-on-failure -> cleanup transaction, over a class of record that
/// transaction never used to see, so it needs the same fault injection.
fn zombie_predecessor(harness: &Harness, workspace: &str, tag: &str, marker: &str) -> String {
    let started = harness.run(&[
        "--json",
        "start",
        "--workspace",
        workspace,
        "--tag",
        tag,
        "--",
        "/bin/sh",
        "-c",
        &format!("printf '{marker}\\n'; exec sleep 300"),
    ]);
    assert!(
        started.status.success(),
        "predecessor failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let started: Value = serde_json::from_slice(&started.stdout).expect("predecessor JSON");
    let id = started["id"].as_str().expect("predecessor id").to_string();
    let worker_pid = started["worker_pid"].as_i64().expect("worker pid") as i32;
    let workload_pid = started["workload_pid"].as_i64().expect("workload pid") as i32;

    // The marker has to be DURABLE before the worker dies, not merely
    // visible: `a capture` answers from the live worker's in-memory ring
    // while the worker is up, so polling that would only prove the marker
    // existed somewhere that a SIGKILL erases. The evidence these tests
    // protect is the persisted transcript, so wait for that one.
    let history = harness
        .state_dir
        .path()
        .join("sessions")
        .join(&id)
        .join("history.bin");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let persisted = aplexer::read_persisted_history_tail(&history, None).unwrap_or_default();
        if persisted
            .windows(marker.len())
            .any(|window| window == marker.as_bytes())
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "predecessor never persisted its marker: {}",
            String::from_utf8_lossy(&persisted)
        );
        std::thread::sleep(std::time::Duration::from_millis(25));
    }

    // Worker first, so it never gets to write a terminal phase; then the
    // workload, so nothing survives that could legitimately retain the claim.
    // Liveness through `aplexer::process_alive`, not a bare `kill(pid, 0)`:
    // the signal probe succeeds for a zombie, and a worker killed here
    // reparents onto whatever child subreaper this suite runs under, so it
    // stays signalable until that subreaper reaps it.
    for pid in [worker_pid, workload_pid] {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while aplexer::process_alive(pid as u32) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!aplexer::process_alive(pid as u32), "pid {pid} did not die");
    }

    let snapshot = harness.run(&["snapshot"]);
    let records: Value = serde_json::from_slice(&snapshot.stdout).expect("snapshot JSON");
    let row = records
        .as_array()
        .expect("snapshot array")
        .iter()
        .find(|record| record["id"] == id)
        .expect("predecessor still listed")
        .clone();
    assert_eq!(
        row["phase"], "running",
        "fixture no longer reproduces the reported stale phase: {row}"
    );
    assert_eq!(row["worker_alive"], false, "{row}");
    assert_eq!(row["state"], "broken", "{row}");
    id
}

/// The reclaim twin of `failed_replacement_preserves_finished_session_evidence`.
/// A reclaim that fails after the predecessor was archived must put it back:
/// a zombie is still the only record of what the user was running, and
/// losing it to a failed `a start` would be strictly worse than the refusal
/// this change replaced.
#[test]
fn failed_reclaim_of_a_broken_record_preserves_its_evidence() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let old_id = zombie_predecessor(&harness, workspace, "daily", "important-old-history");

    let capture_before = harness.run(&["capture", &old_id]);
    assert!(
        String::from_utf8_lossy(&capture_before.stdout).contains("important-old-history"),
        "fixture lost its evidence before the test began"
    );

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

    let capture_after = harness.run(&["capture", &old_id]);
    assert!(
        capture_after.status.success(),
        "a failed reclaim destroyed the broken session's transcript: {}",
        String::from_utf8_lossy(&capture_after.stderr)
    );
    assert_eq!(capture_after.stdout, capture_before.stdout);
    assert!(
        harness
            .state_dir
            .path()
            .join("sessions")
            .join(&old_id)
            .exists(),
        "failed reclaim removed the broken session's durable state"
    );
    assert!(
        !harness
            .state_dir
            .path()
            .join("retired-sessions")
            .join(&old_id)
            .exists(),
        "failed reclaim left the predecessor stranded in the archive"
    );

    // And the reclaim still works on a retry -- the failure above did not
    // leave the registry in a state that blocks the pair permanently.
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
        "successful reclaim failed: {}",
        String::from_utf8_lossy(&successful.stderr)
    );
    let successful: Value = serde_json::from_slice(&successful.stdout).expect("replacement JSON");
    assert!(
        !harness
            .state_dir
            .path()
            .join("sessions")
            .join(&old_id)
            .exists(),
        "ready replacement did not retire the broken predecessor"
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

/// The reclaim twin of
/// `replacement_cleanup_failure_keeps_one_usable_session_and_archived_evidence`.
/// When the post-handoff archive cleanup fails, the registry must still hold
/// exactly one usable session for the pair and the broken predecessor's
/// evidence must be retained where the error message says it is.
#[test]
#[cfg(feature = "startup-test-hooks")]
fn reclaim_cleanup_failure_keeps_one_usable_session_and_archived_evidence() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let old_id = zombie_predecessor(&harness, workspace, "daily", "archived-evidence");

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
        "reclaim ignored cleanup failure"
    );
    let stderr = String::from_utf8_lossy(&replacement.stderr);
    assert!(stderr.contains("ready and manageable by UUID"), "{stderr}");
    assert!(stderr.contains(&old_id), "{stderr}");
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
    assert_eq!(matching.len(), 1, "reclaim left duplicate selectors");
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
        .join(&old_id);
    assert!(
        archived.join("session.json").exists(),
        "the broken predecessor's record is not where the error says it is"
    );
    assert!(
        read_persisted_history_tail(&archived.join("history.bin"), None)
            .expect("recover archived v2 history")
            .windows(b"archived-evidence".len())
            .any(|window| window == b"archived-evidence"),
        "the broken predecessor's transcript was lost"
    );
    assert!(!harness
        .state_dir
        .path()
        .join("sessions")
        .join(&old_id)
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

/// Prove `pin_open_files` actually raises. On a normal runner the inherited
/// soft limit is 1024, so both descriptor-budget tests only ever lower unless
/// the helper first starves the child; a clamp-only `.min()` would then leave
/// the child at `STARVED_OPEN_FILES` and this assertion would fail, instead of
/// staying green until someone reruns under `ulimit -S -n 64`.
#[test]
#[cfg(feature = "startup-test-hooks")]
fn pin_open_files_raises_from_a_starved_soft_limit() {
    const TARGET: libc::rlim_t = 256;
    let mut current: libc::rlimit = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut current) },
        0,
        "read RLIMIT_NOFILE: {}",
        std::io::Error::last_os_error()
    );
    assert!(
        current.rlim_max >= TARGET,
        "this test needs a hard RLIMIT_NOFILE of at least {TARGET}, got {}",
        current.rlim_max
    );
    assert!(
        TARGET > STARVED_OPEN_FILES,
        "target must be above the starved floor so the pin has to raise"
    );

    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "awk '/^Max open files/ { print $4 }' /proc/self/limits",
    ]);
    pin_open_files(&mut command, TARGET);
    let output = command.output().expect("spawn limit reporter");
    assert!(
        output.status.success(),
        "limit reporter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reported: libc::rlim_t = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap_or_else(|_| {
            panic!(
                "limit reporter did not print a soft limit: stdout={:?} stderr={:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
    assert_eq!(
        reported, TARGET,
        "pin_open_files left the child at {reported} instead of raising from \
         {STARVED_OPEN_FILES} to {TARGET}; a clamp-only helper would stay at the \
         starved floor"
    );
}

/// The workload shell plus the children it spawns: containment has to pin this
/// whole tree during rollback, so it also sets the descriptor budget the test
/// has to guarantee.
#[cfg(feature = "startup-test-hooks")]
const ORPHAN_WORKLOAD_CHILDREN: usize = 64;

/// `STARTUP_TERM_GRACE` and `STARTUP_CONTAINMENT_TIMEOUT` from `src/api.rs`,
/// mirrored here because they are private to the lib crate and this is a
/// black-box CLI test. Deliberately duplicated rather than exported, the same
/// way `tests/state_report.rs` mirrors `REPORTED_STATE_STALE_MS`: an
/// integration test asserting against the real CLI should not need a
/// lib-internal escape hatch, and a maintainer changing one is expected to
/// grep for the other.
#[cfg(feature = "startup-test-hooks")]
const STARTUP_TERM_GRACE: Duration = Duration::from_secs(3);
#[cfg(feature = "startup-test-hooks")]
const STARTUP_CONTAINMENT_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
#[cfg(feature = "startup-test-hooks")]
fn timeout_after_workload_spawn_does_not_orphan_workload() {
    const STARTUP_TIMEOUT_MS: u64 = 1_000;
    const ORPHAN_TREE_DESCENDANTS: libc::rlim_t = ORPHAN_WORKLOAD_CHILDREN as libc::rlim_t + 1;
    // Room for the handful of descriptors the launcher opens for its own work
    // between the pin and the containment preflight. Only slack: the budget
    // has to sit strictly ABOVE the tree here, because unlike
    // `pidfd_budget_failure_resumes_tree_and_preserves_evidence` this test
    // requires containment to SUCCEED.
    const DESCENDANT_BUDGET_SLACK: libc::rlim_t = 32;

    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let marker = harness.runtime_dir.path().join("spawned-workload-pid");
    let marker = marker.to_str().expect("UTF-8 marker path");
    let descendant_marker = harness.runtime_dir.path().join("spawned-descendant-pids");
    let descendant_marker = descendant_marker
        .to_str()
        .expect("UTF-8 descendant marker path");
    let workload = format!(
        ": > \"$1\"; i=0; while [ \"$i\" -lt {ORPHAN_WORKLOAD_CHILDREN} ]; \
         do sleep 10 & echo $! >> \"$1\"; i=$((i + 1)); done; wait"
    );
    let startup_timeout_ms = STARTUP_TIMEOUT_MS.to_string();

    let mut command = harness.command_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "hard-hang",
            "--startup-timeout-ms",
            &startup_timeout_ms,
            "--",
            "/bin/sh",
            "-c",
            &workload,
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
    // Containment needs one pidfd per descendant. With no rlimit pinned at all
    // this test spent whatever budget the runner happened to have: it passes at
    // `ulimit -n` 1024 and 128 and fails at 86, 80 and 64, leaving a live
    // 65-process tree behind and reporting "workload process N survived startup
    // rollback". Deriving the budget from the descriptors the launched process
    // actually inherits cancels the ambient term out (same technique as
    // `pidfd_budget_failure_resumes_tree_and_preserves_evidence`), so the
    // launcher gets room for exactly this tree plus its own reserve on any
    // runner.
    pin_open_files(
        &mut command,
        inherited_descriptor_count()
            + LAUNCHER_FD_OVERHEAD
            + ORPHAN_TREE_DESCENDANTS
            + DESCENDANT_BUDGET_SLACK,
    );
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn timing-out start command");
    // The worker writes this marker immediately before the injected hang, so
    // its appearance is the last observable moment before the launcher's
    // rollback window opens. Timing from here rather than from the spawn keeps
    // the machine-dependent prefix -- exec'ing the launcher, spawning the
    // worker, forking a 65-process tree -- out of a bound that is supposed to
    // measure the product's own deadline.
    wait_for_path(Path::new(marker));
    let rollback_window_opened = Instant::now();
    let timed_out = child.wait_with_output().expect("wait for timed-out start");
    let workload_pid: u32 = fs::read_to_string(marker)
        .expect("post-spawn marker was written")
        .trim()
        .parse()
        .expect("marker contains workload PID");
    assert!(!timed_out.status.success(), "paused startup succeeded");
    // Rollback makes ONE bounded pass, so it is bounded by the product's own
    // constants: at most the remainder of the startup timeout, then a single
    // SIGTERM grace, then a single hard-cleanup containment pass. Deriving the
    // bound from those terms instead of a round 8 seconds keeps the assertion
    // exactly as strong as the property it is testing -- a regression to a
    // retry loop still blows straight past it -- and makes it track the product
    // if those deadlines are ever retuned, rather than silently going slack or
    // spuriously red.
    let bounded_rollback = Duration::from_millis(STARTUP_TIMEOUT_MS)
        + STARTUP_TERM_GRACE
        + STARTUP_CONTAINMENT_TIMEOUT;
    let rollback_elapsed = rollback_window_opened.elapsed();
    assert!(
        rollback_elapsed < bounded_rollback,
        "bounded rollback took {rollback_elapsed:?}, past its single hard-cleanup \
         deadline of {bounded_rollback:?}"
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
    assert_eq!(
        descendant_pids.len(),
        ORPHAN_WORKLOAD_CHILDREN,
        "all descendants were spawned"
    );
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
    // The hostile workload shell plus the children it spawns. Containment has
    // to walk past this many descendants to pin the tree.
    const HOSTILE_WORKLOAD_CHILDREN: usize = 32;
    const HOSTILE_TREE_DESCENDANTS: usize = HOSTILE_WORKLOAD_CHILDREN + 1;
    // The descendant budget this test wants the launcher to end up with. Any
    // value in `1..HOSTILE_TREE_DESCENDANTS` exercises the intended path;
    // sitting near the middle leaves room in both directions for the handful
    // of descriptors the launcher opens for its own work before preflighting.
    const TARGET_DESCENDANT_BUDGET: libc::rlim_t = 16;

    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let marker = harness.runtime_dir.path().join("budget-workload-pid");
    let marker_text = marker.to_str().expect("UTF-8 marker path");
    let workload = format!(
        "i=0; while [ \"$i\" -lt {HOSTILE_WORKLOAD_CHILDREN} ]; do sleep 30 & i=$((i + 1)); done; wait"
    );
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
            &workload,
        ],
        &[
            (
                "APLEXER_TEST_HANG_WORKER_STARTUP_AT",
                "after_workload_spawn",
            ),
            ("APLEXER_TEST_WORKER_STARTUP_MARKER", marker_text),
        ],
    );
    // The launcher derives its containment budget as
    //   soft RLIMIT_NOFILE - open descriptors - STARTUP_FD_RESERVE - 1,
    // so a hardcoded soft limit silently spends part of the budget on however
    // many descriptors the environment leaked in, and decides which failure
    // path runs from ambient machine state rather than from the behaviour under
    // test. Paying for the inherited descriptors explicitly cancels that term
    // out: the launcher is left with `TARGET_DESCENDANT_BUDGET` minus its own
    // few working descriptors, whatever the environment looks like. That is
    // room for normal startup and the worker pidfd, but nowhere near enough to
    // pin the hostile tree, so the cleanup path must fail closed and resume
    // everything it stopped instead of exhausting RLIMIT_NOFILE.
    pin_open_files(
        &mut command,
        inherited_descriptor_count() + LAUNCHER_FD_OVERHEAD + TARGET_DESCENDANT_BUDGET,
    );
    let output = command.output().expect("run descriptor-limited start");
    assert!(!output.status.success(), "limited startup succeeded");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("safe descendant limit") && stderr.contains("resumed"),
        "unexpected stderr: {stderr}"
    );
    // Guard the budget arithmetic itself. Reaching the descendant limit proves
    // the budget landed inside `1..HOSTILE_TREE_DESCENDANTS`; re-checking it
    // here turns a future drift towards either edge into a diagnosable failure
    // instead of a silent slide onto the neighbouring preflight path.
    let budget = reported_descendant_limit(&stderr)
        .unwrap_or_else(|| panic!("descendant limit missing from cleanup failure: {stderr}"));
    assert!(
        (1..HOSTILE_TREE_DESCENDANTS).contains(&budget),
        "descendant budget {budget} left no room for startup, or enough to pin all \
         {HOSTILE_TREE_DESCENDANTS} hostile descendants: {stderr}"
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
    // Long enough that only a genuinely wedged launcher reaches it, and short
    // enough that such a launcher still fails the run instead of hanging it.
    let startup_timeout_ms = LIVENESS_BACKSTOP.as_millis().to_string();
    let mut command = harness.command_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "ambiguous-exit",
            // A liveness backstop, not a competitor. At 5000 ms this raced the
            // test's own spawn -> read-record -> SIGKILL sequence: a loaded box
            // that lost the race ran the startup-timeout path instead, and the
            // old `contains("could not be confirmed")` assertion passed anyway.
            // If it is ever reached now, the path anchor below reports it as a
            // failure rather than accepting it as a pass.
            "--startup-timeout-ms",
            &startup_timeout_ms,
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Prove which path ran before asserting what it preserved, so a neighbouring
    // failure mode cannot satisfy this test on the intended path's behalf.
    assert!(
        observed_external_worker_reap(&stderr),
        "startup did not fail through the external-reap path: {stderr}"
    );
    assert!(
        stderr.contains("could not be confirmed"),
        "unexpected stderr: {stderr}"
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

/// A workload that finishes instantly lets its worker record a terminal
/// phase, unlink the control socket and exit before `start_session`'s
/// readiness poll ever sees a live socket to Ping. That session started and
/// ran to completion; reporting it as "worker exited during startup: exit
/// status: 0" is wrong. `APLEXER_TEST_AWAIT_WORKER_EXIT_BEFORE_READINESS_POLL`
/// pins that interleaving so the assertion is deterministic instead of a
/// timing lottery the CI runner happened to lose once.
#[test]
#[cfg(feature = "startup-test-hooks")]
fn fast_workload_that_exits_before_readiness_probe_starts_successfully() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let output = harness.run_with_env(
        &[
            "--json",
            "start",
            "--workspace",
            workspace,
            "--tag",
            "fast-exit",
            "--startup-timeout-ms",
            "10000",
            "--",
            "/bin/sh",
            "-c",
            "printf 'fast-exit\\n'",
        ],
        &[("APLEXER_TEST_AWAIT_WORKER_EXIT_BEFORE_READINESS_POLL", "1")],
    );
    assert!(
        output.status.success(),
        "fast workload reported a startup failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: Value = serde_json::from_slice(&output.stdout).expect("start JSON record");
    assert_eq!(record["phase"], "exited", "unexpected phase: {record}");
    assert!(
        record["id"].as_str().is_some_and(|id| !id.is_empty()),
        "terminal record omitted the session id: {record}"
    );
    assert_eq!(
        record["containment_empty"], true,
        "completed session lacks containment proof: {record}"
    );

    // A clean finish removes the record; the session must not linger as an
    // exited row, and must not be rolled back as a failed startup either.
    let listed = harness.run(&["--json", "list"]);
    assert!(listed.status.success());
    let listed: Value = serde_json::from_slice(&listed.stdout).expect("list JSON");
    assert!(
        listed
            .as_array()
            .is_some_and(|rows| rows.iter().all(|row| row["id"] != record["id"])),
        "completed session lingered in the registry: {listed}"
    );
    let id = record["id"].as_str().unwrap();
    assert!(
        !harness.state_dir.path().join("sessions").join(id).exists(),
        "completed session left durable state"
    );
}

/// The clean-exit acceptance above is scoped to a durable terminal record.
/// A worker that exits with status 0 before it ever registered itself, having
/// recorded no exit at all, never became ready -- so it must still fail closed
/// and roll its state back.
#[test]
#[cfg(feature = "startup-test-hooks")]
fn clean_worker_exit_without_terminal_record_still_fails_startup() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let output = harness.run_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "silent-clean-exit",
            "--startup-timeout-ms",
            "10000",
            "--",
            "/bin/sleep",
            "30",
        ],
        &[
            ("APLEXER_TEST_EXIT_WORKER_AT", "after_worker_lock:0"),
            ("APLEXER_TEST_AWAIT_WORKER_EXIT_BEFORE_READINESS_POLL", "1"),
        ],
    );
    assert!(
        !output.status.success(),
        "recordless clean worker exit was accepted as a started session"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("worker exited during startup"),
        "unexpected stderr: {stderr}"
    );
    harness.assert_no_session_artifacts();
}

/// A worker that dies with a non-zero status is a startup failure whatever
/// its durable record says; the clean-exit acceptance must not widen into
/// "any worker exit is fine".
#[test]
#[cfg(feature = "startup-test-hooks")]
fn nonzero_worker_exit_still_fails_startup() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let output = harness.run_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "nonzero-exit",
            "--startup-timeout-ms",
            "10000",
            "--",
            "/bin/sleep",
            "30",
        ],
        &[
            ("APLEXER_TEST_EXIT_WORKER_AT", "after_worker_lock:9"),
            ("APLEXER_TEST_AWAIT_WORKER_EXIT_BEFORE_READINESS_POLL", "1"),
        ],
    );
    assert!(
        !output.status.success(),
        "non-zero worker exit was accepted as a started session"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("worker exited during startup") && stderr.contains("exit status: 9"),
        "unexpected stderr: {stderr}"
    );
    harness.assert_no_session_artifacts();
}

/// A worker that recorded `Phase::Failed` and then exited must still be
/// reported as a startup failure with its own recorded reason, not laundered
/// into a completed session by the exited-worker path.
#[test]
#[cfg(feature = "startup-test-hooks")]
fn failed_record_from_exited_worker_still_fails_startup() {
    let harness = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let workspace = workspace.path().to_str().expect("UTF-8 workspace");
    let output = harness.run_with_env(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            "failed-record",
            "--startup-timeout-ms",
            "10000",
            "--",
            "/bin/sh",
            "-c",
            "printf 'failed-record\\n'",
        ],
        &[
            (
                "APLEXER_TEST_FAIL_WORKER_STARTUP_AT",
                "after_running_record",
            ),
            ("APLEXER_TEST_AWAIT_WORKER_EXIT_BEFORE_READINESS_POLL", "1"),
        ],
    );
    assert!(
        !output.status.success(),
        "worker startup failure was accepted as a started session"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("worker startup failed")
            && stderr.contains("injected worker startup failure at after_running_record"),
        "unexpected stderr: {stderr}"
    );
    harness.assert_no_session_artifacts();
}

/// Guards the anchor in `externally_reaped_worker_preserves_ambiguous_containment_evidence`
/// against sliding back onto a string it shares with its neighbours.
///
/// Both samples are verbatim launcher stderr, captured from real runs of the
/// same fixture: the first from the external-reap path (the test SIGKILLs the
/// worker while the launcher is polling for readiness), the second from the
/// startup-timeout path with the SIGKILL landing inside the SIGTERM grace --
/// the interleaving a contended runner produces, and the one the old
/// `contains("could not be confirmed")` assertion accepted as a pass.
///
/// Deliberately ungated: the anchor is pure text, so this runs in the default
/// `cargo test` lane as well as the fault-injection one.
#[test]
fn the_external_reap_anchor_rejects_the_startup_timeout_path() {
    let external_reap = "a: startup failed: worker exited during startup: signal: 9 (SIGKILL); \
         rollback also failed: worker 2898677 exited before independent containment cleanup \
         and left no conclusive cleanup record; startup containment for \
         918dac95-6ae4-4fe9-8c4f-5c90c04da34c could not be confirmed; preserved runtime and \
         durable state\n";
    let startup_timeout = "a: startup failed: worker did not become ready within 1000 ms; \
         rollback also failed: worker 2906416 exited before independent containment cleanup \
         and left no conclusive cleanup record; startup containment for \
         c32a2859-082d-44b8-b5d8-ca56202de138 could not be confirmed; preserved runtime and \
         durable state\n";

    // The string the test used to assert on cannot tell the two apart at all.
    assert!(external_reap.contains("could not be confirmed"));
    assert!(startup_timeout.contains("could not be confirmed"));

    assert!(
        observed_external_worker_reap(external_reap),
        "the anchor must accept the path it is meant to identify"
    );
    assert!(
        !observed_external_worker_reap(startup_timeout),
        "the anchor must reject the startup-timeout path it used to be satisfied by"
    );
}
