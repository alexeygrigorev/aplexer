use super::*;

/// Prime a freshly built client model with a tail of the worker's retained
/// raw history, so `Ctrl-b [` has a past to page through from the first
/// second of the attach rather than only what arrives afterwards.
///
/// Failure is not an error the user should see: a worker too old to answer,
/// a session that just exited, or a history file that has been rotated all
/// mean "no history to seed", and the attach continues with an empty grid
/// that fills from the live stream.
pub(crate) fn seed_client_scrollback(
    screen: &Arc<Mutex<aplexer::screen::ClientScreen>>,
    record: &SessionRecord,
) {
    if history_limit() == 0 {
        return;
    }
    let Ok(tail) = rpc_capture(record, Some(scrollback_seed_bytes())) else {
        return;
    };
    screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .seed_history(&tail);
}

/// Rebuild the pager's history from the worker's *current* retained tail
/// before `Ctrl-b [` (or the wheel) opens it.
///
/// The attach-time seed (`seed_client_scrollback`) only helps a client that
/// attached after the output happened. A session attached from before it --
/// `a new`, the default flow -- accumulates history through the live relay,
/// where `vt100` is right to drop every row scrolled out of a DECSTBM
/// sub-range; the codex TUI holds a sub-range almost constantly, so its pager
/// opened on `SCROLL 0/0` no matter how long it had been running. Re-running
/// the seed at entry gives the live attach the same past a fresh attach gets
/// (`ClientScreen::refresh_scrollback` for the model-side mechanics).
///
/// The gate keeps every other workload exactly as it is. `subregion_seen` is
/// false for anything that never sent a sub-range -- shells, logs, tail -f --
/// and those sessions' live-maintained history is complete, so they pay
/// neither the two bounded RPCs (capture + screen, ~13-40 ms of replay
/// measured at `scrollback_seed_bytes`) nor the risk of a byte-capped replay
/// holding fewer rows than their live scrollback already does. The alternate
/// screen skips too: that grid has no scrollback anywhere, so there is
/// nothing to rebuild for a full-screen application, only RPCs to spend.
///
/// The tail is fetched **whole** (`rpc_capture`'s `None`: everything the
/// worker still retains, bounded by `DEFAULT_HISTORY_BYTES`), not at the
/// 2 MiB attach-seed budget. A byte budget is not a row budget -- an agent
/// idling between turns spends its bytes on a spinner that replays to zero
/// rows -- so a budgeted tail can rebuild to *less* than the model already
/// holds, and `refresh_scrollback` rightly refuses to adopt it: the pager
/// would fossilize at whatever its entry happened to catch instead of
/// tracking the transcript. The whole buffer is the most any rebuild can
/// ever know; its parse is bounded by the worker's retention and runs once
/// per explicit pager entry, never on the attach or `Ctrl-b` switch paths
/// (`scrollback_seed_bytes` keeps those cheap). A refusal now costs the
/// replay alone, never the history.
///
/// Failure is invisible, exactly like the attach seed: a worker that has gone
/// away or stopped answering means "page through whatever the live model
/// has", which is the pre-refresh behavior.
pub(crate) fn refresh_pager_history(ctx: &StatusBarCtx) {
    if history_limit() == 0 {
        return;
    }
    let record = ctx
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        if screen.alternate_screen() || !screen.subregion_seen() {
            return;
        }
    }
    let Ok(tail) = rpc_capture(&record, None) else {
        return;
    };
    if tail.is_empty() {
        return;
    }
    let Ok(snapshot) = rpc_capture_screen(&record, false) else {
        return;
    };
    ctx.screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .refresh_scrollback(&tail, &snapshot);
}

