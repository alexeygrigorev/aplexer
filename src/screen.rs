//! Server-side live terminal state (docs/terminal-state-design.md).
//!
//! `ScreenTracker` feeds every PTY byte through a `vt100::Parser` --
//! continuously, whether or not a client is attached -- and can render the
//! *current screen* on demand for reattach (`snapshot()`), instead of
//! replaying raw byte history. This is the aplexer equivalent of tmux's
//! per-pane virtual terminal; see the design doc section 4-6 for the full
//! rationale and section 5.4 for why `MarginTracker` exists alongside it.

/// Byte-level parser states `MarginTracker` walks through. Deliberately not
/// a general escape-sequence parser: only enough state to recognize `ESC c`
/// (RIS) and `ESC [ ... r` (DECSTBM), with everything else falling straight
/// back to `Ground`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarginParseState {
    Ground,
    Esc,
    Csi,
}

/// Cap on how many parameter bytes a single CSI sequence's digits/`;` may
/// accumulate to before this tracker gives up on it (docs/terminal-state-design.md
/// section 5.4: "Param buffer capped (32 bytes; overflow => discard sequence
/// unparsed)").
const MARGIN_PARAM_CAP: usize = 32;

/// A ~50-line worker-side byte state machine that recovers the one piece of
/// terminal state `vt100::Screen` parses correctly during `process()` but
/// does not expose or re-emit in `state_formatted()`: the current DECSTBM
/// scroll-region margins (docs/terminal-state-design.md section 5.4).
///
/// Persistent across chunks by construction -- state lives in `self`, so a
/// sequence split across two PTY reads (`b"\x1b"` in one chunk, `b"[3;20r"`
/// in the next) is handled correctly without any special-casing.
///
/// Recognizes exactly two sequences; everything else passes through
/// unexamined (this is a recognizer for two sequences, not a second
/// emulator):
///
/// - `ESC c` (RIS): margins reset to full-screen; always reported as a
///   margin reset, regardless of what the margins were before.
/// - `ESC [ params r` with **no** private markers (`?`/`<`/`=`/`>`) and no
///   intermediate bytes: DECSTBM. Empty params, or a range that spans the
///   full screen (`top == 1 && bottom == rows`), resets to full-screen and
///   is reported; a validated proper sub-range (`1 <= top < bottom <=
///   rows`) is stored with no reset report (a workload's sub-range is
///   confined to its own rows and cannot reach the client's reserved
///   status-bar row -- see design doc section 7); anything that fails
///   validation is ignored (no state change, no report), matching how real
///   terminals silently ignore a malformed DECSTBM.
#[derive(Debug, Clone)]
pub struct MarginTracker {
    rows: u16,
    state: MarginParseState,
    param_buf: Vec<u8>,
    /// Set when this CSI sequence has a private marker or an intermediate
    /// byte, which disqualifies it from being the bare `CSI params r` this
    /// tracker recognizes.
    disqualified: bool,
    /// Current scroll region, 1-based inclusive `(top, bottom)`. `None`
    /// means full-screen (the default, and the common case).
    margins: Option<(u16, u16)>,
}

impl MarginTracker {
    pub fn new(rows: u16) -> Self {
        Self {
            rows: rows.max(1),
            state: MarginParseState::Ground,
            param_buf: Vec::new(),
            disqualified: false,
            margins: None,
        }
    }

    /// Current scroll region, if non-default.
    pub fn margins(&self) -> Option<(u16, u16)> {
        self.margins
    }

    /// xterm resets margins on resize; matching that is the least-surprise
    /// approximation (design doc section 5.3). Also resets any in-flight
    /// partial CSI parse, since the row count it would validate against is
    /// changing anyway.
    pub fn set_rows(&mut self, rows: u16) {
        self.rows = rows.max(1);
        self.margins = None;
        self.state = MarginParseState::Ground;
        self.param_buf.clear();
        self.disqualified = false;
    }

    /// Feed a chunk of raw PTY bytes. Returns `true` if this chunk caused a
    /// margin *reset* the client should react to (re-assert its own
    /// status-bar reservation) -- see design doc section 7.
    pub fn scan(&mut self, data: &[u8]) -> bool {
        let mut reset = false;
        for &byte in data {
            if self.step(byte) {
                reset = true;
            }
        }
        reset
    }

