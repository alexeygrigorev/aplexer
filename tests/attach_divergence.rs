// Differential reproduction harness for the "Claude Code renders garbled
// through `a attach`" report: two frames of output landing on the SAME
// physical row, welded together in 1-6 character runs, plus a status bar that
// appears duplicated one row apart.
//
// The claim under test is a *host/model divergence*: the physical terminal's
// rows are offset from the worker's own model of the workload screen by one
// row, so every subsequent Ink-style partial repaint (absolute-column word
// painting, no row clear) welds onto whatever the previous frame left on that
// physical row.
//
// The method is the one the report asks for:
//
//   1. start a real aplexer session on an isolated runtime/state dir;
//   2. spawn the real `a attach` on a REAL pty and capture every byte it
//      writes to that pty (`PtyClient`, ported from tests/screen_snapshot.rs);
//   3. replay those bytes into a `vt100::Parser` sized at the HOST geometry --
//      this stands in for the user's terminal;
//   4. independently ask the worker for its own model of the workload screen
//      (`a capture --screen`);
//   5. assert the two agree, row for row and cursor row for cursor row.
//
// Anything the two disagree about is the bug, and the assertion prints which
// row diverged and by how much.
//
// OUTCOME (recorded here so the file is self-describing). Both routes to the
// bug were reproduced against the tree as of this file's first run; a fix for
// the line-feed route landed in `ClientScreen::relay` while this was being
// written, and the wrap route is still open:
//
//   * `ink_style_scroll_region_reset_keeps_host_and_worker_aligned` REPRODUCED
//     the reported +1 row offset (Claude Code's `ESC 7 ESC [ r ESC 8` followed
//     by line feeds off the workload's last row). It now PASSES against the
//     `ClientScreen::relay` rework, and stands as the regression test for it.
//   * `wrapping_off_the_last_row_keeps_host_and_worker_aligned` REPRODUCES and
//     still FAILS: the same reset followed by a *wrap* off the last column of
//     the workload's last row leaves the host exactly one row below the
//     worker's model, permanently, and drags the status bar text onto a
//     workload row -- the "status bar duplicated one row apart" symptom. The
//     assertion is written for the CORRECT behavior, so its failure is the
//     evidence.
//   * `resize_during_a_frame_keeps_host_and_worker_aligned` did NOT reproduce a
//     steady divergence and PASSES. The ungated DECSTBM of open issue #14 is
//     genuinely written (the test asserts it was, and that the bar's re-assert
//     followed, so the pass is not vacuous), but the re-assert repairs the
//     region before anything scrolls through it. Kept as a negative control.
//
// Every test runs against isolated APLEXER_RUNTIME_DIR / APLEXER_STATE_DIR
// temp dirs (short /tmp paths, so the unix socket bind stays under SUN_LEN),
// so this never observes or touches a real user's sessions.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Harness (isolated runtime/state dirs -- mirrors tests/screen_snapshot.rs)
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

fn start_session(harness: &Harness, workspace: &Path, tag: &str) -> String {
    let workspace = workspace.to_str().expect("utf8 workspace path");
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
            "--norc",
            "-l",
        ],
        Duration::from_secs(15),
    );
    let value: Value = serde_json::from_str(&stdout).expect("`a start` output is JSON");
    value["id"]
        .as_str()
        .expect("session id in start output")
        .to_string()
}

/// Kills the session on drop.
///
/// Two of the tests in this file are *meant* to fail while the bug is open, so
/// the trailing `a kill` in their bodies is never reached. Without this, every
/// failing run would leave a worker and its shell alive after the harness'
/// temp dirs had already been unlinked out from under them.
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
// PtyClient: a live `a attach` on a real pty, every byte it writes captured.
// Ported from tests/screen_snapshot.rs (which owns the original) because
// integration tests are separate crates and cannot share it.
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

    fn mark(&self) -> usize {
        self.captured
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("write to pty master");
        self.master.flush().expect("flush pty master");
    }

    fn wait_for(&self, needle: &[u8], from: usize, what: &str) -> usize {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let out = self.output();
            if out.len() >= from {
                if let Some(at) = find_bytes(&out[from..], needle) {
                    return from + at + needle.len();
                }
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what} ({:?}) in the client's output after byte {from}; \
                     captured:\n{}",
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
// The two screens being compared.
// ---------------------------------------------------------------------------

/// The user's terminal: every byte `a attach` wrote, replayed into a parser of
/// the *physical* terminal's size.
fn host_terminal(bytes: &[u8], rows: u16, cols: u16) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    parser
}

