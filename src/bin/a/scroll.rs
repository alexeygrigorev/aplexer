use super::*;

/// Shared scroll-mode state.
///
/// `active` is an atomic rather than part of the mutex because the relay
/// reads it on every chunk, under the stdout lock, purely to decide whether
/// to write. Lock order is `stdout` -> `view` -> `screen`, which extends the
/// existing `stdout` -> `term` -> `screen` order rather than crossing it:
/// nothing takes `stdout` while holding `view`.

pub(crate) struct ScrollMode {
    pub(crate) active: AtomicBool,
    /// Type-through (`i` while the pager is up): `active` stays set -- the
    /// pager keeps the reserved bar row and the scroll offset -- but the
    /// relay flows and stdin forwards to the workload, so typing has its
    /// echo and the reply is visible as it streams. Esc drops it.
    pub(crate) typing: AtomicBool,
    pub(crate) view: Mutex<ScrollView>,
}

impl ScrollMode {
    pub(crate) fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            typing: AtomicBool::new(false),
            view: Mutex::new(ScrollView::default()),
        }
    }
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
    pub(crate) fn is_typing(&self) -> bool {
        self.typing.load(Ordering::Relaxed)
    }
}

/// Where the pager is looking: `offset` lines above the live screen, out of
/// `available` retained.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScrollView {
    pub(crate) offset: usize,
    pub(crate) available: usize,
}

/// Keep the pager's view anchored to its *content* while the workload
/// streams behind it, the way tmux copy-mode does.
///
/// `offset` counts lines above the live screen, so on its own the view
/// slides: every line the workload scrolls off re-bases that coordinate
/// system at the (moved) bottom, and the page the user was reading drifts
/// toward the live screen by exactly the number of lines that arrived since
/// they opened it -- an agent streaming a reply drags the reader back down
/// mid-conversation. Compensating by the growth of `available` pins the
/// same lines under the viewport. At the retained-depth ceiling `available`
/// stops growing while the oldest lines fall off the top; there,
/// distance-from-the-bottom coordinates are already stable and no
/// compensation is due (the transition into the ceiling under-compensates
/// by the lines that filled it -- a one-event sliver, accepted).
///
/// `offset == 0` is the live screen and stays put by definition.
///
/// Every writer that refreshes the pager while bytes keep arriving --
/// `paint_scroll_view` on navigation and resize, `refresh_scroll_bar` on
/// the status tick -- must run this, or the tick's honest `available`
/// update silently swallows the growth and the next paint under-compensates.
pub(crate) fn reanchor_view(view: &mut ScrollView, fresh_available: usize) {
    let grown = fresh_available.saturating_sub(view.available);
    if view.offset > 0 && grown > 0 {
        view.offset += grown;
    }
    view.available = fresh_available;
}

/// One navigation step, resolved against the viewport height by
/// `apply_scroll_command`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollCommand {
    /// Enter the pager without moving (`Ctrl-b [`).
    Stay,
    Up(usize),
    Down(usize),
    PageUp,
    PageDown,
    HalfUp,
    HalfDown,
    Top,
    Bottom,
    /// `i`: hand the keyboard to the workload while staying in the pager
    /// (type-through; a lone `Esc` takes it back).
    TypeThrough,
    /// `q`, `Esc` or `Ctrl-C`: back to the live screen.
    Exit,
}

/// What `scroll_keys` made of the bytes at the front of the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollKey {
    /// A navigation command, and how many bytes it consumed.
    Command(ScrollCommand, usize),
    /// Recognized and deliberately swallowed (a non-wheel mouse report, an
    /// unbound key). **Consumed, never forwarded** -- that is the whole
    /// point of the mode: while the pager is up, no keystroke reaches the
    /// workload (until `i` hands the keyboard over; see `TypeThrough`).
    Ignored(usize),
    /// A sequence that has begun but not finished in this buffer. The caller
    /// keeps the bytes and retries when more arrive.
    Incomplete,
}

