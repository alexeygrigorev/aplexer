fn ctx_in_typing_mode() -> StatusBarCtx {
    let ctx = status_ctx_for_test(true);
    ctx.scroll.active.store(true, Ordering::SeqCst);
    ctx.scroll.typing.store(true, Ordering::SeqCst);
    ctx
}

#[test]
fn typing_bar_waits_for_an_escape_boundary_and_parks_until_one() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = ctx_in_typing_mode();
    // Half a CSI sequence: the relayed stream is mid-escape, so the bar
    // write must be refused, parked for the frame loop, and nothing may
    // reach the terminal.
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");
    let pipe = StdoutToPipe::new();
    assert!(
        !refresh_scroll_bar(&ctx),
        "a mid-sequence typing-bar write must be deferred"
    );
    assert!(
        pipe.take().is_empty(),
        "a deferred typing-bar write must not reach the terminal"
    );
    assert!(
        ctx.pending.load(Ordering::Relaxed),
        "a deferred typing-bar write must be parked for the frame loop"
    );
    // Complete the sequence: the parked write goes out at the boundary,
    // and `pending` (which forced the write past the dirty check) is
    // cleared so the tick's dirty check is honest again.
    feed_test_screen(&ctx.screen, b"m");
    let pipe = StdoutToPipe::new();
    assert!(refresh_scroll_bar(&ctx), "the parked write flushes");
    let text = String::from_utf8_lossy(&pipe.take()).into_owned();
    assert!(
        text.contains("TYPE"),
        "the typing wording must reach the bar row: {text:?}"
    );
    assert!(
        !ctx.pending.load(Ordering::Relaxed),
        "a delivered write must clear the parking flag"
    );
}

#[test]
fn layout_erase_while_typing_repairs_an_unchanged_bar_row() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = ctx_in_typing_mode();
    let pipe = StdoutToPipe::new();
    assert!(refresh_scroll_bar(&ctx), "first draw writes the bar");
    assert!(!pipe.take().is_empty());
    // The dirty check must skip when nothing changed -- this is what
    // kept the erased row blank forever before the fix: the text was
    // unchanged, so every later refresh saw "already drawn" and stopped.
    assert!(
        !refresh_scroll_bar(&ctx),
        "unchanged text must be a dirty-check skip"
    );
    // What the `Layout` arm does when the workload erased the screen
    // while typing: invalidate `last_drawn`, then refresh. The text is
    // byte-identical; the write must happen anyway, because the row the
    // text lives on no longer holds it.
    *ctx.last_drawn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    let pipe = StdoutToPipe::new();
    assert!(
        refresh_scroll_bar(&ctx),
        "an invalidated dirty check must rewrite the bar row"
    );
    assert!(
        !pipe.take().is_empty(),
        "the repair must be a real write, not a bookkeeping update"
    );
}

#[test]
fn pager_bar_without_typing_still_writes_unconditionally() {
    // Deliberate asymmetry, pinned so it reads as decided rather than
    // forgotten: with the pager up but NOT typing, the relay is
    // suspended, so there is no live stream to splice into and the bar
    // does not wait for a boundary -- a workload stopped mid-sequence
    // must not freeze the bar the user is actively reading against.
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = status_ctx_for_test(true);
    ctx.scroll.active.store(true, Ordering::SeqCst);
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");
    let pipe = StdoutToPipe::new();
    assert!(
        refresh_scroll_bar(&ctx),
        "a stream-suspended write goes out at once"
    );
    assert!(!pipe.take().is_empty());
    assert!(
        !ctx.pending.load(Ordering::Relaxed),
        "a stream-suspended write never parks"
    );
}

// -- Issue #14: the resize path is a client-originated writer too ------
//
// `2db19d0` put the escape-boundary gate inside `draw_status_bar`, which
// covered its eight callers and silently did not cover
// `apply_terminal_layout` -- a ninth writer, driven by the resize
// poller's wall clock, that wrote DECSTBM straight to stdout. Nothing
// failed; a reviewer found it by reading. These tests are what fails
// instead, and the last one is what fails for writer eleven.

/// Runs the real resize path into a buffer instead of fd 1. Nothing here
/// re-implements production's decision -- `apply_terminal_layout_to` is
/// what `apply_terminal_layout` calls under the stdout lock -- and
/// keeping the process's fd 1 out of it means these tests neither
/// serialize on `FD1_GUARD` nor can catch another thread's stray write.
fn resize_capturing(ctx: &StatusBarCtx, rows: u16, cols: u16) -> (bool, Vec<u8>) {
    let mut sink = Vec::new();
    let wrote = apply_terminal_layout_to(&mut sink, ctx, rows, cols);
    assert_eq!(
        wrote,
        !sink.is_empty(),
        "the resize path's return value must agree with what it actually wrote"
    );
    (wrote, sink)
}

fn flush_capturing(ctx: &StatusBarCtx) -> (bool, Vec<u8>) {
    let mut sink = Vec::new();
    let wrote = flush_pending_layout_to(&mut sink, ctx);
    (wrote, sink)
}