/// The same, for a capture that spans a terminal resize.
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

/// One rendering of a screen: its rows (trimmed, right-padded to `rows`) and
/// its cursor position, both in the workload's own coordinate space.
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
    // Index the grid row by row rather than splitting `contents()` on
    // newlines. `contents()` joins a soft-wrapped pair of physical rows into
    // ONE line, so on a 24-row host carrying a single wrap it yields 23
    // lines and `.take(rows)` silently pulls the status-bar row into the slot
    // for workload row 23 -- while the 23-row worker model yields 22 lines
    // padded with a blank. That reports a difference between two screens that
    // are in fact identical on every row, which is exactly the false failure
    // this harness exists to rule out.
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

/// The worker's own model of the workload screen, obtained the way the design
/// doc says a reattaching client obtains it: the paintable snapshot from
/// `a capture --screen`, replayed into a fresh parser of the *workload's*
/// geometry (the physical terminal minus the client's reserved status row).
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

/// The first divergence between two renderings, as a human-readable report.
fn divergence_report(host: &Rendering, worker: &Rendering) -> Option<String> {
    let mut report = String::new();
    for (index, (h, w)) in host.rows.iter().zip(worker.rows.iter()).enumerate() {
        if h != w {
            report.push_str(&format!(
                "row {} differs:\n  host   |{h}|\n  worker |{w}|\n",
                index + 1
            ));
            // Where does the worker's row text actually live on the host?
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
            break;
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

/// Polls both screens until they agree, then returns. On timeout, fails with
/// the exact row that diverged and by how much.
///
/// Polling to convergence is not a weakened assertion: the worker is always at
/// or ahead of the host by construction (it parses each chunk before
/// forwarding it, and the chunk still has a socket and a pty write to cross),
/// so a transient in-flight difference is legitimate and is what gets waited
/// out. A genuine row offset is a *steady* difference that never converges.
#[allow(clippy::too_many_arguments)]
fn assert_host_matches_worker(
    client: &PtyClient,
    harness: &Harness,
    id: &str,
    host_of: impl Fn(&[u8]) -> vt100::Parser,
    workload_rows: u16,
    cols: u16,
    require: &str,
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    #[allow(unused_assignments)]
    let mut last: Option<(Rendering, Rendering, String)> = None;
    loop {
        let bytes = client.output();
        let host_parser = host_of(&bytes);
        let host = rendering_of(host_parser.screen(), workload_rows as usize);
        let worker = worker_screen(harness, id, workload_rows, cols);
        // Non-vacuity, structurally rather than as an afterthought: agreement
        // only counts once the workload's own output is on both screens.
        // Without this the loop happily "converges" on the pre-workload state
        // -- and it did. Every marker a `wait_for` can look for is also echoed
        // back by the pty as the command is *typed*, long before the shell
        // runs it, so waiting on one proves nothing. `require` is always a
        // `%s`-composed string that cannot occur in the command text at all.
        let present = |r: &Rendering| r.rows.iter().any(|row| row.contains(require));
        if present(&host) && present(&worker) {
            match divergence_report(&host, &worker) {
                None => return,
                Some(report) => last = Some((host, worker, report)),
            }
        } else if last.is_none() {
            last = Some((
                host,
                worker,
                format!("(the marker {require:?} never reached both screens)\n"),
            ));
        }
        if Instant::now() >= deadline {
            let (host, worker, report) = last.expect("a divergence was recorded");
            panic!(
                "{what}: the host terminal and the worker's own screen model never agreed.\n\n\
                 == DIVERGENCE ==\n{report}\n\
                 == host terminal (what the user sees), workload rows ==\n{}\n\
                 == worker's screen model ==\n{}\n\
                 == raw bytes `a attach` wrote to the pty ==\n{}\n",
                host.render(),
                worker.render(),
                escape(&bytes)
            );
        }
        thread::sleep(Duration::from_millis(150));
    }
}

fn wait_for_screen_marker(harness: &Harness, id: &str, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = harness.run(
            &["capture", id, "--screen", "--plain"],
            Duration::from_secs(5),
        );
        let captured = String::from_utf8_lossy(&output.stdout);
        if captured.contains(marker) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("session {id} screen never showed marker {marker:?}; last screen:\n{captured}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// The workload: Ink, reduced to the escape sequences that matter.
// ---------------------------------------------------------------------------

/// Claude Code's opening idiom, verbatim: save the cursor, reset the scroll
/// region to full screen, restore the cursor. `\033[r` with no parameters is
/// DECSTBM's "reset to the whole screen" -- and the *whole screen* the host
/// terminal has is one row taller than the screen the workload was told it
/// has, because `a attach` reserves the bottom row for its status bar and
/// tells the worker the terminal is 23 rows, not 24.
const INK_SCROLL_REGION_RESET: &str = r"\0337\033[r\0338";

/// Ink's painting style, from a real capture: words placed at absolute
/// columns with CHA (`\033[NG`), only the changed cells rewritten, and no
/// row clear in between -- which is exactly why a one-row host/model offset
/// welds two frames onto one physical row instead of merely looking shifted.
fn ink_frame(words: &[(u16, &str)]) -> String {
    let mut out = String::new();
    for (col, word) in words {
        out.push_str(&format!("\\033[{col}G{word}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// THE REPRODUCTION.
///
/// The workload does what Claude Code does at startup and then paints the way
/// Ink paints:
///
///   1. `ESC 7` `ESC [ r` `ESC 8` -- reset DECSTBM to the full screen. On the
///      host that means rows 1..24, because the host has 24 rows. On the
///      worker's model it means rows 1..23, because the worker was told the
///      terminal is 23 rows tall.
///   2. Park the cursor on the workload's own last row (row 23).
///   3. Line-feed off it, repeatedly, painting as it goes.
///
/// Step 3 is where the two screens come apart. The worker's 23-row model is
/// at its last row, so `LF` *scrolls*: the cursor stays on row 23 and every
/// row moves up one. The host has 24 rows and a scroll region that now covers
/// all of them, so the same `LF` just walks the cursor down onto row 24 -- the
/// client's reserved status-bar row. From that byte on, the host is exactly
/// one row below where the workload believes it is, and it stays there.
///
/// `ClientScreen::relay`'s reserved-row walk (docs/terminal-state-design.md
/// section 7.1) is the machinery meant to catch this. When this test was
/// written that walk was gated on `margins()` reporting a *sub-range*
/// (`Some((_, bottom)) if bottom <= last_row`), and a full-screen reset
/// reports `None` -- so it never engaged for the one sequence Claude Code
/// actually sends, and this test failed with a steady +1 row offset. It
/// passes against the reworked `relay`, and is kept as that rework's
/// end-to-end regression test.
///
/// Whether the corruption happened at all was originally a race, and the
/// capture showed it directly: the worker reports the margin reset as a
/// `Layout` event, the client force-redraws its bar, and that redraw
/// re-asserts `\x1b[1;23r`. If it landed in the gap between the workload's
/// `\x1b[r` and its next line feed, the host was repaired in time:
///
///   ...ESC[2J ESC[H ESC7 ESC[r | ESC[1;23r ESC[H | ESC8 ESC[23;1H ...
///                  workload's -^   ^- client's repair, inside the gap
///
/// Measured at roughly 50/50 in this harness, because it depends on where the
/// pty read boundary falls. A real TUI writes its reset and its frame in one
/// `write(2)`, so in production the repair loses essentially every time --
/// which is why the user saw the corruption consistently.
///
/// The status bar's own DECSTBM re-assert repairs the *region* on its next
/// tick, and the cursor restore repairs the *cursor* -- but neither can move
/// text that has already been painted on the wrong physical row. Ink then
/// repaints only the cells it changed, at absolute columns, over rows that
/// still hold the previous frame: the welded `That'sddecisivel-xandgit/...`
/// runs from the report.
///
/// Assertion is on the CORRECT behavior: the host terminal and the worker's
/// own screen model must agree, row for row.
#[test]
fn ink_style_scroll_region_reset_keeps_host_and_worker_aligned() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("ink-reset");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "ink");
    let _guard = SessionGuard {
        harness: &harness,
        id: id.clone(),
    };
    let client = PtyClient::spawn(&harness, &id, 24, 80);
    client.wait_for(b"\x1b[1;23r", 0, "the client's status-row reservation");
    // Quiesce: the shell's prompt and the attach flash have to be on both
    // screens before the workload starts, or the comparison races them.
    thread::sleep(Duration::from_millis(900));

    // One printf, so the reset and the line feeds that follow it reach the
    // host inside a single relayed chunk -- exactly as a real TUI's frame
    // write does.
    //
    // The closing marker is composed by `printf` from a `%s`, so it cannot
    // occur in the command text the pty echoes back as the line is typed;
    // seeing it really does mean the shell executed the workload.
    let workload = format!(
        r#"printf '\033[2J\033[H{reset}\033[23;1H{f1}\r\n{f2}\r\n{f3}-%s\r\n' FRAME"#,
        reset = INK_SCROLL_REGION_RESET,
        f1 = ink_frame(&[(2, "FRAME-ONE"), (20, "alpha"), (40, "bravo")]),
        f2 = ink_frame(&[(2, "FRAME-TWO"), (20, "charlie"), (40, "delta")]),
        f3 = ink_frame(&[(20, "echo"), (40, "foxtrot"), (2, "LAST")]),
    );
    harness.run_ok(&["send", &id, &workload, "--enter"], Duration::from_secs(5));
    wait_for_screen_marker(&harness, &id, "LAST-FRAME");
    client.wait_for(b"LAST-FRAME", 0, "the workload's last frame");

    assert_host_matches_worker(
        &client,
        &harness,
        &id,
        |bytes| host_terminal(bytes, 24, 80),
        23,
        80,
        "LAST-FRAME",
        "after Claude Code's `ESC 7 ESC [ r ESC 8` scroll-region reset followed by line \
         feeds off the workload's last row",
    );

    client.detach();
    harness.run_ok(&["kill", &id, "--signal", "KILL"], Duration::from_secs(5));
}

/// The same divergence reached the other way the report describes: a *wrap*
/// off the last column of the workload's last row, rather than an explicit
/// line feed.
///
/// `WRAPPING` starts at column 76 of an 80-column row, so its last three
/// characters wrap. On the worker's 23-row model that wrap is off the last
/// row and therefore *scrolls*; on the 24-row host, whose region the
/// workload's own `\x1b[r` has just widened to cover the client's reserved
/// row, it merely steps down onto row 24. Everything after it is one row low,
/// permanently -- and the status bar's own text ends up on a workload row,
/// which is the "status bar rows appear duplicated one row apart" half of the
/// report.
///
/// `ClientScreen::relay`'s doc comment named this case explicitly as one it
/// deliberately does not rewrite ("detecting it needs per-byte column
/// tracking through the whole chunk"), on the grounds that it self-heals
/// because every status-bar redraw restores the cursor absolutely. The cursor
/// does heal. The row the text landed on does not.
///
/// STILL FAILING. The assertion is on the CORRECT behavior -- the host
/// terminal and the worker's own screen model must agree, row for row -- so
/// this failure is the evidence, not a broken test.
#[test]
fn wrapping_off_the_last_row_keeps_host_and_worker_aligned() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("ink-wrap");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "wrap");
    let _guard = SessionGuard {
        harness: &harness,
        id: id.clone(),
    };
    let client = PtyClient::spawn(&harness, &id, 24, 80);
    client.wait_for(b"\x1b[1;23r", 0, "the client's status-row reservation");
    thread::sleep(Duration::from_millis(900));

    // Park on the workload's last row at column 76 and print 8 characters:
    // the row is 80 columns wide, so the last 4 wrap. On a 23-row model that
    // wrap scrolls; on the 24-row host with a full-screen region it does not.
    let workload = format!(
        r#"printf '\033[2J\033[H{reset}\033[23;76HWRAPPING\r\n{after}-%s\r\n' MARK"#,
        reset = INK_SCROLL_REGION_RESET,
        after = ink_frame(&[(30, "tail"), (2, "AFTER")]),
    );
    harness.run_ok(&["send", &id, &workload, "--enter"], Duration::from_secs(5));
    wait_for_screen_marker(&harness, &id, "AFTER-MARK");
    client.wait_for(b"AFTER-MARK", 0, "the post-wrap frame");

    assert_host_matches_worker(
        &client,
        &harness,
        &id,
        |bytes| host_terminal(bytes, 24, 80),
        23,
        80,
        "AFTER-MARK",
        "after a wrap off the last column of the workload's last row",
    );

    client.detach();
    harness.run_ok(&["kill", &id, "--signal", "KILL"], Duration::from_secs(5));
}

/// The resize half of the report ("terminal resize mid-frame"), against the
/// ungated DECSTBM write in `a attach`'s resize poller (open issue #14):
/// `apply_terminal_layout` writes `\x1b[1;{rows-1}r` unconditionally, with no
/// regard for a scroll-region sub-range the workload has in force -- unlike
/// `status_bar_sequence`, which is `ClientScreen::margins`-aware.
///
/// The workload holds a `5;15` sub-range and paints into its bottom row
/// continuously, so output is genuinely in flight when the terminal changes
/// size. The poller then clobbers `5;15` with its own `\x1b[1;23r`, and until
/// the status bar's next re-assert every line the workload feeds off row 15
/// scrolls the *whole host screen* instead of the eleven rows the worker's
/// model scrolls.
///
/// The resize is a SHRINK (40 -> 24 rows) on purpose. Growing the terminal
/// turns the client's old reserved bar row into an ordinary workload row that
/// still holds stale bar text -- a known, purely cosmetic difference from the
/// worker's model (tests/screen_snapshot.rs documents it) that would swamp
/// this comparison with noise. Shrinking drops that row instead.
#[test]
fn resize_during_a_frame_keeps_host_and_worker_aligned() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("resize-frame");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "resize");
    let _guard = SessionGuard {
        harness: &harness,
        id: id.clone(),
    };
    let client = PtyClient::spawn(&harness, &id, 40, 80);
    client.wait_for(b"\x1b[1;39r", 0, "the client's status-row reservation");
    thread::sleep(Duration::from_millis(900));

    // Clear first (so the echoed command line and prompt are not residue in
    // the comparison), set the sub-range, then feed row 15 for ~3 s: long
    // enough that the resize below lands squarely mid-frame and the poller's
    // 200 ms tick fires while the workload is still scrolling.
    let workload = "printf '\\033[2J\\033[H\\033[5;15r'; \
                    for i in $(seq 1 60); do printf '\\033[15;1HRLINE-%d\\r\\n' \"$i\"; \
                    sleep 0.05; done";
    harness.run_ok(&["send", &id, workload, "--enter"], Duration::from_secs(5));
    client.wait_for(
        b"RLINE-5",
        0,
        "the workload painting inside its 5;15 region",
    );

    let resize_at = client.mark();
    client.resize(24, 80);
    client.wait_for(b"RLINE-60", resize_at, "the workload's last line");

    // Non-vacuity: the window this test exists to observe has to have actually
    // opened. `\x1b[1;23r` is the poller's own ungated reservation -- nothing
    // else in the client writes it while a `5;15` sub-range is tracked -- so
    // its presence after the resize proves issue #14's ungated DECSTBM really
    // did clobber the workload's region here, and the bar's later `5;15`
    // proves the re-assert that repairs it also ran.
    let out = client.output();
    assert!(
        find_bytes(&out[resize_at..], b"\x1b[1;23r").is_some(),
        "the resize poller never wrote its ungated reservation, so this test never \
         exercised the window it is about; captured after the resize:\n{}",
        escape(&out[resize_at..])
    );
    assert!(
        find_bytes(&out[resize_at..], b"\x1b[5;15r").is_some(),
        "the status bar never re-asserted the workload's region after the resize; \
         captured after the resize:\n{}",
        escape(&out[resize_at..])
    );

    assert_host_matches_worker(
        &client,
        &harness,
        &id,
        |bytes| host_terminal_resized(bytes, resize_at, (40, 80), (24, 80)),
        23,
        80,
        "RLINE-60",
        "after a terminal resize landed mid-frame while a 5;15 sub-range was in force",
    );

    // ...and the screens that agreed were not two blank ones.
    let worker = worker_screen(&harness, &id, 23, 80);
    assert!(
        worker.rows.iter().any(|row| row.contains("RLINE-60")),
        "the worker's screen never showed the workload's last line:\n{}",
        worker.render()
    );

    client.detach();
    harness.run_ok(&["kill", &id, "--signal", "KILL"], Duration::from_secs(5));
}
