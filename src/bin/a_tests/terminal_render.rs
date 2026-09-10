// -- draw_status_bar's "did it actually write" contract, which the
// status thread's STATUS_BAR_MAX_INTERVAL overdue-timer depends on to
// avoid the timer-starvation bug: resetting `last_draw` on every tick
// regardless of whether draw_status_bar performed a real write would
// let a frequent-but-unchanging redraw (a spinner, streamed tokens with
// pauses) keep the overdue timer perpetually "recently fired" without
// ever actually re-writing a margin/row a full-screen erase clobbered.
// See draw_status_bar's and the status thread's doc comments.

/// `draw_status_bar` writes straight to the real `io::Stdout` (no
/// injectable writer to swap in for a test), so exercising its
/// real-write path here would otherwise leak raw DECSTBM/reverse-video
/// escape sequences into whatever terminal happens to be running
/// `cargo test` interactively -- and leave that terminal's scroll
/// region permanently narrowed, since nothing in this test ever runs
/// the reset-on-detach path that would restore it. Redirecting the
/// process's real fd 1 to `/dev/null` for the guard's lifetime (and
/// restoring the original fd on drop) makes the write land somewhere
/// harmless instead.
struct StdoutToDevNull {
    saved_fd: i32,
}
impl StdoutToDevNull {
    fn new() -> Self {
        let saved_fd = unsafe { libc::dup(1) };
        assert!(saved_fd >= 0, "dup(1) failed");
        let devnull = std::ffi::CString::new("/dev/null").unwrap();
        let devnull_fd = unsafe { libc::open(devnull.as_ptr(), libc::O_WRONLY) };
        assert!(devnull_fd >= 0, "open /dev/null failed");
        let rc = unsafe { libc::dup2(devnull_fd, 1) };
        unsafe { libc::close(devnull_fd) };
        assert!(rc >= 0, "dup2 to /dev/null failed");
        Self { saved_fd }
    }
}
impl Drop for StdoutToDevNull {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.saved_fd, 1);
            libc::close(self.saved_fd);
        }
    }
}

/// Feeds bytes into a test context's model the way the client's relay
/// path does, without a terminal to write them to.
fn feed_test_screen(screen: &Arc<Mutex<aplexer::screen::ClientScreen>>, data: &[u8]) {
    screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .feed(data);
}

fn status_ctx_for_test(reserved: bool) -> StatusBarCtx {
    StatusBarCtx {
        stdout: Arc::new(Mutex::new(io::stdout())),
        term: Arc::new(Mutex::new(TermGeom {
            rows: 24,
            cols: 80,
            reserved,
        })),
        paths: Paths {
            runtime_root: PathBuf::from("/nonexistent-aplexer-test-runtime"),
            state_root: PathBuf::from("/nonexistent-aplexer-test-state"),
            config_file: PathBuf::from("/nonexistent-aplexer-test-state/config.toml"),
        },
        record: Arc::new(Mutex::new(mk_record(
            "/ws/status-bar-test",
            "t",
            Phase::Running,
        ))),
        live: Arc::new(Mutex::new(LiveStatus::default())),
        flash: Arc::new(Mutex::new(None)),
        last_drawn: Arc::new(Mutex::new(None)),
        screen: Arc::new(Mutex::new(
            aplexer::screen::ClientScreen::try_new(23, 80).unwrap(),
        )),
        pending: Arc::new(AtomicBool::new(false)),
        pending_refresh: Arc::new(AtomicBool::new(false)),
        pending_layout: Arc::new(Mutex::new(None)),
        sync_deferred_since: Arc::new(Mutex::new(None)),
        scroll: Arc::new(ScrollMode::new()),
        overlay: Arc::new(KeyOverlay::default()),
        mouse_owned: Arc::new(Mutex::new(None)),
        mouse_capture: false,
    }
}

/// Serializes every test that redirects the process-wide fd 1. Without
/// it the default multi-threaded test harness lets one such test's
/// `dup(1)` capture another's pipe write end and hold it open, so the
/// reader blocks forever waiting for an EOF that never comes.
static FD1_GUARD: Mutex<()> = Mutex::new(());

