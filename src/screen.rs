//! Server-side live terminal state (docs/terminal-state-design.md).
//!
//! `ScreenTracker` feeds every PTY byte through a `vt100::Parser` --
//! continuously, whether or not a client is attached -- and can render the
//! *current screen* on demand for reattach (`snapshot()`), instead of
//! replaying raw byte history. This is the aplexer equivalent of tmux's
//! per-pane virtual terminal; see the design doc section 4-6 for the full
//! rationale and section 5.4 for why `MarginTracker` exists alongside it.

use anyhow::{bail, Result};

/// Maximum number of cells retained by the worker's live terminal model.
///
/// `vt100` keeps both normal and alternate grids and each cell carries
/// formatting state, so accepting the protocol's full `u16 * u16` range
/// would let a local client request multiple gigabytes of allocation. This
/// still permits unusually large terminals (for example 512x512) while
/// keeping each session's model within a defensible fixed bound.
pub const MAX_SCREEN_CELLS: usize = 256 * 1024;

/// Normalize the protocol's zero dimensions and reject grids that would
/// exceed the worker's fixed cell budget. `checked_mul` keeps this correct if
/// the dimension types are widened in the future.
pub fn validate_size(rows: u16, cols: u16) -> Result<(u16, u16)> {
    let rows = rows.max(1);
    let cols = cols.max(1);
    let cells = usize::from(rows)
        .checked_mul(usize::from(cols))
        .ok_or_else(|| anyhow::anyhow!("terminal dimensions overflow"))?;
    if cells > MAX_SCREEN_CELLS {
        bail!("terminal size {rows}x{cols} exceeds the maximum of {MAX_SCREEN_CELLS} cells");
    }
    Ok((rows, cols))
}

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
///   rows`) is stored with no reset report, because the client re-asserts
///   that sub-range rather than replacing it (see `draw_status_bar` in
///   `src/bin/a.rs` and design doc section 7 -- including the documented
///   limitation that a sub-range does not protect the client's reserved row
///   the way the client's own reservation does); anything that fails
///   validation is ignored (no state change, no report), matching how real
///   terminals silently ignore a malformed DECSTBM.
/// - `ESC [ ... J` (Erase in Display / ED), any parameter and any private
///   marker: reported unconditionally as a `CsiEvent::erase` trigger,
///   regardless of the `Ps` value. ED ignores scroll margins per spec, so
///   even a scoped DECSTBM sub-range doesn't protect the client's reserved
///   bottom row from an `ED2`/`ED3` full-screen erase -- and Ink-based TUIs
///   (Codex, Claude Code) send exactly that on nearly every redraw. Being
///   unconditional (not trying to determine from cursor position whether a
///   bare/`0J` "cursor to end of screen" could reach the last row) is a
///   deliberate over-trigger: the fallout is one extra harmless status-bar
///   redraw, while under-triggering means the bar can stay silently wiped
///   until the next debounce/max-interval tick.
#[derive(Debug, Clone)]
pub struct MarginTracker {
    rows: u16,
    state: MarginParseState,
    param_buf: Vec<u8>,
    /// Set when this CSI sequence has a private marker or an intermediate
    /// byte, which disqualifies it from being the bare `CSI params r` this
    /// tracker recognizes.
    disqualified: bool,
    /// Current scroll region as *tracked*, 1-based inclusive `(top, bottom)`;
    /// `None` means full-screen (the default, and the common case).
    ///
    /// Deliberately not the same thing as what should be *emitted*: a resize
    /// can collapse this onto a single row (`top == bottom`), which the grid
    /// beside it also does and which a later resize can grow back, but which
    /// is not expressible as a DECSTBM. `margins()` is the emission-facing
    /// view that filters that out; nothing outside this type reads the field.
    region: Option<(u16, u16)>,
}

impl MarginTracker {
    pub fn new(rows: u16) -> Self {
        Self {
            rows: rows.max(1),
            state: MarginParseState::Ground,
            param_buf: Vec::new(),
            disqualified: false,
            region: None,
        }
    }

    /// The current scroll region as it should be **emitted**: a proper
    /// sub-range, or `None` for "no sub-range -- leave the default, or the
    /// client's own status-bar reservation, in force".
    ///
    /// Filtered, not raw. `set_rows` can leave the tracked region collapsed
    /// onto a single row, matching what the `vt100` grid does (see its doc
    /// comment for why keeping it matters), but a one-row region is not
    /// expressible as a DECSTBM at all: `finish_csi` here and vt100's own
    /// `set_scroll_region` both require `top < bottom`, so emitting
    /// `\x1b[8;8r` would be ignored by the host terminal and leave whatever
    /// region was previously in force -- worse than saying nothing. Both
    /// emission sites (`ScreenTracker::snapshot` and `draw_status_bar` in
    /// `src/bin/a.rs`) want "no sub-range" in that case, which is exactly what
    /// `None` already means to them, so the filter lives here rather than
    /// being repeated at each of them.
    pub fn margins(&self) -> Option<(u16, u16)> {
        self.region.filter(|&(top, bottom)| top < bottom)
    }

    /// The region as *tracked*, before that emission-time filtering -- the
    /// state this tracker carries into the next `set_rows`, which is what has
    /// to match the `vt100` grid case-for-case.
    #[cfg(test)]
    fn tracked_region(&self) -> Option<(u16, u16)> {
        self.region
    }

    /// Forgets everything: full-screen margins and no half-parsed sequence.
    ///
    /// Distinct from `set_rows`, which *clamps* rather than clears (see its
    /// doc comment). This is for the case where the bytes being tracked start
    /// belonging to a different terminal altogether -- the client's in-process
    /// session switch (`Ctrl-b n`), where continuing to hold the previous
    /// session's scroll region would apply it to the new one.
    pub fn reset(&mut self) {
        self.region = None;
        self.state = MarginParseState::Ground;
        self.param_buf.clear();
        self.disqualified = false;
    }

