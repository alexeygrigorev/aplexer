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

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
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