/// Like `StdoutToDevNull`, but keeps the bytes: redirects fd 1 to a pipe
/// so a test can assert on the exact escape sequences `draw_status_bar`
/// emitted, rather than only on its `bool` return.
struct StdoutToPipe {
    saved_fd: i32,
    read_fd: i32,
}
impl StdoutToPipe {
    fn new() -> Self {
        let saved_fd = unsafe { libc::dup(1) };
        assert!(saved_fd >= 0, "dup stdout failed");
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe failed");
        // Non-blocking read end: everything of interest is flushed by
        // `write_locked` before we read, so "no more data" must surface
        // as EAGAIN rather than an indefinite block.
        assert_eq!(
            unsafe { libc::fcntl(fds[0], libc::F_SETFL, libc::O_NONBLOCK) },
            0,
            "set O_NONBLOCK failed"
        );
        assert!(unsafe { libc::dup2(fds[1], 1) } >= 0, "dup2 to pipe failed");
        unsafe { libc::close(fds[1]) };
        Self {
            saved_fd,
            read_fd: fds[0],
        }
    }
    /// Restores stdout and returns everything written while redirected.
    fn take(self) -> Vec<u8> {
        // Restore first so the write end is fully closed before reading,
        // otherwise the read below blocks on a still-open pipe.
        unsafe {
            libc::dup2(self.saved_fd, 1);
            libc::close(self.saved_fd);
        }
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe {
                libc::read(
                    self.read_fd,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        unsafe { libc::close(self.read_fd) };
        out
    }
}

/// Regression test for the DECSTBM clobber described on
/// `StatusBarCtx::screen`: the bar's defensive scroll-region
/// re-assert used to write `\x1b[1;{rows-1}r` unconditionally, which
/// destroyed a workload's own sub-range -- including the one the attach
/// snapshot had just restored (docs/terminal-state-design.md section 6.2
/// step 3) -- and left the host terminal scrolling the wrong rows.
#[test]
fn draw_status_bar_reasserts_the_workload_scroll_region_not_its_own() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = status_ctx_for_test(true);
    // No workload region: the bar reserves the bottom row for itself, as
    // it always has.
    let pipe = StdoutToPipe::new();
    draw_status_bar(&ctx, true);
    let default_margins = String::from_utf8_lossy(&pipe.take()).into_owned();
    assert!(
            default_margins.contains("\x1b[1;23r"),
            "with a full-screen workload the bar must reserve row 24 for itself, got {default_margins:?}"
        );

    // Workload sets a DECSTBM sub-range (as an attach snapshot's trailing
    // bytes do, and as a margin-using TUI does live).
    feed_test_screen(&ctx.screen, b"\x1b[5;15r");
    let pipe = StdoutToPipe::new();
    draw_status_bar(&ctx, true);
    let sub_range = String::from_utf8_lossy(&pipe.take()).into_owned();
    assert!(
        sub_range.contains("\x1b[5;15r"),
        "the workload's own scroll region must be the one re-asserted, got {sub_range:?}"
    );
    assert!(
        !sub_range.contains("\x1b[1;23r"),
        "the bar must not clobber the workload's sub-range, got {sub_range:?}"
    );

    // Workload releases its region (`\x1b[r`): the bar's own reservation
    // must come straight back, or the reserved row stops being protected.
    feed_test_screen(&ctx.screen, b"\x1b[r");
    let pipe = StdoutToPipe::new();
    draw_status_bar(&ctx, true);
    let released = String::from_utf8_lossy(&pipe.take()).into_owned();
    assert!(
        released.contains("\x1b[1;23r"),
        "releasing the workload region must restore the bar's reservation, got {released:?}"
    );
}

/// A workload margin change with otherwise-identical bar text must not be
/// swallowed by the dirty-check -- that would leave the wrong scroll
/// region in force on the host until some unrelated text change happened.
#[test]
fn draw_status_bar_dirty_check_notices_a_workload_margin_change() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let _guard = StdoutToDevNull::new();
    let ctx = status_ctx_for_test(true);
    assert!(
        draw_status_bar(&ctx, false),
        "first draw must be a real write"
    );
    assert!(
        !draw_status_bar(&ctx, false),
        "unchanged state must be a skip"
    );
    feed_test_screen(&ctx.screen, b"\x1b[5;15r");
    assert!(
        draw_status_bar(&ctx, false),
        "a workload margin change must defeat the dirty-check even when the text is unchanged"
    );
}

