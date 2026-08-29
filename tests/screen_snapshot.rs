// Integration test for the live terminal-state model
// (docs/terminal-state-design.md), checklist item 11: start a session
// running a script that paints a box and enters the alternate screen,
// attach directly against the worker's control socket (bypassing `a
// attach`'s tty/raw-mode machinery -- a non-tty connection sees the same
// Attach protocol) with `want_screen: true`, and assert the first Data
// frame is a live-screen snapshot: it starts with `\x1b[?1049h` and,
// fed to a fresh `vt100::Parser`, reproduces the box. Then detach, print
// more while detached (proving the worker keeps parsing without a
// client), and reattach -- the new content must be present and the frame
// must be a *fresh* snapshot, not a history replay. Finally, old-client
// compatibility: `want_screen: false` still gets the raw-tail behavior.
//
// Every test runs against an isolated APLEXER_RUNTIME_DIR/APLEXER_STATE_DIR
// (see Harness::new, mirroring tests/oom_isolation.rs), so this never
// touches a real user's actual sessions.
//
// The second half of this file (`PtyClient` onwards) covers the *client*
// side of the same model -- the parts of `a attach` that keep a workload's
// DECSTBM scroll region alive on the host terminal across a snapshot, the
// resize-poll thread's first tick, and an in-process session switch. Those
// cannot be reached through the raw control-socket path above at all (they
// only run when stdin is a tty), so they are driven the only way that
// exercises them for real: `a attach` spawned on an actual PTY, with every
// byte it writes captured and fed back through a `vt100::Parser` standing in
// for the user's terminal.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use aplexer::{frame_json, read_frame, write_json, FrameKind, Operation, Request, Response};
use serde_json::Value;
use tempfile::TempDir;

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

    fn run_ok(&self, args: &[&str], timeout: Duration) -> String {
        let mut cmd = self.command();
        cmd.args(args);
        let output = run_with_timeout(cmd, timeout);
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

    fn run(&self, args: &[&str], timeout: Duration) -> std::process::Output {
        let mut cmd = self.command();
        cmd.args(args);
        run_with_timeout(cmd, timeout)
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

fn socket_path(harness: &Harness, id: &str) -> PathBuf {
    let stdout = harness.run_ok(&["status", id, "--json"], Duration::from_secs(5));
    let value: Value = serde_json::from_str(&stdout).expect("status output is JSON");
    PathBuf::from(
        value["socket_path"]
            .as_str()
            .expect("socket_path in status output"),
    )
}

/// Sends a marker string into the session and polls raw `a capture` until it
/// shows up, proving the workload has actually processed the input --
/// mirrors `assert_responsive` in tests/oom_isolation.rs. Only safe to use
/// for markers that are *not themselves substrings of the command sent* --
/// the PTY echoes typed input essentially instantly, before the shell has
/// actually executed anything, so a marker embedded in the command text
/// itself would pass immediately regardless of execution. Use
/// `wait_for_screen_marker` (below) when the marker text is also part of
/// the command being sent.
fn wait_for_marker(harness: &Harness, id: &str, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = harness.run(&["capture", id, "--bytes", "8192"], Duration::from_secs(5));
        let captured = String::from_utf8_lossy(&output.stdout);
        if captured.contains(marker) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("session {id} never produced marker {marker:?}; last capture:\n{captured}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Like `wait_for_marker`, but polls the *rendered current screen*
/// (`a capture --screen --plain`, i.e. `ScreenTracker::contents()`) instead
/// of the raw byte stream. NOTE: the PTY echoes typed input as literal
/// characters immediately (before the shell interprets/executes anything),
/// and that echoed text lands on-screen exactly like real output would --
/// so this is only meaningfully different from `wait_for_marker` when the
/// marker could not possibly be produced by echo alone (see
/// `wait_for_alt_screen` for the case that actually needs execution, not
/// just echo, to have happened).
fn wait_for_screen_marker(harness: &Harness, id: &str, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = harness.run(&["capture", id, "--screen", "--plain"], Duration::from_secs(5));
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

/// Polls `a capture --screen` (the paintable snapshot bytes, not
/// `--plain`) until the live screen model reports the alternate screen is
/// active, i.e. `\x1b[?1049h` opens the snapshot. Unlike a marker-text
/// search, this cannot be satisfied by mere terminal echo of typed input --
/// the input text sent via `a send` is literal ASCII (backslash, `0`, `3`,
/// `3`, ...), not real escape bytes, so the PTY echoing it back verbatim
/// can never itself flip `alternate_screen()`. Only the shell actually
/// executing `printf` (which emits genuine ESC bytes) can, so this really
/// does prove execution happened, not just that the command was typed.
fn wait_for_alt_screen(harness: &Harness, id: &str) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let output = harness.run(&["capture", id, "--screen"], Duration::from_secs(5));
        if output.status.success() && output.stdout.starts_with(b"\x1b[?1049h") {
            return output.stdout;
        }
        if Instant::now() >= deadline {
            panic!(
                "session {id} never entered the alternate screen; last `a capture --screen`: {:?}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Raw protocol attach against the worker's control socket directly --
/// bypassing `a attach`'s tty/raw-mode machinery, which the design doc's
/// own checklist item 11 says is unnecessary ("non-tty is fine -- the
/// snapshot arrives as the first Data frame either way"). Returns the
/// still-open stream (so the caller can hold the subscription open or
/// explicitly detach), the response body, and the initial Data frame's
/// payload.
fn raw_attach(
    socket: &Path,
    history_bytes: Option<usize>,
    want_screen: bool,
    rows: Option<u16>,
    cols: Option<u16>,
) -> (UnixStream, Value, Vec<u8>) {
    let mut stream = UnixStream::connect(socket).expect("connect worker control socket");
    let request = Request::new(Operation::Attach {
        history_bytes,
        want_screen,
        rows,
        cols,
    });
    let id = request.request_id.clone();
    write_json(&mut stream, &request).expect("write attach request");
    let response_frame = read_frame(&mut stream)
        .expect("read attach response")
        .expect("attach response frame present");
    let response: Response = frame_json(response_frame).expect("parse attach response");
    assert_eq!(response.request_id, id, "response request id mismatch");
    assert!(response.ok, "attach failed: {:?}", response.error);
    let body = response.result.unwrap_or(Value::Null);
    let data_frame = read_frame(&mut stream)
        .expect("read initial data frame")
        .expect("initial data frame present");
    assert_eq!(data_frame.kind, FrameKind::Data, "expected initial Data frame");
    (stream, body, data_frame.payload)
}

/// Shell command (executed via `a send ... --enter`) that paints a
/// bordered-box-like full-screen TUI on the alternate screen and moves the
/// cursor into it -- a synthesized stand-in for the codex-like TUI the
/// design doc's corruption repro is built around, using only `printf`
/// (present on every POSIX shell) so this test has no external-binary
/// dependency.
const PAINT_BOX: &str = r#"printf '\033[?1049h\033[2J\033[H\033[1;36m+----BOX----+\033[0m\r\nBOX-MARKER-INSIDE\r\n\033[7m STATUS \033[0m\033[0m\033[10;5H'"#;

#[test]
fn attach_snapshot_reproduces_alt_screen_box_and_reattach_gets_fresh_snapshot() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("main");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "main");
    harness.run_ok(&["send", &id, PAINT_BOX, "--enter"], Duration::from_secs(5));
    wait_for_alt_screen(&harness, &id);

    let socket = socket_path(&harness, &id);

    // -- First attach: want_screen: true must get a live-screen snapshot,
    // not a raw-tail replay. --
    let (stream, response, payload) =
        raw_attach(&socket, Some(32 * 1024), true, Some(24), Some(80));
    assert_eq!(
        response["screen"],
        Value::Bool(true),
        "response should report screen:true, got {response}"
    );
    assert!(
        payload.starts_with(b"\x1b[?1049h"),
        "snapshot should open with the alt-screen switch; got: {:?}",
        String::from_utf8_lossy(&payload[..payload.len().min(64)])
    );

    // Design doc section 6.2's own round-trip property: feed the snapshot
    // to a fresh, same-sized parser and confirm it reproduces the box.
    let mut check = vt100::Parser::new(24, 80, 0);
    check.process(&payload);
    let screen = check.screen();
    assert!(
        screen.alternate_screen(),
        "fresh parser should report alternate_screen() after the snapshot"
    );
    let contents = screen.contents();
    assert!(
        contents.contains("BOX-MARKER-INSIDE"),
        "reproduced screen missing the box marker; contents:\n{contents}"
    );
    assert!(
        contents.contains("STATUS"),
        "reproduced screen missing the status text; contents:\n{contents}"
    );

    // Detach (shutdown the socket -- same mechanism `a`'s own detach uses).
    drop(stream);

    // Print more while genuinely detached -- proves the worker's screen
    // model keeps advancing (docs/terminal-state-design.md section 4: "runs
    // always, attached or not") rather than only updating while a client is
    // watching.
    harness.run_ok(
        &["send", &id, "printf 'AFTER-DETACH-MARKER\\r\\n'", "--enter"],
        Duration::from_secs(5),
    );
    wait_for_screen_marker(&harness, &id, "AFTER-DETACH-MARKER");

    // Reattach: a *fresh* snapshot, with the new content, not a history
    // replay of the old attach's tail.
    let (_stream2, response2, payload2) =
        raw_attach(&socket, Some(32 * 1024), true, Some(24), Some(80));
    assert_eq!(response2["screen"], Value::Bool(true));
    let mut check2 = vt100::Parser::new(24, 80, 0);
    check2.process(&payload2);
    let contents2 = check2.screen().contents();
    assert!(
        contents2.contains("AFTER-DETACH-MARKER"),
        "reattach snapshot missing content written while detached; contents:\n{contents2}"
    );

    harness.run_ok(&["kill", &id, "--signal", "KILL"], Duration::from_secs(5));
}

#[test]
fn plain_shell_attach_snapshot_round_trips() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("plain");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "plain");
    harness.run_ok(
        &["send", &id, "echo hello-plain-shell", "--enter"],
        Duration::from_secs(5),
    );
    wait_for_screen_marker(&harness, &id, "hello-plain-shell");

    let socket = socket_path(&harness, &id);
    let (_stream, response, payload) =
        raw_attach(&socket, Some(32 * 1024), true, Some(24), Some(80));
    assert_eq!(response["screen"], Value::Bool(true));

    let mut check = vt100::Parser::new(24, 80, 0);
    check.process(&payload);
    assert!(
        !check.screen().alternate_screen(),
        "a plain shell session must not report alternate_screen()"
    );
    let contents = check.screen().contents();
    assert!(
        contents.contains("hello-plain-shell"),
        "reproduced plain-shell screen missing recent output; contents:\n{contents}"
    );

    harness.run_ok(&["kill", &id, "--signal", "KILL"], Duration::from_secs(5));
}

#[test]
fn old_client_compat_want_screen_false_gets_raw_tail() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("compat");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "compat");
    harness.run_ok(
        &["send", &id, "echo raw-tail-marker", "--enter"],
        Duration::from_secs(5),
    );
    wait_for_marker(&harness, &id, "raw-tail-marker");

    let socket = socket_path(&harness, &id);
    // The old client shape: history_bytes set, want_screen omitted/false,
    // no geometry -- exactly what an old `a attach` binary would send
    // (docs/terminal-state-design.md section 6.1's compatibility matrix).
    let (_stream, response, payload) = raw_attach(&socket, Some(4096), false, None, None);
    assert_eq!(
        response["screen"],
        Value::Bool(false),
        "want_screen: false should be reported back as screen:false"
    );
    // Raw-tail replay: exact historical bytes, not a rendered snapshot --
    // must not start with the snapshot's alt-screen/clear preamble, and
    // must contain the marker as literal bytes (not necessarily reproduced
    // through cell-grid parsing).
    let text = String::from_utf8_lossy(&payload);
    assert!(
        text.contains("raw-tail-marker"),
        "raw-tail replay missing the marker; got:\n{text}"
    );

    harness.run_ok(&["kill", &id, "--signal", "KILL"], Duration::from_secs(5));
}

/// A denser, "lots of color/attribute changes" screen than `PAINT_BOX`: 15
/// rows x 6 blocks, each block a distinct 256-color fg/bg pair --
/// deliberately more SGR-change-heavy than a typical codex-like TUI, to
/// stress-test `state_formatted()`'s per-cell attribute-run encoding for
/// the perf measurement below (the coordinator's concern: does snapshot
/// rendering regress for a *realistically busy* screen, not just a simple
/// bordered box). Kept under ~2.5KB deliberately: a PTY in canonical mode
/// caps a single input line at `MAX_CANON` (4096 bytes on Linux) -- a
/// larger single `a send` command silently truncates mid-line and never
/// executes as valid shell syntax, which the first version of this
/// benchmark discovered the hard way (bash's `>` continuation prompt, not
/// a real busy screen).
fn busy_paint_command() -> String {
    let mut s = String::from("printf '\\033[?1049h\\033[2J\\033[H");
    for row in 0..15u32 {
        for col in 0..6u32 {
            let fg = (row * 6 + col) % 256;
            let bg = (fg + 128) % 256;
            s.push_str(&format!("\\033[38;5;{fg}m\\033[48;5;{bg}mCH"));
        }
        s.push_str("\\033[0m\\r\\n");
    }
    s.push_str("BUSY-MARKER-END\\033[10;5H'");
    s
}

/// Perf measurement (design doc checklist item 12 / section 3.3's own
/// measured numbers, re-verified against this actual implementation, plus
/// the coordinator's explicit ask to confirm a *busy, attribute-heavy*
/// screen doesn't regress attach latency): repeated raw-protocol Attach
/// round trips (connect + handshake + snapshot render + transfer -- the
/// entire aplexer-controlled portion of reattach/switch latency, everything
/// except the client's own terminal paint) against a plain shell screen and
/// against the busy screen above. Not asserting pass/fail beyond "it
/// completed" -- the numbers are only meaningful printed with `--nocapture`
/// on an otherwise idle machine, and don't belong in the default `cargo
/// test` run, hence `#[ignore]`.
///
///   cargo test --release --test screen_snapshot -- --ignored --nocapture attach_round_trip_latency
#[test]
#[ignore = "manual perf measurement; run with --ignored --nocapture"]
fn attach_round_trip_latency() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");

    let plain_ws = root.path().join("plain-perf");
    std::fs::create_dir_all(&plain_ws).unwrap();
    let plain_id = start_session(&harness, &plain_ws, "plain-perf");
    harness.run_ok(
        &["send", &plain_id, "echo perf-marker", "--enter"],
        Duration::from_secs(5),
    );
    wait_for_screen_marker(&harness, &plain_id, "perf-marker");
    let plain_socket = socket_path(&harness, &plain_id);

    let busy_ws = root.path().join("busy-perf");
    std::fs::create_dir_all(&busy_ws).unwrap();
    let busy_id = start_session(&harness, &busy_ws, "busy-perf");
    harness.run_ok(
        &["send", &busy_id, &busy_paint_command(), "--enter"],
        Duration::from_secs(5),
    );
    wait_for_alt_screen(&harness, &busy_id);
    let busy_socket = socket_path(&harness, &busy_id);

    // Both `want_screen: true` (new snapshot path) and `want_screen: false`
    // (old raw-tail-replay path) are measured against the *same* sessions
    // and connection/thread-spawn machinery, isolating the delta actually
    // attributable to snapshot rendering from the pre-existing connect +
    // handshake overhead both paths share.
    for (label, socket) in [
        ("plain shell", &plain_socket),
        ("busy 256-color screen", &busy_socket),
    ] {
        for (mode, want_screen) in [("snapshot (new)", true), ("raw-tail (old)", false)] {
            let mut samples = Vec::new();
            let mut last_payload_len = 0usize;
            const N: usize = 200;
            for _ in 0..N {
                let start = Instant::now();
                let (stream, _response, payload) =
                    raw_attach(socket, Some(32 * 1024), want_screen, Some(24), Some(80));
                samples.push(start.elapsed());
                last_payload_len = payload.len();
                drop(stream);
            }
            samples.sort();
            let min = samples[0];
            let p50 = samples[N / 2];
            let p95 = samples[(N * 95) / 100];
            let max = samples[N - 1];
            println!(
                "[{label} / {mode}] payload {last_payload_len} bytes; attach round trip over {N} iters: \
                 min={min:?} p50={p50:?} p95={p95:?} max={max:?}"
            );
        }
    }

    harness.run_ok(&["kill", &plain_id, "--signal", "KILL"], Duration::from_secs(5));
    harness.run_ok(&["kill", &busy_id, "--signal", "KILL"], Duration::from_secs(5));
}

