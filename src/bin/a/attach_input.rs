use super::*;

/// State shared by the stdin thread while an attach is active. Keeping the
/// thread's inputs together makes the attach orchestrator responsible only
/// for wiring the runtime, while this module owns the input state machine.
/// The paths, geometry and record a switch needs are reached through
/// `status`, the one context every thread shares.
pub(crate) struct InputThreadConfig {
    pub(crate) input_tty: bool,
    pub(crate) status_enabled: bool,
    pub(crate) want_screen: bool,
    pub(crate) replay_bytes: Option<usize>,
    pub(crate) writer: Arc<Mutex<UnixStream>>,
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) detached: Arc<AtomicBool>,
    pub(crate) last_session: Arc<Mutex<Option<Uuid>>>,
    pub(crate) pending_switch: Arc<Mutex<Option<SwitchOutcome>>>,
    pub(crate) switch_in_progress: Arc<AtomicBool>,
    pub(crate) status: StatusBarCtx,
}

pub(crate) fn spawn_input_thread(config: InputThreadConfig) {
    thread::spawn(move || run_input_loop(config));
}

fn run_input_loop(config: InputThreadConfig) {
    let mut input = io::stdin();
    let mut buffer = [0u8; 8192];
    // Ctrl-b (0x02) prefix state machine -- Ctrl-b d detaches,
    // Ctrl-b ? flashes the key reference, Ctrl-b r redraws the live
    // screen, Ctrl-b c creates another session here, Ctrl-b n/p/N/P/l/1-9
    // switch sessions, anything else pending is not a real prefix (both
    // bytes forward to the workload). See `InputScanner` for the byte-level
    // rules and why this needs to survive across separate read() calls, not
    // just within one buffer.
    //
    // Design choice: real tmux turns Ctrl-b into a standing "prefix" that
    // consumes the next keystroke as a command (or no-ops/bells if
    // unrecognized), never forwarding Ctrl-b itself to the pane. aplexer has
    // no such command-prefix system and isn't growing one just for this, so
    // the simplest reasonable behavior is used instead: a *bound* Ctrl-b
    // sequence (d/?/r/c/n/p/N/P/l/1-9) is consumed; anything else is not a
    // prefix at all -- both bytes are forwarded through as ordinary input,
    // so a program that wants a literal Ctrl-b (some editors and REPLs use
    // it) isn't broken by this feature.
    let mut scanner = InputScanner::default();
    // Sits between the chord scanner and the socket: while the pager is up it
    // consumes every byte (no keystroke reaches the workload -- the whole
    // point of the mode), and while the client holds the mouse it consumes
    // mouse reports the workload never asked for and turns a wheel roll up
    // into the pager.
    let mut scroll_input = ScrollInput::default();
    // Whether `KEY_OVERLAY_DELAY` has already fired for the `Ctrl-b` currently
    // being held. Not "is the box up" -- that lives in `KeyOverlay::active`,
    // which the resize thread can also clear -- but "this prefix has had its
    // one chance to raise it", which is what keeps a terminal too small for
    // the box from flashing on a loop.
    let mut overlay_armed = false;

    'outer: while config.active.load(Ordering::Relaxed) {
        // The only two places this loop does not simply block on stdin, and
        // they are mutually exclusive by construction (see
        // `KEY_OVERLAY_DELAY`, which spells out why that matters): a
        // half-typed arrow chord has `CHORD_ESCAPE_TIMEOUT` to complete, and
        // a lone `Ctrl-b` has `KEY_OVERLAY_DELAY` before the keymap is drawn.
        // The short-circuit keeps this free with nothing pending.
        let chord_expired =
            scanner.awaiting_escape() && !readable(libc::STDIN_FILENO, CHORD_ESCAPE_TIMEOUT);
        if !chord_expired
            && !overlay_armed
            && scanner.awaiting_key()
            && !config.status.scroll.is_active()
            && !readable(libc::STDIN_FILENO, KEY_OVERLAY_DELAY)
        {
            // Hesitation on the prefix rather than a chord typed from muscle
            // memory. Draw the keymap and go straight back to waiting for the
            // key it explains -- the prefix is still pending, so that key
            // runs its binding exactly as it would have.
            overlay_armed = true;
            show_key_overlay(&config.status);
            continue;
        }

        let actions = match read_input_actions(
            &mut input,
            &mut buffer,
            &mut scanner,
            &mut overlay_armed,
            chord_expired,
            &config,
        ) {
            InputPoll::Actions(actions) => actions,
            InputPoll::Continue => continue,
            InputPoll::Stop => break,
        };

        for action in actions {
            if !handle_input_action(action, &mut scroll_input, &config) {
                break 'outer;
            }
        }
    }

    // Whatever ended this thread -- stdin EOF, a read error, a dead socket,
    // `Ctrl-b d` -- must not leave the relay suspended behind a box nobody can
    // dismiss any more: this thread is the only one that takes keys. A no-op
    // in the ordinary case, because a key arriving is what dismisses the
    // overlay and `Ctrl-b d` is a key.
    dismiss_key_overlay(&config.status);
}

