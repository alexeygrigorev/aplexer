// Live integration test: start real agent CLIs in skip-permissions mode,
// resolve each session with `a whoami` (the same APLEXER_SESSION_ID a
// process inside the session sees), give them a task that needs tools,
// and `a transcript --follow` until tool events and a final answer appear.
//
// This talks to grok / claude (zlaude if present) / codex over the network
// and takes a minute or two, so it is #[ignore] and is not part of
// `cargo test` / `scripts/validate.sh`. Run it occasionally:
//
//   cargo test --test transcript_live -- --ignored --nocapture
//
// Requires the engine binaries on PATH (or in ~/.local/bin / nvm's bin)
// and a working login for each engine. Aplexer state is isolated; HOME is
// not, because the engines keep credentials and native logs there.

use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TASK: &str = "There are three .txt files in this directory. Count the words in each file, \
write a Markdown table of filename vs word count to report.md, and reply with only the total \
word count across all three files. Do not modify the .txt files.";

const FOLLOW_MARK: &str = "LIVEFOLLOW";
const FOLLOW_PROMPT: &str = "Reply with exactly the word LIVEFOLLOW and nothing else.";

struct Harness {
    runtime_dir: TempDir,
    state_dir: TempDir,
    workspace: TempDir,
    config_file: PathBuf,
    session_ids: Vec<String>,
}

impl Harness {
    fn new() -> Self {
        let runtime_dir = TempDir::new().expect("runtime tempdir");
        let state_dir = TempDir::new().expect("state tempdir");
        let workspace = TempDir::new().expect("workspace tempdir");
        let config_file = runtime_dir.path().join("config.toml");
        Self {
            runtime_dir,
            state_dir,
            workspace,
            config_file,
            session_ids: Vec::new(),
        }
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_a"));
        cmd.env("APLEXER_RUNTIME_DIR", self.runtime_dir.path());
        cmd.env("APLEXER_STATE_DIR", self.state_dir.path());
        cmd.env("APLEXER_CONFIG", &self.config_file);
        cmd.env_remove("APLEXER_SESSION_ID");
        enrich_path(&mut cmd);
        cmd
    }

    /// `a` as if it were running inside the session: `APLEXER_SESSION_ID` is
    /// what `a whoami` and bare `a transcript` key off.
    fn command_inside(&self, session_id: &str) -> Command {
        let mut cmd = self.command();
        cmd.env("APLEXER_SESSION_ID", session_id);
        cmd
    }

