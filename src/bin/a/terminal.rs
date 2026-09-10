/// Default amount of history replayed on attach when the caller didn't ask
/// for more via `--history-bytes`. The old default -- passing `None` through
/// to the server, which `History::snapshot` treats as "the whole buffer" --
/// meant every attach replayed up to the session's entire configured
/// history capacity (DEFAULT_HISTORY_BYTES = 4MB), which looks like the
/// session "rewinding" through its whole scrollback instead of showing
/// anything resembling the current screen. There's no real terminal
/// emulator here (spec.md's v1 non-goal), so this can only approximate
/// "current state" by replaying a short tail of raw bytes -- in practice
/// that tail still usually contains the shell/TUI's own recent
/// cursor-position/clear escapes and renders close enough.

const DEFAULT_ATTACH_REPLAY_BYTES: usize = 32 * 1024;

/// **These timers no longer decide whether a redraw is *safe*, only when one
/// is *wanted*.** The previous version of this comment said aplexer had "no
/// real terminal emulation (spec.md's v1 non-goal)" and that building it was
/// "a much bigger project than this fix", so the timers were the whole
/// mitigation: redraw in an idle gap and hope. That was already stale when it
/// was written -- docs/terminal-state-design.md shipped a live `vt100` model
/// in the worker -- and the hope did not survive contact with the workload
/// aplexer exists for. An agent CLI mid-generation never goes quiet, so
/// `STATUS_BAR_IDLE_GAP` never opened and `STATUS_BAR_MAX_INTERVAL` fired into
/// an arbitrary byte offset of the relayed stream, forever. Measured on a real
/// `a attach` against a continuously-streaming full-screen TUI (issue #5), 5 of
/// 10 redraws were spliced into the middle of an unterminated CSI sequence:
/// the host terminal abandoned the workload's half-read sequence and printed
/// its remaining parameter bytes as literal text into the workload's own
/// frame.
///
/// The client now keeps its own `ClientScreen` (`aplexer::screen`) over the
/// bytes it relays, so "is this a safe place to write?" is answered from the
/// stream's actual parser state rather than guessed from a clock:
/// `draw_status_bar` refuses to write unless the stream is between complete
/// escape sequences and characters, and defers to `ctx.pending` otherwise --
/// which the frame loop flushes at the first safe boundary. What is left for
/// these constants is scheduling: `STATUS_BAR_IDLE_GAP` still debounces an
/// idle session's redraws, and `STATUS_BAR_MAX_INTERVAL` still bounds how
/// stale a continuously-streaming session's bar may get.
const STATUS_BAR_IDLE_GAP: Duration = Duration::from_millis(450);
const STATUS_BAR_MAX_INTERVAL: Duration = Duration::from_secs(3);
const STATUS_BAR_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// While the attached session's state is `working` (a fresh `a state-report`
/// push -- the agent said it is running; see `spinner_frame` for why the
/// guessed `active` state deliberately does not animate), the state glyph
/// becomes a braille spinner so the bar shows liveness a static state word
/// cannot. The spinner is the only thing that moves: the rest of the bar is
/// byte-identical frame to frame, so `draw_status_bar`'s dirty-check means
/// the animation itself is the entire incremental write cost, and the
/// instant the state word leaves working the glyph freezes back to
/// `state_glyph`'s static one. One frame per `STATUS_BAR_POLL_INTERVAL`
/// tick keeps the cadence aligned with the thread that drives it; ten
/// frames is a 1.5s revolution -- standard spinner speed, deliberately
/// unhurried.
const SPINNER_FRAME_MS: u64 = STATUS_BAR_POLL_INTERVAL.as_millis() as u64;
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// How long a redraw may be held back purely because the workload has an
/// unclosed synchronized-output block (`CSI ? 2026 h`).
///
/// Staying out of a declared frame is a genuine improvement -- opencode and
/// codex bracket every frame in `?2026`, so the block boundaries are exact
/// frame boundaries handed to us for free -- but it is a *preference*, not a
/// correctness requirement: an injection at an escape boundary inside a
/// synchronized block is transparent anyway, since the cursor and pen are
/// restored absolutely. Bounding it means a workload that opens a block and
/// never closes it (or a terminal-side sync timeout that already released it)
/// cannot freeze the bar indefinitely.
const STATUS_BAR_SYNC_DEFER_LIMIT: Duration = Duration::from_millis(500);

/// How long a terminal resize's DECSTBM may be held back by the escape
/// boundary gate before it is written anyway (issue #14).
///
/// The gate is the same one `draw_status_bar` uses, but the escape hatch is
/// not. A status redraw that never happens costs a stale bar; a *resize* that
/// never happens leaves the host terminal scrolling a region sized for the
/// old geometry for the rest of the attach, which is exactly the "workload
/// renders at the wrong size indefinitely" outcome that is worse than the
/// splice the gate exists to prevent. A stream normally reaches a boundary
/// within one PTY chunk, so this deadline only fires when a workload has
/// stopped emitting part-way through an escape sequence -- a state in which
/// the host terminal is already stuck waiting for bytes that are not coming.
const LAYOUT_DEFER_LIMIT: Duration = Duration::from_millis(500);

/// A terminal resize whose DECSTBM the boundary gate held back, and when it
/// was first held back (`LAYOUT_DEFER_LIMIT`'s deadline is measured from the
/// first deferral, not from the most recent resize).
#[derive(Clone, Copy)]
struct PendingLayout {
    rows: u16,
    cols: u16,
    since: Instant,
}

/// Physical terminal geometry as last observed by the resize-poll thread,
/// shared with the status-bar thread so its redraws always target the
/// current last row/width without a second ioctl.
#[derive(Clone, Copy)]
struct TermGeom {
    rows: u16,
    cols: u16,
    /// Whether the bottom row is reserved for the status bar. False for
    /// terminals too small to spare a row (see `reserved_rows`), in which
    /// case the scroll region is left/reset to full-screen and the status
    /// bar is simply not drawn.
    reserved: bool,
}

/// The row count told to the SERVER: one less than the physical terminal
/// when a status row is reserved, exactly like tmux tells the remote PTY its
/// terminal is one row shorter than reality so its own output never
/// overwrites the reserved line.
fn reserved_rows(rows: u16) -> u16 {
    if rows > 2 {
        rows - 1
    } else {
        rows
    }
}

/// Serializes a write behind the shared stdout lock so the main frame loop
/// (writing PTY data) and the status-bar/layout threads (writing redraws)
/// can never tear/interleave each other's output.
///
/// **The stdout lock is also the client's terminal-state lock.** Anything that
/// consults `ClientScreen` in order to decide *what* to write -- where the
/// workload's cursor is, whether the stream is between escape sequences --
/// must hold this lock across both the decision and the write, or the answer
/// can go stale in the gap. It did: the first version of the issue #5 fix
/// checked the escape boundary in the status thread and wrote afterwards, and
/// a real capture caught 4 of 39 redraws still landing mid-CSI because the
/// frame loop had written another chunk in between.
///
/// Lock order everywhere is **`stdout` -> `term` -> `screen`**, with
/// `last_drawn`/`flash`/`record` as leaves. Nothing acquires `stdout` while
/// holding `term` or `screen`: `apply_terminal_layout` writes and only then
/// records the new geometry, and the switch path reads the geometry before
/// taking `stdout`.
///
/// **Not for live injections.** This helper writes unconditionally, so it is
/// only correct where there is no relayed stream to splice: attach start
/// (before the first workload byte) and detach (after the last one). A
/// status redraw, a `Ctrl-b r` refresh, or a resize DECSTBM goes through
/// `write_client_locked`, which is the boundary gate. `write_locked`'s two
/// call sites are pinned by
/// `every_client_terminal_write_site_is_gated_or_explicitly_exempt`.
fn write_locked(stdout: &Arc<Mutex<io::Stdout>>, bytes: &[u8]) -> io::Result<()> {
    let mut out = stdout
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    out.write_all(bytes)?;
    out.flush()
}

