use super::*;

/// Shared state for the terminal-resize poller. The poller owns host geometry
/// changes; the attach loop only decides whether a poller should exist.
pub(crate) struct ResizeThreadConfig {
    pub(crate) writer: Arc<Mutex<UnixStream>>,
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) status: StatusBarCtx,
    pub(crate) initial_geometry: Option<(u16, u16)>,
    pub(crate) status_enabled: bool,
}

pub(crate) fn spawn_resize_thread(config: ResizeThreadConfig) {
    thread::spawn(move || run_resize_loop(config));
}

fn run_resize_loop(config: ResizeThreadConfig) {
    // Seeded with the geometry `attach()` already applied and already sent in
    // the Attach request, so this thread reacts to changes only. Starting from
    // `None` made its first poll look like a resize and re-run the layout
    // unconditionally, dropping the workload scroll region just restored by
    // the attach snapshot.
    let mut last = config.initial_geometry;
    while config.active.load(Ordering::Relaxed) {
        let size = terminal_size(libc::STDOUT_FILENO);
        if size != last {
            if let Some((rows, cols)) = size {
                apply_resize(&config, rows, cols);
            }
            last = size;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

pub(crate) fn apply_resize(config: &ResizeThreadConfig, rows: u16, cols: u16) {
    // Keep the client's tracker in step with the worker-side model across the
    // same resize: both re-clamp the region to the new row count rather than
    // dropping it.
    let worker_rows = if config.status_enabled {
        reserved_rows(rows)
    } else {
        rows
    };
    config
        .status
        .screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .set_size(worker_rows, cols);

    if config.status_enabled {
        // The layout write may defer until the relayed stream reaches an
        // escape boundary. The worker resize below remains unconditional, so
        // SIGWINCH is delivered on time even when the host reservation waits.
        apply_terminal_layout(&config.status, rows, cols);
        repaint_resize_modal(&config.status);
    }

    // A switch shuts down the old socket to unblock the frame loop. A resize
    // racing that window may fail here; the poller deliberately keeps running
    // so later resizes remain live across the switch.
    let _ = send_control(
        &config.writer,
        &AttachControl::Resize {
            rows: worker_rows,
            cols,
        },
    );
}

fn repaint_resize_modal(status: &StatusBarCtx) {
    // The pager renders at the reserved geometry, so a resize has to redraw
    // it while the relay is suspended. Not in type-through: the relay is
    // streaming to the host, and the pager's frame would paint old history
    // under the live bytes -- `apply_terminal_layout` repaints the live
    // screen there, as it does for the ordinary live view.
    if status.scroll.owns_host() {
        paint_scroll_view(status);
    }
    // A new geometry may be too small for the key overlay. If it cannot be
    // painted, take it down instead of leaving a stale box over the relay.
    if status.overlay.is_active() && !paint_key_overlay(status) {
        dismiss_key_overlay(status);
    }
}

/// Shared state for the status-bar poller. It is intentionally separate from
/// the resize poller: status scheduling depends on PTY activity, while resize
/// handling must continue even when the workload is silent.
pub(crate) struct StatusThreadConfig {
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) last_activity: Arc<Mutex<Instant>>,
    pub(crate) status: StatusBarCtx,
}

pub(crate) fn spawn_status_thread(config: StatusThreadConfig) {
    thread::spawn(move || run_status_loop(config));
}

fn run_status_loop(config: StatusThreadConfig) {
    let mut last_draw = Instant::now();
    // Edge-triggered, not level-triggered: redraw once after an idle stretch,
    // then stay quiet until new PTY activity resets the edge.
    let mut last_seen_activity = config
        .last_activity
        .lock()
        .map(|time| *time)
        .unwrap_or_else(|_| Instant::now());
    let mut drawn_for_current_idle = false;
    // The falling edge gets one final freeze-frame draw when a spinner stops.
    let mut was_animating = false;

    while config.active.load(Ordering::Relaxed) {
        thread::sleep(STATUS_BAR_POLL_INTERVAL);
        let activity = match config.last_activity.lock() {
            Ok(time) => *time,
            Err(_) => continue,
        };
        if activity != last_seen_activity {
            last_seen_activity = activity;
            drawn_for_current_idle = false;
        }
        if config.status.overlay.is_active() {
            // The overlay owns the terminal until it is dismissed.
            continue;
        }

        sync_client_mouse(&config.status);
        if config.status.scroll.is_active() {
            // The pager owns the bar row; only its position readout moves
            // (and, in type-through, a parked resize is delivered).
            maintain_pager_bar(&config.status);
            continue;
        }

        flush_pending_layout(&config.status);
        // A full repaint can be parked at an unsafe boundary. This thread is
        // still alive while the workload is silent, so retry it here.
        if config.status.pending_refresh.load(Ordering::Relaxed) {
            if redraw_live_screen(&config.status) {
                last_draw = Instant::now();
            }
            continue;
        }

        let idle_for = activity.elapsed();
        let overdue = last_draw.elapsed() >= STATUS_BAR_MAX_INTERVAL;
        let animating = status_is_animating(&config.status);
        let anim_due = animating || was_animating;
        was_animating = animating;
        if anim_due || (idle_for >= STATUS_BAR_IDLE_GAP && !drawn_for_current_idle) || overdue {
            // The one place the worker is asked: before a draw this thread
            // decided on, and only once the cache has aged out. An animating
            // bar draws every tick and fetches once a second.
            if live_status_is_stale(&config.status) {
                refresh_live_status(&config.status);
            }
            // A deferred write does not reset the deadline; the next safe
            // boundary must still get a real draw.
            if draw_status_bar(&config.status, overdue) {
                last_draw = Instant::now();
            }
            drawn_for_current_idle = true;
        }
    }
}

/// Whether the bar's state glyph is a spinner right now -- judged from the
/// same overlaid record `status_bar_text` renders, so a `working` push the
/// worker received after attach animates rather than freezing until the
/// next unrelated redraw.
fn status_is_animating(status: &StatusBarCtx) -> bool {
    let record = status
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let live = cached_live_status(status, record.id);
    let state = overlay_reported_state(&record, live.raw.as_ref());
    spinner_frame(session_ui_state(&state, now_ms()).0, now_ms()).is_some()
}
