//! Scratch differential probe for live-session garbling that survives the
//! fixed routes in `attach_divergence.rs` (scroll-region reset, last-row wrap,
//! resize mid-frame). The workload here mimics what the real agent TUIs
//! (codex / Claude Code, both Ink) do over minutes of a live session:
//!
//!   * a bottom-anchored DECSTBM sub-range the transcript scrolls through,
//!     re-asserted constantly (codex "holds a sub-range almost constantly");
//!   * partial repaints at absolute columns on the composer row (the last
//!     row), never clearing the row first -- the welding class;
//!   * lines long enough to wrap off the last column;
//!   * SGR color runs and full-screen `ED2` redraws;
//!   * `ESC 7 ESC [ r ESC 8` at startup, like Claude Code;
//!   * a SIGWINCH handler that repaints at the new geometry, like a real TUI.
//!
//! The assertion is the same differential one: the host terminal (every byte
//! `a attach` wrote, replayed into a parser at the physical size) must agree
//! with the worker's own screen model, row for row.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness (isolated runtime/state dirs -- mirrors tests/attach_divergence.rs)
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
    let child = cmd.spawn().expect("failed to spawn command");
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => panic!("failed to wait for command: {error}"),
        Err(_) => {
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
            }
            panic!("command (pid {pid}) did not finish within {timeout:?}");
        }
    }
}

fn start_session_args(harness: &Harness, workspace: &Path, tag: &str, extra: &[&str]) -> String {
    let workspace = workspace.to_str().expect("utf8 workspace path");
    let mut args: Vec<&str> = vec!["start", "--workspace", workspace, "--tag", tag, "--json"];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--", "bash", "--norc", "-l"]);
    let stdout = harness.run_ok(&args, Duration::from_secs(15));
    let value: Value = serde_json::from_str(&stdout).expect("`a start` output is JSON");
    value["id"]
        .as_str()
        .expect("session id in start output")
        .to_string()
}