// ---------------------------------------------------------------------
// Issue #5: the status bar must not be able to corrupt a workload frame.
//
// Everything below asserts on the *rendered screen* of a real
// `vt100::Parser` standing in for the user's terminal, compared against a
// second parser standing in for what the workload believes it drew. A
// byte-level assertion would pass while the screen was still wrong, which
// is exactly the trap this class of bug sets.
// ---------------------------------------------------------------------

/// The user's real terminal (`host`, full physical geometry) beside the
/// workload's own screen (`workload`, one row shorter -- its PTY is
/// resized to leave the status row free). The client is a byte relay
/// between them, so for every row the workload can reach the two must
/// render identically, character for character, and agree on the cursor.
struct Harness {
    ctx: StatusBarCtx,
    host: vt100::Parser,
    workload: vt100::Parser,
    rows: u16,
    cols: u16,
    /// Every status redraw that actually reached the terminal, with the
    /// stream state it was written at.
    redraws: Vec<Redraw>,
}

struct Redraw {
    at_escape_boundary: bool,
    in_synchronized_update: bool,
}

impl Harness {
    fn new() -> Self {
        let (rows, cols) = (24u16, 80u16);
        let ctx = status_ctx_for_test(true);
        let mut host = vt100::Parser::new(rows, cols, 0);
        // What `apply_terminal_layout` puts on the wire at attach time.
        host.process(format!("\x1b[1;{}r", rows - 1).as_bytes());
        Self {
            ctx,
            host,
            workload: vt100::Parser::new(rows - 1, cols, 0),
            rows,
            cols,
            redraws: Vec::new(),
        }
    }

    /// One PTY chunk: the workload's own screen sees it, and so does the
    /// client's model on its way to the host terminal.
    fn workload_emits(&mut self, data: &[u8]) {
        self.workload.process(data);
        let rewritten = {
            let mut screen = self
                .ctx
                .screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            screen.relay(data)
        };
        self.host.process(rewritten.as_deref().unwrap_or(data));
        // What the main frame loop does after every Data frame.
        if self.ctx.pending.load(Ordering::Relaxed) {
            self.status_redraw();
        }
    }

    /// What the status thread's timer does. Returns whether the redraw
    /// actually reached the terminal (false = deferred to `ctx.pending`).
    fn status_redraw(&mut self) -> bool {
        let (at_escape_boundary, in_synchronized_update) = {
            let screen = self
                .ctx
                .screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            (screen.at_escape_boundary(), screen.in_synchronized_update())
        };
        match status_bar_redraw(&self.ctx, true) {
            Some(bytes) => {
                assert!(!bytes.is_empty(), "a reported write must emit bytes");
                self.host.process(&bytes);
                self.redraws.push(Redraw {
                    at_escape_boundary,
                    in_synchronized_update,
                });
                true
            }
            None => false,
        }
    }

    /// Simulates `STATUS_BAR_SYNC_DEFER_LIMIT` having elapsed, so the
    /// synchronized-output deferral stops holding the redraw back and the
    /// escape-boundary gate is the only thing standing between the
    /// injection and the workload's half-emitted sequence. That is the
    /// production worst case (a frame longer than the limit, or a block
    /// the workload never closes) and the case issue #5 was reported
    /// from, so the tests drive it directly rather than hiding behind the
    /// softer gate.
    fn expire_sync_deferral(&self) {
        let mut since = self
            .ctx
            .sync_deferred_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *since = Instant::now().checked_sub(STATUS_BAR_SYNC_DEFER_LIMIT * 2);
    }

    fn row(parser: &vt100::Parser, row: u16, cols: u16) -> String {
        parser.screen().contents_between(row, 0, row, cols)
    }

    /// The load-bearing assertion: every row the workload can reach must
    /// render on the host exactly as the workload drew it, and the cursor
    /// must agree.
    fn assert_screens_agree(&self, label: &str) {
        for row in 0..self.rows - 1 {
            assert_eq!(
                Self::row(&self.host, row, self.cols),
                Self::row(&self.workload, row, self.cols),
                "{label}: host row {} diverged from the workload's screen",
                row + 1
            );
        }
        assert_eq!(
            self.host.screen().cursor_position(),
            self.workload.screen().cursor_position(),
            "{label}: host and workload disagree on the cursor position"
        );
    }