/// Whether a client-originated write may still go out when the relayed
/// stream is *not* between complete escape sequences.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BoundaryPolicy {
    /// Refuse and let the caller park the write for the next boundary.
    /// Everything the user can wait for: the status bar, `Ctrl-b r`, and a
    /// resize that has not yet hit `LAYOUT_DEFER_LIMIT`.
    Defer,
    /// Write anyway. **The one narrow exemption in the client** (issue #14):
    /// a resize whose DECSTBM has already been held back for
    /// `LAYOUT_DEFER_LIMIT`, i.e. a workload that stopped emitting part-way
    /// through an escape sequence and is never going to finish it. One
    /// spliced frame beats a host terminal left scrolling the old geometry
    /// for the rest of the attach. Exactly one call site may pass this, and
    /// `every_client_terminal_write_site_is_gated_or_explicitly_exempt`
    /// fails if a second one appears.
    PastDeadline,
    /// Write anyway, because **there is no relayed stream to splice into**.
    ///
    /// Not a second exemption from the gate so much as a case the gate does
    /// not apply to. While scroll mode is active (`Ctrl-b [`, or a wheel
    /// roll) the client has taken the host terminal away from the relay
    /// entirely: `relay_to_terminal` still feeds every workload byte to the
    /// model -- that is what keeps the history growing and makes the exit
    /// repaint correct -- but writes none of them, so the host is not
    /// part-way through anything the workload emitted. `at_escape_boundary`
    /// would still be answering for the *model*, which by then is many
    /// chunks ahead of the host, so consulting it here would be consulting
    /// the wrong stream: it can sit false indefinitely on a workload that
    /// stopped mid-sequence, and deferring on that would freeze the pager
    /// the user is actively driving.
    ///
    /// What the host may genuinely be part-way through is the *last* chunk
    /// written before the relay was suspended. `SCROLL_CANCEL` (`CAN`, the
    /// control every VT parser treats as "abandon the sequence in flight")
    /// leads every write made under this policy, which is what makes it
    /// safe; the sites are pinned by
    /// `scroll_mode_writes_are_the_only_stream_suspended_ones`.
    StreamSuspended,
}

/// **The single funnel for client-originated bytes**, and therefore the one
/// place the escape-boundary gate has to live.
///
/// Issue #5 put the gate inside `draw_status_bar`, which covered its eight
/// callers and silently did not cover `apply_terminal_layout` -- a ninth
/// writer that reached stdout by another route and spliced DECSTBM into
/// half-emitted CSI sequences from the resize poller's wall clock (issue
/// #14). A gate that each new writer has to *remember* is a gate that the
/// next writer forgets, so it moved here: writer eleven is gated because it
/// cannot put bytes on the terminal any other way.
///
/// The caller must already hold the stdout lock. That is not tidiness: the
/// first version of the #5 fix checked the boundary in the status thread and
/// wrote afterwards, and a real capture caught 4 of 39 redraws still landing
/// mid-CSI, because the frame loop wrote another chunk in the gap. Checking
/// and writing under one lock is what closes it -- see `write_locked`.
///
/// Returns whether the bytes went out. `false` means the stream was
/// mid-sequence and the caller must park the write for a later boundary
/// rather than drop it.
fn write_client_locked(
    out: &mut impl Write,
    screen: &Arc<Mutex<aplexer::screen::ClientScreen>>,
    bytes: &[u8],
    policy: BoundaryPolicy,
) -> bool {
    let at_boundary = screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .at_escape_boundary();
    if !at_boundary && policy == BoundaryPolicy::Defer {
        return false;
    }
    let _ = out.write_all(bytes);
    let _ = out.flush();
    true
}

/// The DECSTBM reservation (or its removal, on a terminal too small to spare
/// a row) followed by an absolute cursor restore from the client's model.
///
/// No `\x1b7`/`\x1b8` bracket: see `status_bar_sequence` for why the client
/// must never write to the shared save-cursor register.
fn terminal_layout_sequence(rows: u16, restore: &[u8]) -> Vec<u8> {
    let mut seq = Vec::new();
    if rows > 2 {
        seq.extend_from_slice(format!("\x1b[1;{}r", rows - 1).as_bytes());
    } else {
        seq.extend_from_slice(b"\x1b[r");
    }
    seq.extend_from_slice(restore);
    seq
}

/// Sets (or, for a too-small terminal, clears) the DECSTBM scrolling region
/// and records the resulting geometry for the status-bar thread.
///
/// DECSTBM moves the cursor to the region's home position as a side effect on
/// real terminals, so something has to put it back. That used to be a
/// `\x1b7`/`\x1b8` (DECSC/DECRC) bracket, which is exactly the bug issue #5
/// exists for: a terminal has one save-cursor register, and writing to it from
/// a stream we are only relaying destroys whatever the workload had saved
/// there. The cursor is restored from `ClientScreen` instead -- absolutely,
/// and including the workload's pen -- so the register stays the workload's
/// private property.
///
/// **Boundary-gated, exactly like `draw_status_bar`** (issue #14). This is
/// client-originated output spliced into a stream the client is only
/// relaying, so the same rule applies: a resize landing while the workload is
/// mid-escape-sequence would make the host terminal abandon the workload's
/// half-emitted CSI and print its remaining parameter bytes as literal text.
/// The resize poller fires from a wall clock, so its writes land at arbitrary
/// byte offsets by construction -- the identical defect the status bar had.
///
/// Two deliberate differences from `draw_status_bar`'s use of the same gate:
///
/// - **No synchronized-output deferral.** Holding a *status bar* out of a
///   workload's declared frame is a cosmetic preference (see
///   `STATUS_BAR_SYNC_DEFER_LIMIT`); holding the *scroll region* back is not
///   cosmetic, because until DECSTBM is reasserted the host is scrolling a
///   region sized for the old terminal. An injection at a genuine escape
///   boundary is transparent anyway, so the escape boundary is the whole
///   requirement here.
/// - **A deadline** (`BoundaryPolicy::PastDeadline`, the client's only
///   exemption), because an undelivered resize is worse than a spliced one.
///
/// What is *not* deferred is the workload's own notification: the resize
/// poller's `AttachControl::Resize` goes to the worker unconditionally, so
/// the PTY is resized and SIGWINCH delivered on time no matter what the
/// host-side reservation is doing. A workload blocked on the new size never
/// waits on this gate -- only the client's own row reservation does.
///
/// Returns whether bytes actually reached the terminal. A deferral is
/// recorded in `ctx.pending_layout` and flushed by `flush_pending_layout`;
/// see that function for why deferring here never loses a resize.
fn apply_terminal_layout(ctx: &StatusBarCtx, rows: u16, cols: u16) -> bool {
    // The initial layout is followed by the attach snapshot, so repainting it
    // here would only draw a frame that the snapshot immediately replaces.
    // Every later layout change needs a full repaint: the old status row is
    // outside the new modeled screen and otherwise remains visible above the
    // newly targeted bottom row.
    let had_layout = ctx.term.lock().unwrap_or_else(PoisonError::into_inner).rows != 0;
    let wrote = {
        let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
        apply_terminal_layout_to(&mut *out, ctx, rows, cols)
    };
    if had_layout && wrote {
        redraw_live_screen_after_layout(ctx);
    }
    wrote
}

/// `apply_terminal_layout` with the destination passed in, the stdout lock
/// already held by the caller.
///
/// Split out for the reason `status_bar_redraw` exists: a test can drive the
/// real gate, and the real bytes, into a `vt100` host terminal without
/// redirecting the process's fd 1 out from under a concurrently-running test
/// harness. Production has exactly one caller pair -- `apply_terminal_layout`
/// and `flush_pending_layout` -- and both hold the lock across it, because
/// the boundary check and the write must not be separable.
fn apply_terminal_layout_to(
    out: &mut impl Write,
    ctx: &StatusBarCtx,
    rows: u16,
    cols: u16,
) -> bool {
    let reserved = rows > 2;
    // The deadline is a clock rather than stream state, so unlike the
    // boundary check it cannot go stale under the lock.
    let policy = match layout_deferred_since(ctx) {
        Some(since) if since.elapsed() >= LAYOUT_DEFER_LIMIT => BoundaryPolicy::PastDeadline,
        _ => BoundaryPolicy::Defer,
    };
    let wrote = {
        let restore = ctx
            .screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cursor_restore();
        let seq = terminal_layout_sequence(rows, &restore);
        write_client_locked(out, &ctx.screen, &seq, policy)
    };
    {
        // Park or clear the deferral. Re-parking keeps the *original*
        // deadline, so a stream that never reaches a boundary cannot
        // postpone delivery indefinitely by resizing again; the geometry is
        // overwritten, because only the latest physical size is correct.
        let mut pending = ctx
            .pending_layout
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        *pending = if wrote {
            None
        } else {
            let since = pending.map(|p| p.since).unwrap_or_else(Instant::now);
            Some(PendingLayout { rows, cols, since })
        };
    }
    // Recorded even when the bytes were deferred, and deliberately so: the
    // physical terminal has *already* changed size, so the status bar must
    // start targeting the new last row immediately or it draws over a row
    // the workload now owns. `TermGeom` is internal state, not output --
    // recording it puts nothing on the wire, and the bar's own write is
    // independently gated. `term` is taken under `stdout`, which is the
    // order `write_locked` documents.
    if let Ok(mut g) = ctx.term.lock() {
        *g = TermGeom {
            rows,
            cols,
            reserved,
        };
    }
    wrote
}

