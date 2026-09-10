use super::*;

/// Runtime state used by the frame loop after attach setup is complete. The
/// loop owns the active reader and session transitions; the surrounding
/// attach function owns terminal guards and thread lifetime. Everything
/// about the host terminal -- stdout, the model, the modals, the record --
/// is reached through `status`, the one context every thread shares.
pub(crate) struct SessionLoopConfig {
    pub(crate) status_enabled: bool,
    pub(crate) writer: Arc<Mutex<UnixStream>>,
    pub(crate) status: StatusBarCtx,
    pub(crate) last_activity: Arc<Mutex<Instant>>,
    pub(crate) detached_by_client: Arc<AtomicBool>,
    pub(crate) pending_switch: Arc<Mutex<Option<SwitchOutcome>>>,
    pub(crate) switch_in_progress: Arc<AtomicBool>,
    pub(crate) scrollback_lines: usize,
}

#[derive(Default)]
pub(crate) struct SessionLoopOutcome {
    pub(crate) session_ended: bool,
    pub(crate) worker_error: bool,
}

pub(crate) fn run_session_loop(
    mut reader: UnixStream,
    config: SessionLoopConfig,
) -> Result<SessionLoopOutcome> {
    let mut outcome = SessionLoopOutcome::default();
    loop {
        let frame_outcome = read_session_frames(&mut reader, &config)?;
        outcome.session_ended |= frame_outcome.session_ended;
        outcome.worker_error |= frame_outcome.worker_error;

        let Some(switch) = take_pending_switch(&config.pending_switch, &config.switch_in_progress)
        else {
            break;
        };
        apply_session_switch(switch, &mut reader, &config);
    }
    Ok(outcome)
}

#[derive(Default)]
struct FrameLoopOutcome {
    session_ended: bool,
    worker_error: bool,
}

fn read_session_frames(
    reader: &mut UnixStream,
    config: &SessionLoopConfig,
) -> Result<FrameLoopOutcome> {
    loop {
        let frame = match read_frame(reader) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(FrameLoopOutcome::default()),
            Err(error)
                if error
                    .downcast_ref::<io::Error>()
                    .map(|io_error| {
                        matches!(
                            io_error.kind(),
                            io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
                        )
                    })
                    .unwrap_or(false) =>
            {
                return Ok(FrameLoopOutcome::default());
            }
            Err(error) => return Err(error),
        };

        match frame.kind {
            FrameKind::Data => handle_data_frame(config, &frame.payload)?,
            FrameKind::End => {
                return Ok(FrameLoopOutcome {
                    session_ended: !config.detached_by_client.load(Ordering::Relaxed),
                    worker_error: false,
                });
            }
            FrameKind::Json => match handle_json_frame(config, &frame.payload)? {
                JsonFrameAction::Continue => {}
                JsonFrameAction::SessionEnded => {
                    return Ok(FrameLoopOutcome {
                        session_ended: true,
                        worker_error: false,
                    });
                }
                JsonFrameAction::WorkerError => {
                    return Ok(FrameLoopOutcome {
                        session_ended: false,
                        worker_error: true,
                    });
                }
            },
        }
    }
}

fn handle_data_frame(config: &SessionLoopConfig, payload: &[u8]) -> Result<()> {
    relay_to_terminal(&config.status, payload)?;
    if let Ok(mut time) = config.last_activity.lock() {
        *time = Instant::now();
    }
    if !config.status_enabled {
        return Ok(());
    }

    // While a client modal is up nothing may paint over it. Type-through is
    // the exception: workload bytes are being relayed, so the typing bar
    // still gets the same per-chunk maintenance as the live bar.
    let status = &config.status;
    if status.scroll.is_active() || status.overlay.is_active() {
        if status.scroll.is_typing() && !status.overlay.is_active() {
            maintain_pager_bar(status);
        }
        return Ok(());
    }

    // The deferred resize goes first: a bar redraw is laid out against
    // `TermGeom`, so reasserting the new scroll region before drawing keeps
    // the two consistent within the same chunk.
    flush_pending_layout(&config.status);
    if config.status.pending_refresh.load(Ordering::Relaxed) {
        redraw_live_screen(&config.status);
    } else if config.status.pending.load(Ordering::Relaxed) {
        draw_status_bar(&config.status, true);
    }
    Ok(())
}