/// Keyboard and mouse decoding for scroll mode. Pure, so the split-sequence
/// and modifier cases are unit-testable without a terminal.
///
/// Bindings follow tmux copy-mode where tmux has one and `less` elsewhere,
/// because those are the two muscle memories a user arrives with:
/// arrows/`j`/`k` by the line, PageUp/PageDown and Space/`b` by the screen,
/// `Ctrl-U`/`Ctrl-D`/`u`/`d` by the half screen, Home/`g` and End/`G` to the
/// ends, `q`/`Esc`/`Ctrl-C` back to live, and the wheel by
/// `WHEEL_LINES`.
///
/// A lone `ESC` that is the *entire* remaining buffer is read as the Escape
/// key, not as the start of a sequence that has not arrived yet. Real
/// terminals emit `ESC [ A` for an arrow key in one write, so the ambiguity
/// is only theoretically reachable, and resolving it the other way would
/// mean Escape did nothing until the user pressed another key -- much worse
/// than the rare case of a split arrow key exiting the pager.
pub(crate) fn scroll_keys(buf: &[u8]) -> ScrollKey {
    use ScrollCommand::*;
    let Some(&first) = buf.first() else {
        return ScrollKey::Incomplete;
    };
    if first != 0x1b {
        let command = match first {
            b'q' | b'Q' | 0x03 => Some(Exit),
            b'k' | b'y' => Some(Up(1)),
            b'j' | b'e' => Some(Down(1)),
            b' ' | b'f' | 0x06 => Some(PageDown),
            b'b' | 0x02 => Some(PageUp),
            b'u' | 0x15 => Some(HalfUp),
            b'd' | 0x04 => Some(HalfDown),
            b'g' => Some(Top),
            b'G' => Some(Bottom),
            b'i' => Some(TypeThrough),
            _ => None,
        };
        return match command {
            Some(c) => ScrollKey::Command(c, 1),
            None => ScrollKey::Ignored(1),
        };
    }
    if buf.len() == 1 {
        return ScrollKey::Command(Exit, 1);
    }
    match buf[1] {
        b'[' => {
            if buf.len() == 2 {
                return ScrollKey::Incomplete;
            }
            if buf[2] == b'<' {
                return match parse_sgr_mouse(buf) {
                    MouseParse::Complete(report, consumed) => {
                        // Wheel reports repeat on press only; the release
                        // report a terminal may pair with them is swallowed
                        // by the `Ignored` arm below, so one notch moves
                        // WHEEL_LINES exactly once.
                        match (report.button, report.press) {
                            (MOUSE_WHEEL_UP, true) => ScrollKey::Command(Up(WHEEL_LINES), consumed),
                            (MOUSE_WHEEL_DOWN, true) => {
                                ScrollKey::Command(Down(WHEEL_LINES), consumed)
                            }
                            _ => ScrollKey::Ignored(consumed),
                        }
                    }
                    MouseParse::Incomplete => ScrollKey::Incomplete,
                    MouseParse::NotMouse => ScrollKey::Ignored(1),
                };
            }
            // A generic CSI: scan to the final byte, so `\x1b[5;2~`
            // (shifted PageUp) resolves the same as `\x1b[5~`.
            let Some(end) = buf[2..]
                .iter()
                .position(|b| (0x40..=0x7e).contains(b))
                .map(|i| i + 2)
            else {
                // Bounded, so a stray `ESC [` followed by a stream of digits
                // cannot buffer forever.
                return if buf.len() > 32 {
                    ScrollKey::Ignored(buf.len())
                } else {
                    ScrollKey::Incomplete
                };
            };
            let consumed = end + 1;
            let params = &buf[2..end];
            let leading: u32 = params
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .fold(0u32, |acc, b| {
                    acc.saturating_mul(10).saturating_add(u32::from(b - b'0'))
                });
            let command = match (buf[end], leading) {
                (b'A', _) => Some(Up(1)),
                (b'B', _) => Some(Down(1)),
                (b'H', _) => Some(Top),
                (b'F', _) => Some(Bottom),
                (b'~', 1 | 7) => Some(Top),
                (b'~', 4 | 8) => Some(Bottom),
                (b'~', 5) => Some(PageUp),
                (b'~', 6) => Some(PageDown),
                _ => None,
            };
            match command {
                Some(c) => ScrollKey::Command(c, consumed),
                None => ScrollKey::Ignored(consumed),
            }
        }
        b'O' => {
            if buf.len() == 2 {
                return ScrollKey::Incomplete;
            }
            let command = match buf[2] {
                b'A' => Some(Up(1)),
                b'B' => Some(Down(1)),
                b'H' => Some(Top),
                b'F' => Some(Bottom),
                _ => None,
            };
            match command {
                Some(c) => ScrollKey::Command(c, 3),
                None => ScrollKey::Ignored(3),
            }
        }
        _ => ScrollKey::Ignored(2),
    }
}

