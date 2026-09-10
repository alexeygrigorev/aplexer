use super::*;

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

/// Atomic switch-or-stay, run on the input thread
/// (docs/fast-session-switching-design.md section 3.3). The critical
/// ordering property: the new connection is fully established *before* the
/// old one is touched, so any failure (resolution, `check_attachable`, or
/// `establish` itself) leaves the attachment to the current session
/// completely undisturbed.
pub(crate) fn perform_switch(config: &InputThreadConfig, target: SwitchTarget) -> Result<()> {
    let paths = &config.status.paths;
    let current = config
        .status
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let last = *config
        .last_session
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    // Current terminal geometry (docs/fast-session-switching-design.md
    // section 5.2 / docs/terminal-state-design.md section 10.2): passed
    // into the Attach so the new session's snapshot renders at the right
    // size immediately, rather than relying solely on the post-switch
    // explicit Resize send below. Read before resolution because
    // `SwitchTarget::New` also hands it to the worker it starts.
    let geometry = config
        .status
        .term
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
    config.switch_in_progress.store(true, Ordering::Relaxed);
    let result = (|| -> Result<()> {
        let handshake = establish(&next, config.replay_bytes, config.want_screen, geometry)?;
        let reader = handshake.reader;
        let history = handshake.initial;
        // Cloned before anything is mutated.
        let writer_clone = reader.try_clone()?;
        // Repoint every forwarding thread (input, resize) at B, then retire
        // A's stream. From this instant keystrokes land in B.
        let old = {
            let mut w = config.writer.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *w, writer_clone)
        };
        *config
            .pending_switch
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(SwitchOutcome {
            record: next.clone(),
            reader,
            history,
        });
        *config
            .last_session
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(current.id);
        // Polite detach from A, then shutdown so the main loop's blocked
        // read_frame on A's socket returns immediately. shutdown() is
        // socket-wide, so it also unblocks the reader fd cloned from this
        // stream -- the same mechanism the existing detach path relies on.
        let mut old = old;
        let _ = write_json(&mut old, &AttachControl::Detach);
        let _ = old.shutdown(std::net::Shutdown::Both);
        Ok(())
    })();
    config.switch_in_progress.store(false, Ordering::Relaxed);
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
/// aplexer-shaped answer is already shipped: detach (`Ctrl-b d`), then let
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
         its pane -- detach first (Ctrl-b d), then switch with Ctrl-b arrows; \
         or pass --force to attach nested anyway",
        inner_record.workspace.display(),
        inner_record.tag
    )
}