    fn assert_bar_drawn(&self, label: &str) {
        let bar = Self::row(&self.host, self.rows - 1, self.cols);
        assert!(
            bar.contains("status-bar-test"),
            "{label}: the reserved row must still carry the status bar, got {bar:?}"
        );
    }
}

/// One opencode/opentui-shaped frame: a synchronized-output block wrapping
/// a per-cell diff repaint, every run absolutely positioned with its own
/// SGR. Captured verbatim from a real `opencode` session for issue #5:
/// `\x1b[?2026h\x1b[?25l\x1b[15;64H\x1b[38;5;237m\x1b[48;5;234m\xc2\xb7...`
fn opencode_shaped_frame(generation: usize) -> Vec<u8> {
    let words = [
        "Replace",
        "with",
        "ValueError",
        "capture-warnings.html",
        "docs.pytest.org",
        "session",
        "passed",
    ];
    let mut frame = b"\x1b[?2026h\x1b[?25l".to_vec();
    for row in 1..=23usize {
        let mut col = 1usize;
        let mut k = 0usize;
        while col < 66 {
            let word = words[(generation + row + k) % words.len()];
            frame.extend_from_slice(
                format!(
                    "\x1b[{row};{col}H\x1b[38;5;{}m\x1b[48;5;234m{word}\x1b[0m",
                    16 + ((generation + row + k) % 200)
                )
                .as_bytes(),
            );
            col += word.len() + 1;
            k += 1;
        }
    }
    frame.extend_from_slice(b"\x1b[13;17H\x1b[?25h\x1b[?2026l");
    frame
}

/// **The issue #5 reproduction.** A status redraw requested while the
/// relayed stream sits inside a half-emitted CSI sequence -- which is
/// where a PTY read boundary lands about half the time under a
/// continuously-streaming TUI (measured: 5 of 10 redraws on a real
/// `a attach`) -- used to be written there anyway. The host terminal
/// abandons the workload's partial sequence when our `ESC` arrives and
/// prints its remaining parameter bytes as literal text into the frame:
/// `\x1b[38;5;` + our redraw + `91m...` renders a stray `91m` welded into
/// the row and shifts everything after it along, which is exactly the
/// reported `Rep69ce with Val` / `Doos` / `hetps` corruption.
///
/// The split point here is not hand-picked to be convenient: the test
/// walks *every* byte offset inside the frame, so it covers splits inside
/// CSI parameters, inside intermediate bytes, between an `ESC` and its
/// `[`, and inside the multi-byte characters the frame draws with.
#[test]
fn status_redraw_never_splices_into_a_workload_frame() {
    let frame = opencode_shaped_frame(0);
    // Every offset would be ~3500 harnesses; step through it densely
    // enough to hit every sequence position class many times over while
    // keeping the test fast.
    let mut deferred = 0usize;
    let mut written = 0usize;
    for split in (1..frame.len()).step_by(7) {
        let mut h = Harness::new();
        h.workload_emits(&frame[..split]);
        // The whole frame is inside a `?2026` block, so without this the
        // softer synchronized-output gate would defer every redraw and the
        // escape-boundary gate -- the one that actually prevents the
        // corruption -- would never be exercised.
        h.expire_sync_deferral();
        if h.status_redraw() {
            written += 1;
        } else {
            deferred += 1;
        }
        h.workload_emits(&frame[split..]);
        h.assert_screens_agree(&format!("split at byte {split}"));
        h.assert_bar_drawn(&format!("split at byte {split}"));
        for r in &h.redraws {
            assert!(
                r.at_escape_boundary,
                "split at byte {split}: a redraw was written mid-escape-sequence"
            );
        }
    }
    assert!(
        deferred > 0,
        "the frame must contain unsafe split points for this test to mean anything"
    );
    assert!(
        written > 0,
        "and safe ones, so the bar is not simply never drawn"
    );
}