/// When the currently-parked resize was *first* held back, if there is one.
fn layout_deferred_since(ctx: &StatusBarCtx) -> Option<Instant> {
    ctx.pending_layout
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .map(|p| p.since)
}

/// Delivers a resize whose DECSTBM was deferred by the boundary gate.
///
/// Deferring must never mean dropping (issue #14): a lost resize leaves the
/// host scrolling a region sized for the old terminal for the rest of the
/// attach. Two independent callers guarantee delivery, and they cover
/// disjoint failure modes:
///
/// - the main frame loop, after every relayed chunk -- the boundary the gate
///   was waiting for is by construction reached by relaying more bytes, so
///   this is the normal path and it fires within one PTY chunk;
/// - the status-bar thread's tick, every `STATUS_BAR_POLL_INTERVAL` -- the
///   frame loop only runs when the workload sends something, so a workload
///   that stops mid-sequence would otherwise park the resize forever. This
///   is also what makes `LAYOUT_DEFER_LIMIT` actually fire.
///
/// Peeks rather than takes: `apply_terminal_layout` clears the slot when it
/// writes and re-parks it (keeping the original deadline) when it cannot, so
/// a flush that loses the race with a still-unsafe stream does not drop the
/// resize on the floor.
fn flush_pending_layout(ctx: &StatusBarCtx) -> bool {
    // Cheap pre-check, before the stdout lock. The frame loop calls this
    // after *every* PTY chunk and there is almost never a resize parked, so
    // the common case must not queue behind the status thread's redraw for
    // nothing. Released before `stdout` is taken, so this adds no nesting to
    // the lock order; the authoritative read happens under the lock below.
    if layout_deferred_since(ctx).is_none() {
        return false;
    }
    let wrote = {
        let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
        flush_pending_layout_to(&mut *out, ctx)
    };
    if wrote {
        redraw_live_screen_after_layout(ctx);
    }
    wrote
}

/// `flush_pending_layout` with the destination passed in and the stdout lock
/// already held. Calls `apply_terminal_layout_to`, never
/// `apply_terminal_layout`: the lock is not reentrant.
fn flush_pending_layout_to(out: &mut impl Write, ctx: &StatusBarCtx) -> bool {
    let pending = *ctx
        .pending_layout
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    match pending {
        Some(PendingLayout { rows, cols, .. }) => apply_terminal_layout_to(out, ctx, rows, cols),
        None => false,
    }
}

/// Clear and repaint the live host frame after a physical resize has been
/// applied. A layout change moves the bar to a new row, but escape sequences
/// cannot erase the old row by themselves; the full snapshot is what removes
/// the stale copy that would otherwise look like a second status bar.
///
/// Modal painters already own resize repainting. They deliberately keep this
/// helper live-only so a resize does not replace a pager or key overlay with
/// the workload's screen.
fn redraw_live_screen_after_layout(ctx: &StatusBarCtx) -> bool {
    if ctx.scroll.is_active() || ctx.overlay.is_active() {
        return false;
    }
    redraw_live_screen(ctx)
}

/// Writes a live PTY chunk to the terminal, after feeding it through the
/// client's own model of the workload's screen (`ClientScreen`).
///
/// The model is what makes the status bar safe to inject at all: it knows
/// where the workload's cursor and pen actually are, whether the relayed
/// stream is currently between complete escape sequences, and whether the
/// workload is part-way through a synchronized-output frame. It can also
/// rewrite the chunk -- the one case being the reserved-row walk, see
/// `ClientScreen::relay`.
///
/// The worker already pays the identical parse cost per chunk
/// (docs/terminal-state-design.md section 9's steady-state parse budget);
/// paying it a second time in the client is the price of the client no longer
/// writing blind into someone else's byte stream.
fn relay_to_terminal(
    screen: &Arc<Mutex<aplexer::screen::ClientScreen>>,
    stdout: &Arc<Mutex<io::Stdout>>,
    scroll: &Arc<ScrollMode>,
    overlay: &Arc<KeyOverlay>,
    data: &[u8],
) -> io::Result<()> {
    let mut out = stdout.lock().unwrap_or_else(PoisonError::into_inner);
    {
        let mut s = screen.lock().unwrap_or_else(PoisonError::into_inner);
        let rewritten = s.relay(data);
        // A client modal -- the pager, or the `Ctrl-b` key overlay -- owns
        // the screen: the model still consumes every byte (that is what grows
        // the retained history the user is reading, and what makes the
        // repaint on the way out show everything that arrived meanwhile) but
        // nothing reaches the host. Checked here, under the same stdout lock
        // `enter_scroll_mode` and `show_key_overlay` flip their flags under,
        // so a chunk can never be half-written across a modal's first frame.
        // Type-through is the one exception: while `i` has handed the
        // keyboard over, the pager keeps only the bar row and the offset,
        // and the stream flows -- typing with no echo would be worse than
        // the reading view the user chose to give up. Esc takes it back.
        if (scroll.is_active() && !scroll.is_typing()) || overlay.is_active() {
            return Ok(());
        }
        let src = rewritten.as_deref().unwrap_or(data);
        if let Some(filtered) = s.filter_host(src) {
            out.write_all(&filtered)?;
        } else {
            out.write_all(src)?;
        }
    }
    out.flush()
}

/// Writes bytes the client is emitting verbatim -- the attach snapshot, a
/// switch's replayed screen -- and feeds them to the model under the *same*
/// stdout lock, so a concurrent status redraw can never see a model that is
/// ahead of what the terminal has actually been sent.
fn feed_and_write(
    stdout: &Arc<Mutex<io::Stdout>>,
    screen: &Arc<Mutex<aplexer::screen::ClientScreen>>,
    prefix: &[u8],
    payload: &[u8],
    reset_to: Option<(u16, u16)>,
) -> io::Result<()> {
    let mut out = stdout.lock().unwrap_or_else(PoisonError::into_inner);
    {
        let mut s = screen.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((rows, cols)) = reset_to {
            s.reset(rows, cols);
        }
        s.feed(payload);
        if let Some(filtered) = s.filter_host(prefix) {
            out.write_all(&filtered)?;
        } else {
            out.write_all(prefix)?;
        }
        if let Some(filtered) = s.filter_host(payload) {
            out.write_all(&filtered)?;
        } else {
            out.write_all(payload)?;
        }
    }
    out.flush()
}

/// Undoes `apply_terminal_layout` and clears the screen, exactly like tmux
/// does on detach (Ctrl-b d) -- otherwise whatever was last drawn (including
/// the status bar) just sits in the user's terminal after attach() returns.
/// `\x1b[2J\x1b[H` (full clear + cursor home) is used rather than a fuller
/// reset (`\x1bc`) because it doesn't disturb terminal scrollback history.
const TERMINAL_RESET_SEQUENCE: &[u8] = b"\
\x1b[?1049l\
\x1b[?1007h\
\x1b>\
\x1b[?1l\
\x1b[?2004l\
\x1b[?9l\
\x1b[?1000l\
\x1b[?1002l\
\x1b[?1003l\
\x1b[?1005l\
\x1b[?1006l\
\x1b[r\
\x1b[0m\
\x1b[2J\
\x1b[H\
\x1b[?25h";

