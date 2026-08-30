use aplexer::SessionRecord;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
    sessions: Vec<String>,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let config = runtime.path().join("config.toml");
        std::fs::write(
            &config,
            "version = 1\n\
             [engines.shell]\n\
             command = [\"/bin/sh\", \"-l\"]\n\
             env_unset = [\"SHELL_REMOVE\"]\n\
             [engines.agentish]\n\
             command = [\"/bin/true\"]\n",
        )
        .unwrap();
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
            .env("APLEXER_CONFIG", &self.config);
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().expect("run aplexer CLI")
    }

    fn start(
        &mut self,
        workspace: &Path,
        tag: &str,
        engine: &str,
        environment: &[&str],
        script: &str,
    ) -> SessionRecord {
        let mut command = self.command();
        command
            .args(["--json", "start", "--workspace"])
            .arg(workspace)
            .args(["--tag", tag, "--engine", engine]);
        for value in environment {
            command.args(["--env", value]);
        }
        command.args(["--", "/bin/sh", "-c", script]);
        let output = command.output().expect("start session");
        assert!(
            output.status.success(),
            "start failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record: SessionRecord = serde_json::from_slice(&output.stdout).unwrap();
        self.sessions.push(record.id.to_string());
        record
    }

    fn capture_until(&self, id: &str, marker: &str) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let output = self.run(&["capture", id]);
            if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(marker) {
                return output.stdout;
            }
            assert!(
                Instant::now() < deadline,
                "marker {marker:?} never appeared"
            );
            thread::sleep(Duration::from_millis(50));
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

#[test]
fn shell_keeps_provider_overrides_while_agent_engines_strip_them() {
    let mut harness = Harness::new();
    let workspace = TempDir::new().unwrap();

    let engines = harness.run(&["--json", "engines"]);
    assert!(engines.status.success(), "{engines:?}");
    let engines: Vec<Value> = serde_json::from_slice(&engines.stdout).unwrap();
    let shell = engines
        .iter()
        .find(|engine| engine["name"] == "shell")
        .unwrap();
    assert_eq!(shell["env_unset"], serde_json::json!(["SHELL_REMOVE"]));
    let agent = engines
        .iter()
        .find(|engine| engine["name"] == "agentish")
        .unwrap();
    assert!(agent["env_unset"]
        .as_array()
        .unwrap()
        .iter()
        .any(|name| name == "OPENAI_API_KEY"));

    let shell_spec = harness.run(&["--json", "launch-spec", "--engine", "shell"]);
    assert!(shell_spec.status.success(), "{shell_spec:?}");
    let shell_spec: Value = serde_json::from_slice(&shell_spec.stdout).unwrap();
    assert_eq!(shell_spec["env_unset"], serde_json::json!(["SHELL_REMOVE"]));
    let agent_spec = harness.run(&["--json", "launch-spec", "--engine", "agentish"]);
    assert!(agent_spec.status.success(), "{agent_spec:?}");
    let agent_spec: Value = serde_json::from_slice(&agent_spec.stdout).unwrap();
    assert!(agent_spec["env_unset"]
        .as_array()
        .unwrap()
        .iter()
        .any(|name| name == "OPENAI_API_KEY"));

    let shell = harness.start(
        workspace.path(),
        "shell-env",
        "shell",
        &["OPENAI_API_KEY=visible", "SHELL_REMOVE=hidden"],
        "printf 'shell-api=%s shell-remove=%s\\n' \"${OPENAI_API_KEY-unset}\" \"${SHELL_REMOVE-unset}\"; sleep 30",
    );
    let output = harness.capture_until(&shell.id.to_string(), "shell-api=");
    let output = String::from_utf8_lossy(&output);
    assert!(
        output.contains("shell-api=visible shell-remove=unset"),
        "{output}"
    );

    let agent = harness.start(
        workspace.path(),
        "agent-env",
        "agentish",
        &["OPENAI_API_KEY=hidden"],
        "printf 'agent-api=%s\\n' \"${OPENAI_API_KEY-unset}\"; sleep 30",
    );
    let output = harness.capture_until(&agent.id.to_string(), "agent-api=");
    let output = String::from_utf8_lossy(&output);
    assert!(output.contains("agent-api=unset"), "{output}");
}