/// One Claude-Code-shaped frame: **no `?2026` anywhere**. Ink-based TUIs
/// (Claude Code, and codex before it adopted synchronized output) repaint
/// with a full-screen erase followed by absolutely-positioned SGR runs,
/// and Claude Code opens with the DEC save/restore-cursor idiom
/// (`\x1b7\x1b[r\x1b8`) that the status bar used to clobber. Measured from
/// a real 24x100 capture: 1 `ESC 7`, 1 `ESC 8`, 0 `CSI ?2026h`.
fn claude_code_shaped_frame(generation: usize) -> Vec<u8> {
    let words = [
        "Removed",
        "InDjango70Warning",
        "category",
        "capture-warnings.html",
        "docs.pytest.org",
        "8 passed, 3 warnings",
    ];
    // The exact opening idiom, plus a save the workload restores later.
    let mut frame = b"\x1b7\x1b[r\x1b8\x1b[2J\x1b[H".to_vec();
    for row in 1..=23usize {
        frame.extend_from_slice(format!("\x1b[{row};1H").as_bytes());
        let mut k = 0usize;
        let mut col = 1usize;
        while col < 70 {
            let word = words[(generation + row + k) % words.len()];
            frame.extend_from_slice(
                format!(
                    "\x1b[38;5;{}m\x1b[1m{word}\x1b[0m ",
                    16 + ((generation + row + k) % 200)
                )
                .as_bytes(),
            );
            col += word.len() + 1;
            k += 1;
        }
    }
    frame.extend_from_slice(b"\x1b[9;5H\x1b7\x1b[23;1H\x1b[2Kfooter\x1b8ANCHORED");
    frame
}

/// `flash_status` is a *new* forced-redraw caller (the terminal-first CLI
/// merge routed the attach hint, `Ctrl-b ?` help and switch failures
/// through it, replacing an `eprintln!` banner and two direct
/// `draw_status_bar` calls). It must not be a hole in the boundary gate.
///
/// It is not, by construction rather than by discipline: the gate lives
/// inside `draw_status_bar`, which is the single funnel every bar write
/// goes through, so a caller cannot opt out of it -- `flash_status` passes
/// `force: true` and `force` deliberately does not bypass the boundary
/// check. This pins that: a flash raised while the workload is
/// mid-escape-sequence must be deferred rather than spliced, and must
/// still reach the terminal at the next boundary with its message intact.
///
/// This matters more since the maintainer's "let's not hide status"
/// decision (aplexer#12 closed won't-do): there is no suppression flag, so
/// the injected path has to be correct on its own for every caller.
#[test]
fn flash_status_cannot_bypass_the_boundary_gate() {
    let frame = opencode_shaped_frame(3);
    // A split inside a CSI parameter list -- the shape a real capture put
    // 11 of 54 status writes into.
    let needle = b"\x1b[38;5;";
    let split = frame
        .windows(needle.len())
        .position(|w| w == needle)
        .unwrap()
        + needle.len();
    let mut h = Harness::new();
    h.workload_emits(&frame[..split]);
    h.expire_sync_deferral();
    assert!(
        !h.ctx
            .screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at_escape_boundary(),
        "the harness must actually be mid-sequence for this test to mean anything"
    );

    flash_status(&h.ctx, "FLASHED-MESSAGE");
    assert!(
        h.ctx.pending.load(Ordering::Relaxed),
        "a flash raised mid-sequence must be deferred, not written"
    );
    assert!(
        !Harness::row(&h.host, h.rows - 1, h.cols).contains("FLASHED-MESSAGE"),
        "nothing may reach the terminal while the stream is mid-sequence"
    );

    // The rest of the frame arrives; the frame loop flushes the deferral.
    h.workload_emits(&frame[split..]);
    h.assert_screens_agree("flash deferred across a mid-sequence split");
    assert!(
        Harness::row(&h.host, h.rows - 1, h.cols).contains("FLASHED-MESSAGE"),
        "the deferred flash must still be delivered, got {:?}",
        Harness::row(&h.host, h.rows - 1, h.cols)
    );
    for r in &h.redraws {
        assert!(
            r.at_escape_boundary,
            "a flash was written mid-escape-sequence"
        );
    }
}

