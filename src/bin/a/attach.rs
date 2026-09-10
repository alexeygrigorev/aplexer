use super::*;

pub(crate) fn attach(
    paths: &Paths,
    record: &SessionRecord,
    history_bytes: Option<usize>,
    no_status: bool,
    force: bool,
) -> Result<()> {
    if !force {
        nested_attach_conflict(paths)?;
    }
    check_attachable(record)?;
    let explicit_history = history_bytes.is_some();
    let replay_bytes = Some(history_bytes.unwrap_or(DEFAULT_ATTACH_REPLAY_BYTES));
    let input_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
    let display_tty = unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
    // PocketShell owns the session chrome. In this mode the host terminal is
    // a full-screen relay: no reserved status row, redraw thread, flash hint,
    // or Ctrl-b scanner is installed.
    let status_enabled = display_tty && !no_status;
    // Geometry read up front (docs/terminal-state-design.md section 6.3
    // step 1), not after the handshake: sent in the Attach request itself
    // so the worker can resize the PTY and its screen model *before*
    // rendering the snapshot below -- no wrong-size frame followed by a
    // SIGWINCH repaint. `--history-bytes` is the explicit escape hatch back
    // to the old raw-tail semantics (section 6.1); `want_screen` follows
    // its absence.
    let initial_geometry = if display_tty {
        terminal_size(libc::STDOUT_FILENO)
    } else if input_tty {
        terminal_size(libc::STDIN_FILENO)
    } else {
        None
    };
    let worker_geometry = initial_geometry.map(|(rows, cols)| {
        (
            if status_enabled {
                reserved_rows(rows)
            } else {
                rows
            },
            cols,
        )
    });
    let handshake = establish(record, replay_bytes, !explicit_history, worker_geometry)?;
    let mut reader = handshake.reader;
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let _raw = if input_tty {
        Some(RawMode::enter(libc::STDIN_FILENO)?)
    } else {
        None
    };
    // Display cleanup is independent of where input comes from. In
    // particular, `a attach </dev/null` still writes the snapshot to a tty
    // stdout before stdin EOF detaches, so it must undo that snapshot's
    // alternate-screen and input modes even though stdin was never raw.
    let _ui_guard = if display_tty {
        Some(TerminalUiGuard {
            stdout: stdout.clone(),
        })
    } else {
        None
    };
    let writer = Arc::new(Mutex::new(reader.try_clone()?));
    let active = Arc::new(AtomicBool::new(true));
    let mut signal_bridge = if input_tty || display_tty {
        Some(AttachSignalBridge::install(writer.clone(), active.clone())?)
    } else {
        None
    };
    let term = Arc::new(Mutex::new(TermGeom {
        rows: 0,
        cols: 0,
        reserved: false,
    }));
    // Last time PTY output was written to the real terminal -- read by the
    // status-bar thread to decide when it's a good moment to redraw (see
    // STATUS_BAR_IDLE_GAP's doc comment above).
    let last_activity = Arc::new(Mutex::new(Instant::now()));

    // -- Fast in-process session switching state (survives across
    //    switches, unlike `reader`/`writer`'s inner stream/`record`; see
    //    docs/fast-session-switching-design.md sections 2-3) --
    let shared_record = Arc::new(Mutex::new(record.clone()));
    let pending_switch: Arc<Mutex<Option<SwitchOutcome>>> = Arc::new(Mutex::new(None));
    let switch_in_progress = Arc::new(AtomicBool::new(false));
    let last_session: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
    let switch_replay_bytes = Some(history_bytes.unwrap_or(SWITCH_REPLAY_BYTES));
    // Sized to the *workload's* geometry (the terminal minus the reserved bar
    // row), so the model's coordinates are the host's coordinates for every
    // row the workload can reach -- see `StatusBarCtx::screen`.
    let (screen_rows, screen_cols) = worker_geometry.unwrap_or((
        aplexer::screen::DEFAULT_TERMINAL_ROWS,
        aplexer::screen::DEFAULT_TERMINAL_COLS,
    ));
    // The client's model, unlike the worker's, retains scrollback: this is
    // the grid `Ctrl-b [` pages through, and giving it a real depth is what
    // makes an attached session's history readable at all (see the scroll
    // mode section above `history_limit`). The requested line count is
    // clamped against `MAX_SCROLLBACK_CELLS` at this terminal's width.
    let scrollback_lines = aplexer::screen::scrollback_lines_for(
        screen_cols,
        if status_enabled { history_limit() } else { 0 },
    );
    let workload_screen = Arc::new(Mutex::new(
        aplexer::screen::ClientScreen::try_new_with_scrollback(
            screen_rows,
            screen_cols,
            scrollback_lines,
        )?,
    ));
    let scroll_mode = Arc::new(ScrollMode::new());
    let key_overlay = Arc::new(KeyOverlay::default());
    let status_ctx = StatusBarCtx {
        stdout: stdout.clone(),
        term: term.clone(),
        paths: paths.clone(),
        record: shared_record.clone(),
        flash: Arc::new(Mutex::new(None)),
        last_drawn: Arc::new(Mutex::new(None)),
        screen: workload_screen.clone(),
        pending: Arc::new(AtomicBool::new(false)),
        pending_refresh: Arc::new(AtomicBool::new(false)),
        pending_layout: Arc::new(Mutex::new(None)),
        sync_deferred_since: Arc::new(Mutex::new(None)),
        scroll: scroll_mode.clone(),
        overlay: key_overlay.clone(),
        mouse_owned: Arc::new(Mutex::new(None)),
        mouse_capture: status_enabled && input_tty && mouse_capture_enabled(),
    };

    // Hold the host on the alternate screen for the whole attach, *before*
    // DECSTBM or the snapshot write anything. The primary screen -- and the
    // `a` list sitting on it -- stays frozen underneath, so host scrollback
    // cannot mix those rows into the live view. Workload 1049h/1049l still
    // update the model; `filter_host` keeps them off the wire.
    if display_tty {
        workload_screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .hold_host_on_alt_screen();
        let _ = write_locked(&stdout, ATTACH_ALT_SCREEN_ENTER);
        if status_enabled {
            if let Some((rows, cols)) = initial_geometry {
                // Nothing has been relayed yet, so the model is at a boundary by
                // construction and this always writes -- the gate is free here
                // and costs nothing to keep uniform.
                apply_terminal_layout(&status_ctx, rows, cols);
            }
        }
    }
    // Scanned before the bar is drawn: the snapshot re-emits the workload's
    // DECSTBM sub-range as its last bytes (design doc section 6.2 step 3), so
    // scanning it here is what lets the immediately-following `draw_status_bar`
    // re-assert that region instead of overwriting it with the bar's own.
    // Prime the retained history *before* the reattach snapshot, so its rows
    // sit above the screen the snapshot paints rather than after it. Model
    // only: not one byte of the seed reaches the terminal.
    if scrollback_lines > 0 {
        seed_client_scrollback(&workload_screen, record);
    }
    feed_and_write(&stdout, &workload_screen, b"", &handshake.initial, None)?;
    // After the snapshot, because the snapshot is what tells the model which
    // mouse modes the workload itself wants -- and the workload's wishes
    // decide whether the client may borrow the mouse at all.
    if status_enabled {
        sync_client_mouse(&status_ctx);
        // The attach hint goes through the status-bar flash channel, not an
        // eprintln'd banner: a banner written before/around the snapshot is
        // what once corrupted a live TUI's input box (docs/terminal-state-
        // design.md section 6.3 step 6 / section 10.1 item c). The flash is
        // drawn after the snapshot (whose ED2 blanked the bar row) and
        // disappears on its own after FLASH_DURATION.
        flash_status(
            &status_ctx,
            format!(
                "attached to {} · Ctrl-b ? help · Ctrl-b d detach",
                record.tag
            ),
        );
    }
    // The explicit post-connect Resize control send is unnecessary when the
    // Attach already carried geometry and a new-enough worker honored it
    // (the "screen" key is present in the response either way, true or
    // false); kept only for the old-worker fallback path, whose response
    // predates the field (section 6.3 step 7).
    if handshake.screen.is_none() {
        if let Some((rows, cols)) = worker_geometry {
            send_control(&writer, &AttachControl::Resize { rows, cols })?;
        }
    }

    // The input thread owns the scanner, pager routing, and switching
    // actions; attach only supplies its shared runtime state.
    // Set when THIS client ends its attach on purpose (Ctrl-b d, or its
    // stdin hit EOF) as opposed to the session ending under it. Combined
    // with `session_ended` / `worker_error` after the frame loop, this is
    // what keeps "Detached from ..." off the connection-loss and
    // worker-error paths -- see `classify_attach_stop`.
    let detached_by_client = Arc::new(AtomicBool::new(false));
    spawn_input_thread(InputThreadConfig {
        input_tty,
        status_enabled,
        want_screen: !explicit_history,
        replay_bytes: switch_replay_bytes,
        writer: writer.clone(),
        active: active.clone(),
        detached: detached_by_client.clone(),
        paths: paths.clone(),
        term: term.clone(),
        record: shared_record.clone(),
        last_session: last_session.clone(),
        pending_switch: pending_switch.clone(),
        switch_in_progress: switch_in_progress.clone(),
        status: status_ctx.clone(),
    });
    if display_tty {
        let resize_writer = writer.clone();
        let resize_active = active.clone();
        let resize_ctx = status_ctx.clone();
        let resize_initial = initial_geometry;
        let resize_status_enabled = status_enabled;
        thread::spawn(move || {
            // Seeded with the geometry `attach()` already applied and already
            // sent in the Attach request, so this thread reacts to *changes*
            // only. Starting from `None` made its very first poll look like a
            // resize and re-run `apply_terminal_layout` unconditionally --
            // harmless-looking, but it re-asserted `\x1b[1;{rows-1}r` and
            // dropped the workload scroll region the attach snapshot had just
            // restored (docs/terminal-state-design.md section 6.2 step 3),
            // roughly 200 ms after every attach.
            let mut last = resize_initial;
            while resize_active.load(Ordering::Relaxed) {
                let size = terminal_size(libc::STDOUT_FILENO);
                if size != last {
                    if let Some((rows, cols)) = size {
                        // Keep the client's tracker in step with the
                        // worker-side model across the same resize: both
                        // re-clamp the region to the new row count rather
                        // than dropping it (see `MarginTracker::set_rows`
                        // and design doc section 5.3's correction).
                        let worker_rows = if resize_status_enabled {
                            reserved_rows(rows)
                        } else {
                            rows
                        };
                        if let Ok(mut m) = resize_ctx.screen.lock() {
                            m.set_size(worker_rows, cols);
                        }
                        // May defer the DECSTBM write when the relayed
                        // stream is mid-escape-sequence (issue #14). The
                        // *workload* is never made to wait on that: the
                        // Resize control below goes out unconditionally, so
                        // the PTY is resized and SIGWINCH delivered on time
                        // whatever the host-side reservation is doing. Only
                        // the client's own row reservation waits, and only
                        // for as long as `LAYOUT_DEFER_LIMIT`.
                        if resize_status_enabled {
                            apply_terminal_layout(&resize_ctx, rows, cols);
                            // The pager renders at the reserved geometry, so a
                            // resize has to redraw it -- nothing else will,
                            // because the relay is suspended.
                            if resize_ctx.scroll.is_active() {
                                paint_scroll_view(&resize_ctx);
                            }
                            // Same for the key overlay, with one extra case: the
                            // new geometry may be one the box does not fit into
                            // at all, and a repaint that cannot happen must take
                            // the overlay down rather than leave a stale box over
                            // a suspended relay.
                            if resize_ctx.overlay.is_active() && !paint_key_overlay(&resize_ctx) {
                                dismiss_key_overlay(&resize_ctx);
                            }
                        }
                        // A switch deliberately shuts down the old socket to
                        // unblock the main frame loop's read (see
                        // perform_switch); if a real terminal resize races
                        // that exact window this send_control can fail on
                        // the about-to-die stream even though nothing is
                        // actually wrong going forward. `continue` (not
                        // `break`) so this thread keeps polling across a
                        // switch instead of leaving resizes dead for the
                        // rest of the attach -- the post-switch explicit
                        // Resize the main loop sends covers any update lost
                        // in that exact window.
                        if send_control(
                            &resize_writer,
                            &AttachControl::Resize {
                                rows: worker_rows,
                                cols,
                            },
                        )
                        .is_err()
                        {
                            last = size;
                            continue;
                        }
                    }
                    last = size;
                }
                thread::sleep(Duration::from_millis(200));
            }
        });
    }
    if status_enabled {
        let status_active = active.clone();
        let status_last_activity = last_activity.clone();
        let thread_status_ctx = status_ctx.clone();
        thread::spawn(move || {
            let mut last_draw = Instant::now();
            // Edge-triggered, not level-triggered: once the PTY has been
            // idle for STATUS_BAR_IDLE_GAP, redraw exactly once and then
            // stay quiet -- not on every poll tick for as long as it
            // remains idle. Tracking "have we already redrawn for the
            // current idle stretch" (reset the moment new PTY activity is
            // observed) is what makes this an actual debounce instead of a
            // redraw storm during any sufficiently long idle period.
            let mut last_seen_activity = status_last_activity
                .lock()
                .map(|t| *t)
                .unwrap_or_else(|_| Instant::now());
            let mut drawn_for_current_idle = false;
            // Whether the previous tick found the session agent-busy, so the
            // spinner's stop gets one final freeze-frame draw (see the
            // falling-edge note in the loop body).
            let mut was_animating = false;
            while status_active.load(Ordering::Relaxed) {
                thread::sleep(STATUS_BAR_POLL_INTERVAL);
                let activity = match status_last_activity.lock() {
                    Ok(t) => *t,
                    Err(_) => continue,
                };
                if activity != last_seen_activity {
                    last_seen_activity = activity;
                    drawn_for_current_idle = false;
                }
                // A resize the boundary gate deferred is delivered here even
                // when the workload has gone completely silent, which the
                // frame loop's flush cannot cover: it only runs when a chunk
                // arrives. This tick is also what lets `LAYOUT_DEFER_LIMIT`
                // expire on a workload that stopped mid-escape-sequence.
                // Cheap: one uncontended mutex peek that returns immediately
                // when nothing is deferred, which is the overwhelming case.
                if thread_status_ctx.overlay.is_active() {
                    // The overlay owns the terminal for the moment it is up.
                    // Nothing about the bar, the mouse or a deferred resize is
                    // worth painting into it: each keeps waiting, and the
                    // repaint `dismiss_key_overlay` writes delivers them.
                    continue;
                }
                // Cheap and idempotent: returns immediately unless the
                // workload's own mouse wishes changed since the last tick.
                sync_client_mouse(&thread_status_ctx);
                if thread_status_ctx.scroll.is_active() {
                    // The pager owns the terminal. Its position readout is
                    // the only thing that needs to keep moving while the
                    // workload produces output behind it; a deferred resize
                    // or bar redraw stays deferred until the user comes back
                    // to the live screen.
                    refresh_scroll_bar(&thread_status_ctx);
                    continue;
                }
                flush_pending_layout(&thread_status_ctx);
                // A full repaint requested by a resize (or by Ctrl-b r) may
                // be parked when the last workload chunk ended mid-sequence.
                // Unlike the frame loop, this thread still runs when the
                // workload goes silent, so retry it here instead of leaving
                // the old bar stranded above the new bottom row forever.
                if thread_status_ctx.pending_refresh.load(Ordering::Relaxed) {
                    let wrote = redraw_live_screen(&thread_status_ctx);
                    if wrote {
                        last_draw = Instant::now();
                    }
                    continue;
                }
                let idle_for = activity.elapsed();
                let overdue = last_draw.elapsed() >= STATUS_BAR_MAX_INTERVAL;
                // While the attached session is agent-busy, the bar's state
                // glyph is a spinner (`spinner_frame`) whose frame changes
                // every SPINNER_FRAME_MS -- so "the text changed" becomes a
                // redraw trigger of its own, independent of the PTY-activity
                // debounce above. That is the point of the whole feature: the
                // long silent stretch of a compute-bound tool call streams
                // nothing, and without this branch the only remaining trigger
                // would be the STATUS_BAR_MAX_INTERVAL forced tick, leaving
                // the spinner visibly stuttering once every 3s. The
                // dirty-check still guards the write itself: only the glyph
                // segment differs frame to frame, and a tick whose frame was
                // already flushed (a deferred write the frame loop performed)
                // renders byte-identical text and costs nothing.
                let animating = {
                    let record = thread_status_ctx
                        .record
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .clone();
                    spinner_frame(session_ui_state(&record, now_ms()).0, now_ms()).is_some()
                };
                // On the falling edge (work just stopped) draw once more even
                // though the spinner no longer animates, so the glyph freezes
                // back to its static form and the new state word lands within
                // one poll tick instead of waiting out the 3s overdue bound.
                // Unconditional on purpose: the last animation write was at
                // most one frame ago, so any elapsed-time gate here would
                // skip exactly the tick this exists for. The dirty-check
                // still makes it a no-op when nothing actually changed.
                let anim_due = animating || was_animating;
                was_animating = animating;
                if anim_due
                    || (idle_for >= STATUS_BAR_IDLE_GAP && !drawn_for_current_idle)
                    || overdue
                {
                    // `overdue` forces the write even if the text is
                    // unchanged -- see draw_status_bar's doc comment on why
                    // the margin-defense guarantee needs that. It does *not*
                    // force the write to happen here: if the relayed stream is
                    // mid-escape-sequence (which, for a continuously-streaming
                    // agent CLI, it is about half the time), `draw_status_bar`
                    // marks `ctx.pending` and the main frame loop performs the
                    // write at the next safe boundary instead. `wrote` is
                    // false in that case, so this timer correctly keeps
                    // considering the bar overdue until a real write lands.
                    //
                    // Only reset `last_draw` when a real write happened:
                    // resetting it on every tick regardless -- even ticks
                    // the dirty-check turned into a no-op -- would let the
                    // idle-gap branch's frequent no-op "redraws" keep
                    // `overdue` perpetually false, starving the very
                    // self-heal guarantee this timer exists to provide (see
                    // draw_status_bar's doc comment).
                    let wrote = draw_status_bar(&thread_status_ctx, overdue);
                    if wrote {
                        last_draw = Instant::now();
                    }
                    drawn_for_current_idle = true;
                }
            }
        });
    }

    // Whether the session ended while we were attached to it (worker sent
    // End/Exit) as opposed to the client leaving first -- drives the
    // honest goodbye line after terminal restoration. `worker_error` is the
    // `ServerEvent::Error` arm: neither a detach nor a clean workload exit.
    let mut session_ended = false;
    let mut worker_error = false;
    'session: loop {
        loop {
            let frame = match read_frame(&mut reader) {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e)
                    if e.downcast_ref::<io::Error>()
                        .map(|x| {
                            matches!(
                                x.kind(),
                                io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
                            )
                        })
                        .unwrap_or(false) =>
                {
                    break
                }
                Err(e) => return Err(e),
            };
            match frame.kind {
                FrameKind::Data => {
                    relay_to_terminal(
                        &workload_screen,
                        &stdout,
                        &scroll_mode,
                        &key_overlay,
                        &frame.payload,
                    )?;
                    if let Ok(mut t) = last_activity.lock() {
                        *t = Instant::now();
                    }
                    if !status_enabled {
                        continue;
                    }
                    // While a client modal is up nothing may paint over it:
                    // the deferred resize, the deferred bar and the deferred
                    // `Ctrl-b r` all keep waiting, and are delivered by the
                    // repaint `exit_scroll_mode`/`dismiss_key_overlay`
                    // performs (or by the first chunk after it).
                    //
                    // Type-through is the exception to "nothing may paint":
                    // the workload's bytes are being relayed to the host, so
                    // this IS a live view and the typing bar needs the same
                    // per-chunk maintenance the live bar gets -- a deferred
                    // bar write flushes at this chunk boundary, and the row
                    // the bar lives on is repaired if the workload's own
                    // erase sequences took it out (the `Layout` arm
                    // invalidates the dirty check for exactly that case).
                    if scroll_mode.is_active() || key_overlay.is_active() {
                        if scroll_mode.is_typing() && !key_overlay.is_active() {
                            flush_pending_layout(&status_ctx);
                            refresh_scroll_bar(&status_ctx);
                        }
                        continue;
                    }
                    // A redraw the status thread wanted while the stream was
                    // mid-sequence (or mid-frame) waits here rather than being
                    // written at an unsafe offset. This is the only place a
                    // continuously-streaming workload's bar gets refreshed at
                    // all, and it is by construction a chunk boundary that the
                    // model has just confirmed is also an escape boundary --
                    // see `draw_status_bar`'s boundary gate.
                    //
                    // The deferred *resize* goes first: a bar redraw is laid
                    // out against `TermGeom`, so reasserting the new scroll
                    // region before drawing keeps the two consistent within
                    // the same chunk instead of one chunk apart.
                    flush_pending_layout(&status_ctx);
                    if status_ctx.pending_refresh.load(Ordering::Relaxed) {
                        redraw_live_screen(&status_ctx);
                    } else if status_ctx.pending.load(Ordering::Relaxed) {
                        draw_status_bar(&status_ctx, true);
                    }
                }
                FrameKind::End => {
                    session_ended = !detached_by_client.load(Ordering::Relaxed);
                    break;
                }
                FrameKind::Json => {
                    let event: ServerEvent = serde_json::from_slice(&frame.payload)?;
                    match event {
                        ServerEvent::Exit { .. } => {
                            session_ended = true;
                            break;
                        }
                        ServerEvent::Error { message } => {
                            eprintln!("[aplexer: {message}]");
                            worker_error = true;
                            break;
                        }
                        // The workload reset margins or flipped alt-screen
                        // state (docs/terminal-state-design.md section 7):
                        // re-assert the status-bar reservation and redraw
                        // within one socket round-trip of the bytes that
                        // caused it, instead of waiting on the idle-gap
                        // timer. `draw_status_bar`'s own margin re-assert
                        // (see its doc comment) is the reservation half of
                        // this; the redraw is the other half. Only ever
                        // received when this attach opted in via
                        // `want_screen` (the worker gates it -- see
                        // `handle_attach`), so this arm is unreachable on
                        // the `--history-bytes` raw-tail path, but handling
                        // it unconditionally keeps this match exhaustive and
                        // correct if that ever changes.
                        ServerEvent::Layout { .. } => {
                            if status_enabled {
                                if status_ctx.scroll.is_typing() {
                                    // Type-through streams workload bytes to the
                                    // host, and Erase in Display ignores scroll
                                    // margins: the workload's own `CSI ... J` --
                                    // which Ink-style TUIs emit on nearly every
                                    // frame -- erases past the scroll region and
                                    // takes the reserved row, the typing bar
                                    // included. Nothing else repairs it: the
                                    // dirty check sees unchanged text and skips,
                                    // and the erased row would stay blank until
                                    // the offset or the count next changes.
                                    // Invalidate the check so this refresh
                                    // actually rewrites the row.
                                    *status_ctx
                                        .last_drawn
                                        .lock()
                                        .unwrap_or_else(PoisonError::into_inner) = None;
                                    refresh_scroll_bar(&status_ctx);
                                } else if !status_ctx.scroll.is_active() {
                                    // Live view: re-assert the reservation and
                                    // redraw within one socket round-trip of the
                                    // bytes that caused it, instead of waiting
                                    // on the idle-gap timer. `draw_status_bar`'s
                                    // own margin re-assert (see its doc comment)
                                    // is the reservation half of this; the
                                    // redraw is the other half.
                                    draw_status_bar(&status_ctx, true);
                                }
                                // Pager without type-through: nothing is being
                                // written to the host, so no erase can have
                                // reached the bar row and the pager's own tick
                                // already maintains it.
                            }
                        }
                    }
                }
            }
        }
        // The frame loop broke: either the session ended/we detached, or
        // the input thread killed the old stream to hand us a switch.
        let outcome = take_pending_switch(&pending_switch, &switch_in_progress);
        let Some(outcome) = outcome else { break };

        let switched_to = outcome.record.clone();
        *shared_record.lock().unwrap_or_else(PoisonError::into_inner) = outcome.record;
        reader = outcome.reader; // old stream dropped (closed) here

        // A and B have independent terminal state. Neutralize every buffer
        // and input mode A's snapshot/live stream may have enabled before
        // replaying B's snapshot: a default-mode B deliberately emits no
        // mouse-off or primary-screen transition of its own. B's snapshot
        // follows immediately and re-enables exactly the modes it owns.
        // Raw termios belongs to this client rather than either session, so
        // it remains in force across the switch.
        //
        // Geometry is read before `stdout` is taken, keeping the
        // `stdout` -> `term` -> `screen` order `write_locked` documents.
        let geom = term.lock().map(|g| *g).unwrap_or(TermGeom {
            rows: 0,
            cols: 0,
            reserved: false,
        });
        // The new session's screen is its own: drop the previous one's model
        // (its margins, its half-parsed sequences, its cursor) and learn the
        // new one's from its snapshot payload, under the same lock as the
        // write so the status thread can never redraw against a model that
        // disagrees with what the terminal has been sent.
        let (screen_rows, screen_cols) = if geom.rows > 0 {
            (reserved_rows(geom.rows), geom.cols)
        } else {
            (
                aplexer::screen::DEFAULT_TERMINAL_ROWS,
                aplexer::screen::DEFAULT_TERMINAL_COLS,
            )
        };
        // The reset and the seed are done here rather than through
        // `feed_and_write`'s `reset_to`, because the retained history has to
        // be primed *between* them: reset (drop A's model and its history),
        // seed (give B's model B's past), then paint B's screen on top. Done
        // the other way round the seed's rows would land above nothing, or
        // below B's current screen.
        {
            let mut screen = workload_screen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            screen.reset(screen_rows, screen_cols);
        }
        if scrollback_lines > 0 {
            seed_client_scrollback(&workload_screen, &switched_to);
        }
        let _ = feed_and_write(
            &stdout,
            &workload_screen,
            TERMINAL_RESET_SEQUENCE,
            &outcome.history,
            None,
        );
        // TERMINAL_RESET_SEQUENCE turned every mouse mode off, so whoever
        // owned the mouse before the switch owns nothing now.
        *status_ctx
            .mouse_owned
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        sync_client_mouse(&status_ctx);
        if let Ok(mut t) = last_activity.lock() {
            *t = Instant::now();
        }

        // B's PTY may still be sized for its previous client (or the
        // 24x80 default). The resize thread won't resend an unchanged
        // terminal size (its `last` cache), so push the current geometry
        // explicitly.
        if geom.rows > 0 {
            let _ = send_control(
                &writer,
                &AttachControl::Resize {
                    rows: reserved_rows(geom.rows),
                    cols: geom.cols,
                },
            );
        }
        if status_enabled {
            draw_status_bar(&status_ctx, true); // clear wiped the reserved row; redraw now
        }
        continue 'session;
    }
    active.store(false, Ordering::Relaxed);
    if let Ok(stream) = writer.lock() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    // Restore display state and cooked termios before restoring the prior
    // signal disposition and re-raising. A default TERM/HUP/QUIT action can
    // terminate the process immediately, so RAII alone cannot run after it.
    drop(_ui_guard);
    drop(_raw);
    if let Some(signal) = signal_bridge.take().and_then(AttachSignalBridge::finish) {
        unsafe {
            libc::raise(signal);
        }
    }
    if display_tty {
        // After restoration, so the message lands on a clean cooked
        // terminal: what happened to the session, not just that the client
        // came back. "Detached" means the client left and the session is
        // still running; "Session ended" means the workload is gone; the
        // worker-error and connection-loss lines say which layer failed
        // instead of blaming the user's own detach. A clean exit (including
        // Ctrl-D) removes the record; only Failed/OOM leftovers remain to
        // inspect.
        let current_record = shared_record
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let short_id = current_record.id.to_string();
        let inspect_id = paths
            .record(current_record.id)
            .exists()
            .then(|| &short_id[..8]);
        let stop = classify_attach_stop(
            session_ended,
            detached_by_client.load(Ordering::Relaxed),
            worker_error,
        );
        eprintln!(
            "{}",
            attach_goodbye_line(stop, &current_record.selector(), inspect_id)
        );
    }
    Ok(())
}