fn start_session(harness: &Harness, workspace: &Path, tag: &str) -> String {
    start_session_args(harness, workspace, tag, &[])
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
// PtyClient (ported from tests/attach_divergence.rs)
// ---------------------------------------------------------------------------

struct PtyClient {
    child: std::process::Child,
    master: std::fs::File,
    captured: Arc<Mutex<Vec<u8>>>,
}

impl PtyClient {
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
        let sink = captured.clone();
        let mut reader = master.try_clone().expect("dup pty master for reading");
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
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
            master,
            captured,
        }
    }

    fn output(&self) -> Vec<u8> {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("write to pty master");
        self.master.flush().expect("flush pty master");
    }

    fn wait_for(&self, needle: &[u8], what: &str) {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let out = self.output();
            if find_bytes(&out, needle).is_some() {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what} ({:?}) in the client's output; captured:\n{}",
                    String::from_utf8_lossy(needle),
                    escape(&out)
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn resize(&self, rows: u16, cols: u16) {
        use std::os::unix::io::AsRawFd;
        aplexer::set_winsize(self.master.as_raw_fd(), rows, cols).expect("resize the pty");
    }

    fn detach(mut self) {
        self.send(&[0x02, b'd']);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                _ => break,
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn escape(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\x1b', "<ESC>")
}

// ---------------------------------------------------------------------------
// The two screens being compared (ported from tests/attach_divergence.rs)
// ---------------------------------------------------------------------------

fn host_terminal(bytes: &[u8], rows: u16, cols: u16) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    parser
}

/// The host replay for a capture that spans a client resize: bytes before
/// `split` at the pre-resize geometry, the rest after an in-place resize --
/// what a real terminal does when the user drags its edge.
fn host_terminal_resized(
    bytes: &[u8],
    split: usize,
    before: (u16, u16),
    after: (u16, u16),
) -> vt100::Parser {
    let mut parser = vt100::Parser::new(before.0, before.1, 0);
    parser.process(&bytes[..split.min(bytes.len())]);
    parser.screen_mut().set_size(after.0, after.1);
    if split < bytes.len() {
        parser.process(&bytes[split..]);
    }
    parser
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Rendering {
    rows: Vec<String>,
    cursor: (u16, u16),
}

impl Rendering {
    fn render(&self) -> String {
        let mut out = String::new();
        for (index, row) in self.rows.iter().enumerate() {
            out.push_str(&format!("{:>3} |{row}|\n", index + 1));
        }
        out.push_str(&format!(
            "cursor: row {} col {}\n",
            self.cursor.0 + 1,
            self.cursor.1 + 1
        ));
        out
    }
}

fn rendering_of(screen: &vt100::Screen, rows: usize) -> Rendering {
    let cols = screen.size().1;
    let lines: Vec<String> = (0..rows)
        .map(|row| {
            screen
                .contents_between(row as u16, 0, row as u16, cols)
                .trim_end()
                .to_string()
        })
        .collect();
    Rendering {
        rows: lines,
        cursor: screen.cursor_position(),
    }
}

fn worker_screen(harness: &Harness, id: &str, rows: u16, cols: u16) -> Rendering {
    let output = harness.run(&["capture", id, "--screen"], Duration::from_secs(5));
    assert!(
        output.status.success(),
        "`a capture --screen` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(&output.stdout);
    rendering_of(parser.screen(), rows as usize)
}

fn divergence_report(host: &Rendering, worker: &Rendering) -> Option<String> {
    let mut report = String::new();
    for (index, (h, w)) in host.rows.iter().zip(worker.rows.iter()).enumerate() {
        if h != w {
            report.push_str(&format!(
                "row {} differs:\n  host   |{h}|\n  worker |{w}|\n",
                index + 1
            ));
            if !w.trim().is_empty() {
                if let Some(at) = host.rows.iter().position(|row| row == w) {
                    report.push_str(&format!(
                        "  (the worker's row {} content is on host row {} -- an offset of {})\n",
                        index + 1,
                        at + 1,
                        at as i64 - index as i64
                    ));
                }
            }
        }
    }
    if host.cursor != worker.cursor {
        report.push_str(&format!(
            "cursor differs: host row {} col {}, worker row {} col {} (row offset {})\n",
            host.cursor.0 + 1,
            host.cursor.1 + 1,
            worker.cursor.0 + 1,
            worker.cursor.1 + 1,
            host.cursor.0 as i64 - worker.cursor.0 as i64
        ));
    }
    if report.is_empty() {
        None
    } else {
        Some(report)
    }
}

/// Polls both screens until they agree (the worker is always at or ahead of
/// the host by construction, so a transient in-flight difference is waited
/// out); on timeout, fails with the diverging rows.
#[allow(clippy::too_many_arguments)]
fn assert_eventual_agreement(
    client: &PtyClient,
    harness: &Harness,
    id: &str,
    host_of: impl Fn(&[u8]) -> vt100::Parser,
    model_rows: u16,
    cols: u16,
    require: &str,
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last: Option<String> = None;
    loop {
        let bytes = client.output();
        let host = rendering_of(&host_of(&bytes).screen().clone(), model_rows as usize);
        let worker = worker_screen(harness, id, model_rows, cols);
        let present = |r: &Rendering| r.rows.iter().any(|row| row.contains(require));
        if present(&host) && present(&worker) {
            match divergence_report(&host, &worker) {
                None => return,
                Some(report) => last = Some(report),
            }
        }
        if Instant::now() >= deadline {
            panic!(
                "{what}: the host terminal and the worker's own screen model never agreed.\n\n\
                 == DIVERGENCE ==\n{}\n\
                 == host ==\n{}\n== worker ==\n{}\n== raw bytes ==\n{}",
                last.unwrap_or_else(|| "(marker never reached both screens)".into()),
                host.render(),
                worker.render(),
                escape(&bytes)
            );
        }
        thread::sleep(Duration::from_millis(150));
    }
}

// The worker's model is the physical terminal minus the reserved status row.
const HOST_ROWS: u16 = 24;
const HOST_COLS: u16 = 80;
const MODEL_ROWS: u16 = 23;

// ---------------------------------------------------------------------------
// The fake agent TUI
// ---------------------------------------------------------------------------

/// A python3 script that plays a codex-style inline TUI. It repaints on
/// SIGWINCH like a real Ink app, holds a bottom-anchored sub-range, paints
/// the composer at absolute columns, and writes `DONE-PROBE-MARKER` at the
/// end. `steps` frames at ~30 ms each.
fn fake_agent_script(steps: usize) -> String {
    format!(
        r#"
import sys, time, signal, os
out = sys.stdout
W = "\x1b["
state = {{"step": 0, "winch": False}}
LOG = open("/tmp/fake_agent.log", "a")

def log(msg):
    LOG.write(msg + "\n"); LOG.flush()

def w(s):
    out.write(s); out.flush()

def on_winch(signum, frame):
    state["winch"] = True

signal.signal(signal.SIGWINCH, on_winch)

def size():
    return os.get_terminal_size()

def full_repaint():
    cols, rows = size()
    log(f"repaint rows={{rows}} cols={{cols}} step={{state['step']}}")
    w("\x1b[2J\x1b[H")
    w(W + "2;1H+" + "-" * (cols - 2) + "+")
    for r in range(3, min(rows, 20)):
        w(f"{{W}}{{r}};1H|" + " " * (cols - 2) + "|")
    if rows >= 20:
        w(W + "20;1H+" + "-" * (cols - 2) + "+")
    w(W + f"{{rows - 2}};2Htranscript:")
    # codex-style bottom-anchored sub-range, re-asserted every frame
    if rows >= 23:
        w(W + "23;1H" + W + "7" + W + f"5;{{rows - 3}}r" + W + "8")
    compose(rows, cols)

def compose(rows, cols):
    step = state["step"]
    last = rows - 3  # the workload's own last row
    w(W + f"{{last}};2H" + W + "K")
    w(W + f"{{last}};2G" + f"step-{{step}}")
    if cols > 40:
        w(W + f"{{last}};30G" + f"tokens:{{step * 137}}")
    if cols > 60:
        w(W + f"{{last}};55G" + ("running" if step % 2 else "THINKING"))

w("\x1b[2J\x1b[H" + "\x1b7" + W + "r" + "\x1b8")  # Claude Code startup reset
log("start size=" + repr(size()))
full_repaint()

for step in range(1, {steps} + 1):
    state["step"] = step
    if state["winch"]:
        state["winch"] = False
        log("winch size=" + repr(size()))
        full_repaint()
    cols, rows = size()
    last = rows - 3
    # Transcript line scrolls through the bottom-anchored region.
    w(W + f"{{rows - 5}};1H" + W + "K" + f"step {{step}}: transcript line scrolls the bottom-anchored region, a long-ish line of text for realistic column widths, words words {{step}}")
    # Composer partial repaints on the LAST row (absolute columns, no clear).
    compose(rows, cols)
    # A colored wrap off the last column every 10th step.
    if step % 10 == 0:
        w(W + f"{{last}};2G" + W + "32m" + "green" + W + "0m" + " status line that keeps going past the last column of the row to force a wrap off the edge of the screen now")
    # Periodic full repaint, like the real TUIs do on their tick.
    if step % 40 == 0:
        full_repaint()
    time.sleep(0.03)

w(W + f"{{rows - 3}};2H" + W + "K" + "DONE-PROBE-MARKER")
w("\x1b[r")
"#
    )
}

fn write_agent_script(workspace: &Path, steps: usize) -> String {
    let script_path = workspace.join("fake_agent.py");
    std::fs::write(&script_path, fake_agent_script(steps)).unwrap();
    script_path.to_str().expect("utf8 script path").to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// A long codex-style live run, no resize: constant bottom-anchored sub-range,
/// partial composer repaints on the last row, wraps, colors, periodic ED2.
#[test]
fn long_codex_style_run_keeps_host_and_worker_aligned() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("codex-live");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "live");
    let _guard = SessionGuard {
        harness: &harness,
        id: id.clone(),
    };
    let client = PtyClient::spawn(&harness, &id, HOST_ROWS, HOST_COLS);
    client.wait_for(b"\x1b[1;23r", "the client's status-row reservation");
    thread::sleep(Duration::from_millis(900));

    let script = write_agent_script(&workspace, 120);
    harness.run_ok(
        &["send", &id, &format!("python3 {script}"), "--enter"],
        Duration::from_secs(5),
    );

    client.wait_for(b"DONE-PROBE-MARKER", "the workload's final frame");
    thread::sleep(Duration::from_millis(400));

    assert_eventual_agreement(
        &client,
        &harness,
        &id,
        |bytes| host_terminal(bytes, HOST_ROWS, HOST_COLS),
        MODEL_ROWS,
        HOST_COLS,
        "DONE-PROBE-MARKER",
        "long codex-style live run",
    );

    client.detach();
}

/// The every-attach case: the session starts DETACHED at the 24x80 default,
/// the workload gets going holding its sub-range, and only then does a client
/// attach. First attach shrinks the PTY from 24 to 23 rows -- exactly the
/// truncation `ScreenTracker::try_set_size` compensates for, but its
/// compensation is skipped whenever a DECSTBM sub-range is in force, which is
/// how codex spends most of its life.
#[test]
fn first_attach_to_running_session_keeps_host_and_worker_aligned() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("late-attach");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "late");
    let _guard = SessionGuard {
        harness: &harness,
        id: id.clone(),
    };
    let script = write_agent_script(&workspace, 200);
    harness.run_ok(
        &["send", &id, &format!("python3 {script}"), "--enter"],
        Duration::from_secs(5),
    );
    // Let the detached session fill its screen and hold its sub-range.
    thread::sleep(Duration::from_millis(1500));

    let client = PtyClient::spawn(&harness, &id, HOST_ROWS, HOST_COLS);
    client.wait_for(b"\x1b[1;23r", "the client's status-row reservation");

    client.wait_for(b"DONE-PROBE-MARKER", "the workload's final frame");
    thread::sleep(Duration::from_millis(400));

    assert_eventual_agreement(
        &client,
        &harness,
        &id,
        |bytes| host_terminal(bytes, HOST_ROWS, HOST_COLS),
        MODEL_ROWS,
        HOST_COLS,
        "DONE-PROBE-MARKER",
        "first attach to an already-running session",
    );

    client.detach();
}

