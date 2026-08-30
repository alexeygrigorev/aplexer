//! The embeddable API must never replace its host's SIGCHLD handler. Run each
//! disposition in an isolated copy of this test process because sigaction is
//! process-wide.

use aplexer::api::{start_session, StartRequest};
use aplexer::{read_record, Cgroup, Limits, Paths, Phase};
use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;
use uuid::Uuid;

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

fn successful_start_request(workspace: PathBuf, tag: &str) -> StartRequest {
    StartRequest {
        workspace,
        tag: tag.into(),
        engine: None,
        profile: None,
        cwd: None,
        env: BTreeMap::new(),
        command: vec!["/bin/sh".into(), "-c".into(), "sleep 1".into()],
        memory: None,
        pids: None,
        cpu_quota_us: None,
        cpu_period_us: 100_000,
        history_bytes: None,
        no_skip_permissions: false,
        startup_timeout_ms: 3_000,
        worker_rows: None,
        worker_cols: None,
        python: None,
    }
}

fn exercise_successful_worker_reaping() {
    install_sigchld(custom_sigchld_handler as *const () as libc::sighandler_t, 0);
    let before = sigchld_action();
    let runtime = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let paths = Paths {
        runtime_root: runtime.path().to_path_buf(),
        state_root: state.path().to_path_buf(),
        config_file: state.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    env::set_var("APLEXER_WORKER", env!("CARGO_BIN_EXE_aplexer"));
    let threads_before = std::fs::read_dir("/proc/self/task").unwrap().count();
    let mut sessions = Vec::new();
    for index in 0..8 {
        sessions.push(
            start_session(
                &paths,
                &successful_start_request(
                    workspace.path().to_path_buf(),
                    &format!("reaping-{index}"),
                ),
            )
            .expect("start short-lived worker through in-process API"),
        );
    }
    let threads_after = std::fs::read_dir("/proc/self/task").unwrap().count();
    assert!(
        threads_after <= threads_before + 1,
        "shared worker reaper created {} threads for 8 sessions",
        threads_after.saturating_sub(threads_before)
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    for ready in sessions {
        let worker_pid = ready.worker_pid.expect("ready worker pid");
        loop {
            let record = read_record(&paths.record(ready.id)).expect("read final session record");
            let worker_reaped = !PathBuf::from(format!("/proc/{worker_pid}")).exists();
            if matches!(record.phase, Phase::Exited | Phase::Failed) && worker_reaped {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "completed in-process worker {worker_pid} remained as a child/zombie; phase={:?}",
                record.phase
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    let after = sigchld_action();
    assert_eq!(after.sa_sigaction, before.sa_sigaction);
    assert_eq!(after.sa_flags, before.sa_flags);
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

    let cgroup = Cgroup::create(Uuid::new_v4(), &Limits::default(), || {});
    match mode {
        "custom" => assert!(cgroup.unwrap().is_none()),
        "ignored" => assert!(
            format!("{:#}", cgroup.expect_err("SIG_IGN must be rejected")).contains("SIG_IGN")
        ),
        "no-cld-wait" => assert!(format!(
            "{:#}",
            cgroup.expect_err("SA_NOCLDWAIT must be rejected")
        )
        .contains("SA_NOCLDWAIT")),
        _ => unreachable!(),
    }

    let after = sigchld_action();
    assert_eq!(after.sa_sigaction, before.sa_sigaction);
    assert_eq!(after.sa_flags, before.sa_flags);
}

#[test]
fn in_process_api_preserves_or_rejects_sigchld_without_mutating_it() {
    if let Ok(mode) = env::var(CHILD_MODE) {
        if mode == "reaping" {
            exercise_successful_worker_reaping();
            return;
        }
        exercise_child(&mode);
        return;
    }

    let current_test = env::current_exe().expect("current test executable");
    for mode in ["custom", "ignored", "no-cld-wait", "reaping"] {
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
