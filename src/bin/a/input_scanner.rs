use super::*;

/// What one `InputScanner::scan` call decided to do with a chunk of raw
/// stdin bytes; several may result from a single `read()` (e.g.
/// `"a\x02n"` -> `Forward([b'a'])`, `Switch(Next)`).
pub(crate) enum InputAction {
    /// Ordinary input for the currently attached session.
    Forward(Vec<u8>),
    /// `Ctrl-b d`.
    Detach,
    /// `Ctrl-b n/p/N/P/l/1-9`, and `Ctrl-b c` (which creates the session it
    /// then switches to -- see `SwitchTarget::New`).
    Switch(SwitchTarget),
    /// `Ctrl-b ?`: a purely local help flash on the status bar. Consumed
    /// like every other chord -- no byte reaches the workload, so asking
    /// for help can never type `?` into a prompt.
    Help,
    /// `Ctrl-b r`: repaint the host from the client's live screen model.
    /// Local, like Help -- the garbled cells are on this terminal, not in
    /// the session.
    Redraw,
    /// `Ctrl-b [`: open the scrollback pager (tmux's copy-mode chord). Local
    /// and, crucially, *consumed*: from here until the user leaves the mode,
    /// no keystroke reaches the workload.
    Scroll,
}

/// Byte-scanning state for the `Ctrl-b` prefix state machine, split out of
/// the input thread body so the split-across-`read()` cases are
/// unit-testable independent of any real socket/thread (see the `#[cfg(test)]`
/// module below). `pending_ctrl_b` has to survive across `scan()` calls, not
/// just within one buffer: `Ctrl-b` can legitimately arrive as the very
/// last byte of one `read()` and the following key as the first byte of the
/// next.
/// Whether `fd` has input waiting, waiting up to `timeout` for it. Used only
/// to bound the scanner's wait for the rest of an arrow chord, so an error
/// (or a signal) answers "yes": the caller falls through to its ordinary
/// blocking `read`, which is where read errors are already handled.
pub(crate) fn readable(fd: libc::c_int, timeout: Duration) -> bool {
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().clamp(0, i32::MAX as u128) as i32;
    let ready = unsafe { libc::poll(&mut poll_fd, 1, millis) };
    ready != 0
}

/// How long a `Ctrl-b ESC` may wait for the rest of an arrow-key sequence
/// before the scanner concludes there is no arrow coming and forwards the
/// withheld bytes to the workload.
///
/// The arrow chords (`Ctrl-b Left/Right/Up/Down`) are multi-byte -- `ESC [ C`
/// in normal cursor mode, `ESC O C` in application cursor mode, which plenty
/// of TUIs enable -- and a PTY read can split them anywhere, so the scanner
/// has to be able to hold a half-typed one across `read()` calls. But a bare
/// `Ctrl-b ESC` (a user reaching for the workload's own Escape, having
/// touched the prefix key by accident) is indistinguishable from the first
/// byte of an arrow until either the rest arrives or enough time passes, and
/// holding it indefinitely would leave an editor sitting in insert mode with
/// no idea why. So the wait is bounded: the input thread polls for this long
/// while the scanner holds a partial chord and, on silence, flushes it
/// through (`InputScanner::flush_pending`).
///
/// 100ms is far longer than the gap a terminal can put between the bytes of
/// one escape sequence (they are written in a single `write`; a split is a
/// buffer boundary, not a pause) and short enough to read as instant for the
/// Escape case. It is only ever paid after a literal `Ctrl-b ESC`, never on
/// ordinary input.
pub(crate) const CHORD_ESCAPE_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Default)]
pub(crate) struct InputScanner {
    pub(crate) pending_ctrl_b: bool,
    /// Bytes withheld *after* a `Ctrl-b` because they could still complete an
    /// arrow chord: `ESC`, `ESC [`, or `ESC O`, and nothing else. Empty at
    /// every other moment, which is what `awaiting_escape` reports. The
    /// withheld `Ctrl-b` itself is implied (it is not stored here) and is
    /// re-emitted ahead of these bytes whenever the sequence turns out not to
    /// be a chord.
    pub(crate) pending_escape: Vec<u8>,
}

impl InputScanner {
    /// True while a partial arrow chord is held. The input thread uses this
    /// to bound its wait for the rest (see `CHORD_ESCAPE_TIMEOUT`).
    pub(crate) fn awaiting_escape(&self) -> bool {
        !self.pending_escape.is_empty()
    }

    /// True while a *lone* `Ctrl-b` is held with nothing after it yet -- the
    /// hesitation the key overlay is armed on (`KEY_OVERLAY_DELAY`).
    ///
    /// Deliberately spelled as "prefix pending **and** nothing withheld after
    /// it", even though `scan` already clears `pending_ctrl_b` before it ever
    /// fills `pending_escape`: this is the predicate that makes the overlay's
    /// deadline and `CHORD_ESCAPE_TIMEOUT` mutually exclusive, so it states
    /// that exclusion rather than relying on a reader knowing the other
    /// invariant.
    pub(crate) fn awaiting_key(&self) -> bool {
        self.pending_ctrl_b && self.pending_escape.is_empty()
    }

    /// True when nothing at all is withheld: no prefix, no partial chord.
    /// The input thread reads this as "whatever the user was in the middle of
    /// is resolved", which is when the overlay comes down.
    pub(crate) fn settled(&self) -> bool {
        !self.pending_ctrl_b && self.pending_escape.is_empty()
    }