enum InputPoll {
    Actions(Vec<InputAction>),
    Continue,
    Stop,
}

fn read_input_actions(
    input: &mut io::Stdin,
    buffer: &mut [u8],
    scanner: &mut InputScanner,
    overlay_armed: &mut bool,
    chord_expired: bool,
    config: &InputThreadConfig,
) -> InputPoll {
    if chord_expired {
        let flushed = scanner.flush_pending();
        *overlay_armed = false;
        return if dismiss_key_overlay(&config.status) {
            // `Esc` with the box up means "never mind", and taking the box
            // down is the whole of it: the withheld `Ctrl-b ESC` is consumed
            // rather than typed into the workload.
            InputPoll::Actions(Vec::new())
        } else {
            InputPoll::Actions(flushed)
        };
    }

    let n = match input.read(buffer) {
        Ok(0) => {
            mark_input_detached(config);
            return InputPoll::Stop;
        }
        Ok(n) => n,
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return InputPoll::Continue,
        Err(_) => {
            mark_input_detached(config);
            return InputPoll::Stop;
        }
    };

    if !config.input_tty || !config.status_enabled {
        if send_data(&config.writer, &buffer[..n]).is_err() {
            detach_attached_client(&config.writer, &config.active);
            return InputPoll::Stop;
        }
        return InputPoll::Continue;
    }

    let actions = scanner.scan(&buffer[..n]);
    if scanner.settled() {
        *overlay_armed = false;
        // A key arrived, so the overlay is over before its action runs:
        // switches, flashes, and pager frames draw onto the restored screen.
        dismiss_key_overlay(&config.status);
    }
    InputPoll::Actions(actions)
}

fn handle_input_action(
    action: InputAction,
    scroll_input: &mut ScrollInput,
    config: &InputThreadConfig,
) -> bool {
    match action {
        InputAction::Forward(bytes) => {
            // Ordering matters: a Forward before a Switch goes to the old
            // session; a Forward after it goes to the new one because
            // `perform_switch` swaps the stream inside the writer mutex.
            let bytes = scroll_input.route(&config.status, &bytes);
            if bytes.is_empty() {
                true
            } else if send_data(&config.writer, &bytes).is_err() {
                detach_attached_client(&config.writer, &config.active);
                false
            } else {
                true
            }
        }
        InputAction::Detach => {
            // Leave the pager first: detach restores the host from the live
            // model, and the reset sequence it writes assumes the relay owns
            // the screen again.
            exit_scroll_mode(&config.status);
            mark_input_detached(config);
            false
        }
        InputAction::Help => {
            flash_status(&config.status, attach_key_help());
            true
        }
        InputAction::Redraw => {
            // `Ctrl-b r` redraws the view the user is looking at. While the
            // pager is up that is the pager, not the live screen.
            if config.status.scroll.owns_host() {
                paint_scroll_view(&config.status);
            } else {
                redraw_live_screen(&config.status);
            }
            true
        }
        InputAction::Scroll => {
            enter_scroll_mode(&config.status, ScrollCommand::Stay);
            true
        }
        InputAction::Switch(target) => {
            // A switch replaces the model wholesale; the pager is looking at
            // the outgoing session's history, so it has to close before the
            // swap. Creation and connection happen before the current stream
            // is touched, so an error leaves the user where they were.
            exit_scroll_mode(&config.status);
            if let Err(error) = perform_switch(config, target) {
                flash_status(&config.status, format!("{error:#}"));
            }
            true
        }
    }
}

fn mark_input_detached(config: &InputThreadConfig) {
    config.detached.store(true, Ordering::Relaxed);
    detach_attached_client(&config.writer, &config.active);
}