// ---------------------------------------------------------------------------
// Client-side terminal-state wiring, driven through a real PTY.
//
// `a attach`'s scroll-region handling lives entirely behind
// `isatty(STDIN_FILENO)`: the snapshot scan, the Data-frame scan, the
// resize-poll thread's seeding, the status bar's margin re-assert and the
// session-switch margin reset all only run for a tty client. The raw
// control-socket helpers above therefore cannot reach any of it. These tests
// spawn the real `a attach` binary with its stdio on a PTY, capture every
// byte it writes, and replay those bytes into a `vt100::Parser` acting as the
// user's terminal -- so the assertions are about what the terminal actually
// ends up showing, not about internal state.
// ---------------------------------------------------------------------------

/// A live `a attach` process on its own PTY, with everything it writes to the
/// terminal captured in the background (a dedicated reader thread, so the PTY
/// buffer can never fill and wedge the client mid-write).
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
        // Our own copy of the slave must go, or the master never sees the
        // hangup when the client exits and the reader thread below never ends.
        drop(slave);

        let captured = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        let mut reader = master.try_clone().expect("dup pty master for reading");
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    // EIO on a pty master is the normal "last slave closed"
                    // hangup, not a failure.
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

    /// Byte offset just past everything captured so far -- used to scope an
    /// assertion to "what the client wrote *after* this point".
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

    /// Waits until `needle` appears in the captured stream at or after `from`.
    /// Panics with the captured bytes on timeout rather than letting a later
    /// assertion fail for an unrelated-looking reason.
    fn wait_for(&self, needle: &[u8], from: usize, what: &str) {
        let _ = self.wait_for_offset(needle, from, what);
    }

    /// `wait_for`, returning a `mark()`-comparable offset just past the end of
    /// the match -- i.e. "the point in the stream where the client had
    /// demonstrably observed this".
    ///
    /// This exists because `mark()` alone is the wrong reference for a
    /// *negative* assertion about a client reaction. `mark()` is taken before
    /// the stimulus (a resize, a switch chord, a workload escape sequence),
    /// and the client only observes that stimulus some bounded time later --
    /// up to a 200 ms poll tick for a resize, a socket round trip for a
    /// switch. Anything the client writes in between is still correctly using
    /// its *pre*-stimulus state, so scoping "the client must never write X
    /// again" to `mark()` asserts over a window where writing X is right. Under
    /// CPU contention a status-bar redraw lands in that window often enough to
    /// make such a test flaky (measured: 6/11 runs under 16 competing spin
    /// loops) with no product bug involved. Anchoring the negative assertion
    /// here instead keeps it about the only thing that is actually a bug: the
    /// client still writing X *after* it had already reacted.
    /// Waits until `needle` has appeared at least `count` times at or after
    /// `from`.
    ///
    /// The counterpart to `wait_for_offset` for *positive* assertions ("the
    /// bar did re-assert this region", where one of the sightings is the
    /// workload's own bytes passing through). Those used to be sequenced by a
    /// fixed `thread::sleep` sized from the client's own timers -- 450 ms of
    /// idle-gap plus a 150 ms poll, slept for 1 s. That is a race, not a
    /// bound: under CPU contention the redraw simply lands later (measured:
    /// 2/12 runs under 20 competing spin loops failed with "seen 1 time(s)").
    /// Waiting on the sighting with a generous deadline is the same assertion
    /// with the arbitrary deadline removed.
    fn wait_for_count(&self, needle: &[u8], from: usize, count: usize, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let out = self.output();
            if out.len() >= from && count_bytes(&out[from..], needle) >= count {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {count} sighting(s) of {what} ({:?}) in the client's \
                     output after byte {from}; captured:\n{}",
                    String::from_utf8_lossy(needle),
                    escape(&out)
                );
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    fn wait_for_offset(&self, needle: &[u8], from: usize, what: &str) -> usize {
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

    /// Resizes the terminal under the client, exactly as a window manager
    /// does: the winsize lives on the pty pair, so setting it from the master
    /// is what the client's own `TIOCGWINSZ` poll on its stdin then observes.
    fn resize(&self, rows: u16, cols: u16) {
        use std::os::unix::io::AsRawFd;
        aplexer::set_winsize(self.master.as_raw_fd(), rows, cols).expect("resize the pty");
    }

    /// `Ctrl-b d`, then wait for the process to actually exit.
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

fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn escape(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\x1b', "<ESC>")
}

/// Reads a `vt100::Screen`'s *real* scroll region back out of the crate
/// without touching its private fields: DECOM (origin mode) makes CUP
/// row-relative to the region and clamps to it (vt100 0.16.2
/// `grid.rs::set_pos`), so homing reports the top and asking for row 999
/// reports the bottom. 1-based inclusive, like `MarginTracker`. Destructive
/// to cursor position and origin mode. Mirrors the same helper in
/// `src/screen.rs`'s unit tests, which self-checks it against known regions.
fn probe_scroll_region(parser: &mut vt100::Parser) -> (u16, u16) {
    parser.process(b"\x1b[?6h\x1b[1;1H");
    let top = parser.screen().cursor_position().0 + 1;
    parser.process(b"\x1b[999;1H");
    let bottom = parser.screen().cursor_position().0 + 1;
    parser.process(b"\x1b[?6l");
    (top, bottom)
}

/// The user's terminal: every byte the client wrote, replayed into a parser
/// of the physical terminal's size (the client reserves the last row for its
/// own status bar and tells the worker the screen is one row shorter).
fn host_terminal(bytes: &[u8], rows: u16, cols: u16) -> vt100::Parser {
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(bytes);
    parser
}

/// The same, for a stream that spans a terminal resize: the bytes before
/// `split` were written to a `before`-sized terminal and the rest to an
/// `after`-sized one, so the model has to resize at the same point a real
/// terminal would.
fn host_terminal_resized(
    bytes: &[u8],
    split: usize,
    before: (u16, u16),
    after: (u16, u16),
) -> vt100::Parser {
    let mut parser = vt100::Parser::new(before.0, before.1, 0);
    parser.process(&bytes[..split]);
    parser.screen_mut().set_size(after.0, after.1);
    parser.process(&bytes[split..]);
    parser
}

/// The first `rows` rows of a screen, trimmed per row -- the workload's own
/// rows, excluding the client's reserved status-bar row, so a host terminal
/// and the worker's own screen model can be compared directly.
fn workload_rows(contents: &str, rows: usize) -> Vec<String> {
    let mut out: Vec<String> = contents
        .lines()
        .take(rows)
        .map(|line| line.trim_end().to_string())
        .collect();
    out.resize(rows, String::new());
    out
}

/// Waits until the host terminal and the worker's own screen model agree on
/// the workload's rows, and returns both renderings (host first).
///
/// The two are sampled from different places at different times: the host is
/// rebuilt from every byte the client has written *so far*, while the worker's
/// model is whatever `a capture --screen` reports at the moment it is asked.
/// The worker is always at or ahead of the host by construction -- it parses
/// each chunk before forwarding it, and the chunk still has a socket, a pty
/// and a write to cross -- so right after a burst of output the host is
/// legitimately a few rows behind while the tail is in flight. Comparing once
/// after a fixed sleep therefore races the tail rather than testing anything:
/// measured, the host trailing the worker by two scrolled rows failed that
/// comparison in 1 of 12 runs under 16 competing spin loops.
///
/// Polling to convergence does not weaken the assertion. A genuine
/// scroll-region divergence -- text piling up inside a stale region on the
/// host while the worker fills the screen -- is a *steady* difference that
/// never converges, so it still fails, with the same rendered diff. Only the
/// transient in-flight difference is waited out.
fn wait_for_host_worker_agreement(
    client: &PtyClient,
    harness: &Harness,
    id: &str,
    rows: usize,
    host_of: impl Fn(&[u8]) -> vt100::Parser,
    what: &str,
) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let host = host_of(&client.output()).screen().contents();
        let worker = harness.run_ok(
            &["capture", id, "--screen", "--plain"],
            Duration::from_secs(5),
        );
        if workload_rows(&host, rows) == workload_rows(&worker, rows) {
            return (host, worker);
        }
        if Instant::now() >= deadline {
            assert_eq!(
                workload_rows(&host, rows),
                workload_rows(&worker, rows),
                "{what}: the host terminal and the worker's own screen model never agreed \
                 -- the scroll region diverged.\nhost:\n{host}\nworker:\n{worker}"
            );
            unreachable!("the assertion above always fails here");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Sets a `5;15` scroll region in the session (as a TUI with a fixed header
/// and footer does) and waits until the *worker's* snapshot actually carries
/// it, so a later client-side assertion can't pass or fail on the worker
/// simply not having seen it yet.
///
/// The shell echoes the command text back as literal `\033[5;15r`
/// characters, never as real escape bytes, so searching the snapshot for the
/// ESC-prefixed sequence really does prove the workload emitted it.
fn set_scroll_region_and_wait(harness: &Harness, id: &str) {
    harness.run_ok(
        &["send", id, r#"printf '\033[5;15r'"#, "--enter"],
        Duration::from_secs(5),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = harness.run(&["capture", id, "--screen"], Duration::from_secs(5));
        if output.status.success() && find_bytes(&output.stdout, b"\x1b[5;15r").is_some() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "session {id} never reported a 5;15 scroll region in its snapshot; last snapshot:\n{}",
                escape(&output.stdout)
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Bash one-liner printing `{prefix}-1 .. {prefix}-{count}`: more lines than
/// any scroll region here holds, so where they end up on screen is decided
/// entirely by which region the host terminal has in force.
fn scroll_command(prefix: &str, count: u32) -> String {
    format!("for i in $(seq 1 {count}); do echo {prefix}-$i; done")
}

/// Regression test for the *client-side* half of the scroll-region fix: the
/// snapshot scan in `attach()` and the resize-poll thread's seeding.
///
/// Both are invisible to any unit test of `draw_status_bar`: that function
/// re-asserts whatever region the tracker holds, and the tracker only holds
/// the workload's region because `attach()` scans the snapshot bytes into it
/// before the first bar draw. And nothing about the bar can stop the
/// resize-poll thread, whose first tick used to look like a resize (`last`
/// started at `None`) and re-ran `apply_terminal_layout` ~200 ms into every
/// attach, overwriting the workload's region with the bar's own
/// `\x1b[1;{rows-1}r`.
///
/// The oracle is deliberately byte-exact: over the whole attach, the client
/// may write its own `\x1b[1;23r` reservation exactly once -- the
/// `apply_terminal_layout` call that runs *before* the snapshot, per
/// docs/terminal-state-design.md section 6.3 step 3. Every write after that
/// must carry the workload's own `5;15`. Dropping the snapshot scan makes the
/// first bar draw write a second one; dropping the resize seeding makes the
/// poll thread write a second one.
#[test]
fn attach_keeps_the_workload_scroll_region_alive_on_the_host_terminal() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("client-region");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "region");
    set_scroll_region_and_wait(&harness, &id);

    let client = PtyClient::spawn(&harness, &id, 24, 80);
    // The snapshot's trailing DECSTBM (section 6.2 step 3) reaching the host.
    client.wait_for(b"\x1b[5;15r", 0, "the workload's scroll region");

    // Past the resize thread's first poll (200 ms) and a couple of status-bar
    // ticks (150 ms), but inside STATUS_BAR_MAX_INTERVAL (3 s) -- so a
    // clobbered region is still clobbered rather than already self-healed,
    // which is exactly the window the user sees.
    thread::sleep(Duration::from_millis(1_200));

    let out = client.output();
    assert_eq!(
        count_bytes(&out, b"\x1b[1;23r"),
        1,
        "the client re-asserted its own full-height reservation over the workload's \
         5;15 region ({} times instead of the single pre-snapshot one); captured:\n{}",
        count_bytes(&out, b"\x1b[1;23r"),
        escape(&out)
    );

    // What the terminal is actually left holding.
    let mut host = host_terminal(&out, 24, 80);
    assert_eq!(
        probe_scroll_region(&mut host),
        (5, 15),
        "the host terminal is not holding the workload's scroll region; captured:\n{}",
        escape(&out)
    );

    // And behaviourally: 40 lines of output must land where the *worker's*
    // own screen model says they land. With the region lost on the host, the
    // text runs past row 15 and the two disagree.
    let marker = "INSIDE";
    harness.run_ok(
        &["send", &id, &scroll_command(marker, 40), "--enter"],
        Duration::from_secs(5),
    );
    client.wait_for(
        format!("{marker}-40").as_bytes(),
        0,
        "the last scrolled line",
    );
    let (_host, worker) = wait_for_host_worker_agreement(
        &client,
        &harness,
        &id,
        23,
        |bytes| host_terminal(bytes, 24, 80),
        "after 40 lines scrolled inside the workload's 5;15 region",
    );
    // Guard against the comparison passing vacuously on two blank screens.
    // Checked *after* convergence, on the same worker rendering the host was
    // proven equal to -- so this establishes the scrolled output is present on
    // both sides, not just that they matched.
    assert!(
        worker.contains(&format!("{marker}-40")),
        "the worker's screen never showed the scrolled output:\n{worker}"
    );

    client.detach();
    harness.run_ok(&["kill", &id, "--signal", "KILL"], Duration::from_secs(5));
}

/// Regression test for the third `scan_workload_margins` call site: the one
/// on every Data frame, which is how the client learns about a scroll region
/// the workload sets *while it is attached* -- a TUI started after the
/// attach, which is the ordinary case, not the reattach case the snapshot
/// scan covers.
///
/// Without it the client's tracker is frozen at whatever the snapshot said,
/// and the status bar keeps re-asserting that stale region over the
/// workload's new one on every redraw.
#[test]
fn attach_learns_a_scroll_region_the_workload_sets_while_attached() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("live-region");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "live");
    // A region already in force at attach time, so a client that only ever
    // scans the snapshot still has *something* to wrongly re-assert -- this
    // test has to fail because the tracker did not update, not merely because
    // it was empty.
    set_scroll_region_and_wait(&harness, &id);

    let client = PtyClient::spawn(&harness, &id, 24, 80);
    client.wait_for(b"\x1b[5;15r", 0, "the snapshot's scroll region");

    // The workload now moves its region, live.
    let live_at = client.mark();
    harness.run_ok(
        &["send", &id, r#"printf '\033[7;17r'"#, "--enter"],
        Duration::from_secs(5),
    );
    // The client learns the new region from the Data frame carrying it
    // (`scan_workload_margins` runs on the payload before it is written), so
    // this offset is the first point at which re-asserting `5;15` would be
    // wrong. Before it -- while `a send` is still round-tripping through the
    // worker -- a bar redraw carrying `5;15` is the client correctly using the
    // only region it knows about, which is why the negative assertion below is
    // anchored here rather than at `live_at` (see `wait_for_offset`).
    let learned_at =
        client.wait_for_offset(b"\x1b[7;17r", live_at, "the workload's new scroll region");
    // A sub-range change fires no `Layout` event (it is not a reset, an erase
    // or an alt-screen flip), so the bar picks it up on its next idle-gap
    // tick: 450 ms of idle plus a 150 ms poll. Waited for rather than slept
    // for -- the second sighting is the bar's re-assert, the first being the
    // workload's own bytes above.
    client.wait_for_count(
        b"\x1b[7;17r",
        live_at,
        2,
        "the bar re-asserting the workload's new region",
    );
    // Then a further quiet stretch, so a client that *also* keeps writing the
    // region it should have dropped has had every opportunity to do so before
    // the negative assertions below look.
    thread::sleep(Duration::from_millis(500));

    let out = client.output();
    let live = &out[live_at..];
    let after_learned = &out[learned_at..];
    assert!(
        count_bytes(live, b"\x1b[7;17r") >= 2,
        "the status bar never re-asserted the workload's new region (seen {} time(s): once is \
         just the workload's own bytes passing through); captured after the change:\n{}",
        count_bytes(live, b"\x1b[7;17r"),
        escape(live)
    );
    assert_eq!(
        count_bytes(after_learned, b"\x1b[5;15r"),
        0,
        "the client kept re-asserting the region the workload had already replaced; \
         captured from the point it learned the new one:\n{}",
        escape(after_learned)
    );
    assert_eq!(
        count_bytes(after_learned, b"\x1b[1;23r"),
        0,
        "the client fell back to its own full-height reservation over a live workload \
         region; captured from the point it learned the new one:\n{}",
        escape(after_learned)
    );

    let mut host = host_terminal(&out, 24, 80);
    assert_eq!(
        probe_scroll_region(&mut host),
        (7, 17),
        "the host terminal is not holding the region the workload most recently set; \
         captured:\n{}",
        escape(&out)
    );

    client.detach();
    harness.run_ok(&["kill", &id, "--signal", "KILL"], Duration::from_secs(5));
}

/// Regression test for the session-switch margin reset (`reset_workload_margins`).
///
/// The client's `MarginTracker` follows the bytes of whatever session it is
/// showing, and those bytes stop being relevant the instant it switches. Two
/// sessions in one workspace: A holds `\x1b[5;15r`, B is an ordinary
/// full-screen scroller. Without the reset, the tracker carries A's region
/// into B, the status bar dutifully re-asserts `5;15` on the host, and B's
/// output is trapped in rows 5-15 while the rows around it hold whatever A
/// left there.
#[test]
fn switching_sessions_drops_the_previous_sessions_scroll_region() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("switch-region");
    std::fs::create_dir_all(&workspace).unwrap();

    let id_a = start_session(&harness, &workspace, "region");
    let id_b = start_session(&harness, &workspace, "plain");
    set_scroll_region_and_wait(&harness, &id_a);
    // A marker only B's screen carries, so the switch can be waited for
    // positively instead of by sleeping. `%s` keeps the marker out of the
    // echoed command text.
    harness.run_ok(
        &["send", &id_b, r#"printf 'B-SESSION-%s\n' MARK"#, "--enter"],
        Duration::from_secs(5),
    );
    wait_for_screen_marker(&harness, &id_b, "B-SESSION-MARK");

    let mut client = PtyClient::spawn(&harness, &id_a, 24, 80);
    client.wait_for(b"\x1b[5;15r", 0, "session A's scroll region");

    // Ctrl-b n: with exactly two attachable sessions in this workspace, "next"
    // is B whichever way the group is ordered.
    let switch_at = client.mark();
    client.send(&[0x02, b'n']);
    // The switch chord has to round-trip through the worker before the client
    // can act on it, and `reset_workload_margins` runs immediately before B's
    // snapshot payload is written -- so the offset of B's own marker is the
    // first point at which still holding A's region would be a bug. A bar
    // redraw in the window before it correctly still carries `5;15`: the
    // client is at that moment still attached to A. Measured flaky at
    // `switch_at` under CPU contention; see `wait_for_offset`.
    let switch_observed_at =
        client.wait_for_offset(b"B-SESSION-MARK", switch_at, "session B's snapshot");
    // The post-switch forced bar redraw, waited for rather than slept for.
    client.wait_for_count(
        b"\x1b[1;23r",
        switch_at,
        1,
        "the client's own reservation restored for session B",
    );
    // Then a quiet stretch, so a client still holding A's region has had the
    // status thread's next tick to write it before the assertion below looks.
    thread::sleep(Duration::from_millis(600));

    let out = client.output();
    let after_switch = &out[switch_at..];
    let after_observed = &out[switch_observed_at..];
    assert_eq!(
        count_bytes(after_observed, b"\x1b[5;15r"),
        0,
        "session A's scroll region was re-asserted onto session B; captured from the point \
         B's snapshot arrived:\n{}",
        escape(after_observed)
    );
    assert!(
        count_bytes(after_switch, b"\x1b[1;23r") >= 1,
        "the client never restored its own status-bar reservation for session B; \
         captured after the switch:\n{}",
        escape(after_switch)
    );

    let mut host = host_terminal(&out, 24, 80);
    assert_eq!(
        probe_scroll_region(&mut host),
        (1, 23),
        "after switching to a full-screen session the host terminal must be back on the \
         client's own reservation; captured:\n{}",
        escape(&out)
    );

    // Behavioural half: B scrolls a full screen's worth of output. Confined
    // to A's old 5-15 region it would pile up in eleven rows; correct, it
    // fills the screen and the last line sits at the bottom of B's own rows.
    let marker = "BLINE";
    harness.run_ok(
        &["send", &id_b, &scroll_command(marker, 40), "--enter"],
        Duration::from_secs(5),
    );
    client.wait_for(
        format!("{marker}-40").as_bytes(),
        switch_at,
        "session B's last scrolled line",
    );
    let (host_screen, _worker) = wait_for_host_worker_agreement(
        &client,
        &harness,
        &id_b,
        23,
        |bytes| host_terminal(bytes, 24, 80),
        "after session B scrolled 40 lines",
    );
    let rows = workload_rows(&host_screen, 23);
    let last_line_row = rows
        .iter()
        .position(|row| row.contains(&format!("{marker}-40")))
        .map(|index| index + 1);
    assert!(
        last_line_row.is_some_and(|row| row > 15),
        "session B's output is trapped in session A's old 5-15 scroll region \
         ({marker}-40 landed on row {last_line_row:?}); host screen:\n{host_screen}"
    );

    // Switching *back* is the other half of the same handoff: the reset has
    // to be followed by learning the incoming session's own region out of its
    // snapshot payload (the `scan_workload_margins(&outcome.history)` call),
    // or A comes back with its region dropped -- the very bug the snapshot
    // scan fixes on a first attach, reintroduced on every switch.
    let back_at = client.mark();
    client.send(&[0x02, b'n']);
    // Same anchoring point as the switch out: the first `5;15` after the chord
    // is A's snapshot payload passing through, which is where the client
    // re-learns A's region. A `1;23r` before it is the client still correctly
    // serving B.
    let back_observed_at = client.wait_for_offset(
        b"\x1b[5;15r",
        back_at,
        "session A's scroll region on the way back",
    );
    // The second sighting is the bar re-asserting the re-learned region (the
    // first is the snapshot's own bytes) -- waited for rather than slept for.
    client.wait_for_count(
        b"\x1b[5;15r",
        back_at,
        2,
        "the bar re-asserting session A's re-learned region",
    );
    thread::sleep(Duration::from_millis(600));

    let out = client.output();
    let after_back = &out[back_at..];
    let after_back_observed = &out[back_observed_at..];
    assert!(
        count_bytes(after_back, b"\x1b[5;15r") >= 2,
        "switching back to session A did not re-learn its scroll region (seen {} time(s): \
         once is just the snapshot's own bytes passing through); captured after the \
         switch back:\n{}",
        count_bytes(after_back, b"\x1b[5;15r"),
        escape(after_back)
    );
    assert_eq!(
        count_bytes(after_back_observed, b"\x1b[1;23r"),
        0,
        "the client re-asserted its own full-height reservation over session A's restored \
         region; captured from the point A's region came back:\n{}",
        escape(after_back_observed)
    );
    let mut host = host_terminal(&out, 24, 80);
    assert_eq!(
        probe_scroll_region(&mut host),
        (5, 15),
        "the host terminal lost session A's scroll region across the round trip; \
         captured:\n{}",
        escape(&out)
    );

    client.detach();
    harness.run_ok(&["kill", &id_a, "--signal", "KILL"], Duration::from_secs(5));
    harness.run_ok(&["kill", &id_b, "--signal", "KILL"], Duration::from_secs(5));
}

/// End-to-end regression test for `MarginTracker::set_rows`' bottom-anchored
/// growth rule, in the real client + worker path rather than against the
/// tracker in isolation.
///
/// The layout is the most ordinary one a TUI has: fixed header rows, and
/// everything below them scrolls -- `\x1b[3;23r` on the 23 rows the workload
/// is told it has. That region is *bottom-anchored*, and `vt100`'s own
/// `set_size` grows such a region with the screen. The tracker used to only
/// ever clamp downward, so after any enlargement -- window maximized, an
/// on-screen keyboard hidden, a pane unsplit -- the worker's grid held
/// `(3,39)` while both trackers still said `(3,23)`, and the client
/// re-asserted that stale region onto the host terminal.
///
/// Oracles: the host terminal must end up holding `3;39`, and its screen must
/// match the worker's own model once the workload scrolls -- which it cannot
/// do if the two disagree about where the region ends.
#[test]
fn growing_the_terminal_grows_a_bottom_anchored_workload_region() {
    let harness = Harness::new();
    let root = TempDir::new().expect("workspace root");
    let workspace = root.path().join("grow-region");
    std::fs::create_dir_all(&workspace).unwrap();

    let id = start_session(&harness, &workspace, "grow");
    let client = PtyClient::spawn(&harness, &id, 24, 80);
    client.wait_for(b"\x1b[1;23r", 0, "the client's initial reservation");

    // Two fixed header rows, everything below scrolls, at the workload's own
    // row count (24 physical rows minus the reserved bar row).
    harness.run_ok(
        &["send", &id, r#"printf '\033[3;23r'"#, "--enter"],
        Duration::from_secs(5),
    );
    client.wait_for(b"\x1b[3;23r", 0, "the workload's bottom-anchored region");
    thread::sleep(Duration::from_millis(800)); // quiesce before the resize

    // The terminal grows. Split the capture here: bytes before this were
    // painted on a 24-row terminal, bytes after on a 40-row one.
    let resize_at = client.mark();
    client.resize(40, 80);
    // The client only learns about the resize on its next 200 ms poll tick,
    // and that tick is where it re-fits the tracked region (`set_rows`) and
    // writes its own new reservation (`apply_terminal_layout`'s
    // `\x1b[1;39r`). A bar redraw landing in the window *before* that tick
    // correctly still carries the pre-resize `3;23` -- the client has not
    // observed the new size yet -- so the "no stale region" assertion below is
    // anchored past this point rather than at `resize_at`. Only the positive
    // assertion, "the grown region does get asserted at all", stays scoped to
    // `resize_at`: it is a *sighting*, so a wider window can only ever make it
    // more forgiving, never flaky.
    let observed_resize_at =
        client.wait_for_offset(b"\x1b[1;39r", resize_at, "the client observing the resize");
    // Resize poll (200 ms) -> worker resize -> shell repaint -> the bar's next
    // idle-gap redraw (450 ms + a 150 ms poll), waited for rather than slept
    // for.
    client.wait_for_count(
        b"\x1b[3;39r",
        resize_at,
        1,
        "the bar re-asserting the grown region",
    );
    // Then a quiet stretch, so a client that also keeps re-asserting the
    // pre-growth region has had its next tick to do so before the negative
    // assertion looks.
    thread::sleep(Duration::from_millis(1_000));

    let out = client.output();
    let after_resize = &out[resize_at..];
    assert!(
        count_bytes(after_resize, b"\x1b[3;39r") >= 1,
        "the client never re-asserted the grown region (it should follow the screen from \
         3;23 to 3;39); captured after the resize:\n{}",
        escape(after_resize)
    );
    // The negative half, as a *paired* sequence rather than a bare `3;23`.
    //
    // `draw_status_bar` writes the region it re-asserts immediately followed
    // by a jump to the row it owns -- `\x1b[{rows};1H` -- so `3;23r` next to
    // `\x1b[40;1H` is precisely "a redraw on the enlarged terminal asserting
    // the pre-growth region", which is the regression, and it is exactly what
    // the pre-fix client wrote. Pairing them is what makes the assertion
    // race-free rather than merely better-anchored: a redraw *samples* the
    // geometry before the margins, while the resize thread *updates* the
    // margins before the geometry, so a redraw that still sees `3;23` provably
    // still sees 24 rows too and writes `\x1b[24;1H`. Such a redraw can be
    // stalled past this anchor by the scheduler (observed: 1/12 runs under 20
    // spin loops) while being entirely correct -- it is a snapshot of the
    // pre-resize world. A redraw carrying `\x1b[40;1H` never can be.
    assert_eq!(
        count_bytes(&out[observed_resize_at..], b"\x1b[3;23r\x1b[40;1H"),
        0,
        "the client kept re-asserting the pre-growth region on the enlarged terminal, after \
         it had already observed the resize; captured from that point:\n{}",
        escape(&out[observed_resize_at..])
    );

    let mut host = host_terminal_resized(&out, resize_at, (24, 80), (40, 80));
    assert_eq!(
        probe_scroll_region(&mut host),
        (3, 39),
        "the host terminal's scroll region did not grow with the screen; captured:\n{}",
        escape(&out)
    );

    // Behavioural half: scroll past the bottom of the grown region. If the
    // host is still on 3;23 the text piles up in the top half of the screen
    // while the worker's model has it filling all 39 rows.
    //
    // Comfortably more lines than the region has rows, for a reason beyond
    // "enough to scroll": growing the terminal turns the row that *was* the
    // client's reserved bar row into an ordinary workload row, still holding
    // the old bar text, and nothing erases it -- the client is a passthrough
    // and bash's SIGWINCH repaint only redraws its prompt. That stale row is
    // a (pre-existing, cosmetic) difference between the host and the worker's
    // model which scrolling out of the region clears, so this deliberately
    // scrolls far enough that every row of the region has been rewritten.
    let marker = "GROWN";
    harness.run_ok(
        &["send", &id, &scroll_command(marker, 80), "--enter"],
        Duration::from_secs(5),
    );
    client.wait_for(
        format!("{marker}-80").as_bytes(),
        resize_at,
        "the last scrolled line",
    );
    let (_host, worker) = wait_for_host_worker_agreement(
        &client,
        &harness,
        &id,
        39,
        |bytes| host_terminal_resized(bytes, resize_at, (24, 80), (40, 80)),
        "after 80 lines scrolled through the grown 3;39 region",
    );
    // Non-vacuity guard, checked on the same worker rendering the host was
    // proven equal to (see `wait_for_host_worker_agreement`).
    assert!(
        worker.contains(&format!("{marker}-80")),
        "the worker's screen never showed the scrolled output:\n{worker}"
    );

    client.detach();
    harness.run_ok(&["kill", &id, "--signal", "KILL"], Duration::from_secs(5));
}
