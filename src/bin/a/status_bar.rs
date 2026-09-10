use super::*;

/// Everything a status-bar redraw needs, cloned into each thread that might
/// trigger one (status thread, input thread on a switch flash, main loop
/// after a switch) instead of five loose `Arc` parameters -- see
/// docs/fast-session-switching-design.md section 3. `record` is shared and
/// swappable so an in-process switch is visible to the bar without
/// respawning the thread; `flash` is a transient error line (switch
/// failures); `last_drawn` backs the dirty-check in `draw_status_bar`.
#[derive(Clone)]
pub(crate) struct StatusBarCtx {
    pub(crate) stdout: Arc<Mutex<io::Stdout>>,
    pub(crate) term: Arc<Mutex<TermGeom>>,
    pub(crate) paths: Paths,
    pub(crate) record: Arc<Mutex<SessionRecord>>,
    /// The worker-side facts the bar shows (memory, foreground, reported
    /// state, siblings), fetched by the status thread and rendered from by
    /// everyone else -- see `LiveStatus` for why no render may fetch.
    pub(crate) live: Arc<Mutex<LiveStatus>>,
    pub(crate) flash: Arc<Mutex<Option<(String, Instant)>>>,
    /// (text, rows, cols, workload margins) last actually written, so an
    /// unchanged bar isn't rewritten every debounce tick -- see
    /// `draw_status_bar`'s doc comment and
    /// docs/low-bandwidth-remote-access-design.md section 2.1.
    pub(crate) last_drawn: Arc<Mutex<LastDrawnStatus>>,
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
    pub(crate) screen: Arc<Mutex<aplexer::screen::ClientScreen>>,
    /// Set when a redraw was wanted but the stream was not at a safe boundary
    /// (or was inside a synchronized-output frame). The main frame loop
    /// flushes it at the first boundary that is safe, so deferring never
    /// means dropping.
    pub(crate) pending: Arc<AtomicBool>,
    /// Set when `Ctrl-b r` wanted a full live-screen repaint but the stream
    /// was not at a safe boundary. Flushed by the main frame loop the same
    /// way as `pending`; a successful refresh also redraws the status bar,
    /// so it subsumes a pending bar redraw.
    pub(crate) pending_refresh: Arc<AtomicBool>,
    /// The physical geometry a terminal resize wanted to reserve a row out
    /// of, parked here because the relayed stream was mid-escape-sequence
    /// when the resize poller fired (issue #14). Flushed by
    /// `flush_pending_layout` from both the frame loop and the status
    /// thread, so a deferred resize is delivered late, never dropped.
    pub(crate) pending_layout: Arc<Mutex<Option<PendingLayout>>>,
    /// When the current synchronized-output deferral started, so
    /// `STATUS_BAR_SYNC_DEFER_LIMIT` can bound it.
    pub(crate) sync_deferred_since: Arc<Mutex<Option<Instant>>>,
    /// Scroll mode (`Ctrl-b [`, or a wheel roll): whether the pager is up
    /// and where in the retained history it is looking. Read by the relay on
    /// every chunk to decide whether the host may be written to at all.
    pub(crate) scroll: Arc<ScrollMode>,
    /// The which-key overlay: whether the `Ctrl-b` keymap is currently drawn
    /// over the screen. Read by the relay on every chunk for the same reason
    /// `scroll` is -- while a modal owns the host, the model keeps eating
    /// bytes and the terminal is written nothing.
    pub(crate) overlay: Arc<KeyOverlay>,
    /// Who currently owns mouse reporting on the host: `Some(true)` this
    /// client (so the wheel reaches `a`), `Some(false)` the workload,
    /// `None` nothing asserted yet. See `sync_client_mouse`.
    pub(crate) mouse_owned: Arc<Mutex<Option<bool>>>,
    /// Whether borrowing the mouse is permitted at all (`APLEXER_MOUSE`).
    pub(crate) mouse_capture: bool,
}

type LastDrawnStatus = Option<(String, u16, u16, Option<(u16, u16)>)>;

/// How long a transient status-bar message (switch failure, attach hint,
/// `Ctrl-b ?` help) stays visible before the normal text resumes
/// (docs/fast-session-switching-design.md section 6.1). Three seconds
/// rather than two: help text has to be readable, not merely noticed.
pub(crate) const FLASH_DURATION: Duration = Duration::from_secs(3);

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
pub(crate) struct AttachBinding {
    /// The keys, as the `a keys` listing's left column shows them.
    pub(crate) keys: &'static str,
    /// `key label` for the one-line status-bar flash, which has a terminal
    /// width to live inside; `None` keeps a binding out of that line only.
    /// Order here is the order shown, and the flash is truncated from the
    /// right, so the entries most worth seeing on an 80-column terminal come
    /// first.
    pub(crate) brief: Option<&'static str>,
    /// The sentence `a keys` prints.
    pub(crate) description: &'static str,
}

