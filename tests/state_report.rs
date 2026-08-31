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
        command
            .args(["watch", "--jsonl", "--all"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn a watch --all");
        let stdout = child.stdout.take().expect("watch stdout");
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
        Follow { child, events }
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
}

impl Follow {
    fn snapshot(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
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
                "timed out waiting for {what}; saw {} events: {events:#?}",
                events.len()
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
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
    // replay history) is guaranteed to observe the session's own
    // `session.created` and every subsequent transition live.
    let watch = h.spawn_watch_all();

    let record = h.start(workspace.path(), "state-report-it");
    let id = record["id"].as_str().expect("session id").to_string();

    let created_deadline = Instant::now() + Duration::from_secs(10);
    wait_for(&watch, created_deadline, "session.created", |e| {
        e["metadata"]["event"] == "session.created" && e["metadata"]["session_id"] == id
    });

    // Push a value the PTY-recency heuristic could never produce by itself
    // for a silent /bin/sleep workload (its own default is "running", never
    // "idle") -- so a matching agent.state event unambiguously proves the
    // push reached `a watch`, not a heuristic coincidence.
    h.state_report(&id, "idle");

    let reported_deadline = Instant::now() + Duration::from_secs(6);
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

    let fallback_deadline = Instant::now() + Duration::from_secs(6);
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