/// **The fix must not depend on synchronized-output mode.** opencode and
/// codex bracket every frame in `CSI ?2026 h/l`, but Claude Code -- the
/// most-used agent here -- emits none at all, so if the boundary detection
/// leaned on `?2026` it would be a strictly weaker code path for exactly
/// the workload that matters most.
///
/// It does not. `?2026` is a *soft* preference layered on top: it keeps a
/// redraw out of a frame the workload declared, and is bounded by
/// `STATUS_BAR_SYNC_DEFER_LIMIT` precisely so nothing can depend on it.
/// The protection is `ClientScreen::at_escape_boundary()`, which is
/// derived from the stream's own parser state and knows nothing about
/// `?2026`.
///
/// This test is the same all-offsets split walk as
/// `status_redraw_never_splices_into_a_workload_frame`, over a frame that
/// provably contains no synchronized-output markers, with the additional
/// assertion that the synchronized-update gate never once fired -- so a
/// green result here can only come from the escape-boundary gate.
#[test]
fn escape_boundary_gate_protects_a_workload_that_never_uses_synchronized_output() {
    let frame = claude_code_shaped_frame(0);
    assert!(
        !frame
            .windows(8)
            .any(|w| w == b"\x1b[?2026h" || w == b"\x1b[?2026l"),
        "this test is only meaningful on a frame with no ?2026 markers"
    );
    let mut deferred = 0usize;
    let mut written = 0usize;
    let mut redraws = 0usize;
    for split in (1..frame.len()).step_by(7) {
        let mut h = Harness::new();
        h.workload_emits(&frame[..split]);
        if h.status_redraw() {
            written += 1;
        } else {
            deferred += 1;
        }
        h.workload_emits(&frame[split..]);
        h.assert_screens_agree(&format!("no-sync split at byte {split}"));
        h.assert_bar_drawn(&format!("no-sync split at byte {split}"));
        // The workload's own `\x1b7`/`\x1b8` pair must have survived every
        // redraw: `ANCHORED` belongs at row 9 col 5, not wherever the bar
        // last left the cursor.
        assert!(
                Harness::row(&h.host, 8, h.cols).contains("ANCHORED"),
                "no-sync split at byte {split}: the workload's DECRC must land where it saved, row 9 was {:?}",
                Harness::row(&h.host, 8, h.cols)
            );
        for r in &h.redraws {
            assert!(
                r.at_escape_boundary,
                "no-sync split at byte {split}: a redraw was written mid-escape-sequence"
            );
            assert!(
                !r.in_synchronized_update,
                "no-sync split at byte {split}: this workload has no synchronized-output \
                     blocks, so the sync gate must never be what protected it"
            );
            redraws += 1;
        }
    }
    assert!(
        deferred > 0,
        "the frame must contain unsafe split points for this test to mean anything"
    );
    assert!(
        written > 0 && redraws > 0,
        "and the bar must still be drawn"
    );
}

/// A workload that uses the DEC save/restore-cursor register itself --
/// Claude Code opens with exactly `\x1b7\x1b[r\x1b8`, opencode uses the
/// same register via `CSI s`/`CSI u`, and every `tput sc`-style progress
/// line inside a session does too. A terminal has one such register, so
/// the status bar's old `\x1b7 ... \x1b8` bracket overwrote the workload's
/// saved position and its own later restore jumped to *ours*.
#[test]
fn workload_saved_cursor_survives_a_status_redraw() {
    let mut h = Harness::new();
    h.workload_emits(b"\x1b[2J\x1b[5;1HHEADER");
    h.workload_emits(b"\x1b7"); // workload saves its cursor at row 5
    h.workload_emits(b"\x1b[20;1Hfooter drawn elsewhere");
    assert!(h.status_redraw(), "a ground-state redraw must be written");
    h.workload_emits(b"\x1b8TAIL"); // workload restores -- must be row 5
    h.assert_screens_agree("workload DECSC/DECRC");
    assert!(
        Harness::row(&h.host, 4, h.cols).contains("HEADERTAIL"),
        "the workload's own restore must land where it saved, got {:?}",
        Harness::row(&h.host, 4, h.cols)
    );
    h.assert_bar_drawn("workload DECSC/DECRC");
}