pub(crate) const ATTACH_BINDINGS: &[AttachBinding] = &[
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
pub(crate) fn attach_key_help() -> String {
    let brief: Vec<&str> = ATTACH_BINDINGS.iter().filter_map(|b| b.brief).collect();
    format!("Ctrl-b: {}", brief.join(" · "))
}

/// Shows a transient message on the status bar and redraws immediately --
/// the single channel for attach hints, help, and switch failures, so
/// nothing is ever printed into the workload's output stream (the original
/// attach banner's corruption failure mode, docs/terminal-state-design.md
/// section 6.3 step 6).
pub(crate) fn flash_status(ctx: &StatusBarCtx, message: impl Into<String>) {
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
pub(crate) fn status_bar_text(ctx: &StatusBarCtx, cols: usize) -> String {
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
    // Nothing below leaves the process: the worker round-trip, the agent
    // detection and the registry read all happened on the status thread
    // (`LiveStatus`).
    let live = cached_live_status(ctx, record.id);
    let home = env::var_os("HOME").map(PathBuf::from);
    let ws = display_workspace(&record.workspace, home.as_deref());
    let mut ep = engine_profile(&record);
    let raw = live.raw;
    // Which agent is live in this session right now -- the same query-time
    // detection every JSON surface carries (`api::record_detected`). When it
    // names the same program as the live foreground read, the foreground
    // annotation steps aside -- `claude  shell -> claude` would say claude
    // twice -- so an agent not in the foreground (claude running, vim in
    // front) shows both facts: `claude  shell -> vim`.
    let agent = extra_agent_label(&record, live.agent.as_ref());
    let foreground = raw
        .as_ref()
        .and_then(|raw| foreground_override(&record, raw))
        .filter(|fg| Some(fg.as_str()) != agent.as_deref());
    if let Some(fg) = foreground {
        ep.push_str(&format!(" -> {fg}"));
    }
    let agent_segment = agent.map(|name| format!("  {name}")).unwrap_or_default();
    let mem = raw.as_ref().and_then(|raw| memory_indicator(&record, raw));
    let siblings = live.siblings;
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
    let tag = &record.tag;
    let sibling_segment = if siblings.is_empty() {
        String::new()
    } else {
        format!("  |  {siblings}")
    };

    // Widest first; `fit_bar_text` renders each only until one fits.
    let full = || {
        let mem = mem
            .as_ref()
            .map(|mem| format!("  mem {mem}"))
            .unwrap_or_default();
        format!("{ws}:{tag}  {state}{agent_segment}  {ep}{mem}{sibling_segment}  |  ^b ?")
    };
    let medium = || format!("{tag}  {state}{agent_segment}  {ep}{sibling_segment}  |  ^b ?");
    let compact = || format!("{tag}  {state}{agent_segment}  ^b ?");
    let candidates: [&dyn Fn() -> String; 3] = [&full, &medium, &compact];
    fit_bar_text(
        cols,
        candidates.into_iter().map(|render| render()),
        &format!("{state}  ^b ?"),
    )
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
pub(crate) fn draw_status_bar(ctx: &StatusBarCtx, force: bool) -> bool {
    // The stdout lock is taken *before* `status_bar_redraw` consults the
    // client's terminal model, and held across the write. Checking the escape
    // boundary and then writing without the lock is a race the frame loop
    // wins about 10% of the time (measured: 4 of 39 redraws in a real capture
    // still landed mid-CSI) -- it writes another chunk in between, and the
    // "safe" answer the status thread got is stale by the time its bytes go
    // out. See `write_locked` for the lock order this relies on.
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
pub(crate) fn redraw_live_screen(ctx: &StatusBarCtx) -> bool {
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
pub(crate) fn live_screen_refresh_locked(ctx: &StatusBarCtx) -> Option<Vec<u8>> {
    if defer_unless_safe(ctx, &ctx.pending_refresh) {
        return None;
    }
    ctx.pending_refresh.store(false, Ordering::Relaxed);
    let mut seq = host_snapshot(&ctx.screen);
    if let Some((geom, text)) = status_bar_render(ctx) {
        // Gated once, above, for the whole sequence: the bar rides on the
        // snapshot's boundary.
        if let Some(bar) = status_bar_changed_sequence(ctx, geom, &text, true) {
            seq.extend_from_slice(&bar);
        }
    }
    Some(seq)
}

/// The live screen as the host should be repainted with it:
/// `ClientScreen::snapshot` -- grid, cursor, input modes, margins -- with
/// the workload's alt-screen switches kept off the wire (`filter_host`).
/// Every full repaint the client makes starts from this: `Ctrl-b r`, the
/// pager's exit and type-through entry, and the key overlay going up or
/// down.
pub(crate) fn host_snapshot(screen: &Arc<Mutex<aplexer::screen::ClientScreen>>) -> Vec<u8> {
    screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .host_snapshot()
}

/// `host_snapshot` followed by the status bar, whose reserved row the
/// snapshot's ED2 just blanked, with the workload's cursor and pen put back
/// absolutely from the model. `bar` is `status_bar_render`'s answer, taken
/// before the caller took the stdout lock.
pub(crate) fn live_screen_sequence(ctx: &StatusBarCtx, bar: Option<(TermGeom, String)>) -> Vec<u8> {
    let mut seq = host_snapshot(&ctx.screen);
    if let Some((geom, text)) = bar {
        let (restore, margins) = {
            let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
            (screen.cursor_restore(), screen.margins())
        };
        seq.extend_from_slice(&status_bar_sequence(geom, &text, margins, &restore));
    }
    seq
}

/// Geometry plus the rendered bar text, or `None` when the terminal has no
/// reserved row. Computed before the stdout lock is taken by convention:
/// the text is a pure format of the record and the `LiveStatus` cache, so
/// nothing here can block the PTY relay, but nothing needs the lock either.
pub(crate) fn status_bar_render(ctx: &StatusBarCtx) -> Option<(TermGeom, String)> {
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
pub(crate) fn status_bar_redraw(ctx: &StatusBarCtx, force: bool) -> Option<Vec<u8>> {
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
pub(crate) fn status_bar_redraw_locked(
    ctx: &StatusBarCtx,
    geom: TermGeom,
    text: &str,
    force: bool,
) -> Option<Vec<u8>> {
    if defer_unless_safe(ctx, &ctx.pending) {
        return None;
    }
    status_bar_changed_sequence(ctx, geom, text, force)
}

/// The escape-boundary gate for a client injection into the live stream,
/// as every gated writer asks it: `true` means the bytes must wait, and the
/// caller's parking flag has been raised so the frame loop retries at the
/// next chunk.
///
/// The client is a raw byte relay, so a PTY read boundary lands at an
/// arbitrary offset in the workload's output: "between two chunks" is not
/// "between two escape sequences". Writing anywhere else splices our
/// `\x1b...` into the middle of the workload's half-emitted sequence (or
/// its half-emitted UTF-8 character); the host terminal abandons the
/// partial sequence and prints its remaining parameter bytes as literal
/// text into the workload's own frame. That is the reported corruption,
/// and it is not fixable by re-timing -- only by asking the stream.
///
/// Deferring is never dropping: `parked` is flushed by the main frame loop
/// at the first safe boundary, which is at most one PTY chunk away. The
/// answer is only worth having under the stdout lock (`write_locked`), so
/// each writer asks once, there; a pre-check outside the lock is a second
/// read of a value the relay may change before the write.
pub(crate) fn defer_unless_safe(ctx: &StatusBarCtx, parked: &AtomicBool) -> bool {
    let (at_boundary, in_sync) = {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        (screen.at_escape_boundary(), screen.in_synchronized_update())
    };
    if !at_boundary || sync_defer(ctx, in_sync) {
        parked.store(true, Ordering::Relaxed);
        return true;
    }
    false
}

/// The bar's bytes when its text or geometry changed since the last write
/// (or unconditionally with `force`), recorded as drawn; `None` when the
/// dirty check skipped it. Not gated: the caller has already asked
/// `defer_unless_safe` under the stdout lock.
pub(crate) fn status_bar_changed_sequence(
    ctx: &StatusBarCtx,
    geom: TermGeom,
    text: &str,
    force: bool,
) -> Option<Vec<u8>> {
    let (restore, workload_margins) = {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        (screen.cursor_restore(), screen.margins())
    };
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
pub(crate) fn sync_defer(ctx: &StatusBarCtx, in_sync: bool) -> bool {
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
pub(crate) fn status_bar_sequence(
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