/// Backdates the parked resize's deadline, standing in for
/// `LAYOUT_DEFER_LIMIT` having elapsed without the stream ever reaching
/// a boundary -- a workload that stopped mid-escape-sequence.
fn expire_layout_deferral(ctx: &StatusBarCtx) {
    let mut pending = ctx
        .pending_layout
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(p) = pending.as_mut() {
        p.since = Instant::now()
            .checked_sub(LAYOUT_DEFER_LIMIT * 2)
            .expect("backdate the layout deadline");
    }
}

/// A resize raised while the workload is mid-escape-sequence must put
/// nothing on the wire. This is the splice the issue describes: the host
/// terminal would abandon the workload's half-emitted CSI and print its
/// remaining parameter bytes as literal text.
///
/// The geometry is still recorded on the spot, deliberately: the
/// physical terminal has already changed size, and `TermGeom` is
/// internal state rather than output, so the status bar must start
/// targeting the real last row immediately.
#[test]
fn resize_mid_escape_sequence_defers_decstbm_instead_of_splicing() {
    let ctx = status_ctx_for_test(true);

    // Control: at a boundary the same call writes, so a later "nothing
    // was written" assertion means the gate, not a broken fixture.
    let (wrote, bytes) = resize_capturing(&ctx, 24, 80);
    assert!(wrote, "a resize at an escape boundary must be written");
    assert!(
        String::from_utf8_lossy(&bytes).contains("\x1b[1;23r"),
        "expected the row reservation for a 24-row terminal, got {:?}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(
        ctx.pending_layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none(),
        "a resize that was written must leave nothing parked"
    );

    // Now mid-CSI, exactly as a PTY read boundary leaves the stream
    // about half the time under a streaming TUI.
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");
    assert!(
        !ctx.screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at_escape_boundary(),
        "the fixture must actually be mid-sequence for this test to mean anything"
    );
    let (wrote, bytes) = resize_capturing(&ctx, 30, 100);
    assert!(!wrote, "a resize raised mid-sequence must not be written");
    assert!(
        bytes.is_empty(),
        "nothing may reach the terminal mid-sequence, got {:?}",
        String::from_utf8_lossy(&bytes)
    );
    let parked = *ctx
        .pending_layout
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let parked = parked.expect("a deferred resize must be parked, not dropped");
    assert_eq!((parked.rows, parked.cols), (30, 100));
    let geom = *ctx.term.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(
        (geom.rows, geom.cols),
        (30, 100),
        "the new physical geometry must be recorded even while the bytes wait"
    );
}

/// Deferral is not dropping. The parked resize must reach the terminal
/// at the next boundary -- and must carry the *latest* geometry, since a
/// superseded size would leave the workload rendering at the wrong
/// geometry just as surely as dropping it would.
#[test]
fn deferred_resize_is_delivered_at_the_next_boundary_with_the_latest_geometry() {
    let ctx = status_ctx_for_test(true);
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");

    assert!(!resize_capturing(&ctx, 28, 80).0);
    // A second resize while the first is still parked: the user kept
    // dragging the window edge.
    assert!(!resize_capturing(&ctx, 32, 100).0);
    let (wrote, bytes) = flush_capturing(&ctx);
    assert!(
        !wrote && bytes.is_empty(),
        "a flush while still mid-sequence must stay silent, got {:?}",
        String::from_utf8_lossy(&bytes)
    );

    // The workload completes its sequence: the stream is at a boundary
    // again, which is exactly what the frame loop's flush waits for.
    feed_test_screen(&ctx.screen, b"91m");
    let (wrote, bytes) = flush_capturing(&ctx);
    assert!(wrote, "the deferred resize must be delivered, not dropped");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    assert!(
        text.contains("\x1b[1;31r"),
        "the latest geometry (32 rows -> DECSTBM 1;31) must be the one delivered, got {text:?}"
    );
    assert!(
        !text.contains("\x1b[1;27r"),
        "a superseded deferred resize must not be the one delivered, got {text:?}"
    );
    assert!(
        ctx.pending_layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none(),
        "a delivered resize must clear the parking slot"
    );
    // Idempotent: nothing parked, nothing written.
    let (wrote, bytes) = flush_capturing(&ctx);
    assert!(
        !wrote && bytes.is_empty(),
        "flushing with nothing parked must be a no-op, got {:?}",
        String::from_utf8_lossy(&bytes)
    );
}