    fn step(&mut self, byte: u8) -> bool {
        match self.state {
            MarginParseState::Ground => {
                if byte == 0x1b {
                    self.state = MarginParseState::Esc;
                }
                false
            }
            MarginParseState::Esc => match byte {
                b'c' => {
                    self.state = MarginParseState::Ground;
                    self.margins = None;
                    true
                }
                b'[' => {
                    self.state = MarginParseState::Csi;
                    self.param_buf.clear();
                    self.disqualified = false;
                    false
                }
                _ => {
                    // Not a sequence we track -- back to ground so the next
                    // byte is processed fresh.
                    self.state = MarginParseState::Ground;
                    false
                }
            },
            MarginParseState::Csi => match byte {
                b'0'..=b'9' | b';' => {
                    if self.param_buf.len() >= MARGIN_PARAM_CAP {
                        // Overflow: discard this sequence unparsed.
                        self.state = MarginParseState::Ground;
                    } else {
                        self.param_buf.push(byte);
                    }
                    false
                }
                b'?' | b'<' | b'=' | b'>' => {
                    self.disqualified = true;
                    false
                }
                0x20..=0x2f => {
                    // Intermediate byte.
                    self.disqualified = true;
                    false
                }
                0x40..=0x7e => {
                    let reset = self.finish_csi(byte);
                    self.state = MarginParseState::Ground;
                    reset
                }
                _ => false,
            },
        }
    }

    /// `final_byte` is the CSI sequence's terminating byte. Returns whether
    /// this was a margin-resetting DECSTBM/RIS-equivalent.
    fn finish_csi(&mut self, final_byte: u8) -> bool {
        if final_byte != b'r' || self.disqualified {
            return false;
        }
        let text = match std::str::from_utf8(&self.param_buf) {
            Ok(text) => text,
            Err(_) => return false,
        };
        if text.is_empty() {
            self.margins = None;
            return true;
        }
        let mut parts = text.splitn(2, ';');
        let top_raw = parts.next().unwrap_or("");
        let bottom_raw = parts.next().unwrap_or("");
        let top: u16 = if top_raw.is_empty() {
            1
        } else {
            match top_raw.parse() {
                Ok(value) => value,
                Err(_) => return false,
            }
        };
        let bottom: u16 = if bottom_raw.is_empty() {
            self.rows
        } else {
            match bottom_raw.parse() {
                Ok(value) => value,
                Err(_) => return false,
            }
        };
        if top < 1 || bottom > self.rows || top >= bottom {
            // Malformed / out of range: real terminals ignore this; so do
            // we -- no state change, no report.
            return false;
        }
        if top == 1 && bottom == self.rows {
            self.margins = None;
            true
        } else {
            self.margins = Some((top, bottom));
            false
        }
    }
}

/// What the workload did that the attached client must react to (design doc
/// section 5.1/7): re-assert its DECSTBM status-bar reservation and redraw
/// the bar. Fired on a margin reset (RIS or a full-range/empty DECSTBM) or
/// on an alternate-screen enter/exit -- margins are formally preserved
/// across 1049 on xterm, but emulator variance exists and TUIs commonly wrap
/// transitions in `\x1b[r`, so the client re-asserts unconditionally on
/// every flip; it is idempotent and cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutChange {
    pub alt_screen: bool,
    pub margins_reset: bool,
}

/// The live per-session screen model: a `vt100::Parser` fed continuously by
/// the worker's PTY reader, plus the `MarginTracker` that compensates for
/// the one gap in what `vt100` exposes. See design doc sections 4-6.
pub struct ScreenTracker {
    parser: vt100::Parser,
    margins: MarginTracker,
    /// Last-observed `alternate_screen()` value, for flip detection.
    alt_screen: bool,
}