/// A live SHRINK mid-run while the workload holds its sub-range: the
/// `try_set_size` SU compensation is skipped under a sub-range, so this is
/// the second half of the same hole.
#[test]
fn midrun_shrink_keeps_host_and_worker_aligned() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("shrink");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "shrink");
    let _guard = SessionGuard {
        harness: &harness,
        id: id.clone(),
    };
    let client = PtyClient::spawn(&harness, &id, HOST_ROWS, HOST_COLS);
    client.wait_for(b"\x1b[1;23r", "the client's status-row reservation");
    thread::sleep(Duration::from_millis(900));

    let script = write_agent_script(&workspace, 400);
    harness.run_ok(
        &["send", &id, &format!("python3 {script}"), "--enter"],
        Duration::from_secs(5),
    );
    thread::sleep(Duration::from_millis(600));

    let split = client.output().len();
    let after = (20u16, 90u16);
    client.resize(after.0, after.1);

    client.wait_for(b"DONE-PROBE-MARKER", "the workload's final frame");
    thread::sleep(Duration::from_millis(400));

    let model_rows = after.0 - 1;
    assert_eventual_agreement(
        &client,
        &harness,
        &id,
        |bytes| host_terminal_resized(bytes, split, (HOST_ROWS, HOST_COLS), after),
        model_rows,
        after.1,
        "DONE-PROBE-MARKER",
        "mid-run shrink with a sub-range in force",
    );

    client.detach();
}

