/// Where in an escape sequence (or a multi-byte UTF-8 character) a relayed
/// byte stream currently sits, how deeply nested it is inside a
/// synchronized-output block, and which `ESC`/`CSI` sequences it has just
/// completed.
///
/// This exists because `a attach` is a raw byte relay and the status bar
/// interjects its own bytes into that relay. A PTY read boundary lands at
/// an arbitrary byte offset, so "between two chunks" is not "between two
/// escape sequences": measured against a real continuously-streaming TUI,
/// 5 of 10 status-bar redraws were spliced into the middle of an
/// unterminated `CSI` (`...\x1b[38;5;` + our redraw + `91m...`), and the
/// host terminal then printed the workload's remaining parameter bytes as
/// text into its own frame. Splitting a multi-byte UTF-8 character is the
/// same failure with a replacement glyph instead of digits. So the client
/// asks this type "is the stream at a boundary where an injection is
/// invisible?" before writing anything of its own.
///
/// A boundary recognizer, not a parser: it follows `vte`'s transitions --
/// what opens a sequence, what ends it, what aborts it -- so it agrees with
/// the `vt100` model about where sequences are, but never interprets one.
/// The consumers that do (`MarginTracker`) take the completed sequences
/// from `feed_with`. `Copy` with a fixed parameter buffer, so
/// `bytes_to_ground` can probe a copy without allocating on the relay path.
#[derive(Debug, Clone, Copy)]
pub struct StreamBoundary {
    state: BoundaryState,
    /// UTF-8 continuation bytes still expected for the character in flight.
    utf8_remaining: u8,
    /// Nesting depth of `CSI ? 2026 h` / `CSI ? 2026 l` (DEC synchronized
    /// output). A workload that brackets each frame in these -- opencode,
    /// codex and every other Bubble Tea/opentui-style renderer measured for
    /// issue #5 -- is telling us precisely where its frame boundaries are,
    /// for free.
    sync_depth: u16,
    /// Private-marker, parameter and intermediate bytes (`0x20..=0x3f`) of
    /// the CSI in flight -- the first `CSI_PARAM_CAP` of them.
    params: [u8; CSI_PARAM_CAP],
    params_len: u8,
    /// The CSI in flight is a bare `Ps;Ps...` list: no private marker, no
    /// intermediate byte, and short enough to have been kept whole.
    csi_plain: bool,
    /// The `ESC` in flight has collected an intermediate byte (`ESC ( B`),
    /// so its final byte does not make a bare `ESC c`-shaped sequence.
    esc_intermediate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryState {
    Ground,
    /// `ESC` seen (or `ESC` plus intermediate bytes).
    Esc,
    /// `ESC [` seen; consuming parameter/intermediate bytes.
    Csi,
    /// Inside an `OSC`/`DCS`/`SOS`/`PM`/`APC` string. `ESC` ends it (the
    /// `\\` of an `ST` is then an ordinary `ESC` final byte), as does `BEL`
    /// for an `OSC` -- `vte`'s transitions, so the model and this recognizer
    /// agree on where the string stops.
    Str {
        osc: bool,
    },
}

/// Cap on the CSI parameter bytes kept for a sequence's consumers
/// (docs/terminal-state-design.md section 5.4: "Param buffer capped (32
/// bytes; overflow => discard sequence unparsed)"). A longer sequence still
/// ends where it ends -- the boundary is tracked regardless -- but is
/// reported as not `plain`.
const CSI_PARAM_CAP: usize = 32;

/// A complete `ESC` or `CSI` sequence `StreamBoundary::feed_with` has just
/// consumed, in the shape its consumers match on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sequence<'a> {
    /// `ESC final` with no intermediate byte: `ESC c` (RIS), `ESC E` (NEL),
    /// `ESC 7` -- but not `ESC ( B`.
    Esc(u8),
    /// `ESC [ params final`. `plain` is false when a private marker, an
    /// intermediate byte or an over-long parameter list means `params` is
    /// not a bare `Ps;Ps...` list to be read.
    Csi {
        params: &'a [u8],
        final_byte: u8,
        plain: bool,
    },
}

impl Default for StreamBoundary {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamBoundary {
    pub fn new() -> Self {
        Self {
            state: BoundaryState::Ground,
            utf8_remaining: 0,
            sync_depth: 0,
            params: [0; CSI_PARAM_CAP],
            params_len: 0,
            csi_plain: true,
            esc_intermediate: false,
        }
    }

    /// Forget everything, for an in-process session switch: the next
    /// session's bytes are a different stream and cannot continue this one's
    /// half-parsed sequence or synchronized-output block.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// True when the stream is between complete sequences and characters --
    /// the only place the client may write bytes of its own.
    pub fn at_escape_boundary(&self) -> bool {
        self.state == BoundaryState::Ground && self.utf8_remaining == 0
    }