/// Written once at attach start, before layout or the snapshot. Isolates the
/// live session from the host's primary-screen scrollback (the `a` list), and
/// stops the host translating the mouse wheel into arrow keys.
///
/// `?1049h` alone created a second bug: the alternate screen has no
/// scrollback, so a terminal with xterm's `alternateScroll` (DECSET 1007,
/// on by default nearly everywhere) answers a wheel event by *synthesizing
/// cursor-up/down key presses* and sending them to the workload. Inside an
/// agent TUI that is not merely useless -- the wheel silently walks the
/// agent's own menus and prompt history, i.e. scrolling to read types input
/// into the session. `?1007l` turns that translation off for the duration of
/// the attach, so the wheel does nothing instead of something destructive.
/// `reset_terminal` restores it on detach, since the mode is terminal-global
/// and the user's next `less`/`vim` expects the default back.
const ATTACH_ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h\x1b[?1007l";

fn reset_terminal(stdout: &Arc<Mutex<io::Stdout>>) {
    // `\x1b[?1049l` first (docs/terminal-state-design.md section 6.3): the
    // attach client holds the host on the alternate screen for the whole
    // session (see `ATTACH_ALT_SCREEN_ENTER`) so the pre-attach primary
    // scrollback -- typically the `a` session list -- cannot mix into the
    // live view. Detach must return the host to that primary screen.
    // Workload-originated 1049l is stripped from the relay and never
    // reaches the host; this write is the one exit that does.
    //
    // The snapshot path also reproduces every input mode tracked by vt100.
    // Disable all of their possible variants unconditionally: application
    // keypad/cursor, bracketed paste, the four mouse protocols, and both
    // non-default mouse encodings. Sending the resets is harmless when a
    // mode was already off and avoids leaving the user's shell consuming
    // application key or mouse reports after any attach exit.
    //
    // `\x1b[?25h` (DECTCEM show cursor) is included unconditionally: a
    // full-screen TUI in the workload (htop, vim, an agent CLI's spinner,
    // ...) commonly hides the cursor with `\x1b[?25l` while it owns the
    // screen and relies on its own exit path to show it again -- but that
    // exit path runs on the *workload's* side, and detaching doesn't wait
    // for or depend on it. Without this, a detach can leave the user's real
    // terminal with an invisible cursor after the workload's last draw
    // happened to hide it. Showing an already-visible cursor is a no-op, so
    // this is safe to send regardless of what state the workload (or our
    // own status-bar redraw, which never hides the cursor) left it in.
    let _ = write_locked(stdout, TERMINAL_RESET_SEQUENCE);
}

/// RAII guard that runs `reset_terminal` on every exit path out of attach()
/// -- explicit Ctrl-b d detach, the remote session exiting, a connection
/// error, or an early `?` return -- so a new exit path added later can't
/// forget the cleanup. Constructed whenever stdout is a tty, independently
/// of whether stdin is interactive; `RawMode` remains stdin-specific.
struct TerminalUiGuard {
    stdout: Arc<Mutex<io::Stdout>>,
}
impl Drop for TerminalUiGuard {
    fn drop(&mut self) {
        reset_terminal(&self.stdout);
    }
}

fn format_bytes(bytes: u64) -> String {
    const KI: u64 = 1024;
    const MI: u64 = KI * 1024;
    const GI: u64 = MI * 1024;
    if bytes >= GI {
        format!("{:.1}G", bytes as f64 / GI as f64)
    } else if bytes >= MI {
        format!("{:.0}M", bytes as f64 / MI as f64)
    } else if bytes >= KI {
        format!("{:.0}K", bytes as f64 / KI as f64)
    } else {
        format!("{bytes}B")
    }
}

/// One `Operation::Status` round-trip per status-bar redraw, shared by the
/// memory and foreground-command indicators below so a single bar refresh
/// costs one worker round-trip, not one per indicator. `None` on any RPC
/// failure (worker briefly unreachable) -- every indicator built from this
/// just degrades to "omitted" in that case, same as before this was
/// shared.
fn live_status(record: &SessionRecord) -> Option<Value> {
    rpc_simple(record, Operation::Status, None).ok()
}

/// The attached session's record as the state derivation should see it.
///
/// `ctx.record` is a snapshot from attach/switch time, but state-report
/// pushes land in the worker's in-memory record (and on disk) with no event
/// reaching the attached client -- deriving the state from the snapshot
/// alone would trust a push that is minutes old and miss every push made
/// after attach, which is exactly the "agent started working while I
/// watched" case the spinner exists for. The Status answer already
/// serializes the worker's live record (`public_session_record`), so
/// overlay its reported-state pair and its activity stamp onto the
/// snapshot: the activity stamp is half of the `idle` push's validity rule
/// (`watch::fresh_reported_state` retracts a resting push once newer PTY
/// output appears), so deriving from the attach-time stamp would judge
/// every post-attach rest against pre-attach output -- an agent that went
/// back to work after attach would keep its stale `idle` claim forever
/// from the bar's point of view. A missing field (older worker) or a
/// failed RPC (`raw` None) leaves the snapshot untouched, same degradation
/// as the memory indicator.
fn overlay_reported_state(record: &SessionRecord, raw: Option<&Value>) -> SessionRecord {
    let mut fresh = record.clone();
    let Some(raw) = raw else {
        return fresh;
    };
    if let Some(s) = raw.get("reported_state").and_then(Value::as_str) {
        fresh.reported_state = Some(s.to_string());
    }
    if let Some(ms) = raw.get("reported_state_at_ms").and_then(Value::as_u64) {
        fresh.reported_state_at_ms = Some(ms);
    }
    if let Some(ms) = raw.get("last_activity_ms").and_then(Value::as_u64) {
        fresh.last_activity_ms = Some(ms);
    }
    fresh
}

/// Live memory indicator from the session's cgroup, if it has one -- a
/// small "useful for our application" touch given aplexer's whole reason
/// for existing is resource-isolated agent sessions. Best-effort: absence
/// of cgroup stats in `raw` (no cgroup configured) just omits the
/// indicator rather than disrupting the status bar.
fn memory_indicator(record: &SessionRecord, raw: &Value) -> Option<String> {
    let current = raw.get("cgroup")?.get("memory_current")?.as_u64()?;
    let used = format_bytes(current);
    Some(match record.limits.memory_bytes {
        Some(max) => format!("{used}/{}", format_bytes(max)),
        None => used,
    })
}

/// Plain interactive shells: showing e.g. `[shell -> bash]` for an ordinary
/// shell session would be redundant noise (that's what `shell` already
/// means), not information. Only an actually interesting foreground
/// program -- something manually run inside the session that isn't just
/// its own shell -- is worth surfacing.
const PLAIN_SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "fish", "ksh", "tcsh", "csh"];

/// The live foreground-command override for the status bar, if there's
/// anything worth showing beyond `record.engine` alone (see
/// `foreground_command` in lib.rs and `Operation::Status`'s worker-side
/// handler for where `raw["foreground_command"]` comes from -- a live,
/// never-persisted read of the pty's current foreground process, the same
/// mechanism tmux uses for `pane_current_command`). `None` when: the
/// worker didn't report one (RPC failure, no foreground process group
/// yet); it's a bare interactive shell (`PLAIN_SHELLS`); or it's just the
/// engine's own launch command running as expected (e.g. a `codex`-engine
/// session actually running `codex` shouldn't redundantly show
/// `[codex -> codex]`).
fn foreground_override(record: &SessionRecord, raw: &Value) -> Option<String> {
    let fg = raw.get("foreground_command")?.as_str()?;
    if PLAIN_SHELLS.contains(&fg) {
        return None;
    }
    let launched = record
        .command
        .first()
        .and_then(|c| Path::new(c).file_name())
        .and_then(|n| n.to_str());
    if launched == Some(fg) {
        return None;
    }
    Some(fg.to_string())
}

/// The detected agent's display name when it adds information beyond the
/// declared engine, `None` when it doesn't. One display rule for every
/// human surface (list rows, `a status`, the attach status bar): a session
/// declared `engine: "claude"` that is running claude says "claude" once;
/// a `shell` session running claude, or a `claude` session someone started
/// codex inside, gets the detected name appended. The engine compares by
/// family (`engine_family`): a `zcodex`-engine session running
/// zcodex says codex once, because zcodex is a codex variant, not a second
/// agent.
fn extra_agent_label(
    record: &SessionRecord,
    detected: Option<aplexer::agent_kind::AgentKind>,
) -> Option<&'static str> {
    let agent = detected?;
    (agent.name() != aplexer::engine_family(&record.engine)).then_some(agent.name())
}

/// The list/status engine cell. A plain `shell` workload that detection
/// found an agent inside is labeled by the agent alone: `shell` is the
/// absence of a choice, so `shell -> codex` spent the column on noise when
/// `codex` is the fact. A declared engine keeps the `engine -> agent` form,
/// where the base carries real information (a claude session someone
/// started codex inside).
fn engine_label(
    record: &SessionRecord,
    detected: Option<aplexer::agent_kind::AgentKind>,
) -> String {
    let agent = extra_agent_label(record, detected);
    if record.engine == "shell" {
        if let Some(agent) = agent {
            return agent.to_string();
        }
    }
    let base = match &record.profile {
        Some(profile) => format!("{}/{}", record.engine, profile),
        None => record.engine.clone(),
    };
    match agent {
        Some(agent) => format!("{base} -> {agent}"),
        None => base,
    }
}