/// The scroll-mode status bar: what mode the user is in, how far back they
/// are, and how to get out.
///
/// The position readout is tmux copy-mode's `[n/total]` in words rather than
/// brackets, because this bar is the *only* thing telling the user their
/// keystrokes are going to the pager instead of to their agent -- the single
/// question the mode has to answer at a glance. Trimmed from the right as
/// the terminal narrows, down to a minimum that keeps the word SCROLL and
/// the way out.
pub(crate) fn scroll_bar_text(view: ScrollView, cols: usize, alt_screen: bool) -> String {
    let position = format!("SCROLL {}/{}", view.offset, view.available);
    // An empty pager must say *why* it is empty, and must say it on an
    // ordinary 80-column terminal rather than only on a wide one. So the two
    // cases get different ladders: with history the row spends its width on
    // the navigation keys, and with none it spends the width on the reason
    // instead -- offering PgUp/PgDn for a pager that cannot move is the thing
    // that reads as a broken feature.
    let candidates: Vec<String> = if view.available == 0 {
        let why = if alt_screen {
            // A full-screen application owns the grid; `vt100` gives the
            // alternate screen no scrollback, as every real terminal does.
            "no history: the workload owns the screen"
        } else {
            // The primary-screen case, and the one the generic hint used to
            // hide: a TUI that repaints in place with absolute cursor
            // addressing never scrolls, so nothing has ever left the top of
            // the screen for the history to hold. Measured on a real opencode
            // session whose whole 4 MiB of retained history holds 81,584
            // cursor addresses and zero line feeds -- there is genuinely
            // nothing to page back to, and what the user wants is on screen.
            "no history: nothing has scrolled off this screen"
        };
        vec![
            format!("{position} · {why} · q live"),
            format!("{position} · {why}"),
            format!("{position} · no history · q live"),
            format!("{position} · no history"),
        ]
    } else {
        vec![
            format!(
                "{position} · PgUp/PgDn ↑↓ Home/End · q live · keys go here, not to the session"
            ),
            format!("{position} · PgUp/PgDn ↑↓ Home/End · q live"),
            format!("{position} · q live"),
        ]
    };
    for candidate in candidates.into_iter().chain([position.clone()]) {
        if terminal_display_width(&candidate) <= cols {
            return pad_or_truncate(&sanitize_terminal_text(&candidate), cols);
        }
    }
    pad_or_truncate(&sanitize_terminal_text("SCROLL · q"), cols)
}

/// The bar row, drawn for scroll mode: no workload cursor restore (the
/// workload's cursor is not on screen -- the pager is), and the cursor left
/// hidden.
pub(crate) fn scroll_bar_sequence(geom: TermGeom, text: &str) -> Vec<u8> {
    let mut seq = Vec::new();
    seq.extend_from_slice(b"\x1b[?25l");
    seq.extend_from_slice(format!("\x1b[{};1H", geom.rows).as_bytes());
    seq.extend_from_slice(b"\x1b[2K\x1b[7m");
    seq.extend_from_slice(text.as_bytes());
    seq.extend_from_slice(b"\x1b[0m\x1b[?25l");
    seq
}