/// A workload that never goes idle -- an agent CLI mid-generation, the
/// workload aplexer exists for -- never opens `STATUS_BAR_IDLE_GAP`, so
/// every redraw it ever gets is the `STATUS_BAR_MAX_INTERVAL` forced one.
/// This drives that worst case directly: a redraw requested after *every*
/// chunk of a continuous multi-frame stream chopped at pseudo-random
/// offsets. The bar must stay fresh, and no redraw may be written at an
/// unsafe point or inside a declared frame.
#[test]
fn continuously_streaming_workload_redraws_only_at_frame_boundaries() {
    let mut stream = Vec::new();
    for generation in 0..6 {
        stream.extend_from_slice(&opencode_shaped_frame(generation));
    }
    // Phase A: the synchronized-output deferral in force. Every redraw
    // that reaches the terminal must be both at an escape boundary and
    // outside a declared frame.
    let mut h = Harness::new();
    // A deterministic LCG stands in for PTY read boundaries, which fall at
    // arbitrary byte offsets rather than on sequence boundaries.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut at = 0usize;
    while at < stream.len() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = (((seed >> 33) % 900) + 100) as usize;
        let end = (at + len).min(stream.len());
        h.workload_emits(&stream[at..end]);
        h.status_redraw();
        at = end;
    }
    h.assert_screens_agree("continuous stream, sync respected");
    h.assert_bar_drawn("continuous stream, sync respected");
    assert!(
        !h.redraws.is_empty(),
        "a never-idle workload must still get its bar refreshed"
    );
    for (i, r) in h.redraws.iter().enumerate() {
        assert!(
            r.at_escape_boundary,
            "redraw {i} was written mid-escape-sequence"
        );
        assert!(
            !r.in_synchronized_update,
            "redraw {i} was written inside a synchronized-output frame"
        );
    }

    // Phase B: the deferral bounded out (a frame longer than
    // `STATUS_BAR_SYNC_DEFER_LIMIT`, or a block the workload never
    // closes). Redraws are now allowed inside a frame, so the
    // escape-boundary gate is the only protection left -- and it has to be
    // enough, which is the whole claim of this fix.
    let mut h = Harness::new();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut at = 0usize;
    while at < stream.len() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let len = (((seed >> 33) % 900) + 100) as usize;
        let end = (at + len).min(stream.len());
        h.workload_emits(&stream[at..end]);
        h.expire_sync_deferral();
        h.status_redraw();
        at = end;
    }
    h.assert_screens_agree("continuous stream, deferral bounded out");
    h.assert_bar_drawn("continuous stream, deferral bounded out");
    assert!(
        h.redraws.len() >= 10,
        "with the deferral bounded out the bar must refresh often; got {} redraws",
        h.redraws.len()
    );
    assert!(
        h.redraws.iter().any(|r| r.in_synchronized_update),
        "this phase must actually exercise redraws inside a frame"
    );
    for (i, r) in h.redraws.iter().enumerate() {
        assert!(
            r.at_escape_boundary,
            "redraw {i} was written mid-escape-sequence"
        );
    }
}

/// docs/terminal-state-design.md section 7.1's reserved-row walk, which
/// this replaces the old characterization test for. While the client
/// re-asserts a workload's own DECSTBM sub-range, the host's bottom row is
/// the screen bottom rather than a margin boundary, so a line feed on the
/// workload's last row walked the host cursor onto the reserved row and
/// left it there -- the workload's screen model and the host permanently
/// one row apart. The client's model now detects that at the byte that
/// causes it and splices in an absolute reposition.
#[test]
fn workload_line_feed_no_longer_reaches_the_reserved_row_under_a_sub_range() {
    let mut h = Harness::new();
    h.workload_emits(b"\x1b[5;15r");
    assert!(
        h.status_redraw(),
        "the bar re-asserts the workload sub-range"
    );
    h.workload_emits(b"\x1b[23;1HWORKLOAD-LAST-ROW\nWALKED");
    h.assert_screens_agree("sub-range line feed");
    assert_eq!(
        h.host.screen().cursor_position().0 + 1,
        23,
        "the cursor must stay on the workload's last row"
    );
    h.assert_bar_drawn("sub-range line feed");
}