/// Result of `establish()`: the connected/subscribed stream, its initial
/// payload (either a live-screen snapshot or a raw-tail replay -- see
/// `screen`), and enough of the response to know which one it got.
pub(crate) struct AttachHandshake {
    pub(crate) reader: UnixStream,
    pub(crate) initial: Vec<u8>,
    /// The response's `"screen"` field: `Some(true)`/`Some(false)` from a
    /// worker new enough to report it, `None` from an old worker whose
    /// response predates the field entirely (docs/terminal-state-design.md
    /// section 6.1's compatibility matrix) -- used to decide whether the
    /// explicit post-connect Resize control send is still needed (section
    /// 6.3 step 7).
    pub(crate) screen: Option<bool>,
}

/// Extracted attach handshake (connect + `Operation::Attach` request +
/// response check + initial payload frame), used by both the initial
/// attach and every in-process switch (docs/fast-session-switching-design.md
/// section 3.1).
///
/// `want_screen` requests the live-screen snapshot (docs/terminal-state-design.md
/// section 6.1); `geometry`, when known (a real tty), is `(rows, cols)`
/// already reserved-rows-adjusted by the caller -- sent so the worker can
/// resize the PTY and its screen model *before* rendering the snapshot, so
/// there is no wrong-size frame followed by a SIGWINCH repaint (section
/// 6.3 step 1). An old worker's serde simply ignores these unknown request
/// fields and falls back to today's raw-tail replay -- no worse than
/// before.
pub(crate) fn establish(
    record: &SessionRecord,
    replay_bytes: Option<usize>,
    want_screen: bool,
    geometry: Option<(u16, u16)>,
) -> Result<AttachHandshake> {
    let mut reader = connect(record)?;
    let (rows, cols) = match geometry {
        Some((rows, cols)) => (Some(rows), Some(cols)),
        None => (None, None),
    };
    let request = Request::new(
        record.id,
        Operation::Attach {
            history_bytes: replay_bytes,
            want_screen,
            rows,
            cols,
        },
    );
    let id = request.request_id.clone();
    write_json(&mut reader, &request)?;
    let response: Response =
        frame_json(read_frame(&mut reader)?.ok_or_else(|| anyhow!("missing attach response"))?)?;
    if response.request_id != id {
        bail!("response request id mismatch");
    }
    let result = response.into_result()?;
    let screen = result.get("screen").and_then(|v| v.as_bool());
    let initial = read_frame(&mut reader)?.ok_or_else(|| anyhow!("missing history frame"))?;
    if initial.kind != FrameKind::Data {
        bail!("expected history data");
    }
    // Only the handshake is an RPC. Once subscribed, silence is a normal
    // state for an interactive terminal and must not detach the client.
    clear_streaming_deadlines(&reader)?;
    Ok(AttachHandshake {
        reader,
        initial: initial.payload,
        screen,
    })
}

/// Default amount of history replayed on an in-process switch
/// (`Ctrl-b n/p/N/P/l/1-9`), separate from `DEFAULT_ATTACH_REPLAY_BYTES`
/// used by a fresh `a attach`.
///
/// Deviation from docs/fast-session-switching-design.md section 3.1, which
/// specifies reusing the exact same replay budget as a fresh attach: a
/// switch target is a session the user was just attached to, or explicitly
/// picked off the status bar's own sibling list -- not a cold, unfamiliar
/// session -- so the "show what's currently on its screen" justification
/// for a full 32KB tail is considerably weaker than on first attach, and
/// every byte here sits on the hot path the user actually experiences as
/// switch latency (it's written to the real terminal, and the terminal
/// emulator parsing/painting it dominates the whole switch -- see the
/// design doc's own section 8 budget). 4KB is still ~2-4 screens of typical
/// tail -- comfortably enough for a shell prompt or an agent CLI's last few
/// lines -- at 1/8th the bytes and, per informal measurement during
/// implementation, a visibly snappier repaint than 32KB on a small
/// terminal. An explicit `--history-bytes` from the CLI is still honored
/// (see `switch_replay_bytes` in `attach()`) -- this only changes the
/// *default*, the same way `DEFAULT_ATTACH_REPLAY_BYTES` is only a default.
pub(crate) const SWITCH_REPLAY_BYTES: usize = 4 * 1024;

