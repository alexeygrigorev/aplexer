//! The embeddable API must never replace its host's SIGCHLD handler. Run each
//! disposition in an isolated copy of this test process because sigaction is
//! process-wide.

use aplexer::api::{start_session, StartRequest};
use aplexer::Paths;
use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

const CHILD_MODE: &str = "APLEXER_SIGCHLD_API_TEST_MODE";

extern "C" fn custom_sigchld_handler(_: libc::c_int) {}

fn sigchld_action() -> libc::sigaction {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        assert_eq!(
            libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action),
            0,
            "inspect SIGCHLD: {}",
            std::io::Error::last_os_error()
        );
        action
    }
}

fn install_sigchld(handler: libc::sighandler_t, flags: libc::c_int) {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler;
        action.sa_flags = flags;
        libc::sigemptyset(&mut action.sa_mask);
        assert_eq!(
            libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()),
            0,
            "install SIGCHLD: {}",
            std::io::Error::last_os_error()
        );
    }
}

fn invalid_start_request(workspace: PathBuf) -> StartRequest {
    StartRequest {
        workspace,
        tag: "invalid/tag".into(),
        engine: None,
        profile: None,
        cwd: None,
        env: BTreeMap::new(),
        command: vec!["/bin/true".into()],
        memory: None,
        pids: None,
        cpu_quota_us: None,
        cpu_period_us: 100_000,
        history_bytes: None,
        no_skip_permissions: false,
        startup_timeout_ms: 100,
        worker_rows: None,
        worker_cols: None,
        python: None,
    }
}

fn exercise_child(mode: &str) {
    let (handler, flags) = match mode {
        "custom" => (custom_sigchld_handler as *const () as libc::sighandler_t, 0),
        "ignored" => (libc::SIG_IGN, 0),
        "no-cld-wait" => (
            custom_sigchld_handler as *const () as libc::sighandler_t,
            libc::SA_NOCLDWAIT,
        ),
        other => panic!("unknown child mode {other}"),
    };
    install_sigchld(handler, flags);
    let before = sigchld_action();

    let runtime = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let paths = Paths {
        runtime_root: runtime.path().to_path_buf(),
        state_root: state.path().to_path_buf(),
        config_file: state.path().join("config.toml"),
    };
    let error = start_session(
        &paths,
        &invalid_start_request(workspace.path().to_path_buf()),
    )
    .expect_err("invalid request must fail");
    let message = format!("{error:#}");
    match mode {
        "custom" => assert!(message.contains("tag"), "unexpected error: {message}"),
        "ignored" => assert!(message.contains("SIG_IGN"), "unexpected error: {message}"),
        "no-cld-wait" => assert!(
            message.contains("SA_NOCLDWAIT"),
            "unexpected error: {message}"
        ),
        _ => unreachable!(),
    }

    let after = sigchld_action();
    assert_eq!(after.sa_sigaction, before.sa_sigaction);
    assert_eq!(after.sa_flags, before.sa_flags);
}

#[test]
fn in_process_api_preserves_or_rejects_sigchld_without_mutating_it() {
    if let Ok(mode) = env::var(CHILD_MODE) {
        exercise_child(&mode);
        return;
    }

    let current_test = env::current_exe().expect("current test executable");
    for mode in ["custom", "ignored", "no-cld-wait"] {
        let output = Command::new(&current_test)
            .env(CHILD_MODE, mode)
            .args([
                "--exact",
                "in_process_api_preserves_or_rejects_sigchld_without_mutating_it",
                "--nocapture",
            ])
            .output()
            .expect("spawn isolated SIGCHLD test");
        assert!(
            output.status.success(),
            "{mode} child failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
