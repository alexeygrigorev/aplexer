//! A stalled attached client must not be dropped by a flooding session --
//! and a wedged one must not clog the worker forever.
//!
//! Reproduction of the "attach silently disconnects on busy codex sessions"
//! class of report: the client's terminal (the pty this test plays) stops
//! draining for a while -- a background tab, a slow link, a slept laptop --
//! while the workload keeps producing. The worker's subscriber queue is
//! coalesced exactly so this stays survivable, and the streaming writer runs
//! under a stall guard (`pump_output`'s `StallGuard`): writes use a tick-long
//! SO_SNDTIMEO, and only a send queue that has not drained a single byte
//! across ATTACH_STALL_TICKS consecutive ticks (a peer gone without a FIN,
//! or a client wedged writing into a dead terminal) gets the attach closed.
//! A merely slow or paused client always drains something within the window
//! and is never dropped.
//!
//! The first test floods the session, stalls the pty reader well short of
//! the reap window, then resumes and requires the same attach to still be
//! alive and still streaming live output. The second stalls past the window
//! and requires the worker to reap the attach: the client exits with the
//! connection-loss goodbye instead of the worker carrying a blocked writer
//! thread (and its subscriber + connection slots) until the session ends.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness (isolated runtime/state dirs -- mirrors tests/attach_live_streaming.rs)
// ---------------------------------------------------------------------------

struct Harness {
    runtime_dir: TempDir,
    state_dir: TempDir,
    config_file: PathBuf,
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

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_a"));
        cmd.env("APLEXER_RUNTIME_DIR", self.runtime_dir.path());
        cmd.env("APLEXER_STATE_DIR", self.state_dir.path());
        cmd.env("APLEXER_CONFIG", &self.config_file);
        cmd
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
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::process::Output {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("failed to spawn command");
    // Drain stdout eagerly so a chatty command can never deadlock the wait.
    let mut stdout = child.stdout.take().expect("stdout piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::Read::read_to_end(&mut stdout, &mut buf);
        let _ = tx.send(child.wait().map(|status| (status, buf)));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok((status, stdout))) => std::process::Output {
            status,
            stdout,
            stderr: Vec::new(),
        },
        Ok(Err(error)) => panic!("failed to wait for command: {error}"),
        Err(_) => panic!("command did not finish within {timeout:?}"),
    }
}

fn start_flood_session(harness: &Harness, workspace: &Path, tag: &str) -> String {
    let workspace = workspace.to_str().expect("utf8 workspace path");
    // Each iteration is one MARKER line plus ~11 KB of filler: enough volume
    // to fill the socket buffer within a second of the client stalling, few
    // enough bytes per marker that the post-stall marker still reaches the
    // client through the pty at terminal-drain speed in seconds.
    let flood = "for i in $(seq -w 0 999999); do echo MARKER-$i; head -c 8192 /dev/zero | base64 -w0; echo; done";
    let stdout = harness.run_ok(
        &[
            "start",
            "--workspace",
            workspace,
            "--tag",
            tag,
            "--json",
            "--",
            "bash",
            "-c",
            flood,
        ],
        Duration::from_secs(15),
    );
    let value: Value = serde_json::from_str(&stdout).expect("`a start` output is JSON");
    value["id"]
        .as_str()
        .expect("session id in start output")
        .to_string()
}

struct SessionGuard<'a> {
    harness: &'a Harness,
    id: String,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        let _ = self.harness.run(
            &["kill", &self.id, "--signal", "KILL"],
            Duration::from_secs(5),
        );
    }
}

// ---------------------------------------------------------------------------
// A pty attach client whose terminal can be stalled mid-stream
// ---------------------------------------------------------------------------

struct StallablePtyClient {
    child: std::process::Child,
    /// Held open for the client's lifetime so the pty is not hung up while
    /// the reader thread's dup is the only other handle.
    _master: std::fs::File,
    captured: Arc<Mutex<Vec<u8>>>,
    paused: Arc<AtomicBool>,
}

impl StallablePtyClient {
    fn spawn(harness: &Harness, id: &str, rows: u16, cols: u16) -> Self {
        let (master, slave) = aplexer::open_pty(rows, cols).expect("open pty");
        let mut cmd = harness.command();
        cmd.args(["attach", id]);
        cmd.stdin(Stdio::from(slave.try_clone().expect("dup slave for stdin")));
        cmd.stdout(Stdio::from(
            slave.try_clone().expect("dup slave for stdout"),
        ));
        cmd.stderr(Stdio::from(
            slave.try_clone().expect("dup slave for stderr"),
        ));
        let child = cmd.spawn().expect("spawn `a attach` on a pty");
        drop(slave);

        let captured = Arc::new(Mutex::new(Vec::new()));
        let paused = Arc::new(AtomicBool::new(false));
        let sink = captured.clone();
        let stall_flag = paused.clone();
        let mut reader = master.try_clone().expect("dup pty master for reading");
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                // A paused reader is the frozen terminal: the pty's output
                // buffer fills, the client blocks writing stdout, and stops
                // reading its socket -- the stall under test.
                if stall_flag.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(20));
                    continue;
                }
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Err(_) => break,
                    Ok(n) => sink
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .extend_from_slice(&buf[..n]),
                }
            }
        });
        Self {
            child,
            _master: master,
            captured,
            paused,
        }
    }

    fn output(&self) -> Vec<u8> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_stalled(&self, stalled: bool) {
        self.paused.store(stalled, Ordering::Relaxed);
    }

    fn try_wait(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().expect("try_wait attach client")
    }
}