/// A fully established connection to the new session, handed from the
/// input thread (which runs `perform_switch`) to the main frame loop
/// (which installs it -- see the `'session` loop in `attach()`).
pub(crate) struct SwitchOutcome {
    pub(crate) record: SessionRecord,
    /// Attach handshake already completed on this socket.
    pub(crate) reader: UnixStream,
    /// The replay tail read during that handshake.
    pub(crate) history: Vec<u8>,
}

/// One button press/release reported by xterm's SGR extended mouse mode
/// (`CSI ?1006h`, paired with `CSI ?1000h` click tracking) --
/// docs/clickable-status-bar-design.md section 2. `col`/`row` are 1-based,
/// matching the wire format, so callers subtract 1 to index into
/// `BarRegion` column ranges or compare against `TermGeom.rows`.
///
/// **Not yet wired into `InputScanner`/`attach()`** -- see the design doc
/// section 7 for why this is landing as a standalone, unit-tested primitive
/// ahead of the riskier live-input-thread integration.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MouseReport {
    pub(crate) button: u32,
    pub(crate) press: bool,
    pub(crate) col: u16,
    pub(crate) row: u16,
}

/// Result of attempting to parse an SGR mouse report off the front of a
/// buffer: a real hit (with the byte length consumed), "not this at all"
/// (any other byte sequence, including ordinary CSI sequences like arrow
/// keys -- `ESC [ <` is not a prefix any keyboard-generated input or other
/// terminal report uses, so this is an unambiguous, fast rejection), or
/// "looks like the start of one but the buffer ends before `M`/`m`" -- the
/// signal a live scanner needs to keep buffering across `read()` calls, the
/// same role `pending_ctrl_b` plays for the one-byte `Ctrl-b` prefix.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseParse {
    NotMouse,
    Incomplete,
    Complete(MouseReport, usize),
}

/// Pure parser for `ESC [ < Cb ; Cx ; Cy [Mm]` at the start of `buf`
/// (docs/clickable-status-bar-design.md section 2/4.4). Never panics on
/// malformed input; malformed-but-prefix-matching input that can't
/// possibly resolve (non-digit where a number is expected, once the `<`
/// has been seen) is reported `NotMouse` rather than `Incomplete`, so a
/// caller doesn't buffer forever waiting for a `M`/`m` that will never
/// come.
#[allow(dead_code)]
pub(crate) fn parse_sgr_mouse(buf: &[u8]) -> MouseParse {
    const PREFIX: &[u8] = b"\x1b[<";
    if buf.len() < PREFIX.len() {
        if PREFIX.starts_with(buf) {
            return MouseParse::Incomplete;
        }
        return MouseParse::NotMouse;
    }
    if &buf[..PREFIX.len()] != PREFIX {
        return MouseParse::NotMouse;
    }
    // Three ';'-separated decimal fields, terminated by 'M' (press) or 'm'
    // (release). Parse by scanning for the terminator rather than
    // pre-splitting, so a genuinely truncated buffer (no terminator yet)
    // is correctly reported Incomplete instead of NotMouse.
    let rest = &buf[PREFIX.len()..];
    let mut fields: [u32; 3] = [0; 3];
    let mut field_idx = 0;
    let mut cur: u32 = 0;
    let mut have_digit = false;
    for (i, &b) in rest.iter().enumerate() {
        match b {
            b'0'..=b'9' => {
                have_digit = true;
                cur = cur.saturating_mul(10).saturating_add((b - b'0') as u32);
            }
            b';' => {
                if !have_digit || field_idx >= 2 {
                    return MouseParse::NotMouse;
                }
                fields[field_idx] = cur;
                field_idx += 1;
                cur = 0;
                have_digit = false;
            }
            b'M' | b'm' => {
                if !have_digit || field_idx != 2 {
                    return MouseParse::NotMouse;
                }
                fields[2] = cur;
                let consumed = PREFIX.len() + i + 1;
                let row = u16::try_from(fields[2]).unwrap_or(u16::MAX);
                let col = u16::try_from(fields[1]).unwrap_or(u16::MAX);
                return MouseParse::Complete(
                    MouseReport {
                        button: fields[0],
                        press: b == b'M',
                        col,
                        row,
                    },
                    consumed,
                );
            }
            _ => return MouseParse::NotMouse,
        }
    }
    // Ran out of buffer with no terminator yet, but every byte seen so far
    // was a valid digit/`;` -- genuinely incomplete, keep buffering.
    MouseParse::Incomplete
}