    /// Give up on a partial arrow chord and release what was withheld -- the
    /// `Ctrl-b` and the escape bytes -- as ordinary input, exactly as the
    /// "not a bound chord" fall-through in `scan` does.
    ///
    /// Deliberately does *not* flush a lone pending `Ctrl-b`: waiting
    /// indefinitely for its second key is this keymap's documented behavior
    /// (a chord is only a chord once its key arrives), and only the
    /// multi-byte arrows introduced ambiguity worth timing out.
    pub(crate) fn flush_pending(&mut self) -> Vec<InputAction> {
        if self.pending_escape.is_empty() {
            return Vec::new();
        }
        let mut out = vec![0x02];
        out.append(&mut self.pending_escape);
        vec![InputAction::Forward(out)]
    }

    /// Scan rules (docs/fast-session-switching-design.md section 5.1):
    /// `Ctrl-b d` detaches; `?` flashes the key reference; `r` redraws the
    /// live screen; `[` opens the scrollback pager; `n` creates another
    /// session in this workspace and switches to it; `Right`/`Left` move
    /// between the sessions of this workspace and `Down`/`Up` between
    /// workspaces; `N P l 1-9` switch. Anything else pending is "not a
    /// real prefix" -- the withheld `Ctrl-b` byte is forwarded and the
    /// current byte is reprocessed normally, so unbound `Ctrl-b` sequences
    /// still pass through to the workload untouched.
    pub(crate) fn scan(&mut self, buffer: &[u8]) -> Vec<InputAction> {
        let mut actions = Vec::new();
        let mut out: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < buffer.len() {
            let byte = buffer[i];
            // Mid-arrow: `pending_escape` is only ever non-empty between the
            // `ESC` of a possible `Ctrl-b <arrow>` and its final byte, and
            // `pending_ctrl_b` is cleared before we get here, so the two
            // states cannot both be live.
            if !self.pending_escape.is_empty() {
                let complete = match (self.pending_escape.len(), byte) {
                    // Both encodings: CSI (`ESC [`, normal cursor mode) and
                    // SS3 (`ESC O`, application cursor mode). A TUI can flip
                    // the terminal into either, so binding only one of them
                    // would make the arrows work until the workload changed
                    // its mind.
                    (1, b'[') | (1, b'O') => {
                        self.pending_escape.push(byte);
                        i += 1;
                        continue;
                    }
                    (2, b'A') => Some(SwitchTarget::PrevWorkspace),
                    (2, b'B') => Some(SwitchTarget::NextWorkspace),
                    (2, b'C') => Some(SwitchTarget::Next),
                    (2, b'D') => Some(SwitchTarget::Prev),
                    _ => None,
                };
                match complete {
                    Some(target) => {
                        self.pending_escape.clear();
                        if !out.is_empty() {
                            actions.push(InputAction::Forward(std::mem::take(&mut out)));
                        }
                        actions.push(InputAction::Switch(target));
                        i += 1;
                        continue;
                    }
                    None => {
                        // Not an arrow after all (`Ctrl-b ESC`, `Ctrl-b ESC [ H`,
                        // ...): release the withheld `Ctrl-b` and escape bytes
                        // and reprocess this byte normally -- it may itself be
                        // a fresh `Ctrl-b`, so `i` does not advance.
                        out.push(0x02);
                        out.append(&mut self.pending_escape);
                        continue;
                    }
                }
            }
            if self.pending_ctrl_b {
                self.pending_ctrl_b = false;
                // `ESC` after the prefix is the start of a possible arrow
                // chord; withhold it until the following bytes say which.
                if byte == 0x1b {
                    self.pending_escape.push(byte);
                    i += 1;
                    continue;
                }
                let action = match byte {
                    b'd' => {
                        if !out.is_empty() {
                            actions.push(InputAction::Forward(std::mem::take(&mut out)));
                        }
                        actions.push(InputAction::Detach);
                        return actions;
                    }
                    b'?' => Some(InputAction::Help),
                    b'r' => Some(InputAction::Redraw),
                    b'[' => Some(InputAction::Scroll),
                    // The product ask, on the key the user asked for. Session
                    // navigation lives on the arrows (above), so `n` is free
                    // to mean "new" the way it reads. `p` is deliberately
                    // *unbound*: it was only ever the other half of `n`/`p`,
                    // and leaving it as a lone "previous" next to an `n` that
                    // creates would be a trap. It falls through untouched.
                    b'n' => Some(InputAction::Switch(SwitchTarget::New)),
                    b'N' => Some(InputAction::Switch(SwitchTarget::NextGlobal)),
                    b'P' => Some(InputAction::Switch(SwitchTarget::PrevGlobal)),
                    b'l' => Some(InputAction::Switch(SwitchTarget::Last)),
                    b'1'..=b'9' => Some(InputAction::Switch(SwitchTarget::Index(
                        (byte - b'0') as usize,
                    ))),
                    _ => None,
                };
                if let Some(action) = action {
                    if !out.is_empty() {
                        actions.push(InputAction::Forward(std::mem::take(&mut out)));
                    }
                    actions.push(action);
                    i += 1;
                    continue;
                }
                // Not a bound chord: forward the withheld Ctrl-b and
                // reprocess this byte normally (it might itself be a fresh
                // Ctrl-b) -- do not advance `i`.
                out.push(0x02);
                continue;
            }
            if byte == 0x02 {
                self.pending_ctrl_b = true;
                i += 1;
                continue;
            }
            out.push(byte);
            i += 1;
        }
        if !out.is_empty() {
            actions.push(InputAction::Forward(out));
        }
        actions
    }
}
