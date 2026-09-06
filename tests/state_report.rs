// Integration test for `a state-report` (docs/pocketshell-integration-plan.md
// Open question #2, "Agent-state ingestion"): a hook running inside a
// session pushes its own semantic state, and `a watch --jsonl`'s
// `agent.state` event must reflect it -- authoritative while fresh, falling
// back to the PTY-recency heuristic once the staleness window elapses with
// no PTY activity and no further push. See src/watch.rs's
// `fresh_reported_state`/`derive_agent_state_with_source` for the exact
// merge rule and its unit tests for the boundary cases; this test proves
// the real end-to-end pipeline (worker RPC -> persisted session.json ->
// `a watch`'s poll -> JSONL) rather than re-deriving that logic.
//
// Follows the harness style of tests/rename_uniqueness.rs (direct-CLI
// harness) and tests/transcript_live.rs (a spawned `--follow`-style
// subprocess read on a background thread into a shared Vec).

use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Real elapsed time a fresh push stays authoritative -- must track
/// watch.rs::REPORTED_STATE_STALE_MS exactly, since that constant is
/// private to the lib crate and this is a black-box CLI test. Deliberately
/// duplicated rather than exposed as a public constant: an integration
/// test asserting against the CLI's real behavior should not need a
/// lib-internal escape hatch, and if the two ever drift a maintainer
/// updating one is expected to grep for the other.
const REPORTED_STATE_STALE_MS: u64 = 8_000;

/// How long a poll for something that must eventually become true is allowed
/// to run before the test gives up.
///
/// Every `wait_for` below is armed only after `await_watch_ready` has proved
/// the watcher is live, so the event it waits for is guaranteed to be emitted:
/// the budget decides how long a genuinely broken run takes to report itself,
/// not whether a healthy one passes. It is therefore sized for a saturated CI
/// runner rather than for an idle laptop -- raising it cannot mask a defect,
/// because none of these loops can ever pass by timing out.
const LIVENESS_BACKSTOP: Duration = Duration::from_secs(60);