/// `{i}:{tag}[*][({state})]` for every session in the current workspace,
/// mirroring how `a list`'s tree groups sessions by workspace (see
/// `group_by_workspace`) -- a live glance at what else is running here
/// without detaching, and (unlike the old `sibling_summary` it replaces)
/// self-documenting: `i` is exactly the number `Ctrl-b 1`..`9` jumps to
/// (`pick_switch_target`'s `Index` arm), because both walk the same
/// `list_records` order (`Reverse(created_at_ms)`) that `group_by_workspace`
/// preserves within a group -- see the equivalence note on
/// `resolve_quick_index`. `*` marks the currently attached session;
/// `(state)` is appended only when the state is not "running" (the common
/// case needs no label). Lists **all** sessions including the current one
/// (the old version listed only "the others") because the numbering only
/// makes sense as a complete index. Example: `1:main* 2:review
/// 3:build(broken)`. A single-session workspace omits the segment (empty
/// string), same as before.
fn workspace_summary(ctx: &StatusBarCtx, record: &SessionRecord) -> String {
    let records = match list_records(&ctx.paths) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let siblings: Vec<SessionRecord> = records
        .into_iter()
        .filter(|r| r.workspace == record.workspace)
        .collect();
    if siblings.len() <= 1 {
        return String::new();
    }
    siblings
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let (state, _) = session_ui_state(r, now_ms());
            let mut part = format!("{}:{}", i + 1, r.tag);
            if r.id == record.id {
                part.push('*');
            }
            // Running-ish states are the expected background; anything else
            // (a reported wait, a death, a broken worker) is worth seeing
            // while attached. The same rule `workspace_summary_regions`
            // mirrors for the click map.
            if !matches!(state, "running" | "working" | "active" | "quiet") {
                part.push_str(&format!("({state})"));
            }
            part
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Makes plain status-bar data safe to interpolate into terminal output.
/// Session records and transient errors can contain arbitrary persisted or
/// remote text; C0/C1 controls (including ESC, BEL, CR, and LF) must never be
/// allowed to become terminal instructions when the bar is drawn.
fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect()
}

fn terminal_display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Pads or truncates to exactly `cols` terminal display cells without
/// splitting an extended grapheme cluster. This keeps wide glyphs, combining
/// sequences, and emoji aligned while the reverse-video bar spans the full
/// terminal width like tmux's own.
fn pad_or_truncate(text: &str, cols: usize) -> String {
    let cols = cols.max(1);
    let mut rendered = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = terminal_display_width(grapheme);
        if grapheme_width > cols.saturating_sub(width) {
            break;
        }
        rendered.push_str(grapheme);
        width += grapheme_width;
    }
    rendered.push_str(&" ".repeat(cols - width));
    rendered
}

/// Everything a status-bar redraw needs, cloned into each thread that might
/// trigger one (status thread, input thread on a switch flash, main loop
/// after a switch) instead of five loose `Arc` parameters -- see
/// docs/fast-session-switching-design.md section 3. `record` is shared and
/// swappable so an in-process switch is visible to the bar without
/// respawning the thread; `flash` is a transient error line (switch
/// failures); `last_drawn` backs the dirty-check in `draw_status_bar`.
#[derive(Clone)]
struct StatusBarCtx {
    stdout: Arc<Mutex<io::Stdout>>,
    term: Arc<Mutex<TermGeom>>,
    paths: Paths,
    record: Arc<Mutex<SessionRecord>>,
    flash: Arc<Mutex<Option<(String, Instant)>>>,
    /// (text, rows, cols, workload margins) last actually written, so an
    /// unchanged bar isn't rewritten every debounce tick -- see
    /// `draw_status_bar`'s doc comment and
    /// docs/low-bandwidth-remote-access-design.md section 2.1.
    last_drawn: Arc<Mutex<LastDrawnStatus>>,
    /// The client's own live model of the *workload's* screen, fed every PTY
    /// byte this client writes to the terminal (including the attach
    /// snapshot, which is a full repaint of that screen per
    /// docs/terminal-state-design.md section 6.2). It answers the three
    /// questions a status-bar redraw has to answer before it may write
    /// anything at all:
    ///
    /// - *May I write here?* -- `at_escape_boundary()`. The relayed stream
    ///   must be between complete escape sequences and complete characters.
    ///   A PTY read boundary is not one of those by construction, which is
    ///   how the redraw used to land inside a workload's half-emitted
    ///   `\x1b[38;5;` and turn its remaining parameter bytes into literal
    ///   text (issue #5).
    /// - *Where do I put the cursor back?* -- `cursor_restore()`. Absolutely,
    ///   from the model, instead of through the single shared DECSC register
    ///   the workload also owns.
    /// - *Which scroll region should be in force?* -- `margins()`, the same
    ///   distinction the previous `MarginTracker`-only field existed for:
    ///   re-asserting `\x1b[1;{rows-1}r` unconditionally destroys a
    ///   workload's own sub-range, including the one the attach snapshot just
    ///   restored.
    screen: Arc<Mutex<aplexer::screen::ClientScreen>>,
    /// Set when a redraw was wanted but the stream was not at a safe boundary
    /// (or was inside a synchronized-output frame). The main frame loop
    /// flushes it at the first boundary that is safe, so deferring never
    /// means dropping.
    pending: Arc<AtomicBool>,
    /// Set when `Ctrl-b r` wanted a full live-screen repaint but the stream
    /// was not at a safe boundary. Flushed by the main frame loop the same
    /// way as `pending`; a successful refresh also redraws the status bar,
    /// so it subsumes a pending bar redraw.
    pending_refresh: Arc<AtomicBool>,
    /// The physical geometry a terminal resize wanted to reserve a row out
    /// of, parked here because the relayed stream was mid-escape-sequence
    /// when the resize poller fired (issue #14). Flushed by
    /// `flush_pending_layout` from both the frame loop and the status
    /// thread, so a deferred resize is delivered late, never dropped.
    pending_layout: Arc<Mutex<Option<PendingLayout>>>,
    /// When the current synchronized-output deferral started, so
    /// `STATUS_BAR_SYNC_DEFER_LIMIT` can bound it.
    sync_deferred_since: Arc<Mutex<Option<Instant>>>,
    /// Scroll mode (`Ctrl-b [`, or a wheel roll): whether the pager is up
    /// and where in the retained history it is looking. Read by the relay on
    /// every chunk to decide whether the host may be written to at all.
    scroll: Arc<ScrollMode>,
    /// The which-key overlay: whether the `Ctrl-b` keymap is currently drawn
    /// over the screen. Read by the relay on every chunk for the same reason
    /// `scroll` is -- while a modal owns the host, the model keeps eating
    /// bytes and the terminal is written nothing.
    overlay: Arc<KeyOverlay>,
    /// Who currently owns mouse reporting on the host: `Some(true)` this
    /// client (so the wheel reaches `a`), `Some(false)` the workload,
    /// `None` nothing asserted yet. See `sync_client_mouse`.
    mouse_owned: Arc<Mutex<Option<bool>>>,
    /// Whether borrowing the mouse is permitted at all (`APLEXER_MOUSE`).
    mouse_capture: bool,
}

type LastDrawnStatus = Option<(String, u16, u16, Option<(u16, u16)>)>;

/// How long a transient status-bar message (switch failure, attach hint,
/// `Ctrl-b ?` help) stays visible before the normal text resumes
/// (docs/fast-session-switching-design.md section 6.1). Three seconds
/// rather than two: help text has to be readable, not merely noticed.
const FLASH_DURATION: Duration = Duration::from_secs(3);

/// One attach-mode chord, as every rendering of it needs it.
///
/// The keymap is defined **once**, here. Three things render it -- the
/// `Ctrl-b ?` status-bar flash (`attach_key_help`, from `brief`), the
/// `a keys`/`a hotkeys` listing (`cmd_hotkeys`, from `keys` + `description`)
/// and the which-key overlay a held `Ctrl-b` raises (`key_overlay_lines`,
/// from the same two) -- and none of them holds a string of its own, so a
/// binding can no longer be changed in the scanner and updated in only some
/// of the places that document it. (It used to be two hand-maintained lists
/// with a comment asking future editors to keep them in sync.) Anything else
/// that has to show the keymap reads this table too rather than adding
/// another copy; if it needs something the table does not carry, the field
/// belongs here.
struct AttachBinding {
    /// The keys, as the `a keys` listing's left column shows them.
    keys: &'static str,
    /// `key label` for the one-line status-bar flash, which has a terminal
    /// width to live inside; `None` keeps a binding out of that line only.
    /// Order here is the order shown, and the flash is truncated from the
    /// right, so the entries most worth seeing on an 80-column terminal come
    /// first.
    brief: Option<&'static str>,
    /// The sentence `a keys` prints.
    description: &'static str,
}

const ATTACH_BINDINGS: &[AttachBinding] = &[
    AttachBinding {
        keys: "Right / Left",
        brief: Some("←/→ session"),
        description: "next / previous session in this workspace",
    },
    AttachBinding {
        keys: "Down / Up",
        brief: Some("↑/↓ workspace"),
        description: "next / previous workspace (at its most recent session)",
    },
    AttachBinding {
        keys: "n",
        brief: Some("n new"),
        description: "create another session in this workspace and switch to it",
    },
    AttachBinding {
        keys: "d",
        brief: Some("d detach"),
        description: "detach (the workload keeps running)",
    },
    AttachBinding {
        keys: "[",
        brief: Some("[ scroll"),
        description: "scroll back through this session's output (i types, q or Esc leaves)",
    },
    AttachBinding {
        keys: "N / P",
        brief: Some("N/P global"),
        description: "next / previous session across all workspaces",
    },
    AttachBinding {
        keys: "1-9",
        brief: Some("1-9 jump"),
        description: "jump to the numbered session in the status bar",
    },
    AttachBinding {
        keys: "l",
        brief: Some("l last"),
        description: "return to the previously attached session",
    },
    AttachBinding {
        keys: "r",
        brief: Some("r redraw"),
        description: "redraw the live screen (recover a garbled display)",
    },
    AttachBinding {
        keys: "?",
        brief: Some("? help"),
        description: "show this reference in the status bar",
    },
];