/// Paint the pager: the model's screen as it looked `offset` lines back,
/// plus the scroll-mode bar.
///
/// Three things this does not do, each deliberate:
///
/// - It does not consult the escape boundary (`BoundaryPolicy::StreamSuspended`).
///   Nothing of the workload's is being written while the pager is up, so
///   there is no half-emitted sequence of the workload's to splice into; the
///   `SCROLL_CANCEL` prefix ends whatever the host had in flight at the
///   moment the relay was suspended.
/// - It does not leave the model scrolled. `ClientScreen::scrolled_frame`
///   applies the offset, renders, and puts the model back at the live
///   screen, so `relay`, `cursor_restore` and `snapshot` keep describing the
///   live session throughout.
/// - It re-asserts the client's own `1;{rows-1}` reservation rather than the
///   workload's margins. The pager owns the whole screen above the bar; a
///   workload sub-range is meaningless to it, and would let the frame's own
///   absolute row addressing fall outside the region.
pub(crate) fn paint_scroll_view(ctx: &StatusBarCtx) -> bool {
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return false,
    };
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let mut view = ctx
        .scroll
        .view
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (frame, alt_screen) = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        // Lines the workload scrolled off since the last paint re-based the
        // offset's coordinate system; grow the offset first so the frame
        // renders the lines the user was reading rather than newer ones
        // (`reanchor_view`).
        reanchor_view(&mut view, screen.scrollback_available());
        let (frame, offset, available) = screen.scrolled_frame(view.offset);
        view.offset = offset;
        view.available = available;
        let frame = screen.filter_host(&frame).unwrap_or(frame);
        (frame, screen.alternate_screen())
    };
    let mut seq = SCROLL_CANCEL.to_vec();
    if geom.reserved {
        seq.extend_from_slice(format!("\x1b[1;{}r", geom.rows - 1).as_bytes());
    }
    seq.extend_from_slice(&frame);
    if geom.reserved {
        let text = if ctx.scroll.is_typing() {
            scroll_bar_typing_text(*view, geom.cols as usize)
        } else {
            scroll_bar_text(*view, geom.cols as usize, alt_screen)
        };
        seq.extend_from_slice(&scroll_bar_sequence(geom, &text));
        // Recorded against the same dirty-check `refresh_scroll_bar` reads,
        // so the status tick right after a navigation does not rewrite a row
        // this frame just drew.
        *ctx.last_drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((text, geom.rows, geom.cols, None));
    }
    seq.extend_from_slice(b"\x1b[?25l");
    drop(view);
    write_client_locked(
        &mut *out,
        &ctx.screen,
        &seq,
        BoundaryPolicy::StreamSuspended,
    )
}

/// Refresh just the pager's bar row, so the retained-line count stays honest
/// while the workload keeps producing output behind the pager, without
/// repainting (and flickering) a whole screen on a timer.
///
/// The write policy follows the mode. With the pager owning the screen the
/// relay is suspended, so `StreamSuspended` applies and the bytes go out
/// unconditionally. While **type-through** (`i`) has handed the keyboard back
/// the relay is streaming again, so the bar is client-originated output
/// spliced into a live stream like any other: it waits for an escape boundary
/// (`Defer`), and on a refusal arms `ctx.pending` so the frame loop retries at
/// the next chunk. `pending` doubles as the force flag here -- a deferred bar
/// must not be swallowed by the dirty check on the retry, because between the
/// deferral and the retry nothing else may have changed the text.
pub(crate) fn refresh_scroll_bar(ctx: &StatusBarCtx) -> bool {
    let typing = ctx.scroll.is_typing();
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return false,
    };
    if !geom.reserved {
        return false;
    }
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let mut view = ctx
        .scroll
        .view
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (available, alt_screen) = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        (screen.scrollback_available(), screen.alternate_screen())
    };
    // Not just the honest count: the growth this tick observes is growth
    // the next paint must compensate by, and the tick is the only writer
    // between paints when the user is only reading (`reanchor_view`).
    reanchor_view(&mut view, available);
    let text = if typing {
        scroll_bar_typing_text(*view, geom.cols as usize)
    } else {
        scroll_bar_text(*view, geom.cols as usize, alt_screen)
    };
    drop(view);
    // Dirty-checked against the same `last_drawn` the live bar uses, so this
    // tick is a no-op in the common case *and* still repairs the row when
    // something else wrote over it -- a `Ctrl-b ?` help flash, most
    // obviously, which would otherwise sit on the bar for the rest of the
    // time the user spends reading. A pending deferred write forces through
    // the check: the text is by assumption unchanged (that is why the check
    // would skip), and the row may have been erased in the meantime.
    let force = typing && ctx.pending.load(Ordering::Relaxed);
    {
        let mut last = ctx
            .last_drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let key = (text.clone(), geom.rows, geom.cols, None);
        if !force && last.as_ref() == Some(&key) {
            return false;
        }
        *last = Some(key);
    }
    let policy = if typing {
        BoundaryPolicy::Defer
    } else {
        BoundaryPolicy::StreamSuspended
    };
    let wrote = write_client_locked(
        &mut *out,
        &ctx.screen,
        &scroll_bar_sequence(geom, &text),
        policy,
    );
    if wrote {
        ctx.pending.store(false, Ordering::Relaxed);
    } else {
        ctx.pending.store(true, Ordering::Relaxed);
    }
    wrote
}