/// The one case the boundary gate cannot wait out: a workload that stops
/// emitting part-way through an escape sequence. There is no next
/// boundary, so `LAYOUT_DEFER_LIMIT` writes anyway -- one spliced frame
/// beats a host terminal left scrolling the old geometry for the rest of
/// the attach, which is the failure the issue calls worse than the
/// splice. This asserts the exemption rather than describing it.
#[test]
fn deferred_resize_is_written_once_the_defer_limit_expires() {
    let ctx = status_ctx_for_test(true);
    feed_test_screen(&ctx.screen, b"\x1b[38;5;");
    assert!(!resize_capturing(&ctx, 20, 80).0);

    expire_layout_deferral(&ctx);
    assert!(
        !ctx.screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at_escape_boundary(),
        "the stream must still be mid-sequence: the point is that the deadline, \
             not a recovered boundary, is what delivers this"
    );
    let (wrote, bytes) = flush_capturing(&ctx);
    assert!(
        wrote,
        "past LAYOUT_DEFER_LIMIT the resize must go out rather than be stranded"
    );
    assert!(
        String::from_utf8_lossy(&bytes).contains("\x1b[1;19r"),
        "expected the row reservation for a 20-row terminal, got {:?}",
        String::from_utf8_lossy(&bytes)
    );
    assert!(
        ctx.pending_layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none(),
        "the deadline write must also clear the parking slot"
    );
}

/// A physical grow changes the bar's target row, but DECSTBM only changes
/// scrolling behavior; it does not erase the bar that was painted at the
/// old bottom. The production wrapper must therefore repaint the model so
/// the old row is cleared and the only visible bar is on the new bottom.
#[test]
fn resize_repaints_the_status_bar_at_the_new_bottom() {
    let _fd1 = FD1_GUARD.lock().unwrap_or_else(PoisonError::into_inner);
    let ctx = status_ctx_for_test(true);
    let mut host = vt100::Parser::new(34, 80, 0);

    let old_bar = status_bar_redraw(&ctx, true).expect("the old bar must render");
    host.process(&old_bar);
    assert!(
        host.screen()
            .contents_between(23, 0, 23, 80)
            .contains("RUNNING"),
        "the fixture must start with a bar on the old bottom row"
    );

    ctx.screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .set_size(33, 80);
    let pipe = StdoutToPipe::new();
    assert!(apply_terminal_layout(&ctx, 34, 80));
    let repaint = pipe.take();
    host.process(&repaint);

    assert!(
        !host
            .screen()
            .contents_between(23, 0, 23, 80)
            .contains("RUNNING"),
        "the old status-bar row must be cleared after a grow"
    );
    assert!(
        host.screen()
            .contents_between(33, 0, 33, 80)
            .contains("RUNNING"),
        "the status bar must be painted on the physical bottom row"
    );
}

/// The screen-level statement of the same thing, through a real `vt100`
/// host terminal: after a resize raised in the middle of a workload's
/// absolute-positioning sequence, every row the workload can reach must
/// still render exactly what the workload drew.
///
/// The second half is the control. It replays the identical resize the
/// *ungated* code would have written at the same offset, and asserts the
/// host screen is then wrong -- so this test fails if the gate is
/// removed, rather than passing because the splice happened to be
/// harmless.
#[test]
fn resize_across_a_mid_sequence_split_leaves_the_host_screen_intact() {
    let ctx = status_ctx_for_test(true);
    let (rows, cols) = (24u16, 80u16);
    let mut host = vt100::Parser::new(rows, cols, 0);
    let mut ungated = vt100::Parser::new(rows, cols, 0);
    let mut workload = vt100::Parser::new(rows - 1, cols, 0);
    for p in [&mut host, &mut ungated] {
        p.process(format!("\x1b[1;{}r", rows - 1).as_bytes());
    }

    // Ink-shaped output: words painted at absolute columns, which is
    // what makes a misaligned injection weld two frames onto one row.
    let frame = b"\x1b[2;1H\x1b[0m\x1b[2GQuick\x1b[8Gsafety\x1b[16Gcheck";
    // Split inside `\x1b[16G` -- a CSI with its parameters half emitted,
    // which is what a PTY read boundary looks like about half the time
    // under a streaming TUI.
    let split = frame.len() - 7;
    for p in [&mut host, &mut ungated] {
        p.process(&frame[..split]);
    }
    workload.process(&frame[..split]);
    feed_test_screen(&ctx.screen, &frame[..split]);

    // What the resize poller does at this instant.
    let (wrote, _) = resize_capturing(&ctx, 20, cols);
    assert!(!wrote, "the resize must be deferred here, not written");
    // What it used to do: DECSTBM straight onto the wire, mid-CSI.
    let restore = ctx
        .screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .cursor_restore();
    ungated.process(&terminal_layout_sequence(20, &restore));

    for p in [&mut host, &mut ungated] {
        p.process(&frame[split..]);
    }
    workload.process(&frame[split..]);
    feed_test_screen(&ctx.screen, &frame[split..]);
    let (wrote, bytes) = flush_capturing(&ctx);
    assert!(wrote, "the deferred resize must still be delivered");
    host.process(&bytes);

    let row_of = |p: &vt100::Parser| p.screen().contents_between(1, 0, 1, cols);
    assert_eq!(
        row_of(&host),
        row_of(&workload),
        "the gated resize must leave the host row exactly as the workload drew it"
    );
    assert_ne!(
        row_of(&ungated),
        row_of(&workload),
        "control: the ungated resize must actually corrupt this row, otherwise \
             this test would pass with the gate removed"
    );
}