/// The one-line key reference `Ctrl-b ?` flashes onto the status bar --
/// the same chords `a keys`/`a hotkeys` print, compressed to what fits a
/// terminal line (and truncated by the bar renderer when it does not).
/// Consumed locally: no byte reaches the workload.
fn attach_key_help() -> String {
    let brief: Vec<&str> = ATTACH_BINDINGS.iter().filter_map(|b| b.brief).collect();
    format!("Ctrl-b: {}", brief.join(" · "))
}

/// Shows a transient message on the status bar and redraws immediately --
/// the single channel for attach hints, help, and switch failures, so
/// nothing is ever printed into the workload's output stream (the original
/// attach banner's corruption failure mode, docs/terminal-state-design.md
/// section 6.3 step 6).
fn flash_status(ctx: &StatusBarCtx, message: impl Into<String>) {
    if let Ok(mut flash) = ctx.flash.lock() {
        *flash = Some((message.into(), Instant::now()));
    }
    draw_status_bar(ctx, true);
}

/// Status-bar text, adaptive by width. All layouts lead with identity and
/// state -- the two things a returning human needs -- and drop detail from
/// the right as the terminal narrows: full (workspace:tag, state, detected
/// agent, engine/foreground, memory, sibling list, help affordance), medium
/// (tag-first), compact (tag + state + detected agent + help), and a minimum
/// that keeps state and `^b ?` alive on even a few columns. Renders a flashed
/// message instead of all of these while one is active (section 6.1).
fn status_bar_text(ctx: &StatusBarCtx, cols: usize) -> String {
    {
        let mut flash = ctx.flash.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((msg, at)) = flash.clone() {
            if at.elapsed() < FLASH_DURATION {
                return pad_or_truncate(&sanitize_terminal_text(&format!("[{msg}]")), cols);
            }
            *flash = None;
        }
    }
    let record = ctx
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let home = env::var_os("HOME").map(PathBuf::from);
    let ws = display_workspace(&record.workspace, home.as_deref());
    let mut ep = match &record.profile {
        Some(p) => format!("{}/{}", record.engine, p),
        None => record.engine.clone(),
    };
    let raw = live_status(&record);
    // Which agent is live in this session right now -- the same query-time
    // detection every JSON surface carries (`api::record_agent`): one walk
    // of this session's own, shallow process tree per bar refresh, cheap
    // next to the Status round-trip the bar already pays. When it names the
    // same program as the live foreground read, the foreground annotation
    // steps aside -- `claude  shell -> claude` would say claude twice -- so
    // an agent not in the foreground (claude running, vim in front) shows
    // both facts: `claude  shell -> vim`.
    let agent = extra_agent_label(&record, aplexer::api::record_agent(&record));
    let foreground = raw
        .as_ref()
        .and_then(|raw| foreground_override(&record, raw))
        .filter(|fg| Some(fg.as_str()) != agent);
    if let Some(fg) = foreground {
        ep.push_str(&format!(" -> {fg}"));
    }
    let agent_segment = agent.map(|name| format!("  {name}")).unwrap_or_default();
    let mem = raw.as_ref().and_then(|raw| memory_indicator(&record, raw));
    let siblings = workspace_summary(ctx, &record);
    let state_record = overlay_reported_state(&record, raw.as_ref());
    let now = now_ms();
    let (state_word, _) = session_ui_state(&state_record, now);
    let (glyph, _) = state_glyph(state_word);
    // Agent-busy states animate: the static dot is replaced by the current
    // braille frame, and the bar starts moving (see the status thread's
    // animation tick, which is what makes redraws actually happen at the
    // frame rate even when the PTY itself is quiet).
    let glyph = match spinner_frame(state_word, now) {
        Some(frame) => frame.to_string(),
        None => glyph.to_string(),
    };
    let state = format!("{glyph} {}", state_word.to_uppercase());

    let mut full = format!("{ws}:{}  {state}{agent_segment}  {ep}", record.tag);
    if let Some(mem) = &mem {
        full.push_str(&format!("  mem {mem}"));
    }
    if !siblings.is_empty() {
        full.push_str("  |  ");
        full.push_str(&siblings);
    }
    full.push_str("  |  ^b ?");

    let mut medium = format!("{}  {state}{agent_segment}  {ep}", record.tag);
    if !siblings.is_empty() {
        medium.push_str("  |  ");
        medium.push_str(&siblings);
    }
    medium.push_str("  |  ^b ?");

    let compact = format!("{}  {state}{agent_segment}  ^b ?", record.tag);
    let minimum = format!("{state}  ^b ?");

    let rendered = [full, medium, compact]
        .into_iter()
        .map(|candidate| sanitize_terminal_text(&candidate))
        .find(|candidate| terminal_display_width(candidate) <= cols)
        .unwrap_or_else(|| sanitize_terminal_text(&minimum));
    pad_or_truncate(&rendered, cols)
}

/// Redraws the reserved bottom row in place: jump to the last row, clear it,
/// draw the (reverse-video, full-width) status line, and put the workload's
/// cursor and pen back absolutely from the client's own screen model. No-ops
/// when the current terminal is too small to have a reserved row.
///
/// Four properties, each load-bearing:
///
/// - **Only writes at a safe boundary.** The relayed stream must be between
///   complete escape sequences and complete characters
///   (`ClientScreen::at_escape_boundary`), and preferably not inside a
///   workload's synchronized-output frame (`sync_defer`). A PTY read boundary
///   is neither of those by construction: measured on a real `a attach`
///   against a continuously-streaming full-screen TUI, 5 of 10 redraws landed
///   inside an unterminated CSI sequence, whose remaining parameter bytes the
///   host then printed as literal text into the workload's frame. When the
///   stream is not safe the redraw is *deferred*, not dropped -- `ctx.pending`
///   is flushed by the main frame loop at the next boundary.
/// - **Never touches the shared save-cursor register.** See
///   `status_bar_sequence`.
/// - **Dirty-checked**: skips the write entirely when the rendered text and
///   geometry are byte-identical to the last actual write (`ctx.last_drawn`).
///   An idle session's bar is naturally quantized (memory rounds to whole
///   units, sibling states rarely change), so this removes nearly all idle
///   redraw chatter with no behavior change when something *did* change.
///   See docs/low-bandwidth-remote-access-design.md section 2.1.
/// - **Defensively reasserts the DECSTBM scroll region** every time it
///   actually writes. A full-screen TUI switching to the alternate screen
///   buffer, or resetting margins itself before laying out its own UI, can
///   silently undo the reservation outside our control; the resize-poll
///   thread only reapplies it when the physical terminal *size* changes, so
///   a clobbered margin would otherwise stay clobbered for the rest of the
///   attach. Reasserting it here means the reservation self-heals within one
///   redraw cycle instead of being lost permanently. Which region gets
///   reasserted is `ClientScreen::margins`-aware -- see `status_bar_sequence`
///   for why reasserting `1;{rows-1}` unconditionally is a bug, reproduced
///   directly as a workload holding `\x1b[5;15r` rendering
///   `SCROLLER-70M-ROW-16` over its own fixed row 16.
///
/// `force`: bypass the dirty-check and write unconditionally. The
/// dirty-check alone would let a *clobbered margin* go unrepaired
/// indefinitely during a long idle stretch where the bar's *text* never
/// changes (nothing to detect); callers that need the margin-defense
/// guarantee to actually bound in time -- the status thread's own
/// `STATUS_BAR_MAX_INTERVAL` forced tick, and every switch/flash redraw,
/// which are already low-frequency, user-triggered events where bandwidth
/// isn't the concern -- pass `true`. `force` does **not** bypass the boundary
/// gate: nothing does, because writing at an unsafe point is the bug.
///
/// Returns whether a real write to the terminal happened (`false` when the
/// reserved row doesn't exist, the redraw was deferred, or the dirty-check
/// skipped an unchanged redraw). Callers that drive
/// `STATUS_BAR_MAX_INTERVAL`'s overdue timer must only reset it on `true` --
/// resetting on a dirty-check no-op would let a workload with
/// frequent-but-unchanging redraws (a spinner, streamed tokens with pauses)
/// keep the timer perpetually "recently fired" without ever actually
/// rewriting a margin a full-screen erase clobbered, breaking the self-heal
/// guarantee this constant exists for.
fn draw_status_bar(ctx: &StatusBarCtx, force: bool) -> bool {
    // The stdout lock is taken *before* `status_bar_redraw` consults the
    // client's terminal model, and held across the write. Checking the escape
    // boundary and then writing without the lock is a race the frame loop
    // wins about 10% of the time (measured: 4 of 39 redraws in a real capture
    // still landed mid-CSI) -- it writes another chunk in between, and the
    // "safe" answer the status thread got is stale by the time its bytes go
    // out. See `write_locked` for the lock order this relies on.
    // Cheap pre-gate, before anything is rendered. The main frame loop calls
    // this after *every* PTY chunk while a redraw is pending, and rendering
    // the bar text reads session records off disk -- doing that per chunk
    // under a streaming workload is a throughput cliff. The authoritative
    // check is the one inside `status_bar_redraw_locked`, which runs under
    // the stdout lock; this one only avoids the work when the answer is
    // already known to be "not here".
    {
        let (at_boundary, in_sync) = {
            let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
            (screen.at_escape_boundary(), screen.in_synchronized_update())
        };
        if !at_boundary || sync_defer(ctx, in_sync) {
            ctx.pending.store(true, Ordering::Relaxed);
            return false;
        }
    }
    let Some((geom, text)) = status_bar_render(ctx) else {
        return false;
    };
    let mut out = ctx
        .stdout
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match status_bar_redraw_locked(ctx, geom, &text, force) {
        Some(seq) => {
            // `status_bar_redraw_locked` already refused at an unsafe
            // boundary, so this can only say no if the stream moved under a
            // lock nothing else can hold -- but the funnel is where the
            // guarantee lives, not in each caller remembering, so the
            // deferral is re-armed rather than assumed impossible.
            if write_client_locked(&mut *out, &ctx.screen, &seq, BoundaryPolicy::Defer) {
                true
            } else {
                ctx.pending.store(true, Ordering::Relaxed);
                false
            }
        }
        None => false,
    }
}