/// The bar while type-through is active. The pager still owns the reserved
/// row and its position readout stays honest, but the mode word and the hint
/// change: the keyboard currently belongs to the session, and Esc is the way
/// back to paging.
pub(crate) fn scroll_bar_typing_text(view: ScrollView, cols: usize) -> String {
    let full = format!(
        "SCROLL {}/{} · TYPE — keys go to the session · Esc back to paging",
        view.offset, view.available
    );
    if full.chars().count() <= cols {
        full
    } else if cols >= 14 {
        format!("SCROLL {}/{} · TYPE", view.offset, view.available)
    } else {
        "TYPE".to_string()
    }
}

/// Enter scroll mode and run the gesture that asked for it.
///
/// `active` is flipped under the stdout lock, which is the same lock
/// `relay_to_terminal` checks it under -- so a chunk cannot be half-written
/// over the pager's first frame.
pub(crate) fn enter_scroll_mode(ctx: &StatusBarCtx, first: ScrollCommand) {
    {
        let _held = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
        if ctx.scroll.active.swap(true, Ordering::SeqCst) {
            return;
        }
        ctx.scroll.typing.store(false, Ordering::SeqCst);
        *ctx.scroll
            .view
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = ScrollView::default();
    }
    // The bar is about to say something completely different from whatever
    // the dirty-check last recorded, and will again on the way out.
    *ctx.last_drawn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    // Before the first frame paints: for a workload that scrolls through its
    // own DECSTBM sub-ranges, the live model's history is missing exactly the
    // rows the user is opening the pager to find, and the worker's retained
    // raw tail still has them (`refresh_pager_history`). Runs with
    // `active` already set, so the relay is suspended while it works and the
    // pager's first frame goes onto a quiet host.
    refresh_pager_history(ctx);
    // A fresh view says `available: 0`, but the first pager frame will find
    // a model that may already hold pages of history (the live grid's own,
    // or a freshly rebuilt one). Reading that backlog as lines that arrived
    // *behind the pager* would let `reanchor_view` fling the entry gesture
    // straight to the top of the history; the baseline belongs to the model
    // as the pager is about to see it, not to the empty view.
    {
        let available = {
            let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
            screen.scrollback_available()
        };
        ctx.scroll
            .view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .available = available;
    }
    apply_scroll_command(ctx, first);
}

/// Leave scroll mode: repaint the host from the live model and let the relay
/// resume.
///
/// The repaint is the same snapshot `Ctrl-b r` writes -- the model has been
/// fed every byte that arrived while the pager was up, so it is current, and
/// the frames the relay declined to write are already accounted for in it.
/// The bar is rewritten in the same sequence because the snapshot's `ED2`
/// blanks the reserved row.
pub(crate) fn exit_scroll_mode(ctx: &StatusBarCtx) {
    if !ctx.scroll.is_active() {
        return;
    }
    paint_live_screen(ctx);
    ctx.scroll.typing.store(false, Ordering::SeqCst);
    ctx.scroll.active.store(false, Ordering::SeqCst);
}

/// Paint the host from the live model while staying in scroll mode: the
/// snapshot `Ctrl-b r` writes, plus the bar, with the relay still suspended
/// throughout. `exit_scroll_mode` runs this before dropping `active`, and
/// `enter_typing` runs it before raising `typing` -- both orderings mean a
/// relay chunk can only ever land on the view it belongs on.
pub(crate) fn paint_live_screen(ctx: &StatusBarCtx) {
    // Reads session records off disk; must not happen under the stdout lock.
    let bar = status_bar_render(ctx);
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let (snapshot, restore, margins) = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = screen.snapshot();
        let snapshot = screen.filter_host(&snapshot).unwrap_or(snapshot);
        (snapshot, screen.cursor_restore(), screen.margins())
    };
    let mut seq = SCROLL_CANCEL.to_vec();
    seq.extend_from_slice(&snapshot);
    if let Some((geom, text)) = bar {
        seq.extend_from_slice(&status_bar_sequence(geom, &text, margins, &restore));
    }
    write_client_locked(
        &mut *out,
        &ctx.screen,
        &seq,
        BoundaryPolicy::StreamSuspended,
    );
    *ctx.last_drawn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
}

