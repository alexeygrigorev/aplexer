use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

struct Harness {
    runtime: TempDir,
    state: TempDir,
    config: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let runtime = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let config = runtime.path().join("config.toml");
        Self {
            runtime,
            state,
            config,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_a"));
        command.env("APLEXER_RUNTIME_DIR", self.runtime.path());
        command.env("APLEXER_STATE_DIR", self.state.path());
        command.env("APLEXER_CONFIG", &self.config);
        command
    }

    fn output(&self, args: &[&str]) -> std::process::Output {
        let mut command = self.command();
        command.args(args);
        command.output().unwrap()
    }

    fn start(&self, workspace: &Path, tag: &str) -> String {
        let output = self.output(&[
            "start",
            "--workspace",
            workspace.to_str().unwrap(),
            "--tag",
            tag,
            "--json",
            "--",
            "bash",
            "--norc",
        ]);
        assert!(output.status.success(), "start failed: {output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        value["id"].as_str().unwrap().to_owned()
    }
}

fn termios(fd: i32) -> libc::termios {
    let mut value = std::mem::MaybeUninit::uninit();
    assert_eq!(unsafe { libc::tcgetattr(fd, value.as_mut_ptr()) }, 0);
    unsafe { value.assume_init() }
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("attach did not exit after its termination condition");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn read_until(master: &mut File, output: &mut Vec<u8>, needle: &[u8]) {
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(
        unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buffer = [0u8; 4096];
    while !output.windows(needle.len()).any(|window| window == needle) {
        match master.read(&mut buffer) {
            Ok(0) => {}
            Ok(count) => output.extend_from_slice(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("read attach PTY: {error}"),
        }
        assert!(Instant::now() < deadline, "missing PTY output {needle:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn termination_signals_restore_real_pty_termios_and_terminal_ui() {
    use std::os::unix::process::ExitStatusExt;

    for (signal, tag) in [
        (libc::SIGTERM, "term"),
        (libc::SIGHUP, "hup"),
        (libc::SIGQUIT, "quit"),
    ] {
        let harness = Harness::new();
        let workspace = TempDir::new().unwrap();
        let id = harness.start(workspace.path(), tag);
        let (mut master, slave) = aplexer::open_pty(24, 80).unwrap();
        let original = termios(slave.as_raw_fd());

        let mut command = harness.command();
        command.args(["attach", &id]);
        command.stdin(Stdio::from(slave.try_clone().unwrap()));
        command.stdout(Stdio::from(slave.try_clone().unwrap()));
        command.stderr(Stdio::from(slave.try_clone().unwrap()));
        let mut child = command.spawn().unwrap();

        let mut bytes = Vec::new();
        read_until(&mut master, &mut bytes, b"[aplexer attached");
        let raw = termios(slave.as_raw_fd());
        assert_eq!(raw.c_lflag & (libc::ICANON | libc::ECHO), 0);

        assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
        let status = wait_for_exit(&mut child);
        assert_eq!(status.signal(), Some(signal));

        let restored = termios(slave.as_raw_fd());
        assert_eq!(
            restored.c_lflag & (libc::ICANON | libc::ECHO),
            original.c_lflag & (libc::ICANON | libc::ECHO)
        );
        read_until(
            &mut master,
            &mut bytes,
            b"\x1b[?1049l\x1b>\x1b[?1l\x1b[?2004l\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[r\x1b[0m\x1b[2J\x1b[H\x1b[?25h",
        );

        let _ = harness.output(&["kill", &id, "--signal", "KILL", "--grace-ms", "0"]);
    }
}

#[test]
fn stdin_eof_detaches_without_ending_the_session() {
    let harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let id = harness.start(workspace.path(), "stdin-eof");

    let mut command = harness.command();
    command.args(["attach", &id]);
    command.stdin(Stdio::piped());
    command.stdout(Stdio::null());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();

    drop(child.stdin.take());
    let status = wait_for_exit(&mut child);
    assert!(status.success(), "attach failed after stdin EOF: {status}");

    let output = harness.output(&["status", &id, "--json"]);
    assert!(output.status.success(), "status failed: {output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["phase"], "running");
    assert_eq!(value["worker_alive"], true);

    let _ = harness.output(&["kill", &id, "--signal", "KILL", "--grace-ms", "0"]);
}

#[test]
fn redirected_stdin_eof_resets_modes_written_to_tty_stdout() {
    let harness = Harness::new();
    let workspace = TempDir::new().unwrap();
    let id = harness.start(workspace.path(), "mixed-fd-eof");
    let sent = harness.output(&[
        "send",
        &id,
        r#"printf '\033[?1049h\033[?1000h\033[?1006hMIXED-FD-MARK'"#,
        "--enter",
    ]);
    assert!(sent.status.success(), "send failed: {sent:?}");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = harness.output(&["capture", &id, "--screen"]);
        if snapshot.status.success()
            && snapshot.stdout.starts_with(b"\x1b[?1049h")
            && snapshot
                .stdout
                .windows(b"MIXED-FD-MARK".len())
                .any(|window| window == b"MIXED-FD-MARK")
        {
            break;
        }
        assert!(Instant::now() < deadline, "mode snapshot was not ready");
        thread::sleep(Duration::from_millis(10));
    }

    let (mut master, slave) = aplexer::open_pty(24, 80).unwrap();
    let mut command = harness.command();
    command.args(["attach", &id]);
    command.stdin(Stdio::null());
    command.stdout(Stdio::from(slave.try_clone().unwrap()));
    command.stderr(Stdio::from(slave.try_clone().unwrap()));
    let mut child = command.spawn().unwrap();

    let status = wait_for_exit(&mut child);
    assert!(status.success(), "attach failed after stdin EOF: {status}");
    let mut bytes = Vec::new();
    read_until(
        &mut master,
        &mut bytes,
        b"\x1b[?1049l\x1b>\x1b[?1l\x1b[?2004l\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[r\x1b[0m\x1b[2J\x1b[H\x1b[?25h",
    );
    assert!(
        bytes
            .windows(b"\x1b[?1049h".len())
            .any(|window| window == b"\x1b[?1049h"),
        "attach never wrote the alternate-screen snapshot"
    );
    assert!(
        bytes
            .windows(b"\x1b[?1000h".len())
            .any(|window| window == b"\x1b[?1000h")
            && bytes
                .windows(b"\x1b[?1006h".len())
                .any(|window| window == b"\x1b[?1006h"),
        "attach never wrote the snapshot's mouse modes"
    );

    let output = harness.output(&["status", &id, "--json"]);
    assert!(output.status.success(), "status failed: {output:?}");
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["phase"], "running");
    assert_eq!(value["worker_alive"], true);

    let _ = harness.output(&["kill", &id, "--signal", "KILL", "--grace-ms", "0"]);
}
