use serde_json::Value;
use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const PROBE_ENV: &str = "APLEXER_TEST_SIGNAL_STATE_PROBE";

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
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            panic!("command {pid} exceeded {timeout:?}");
        }
    }
}

fn command(runtime: &TempDir, state: &TempDir) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
    command
        .env("APLEXER_RUNTIME_DIR", runtime.path())
        .env("APLEXER_STATE_DIR", state.path())
        .env("APLEXER_CONFIG", runtime.path().join("config.toml"));
    command
}

#[test]
fn inherited_signal_state_is_normalized_for_workload() {
    let runtime = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let probe = std::env::current_exe().unwrap();
    let mut start = command(&runtime, &state);
    start.args([
        "--json",
        "start",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "--tag",
        "signal-state",
        "--env",
        &format!("{PROBE_ENV}=1"),
        "--",
        probe.to_str().unwrap(),
        "--exact",
        "workload_signal_probe",
        "--nocapture",
    ]);
    unsafe {
        start.pre_exec(|| {
            let mut ignored: libc::sigaction = std::mem::zeroed();
            ignored.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&mut ignored.sa_mask);
            if libc::sigaction(libc::SIGUSR2, &ignored, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut blocked: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut blocked);
            libc::sigaddset(&mut blocked, libc::SIGUSR1);
            let rc = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut());
            if rc != 0 {
                return Err(io::Error::from_raw_os_error(rc));
            }
            Ok(())
        });
    }
    let started = run_with_timeout(start, Duration::from_secs(10));
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let record: Value = serde_json::from_slice(&started.stdout).unwrap();
    let id = record["id"].as_str().unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = run_with_timeout(
            {
                let mut cmd = command(&runtime, &state);
                cmd.args(["capture", id, "--bytes", "4096"]);
                cmd
            },
            Duration::from_secs(3),
        );
        let capture = String::from_utf8_lossy(&output.stdout);
        if capture.contains("signal-state-clean") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "workload did not report clean signal state; capture={capture:?}, stderr={:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn workload_signal_probe() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return;
    }
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        assert_eq!(
            libc::sigaction(libc::SIGUSR2, std::ptr::null(), &mut current),
            0
        );
        assert_eq!(current.sa_sigaction, libc::SIG_DFL);

        let mut mask: libc::sigset_t = std::mem::zeroed();
        let rc = libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut mask);
        assert_eq!(rc, 0);
        assert_eq!(libc::sigismember(&mask, libc::SIGUSR1), 0);
    }
    println!("signal-state-clean");
}