/// Same, growing -- the direction where the old reserved row becomes an
/// ordinary workload row that may still hold stale bar text.
#[test]
fn midrun_grow_keeps_host_and_worker_aligned() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("grow");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "grow");
    let _guard = SessionGuard {
        harness: &harness,
        id: id.clone(),
    };
    let client = PtyClient::spawn(&harness, &id, HOST_ROWS, HOST_COLS);
    client.wait_for(b"\x1b[1;23r", "the client's status-row reservation");
    thread::sleep(Duration::from_millis(900));

    let script = write_agent_script(&workspace, 400);
    harness.run_ok(
        &["send", &id, &format!("python3 {script}"), "--enter"],
        Duration::from_secs(5),
    );
    thread::sleep(Duration::from_millis(600));

    let split = client.output().len();
    let after = (30u16, 100u16);
    client.resize(after.0, after.1);

    client.wait_for(b"DONE-PROBE-MARKER", "the workload's final frame");
    thread::sleep(Duration::from_millis(400));

    let model_rows = after.0 - 1;
    assert_eventual_agreement(
        &client,
        &harness,
        &id,
        |bytes| host_terminal_resized(bytes, split, (HOST_ROWS, HOST_COLS), after),
        model_rows,
        after.1,
        "DONE-PROBE-MARKER",
        "mid-run grow",
    );

    client.detach();
}