    /// True while the workload has an unclosed `CSI ? 2026 h` block, i.e.
    /// while it is part-way through emitting a frame it asked the terminal
    /// to display atomically.
    pub fn in_synchronized_update(&self) -> bool {
        self.sync_depth > 0
    }

    pub fn feed(&mut self, data: &[u8]) {
        self.feed_with(data, |_| {});
    }

    /// Feed bytes, handing every `ESC`/`CSI` sequence that completes inside
    /// them to `on_sequence`, in order.
    pub(crate) fn feed_with(&mut self, data: &[u8], mut on_sequence: impl FnMut(Sequence<'_>)) {
        for &byte in data {
            if let Some(sequence) = self.step(byte) {
                on_sequence(sequence);
            }
        }
    }

    /// How many leading bytes of `data` this stream needs before it is back
    /// at a boundary (a complete sequence / complete character), or all of
    /// `data` when it does not get there inside this chunk.
    ///
    /// Non-mutating: `ClientScreen::relay` uses it to hand a whole escape
    /// sequence to the model in **one** `process` call instead of walking it
    /// a byte at a time, while still guaranteeing that the model's cursor is
    /// re-read at every point where a printable character could actually be
    /// printed. A probe over a copy of the state machine rather than a
    /// second, subtly-different recognizer.
    pub fn bytes_to_ground(&self, data: &[u8]) -> usize {
        let mut probe = *self;
        for (n, &byte) in data.iter().enumerate() {
            probe.step(byte);
            if probe.at_escape_boundary() {
                return n + 1;
            }
        }
        data.len()
    }

    fn step(&mut self, byte: u8) -> Option<Sequence<'_>> {
        // `vte`'s "anywhere" transitions: `ESC` abandons whatever is in
        // flight -- a half-parsed sequence, a control string, a partial
        // character -- and starts a sequence; `CAN`/`SUB` abandon it and
        // return to ground.
        match byte {
            0x1b => {
                self.state = BoundaryState::Esc;
                self.utf8_remaining = 0;
                self.esc_intermediate = false;
                return None;
            }
            0x18 | 0x1a => {
                self.state = BoundaryState::Ground;
                self.utf8_remaining = 0;
                return None;
            }
            _ => {}
        }
        match self.state {
            BoundaryState::Ground => {
                if self.utf8_remaining > 0 && (0x80..0xc0).contains(&byte) {
                    self.utf8_remaining -= 1;
                    return None;
                }
                // A continuation that was not expected, or a lead byte where
                // a continuation was: the host terminal gives up on the
                // character too, so this byte starts fresh.
                self.utf8_remaining = match byte {
                    0xc2..=0xdf => 1,
                    0xe0..=0xef => 2,
                    0xf0..=0xf4 => 3,
                    _ => 0,
                };
                None
            }
            BoundaryState::Esc => match byte {
                // Intermediate bytes keep the escape sequence open.
                0x20..=0x2f => {
                    self.esc_intermediate = true;
                    None
                }
                b'[' if !self.esc_intermediate => {
                    self.state = BoundaryState::Csi;
                    self.params_len = 0;
                    self.csi_plain = true;
                    None
                }
                b']' if !self.esc_intermediate => {
                    self.state = BoundaryState::Str { osc: true };
                    None
                }
                b'P' | b'X' | b'^' | b'_' if !self.esc_intermediate => {
                    self.state = BoundaryState::Str { osc: false };
                    None
                }
                0x30..=0x7e => {
                    self.state = BoundaryState::Ground;
                    (!self.esc_intermediate).then_some(Sequence::Esc(byte))
                }
                // C0 controls execute without closing the sequence; DEL and
                // high bytes are ignored.
                _ => None,
            },
            BoundaryState::Csi => match byte {
                0x40..=0x7e => {
                    self.state = BoundaryState::Ground;
                    Some(self.finish_csi(byte))
                }
                0x20..=0x3f => {
                    self.record_param(byte);
                    None
                }
                _ => None,
            },
            BoundaryState::Str { osc } => {
                if osc && byte == 0x07 {
                    self.state = BoundaryState::Ground;
                }
                None
            }
        }
    }

    fn record_param(&mut self, byte: u8) {
        let len = usize::from(self.params_len);
        if len == CSI_PARAM_CAP {
            self.csi_plain = false;
            return;
        }
        self.params[len] = byte;
        self.params_len += 1;
        if !matches!(byte, b'0'..=b'9' | b';' | b':') {
            self.csi_plain = false;
        }
    }

    fn finish_csi(&mut self, final_byte: u8) -> Sequence<'_> {
        let len = usize::from(self.params_len);
        if &self.params[..len] == b"?2026" {
            match final_byte {
                b'h' => self.sync_depth = self.sync_depth.saturating_add(1),
                b'l' => self.sync_depth = self.sync_depth.saturating_sub(1),
                _ => {}
            }
        }
        Sequence::Csi {
            params: &self.params[..len],
            final_byte,
            plain: self.csi_plain,
        }
    }
}