    /// Re-fits the tracked region to a new row count, following
    /// `vt100::Screen::set_size`'s rules for the grid's own scroll region --
    /// exactly, with no exemptions (what `margins()` chooses to *report* for a
    /// degenerate region is a separate, emission-time question; see below).
    ///
    /// This deliberately does **not** follow design doc section 5.3's
    /// "margins reset to full-screen on resize, matching xterm". That would
    /// be right for a tracker modelling a *terminal*, but this one models
    /// what the `vt100` grid beside it believes, because `snapshot()` pairs
    /// the grid's `state_formatted()` with *these* margins.
    ///
    /// vt100 0.16.2 (`grid.rs::set_size`, lines 66-99) applies three rules,
    /// in this order, all translated here from its 0-based half-inclusive
    /// storage to this tracker's 1-based inclusive `(top, bottom)`:
    ///
    /// 1. A **bottom-anchored** region -- one whose bottom edge sits on the
    ///    old screen's last row -- follows the screen, in *both* directions
    ///    (`if scroll_bottom == self.size.rows - 1 { scroll_bottom =
    ///    size.rows - 1 }`). So `(3,23)` at 23 rows becomes `(3,39)` at 39
    ///    rows: "two fixed header rows, everything below scrolls" keeps
    ///    meaning that after the terminal is enlarged.
    /// 2. A bottom past the new end is clamped to it, top preserved:
    ///    `(5,23)` at 20 rows becomes `(5,20)`.
    /// 3. A top that no longer fits below the clamped bottom degenerates to
    ///    the full screen: `(21,23)` at 10 rows becomes full-screen (`None`).
    ///
    /// A region that still fits is left alone, which is the case every
    /// attach hits -- and resetting instead made this tracker and the grid
    /// disagree after every resize. Since *every* attach resizes the PTY by
    /// one row to reserve the status-bar row, the practical effect was that
    /// attaching to a workload with a scroll region silently dropped that
    /// region from the snapshot, and the host then scrolled the wrong rows
    /// for the rest of the session. See
    /// `round_trip_preserves_scroll_region_across_resize`, and
    /// `margin_tracker_resize_matches_real_vt100_set_size` for the
    /// case-by-case differential against the real crate.
    ///
    /// **The degenerate one-row case.** When the clamps leave `top == bottom`
    /// (only reachable as `top == bottom == rows`, e.g. `(5,15)` resized to 5
    /// rows), vt100's grid keeps that single-row region -- and so does this
    /// tracker. It is held as `Some((rows, rows))` and filtered out only at
    /// emission time by `margins()`, which reports "no sub-range" because a
    /// one-row region is not expressible as a DECSTBM.
    ///
    /// Keeping it is load-bearing rather than pedantic. A collapsed region is
    /// always bottom-anchored -- `bottom == rows` by construction -- so rule 1
    /// grows it again on the next enlargement, exactly as vt100 does:
    /// `\x1b[8;9r` at 20 rows, shrunk to 8 rows and re-grown to 24, is
    /// `(8,24)` in the grid, and now here too. Dropping it to full-screen
    /// instead made the loss **sticky**: the tracker reported full-screen from
    /// then on, at every later size, while the grid whose `state_formatted()`
    /// `snapshot()` pairs these margins with held an ordinary region.
    ///
    /// An earlier version of this comment called that divergence
    /// inconsequential *by construction*, on the grounds that a one-row region
    /// "could not be re-emitted even if it were tracked". That reasoning was
    /// wrong: it is about what can be emitted at that instant and says nothing
    /// about what the region becomes after the next resize, which is when the
    /// discarded state was needed. Measured over a two-step sweep, 4,823 of
    /// 147,420 resize pairs ended up reporting a region that disagreed with
    /// vt100 about a perfectly expressible sub-range. Both sweeps below now
    /// pin the tracked region against the real crate with no exemption at all:
    /// `margin_tracker_resize_divergence_from_vt100_is_only_the_degenerate_row`
    /// (single step) and
    /// `margin_tracker_tracked_region_matches_vt100_across_two_resizes`, plus
    /// `margin_tracker_regrows_a_region_a_shrink_collapsed_onto_one_row` for
    /// the reported-margins path end to end. The only remaining difference
    /// anywhere is what `margins()` *reports* while the region is degenerate,
    /// which is an emission decision, documented on `margins()`, and no longer
    /// costs the tracker any state.
    ///
    /// Any in-flight partial CSI parse is deliberately preserved: a DECSTBM
    /// split across two PTY reads with a resize landing in between is still
    /// a DECSTBM, and dropping it would lose exactly the state this tracker
    /// exists to keep.
    pub fn set_rows(&mut self, rows: u16) {
        let rows = rows.max(1);
        let old_rows = self.rows;
        self.rows = rows;
        self.region = match self.region {
            Some((top, bottom)) => {
                let bottom = if bottom == old_rows {
                    // Rule 1: bottom-anchored, so it follows the screen (and
                    // needs no further clamping -- it *is* the new bottom).
                    rows
                } else {
                    // Rule 2.
                    bottom.min(rows)
                };
                // Rule 3 (`top > bottom`, which after the clamps can only
                // mean `bottom == rows`, i.e. the full screen), plus the same
                // whole-screen normalization `finish_csi` applies -- both of
                // which this tracker spells `None`.
                //
                // `top == bottom` is deliberately *not* in here: that is the
                // degenerate single-row region, which vt100's grid keeps and
                // this tracker keeps with it, so a later enlargement can grow
                // it back (see the doc comment above). `margins()` is what
                // declines to emit it.
                if top > bottom || (top == 1 && bottom == rows) {
                    None
                } else {
                    Some((top, bottom))
                }
            }
            // Full-screen is bottom-anchored by definition, so rule 1 keeps
            // it full-screen at the new size.
            None => None,
        };
    }

    /// Feed a chunk of raw PTY bytes. Returns the triggers this chunk
    /// caused that the client should react to (re-assert its own
    /// status-bar reservation) -- see design doc section 7 and `CsiEvent`.
    pub fn scan(&mut self, data: &[u8]) -> CsiEvent {
        let mut result = CsiEvent::default();
        for &byte in data {
            let event = self.step(byte);
            result.margins_reset |= event.margins_reset;
            result.erase |= event.erase;
        }
        result
    }