impl Drop for StallablePtyClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Marker helpers
// ---------------------------------------------------------------------------

/// The highest MARKER-N the client has shown so far.
fn last_marker(bytes: &[u8]) -> Option<u32> {
    let mut best = None;
    let mut from = 0;
    while let Some(at) = find_bytes(&bytes[from..], b"MARKER-") {
        let start = from + at + b"MARKER-".len();
        let end = bytes[start..]
            .iter()
            .position(|b| !b.is_ascii_digit())
            .map(|e| start + e)
            .unwrap_or(bytes.len());
        if end > start {
            if let Some(value) = std::str::from_utf8(&bytes[start..end])
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
            {
                best = Some(best.map_or(value, |b: u32| b.max(value)));
            }
        }
        from = start;
    }
    best
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn escape(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\x1b', "<ESC>")
}

/// Long enough for the socket buffer (~200 KB at net.core.wmem_default) to
/// fill behind the stalled client and the stall guard to count several
/// barren ticks -- still far short of the reap window, so a survivor test
/// never races it.
const STALL: Duration = Duration::from_secs(18);

/// The reap window is ATTACH_STALL_TICKS x ATTACH_STALL_TICK = 60s; stalling
/// past it plus one margin means the worker must have reaped the attach by
/// the time the client unblocks.
const WEDGE: Duration = Duration::from_secs(80);

#[test]
fn stalled_client_survives_a_flooding_session_and_resumes_live_output() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("stall");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_flood_session(&harness, &workspace, "stall");
    let _guard = SessionGuard {
        harness: &harness,
        id: id.clone(),
    };

    let mut client = StallablePtyClient::spawn(&harness, &id, 24, 80);
    // Attach established and streaming: wait for the first markers.
    let deadline = Instant::now() + Duration::from_secs(30);
    while last_marker(&client.output()).is_none() {
        assert!(
            Instant::now() < deadline,
            "the client never saw the flood; captured:\n{}",
            escape(&client.output())
        );
        assert!(
            client.try_wait().is_none(),
            "the attach client exited early"
        );
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_millis(500));
    let before_stall = last_marker(&client.output()).expect("a marker before the stall");

    // --- the stall under test ---
    client.set_stalled(true);
    thread::sleep(STALL);
    client.set_stalled(false);

    // The attach must still be alive after the stall.
    let liveness_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < liveness_deadline {
        assert!(
            client.try_wait().is_none(),
            "the attach client exited during/after the stall (the worker dropped \
             a stalled client): goodbye line:\n{}",
            escape(&client.output())
        );
        thread::sleep(Duration::from_millis(100));
    }

    // And it must be receiving LIVE output again -- markers well past the
    // pre-stall position, on the same connection.
    let target = before_stall.saturating_add(30);
    let deadline = Instant::now() + Duration::from_secs(45);
    while last_marker(&client.output()).is_none_or(|m| m < target) {
        if let Some(status) = client.try_wait() {
            panic!(
                "the attach client exited (status {status:?}) instead of resuming \
                 live output; goodbye line:\n{}",
                escape(&client.output())
            );
        }
        assert!(
            Instant::now() < deadline,
            "live output never resumed past MARKER-{target}; captured tail:\n{}",
            escape(&{
                let out = client.output();
                let tail = out.len().saturating_sub(4096);
                out[tail..].to_vec()
            })
        );
        thread::sleep(Duration::from_millis(100));
    }

    let out = client.output();
    assert!(
        find_bytes(&out, b"Connection to").is_none(),
        "the client printed a connection-loss goodbye even though it survived:\n{}",
        escape(&out)
    );
}

#[test]
fn wedged_client_is_reaped_after_sustained_zero_drain() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("wedge");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_flood_session(&harness, &workspace, "wedge");
    let _guard = SessionGuard {
        harness: &harness,
        id: id.clone(),
    };

    let mut client = StallablePtyClient::spawn(&harness, &id, 24, 80);
    let deadline = Instant::now() + Duration::from_secs(30);
    while last_marker(&client.output()).is_none() {
        assert!(
            Instant::now() < deadline,
            "the client never saw the flood; captured:\n{}",
            escape(&client.output())
        );
        thread::sleep(Duration::from_millis(50));
    }

    // Wedge the "terminal" past the reap window: the client wedges writing
    // into the frozen pty, stops reading its socket, and the worker's stall
    // guard must close the attach instead of carrying a blocked writer (and
    // its subscriber + connection slots) until the session ends.
    client.set_stalled(true);
    thread::sleep(WEDGE);
    client.set_stalled(false);

    // Unfreezing drains the pty, the wedged write completes, and the next
    // socket read sees the worker's shutdown: the client must exit on its
    // own with the connection-loss goodbye (which the embedding client --
    // PocketShell's reattach ladder -- already treats as "dial again").
    let exit_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(_status) = client.try_wait() {
            break;
        }
        assert!(
            Instant::now() < exit_deadline,
            "the worker never reaped the wedged attach; the client is still alive"
        );
        thread::sleep(Duration::from_millis(200));
    }
    let out = client.output();
    assert!(
        find_bytes(&out, b"Connection to").is_some(),
        "the reaped client did not report the connection loss; captured tail:\n{}",
        escape(&{
            let tail = out.len().saturating_sub(4096);
            out[tail..].to_vec()
        })
    );
}