/// Repaint the host terminal from the client's live screen model (`Ctrl-b r`).
///
/// This is the recovery for a garbled display: native scrollback mixed with
/// the pre-attach `a` list, a status-bar injection that the inner TUI did
/// not expect, a missed alt-screen frame. It writes the same snapshot
/// attach uses -- current grid, cursor, input modes -- then redraws the
/// status bar, whose reserved row the snapshot's ED2 just blanked.
///
/// Same boundary rules as `draw_status_bar`: never splice into a half-
/// emitted CSI. Deferring sets `pending_refresh`, which the main frame loop
/// flushes at the next safe chunk.
fn redraw_live_screen(ctx: &StatusBarCtx) -> bool {
    {
        let (at_boundary, in_sync) = {
            let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
            (screen.at_escape_boundary(), screen.in_synchronized_update())
        };
        if !at_boundary || sync_defer(ctx, in_sync) {
            ctx.pending_refresh.store(true, Ordering::Relaxed);
            return false;
        }
    }
    let mut out = ctx
        .stdout
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match live_screen_refresh_locked(ctx) {
        Some(seq) => {
            if write_client_locked(&mut *out, &ctx.screen, &seq, BoundaryPolicy::Defer) {
                true
            } else {
                ctx.pending_refresh.store(true, Ordering::Relaxed);
                false
            }
        }
        None => false,
    }
}

/// Snapshot plus a forced status-bar sequence, or `None` when the stream is
/// not at a safe boundary (in which case `pending_refresh` is set).
fn live_screen_refresh_locked(ctx: &StatusBarCtx) -> Option<Vec<u8>> {
    let (at_boundary, in_sync, snapshot) = {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        (
            screen.at_escape_boundary(),
            screen.in_synchronized_update(),
            screen.snapshot(),
        )
    };
    if !at_boundary || sync_defer(ctx, in_sync) {
        ctx.pending_refresh.store(true, Ordering::Relaxed);
        return None;
    }
    ctx.pending_refresh.store(false, Ordering::Relaxed);
    let snapshot = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        screen.filter_host(&snapshot).unwrap_or(snapshot)
    };
    let mut seq = snapshot;
    if let Some((geom, text)) = status_bar_render(ctx) {
        if let Some(bar) = status_bar_redraw_locked(ctx, geom, &text, true) {
            seq.extend_from_slice(&bar);
        }
    }
    Some(seq)
}

/// Geometry plus the rendered bar text, or `None` when the terminal has no
/// reserved row. Deliberately computed *before* the stdout lock is taken:
/// `status_bar_text` reads session records off disk, and the PTY relay must
/// not block behind that.
fn status_bar_render(ctx: &StatusBarCtx) -> Option<(TermGeom, String)> {
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return None,
    };
    if !geom.reserved {
        return None;
    }
    let text = status_bar_text(ctx, geom.cols as usize);
    Some((geom, text))
}

/// `status_bar_render` + `status_bar_redraw_locked`, for tests and for
/// callers with no concurrent writer.
#[cfg(test)]
fn status_bar_redraw(ctx: &StatusBarCtx, force: bool) -> Option<Vec<u8>> {
    let (geom, text) = status_bar_render(ctx)?;
    status_bar_redraw_locked(ctx, geom, &text, force)
}

/// `draw_status_bar` minus the write: every gate (reserved row, escape
/// boundary, synchronized-output deferral, dirty check) and the exact bytes
/// that would go to the terminal, or `None` when nothing should be written.
///
/// Split out so tests can drive the real decision path and feed the real
/// bytes through a real `vt100` host terminal, without redirecting the
/// process's fd 1 out from under a concurrently-running test harness.
fn status_bar_redraw_locked(
    ctx: &StatusBarCtx,
    geom: TermGeom,
    text: &str,
    force: bool,
) -> Option<Vec<u8>> {
    // -- Boundary gate, before anything is rendered or written --------------
    //
    // The client is a raw byte relay, so a PTY read boundary lands at an
    // arbitrary offset in the workload's output: "between two chunks" is not
    // "between two escape sequences". Writing anywhere else splices our
    // `\x1b...` into the middle of the workload's half-emitted sequence (or
    // its half-emitted UTF-8 character); the host terminal abandons the
    // partial sequence and prints its remaining parameter bytes as literal
    // text into the workload's own frame. That is the reported corruption,
    // and it is not fixable by re-timing -- only by asking the stream.
    //
    // Deferring is never dropping: `ctx.pending` is flushed by the main frame
    // loop at the first safe boundary, which is at most one PTY chunk away.
    let (at_boundary, in_sync, restore, workload_margins) = {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        (
            screen.at_escape_boundary(),
            screen.in_synchronized_update(),
            screen.cursor_restore(),
            screen.margins(),
        )
    };
    if !at_boundary || sync_defer(ctx, in_sync) {
        ctx.pending.store(true, Ordering::Relaxed);
        return None;
    }
    {
        let mut last = ctx
            .last_drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let key = (text.to_string(), geom.rows, geom.cols, workload_margins);
        if !force && last.as_ref() == Some(&key) {
            ctx.pending.store(false, Ordering::Relaxed);
            return None;
        }
        *last = Some(key);
    }
    ctx.pending.store(false, Ordering::Relaxed);
    Some(status_bar_sequence(geom, text, workload_margins, &restore))
}

/// Whether a redraw should be held back because the workload is part-way
/// through a synchronized-output frame, bounded by
/// `STATUS_BAR_SYNC_DEFER_LIMIT` so an unclosed block cannot freeze the bar.
fn sync_defer(ctx: &StatusBarCtx, in_sync: bool) -> bool {
    let mut since = ctx
        .sync_deferred_since
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if !in_sync {
        *since = None;
        return false;
    }
    match *since {
        Some(started) => started.elapsed() < STATUS_BAR_SYNC_DEFER_LIMIT,
        None => {
            *since = Some(Instant::now());
            true
        }
    }
}