/// `i` in the pager: hand the keyboard to the workload without leaving the
/// pager -- the thing tmux copy-mode cannot do. The view repaints to the
/// live screen first (so typing has its echo and the reply is visible as it
/// streams), then `typing` rises under the stdout lock, which is the lock
/// `relay_to_terminal` checks the flag under -- the relay stays suspended
/// until the flip, so no chunk can land on the pager's view.
pub(crate) fn enter_typing(ctx: &StatusBarCtx) {
    if !ctx.scroll.is_active() || ctx.scroll.is_typing() {
        return;
    }
    paint_live_screen(ctx);
    {
        let _held = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
        ctx.scroll.typing.store(true, Ordering::SeqCst);
    }
    // The bar's wording changes with the mode; the dirty-check still holds
    // the live bar text, so rewrite the row now rather than on the next tick.
    refresh_scroll_bar(ctx);
}

/// Esc while typing: take the keyboard back for the pager at the same
/// offset. `typing` drops first, under the stdout lock, and the pager frame
/// repaints after -- a relay chunk in between can only land on the live view
/// it was headed for anyway, and is covered by the frame immediately.
pub(crate) fn exit_typing(ctx: &StatusBarCtx) {
    if !ctx.scroll.is_typing() {
        return;
    }
    {
        let _held = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
        ctx.scroll.typing.store(false, Ordering::SeqCst);
    }
    apply_scroll_command(ctx, ScrollCommand::Stay);
}

/// Resolve one navigation step against the current viewport and repaint.
///
/// `Down` past the live screen leaves scroll mode, the way tmux's copy-mode
/// does not but every pager the user has ever used does: scrolling back to
/// the bottom means "I am done reading", and having to also press `q` to get
/// the keyboard back is exactly the confusion this mode must not create.
pub(crate) fn apply_scroll_command(ctx: &StatusBarCtx, command: ScrollCommand) {
    let page = {
        let geom = ctx.term.lock().map(|g| *g).unwrap_or(TermGeom {
            rows: 0,
            cols: 0,
            reserved: false,
        });
        usize::from(reserved_rows(geom.rows)).max(1)
    };
    if command == ScrollCommand::Exit {
        exit_scroll_mode(ctx);
        return;
    }
    if command == ScrollCommand::TypeThrough {
        enter_typing(ctx);
        return;
    }
    let exit = {
        let mut view = ctx
            .scroll
            .view
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let (delta, downward): (isize, bool) = match command {
            ScrollCommand::Exit => unreachable!("handled above"),
            ScrollCommand::TypeThrough => unreachable!("handled above"),
            ScrollCommand::Stay => (0, false),
            ScrollCommand::Up(n) => (n as isize, false),
            ScrollCommand::Down(n) => (-(n as isize), true),
            ScrollCommand::PageUp => (page as isize, false),
            ScrollCommand::PageDown => (-(page as isize), true),
            ScrollCommand::HalfUp => ((page / 2).max(1) as isize, false),
            ScrollCommand::HalfDown => (-((page / 2).max(1) as isize), true),
            ScrollCommand::Top => (view.available as isize, false),
            ScrollCommand::Bottom => (-(view.offset as isize), true),
        };
        if downward && view.offset == 0 {
            true
        } else {
            view.offset = (view.offset as isize + delta).max(0) as usize;
            false
        }
    };
    if exit {
        exit_scroll_mode(ctx);
    } else {
        paint_scroll_view(ctx);
    }
}