/// A clickable span of the rendered status-bar line
/// (docs/clickable-status-bar-design.md section 4.1), in 0-based display-cell
/// columns `[start, end)` -- the same units `pad_or_truncate` counts in, so
/// a click's 1-based `Cx` maps in with a single `- 1`.
///
/// **Not yet wired into `draw_status_bar`** -- see the design doc section 7.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BarRegion {
    pub(crate) cols: std::ops::Range<usize>,
    pub(crate) action: BarClick,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BarClick {
    /// Click a sibling's `{i}:{tag}` token: switch to it (`i` is exactly
    /// the digit `Ctrl-b <i>` would send, `SwitchTarget::Index`).
    Sibling(usize),
    /// Click the cross-workspace picker indicator (design doc section 5.2).
    WorkspacePicker,
    /// Click a session while browsing another workspace's sibling list
    /// (design doc section 5.2/5.3): jump straight to it by identity.
    RemoteSession(Uuid),
}

/// Pure builder mirroring `workspace_summary`'s rendering
/// (`{i}:{tag}[*][(state)]`, space-joined, `list_records` order) but also
/// returning the column range each token occupies, for the status-bar
/// click map (docs/clickable-status-bar-design.md section 4.2). `siblings`
/// must already be filtered to one workspace and ordered the way
/// `workspace_summary` expects; kept pure (no `Paths`/filesystem access)
/// so it's testable without touching disk, the same split
/// `pick_switch_target`/`resolve_switch_target` already use.
///
/// **Not yet called from `draw_status_bar`** -- see the design doc
/// section 7; `workspace_summary` (the currently-live renderer) is
/// untouched by this addition.
#[allow(dead_code)]
pub(crate) fn workspace_summary_regions(
    siblings: &[SessionRecord],
    current_id: Uuid,
) -> (String, Vec<BarRegion>) {
    let mut text = String::new();
    let mut regions = Vec::new();
    for (i, r) in siblings.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        let start = terminal_display_width(&text);
        let (state, _) = session_ui_state(r, now_ms());
        text.push_str(&format!("{}:{}", i + 1, sanitize_terminal_text(&r.tag)));
        if r.id == current_id {
            text.push('*');
        }
        if !matches!(state, "running" | "working" | "active" | "quiet") {
            text.push_str(&format!("({state})"));
        }
        let end = terminal_display_width(&text);
        regions.push(BarRegion {
            cols: start..end,
            action: BarClick::Sibling(i + 1),
        });
    }
    (text, regions)
}

