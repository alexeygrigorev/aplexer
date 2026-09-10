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
    let reader = handshake.reader;
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
        live: Arc::new(Mutex::new(LiveStatus::default())),
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
        last_session: last_session.clone(),
        pending_switch: pending_switch.clone(),
        switch_in_progress: switch_in_progress.clone(),
        status: status_ctx.clone(),
    });
    if display_tty {
        spawn_resize_thread(ResizeThreadConfig {
            writer: writer.clone(),
            active: active.clone(),
            status: status_ctx.clone(),
            initial_geometry,
            status_enabled,
        });
    }
    if status_enabled {
        spawn_status_thread(StatusThreadConfig {
            active: active.clone(),
            last_activity: last_activity.clone(),
            status: status_ctx.clone(),
        });
    }
    // Whatever the frame loop returns -- including an error -- the
    // shutdown below runs in this order, so `?` waits until it has. An early
    // return dropped the signal bridge *before* the terminal guards (reverse
    // declaration order), restoring the prior TERM/HUP disposition while the
    // host was still on the alternate screen in raw mode, and never re-raised
    // a caught signal at all.
    let session_outcome = run_session_loop(
        reader,
        SessionLoopConfig {
            status_enabled,
            writer: writer.clone(),
            status: status_ctx.clone(),
            last_activity: last_activity.clone(),
            detached_by_client: detached_by_client.clone(),
            pending_switch: pending_switch.clone(),
            switch_in_progress: switch_in_progress.clone(),
            scrollback_lines,
        },
    );
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
    let session_outcome = session_outcome?;
    let session_ended = session_outcome.session_ended;
    let worker_error = session_outcome.worker_error;
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