    fn step(&mut self, byte: u8) -> CsiEvent {
        match self.state {
            MarginParseState::Ground => {
                if byte == 0x1b {
                    self.state = MarginParseState::Esc;
                }
                CsiEvent::default()
            }
            MarginParseState::Esc => match byte {
                b'c' => {
                    self.state = MarginParseState::Ground;
                    self.region = None;
                    CsiEvent {
                        margins_reset: true,
                        erase: false,
                    }
                }
                b'[' => {
                    self.state = MarginParseState::Csi;
                    self.param_buf.clear();
                    self.disqualified = false;
                    CsiEvent::default()
                }
                _ => {
                    // Not a sequence we track -- back to ground so the next
                    // byte is processed fresh.
                    self.state = MarginParseState::Ground;
                    CsiEvent::default()
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
                    CsiEvent::default()
                }
                b'?' | b'<' | b'=' | b'>' => {
                    self.disqualified = true;
                    CsiEvent::default()
                }
                0x20..=0x2f => {
                    // Intermediate byte.
                    self.disqualified = true;
                    CsiEvent::default()
                }
                0x40..=0x7e => {
                    let event = self.finish_csi(byte);
                    self.state = MarginParseState::Ground;
                    event
                }
                _ => CsiEvent::default(),
            },
        }
    }

    /// `final_byte` is the CSI sequence's terminating byte. Returns the
    /// triggers this sequence caused, if any.
    fn finish_csi(&mut self, final_byte: u8) -> CsiEvent {
        if final_byte == b'J' {
            // Erase in Display -- see `CsiEvent::erase`'s doc comment above:
            // unconditional, regardless of Ps or `disqualified`.
            return CsiEvent {
                margins_reset: false,
                erase: true,
            };
        }
        if final_byte != b'r' || self.disqualified {
            return CsiEvent::default();
        }
        let text = match std::str::from_utf8(&self.param_buf) {
            Ok(text) => text,
            Err(_) => return CsiEvent::default(),
        };
        if text.is_empty() {
            self.region = None;
            return CsiEvent {
                margins_reset: true,
                erase: false,
            };
        }
        let mut parts = text.splitn(2, ';');
        let top_raw = parts.next().unwrap_or("");
        let bottom_raw = parts.next().unwrap_or("");
        let top: u16 = if top_raw.is_empty() {
            1
        } else {
            match top_raw.parse() {
                Ok(value) => value,
                Err(_) => return CsiEvent::default(),
            }
        };
        let bottom: u16 = if bottom_raw.is_empty() {
            self.rows
        } else {
            match bottom_raw.parse() {
                Ok(value) => value,
                Err(_) => return CsiEvent::default(),
            }
        };
        if top < 1 || bottom > self.rows || top >= bottom {
            // Malformed / out of range: real terminals ignore this; so do
            // we -- no state change, no report.
            return CsiEvent::default();
        }
        if top == 1 && bottom == self.rows {
            self.region = None;
            CsiEvent {
                margins_reset: true,
                erase: false,
            }
        } else {
            self.region = Some((top, bottom));
            CsiEvent::default()
        }
    }
}

/// What a scanned chunk of PTY bytes did that `ScreenTracker::process`
/// should fold into a `LayoutChange` -- see `MarginTracker`'s doc comment
/// for exactly which sequences set which field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CsiEvent {
    pub margins_reset: bool,
    pub erase: bool,
}

