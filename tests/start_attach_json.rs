use std::process::{Command, Stdio};
use tempfile::TempDir;

#[test]
fn global_json_rejects_start_attach_before_creating_a_session() {
    let runtime = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let config = state.path().join("config.toml");

    let output = Command::new(env!("CARGO_BIN_EXE_a"))
        .env("APLEXER_RUNTIME_DIR", runtime.path())
        .env("APLEXER_STATE_DIR", state.path())
        .env("APLEXER_CONFIG", &config)
        .args([
            "--json",
            "start",
            "--attach",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--tag",
            "must-not-start",
            "--",
            "/bin/sh",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--json cannot be combined with `start --attach`"),
        "{stderr}"
    );

    let sessions = state.path().join("sessions");
    if sessions.exists() {
        assert_eq!(std::fs::read_dir(sessions).unwrap().count(), 0);
    }
}