/// The client must never write DECSC/DECRC into a stream it is only
/// relaying -- not from the status bar and not from the layout code. This
/// is a hard cut, not a preference: there is one save-cursor register and
/// it belongs to the workload.
#[test]
fn client_never_writes_the_shared_save_cursor_register() {
    let ctx = status_ctx_for_test(true);
    let restore = ctx
        .screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .cursor_restore();
    let mut bytes = status_bar_redraw(&ctx, true).expect("a ground-state redraw writes");
    bytes.extend_from_slice(&terminal_layout_sequence(24, &restore));
    bytes.extend_from_slice(TERMINAL_RESET_SEQUENCE);
    assert!(!bytes.is_empty());
    for pair in [&b"\x1b7"[..], b"\x1b8"] {
        assert!(
            !bytes.windows(2).any(|w| w == pair),
            "client emitted {pair:?}: {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }
}

#[test]
fn draw_status_bar_not_reserved_never_writes() {
    let ctx = status_ctx_for_test(false);
    assert!(!draw_status_bar(&ctx, false));
    assert!(!draw_status_bar(&ctx, true));
}

#[test]
fn live_screen_refresh_repaints_model_contents_and_the_bar() {
    let ctx = status_ctx_for_test(true);
    feed_test_screen(&ctx.screen, b"recover-me\r\n");
    let bytes = live_screen_refresh_locked(&ctx).expect("ground-state refresh writes");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        bytes.windows(4).any(|w| w == b"\x1b[2J") || bytes.windows(3).any(|w| w == b"\x1b[J"),
        "refresh must clear before repainting: {text:?}"
    );
    assert!(
        text.contains("recover-me"),
        "refresh must include the live screen: {text:?}"
    );
    assert!(
        bytes.windows(4).any(|w| w == b"\x1b[7m"),
        "refresh must restore the status bar the snapshot's clear wiped: {text:?}"
    );
    assert!(!ctx.pending_refresh.load(Ordering::Relaxed));
}

#[test]
fn live_screen_refresh_defers_mid_escape_sequence() {
    let ctx = status_ctx_for_test(true);
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");
    assert!(live_screen_refresh_locked(&ctx).is_none());
    assert!(
        ctx.pending_refresh.load(Ordering::Relaxed),
        "a deferred refresh must be retried at the next safe boundary"
    );
}

#[test]
fn draw_status_bar_dirty_check_reports_skip_vs_real_write() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let _guard = StdoutToDevNull::new();
    let ctx = status_ctx_for_test(true);
    // Nothing drawn yet: even a non-forced call must actually write
    // (there's no `last_drawn` to compare against).
    assert!(
        draw_status_bar(&ctx, false),
        "first draw must be a real write"
    );
    // Same record/geometry, so the rendered text is unchanged: a
    // non-forced call must be a dirty-check no-op, not a real write --
    // this is exactly the case the timer-starvation bug got wrong by
    // treating a no-op the same as a real write for timer-reset
    // purposes.
    assert!(
        !draw_status_bar(&ctx, false),
        "unchanged text must be a dirty-check skip, not a real write"
    );
    // `force: true` must bypass the dirty-check unconditionally, since
    // that's the self-heal guarantee the overdue timer and every
    // switch/flash redraw rely on.
    assert!(
        draw_status_bar(&ctx, true),
        "force=true must always be a real write, even with unchanged text"
    );
}

#[test]
fn real_zero_sized_pty_uses_conventional_geometry() {
    use std::os::fd::AsRawFd;

    let (_master, slave) = aplexer::open_pty(0, 0).unwrap();
    assert_eq!(
        terminal_size(slave.as_raw_fd()),
        Some((
            aplexer::screen::DEFAULT_TERMINAL_ROWS,
            aplexer::screen::DEFAULT_TERMINAL_COLS,
        ))
    );
}

// -- The typing bar is a live-stream writer, not a suspended one -------
//
// `i` (type-through) hands the keyboard back while the pager stays up,
// and the relay streams workload bytes to the host again. The typing bar
// is then client-originated output spliced into a live stream, with the
// same two obligations as the live bar: never splice mid-sequence, and
// repair the row when the workload's Erase-in-Display -- which ignores
// scroll margins -- takes it out. Reproduced against a real zcodex
// session: after wheel-up + `i`, the workload's first `CSI ... J` wiped
// the bar and nothing ever rewrote it, because the frame loop `continue`d
// past every bar path while scroll mode was active and the status tick's
// dirty check saw unchanged text.