/// Atomic switch-or-stay, run on the input thread
/// (docs/fast-session-switching-design.md section 3.3). The critical
/// ordering property: the new connection is fully established *before* the
/// old one is touched, so any failure (resolution, `check_attachable`, or
/// `establish` itself) leaves the attachment to the current session
/// completely undisturbed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn perform_switch(
    paths: &Paths,
    target: SwitchTarget,
    replay_bytes: Option<usize>,
    want_screen: bool,
    term: &Arc<Mutex<TermGeom>>,
    shared_record: &Arc<Mutex<SessionRecord>>,
    last_session: &Arc<Mutex<Option<Uuid>>>,
    writer: &Arc<Mutex<UnixStream>>,
    pending_switch: &Arc<Mutex<Option<SwitchOutcome>>>,
    switch_in_progress: &Arc<AtomicBool>,
) -> Result<()> {
    let current = shared_record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let last = *last_session.lock().unwrap_or_else(PoisonError::into_inner);
    // Current terminal geometry (docs/fast-session-switching-design.md
    // section 5.2 / docs/terminal-state-design.md section 10.2): passed
    // into the Attach so the new session's snapshot renders at the right
    // size immediately, rather than relying solely on the post-switch
    // explicit Resize send below. Read before resolution because
    // `SwitchTarget::New` also hands it to the worker it starts.
    let geometry = term
        .lock()
        .ok()
        .map(|g| *g)
        .filter(|g| g.rows > 0)
        .map(|g| (reserved_rows(g.rows), g.cols));
    // `New` makes its target instead of picking one; everything below is the
    // same for both, which is what keeps "which key creates a session" a
    // one-line change in `InputScanner::scan`. Creation happens here, still
    // before the current attachment has been touched, so a failed create is
    // indistinguishable from a failed resolve: an Err, and the user stays put.
    let next = match target {
        SwitchTarget::New => create_sibling_session(paths, &current, geometry)?,
        _ => resolve_switch_target(paths, &current, target, last)?,
    };
    if next.id == current.id {
        return Ok(()); // switching to yourself: silent no-op
    }
    check_attachable(&next)?;
    switch_in_progress.store(true, Ordering::Relaxed);
    let result = (|| -> Result<()> {
        let handshake = establish(&next, replay_bytes, want_screen, geometry)?;
        let reader = handshake.reader;
        let history = handshake.initial;
        let writer_clone = reader.try_clone()?; // before mutating anything
                                                // Repoint every forwarding thread (input, resize) at B, then retire
                                                // A's stream. From this instant keystrokes land in B.
        let old = {
            let mut w = writer.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *w, writer_clone)
        };
        *pending_switch
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(SwitchOutcome {
            record: next.clone(),
            reader,
            history,
        });
        *last_session.lock().unwrap_or_else(PoisonError::into_inner) = Some(current.id);
        // Polite detach from A, then shutdown so the main loop's blocked
        // read_frame on A's socket returns immediately. shutdown() is
        // socket-wide, so it also unblocks the reader fd cloned from this
        // stream -- the same mechanism the existing detach path relies on.
        let mut old = old;
        let _ = write_json(&mut old, &AttachControl::Detach);
        let _ = old.shutdown(std::net::Shutdown::Both);
        Ok(())
    })();
    switch_in_progress.store(false, Ordering::Relaxed);
    result
}

/// Closes the race where the frame loop breaks because A's worker died at
/// the same moment the user pressed a switch chord, *before* the input
/// thread finished storing the outcome: if `pending_switch` is `None` but
/// `switch_in_progress` is true, poll briefly for the outcome before giving
/// up and treating it as a normal exit (docs/fast-session-switching-design.md
/// section 5.2).
pub(crate) fn take_pending_switch(
    pending: &Arc<Mutex<Option<SwitchOutcome>>>,
    in_progress: &Arc<AtomicBool>,
) -> Option<SwitchOutcome> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if let Some(o) = pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Some(o);
        }
        if !in_progress.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Ends the current attach from the input side. Sending the polite control
/// frame lets the worker release its subscriber immediately; shutting down
/// our socket locally guarantees the main frame loop wakes even if the peer
/// cannot read the control frame. This is shared by explicit detach, stdin
/// EOF, and terminal read/write failure so none can strand `attach()` in its
/// blocking socket read.
pub(crate) fn detach_attached_client(writer: &Arc<Mutex<UnixStream>>, active: &Arc<AtomicBool>) {
    let _ = send_control(writer, &AttachControl::Detach);
    active.store(false, Ordering::Relaxed);
    let stream = writer.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Why the attach frame loop stopped. The goodbye line is the user's
/// diagnosis of which layer to look at, so "Detached" is reserved for a
/// client that left on purpose -- a worker-side error or a dropped socket
/// must not borrow that word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachStop {
    /// Ctrl-b d, stdin EOF, or a terminal read/write failure that made this
    /// client tear its own attach down.
    ClientDetached,
    /// The worker reported the workload gone (End frame we did not cause, or
    /// `ServerEvent::Exit`).
    SessionEnded,
    /// The worker sent `ServerEvent::Error`: a PTY/waiter failure, a
    /// containment cleanup that could not be proven, or a raw-tail
    /// (`--history-bytes`) subscriber evicted for falling behind. Live-screen
    /// subscribers coalesce instead of being evicted (issue #16), so for a
    /// plain `a attach` this now means a genuine worker-side failure.
    WorkerError,
    /// The socket ended under us with no explanation: EOF, ConnectionReset,
    /// or UnexpectedEof. The worker is unreachable; the session may well
    /// still be fine.
    SocketLost,
}