/// Borrow the host's mouse reporting for the client, or hand it back to the
/// workload -- whichever the workload's own state currently calls for.
///
/// **Precedence, stated deliberately: the workload wins.** A TUI that has
/// asked for mouse reporting is a TUI with panes of its own to scroll, and
/// tmux's answer -- the pane's application gets the mouse when it requested
/// it -- is the right one. So the client borrows the mouse only while the
/// workload wants none, and gives it back the moment the workload asks,
/// re-asserting the workload's exact modes with
/// `ClientScreen::workload_mouse_sequence` so the handover cannot leave the
/// terminal in a state neither side chose. While the workload holds the
/// mouse the wheel goes to it and `Ctrl-b [` is the way into the pager.
///
/// Boundary-gated like every other client injection: this is a live splice
/// into a relayed stream (the workload can flip mouse modes at any byte),
/// so it waits for a real boundary and simply tries again on the next status
/// tick if it does not get one.
pub(crate) fn sync_client_mouse(ctx: &StatusBarCtx) -> bool {
    if !ctx.mouse_capture {
        return false;
    }
    let (want, seq) = {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        if screen.workload_wants_mouse() {
            (false, screen.workload_mouse_sequence())
        } else {
            (true, CLIENT_MOUSE_ENABLE.to_vec())
        }
    };
    {
        let owned = ctx
            .mouse_owned
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *owned == Some(want) {
            return false;
        }
    }
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let policy = if ctx.scroll.is_active() {
        BoundaryPolicy::StreamSuspended
    } else {
        BoundaryPolicy::Defer
    };
    if write_client_locked(&mut *out, &ctx.screen, &seq, policy) {
        *ctx.mouse_owned
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(want);
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// The key overlay -- which-key for `Ctrl-b`
// ---------------------------------------------------------------------------
//
// `Ctrl-b` on its own is a mode with nothing on screen to say so. The overlay
// makes it visible the way which-key does in an editor: hesitate on the
// prefix and the keymap appears, press a key and it is gone. It renders from
// `ATTACH_BINDINGS`, so it is a third *view* of the one keymap the scanner
// implements rather than a third copy of it -- there is no string here that
// can drift from what `Ctrl-b <key>` actually does.
//
// Four properties it has to hold, each of which decided a design point:
//
// - **Muscle memory must never see it.** It is armed on a delay
//   (`KEY_OVERLAY_DELAY`) instead of being drawn by the prefix key, so a
//   `Ctrl-b Right` typed at speed draws nothing at all -- not a frame of it.
// - **The screen underneath must come back exactly.** Dismissal does not
//   restore a saved rectangle of cells; it repaints from
//   `ClientScreen::snapshot`, the same full-model repaint `Ctrl-b r` and the
//   pager's exit already use. The model was fed every byte that arrived while
//   the box was up, so the repaint is the *live* screen -- grid, cursor, SGR
//   pen, margins, input modes -- not a photograph of the one the box covered.
// - **Nothing may paint over it while it is up.** Like the pager, the overlay
//   suspends the relay: `relay_to_terminal` reads `KeyOverlay::is_active`
//   under the stdout lock and keeps feeding the model while writing nothing.
//   That is also what makes the previous point true.
// - **It must degrade honestly.** A terminal the box cannot fit into gets the
//   one-line `Ctrl-b ?` reference flashed on the status bar instead, and
//   nothing is suspended in that case.

/// How long a lone `Ctrl-b` waits for its second key before the keymap is
/// drawn for it.
///
/// which-key's whole trick is being invisible to anyone who already knows the
/// chord, so the delay has to sit above a typed two-key sequence and below
/// the point where hesitation stops feeling answered. 350ms is comfortably
/// both: a `Ctrl-b Right` from muscle memory lands in well under 200ms and
/// never draws anything.
///
/// **Its relationship with `CHORD_ESCAPE_TIMEOUT` is exclusion, not
/// ordering.** The two deadlines are armed in different scanner states and
/// can never be armed at the same moment: this one only while a *lone*
/// `Ctrl-b` is held (`InputScanner::awaiting_key`), that one only once an
/// `ESC` has arrived after the prefix and a partial arrow chord is being
/// withheld (`InputScanner::awaiting_escape`). Two consequences worth
/// spelling out, because they are what "they cannot fight each other" means
/// here: a half-typed `Ctrl-b Left` can never pop the overlay -- by the time
/// the chord deadline exists, the overlay's own is gone -- and a bare
/// `Ctrl-b ESC` still reaches the workload after exactly
/// `CHORD_ESCAPE_TIMEOUT`, not after that plus this.
pub(crate) const KEY_OVERLAY_DELAY: Duration = Duration::from_millis(350);

/// Rows the box spends on things that are not bindings: its two borders and
/// the footer line.
pub(crate) const KEY_OVERLAY_CHROME_ROWS: usize = 3;

/// Fewer binding rows than this and the box has stopped being a reference;
/// the one-line status-bar flash says more in less space.
pub(crate) const KEY_OVERLAY_MIN_BINDINGS: usize = 3;

/// The narrowest description column worth drawing. Below it the rows stop
/// being sentences and become ellipses, which is again worse than the flash.
pub(crate) const KEY_OVERLAY_MIN_DESC: usize = 14;