/// The exact bytes a status-bar redraw writes. Split out from
/// `draw_status_bar` so a test can drive the real sequence through a real
/// `vt100` host terminal rather than assert on substrings of it.
///
/// There is deliberately **no `\x1b7`/`\x1b8` (DECSC/DECRC) bracket** here any
/// more, and none anywhere else in the client. A terminal has exactly one
/// save-cursor register. Saving into it from a stream we are only relaying
/// silently destroys whatever the workload put there, and the workload's own
/// later `\x1b8` then restores to *our* saved position -- text landing on the
/// wrong row, which is the superimposed-frames half of issue #5. Claude Code
/// opens with exactly that idiom (`\x1b7\x1b[r\x1b8`), opencode uses the same
/// register through `CSI s`/`CSI u`, and every `tput sc`-style progress line
/// run inside a session does too. The register is the workload's; the client
/// restores absolutely from its own model instead (`restore`, from
/// `ClientScreen::cursor_restore`), which also restores the workload's SGR pen
/// -- something DECRC only gives back on terminals whose DECSC saves
/// attributes, and `vt100` (the model aplexer itself runs) is not one.
///
/// `\x1b[?25l` first so the cursor does not visibly hop to the bar row and
/// back; `restore` ends with the workload's own cursor visibility, so the
/// hide is undone exactly as the workload wants it.
///
/// The scroll region re-asserted is the workload's own sub-range when it has
/// one, otherwise the bar's `1;{rows-1}` reservation. Re-asserting
/// `1;{rows-1}` unconditionally destroys a margin-using TUI's region --
/// including the one the attach snapshot just restored
/// (docs/terminal-state-design.md section 6.2 step 3) -- and makes the host
/// scroll the wrong rows. DECSTBM homes the cursor as a side effect on real
/// terminals, which is precisely why the absolute restore has to come after
/// it rather than being skipped when the region is unchanged.
fn status_bar_sequence(
    geom: TermGeom,
    text: &str,
    workload_margins: Option<(u16, u16)>,
    restore: &[u8],
) -> Vec<u8> {
    let mut seq = Vec::new();
    seq.extend_from_slice(b"\x1b[?25l");
    seq.extend_from_slice(
        match workload_margins {
            Some((top, bottom)) => format!("\x1b[{top};{bottom}r"),
            None => format!("\x1b[1;{}r", geom.rows - 1),
        }
        .as_bytes(),
    );
    seq.extend_from_slice(format!("\x1b[{};1H", geom.rows).as_bytes());
    seq.extend_from_slice(b"\x1b[2K\x1b[7m");
    seq.extend_from_slice(text.as_bytes());
    seq.extend_from_slice(b"\x1b[0m");
    seq.extend_from_slice(restore);
    seq
}

// ---------------------------------------------------------------------------
// Scroll mode (`Ctrl-b [`, or the wheel) -- aplexer's copy-mode
// ---------------------------------------------------------------------------
//
// The problem it solves. `a attach` holds the host terminal on the alternate
// screen for the whole attach (`ATTACH_ALT_SCREEN_ENTER`) so the pre-attach
// `a` session list cannot bleed into the live view. The alternate screen has
// no scrollback, so from the host terminal there is nothing to scroll back
// *to* -- and worse, a terminal with xterm's `alternateScroll` answers a
// wheel event there by synthesizing cursor-up/down key presses and sending
// them to the workload, i.e. scrolling to read types into the user's agent.
// That translation is now off (`?1007l`), which stopped the harm and left the
// user with no way to read earlier output at all.
//
// The shape of the fix is tmux's, not a terminal's. A tmux pane's virtual
// terminal retains a scrollback grid above the visible screen, and copy-mode
// pages through that grid; tmux never asks the host for scrollback and never
// re-parses a byte log. aplexer's equivalent emulator is `ScreenTracker`,
// which the attach client already runs over every relayed byte -- it was just
// built with a scrollback length of zero. Giving the *client's* model a real
// scrollback length (`ClientScreen::try_new_with_scrollback`) makes the
// history accumulate as a side effect of the parse that was happening anyway,
// and `Screen::set_scrollback` pages it.
//
// Why the client's model and not the worker's. The worker is the tmux-faithful
// home for it -- one parse, survives detach -- but it would need a protocol
// addition to serve scrolled-back rows, and the worker parses every session
// whether or not anyone is attached, so the memory would be spent on sessions
// nobody is reading. The client pays only while attached, is already at the
// exact geometry the pager has to render at, and reaches the same "scroll
// back through what happened while I was away" outcome by priming its grid
// once from the worker's retained raw history at attach
// (`ClientScreen::seed_history`, over the `capture` RPC that already exists).
// Only the priming replay reads bytes; from then on the live model *is* the
// history.

/// Retained history depth, in lines, for an attach client's model --
/// `history-limit` in tmux, whose default this deliberately matches.
///
/// Overridable with `APLEXER_HISTORY_LIMIT`; `0` disables scroll mode's
/// history entirely (the pager then has only the current screen, and the
/// model costs exactly what it did before this feature). The value is
/// clamped against `MAX_SCROLLBACK_CELLS` at the terminal's width, so a
/// large number cannot turn into a large allocation.
fn history_limit() -> usize {
    match env::var("APLEXER_HISTORY_LIMIT") {
        Ok(v) => v
            .trim()
            .parse::<usize>()
            .unwrap_or(aplexer::screen::DEFAULT_SCROLLBACK_LINES),
        Err(_) => aplexer::screen::DEFAULT_SCROLLBACK_LINES,
    }
}

/// How much of the worker's retained raw history is replayed into a fresh
/// client model to give it a past (`ClientScreen::seed_history`).
///
/// This used to be sized from the line limit at an assumed ~512 raw bytes per
/// rendered line, which put the default 2000-line grid at ~1 MiB. **A byte
/// budget is not a line budget**, and for the workload aplexer exists for the
/// two diverge in the direction that empties the pager: an agent CLI that has
/// been idle spends its bytes on animation, not on rows. Measured over the
/// retained history of thirteen live agent sessions, one had spent 500 KiB on
/// a spinner containing *zero* line feeds -- 18,000 absolute cursor addresses
/// and not one row of transcript. Any fixed per-line guess is one idle hour
/// away from being a budget of pure noise, so this is simply a flat budget
/// with its cost measured rather than a guess dressed as arithmetic.
///
/// A line-feed-counting budget was tried and refused by measurement: agent
/// CLIs emit many `\n` per *rendered* row (wrapped and redrawn rows), so
/// "the suffix holding 4000 line feeds" cut four sessions from ~2000 retained
/// lines to 51-245. Counting line feeds is no better a proxy for rows than
/// counting bytes is.
///
/// **Why 2 MiB.** Replaying real captures through the real seed path, at
/// 23x100 into a 2000-line grid, minimum of five runs -- retained lines, and
/// the parse those lines cost:
///
/// ```text
///                  worst session   parse (mean / worst)
///   1 MiB shipped      0 lines        16.1 / 20.3 ms
///   2 MiB             83 lines        28.7 / 38.5 ms
///   4 MiB            225 lines        55.8 / 86.0 ms
/// ```
///
/// The seed is synchronous on the attach path *and* on every `Ctrl-b Right`
/// switch, where the protocol round trip it sits beside is 0.3-6 ms at p50
/// and 13-22 ms at p95 (`attach_round_trip_latency`). 2 MiB buys every one of
/// those thirteen sessions a pager with real content in it for ~13 ms; 4 MiB
/// spends another ~27 ms on every switch anyone ever makes to take a single
/// pathological session from 83 rows of history to 225. That is not a trade
/// worth making, and 83 rows is already three and a half screens.
fn scrollback_seed_bytes() -> usize {
    (2 * 1024 * 1024).min(aplexer::DEFAULT_HISTORY_BYTES)
}

/// Whether the client may borrow mouse reporting from the host terminal.
///
/// It has to, to see a wheel event at all: the host reports the wheel only
/// while some mouse protocol is enabled, and with `?1007l` in force nothing
/// else turns a wheel roll into anything. The cost is tmux's cost with
/// `mouse on` -- while the client owns the mouse, drag-to-select needs the
/// terminal's usual Shift override -- so `APLEXER_MOUSE=off` turns the
/// borrowing off and leaves `Ctrl-b [` as the way in.
fn mouse_capture_enabled() -> bool {
    !matches!(
        env::var("APLEXER_MOUSE").as_deref(),
        Ok("off") | Ok("0") | Ok("no") | Ok("false")
    )
}

/// The client's own mouse reporting: every protocol and encoding this client
/// knows about turned off, then button press/release (`?1000h`) in SGR
/// encoding (`?1006h`).
///
/// `?1000h` rather than `?1002h`/`?1003h` deliberately: press/release is all
/// a wheel needs, and not asking for motion reports keeps the terminal from
/// streaming a report per cell of mouse movement across the socket.
const CLIENT_MOUSE_ENABLE: &[u8] =
    b"\x1b[?9l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1000h\x1b[?1006h";

/// `CAN` -- "abandon any control sequence in flight". Leads every write made
/// under `BoundaryPolicy::StreamSuspended`; see that variant's doc comment
/// for why that is what makes those writes safe without the boundary gate.
const SCROLL_CANCEL: &[u8] = b"\x18";

/// Lines a wheel notch moves, matching tmux's own three.
const WHEEL_LINES: usize = 3;

/// SGR mouse button numbers for the wheel (xterm: 64 + button index).
const MOUSE_WHEEL_UP: u32 = 64;
const MOUSE_WHEEL_DOWN: u32 = 65;