/// Client intent wins over everything but a session that actually ended:
/// Ctrl-b d shuts our own stream down, so the frame loop very often then
/// observes a reset that must not be reported as a connection loss.
pub(crate) fn classify_attach_stop(
    session_ended: bool,
    detached_by_client: bool,
    worker_error: bool,
) -> AttachStop {
    if session_ended {
        AttachStop::SessionEnded
    } else if detached_by_client {
        AttachStop::ClientDetached
    } else if worker_error {
        AttachStop::WorkerError
    } else {
        AttachStop::SocketLost
    }
}

/// `inspect_id` is the short session id when a record survived the session's
/// end (Failed/OOM leftovers); a clean exit removes the record and leaves
/// nothing to point the user at.
pub(crate) fn attach_goodbye_line(
    stop: AttachStop,
    selector: &str,
    inspect_id: Option<&str>,
) -> String {
    match stop {
        AttachStop::ClientDetached => format!("Detached from {selector}."),
        AttachStop::SessionEnded => match inspect_id {
            Some(id) => format!(
                "Session ended: {selector}. Inspect output with `a capture {id} --screen --plain`."
            ),
            None => format!("Session ended: {selector}."),
        },
        AttachStop::WorkerError => format!("Attach dropped: {selector}."),
        AttachStop::SocketLost => format!("Connection to {selector} lost."),
    }
}

/// Refuse to render a session inside another live aplexer session's pane.
/// Two full-screen clients on one terminal fight over the DECSTBM margins,
/// alternate-screen state, and the `Ctrl-b` chord, and the inner session's
/// output replaces what the outer session's agent believes is on screen --
/// the same reason tmux routes `switch-client` through its server. The
/// aplexer-shaped answer is already shipped: detach (`Ctrl-]`), then let
/// the now-outer client do the in-process `Ctrl-b` switch. The inner id
/// comes from `discover_session_id` (the env var, or the ancestor /proc
/// walk when an agent runs `a attach` with a scrubbed env) and is only
/// acted on when it resolves to a live worker in *this* runtime dir, so a
/// stale id inherited from elsewhere costs one failed record read and the
/// common not-in-a-session attach pays one `getenv`. `--force` opts in for
/// a deliberate peek at the cost of the nesting above.
pub(crate) fn nested_attach_conflict(paths: &Paths) -> Result<()> {
    nested_attach_conflict_for(paths, discover_session_id())
}

/// `nested_attach_conflict` with the inner session id injected, so tests
/// never mutate process-global state.
pub(crate) fn nested_attach_conflict_for(paths: &Paths, inner: Option<Uuid>) -> Result<()> {
    let Some(inner) = inner else {
        return Ok(());
    };
    let inner_record = match read_record(&paths.record(inner)) {
        Ok(record) => record,
        Err(_) => return Ok(()),
    };
    if !inner_record.worker_alive() {
        return Ok(());
    }
    bail!(
        "already inside session {}/{}, refusing to render another nested in \
         its pane -- detach first (Ctrl-]), then switch with Ctrl-b arrows; \
         or pass --force to attach nested anyway",
        inner_record.workspace.display(),
        inner_record.tag
    )
}