enum JsonFrameAction {
    Continue,
    SessionEnded,
    WorkerError,
}

fn handle_json_frame(config: &SessionLoopConfig, payload: &[u8]) -> Result<JsonFrameAction> {
    let event: ServerEvent = serde_json::from_slice(payload)?;
    Ok(match event {
        ServerEvent::Exit { .. } => JsonFrameAction::SessionEnded,
        ServerEvent::Error { message } => {
            eprintln!("[aplexer: {message}]");
            JsonFrameAction::WorkerError
        }
        ServerEvent::Layout { .. } => {
            if config.status_enabled {
                handle_layout_event(&config.status);
            }
            JsonFrameAction::Continue
        }
    })
}

fn handle_layout_event(status: &StatusBarCtx) {
    if status.scroll.is_typing() {
        // Type-through streams workload bytes to the host, and Erase in
        // Display ignores scroll margins. Invalidate the dirty check so the
        // typing bar rewrites its row after the workload erases it.
        *status
            .last_drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        refresh_scroll_bar(status);
    } else if !status.scroll.is_active() {
        // Live view: reassert the reservation and redraw within one socket
        // round-trip of the bytes that caused the layout event.
        draw_status_bar(status, true);
    }
    // Pager without type-through writes nothing to the host, so its own tick
    // maintains the view and no layout repair is needed here.
}

fn apply_session_switch(
    switch: SwitchOutcome,
    reader: &mut UnixStream,
    config: &SessionLoopConfig,
) {
    let status = &config.status;
    let switched_to = switch.record.clone();
    *status.record.lock().unwrap_or_else(PoisonError::into_inner) = switch.record;
    *reader = switch.reader;

    // A and B have independent terminal state. Reset every buffer and input
    // mode A may have enabled before replaying B's snapshot. Raw termios
    // belongs to this client, so it remains in force across the switch.
    let geometry = status.term.lock().map(|geom| *geom).unwrap_or(TermGeom {
        rows: 0,
        cols: 0,
        reserved: false,
    });
    let (screen_rows, screen_cols) = switch_screen_geometry(geometry);
    status
        .screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .reset(screen_rows, screen_cols);
    if config.scrollback_lines > 0 {
        seed_client_scrollback(&status.screen, &switched_to);
    }
    let _ = feed_and_write(
        &status.stdout,
        &status.screen,
        SWITCH_RESET_SEQUENCE,
        &switch.history,
        None,
    );

    *status
        .mouse_owned
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    sync_client_mouse(status);
    if let Ok(mut time) = config.last_activity.lock() {
        *time = Instant::now();
    }
    if geometry.rows > 0 {
        let _ = send_control(
            &config.writer,
            &AttachControl::Resize {
                rows: reserved_rows(geometry.rows),
                cols: geometry.cols,
            },
        );
    }
    if config.status_enabled {
        // The cache belongs to the outgoing session. This is the one draw
        // outside the status thread that fetches: the relay is idle here
        // (B's first frame has not been read yet), and the alternative is a
        // bar that fills in its siblings and memory a tick later.
        refresh_live_status(status);
        draw_status_bar(status, true);
    }
}

fn switch_screen_geometry(geometry: TermGeom) -> (u16, u16) {
    if geometry.rows > 0 {
        (reserved_rows(geometry.rows), geometry.cols)
    } else {
        (
            aplexer::screen::DEFAULT_TERMINAL_ROWS,
            aplexer::screen::DEFAULT_TERMINAL_COLS,
        )
    }
}