/// What the workload did that the attached client must react to (design doc
/// section 5.1/7): re-assert its DECSTBM status-bar reservation and redraw
/// the bar. Fired on a margin reset (RIS or a full-range/empty DECSTBM), on
/// an alternate-screen enter/exit -- margins are formally preserved across
/// 1049 on xterm, but emulator variance exists and TUIs commonly wrap
/// transitions in `\x1b[r`, so the client re-asserts unconditionally on
/// every flip -- or on an Erase in Display (`CSI ... J`), which ignores
/// scroll margins per spec and so can wipe the client's reserved bottom row
/// even under an otherwise-untouched DECSTBM sub-range. All three triggers
/// are idempotent and cheap to react to, so being liberal about firing them
/// (`erase_reset` in particular is unconditional on `Ps`, see
/// `CsiEvent::erase`'s doc comment) costs nothing but an extra redraw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutChange {
    pub alt_screen: bool,
    pub margins_reset: bool,
    pub erase_reset: bool,
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
    pub fn try_new(rows: u16, cols: u16) -> Result<Self> {
        let (rows, cols) = validate_size(rows, cols)?;
        Ok(Self {
            // Invariant: the third argument (scrollback rows) must stay 0.
            // `ScreenTracker` is a current-screen-only cache, never a
            // retained history buffer -- retaining scrollback here would
            // expose it to resize-time reflow of that history, which is
            // exactly the class of bug that garbles tmux scrollback (see
            // module doc above and docs/scrollback-design.md sections 2-3).
            parser: vt100::Parser::new(rows, cols, 0),
            margins: MarginTracker::new(rows),
            alt_screen: false,
        })
    }

    #[cfg(test)]
    pub fn new(rows: u16, cols: u16) -> Self {
        Self::try_new(rows, cols).expect("test terminal size should be valid")
    }

    /// Feed PTY bytes; returns `Some(LayoutChange)` when the workload did
    /// something the attached client must react to.
    pub fn process(&mut self, data: &[u8]) -> Option<LayoutChange> {
        let csi = self.margins.scan(data);
        self.parser.process(data);
        let now_alt = self.parser.screen().alternate_screen();
        let alt_flip = now_alt != self.alt_screen;
        self.alt_screen = now_alt;
        if csi.margins_reset || csi.erase || alt_flip {
            Some(LayoutChange {
                alt_screen: now_alt,
                margins_reset: csi.margins_reset,
                erase_reset: csi.erase,
            })
        } else {
            None
        }
    }

    /// Resizes the parser's grid (content-preserving) and re-fits the margin
    /// tracker to the new row count the same way the grid re-fits its own
    /// scroll region -- it is *not* reset to full-screen, correcting design
    /// doc section 5.3's original "margins reset on resize" plan. See
    /// `MarginTracker::set_rows` for the exact rules and why the tracker has
    /// to follow the grid rather than a real terminal here.
    pub fn try_set_size(&mut self, rows: u16, cols: u16) -> Result<()> {
        let (rows, cols) = validate_size(rows, cols)?;
        self.parser.screen_mut().set_size(rows, cols);
        self.margins.set_rows(rows);
        Ok(())
    }

    #[cfg(test)]
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        self.try_set_size(rows, cols)
            .expect("test terminal size should be valid");
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

    #[test]
    fn screen_dimensions_are_bounded_and_overflow_safe() {
        assert_eq!(validate_size(0, 0).unwrap(), (1, 1));
        assert_eq!(validate_size(512, 512).unwrap(), (512, 512));
        assert!(validate_size(513, 512).is_err());
        assert!(validate_size(u16::MAX, u16::MAX).is_err());
    }

    // -- MarginTracker --

    #[test]
    fn margin_tracker_default_is_full_screen() {
        let t = MarginTracker::new(24);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_sub_range_stored_no_reset() {
        let mut t = MarginTracker::new(24);
        let event = t.scan(b"\x1b[3;20r");
        assert!(!event.margins_reset);
        assert!(!event.erase);
        assert_eq!(t.margins(), Some((3, 20)));
    }

    #[test]
    fn margin_tracker_full_range_reports_reset() {
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;20r");
        let event = t.scan(b"\x1b[1;24r");
        assert!(event.margins_reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_bare_r_reports_reset() {
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;20r");
        let event = t.scan(b"\x1b[r");
        assert!(event.margins_reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_ris_reports_reset() {
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;20r");
        let event = t.scan(b"\x1bc");
        assert!(event.margins_reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_split_at_every_byte_boundary() {
        let seq = b"\x1b[3;20r";
        for split in 0..=seq.len() {
            let mut t = MarginTracker::new(24);
            let event1 = t.scan(&seq[..split]);
            let event2 = t.scan(&seq[split..]);
            assert!(
                !event1.margins_reset && !event2.margins_reset,
                "split at {split} reported a spurious reset"
            );
            assert_eq!(t.margins(), Some((3, 20)), "split at {split} lost the margin");
        }
    }

    #[test]
    fn margin_tracker_esc_then_c_split() {
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;20r");
        assert!(!t.scan(b"\x1b").margins_reset);
        assert!(t.scan(b"c").margins_reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_private_marker_ignored() {
        let mut t = MarginTracker::new(24);
        // DECSET/DECRST-shaped private sequence ending in 'r' must not be
        // mistaken for DECSTBM.
        let event = t.scan(b"\x1b[?1049r");
        assert!(!event.margins_reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_alt_screen_enter_not_mistaken_for_margin() {
        let mut t = MarginTracker::new(24);
        let event = t.scan(b"\x1b[?1049h");
        assert!(!event.margins_reset);
        assert!(!event.erase);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_oversized_params_discarded() {
        let mut t = MarginTracker::new(24);
        let mut seq = b"\x1b[".to_vec();
        seq.extend(std::iter::repeat(b'1').take(64));
        seq.push(b'r');
        let event = t.scan(&seq);
        assert!(!event.margins_reset);
        assert_eq!(t.margins(), None);
    }

    #[test]
    fn margin_tracker_invalid_range_ignored() {
        let mut t = MarginTracker::new(24);
        // top >= bottom: invalid, ignored.
        let event = t.scan(b"\x1b[20;3r");
        assert!(!event.margins_reset);
        assert_eq!(t.margins(), None);
        // bottom > rows: invalid, ignored.
        let event = t.scan(b"\x1b[1;99r");
        assert!(!event.margins_reset);
        assert_eq!(t.margins(), None);
    }

    // -- MarginTracker: Erase in Display (CSI ... J) detection --

    #[test]
    fn margin_tracker_full_erase_reports_erase() {
        let mut t = MarginTracker::new(24);
        let event = t.scan(b"\x1b[2J");
        assert!(event.erase);
        assert!(!event.margins_reset);
    }

    #[test]
    fn margin_tracker_scrollback_erase_reports_erase() {
        let mut t = MarginTracker::new(24);
        let event = t.scan(b"\x1b[3J");
        assert!(event.erase);
    }

    #[test]
    fn margin_tracker_bare_erase_reports_erase() {
        // Bare `CSI J` / `CSI 0J` ("cursor to end of screen") could
        // plausibly reach the bottom row depending on cursor position --
        // conservatively treated the same as a full erase.
        let mut t = MarginTracker::new(24);
        let event = t.scan(b"\x1b[J");
        assert!(event.erase);
        let mut t2 = MarginTracker::new(24);
        let event2 = t2.scan(b"\x1b[0J");
        assert!(event2.erase);
    }

    #[test]
    fn margin_tracker_erase_under_active_sub_range_still_reports() {
        // ED ignores DECSTBM margins per spec, so even a scoped scroll
        // region shouldn't suppress the erase trigger.
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;20r");
        let event = t.scan(b"\x1b[2J");
        assert!(event.erase);
        // The sub-range itself must be unaffected by the erase.
        assert_eq!(t.margins(), Some((3, 20)));
    }

    #[test]
    fn margin_tracker_selective_erase_with_private_marker_still_reports() {
        // DECSED (`CSI ? Ps J`) carries a private marker that disqualifies
        // it as DECSTBM, but it must still trigger the erase heuristic.
        let mut t = MarginTracker::new(24);
        let event = t.scan(b"\x1b[?2J");
        assert!(event.erase);
    }

    #[test]
    fn margin_tracker_erase_split_at_every_byte_boundary() {
        let seq = b"\x1b[2J";
        for split in 0..=seq.len() {
            let mut t = MarginTracker::new(24);
            let event1 = t.scan(&seq[..split]);
            let event2 = t.scan(&seq[split..]);
            assert!(
                event1.erase || event2.erase,
                "split at {split} lost the erase trigger"
            );
        }
    }

    /// Resize clamping must match `vt100::Screen::set_size` exactly (see
    /// `MarginTracker::set_rows`) -- these three cases were measured against
    /// vt100 0.16.2 behaviourally, by line-feeding at the region bottom after
    /// a resize and observing which rows scrolled.
    #[test]
    fn margin_tracker_resize_clamps_like_vt100_instead_of_resetting() {
        // Region still fits: kept as-is. This is the case every attach hits,
        // since reserving the status-bar row shrinks the PTY by one row.
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[5;15r");
        t.set_rows(23);
        assert_eq!(
            t.margins(),
            Some((5, 15)),
            "a region that still fits must survive a resize"
        );

        // Bottom past the new end: clamped to it, top preserved.
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[5;23r");
        t.set_rows(20);
        assert_eq!(t.margins(), Some((5, 20)));

        // Top no longer fits: degenerates to full-screen.
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[21;23r");
        t.set_rows(10);
        assert_eq!(t.margins(), None);

        // Clamping that happens to produce the whole screen normalizes to
        // `None`, the same representation `finish_csi` uses.
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[1;20r");
        t.set_rows(20);
        assert_eq!(t.margins(), None);

        // Growing the terminal leaves a sub-range alone.
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;20r");
        t.set_rows(30);
        assert_eq!(t.margins(), Some((3, 20)));
    }

    /// Reads a `vt100::Screen`'s *real* scroll region back out of the crate,
    /// without access to its private `scroll_top`/`scroll_bottom`: DECOM
    /// (origin mode, `CSI ? 6 h`) makes CUP row-relative to the scroll region
    /// *and* clamps to it (vt100 0.16.2 `grid.rs::set_pos` ->
    /// `row_clamp_top`/`row_clamp_bottom`), so homing reports the top and
    /// asking for row 999 reports the bottom.
    ///
    /// Returns 1-based inclusive `(top, bottom)` -- the same convention
    /// `MarginTracker::margins()` uses, with full-screen spelled `(1, rows)`
    /// rather than `None`. Destructive to cursor position and origin mode, so
    /// only call it on a parser nothing else will assert on afterwards.
    fn probe_vt100_scroll_region(parser: &mut vt100::Parser) -> (u16, u16) {
        parser.process(b"\x1b[?6h\x1b[1;1H");
        let top = parser.screen().cursor_position().0 + 1;
        parser.process(b"\x1b[999;1H");
        let bottom = parser.screen().cursor_position().0 + 1;
        parser.process(b"\x1b[?6l");
        (top, bottom)
    }

    /// The probe above, validated against regions vt100 is known to hold, so
    /// a silently-broken probe can't quietly make the differential test below
    /// vacuous (its whole job is to be the source of truth).
    #[test]
    fn vt100_scroll_region_probe_reads_back_what_decstbm_set() {
        let mut p = vt100::Parser::new(24, 80, 0);
        assert_eq!(
            probe_vt100_scroll_region(&mut p),
            (1, 24),
            "a fresh parser must probe as full-screen"
        );

        let mut p = vt100::Parser::new(24, 80, 0);
        p.process(b"\x1b[5;15r");
        assert_eq!(probe_vt100_scroll_region(&mut p), (5, 15));

        let mut p = vt100::Parser::new(24, 80, 0);
        p.process(b"\x1b[5;15r\x1b[r");
        assert_eq!(
            probe_vt100_scroll_region(&mut p),
            (1, 24),
            "a bare CSI r must probe as full-screen again"
        );
    }

    /// Differential test: `MarginTracker::set_rows` versus what the real
    /// `vt100::Screen::set_size` actually does to the grid's scroll region
    /// (0.16.2 `grid.rs::set_size`, lines 66-99), read back with the probe
    /// above rather than assumed.
    ///
    /// vt100's rule set, in its own order:
    ///
    /// 1. `if scroll_bottom == old_rows - 1 { scroll_bottom = new_rows - 1 }`
    ///    -- a *bottom-anchored* region follows the screen, in both
    ///    directions. This is the rule the tracker was missing: two fixed
    ///    header rows plus "everything below scrolls" (`CSI 3;23r` at 23
    ///    rows) is the most ordinary TUI layout there is, and any terminal
    ///    enlargement -- window maximize, on-screen keyboard hiding, a pane
    ///    unsplit -- made the tracker and the grid disagree (vt100
    ///    `(3, 39)`, tracker `(3, 23)`).
    /// 2. `if scroll_bottom >= new_rows { scroll_bottom = new_rows - 1 }` --
    ///    the shrink clamp.
    /// 3. `if scroll_bottom < scroll_top { scroll_top = 0 }` -- a region
    ///    whose top no longer fits below the clamped bottom degenerates to
    ///    the full screen.
    #[test]
    fn margin_tracker_resize_matches_real_vt100_set_size() {
        // (rows before, DECSTBM, rows after)
        let cases: &[(u16, (u16, u16), u16)] = &[
            // Shrink by one row -- the resize every attach performs to
            // reserve the status-bar row.
            (24, (5, 15), 23),
            // Bottom past the new end: clamped, top preserved.
            (24, (5, 23), 20),
            // Top no longer fits below the clamped bottom: full screen.
            (24, (21, 23), 10),
            // Clamping that lands on the whole screen.
            (24, (1, 20), 20),
            // Growth, region not bottom-anchored: untouched.
            (24, (3, 20), 30),
            // Growth, region bottom-anchored: follows the screen. This is
            // the case that used to mismatch.
            (23, (3, 23), 39),
            (24, (2, 24), 40),
            // Bottom-anchored *and* shrinking: rule 1 then rule 2 agree.
            (24, (5, 24), 12),
            // Bottom-anchored growth by a single row (the inverse of the
            // attach reserve).
            (23, (5, 23), 24),
            // Full-screen tracker state (`None`) across both directions.
            (24, (1, 24), 40),
            (24, (1, 24), 10),
        ];
        for &(before, (top, bottom), after) in cases {
            let decstbm = format!("\x1b[{top};{bottom}r");

            let mut real = vt100::Parser::new(before, 80, 0);
            real.process(decstbm.as_bytes());
            real.screen_mut().set_size(after, 80);
            let expected = probe_vt100_scroll_region(&mut real);

            let mut tracker = MarginTracker::new(before);
            tracker.scan(decstbm.as_bytes());
            tracker.set_rows(after);
            let actual = tracker.margins().unwrap_or((1, after));

            assert_eq!(
                actual, expected,
                "DECSTBM {top};{bottom} @ {before} rows -> {after} rows: \
                 vt100={expected:?} tracker={actual:?}"
            );
        }
    }

    /// Exhaustive sweep of every DECSTBM sub-range at every screen height
    /// from 2 to 24 rows, resized to every height from 1 to 30, against the
    /// real crate -- so `set_rows`'s claim to follow `vt100::Screen::set_size`
    /// is pinned by measurement rather than by reading its source once.
    ///
    /// Two claims, at the two different levels:
    ///
    /// - the *tracked* region (`tracked_region`) matches vt100 case-for-case
    ///   with **no** exemption at all, degenerate cases included -- that is
    ///   the state carried into the next resize, and losing any of it is what
    ///   used to be sticky (see `set_rows`);
    /// - the *reported* region (`margins()`) matches too, except while the
    ///   region is degenerate, where it reports full-screen instead because a
    ///   one-row region cannot be emitted as a DECSTBM. The test asserts that
    ///   shape specifically, and that it is reached at all, so the exemption
    ///   can't silently start absorbing real divergences.
    #[test]
    fn margin_tracker_resize_divergence_from_vt100_is_only_the_degenerate_row() {
        let mut compared = 0usize;
        let mut degenerate = 0usize;
        for before in 2u16..=24 {
            for top in 1u16..before {
                for bottom in (top + 1)..=before {
                    let decstbm = format!("\x1b[{top};{bottom}r");
                    for after in 1u16..=30 {
                        let mut real = vt100::Parser::new(before, 4, 0);
                        real.process(decstbm.as_bytes());
                        real.screen_mut().set_size(after, 4);
                        let expected = probe_vt100_scroll_region(&mut real);

                        let mut tracker = MarginTracker::new(before);
                        tracker.scan(decstbm.as_bytes());
                        tracker.set_rows(after);
                        let tracked = tracker.tracked_region().unwrap_or((1, after));
                        let actual = tracker.margins().unwrap_or((1, after));

                        compared += 1;
                        assert_eq!(
                            tracked, expected,
                            "the tracked region must match vt100 exactly: DECSTBM \
                             {top};{bottom} @ {before} rows -> {after} rows: \
                             vt100={expected:?} tracker={tracked:?}"
                        );
                        if actual == expected {
                            continue;
                        }
                        assert_eq!(
                            expected.0, expected.1,
                            "undocumented divergence: DECSTBM {top};{bottom} @ {before} rows \
                             -> {after} rows: vt100={expected:?} tracker={actual:?}"
                        );
                        assert_eq!(
                            actual,
                            (1, after),
                            "the degenerate case must report full-screen: DECSTBM {top};{bottom} \
                             @ {before} rows -> {after} rows"
                        );
                        degenerate += 1;
                    }
                }
            }
        }
        assert!(compared > 10_000, "sweep covered only {compared} cases");
        assert!(
            degenerate > 0,
            "the degenerate one-row case was never reached -- the exemption above is \
             now unfalsifiable and should be removed"
        );
    }

    /// The sticky-collapse regression, as a single readable case, at the level
    /// the rest of the system actually consumes (`margins()`), differentially
    /// against the real crate.
    ///
    /// A shrink past the region's top collapses it onto one row; growing the
    /// terminal again has to bring the region back, because that collapsed
    /// region is bottom-anchored and vt100's rule 1 grows it with the screen.
    /// Before the fix the tracker discarded the collapsed region entirely and
    /// reported full-screen from then on, at every later size, while the grid
    /// held `(8,24)`. Terminals get resized more than once -- every attach
    /// reserves the status-bar row, on-screen keyboards come and go -- so
    /// "shrunk, then grown" is an ordinary sequence, not a contrived one.
    #[test]
    fn margin_tracker_regrows_a_region_a_shrink_collapsed_onto_one_row() {
        let mut real = vt100::Parser::new(20, 80, 0);
        real.process(b"\x1b[8;9r");
        real.screen_mut().set_size(8, 80);
        real.screen_mut().set_size(24, 80);
        assert_eq!(
            probe_vt100_scroll_region(&mut real),
            (8, 24),
            "the real crate is expected to hold a grown region here"
        );

        let mut tracker = MarginTracker::new(20);
        tracker.scan(b"\x1b[8;9r");
        assert_eq!(tracker.margins(), Some((8, 9)));

        tracker.set_rows(8);
        assert_eq!(
            tracker.margins(),
            None,
            "a region collapsed onto one row is not emittable, so nothing is reported"
        );
        assert_eq!(
            tracker.tracked_region(),
            Some((8, 8)),
            "...but it must still be tracked, or the growth below cannot recover it"
        );

        tracker.set_rows(24);
        assert_eq!(
            tracker.margins(),
            Some((8, 24)),
            "the collapsed region is bottom-anchored, so growing the terminal must grow it \
             back with the screen -- reporting full-screen here is the sticky bug"
        );
    }

    /// The same sweep across **two** consecutive resizes.
    ///
    /// A single-step sweep cannot see a sticky error: it always starts from a
    /// fresh tracker whose state is a real DECSTBM, so a step that throws
    /// state away looks identical to one that keeps it. Resizing twice is
    /// what makes the difference observable -- and a terminal gets resized
    /// more than once (every attach reserves a row, every window change,
    /// every on-screen keyboard).
    ///
    /// The invariant asserted is the strong one: after two arbitrary resizes
    /// the *tracked* region equals what the real `vt100` grid holds,
    /// case-for-case, with no exemptions. `margins()` may still report
    /// full-screen for a region that is currently degenerate -- see its doc
    /// comment, that is an emission-time decision -- but the tracker must not
    /// have *forgotten* anything, or the next resize compounds the loss.
    #[test]
    fn margin_tracker_tracked_region_matches_vt100_across_two_resizes() {
        let mut compared = 0usize;
        let mut via_degenerate = 0usize;
        for before in 2u16..=14 {
            for top in 1u16..before {
                for bottom in (top + 1)..=before {
                    let decstbm = format!("\x1b[{top};{bottom}r");
                    for mid in 1u16..=18 {
                        for after in 1u16..=18 {
                            let mut real = vt100::Parser::new(before, 4, 0);
                            real.process(decstbm.as_bytes());
                            real.screen_mut().set_size(mid, 4);
                            real.screen_mut().set_size(after, 4);
                            let expected = probe_vt100_scroll_region(&mut real);

                            let mut tracker = MarginTracker::new(before);
                            tracker.scan(decstbm.as_bytes());
                            tracker.set_rows(mid);
                            let midpoint = tracker.tracked_region();
                            tracker.set_rows(after);
                            let actual = tracker.tracked_region().unwrap_or((1, after));

                            compared += 1;
                            if midpoint.is_some_and(|(t, b)| t == b) {
                                via_degenerate += 1;
                            }
                            assert_eq!(
                                actual, expected,
                                "DECSTBM {top};{bottom} @ {before} rows -> {mid} rows -> \
                                 {after} rows: vt100={expected:?} tracker={actual:?}"
                            );
                        }
                    }
                }
            }
        }
        assert!(compared > 100_000, "sweep covered only {compared} cases");
        assert!(
            via_degenerate > 0,
            "no two-step path went through a degenerate intermediate region -- the case this \
             sweep exists for was never reached"
        );
    }

    /// `reset` must actually clear, unlike `set_rows` -- the client's session
    /// switch relies on it, and using `set_rows` there silently leaked the
    /// previous session's scroll region onto the next one.
    #[test]
    fn margin_tracker_reset_clears_where_set_rows_only_clamps() {
        let mut t = MarginTracker::new(23);
        t.scan(b"\x1b[5;15r");
        t.set_rows(23);
        assert_eq!(
            t.margins(),
            Some((5, 15)),
            "set_rows must not clear a region that still fits"
        );
        t.reset();
        assert_eq!(t.margins(), None, "reset must clear the region");

        // A half-parsed sequence must not survive a reset either, or it would
        // complete against the new session's byte stream.
        let mut t = MarginTracker::new(23);
        t.scan(b"\x1b[5;");
        t.reset();
        t.scan(b"15r");
        assert_eq!(
            t.margins(),
            None,
            "a half-parsed CSI must not survive a reset"
        );
    }

    #[test]
    fn margin_tracker_resize_keeps_a_split_sequence_parsing() {
        // A DECSTBM split across two PTY reads with a resize landing between
        // them is still a DECSTBM.
        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[3;");
        t.set_rows(23);
        t.scan(b"20r");
        assert_eq!(t.margins(), Some((3, 20)));
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
        assert!(!change.erase_reset);
        assert!(!change.alt_screen);
    }

    #[test]
    fn erase_in_display_emits_layout_change() {
        // Regression test: `CSI 2J` used to be invisible to the layout-change
        // detector, so a full-screen erase (which ignores DECSTBM margins
        // and can wipe the client's reserved bottom row) would not trigger
        // the same-round-trip self-heal that margin-reset/alt-screen flips
        // get -- the client would have to wait on the slower idle/
        // max-interval timers instead.
        let mut tracker = ScreenTracker::new(24, 80);
        let change = tracker
            .process(b"\x1b[2J")
            .expect("full erase should emit a layout change");
        assert!(change.erase_reset);
        assert!(!change.margins_reset);
    }

    #[test]
    fn erase_in_display_under_sub_range_still_emits_layout_change() {
        // Even with an active (and otherwise untouched) DECSTBM sub-range,
        // ED must still trigger -- it ignores scroll margins per spec.
        let mut tracker = ScreenTracker::new(24, 80);
        assert!(tracker.process(b"\x1b[3;20r").is_none());
        let change = tracker
            .process(b"\x1b[2J")
            .expect("erase under a sub-range should still emit a layout change");
        assert!(change.erase_reset);
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

    /// Regression test for the scroll region being silently dropped from the
    /// snapshot after a resize.
    ///
    /// `contents()` alone cannot catch this: immediately after the resize the
    /// live screen and a snapshot-restored one look identical, and only
    /// *diverge later*, once the workload line-feeds at the bottom of the
    /// region it still believes in. So the oracle here is behavioural --
    /// restore a snapshot into a fresh parser, then feed the *same subsequent
    /// bytes* to both and require they still agree. With the region lost, the
    /// restored screen scrolls the wrong rows and overwrites the ones below.
    ///
    /// This is the exact end-to-end failure it stands in for: every `a attach`
    /// resizes the PTY by one row to reserve the status-bar row, so a workload
    /// holding `\x1b[5;15r` lost it on every single attach, and the host
    /// terminal then rendered its scrolling text over the fixed rows beneath
    /// the region.
    #[test]
    fn round_trip_preserves_scroll_region_across_resize() {
        let mut a = ScreenTracker::new(24, 80);
        for row in 1..=23 {
            a.process(format!("\x1b[{row};1HROW-{row:02}").as_bytes());
        }
        a.process(b"\x1b[5;15r");
        // The resize every attach performs: reserve one row for the bar.
        a.set_size(23, 80);
        assert_eq!(
            a.margins.margins(),
            Some((5, 15)),
            "the tracker must still hold the region the vt100 grid still holds"
        );

        let mut b = vt100::Parser::new(23, 80, 0);
        b.process(&a.snapshot());

        // Now make the workload scroll inside its region, exactly as a
        // margin-using TUI does: park at the region's bottom row and feed.
        for i in 1..=4 {
            let bytes = format!("\x1b[15;1H\nSCROLLED-{i}");
            a.process(bytes.as_bytes());
            b.process(bytes.as_bytes());
        }

        assert_eq!(
            a.contents(),
            b.screen().contents(),
            "a snapshot-restored screen must scroll the same rows as the live one"
        );
        // And specifically: the row just below the region must be untouched.
        assert!(
            b.screen().contents().contains("ROW-16"),
            "the row below the scroll region was overwritten -- the region was lost:\n{}",
            b.screen().contents()
        );
    }

    /// The growth-direction half of
    /// `round_trip_preserves_scroll_region_across_resize`, and the
    /// behavioural regression test for `set_rows`'s rule 1 (a bottom-anchored
    /// region follows the screen).
    ///
    /// The layout is the most ordinary one a TUI has: two fixed header rows,
    /// everything below them scrolls (`\x1b[3;23r` on a 23-row workload
    /// screen). The terminal then *grows* -- window maximized, on-screen
    /// keyboard hidden, a pane unsplit -- and the workload's region has to
    /// grow with it, because that is what the `vt100` grid beside this
    /// tracker does.
    ///
    /// Same behavioural oracle as the shrink case: a snapshot restored into a
    /// fresh parser must scroll the *same rows* as the live screen when both
    /// are fed the same subsequent bytes. With the region frozen at its
    /// pre-growth bottom, the restored screen stops scrolling at row 23 and
    /// leaves rows 24-39 holding stale text while the live screen has moved
    /// on.
    #[test]
    fn round_trip_preserves_a_bottom_anchored_region_when_the_screen_grows() {
        let mut a = ScreenTracker::new(23, 80);
        for row in 1..=23 {
            a.process(format!("\x1b[{row};1HROW-{row:02}").as_bytes());
        }
        // Two fixed header rows; rows 3..23 scroll.
        a.process(b"\x1b[3;23r");
        // The terminal grows.
        a.set_size(39, 80);
        assert_eq!(
            a.margins.margins(),
            Some((3, 39)),
            "a bottom-anchored region must follow the screen when it grows, \
             the way the vt100 grid beside it does"
        );

        let mut b = vt100::Parser::new(39, 80, 0);
        b.process(&a.snapshot());

        // Scroll at the *new* region bottom, which is where the two models
        // disagree if the region did not grow.
        for i in 1..=4 {
            let bytes = format!("\x1b[39;1H\nGROWN-{i}");
            a.process(bytes.as_bytes());
            b.process(bytes.as_bytes());
        }

        assert_eq!(
            a.contents(),
            b.screen().contents(),
            "a snapshot-restored screen must scroll the same rows as the live one"
        );
        // The fixed header rows are above the region and must be untouched by
        // that scrolling; row 3 (the region's top) must have scrolled away.
        let restored = b.screen().contents();
        assert!(
            restored.contains("ROW-01") && restored.contains("ROW-02"),
            "the fixed header rows above the region were scrolled away:\n{restored}"
        );
        assert!(
            !restored.contains("ROW-03"),
            "the region's own top row did not scroll -- the region was not in force:\n{restored}"
        );
    }

    #[test]
    fn contents_plain_text_matches_screen() {
        let mut tracker = ScreenTracker::new(24, 80);
        tracker.process(b"hello there\r\n");
        assert!(tracker.contents().contains("hello there"));
    }

    // Regression test for the tmux scrollback-garbling bug class (see the
    // module doc and docs/scrollback-design.md sections 2-3): that bug comes
    // from re-flowing soft-wrapped lines across a resize of a grid that
    // *retains* history. `ScreenTracker` has zero retained scrollback (the
    // `0` in `Parser::new`, see the invariant comment on `ScreenTracker::new`)
    // and this vt100 version's resize truncates/pads rows in place rather
    // than rejoining/re-wrapping them, so a soft-wrapped line surviving a
    // resize must come out as a clean per-row truncation, never transposed
    // or shredded into one character per row.
    #[test]
    fn resize_does_not_transpose_soft_wrapped_line() {
        let cols = 20u16;
        let mut tracker = ScreenTracker::new(24, cols);
        // A single logical line long enough to soft-wrap across 4 rows at
        // 20 columns: distinct word-ish chunks so we can check relative
        // order survives the resize.
        let line = "AAAAAAAAAA-BBBBBBBBBB-CCCCCCCCCC-DDDDDDDDDD-EOL";
        tracker.process(line.as_bytes());

        // Resize to a different width -- this is where an implementation
        // that reflowed a retained history grid could transpose/shred text.
        tracker.set_size(24, 40);
        tracker.process(b"");

        let contents = tracker.contents();

        // Clean per-row truncation/pad (as opposed to reflow) preserves
        // every character's relative order, but a row boundary can still
        // fall mid-chunk (the same way it could before the resize) -- that's
        // wrapping, not corruption. So reconstruct the logical stream by
        // trimming each row's trailing pad and concatenating rows with no
        // separator; a transposition/shredding bug would scramble character
        // order within this reconstruction, whereas clean truncation/pad
        // reproduces the original line exactly.
        let dewrapped: String = contents.lines().map(|l| l.trim_end()).collect();
        assert!(
            dewrapped.contains(line),
            "resized screen does not reconstruct the original line in order -- \
             possible transposition/shredding:\ngot: {dewrapped:?}\nwant substring: {line:?}"
        );

        // The pathological one-char-per-row shredding pattern: every
        // non-empty row reduced to a single character. Detect it generically
        // by checking that at least one row still holds more than one
        // contiguous run of our text, rather than every populated row being
        // exactly one character wide.
        let shredded = contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .all(|l| l.trim().chars().count() <= 1);
        assert!(
            !shredded,
            "screen contents look shredded to one character per row:\n{contents}"
        );
    }
}