/// The wheel + type-through gesture, under fire: enter the pager with a wheel
/// roll, hand the keyboard to the session with `i`, and have the workload
/// keep painting the way the real agent TUIs do -- every frame opens with an
/// `Erase in Display` from a mid-screen cursor position, and ED ignores
/// scroll margins, so on the host every frame erases the reserved row the
/// typing bar lives on.
///
/// Reproduced against a real zcodex session before the fix: the bar was
/// drawn once at `i` and then vanished for good. The frame loop `continue`d
/// past every bar path while scroll mode was active, the status tick's dirty
/// check saw unchanged text and skipped, and nothing invalidated it -- the
/// user was left with a blank bottom row for the rest of the type-through.
///
/// The assertion is on the host terminal only (the worker model never sees
/// the bar): after two seconds of frames that each erase the row, the typing
/// bar's wording must still be on the physical last row.
#[test]
fn typing_bar_survives_workload_erases_during_type_through() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("type-through");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "tt");
    let _guard = SessionGuard {
        harness: &harness,
        id: id.clone(),
    };
    let mut client = PtyClient::spawn(&harness, &id, HOST_ROWS, HOST_COLS);
    client.wait_for(b"\x1b[1;23r", "the client's status-row reservation");
    thread::sleep(Duration::from_millis(900));

    // The workload: codex-style frames -- each repaint opens with
    // `CUP row 2` + `ED0` (erase to end of screen, margins be damned), then
    // repaints a composer line. It holds no sub-range so nothing else is in
    // play; the erase alone is the thing under test.
    let workload = r#"for i in $(seq 1 120); do printf "\033[2;1H\033[Jframe $i\n\033[10;2Hcomposer-$i"; sleep 0.05; done "#;
    harness.run_ok(
        &["send", &id, &format!("{} &", workload), "--enter"],
        Duration::from_secs(5),
    );
    // Wait until the erasing frames are actually flowing.
    client.wait_for(b"composer-3", "the workload's erase frames");
    thread::sleep(Duration::from_millis(200));

    // Wheel roll: SGR wheel-up at (col, row) -- the gesture every user has
    // in their fingers. The client borrows the mouse, so this opens the
    // pager; the host's last row switches to the SCROLL readout.
    for row in 5..8 {
        client.send(format!("\x1b[<64;30;{row}M").as_bytes());
        thread::sleep(Duration::from_millis(120));
    }
    client.wait_for(b"SCROLL", "the pager's bar");

    // `i`: hand the keyboard to the session. The bar's wording changes to
    // TYPE and the relay starts streaming workload bytes to the host again
    // -- from this byte on, every workload ED0 erases the bar row.
    client.send(b"i");
    client.wait_for(b"TYPE", "the typing bar");

    // Let the erasing frames run well past the bar paint: seconds of frames
    // that each wipe the row. Before the fix the bar never came back; after
    // it, every Layout event invalidates and the frame loop flushes the
    // repair at the next chunk. Wait for the workload to finish so the final
    // state is settled, then poll: an assert at an arbitrary instant could
    // land inside the microsecond window between a frame's erase and the
    // repair that follows it within the same chunk.
    client.wait_for(b"composer-99", "late erase frames");
    thread::sleep(Duration::from_millis(600));

    let last_row_of = |client: &PtyClient| -> String {
        let host = host_terminal(&client.output(), HOST_ROWS, HOST_COLS);
        let screen = host.screen();
        let (rows, cols) = screen.size();
        screen
            .contents_between(rows - 1, 0, rows - 1, cols)
            .trim_end()
            .to_string()
    };
    // Stability, not presence: before the fix the row oscillated -- each
    // workload erase blanked it, the `Layout` arm painted the LIVE bar over
    // the pager's row, and the occasional TYPE repaint was accidental. A
    // single "TYPE was seen at some point" assertion passes on that mess.
    // Sample the row repeatedly across erasing frames and require it to be
    // the typing bar essentially always: every erase must be repaired within
    // its chunk, and the live bar must never overpaint the pager's row.
    let mut good = 0;
    let mut samples = Vec::new();
    for _ in 0..12 {
        thread::sleep(Duration::from_millis(250));
        let row = last_row_of(&client);
        let ok = row.contains("TYPE");
        if ok {
            good += 1;
        }
        samples.push(row);
    }
    assert!(
        good >= 11,
        "the typing bar must hold its row through workload erases during \
         type-through ({good}/12 samples ok); rows seen: {samples:?}"
    );
    assert!(
        samples.last().map(|r| r.contains("TYPE")).unwrap_or(false),
        "the bar must be present in the final state; rows seen: {samples:?}"
    );
    // Non-vacuity: the workload's frames really did reach the host while
    // typing, so the erases really were in play.
    let host = host_terminal(&client.output(), HOST_ROWS, HOST_COLS);
    let screen = host.screen();
    let (rows, cols) = screen.size();
    let body: String = (0..rows - 1)
        .map(|r| screen.contents_between(r, 0, r, cols))
        .collect();
    assert!(
        body.contains("frame"),
        "the workload's frames must be streaming during type-through: {body:?}"
    );

    client.detach();
    harness.run_ok(&["kill", &id, "--signal", "KILL"], Duration::from_secs(5));
}