    fn whoami(&self, session_id: &str) -> Value {
        let mut cmd = self.command_inside(session_id);
        cmd.args(["--json", "whoami"]);
        let output = run_with_timeout(cmd, Duration::from_secs(5));
        assert!(
            output.status.success(),
            "a whoami failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("whoami json")
    }

    fn run(&self, args: &[&str], timeout: Duration) -> std::process::Output {
        let mut cmd = self.command();
        cmd.args(args);
        run_with_timeout(cmd, timeout)
    }

    fn run_ok(&self, args: &[&str], timeout: Duration) -> String {
        let output = self.run(args, timeout);
        assert!(
            output.status.success(),
            "`a {}` failed (status {:?}):\nstdout: {}\nstderr: {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn workspace(&self) -> &str {
        self.workspace.path().to_str().expect("utf8 workspace")
    }

    fn send_enter(&self, session_id: &str, text: &str) {
        self.run_ok(
            &["send", session_id, "--enter", text],
            Duration::from_secs(5),
        );
        // Codex/Claude TUIs submit on CR, not LF.
        self.run_ok(&["send", session_id, "--hex", "0d"], Duration::from_secs(5));
    }

    fn confirm_trust(&self, session_id: &str) {
        let _ = self.run(&["send", session_id, "--hex", "0d"], Duration::from_secs(5));
    }

    fn transcript_events(&self, session_id: &str) -> Vec<Value> {
        let mut cmd = self.command_inside(session_id);
        cmd.args(["--json", "transcript"]);
        let output = run_with_timeout(cmd, Duration::from_secs(15));
        if !output.status.success() {
            return Vec::new();
        }
        parse_jsonl(&String::from_utf8_lossy(&output.stdout))
    }

    fn dump_agent(&self, session_id: &str) {
        let events = self.transcript_events(session_id);
        eprintln!(
            "dump {session_id}: {} events kinds={:?}",
            events.len(),
            events
                .iter()
                .filter_map(|e| e["kind"].as_str())
                .collect::<Vec<_>>()
        );
        let cap = self.run(
            &["capture", session_id, "--bytes", "2500"],
            Duration::from_secs(5),
        );
        let mut screen = String::from_utf8_lossy(&cap.stdout).into_owned();
        screen.retain(|c| c == '\n' || c == ' ' || (' '..='~').contains(&c));
        let tail = screen.chars().rev().take(600).collect::<String>();
        let tail: String = tail.chars().rev().collect();
        eprintln!("capture {session_id} tail: {tail}");
    }

    fn spawn_follow(&self, session_id: &str, after: u64) -> Follow {
        let mut cmd = self.command_inside(session_id);
        cmd.args([
            "--json",
            "transcript",
            "--after",
            &after.to_string(),
            "--follow",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn a transcript --follow");
        let stdout = child.stdout.take().expect("follow stdout");
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
        for id in &self.session_ids {
            let _ = self.run(&["kill", id], Duration::from_secs(8));
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

struct Agent {
    tag: &'static str,
    engine: &'static str,
    profile: Option<&'static str>,
}

fn enrich_path(cmd: &mut Command) {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/bin"));
        let nvm = home.join(".nvm/versions/node");
        if let Ok(entries) = fs::read_dir(&nvm) {
            for entry in entries.flatten() {
                dirs.push(entry.path().join("bin"));
            }
        }
    }
    let mut path = std::env::var("PATH").unwrap_or_default();
    for dir in dirs.into_iter().rev() {
        if dir.is_dir() {
            path = format!("{}:{path}", dir.display());
        }
    }
    cmd.env("PATH", path);
}

fn command_on_path(name: &str) -> bool {
    let mut cmd = Command::new("sh");
    cmd.args(["-c", &format!("command -v {name} >/dev/null 2>&1")]);
    enrich_path(&mut cmd);
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

fn zlaude_available() -> bool {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".zlaude").is_dir())
        .unwrap_or(false)
}

fn agents_to_run() -> Vec<Agent> {
    let mut out = Vec::new();
    if command_on_path("grok") {
        out.push(Agent {
            tag: "live",
            engine: "grok",
            profile: None,
        });
    }
    if command_on_path("claude") && zlaude_available() {
        // Default Anthropic Claude is often out of usage; this test uses the
        // Z.AI-routed zlaude profile (`CLAUDE_CONFIG_DIR=~/.zlaude`).
        out.push(Agent {
            tag: "zsp",
            engine: "claude",
            profile: Some("zlaude"),
        });
    }
    if command_on_path("codex") {
        out.push(Agent {
            tag: "yolo",
            engine: "codex",
            profile: None,
        });
    }
    out
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::process::Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = cmd.spawn().expect("spawn a");
    let pid = child.id();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("wait failed: {error}"),
        Err(_) => {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
            panic!("command (pid {pid}) did not finish within {timeout:?}");
        }
    }
}

fn parse_jsonl(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|l| l.starts_with('{'))
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn event_text(event: &Value) -> String {
    [
        event["content"].as_str().unwrap_or(""),
        event["tool_name"].as_str().unwrap_or(""),
        event["tool_input"].as_str().unwrap_or(""),
        event["tool_output"].as_str().unwrap_or(""),
    ]
    .join(" ")
}

fn last_sequence(events: &[Value]) -> u64 {
    events
        .iter()
        .filter_map(|e| e["sequence"].as_u64())
        .max()
        .unwrap_or(0)
}

fn has_task_progress(events: &[Value]) -> bool {
    let kinds: Vec<&str> = events.iter().filter_map(|e| e["kind"].as_str()).collect();
    let saw_user = events.iter().any(|e| {
        e["kind"] == "message" && e["role"] == "user" && event_text(e).contains("report.md")
    });
    let saw_tool = kinds
        .iter()
        .any(|k| *k == "tool_call" || *k == "tool_result");
    let saw_answer = events.iter().any(|e| {
        e["kind"] == "message" && e["role"] == "assistant" && event_text(e).contains("12")
    });
    saw_user && saw_tool && saw_answer
}

fn seed_workspace(dir: &Path) {
    fs::write(dir.join("alpha.txt"), "alpha one\nalpha two\n").unwrap();
    fs::write(dir.join("beta.txt"), "beta just one line\n").unwrap();
    fs::write(dir.join("notes.txt"), "notes: apples bananas cherries\n").unwrap();
}

/// Occasionally-run live follow across grok, claude, and codex.
#[test]
#[ignore]
fn transcript_follow_live_agents() {
    let agents = agents_to_run();
    assert!(
        !agents.is_empty(),
        "need at least one of grok, claude, codex on PATH to run this ignored test"
    );

    let mut h = Harness::new();
    seed_workspace(h.workspace.path());
    let ws = h.workspace().to_string();
    let mut ids: BTreeMap<&str, String> = BTreeMap::new();

    for agent in &agents {
        let mut args = vec![
            "start",
            "--json",
            "--workspace",
            ws.as_str(),
            "--cwd",
            ws.as_str(),
            "--tag",
            agent.tag,
            "--engine",
            agent.engine,
            "--startup-timeout-ms",
            "30000",
        ];
        if let Some(profile) = agent.profile {
            args.extend(["--profile", profile]);
        }
        let stdout = h.run_ok(&args, Duration::from_secs(40));
        let record: Value = serde_json::from_str(&stdout).expect("start json");
        let id = record["id"].as_str().expect("start json id").to_string();
        let me = h.whoami(&id);
        assert_eq!(me["id"].as_str(), Some(id.as_str()), "whoami id");
        assert_eq!(me["engine"].as_str(), Some(agent.engine), "whoami engine");
        assert_eq!(me["tag"].as_str(), Some(agent.tag), "whoami tag");
        if let Some(profile) = agent.profile {
            assert_eq!(me["profile"].as_str(), Some(profile), "whoami profile");
        }
        let command: Vec<&str> = record["command"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(
            command
                .iter()
                .any(|a| a.contains("skip") || a.contains("bypass") || *a == "--always-approve"),
            "{} should start in skip-permissions mode, got {command:?}",
            agent.engine
        );
        eprintln!(
            "whoami {} -> {} tag={} engine={}",
            agent.tag, id, me["tag"], me["engine"]
        );
        h.session_ids.push(id.clone());
        ids.insert(agent.tag, id);
    }

    thread::sleep(Duration::from_secs(6));
    for _ in 0..3 {
        for agent in &agents {
            h.confirm_trust(&ids[agent.tag]);
        }
        thread::sleep(Duration::from_secs(3));
    }

    for agent in &agents {
        h.send_enter(&ids[agent.tag], TASK);
        eprintln!("sent task to {}", agent.tag);
    }

    let mut ready: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    let mut resent = false;
    let deadline = Instant::now() + Duration::from_secs(150);
    let started_wait = Instant::now();
    while Instant::now() < deadline && ready.len() < agents.len() {
        for agent in &agents {
            if ready.contains_key(agent.tag) {
                continue;
            }
            let events = h.transcript_events(&ids[agent.tag]);
            if events.is_empty() {
                continue;
            }
            if has_task_progress(&events) {
                eprintln!(
                    "{} reached tool+answer ({} events)",
                    agent.tag,
                    events.len()
                );
                ready.insert(agent.tag, events);
            } else {
                eprintln!(
                    "{} waiting, {} events kinds={:?}",
                    agent.tag,
                    events.len(),
                    events
                        .iter()
                        .filter_map(|e| e["kind"].as_str())
                        .collect::<Vec<_>>()
                );
            }
        }
        if !resent && started_wait.elapsed() > Duration::from_secs(40) {
            for agent in &agents {
                if !ready.contains_key(agent.tag) {
                    eprintln!("resending task to {}", agent.tag);
                    h.send_enter(&ids[agent.tag], TASK);
                }
            }
            resent = true;
        }
        if ready.len() < agents.len() {
            thread::sleep(Duration::from_millis(750));
        }
    }
    if ready.len() != agents.len() {
        for agent in &agents {
            if !ready.contains_key(agent.tag) {
                h.dump_agent(&ids[agent.tag]);
            }
        }
        panic!(
            "not every agent finished the word-count task in time; got {}",
            ready.keys().cloned().collect::<Vec<_>>().join(",")
        );
    }

    let report = h.workspace.path().join("report.md");
    assert!(
        report.is_file(),
        "expected an agent to write {}",
        report.display()
    );

    let mut follows: BTreeMap<&str, Follow> = BTreeMap::new();
    for agent in &agents {
        let after = last_sequence(ready.get(agent.tag).unwrap());
        follows.insert(agent.tag, h.spawn_follow(&ids[agent.tag], after));
    }
    thread::sleep(Duration::from_millis(500));
    for agent in &agents {
        h.send_enter(&ids[agent.tag], FOLLOW_PROMPT);
    }

    let follow_deadline = Instant::now() + Duration::from_secs(45);
    let mut followed: BTreeMap<&str, bool> = BTreeMap::new();
    while Instant::now() < follow_deadline && followed.len() < agents.len() {
        for agent in &agents {
            if followed.contains_key(agent.tag) {
                continue;
            }
            let events = follows.get(agent.tag).unwrap().snapshot();
            let saw = events.iter().any(|e| {
                e["kind"] == "message"
                    && e["role"] == "assistant"
                    && event_text(e).contains(FOLLOW_MARK)
            });
            if saw {
                eprintln!(
                    "{} follow saw {FOLLOW_MARK} ({} new events)",
                    agent.tag,
                    events.len()
                );
                followed.insert(agent.tag, true);
            }
        }
        if followed.len() < agents.len() {
            thread::sleep(Duration::from_millis(500));
        }
    }
    assert_eq!(
        followed.len(),
        agents.len(),
        "--follow missed {FOLLOW_MARK} for some agents; saw {}",
        followed.keys().cloned().collect::<Vec<_>>().join(",")
    );
}
