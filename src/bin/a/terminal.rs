use super::*;

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
pub(crate) const DEFAULT_ATTACH_REPLAY_BYTES: usize = 32 * 1024;

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
pub(crate) const STATUS_BAR_IDLE_GAP: Duration = Duration::from_millis(450);
pub(crate) const STATUS_BAR_MAX_INTERVAL: Duration = Duration::from_secs(3);
pub(crate) const STATUS_BAR_POLL_INTERVAL: Duration = Duration::from_millis(150);

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
pub(crate) const SPINNER_FRAME_MS: u64 = STATUS_BAR_POLL_INTERVAL.as_millis() as u64;
pub(crate) const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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
pub(crate) const STATUS_BAR_SYNC_DEFER_LIMIT: Duration = Duration::from_millis(500);

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
pub(crate) const LAYOUT_DEFER_LIMIT: Duration = Duration::from_millis(500);

/// A terminal resize whose DECSTBM the boundary gate held back, and when it
/// was first held back (`LAYOUT_DEFER_LIMIT`'s deadline is measured from the
/// first deferral, not from the most recent resize).
#[derive(Clone, Copy)]
pub(crate) struct PendingLayout {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    pub(crate) since: Instant,
}

/// Physical terminal geometry as last observed by the resize-poll thread,
/// shared with the status-bar thread so its redraws always target the
/// current last row/width without a second ioctl.
#[derive(Clone, Copy)]
pub(crate) struct TermGeom {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    /// Whether the bottom row is reserved for the status bar. False for
    /// terminals too small to spare a row (see `reserved_rows`), in which
    /// case the scroll region is left/reset to full-screen and the status
    /// bar is simply not drawn.
    pub(crate) reserved: bool,
}

/// The row count told to the SERVER: one less than the physical terminal
/// when a status row is reserved, exactly like tmux tells the remote PTY its
/// terminal is one row shorter than reality so its own output never
/// overwrites the reserved line.
pub(crate) fn reserved_rows(rows: u16) -> u16 {
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
pub(crate) fn write_locked(stdout: &Arc<Mutex<io::Stdout>>, bytes: &[u8]) -> io::Result<()> {
    let mut out = stdout
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    out.write_all(bytes)?;
    out.flush()
}

/// Whether a client-originated write may still go out when the relayed
/// stream is *not* between complete escape sequences.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BoundaryPolicy {
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
pub(crate) fn write_client_locked(
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
pub(crate) fn terminal_layout_sequence(rows: u16, restore: &[u8]) -> Vec<u8> {
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
pub(crate) fn apply_terminal_layout(ctx: &StatusBarCtx, rows: u16, cols: u16) -> bool {
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
pub(crate) fn apply_terminal_layout_to(
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
pub(crate) fn layout_deferred_since(ctx: &StatusBarCtx) -> Option<Instant> {
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
pub(crate) fn flush_pending_layout(ctx: &StatusBarCtx) -> bool {
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
pub(crate) fn flush_pending_layout_to(out: &mut impl Write, ctx: &StatusBarCtx) -> bool {
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
/// the workload's screen. Type-through is live: the host shows the relayed
/// stream, so it is repainted here like the ordinary live view.
pub(crate) fn redraw_live_screen_after_layout(ctx: &StatusBarCtx) -> bool {
    if ctx.scroll.owns_host() || ctx.overlay.is_active() {
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
pub(crate) fn relay_to_terminal(ctx: &StatusBarCtx, data: &[u8]) -> io::Result<()> {
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    {
        let mut s = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
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
        if ctx.scroll.owns_host() || ctx.overlay.is_active() {
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
pub(crate) fn feed_and_write(
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
///
/// Leads with the client's own two host modes -- `ATTACH_ALT_SCREEN_EXIT`,
/// the exact undo of `ATTACH_ALT_SCREEN_ENTER` -- which belong to the attach
/// as a whole, not to any one session; `SWITCH_RESET_SEQUENCE` is the rest.
pub(crate) const TERMINAL_RESET_SEQUENCE: &[u8] = b"\
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

/// The detach-only head of `TERMINAL_RESET_SEQUENCE`: back to the primary
/// screen, and `alternateScroll` back on for the user's next `less`.
pub(crate) const ATTACH_ALT_SCREEN_EXIT: &[u8] = b"\x1b[?1049l\x1b[?1007h";

/// What an in-process switch writes between session A's last byte and
/// session B's replayed screen: every input mode and the screen reset, but
/// **not** the host modes. The host stays on the alternate screen across a
/// switch (`filter_host` would drop the `?1049l` anyway), and `?1007h` must
/// not go out: `filter_host` passes it, so writing the full detach sequence
/// here re-enabled `alternateScroll` for the rest of the attach whenever the
/// client was not holding the mouse (`APLEXER_MOUSE=off`, a non-tty stdin)
/// -- the wheel-types-into-the-agent bug `ATTACH_ALT_SCREEN_ENTER` exists
/// to prevent. Derived from the one literal so the two cannot drift; the
/// derivation is pinned by `switch_reset_keeps_the_hosts_own_modes`.
pub(crate) const SWITCH_RESET_SEQUENCE: &[u8] = TERMINAL_RESET_SEQUENCE
    .split_at(ATTACH_ALT_SCREEN_EXIT.len())
    .1;

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
pub(crate) const ATTACH_ALT_SCREEN_ENTER: &[u8] = b"\x1b[?1049h\x1b[?1007l";

pub(crate) fn reset_terminal(stdout: &Arc<Mutex<io::Stdout>>) {
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
pub(crate) struct TerminalUiGuard {
    pub(crate) stdout: Arc<Mutex<io::Stdout>>,
}
impl Drop for TerminalUiGuard {
    fn drop(&mut self) {
        reset_terminal(&self.stdout);
    }
}