/// How long one readiness probe waits for its own `session.created` before the
/// handshake assumes that probe was itself swallowed by `a watch`'s startup
/// snapshot and tries another. Several poll intervals, so a healthy watcher
/// never needs a second probe.
const READINESS_PROBE_WINDOW: Duration = Duration::from_secs(5);

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
    sessions: Vec<String>,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().expect("runtime tempdir");
        let state = TempDir::new().expect("state tempdir");
        let config = runtime.path().join("config.toml");
        Self {
            runtime,
            state,
            config,
            sessions: Vec::new(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            .env_remove("APLEXER_SESSION_ID");
        command
    }

    /// `a` as if it were running inside `session_id` -- what a hook script
    /// sees, and exactly what `a state-report`/`a whoami` key off.
    fn command_inside(&self, session_id: &str) -> Command {
        let mut command = self.command();
        command.env("APLEXER_SESSION_ID", session_id);
        command
    }

    fn run(&self, args: &[&str], timeout: Duration) -> Output {
        let mut command = self.command();
        command.args(args);
        run_with_timeout(command, timeout)
    }

    fn run_ok(&self, args: &[&str], timeout: Duration) -> Output {
        let output = self.run(args, timeout);
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

    fn start(&mut self, workspace: &Path, tag: &str) -> Value {
        let output = self.run_ok(
            &[
                "--json",
                "start",
                "--workspace",
                workspace.to_str().expect("UTF-8 workspace"),
                "--tag",
                tag,
                "--",
                "/bin/sleep",
                "300",
            ],
            Duration::from_secs(10),
        );
        let record: Value = serde_json::from_slice(&output.stdout).expect("start record JSON");
        self.sessions
            .push(record["id"].as_str().expect("id").to_string());
        record
    }

    /// Push `state` from inside `session_id`, exactly as a hook script
    /// would (`a state-report waiting` with `APLEXER_SESSION_ID` in its
    /// environment, no selector).
    fn state_report(&self, session_id: &str, state: &str) {
        let mut command = self.command_inside(session_id);
        command.args(["state-report", state]);
        let output = run_with_timeout(command, Duration::from_secs(5));
        assert!(
            output.status.success(),
            "a state-report {state} failed (status {:?}): stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn spawn_watch_all(&self) -> Follow {
        let mut command = self.command();
        command.args(["watch", "--jsonl", "--all"]);
        Self::follow(command)
    }

    /// `a watch --jsonl --all`, but held at the starting line until `gate`
    /// exists. Lets a test place a session's creation provably before the
    /// watcher's startup snapshot instead of hoping a sleep is long enough --
    /// the deterministic form of the scheduling delay that made this suite
    /// miss `session.created` on a loaded box.
    fn spawn_watch_all_gated(&self, gate: &Path) -> Follow {
        let mut command = Command::new("/bin/sh");
        command
            .env("APLEXER_RUNTIME_DIR", self.runtime.path())
            .env("APLEXER_STATE_DIR", self.state.path())
            .env("APLEXER_CONFIG", &self.config)
            .env_remove("APLEXER_SESSION_ID")
            .arg("-c")
            .arg(
                "gate=\"$1\"; shift; \
                 while [ ! -e \"$gate\" ]; do sleep 0.02; done; \
                 exec \"$@\"",
            )
            .arg("sh")
            .arg(gate)
            .arg(env!("CARGO_BIN_EXE_a"))
            .args(["watch", "--jsonl", "--all"]);
        Self::follow(command)
    }

    fn follow(mut command: Command) -> Follow {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn a watch --all");
        let stdout = child.stdout.take().expect("watch stdout");
        let stderr = child.stderr.take().expect("watch stderr");
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                if let Ok(value) = serde_json::from_str::<Value>(&line) {
                    sink.lock().unwrap().push(value);
                }
            }
        });
        // Drained into the failure message rather than discarded: a watcher
        // that dies on startup otherwise looks exactly like a watcher that saw
        // nothing, which is the difference between a broken fixture and a
        // broken product.
        let diagnostics = Arc::new(Mutex::new(Vec::new()));
        let diagnostics_sink = diagnostics.clone();
        thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                diagnostics_sink.lock().unwrap().push(line);
            }
        });
        Follow {
            child,
            events,
            diagnostics,
        }
    }

    /// Block until `a watch` has provably finished its startup snapshot and is
    /// polling for changes.
    ///
    /// `watch::run` records every session that already exists before entering
    /// its loop and never replays history, so a session created before that
    /// snapshot is invisible to the watcher for the rest of its life. Spawning
    /// the watcher and immediately starting a session is therefore a race, and
    /// losing it does not merely delay `session.created`, it removes the event
    /// entirely -- which is why the failure looked like "timed out waiting for
    /// session.created; saw 0 events: []" (issue #2) and why raising the
    /// deadline could never have fixed it.
    ///
    /// The watcher publishes no readiness signal, so probe for one: start a
    /// throwaway session and wait a few poll intervals for its own
    /// `session.created`. A probe that was itself caught by the snapshot
    /// produces nothing, so try another; the snapshot is taken exactly once, so
    /// a later probe is guaranteed to land after it. Once any probe is seen,
    /// every session created afterwards is guaranteed to be reported, which is
    /// what turns the deadlines further down into pure liveness backstops.
    fn await_watch_ready(&mut self, watch: &Follow, workspace: &Path) {
        let deadline = Instant::now() + LIVENESS_BACKSTOP;
        let mut probes = 0;
        loop {
            probes += 1;
            let record = self.start(workspace, &format!("watch-readiness-probe-{probes}"));
            let id = record["id"].as_str().expect("probe session id").to_string();
            let probe_deadline = Instant::now() + READINESS_PROBE_WINDOW;
            loop {
                if watch.snapshot().iter().any(|event| is_created(event, &id)) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "`a watch` never reported a session as created after {probes} probes; \
                     saw {:#?}; watcher stderr: {}",
                    watch.snapshot(),
                    watch.stderr()
                );
                if Instant::now() >= probe_deadline {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        for id in &self.sessions {
            let _ = self
                .command()
                .args(["kill", id, "--signal", "KILL", "--grace-ms", "0"])
                .output();
        }
    }
}

struct Follow {
    child: Child,
    events: Arc<Mutex<Vec<Value>>>,
    diagnostics: Arc<Mutex<Vec<String>>>,
}

impl Follow {
    fn snapshot(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }

    fn stderr(&self) -> String {
        self.diagnostics.lock().unwrap().join("\n")
    }
}

impl Drop for Follow {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_with_timeout(mut command: Command, timeout: Duration) -> Output {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().expect("spawn command");
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("wait for command: {error}"),
        Err(_) => {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            panic!("command pid {pid} exceeded {timeout:?}");
        }
    }
}

/// Polls `follow.snapshot()` until `predicate` matches an event, or panics
/// with the full snapshot for debugging once `deadline` passes -- the same
/// bounded-poll shape used throughout this test tree (e.g.
/// tests/containment_recovery.rs) rather than a single blind sleep+check.
fn wait_for(
    follow: &Follow,
    deadline: Instant,
    what: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    loop {
        let events = follow.snapshot();
        if let Some(found) = events.iter().find(|e| predicate(e)) {
            return found.clone();
        }
        if Instant::now() >= deadline {
            panic!(
                "timed out waiting for {what}; saw {} events: {events:#?}; \
                 watcher stderr: {}",
                events.len(),
                follow.stderr()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn is_created(event: &Value, session_id: &str) -> bool {
    event["metadata"]["event"] == "session.created" && event["metadata"]["session_id"] == session_id
}

fn agent_state_events<'a>(events: &'a [Value], session_id: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| {
            e["metadata"]["event"] == "agent.state" && e["metadata"]["session_id"] == session_id
        })
        .collect()
}

#[test]
fn state_report_is_authoritative_then_falls_back_to_heuristic_once_stale() {
    let mut h = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");

    // Start watching BEFORE the session exists so `a watch` (which does not
    // replay history) can observe the session's own `session.created` and
    // every subsequent transition live -- and then WAIT until the watcher has
    // provably reached that point. Spawning it is not the same as it watching:
    // until its startup snapshot is done, a session created in the gap is
    // invisible to it forever, and no deadline recovers from that.
    let watch = h.spawn_watch_all();
    h.await_watch_ready(&watch, workspace.path());

    let record = h.start(workspace.path(), "state-report-it");
    let id = record["id"].as_str().expect("session id").to_string();

    // Armed only now, after the handshake, so it measures how long the
    // watcher takes to report a session it is guaranteed to see rather than
    // how responsive the machine was while it was still starting up.
    let created_deadline = Instant::now() + LIVENESS_BACKSTOP;
    wait_for(&watch, created_deadline, "session.created", |e| {
        is_created(e, &id)
    });

    // Push a value the PTY-recency heuristic could never produce by itself
    // for a silent /bin/sleep workload (its own default is "running", never
    // "idle") -- so a matching agent.state event unambiguously proves the
    // push reached `a watch`, not a heuristic coincidence.
    h.state_report(&id, "idle");

    let reported_deadline = Instant::now() + LIVENESS_BACKSTOP;
    let reported_event = wait_for(
        &watch,
        reported_deadline,
        "reported agent.state=idle",
        |e| {
            e["metadata"]["event"] == "agent.state"
                && e["metadata"]["session_id"] == id
                && e["metadata"]["state"] == "idle"
        },
    );
    assert_eq!(
        reported_event["metadata"]["state_source"], "reported",
        "a fresh push must be tagged state_source=reported: {reported_event:#?}"
    );
    let reported_sequence = reported_event["sequence"].as_u64().expect("sequence");

    // Let it go silent past the staleness window: no further state-report
    // call, and /bin/sleep never produces PTY output, so nothing refreshes
    // the push and nothing feeds the heuristic's activity signal either.
    thread::sleep(Duration::from_millis(REPORTED_STATE_STALE_MS + 1_500));

    let fallback_deadline = Instant::now() + LIVENESS_BACKSTOP;
    let fallback_event = wait_for(
        &watch,
        fallback_deadline,
        "heuristic agent.state after staleness",
        |e| {
            e["metadata"]["event"] == "agent.state"
                && e["metadata"]["session_id"] == id
                && e["metadata"]["state_source"] == "heuristic"
                && e["sequence"].as_u64().unwrap_or(0) > reported_sequence
        },
    );
    // A silent, still-`Running`-phase session with no last_activity_ms ever
    // recorded falls back to the heuristic's own "no evidence of going
    // quiet yet" default (watch.rs derive_agent_state_with_source) --
    // "running", not "waiting". The load-bearing assertion here is
    // state_source flipping back to "heuristic" (checked by wait_for's
    // predicate above); this just documents which value that heuristic
    // actually produced for this fixture.
    assert_eq!(fallback_event["metadata"]["state"], "running");

    // Sanity: exactly the sequence of agent.state events we expect for
    // this session, no unrelated states/sources sprinkled in between.
    let all = watch.snapshot();
    let states = agent_state_events(&all, &id);
    assert!(
        states.len() >= 2,
        "expected at least the reported and fallback events: {states:#?}"
    );
}

/// Control for the handshake in the test above: it proves the failure mode the
/// handshake exists to remove is real, and that the handshake itself is what
/// detects readiness rather than luck.
///
/// A watcher held at the starting line until after a session exists snapshots
/// that session as pre-existing and never emits `session.created` for it --
/// permanently, not late. On the pre-handshake test that presented as
/// "timed out waiting for session.created; saw 0 events: []" after a 10s
/// wall-clock budget, indistinguishable from a slow machine. Here the miss is
/// asserted directly, and the probe session created after the same watcher is
/// released proves the watcher was alive and reporting the whole time, so the
/// missing event cannot be blamed on a dead or slow watcher.
#[test]
fn a_session_created_before_the_watch_snapshot_is_never_reported_as_created() {
    let mut h = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    let gate = h.runtime.path().join("release-the-watcher");

    let watch = h.spawn_watch_all_gated(&gate);
    let missed = h.start(workspace.path(), "created-before-the-snapshot");
    let missed_id = missed["id"].as_str().expect("session id").to_string();

    std::fs::write(&gate, b"go").expect("release the watcher");
    h.await_watch_ready(&watch, workspace.path());

    let events = watch.snapshot();
    assert!(
        !events.iter().any(|event| is_created(event, &missed_id)),
        "a watcher that started after {missed_id} existed reported it as newly \
         created: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event["metadata"]["event"] == "session.created"),
        "the readiness probe should have produced a session.created: {events:#?}"
    );
}

/// End-to-end regression for the other half of "saw 0 events: []": `a watch`
/// used to EXIT when the registry contained a session directory whose record
/// had not been written yet, which is the state `a start` leaves on disk for a
/// moment while it creates a session.
///
/// A watcher racing a concurrent `a start` therefore died about 1 run in 10 on
/// an idle box, printing
///
/// ```text
/// a: load session registry entry <dir>: read <dir>/session.json:
/// No such file or directory (os error 2)
/// ```
///
/// to a stderr nobody was reading, and every later `wait_for` then timed out
/// against a corpse. This test pins that exact on-disk state deterministically
/// instead of racing for it, and requires the watcher to keep running and keep
/// reporting.
#[test]
fn watch_keeps_running_when_a_session_record_is_not_written_yet() {
    // A syntactically valid session id with no record behind it: byte for byte
    // what `a start` has on disk between `ensure_private_dir` and its first
    // `atomic_write_json`.
    const PENDING_SESSION: &str = "0f1e2d3c-4b5a-4968-8776-655443332211";

    let mut h = Harness::new();
    let workspace = TempDir::new().expect("workspace tempdir");
    std::fs::create_dir_all(h.state.path().join("sessions").join(PENDING_SESSION))
        .expect("create a session directory with no record in it");

    let watch = h.spawn_watch_all();
    // Fails outright if the watcher exited on the pending directory: the probe
    // handshake is exactly "prove this watcher is alive and reporting".
    h.await_watch_ready(&watch, workspace.path());

    assert!(
        !watch
            .snapshot()
            .iter()
            .any(|event| event["metadata"]["session_id"] == PENDING_SESSION),
        "a session with no record was reported as a session: {:#?}",
        watch.snapshot()
    );
}

#[test]
fn state_report_outside_a_session_fails_with_a_clear_exit_code() {
    let h = Harness::new();
    let mut command = h.command();
    command.env_remove("APLEXER_SESSION_ID");
    command.args(["state-report", "waiting"]);
    let output = run_with_timeout(command, Duration::from_secs(5));
    assert!(
        !output.status.success(),
        "state-report must fail outside a session"
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("APLEXER_SESSION_ID"),
        "stderr should explain the missing env var: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn state_report_rejects_an_unrecognised_state_value_at_the_cli_layer() {
    let h = Harness::new();
    let mut command = h.command();
    command.args(["state-report", "bogus"]);
    let output = run_with_timeout(command, Duration::from_secs(5));
    assert!(
        !output.status.success(),
        "clap's ValueEnum should reject a state outside idle/waiting/working"
    );
}