impl ScreenTracker {
    /// `Parser::new(rows, cols, 0)` -- zero model scrollback; scrolling is
    /// the host terminal's job (docs/scrollback-design.md), and this caps
    /// the model's memory at the two-grid cost (design doc section 5.2).
    pub fn new(rows: u16, cols: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
            margins: MarginTracker::new(rows),
            alt_screen: false,
        }
    }

    /// Feed PTY bytes; returns `Some(LayoutChange)` when the workload did
    /// something the attached client must react to.
    pub fn process(&mut self, data: &[u8]) -> Option<LayoutChange> {
        let margins_reset = self.margins.scan(data);
        self.parser.process(data);
        let now_alt = self.parser.screen().alternate_screen();
        let alt_flip = now_alt != self.alt_screen;
        self.alt_screen = now_alt;
        if margins_reset || alt_flip {
            Some(LayoutChange {
                alt_screen: now_alt,
                margins_reset,
            })
        } else {
            None
        }
    }

    /// Resizes both the parser's grid (content-preserving) and resets the
    /// margin tracker to full-screen (design doc section 5.3).
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        self.parser.screen_mut().set_size(rows, cols);
        self.margins.set_rows(rows);
    }

    /// Plain text of the current screen, for `a capture --screen` and the
    /// dead-session `screen.txt` fallback (design doc section 5.5/8).
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    /// The reattach payload (design doc section 6.2), in order:
    ///
    /// 1. `\x1b[?1049h` -- only if the live screen is on the alternate
    ///    screen, so the *host* terminal genuinely switches too.
    /// 2. `state_formatted()` -- clear + full active-grid repaint + cursor
    ///    position/visibility + input modes (bracketed paste, mouse,
    ///    application keypad/cursor).
    /// 3. If `MarginTracker` holds non-default margins: the DECSTBM
    ///    sequence, followed by re-fixing the cursor (DECSTBM homes the
    ///    cursor as a side effect on real terminals). Skipped when margins
    ///    are default, leaving the client's own status-bar reservation in
    ///    force.
    pub fn snapshot(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut out = Vec::new();
        if screen.alternate_screen() {
            out.extend_from_slice(b"\x1b[?1049h");
        }
        out.extend_from_slice(&screen.state_formatted());
        if let Some((top, bottom)) = self.margins.margins() {
            out.extend_from_slice(format!("\x1b[{top};{bottom}r").as_bytes());
            let (row, col) = screen.cursor_position();
            out.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- MarginTracker --

    #[test]
    fn margin_tracker_default_is_full_screen() {
        let t = MarginTracker::new(24);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_sub_range_stored_no_reset() {
        let mut t = MarginTracker::new(24);
        let reset = t.scan(b"\x1b[3;20r");
        assert!(!reset);
        assert_eq!(t.margins(), Some((3, 20)));
    }

    #[test]
    fn margin_tracker_full_range_reports_reset() {
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;20r");
        let reset = t.scan(b"\x1b[1;24r");
        assert!(reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_bare_r_reports_reset() {
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;20r");
        let reset = t.scan(b"\x1b[r");
        assert!(reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_ris_reports_reset() {
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;20r");
        let reset = t.scan(b"\x1bc");
        assert!(reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_split_at_every_byte_boundary() {
        let seq = b"\x1b[3;20r";
        for split in 0..=seq.len() {
            let mut t = MarginTracker::new(24);
            let reset1 = t.scan(&seq[..split]);
            let reset2 = t.scan(&seq[split..]);
            assert!(!reset1 && !reset2, "split at {split} reported a spurious reset");
            assert_eq!(t.margins(), Some((3, 20)), "split at {split} lost the margin");
        }
    }

    #[test]
    fn margin_tracker_esc_then_c_split() {
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;20r");
        assert!(!t.scan(b"\x1b"));
        assert!(t.scan(b"c"));
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_private_marker_ignored() {
        let mut t = MarginTracker::new(24);
        // DECSET/DECRST-shaped private sequence ending in 'r' must not be
        // mistaken for DECSTBM.
        let reset = t.scan(b"\x1b[?1049r");
        assert!(!reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_alt_screen_enter_not_mistaken_for_margin() {
        let mut t = MarginTracker::new(24);
        let reset = t.scan(b"\x1b[?1049h");
        assert!(!reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_oversized_params_discarded() {
        let mut t = MarginTracker::new(24);
        let mut seq = b"\x1b[".to_vec();
        seq.extend(std::iter::repeat(b'1').take(64));
        seq.push(b'r');
        let reset = t.scan(&seq);
        assert!(!reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_invalid_range_ignored() {
        let mut t = MarginTracker::new(24);
        // top >= bottom: invalid, ignored.
        let reset = t.scan(b"\x1b[20;3r");
        assert!(!reset);
        assert_eq!(t.margins(), None);
        // bottom > rows: invalid, ignored.
        let reset = t.scan(b"\x1b[1;99r");
        assert!(!reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_resize_resets_margins() {
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;20r");
        assert_eq!(t.margins(), Some((3, 20)));
        t.set_rows(30);
        assert_eq!(t.margins(), None);
    }

    // -- ScreenTracker: Layout events --

    #[test]
    fn alt_screen_enter_emits_exactly_one_layout_change() {
        let mut tracker = ScreenTracker::new(24, 80);
        let mut changes = 0;
        // Feed byte-by-byte so a naive implementation firing per-byte would
        // be caught.
        for &byte in b"\x1b[?1049h" {
            if tracker.process(&[byte]).is_some() {
                changes += 1;
            }
        }
        assert_eq!(changes, 1);
        assert!(tracker.process(b"hello").is_none());
    }

    #[test]
    fn margin_reset_emits_layout_change() {
        let mut tracker = ScreenTracker::new(24, 80);
        assert!(tracker.process(b"\x1b[3;20r").is_none());
        let change = tracker.process(b"\x1b[r").expect("reset should emit a change");
        assert!(change.margins_reset);
        assert!(!change.alt_screen);
    }

    // -- Round-trip property (design doc section 6.2/12 item 10): feeding
    // the snapshot to a fresh, same-sized parser must reproduce contents(),
    // cursor position, and alternate_screen(). --

    fn round_trip_check(stream: &[u8], rows: u16, cols: u16) {
        let mut a = ScreenTracker::new(rows, cols);
        a.process(stream);
        let snapshot = a.snapshot();

        let mut b = vt100::Parser::new(rows, cols, 0);
        b.process(&snapshot);

        let screen_a = a.parser.screen();
        let screen_b = b.screen();
        assert_eq!(screen_a.contents(), screen_b.contents(), "contents mismatch");
        assert_eq!(
            screen_a.cursor_position(),
            screen_b.cursor_position(),
            "cursor position mismatch"
        );
        assert_eq!(
            screen_a.alternate_screen(),
            screen_b.alternate_screen(),
            "alternate_screen mismatch"
        );
        assert_eq!(
            screen_a.bracketed_paste(),
            screen_b.bracketed_paste(),
            "bracketed_paste mismatch"
        );
        assert_eq!(
            screen_a.mouse_protocol_mode(),
            screen_b.mouse_protocol_mode(),
            "mouse_protocol_mode mismatch"
        );
    }

    #[test]
    fn round_trip_plain_shell_scrollout() {
        let stream = b"$ echo hello\r\nhello\r\n$ ls\r\nfoo bar baz\r\n$ ";
        round_trip_check(stream, 24, 80);
    }

    #[test]
    fn round_trip_codex_like_alt_screen_tui_with_colors_and_bracketed_paste() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"\x1b[?1049h"); // enter alt screen
        stream.extend_from_slice(b"\x1b[?2004h"); // bracketed paste on
        stream.extend_from_slice(b"\x1b[2J\x1b[H");
        stream.extend_from_slice(b"\x1b[1;36m\xe2\x94\x8c\xe2\x94\x80\xe2\x94\x80\xe2\x94\x90\x1b[0m\r\n");
        stream.extend_from_slice(b"\x1b[32mAsk Codex to do anything\x1b[0m\r\n");
        stream.extend_from_slice(b"\x1b[7m status: idle \x1b[0m\r\n");
        stream.extend_from_slice(b"\x1b[10;5H");
        round_trip_check(&stream, 24, 80);
    }

    #[test]
    fn round_trip_decstbm_sub_range_workload() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"\x1b[3;20r");
        stream.extend_from_slice(b"line one\r\nline two\r\n");
        round_trip_check(&stream, 24, 80);
    }

    #[test]
    fn round_trip_stream_ending_mid_escape_sequence() {
        let mut stream = Vec::new();
        stream.extend_from_slice(b"hello world\r\n");
        stream.extend_from_slice(b"\x1b[31m"); // truncated mid-SGR-ish (still complete here)
        stream.extend_from_slice(b"\x1b["); // then genuinely truncated CSI
        round_trip_check(&stream, 24, 80);
    }

    #[test]
    fn round_trip_after_resize() {
        let mut tracker = ScreenTracker::new(24, 80);
        tracker.process(b"hello\r\nworld\r\n");
        tracker.set_size(30, 100);
        tracker.process(b"more output after resize\r\n");
        let snapshot = tracker.snapshot();
        let mut b = vt100::Parser::new(30, 100, 0);
        b.process(&snapshot);
        assert_eq!(tracker.contents(), b.screen().contents());
        assert_eq!(tracker.parser.screen().cursor_position(), b.screen().cursor_position());
    }

    #[test]
    fn contents_plain_text_matches_screen() {
        let mut tracker = ScreenTracker::new(24, 80);
        tracker.process(b"hello there\r\n");
        assert!(tracker.contents().contains("hello there"));
    }
}
