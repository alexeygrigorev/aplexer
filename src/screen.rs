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

/// Ceiling on the *scrollback* cells a client-side model may retain, on top
/// of the visible grid `MAX_SCREEN_CELLS` bounds.
///
/// Scrollback is the one part of the model whose size the user configures
/// (`APLEXER_HISTORY_LIMIT`, tmux's `history-limit`), so it needs its own
/// bound rather than borrowing the screen's: a `vt100` cell is 32 bytes, so
/// a naive "100000 lines" on a 200-column terminal would ask for 640 MB.
/// One million cells is 32 MB at that cell size -- generous next to the
/// 2000-line default (12.8 MB at 200 columns, 5 MB at 80) and still a hard
/// ceiling a typo cannot blow past.
pub const MAX_SCROLLBACK_CELLS: usize = 1024 * 1024;

/// Default retained scrollback, in lines. Deliberately tmux's own
/// `history-limit` default, because that is the number the user is
/// measuring aplexer against.
pub const DEFAULT_SCROLLBACK_LINES: usize = 2000;

/// The scrollback line count actually usable at `cols` columns: the request,
/// clamped so `lines * cols` stays inside `MAX_SCROLLBACK_CELLS`. Returns 0
/// for a request of 0 (the worker's model, which retains no history -- see
/// `ScreenTracker::try_new`).
pub fn scrollback_lines_for(cols: u16, lines: usize) -> usize {
    if lines == 0 {
        return 0;
    }
    let cols = usize::from(cols.max(1));
    lines.min(MAX_SCROLLBACK_CELLS / cols).max(1)
}

/// Conventional geometry used when a PTY exists but its kernel winsize has
/// not been initialized. Linux reports that state as `0x0`; feeding the
/// zeros (or a `1x1` clamp) to vt100 leaves several parser operations with a
/// degenerate grid that real terminals never use.
pub const DEFAULT_TERMINAL_ROWS: u16 = 24;
pub const DEFAULT_TERMINAL_COLS: u16 = 80;

/// Normalize the protocol's zero dimensions and reject grids that would
/// exceed the worker's fixed cell budget. `checked_mul` keeps this correct if
/// the dimension types are widened in the future.
pub fn validate_size(rows: u16, cols: u16) -> Result<(u16, u16)> {
    let rows = if rows == 0 {
        DEFAULT_TERMINAL_ROWS
    } else {
        rows
    };
    let cols = if cols == 0 {
        DEFAULT_TERMINAL_COLS
    } else {
        cols
    };
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
    /// Whether a DECSTBM sub-range has been in force at any point since this
    /// tracker last became authoritative (construction, or the reset that
    /// `ClientScreen::seed_history` does). Sticky by design: `vt100` drops a
    /// row scrolled out of *any* sub-range (`grid.rs::scroll_up`), so one
    /// sub-range anywhere in the model's lifetime means the live scrollback
    /// may silently be missing rows, even after the workload resets its
    /// margins. The attach client reads this to decide whether the pager's
    /// history is worth rebuilding from the worker's raw tail before opening
    /// (`refresh_pager_history` in `src/bin/a.rs`); a workload that never
    /// sends a sub-range retains every row through the ordinary live path and
    /// pays for nothing.
    subregion_seen: bool,
}

impl MarginTracker {
    pub fn new(rows: u16) -> Self {
        Self {
            rows: rows.max(1),
            state: MarginParseState::Ground,
            param_buf: Vec::new(),
            disqualified: false,
            region: None,
            subregion_seen: false,
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

    /// Whether a DECSTBM sub-range has been seen at all since the last
    /// `reset` -- see the field's doc comment for why this is the "the live
    /// scrollback may be missing rows" signal rather than "a sub-range is in
    /// force right now". A full-range or bare `CSI r`, RIS, and every
    /// malformed sequence leave it alone: they say the workload stopped (or
    /// never started) using a region, not that the rows scrolled out while it
    /// did are back.
    pub fn subregion_seen(&self) -> bool {
        self.subregion_seen
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
        self.subregion_seen = false;
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
            self.subregion_seen = true;
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
    /// Whether the stream this tracker has consumed is currently between
    /// escape sequences. `try_set_size` consults it before injecting the
    /// synthetic scroll-up a shrink is compensated with: glued into a
    /// half-received sequence it would corrupt the parse instead of fixing
    /// the resize.
    boundary: StreamBoundary,
    /// Last-observed `alternate_screen()` value, for flip detection.
    alt_screen: bool,
    /// Retained scrollback lines this tracker was built with, so a rebuild
    /// (`ClientScreen::reset` on a session switch) keeps the same depth.
    /// `0` for the worker's own model -- see `try_new`.
    scrollback: usize,
}

impl ScreenTracker {
    /// The **worker's** tracker: `Parser::new(rows, cols, 0)`, no retained
    /// scrollback. The worker parses every byte of every session whether or
    /// not anyone is attached, so its per-session cost has to stay at the
    /// two-grid minimum (design doc section 5.2); the raw history file is
    /// what carries a session's past there.
    ///
    /// The *client's* model is built with
    /// [`try_new_with_scrollback`](Self::try_new_with_scrollback) instead,
    /// which is what `Ctrl-b [` pages through.
    pub fn try_new(rows: u16, cols: u16) -> Result<Self> {
        Self::try_new_with_scrollback(rows, cols, 0)
    }

    /// A tracker that retains `scrollback` lines of history above the
    /// visible grid -- aplexer's equivalent of a tmux pane's history, and
    /// the thing `Ctrl-b [` pages through with `Screen::set_scrollback`.
    ///
    /// The emulator that is *already* parsing every byte accumulates the
    /// history as a side effect, exactly the way tmux's per-pane virtual
    /// terminal does; nothing re-parses a raw byte log to reconstruct the
    /// past, so the pager can never disagree with the live screen.
    ///
    /// Two properties inherited from `vt100` and deliberately kept, because
    /// they are also tmux's:
    ///
    /// - Lines enter the scrollback only when the *primary* grid scrolls
    ///   with no DECSTBM sub-range in force. A full-screen alternate-screen
    ///   application (vim, htop, a TUI agent) therefore has no history while
    ///   it owns the screen, which is what tmux's copy-mode shows too.
    /// - History is **not** reflowed on resize. Retained rows keep the width
    ///   they were written at; a later resize changes the visible grid only.
    ///   Reflowing retained history is the class of bug that garbles tmux
    ///   scrollback (docs/scrollback-design.md sections 2-3), so it is not
    ///   attempted here either.
    ///
    /// `scrollback` is taken as given; callers clamp it with
    /// `scrollback_lines_for` so `lines * cols` respects
    /// `MAX_SCROLLBACK_CELLS`.
    pub fn try_new_with_scrollback(rows: u16, cols: u16, scrollback: usize) -> Result<Self> {
        let (rows, cols) = validate_size(rows, cols)?;
        Ok(Self {
            parser: vt100::Parser::new(rows, cols, scrollback),
            margins: MarginTracker::new(rows),
            boundary: StreamBoundary::new(),
            alt_screen: false,
            scrollback,
        })
    }

    /// How many lines of history this tracker retains at most.
    pub fn scrollback_capacity(&self) -> usize {
        self.scrollback
    }

    /// How many lines of history are available above the *active* grid right
    /// now, and therefore how far `scrolled_frame` can go back.
    ///
    /// `vt100` exposes the retained-row count only through the clamp inside
    /// `set_scrollback`, so this asks for an impossible offset, reads back
    /// what it was clamped to, and restores the offset to 0. The tracker's
    /// resting offset is always 0 (see `scrolled_frame`), which is what
    /// keeps `cursor_restore`/`snapshot`/`relay` describing the live screen
    /// no matter what the pager is showing.
    pub fn scrollback_available(&mut self) -> usize {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let available = screen.scrollback();
        screen.set_scrollback(0);
        available
    }

    /// Render the screen as it looks `offset` lines back in the history, as
    /// escape codes suitable for painting a host terminal, and return
    /// `(bytes, clamped offset, lines available)`.
    ///
    /// The offset is applied, read back (`vt100` clamps it), rendered, and
    /// **reset to 0** before returning: the model's steady state is always
    /// the live screen, so a scrolled-back pager never changes what
    /// `snapshot`, `cursor_restore` or `ClientScreen::relay` see.
    pub fn scrolled_frame(&mut self, offset: usize) -> (Vec<u8>, usize, usize) {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let available = screen.scrollback();
        screen.set_scrollback(offset);
        let clamped = screen.scrollback();
        let bytes = screen.contents_formatted();
        screen.set_scrollback(0);
        (bytes, clamped, available)
    }

    /// The mouse reporting the *workload* has asked the terminal for, as a
    /// self-contained assertion: every protocol and encoding this client
    /// knows how to turn on is turned off first, then whatever the workload
    /// actually wants is turned back on.
    ///
    /// Written down as an absolute rather than a diff because it is used to
    /// hand the mouse *back*: the attach client turns SGR mouse reporting on
    /// for itself while the workload wants none (that is the only way a
    /// wheel event can reach `a` at all -- see `sync_client_mouse` in
    /// `src/bin/a.rs`), and the moment the workload asks for the mouse the
    /// client must undo exactly its own modes and leave the workload's in
    /// force, in one write that cannot half-apply.
    pub fn workload_mouse_sequence(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut out: Vec<u8> =
            b"\x1b[?9l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l".to_vec();
        out.extend_from_slice(match screen.mouse_protocol_mode() {
            vt100::MouseProtocolMode::None => b"".as_slice(),
            vt100::MouseProtocolMode::Press => b"\x1b[?9h".as_slice(),
            vt100::MouseProtocolMode::PressRelease => b"\x1b[?1000h".as_slice(),
            vt100::MouseProtocolMode::ButtonMotion => b"\x1b[?1002h".as_slice(),
            vt100::MouseProtocolMode::AnyMotion => b"\x1b[?1003h".as_slice(),
        });
        out.extend_from_slice(match screen.mouse_protocol_encoding() {
            vt100::MouseProtocolEncoding::Default => b"".as_slice(),
            vt100::MouseProtocolEncoding::Utf8 => b"\x1b[?1005h".as_slice(),
            vt100::MouseProtocolEncoding::Sgr => b"\x1b[?1006h".as_slice(),
        });
        out
    }

    /// True while the workload has asked for mouse reporting of its own. The
    /// wheel belongs to it then, exactly as a tmux pane's application owns
    /// the mouse when it requested it.
    pub fn workload_wants_mouse(&self) -> bool {
        self.parser.screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
    }

    /// True while the alternate screen is the active grid. `vt100` gives the
    /// alternate grid no scrollback (nor does any real terminal), so this is
    /// also "there is no history to page through right now".
    pub fn alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    #[cfg(test)]
    pub fn new(rows: u16, cols: u16) -> Self {
        Self::try_new(rows, cols).expect("test terminal size should be valid")
    }

    /// Feed PTY bytes; returns `Some(LayoutChange)` when the workload did
    /// something the attached client must react to.
    pub fn process(&mut self, data: &[u8]) -> Option<LayoutChange> {
        self.boundary.feed(data);
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

    /// Replay bytes for their *history* only -- `ClientScreen::seed_history`'s
    /// inner loop, and the one caller that is allowed to skip the margin
    /// scan `process` does.
    ///
    /// Skipping it is not an optimization looking for a justification: the
    /// seed strips every DECSTBM out of the tail before this sees it
    /// (`without_scroll_regions`) and resets the tracker immediately
    /// afterwards, so the scan is guaranteed to find nothing and its result
    /// is guaranteed to be discarded. It is a second full pass over multiple
    /// megabytes on the attach path, and the attach path is where it is least
    /// affordable. `alt_screen` is still tracked, because the seed's epilogue
    /// relies on it being current.
    pub fn seed(&mut self, data: &[u8]) {
        self.boundary.feed(data);
        self.parser.process(data);
        self.alt_screen = self.parser.screen().alternate_screen();
    }

    /// Resizes the parser's grid (content-preserving) and re-fits the margin
    /// tracker to the new row count the same way the grid re-fits its own
    /// scroll region -- it is *not* reset to full-screen, correcting design
    /// doc section 5.3's original "margins reset on resize" plan. See
    /// `MarginTracker::set_rows` for the exact rules and why the tracker has
    /// to follow the grid rather than a real terminal here.
    ///
    /// One place the tracker *does* have to follow a real terminal rather
    /// than `vt100`: a shrink keeps the cursor's line on screen. A real
    /// terminal (xterm, and tmux's vt100) scrolls the content up by exactly
    /// the excess when the cursor sits below the new bottom row;
    /// `vt100`'s `Grid::set_size` instead truncates rows from the bottom,
    /// so the newest line falls off and the cursor clamps onto the row
    /// above it -- where the workload's WINCH redraw (a shell repainting
    /// its prompt) then overwrites what is left. Attaching to an idle shell
    /// visibly lost its last output line this way, and every later snapshot
    /// carried the loss. The compensation is a synthetic SU by the excess,
    /// under two gates: no DECSTBM sub-range in force (a region holder owns
    /// its own resize semantics, and `set_size` already follows a
    /// bottom-anchored region), and the stream not mid-escape-sequence
    /// (`boundary`), because an SU glued into a half-received sequence
    /// corrupts the parse it was meant to protect. A skipped compensation
    /// costs nothing permanent: SIGWINCH makes the workload repaint anyway,
    /// and the next resize retries.
    pub fn try_set_size(&mut self, rows: u16, cols: u16) -> Result<()> {
        let (rows, cols) = validate_size(rows, cols)?;
        if rows < self.rows()
            && self.margins.margins().is_none()
            && self.boundary.at_escape_boundary()
        {
            let (cursor_row, _) = self.cursor_position();
            let excess = i32::from(cursor_row) - i32::from(rows) + 1;
            if excess > 0 {
                let su = format!("\x1b[{excess}S").into_bytes();
                self.boundary.feed(&su);
                self.parser.process(&su);
            }
        }
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

    /// The current DECSTBM sub-range, or `None` for full-screen margins --
    /// the same emission-facing view `MarginTracker::margins` documents.
    pub fn margins(&self) -> Option<(u16, u16)> {
        self.margins.margins()
    }

    /// Forget any tracked DECSTBM sub-range and any half-parsed sequence,
    /// without touching the grid. Used after `ClientScreen::seed_history`
    /// replays a raw tail, whose trailing state was never this client's --
    /// which is also why the boundary tracker is reset here: the tail's
    /// mid-sequence ending was the log's, not the live stream's.
    pub fn reset_margins(&mut self) {
        self.margins.reset();
        self.boundary.reset();
    }

    /// Whether a DECSTBM sub-range has been seen since this tracker's margins
    /// were last reset -- `MarginTracker::subregion_seen` delegated, read by
    /// the attach client to gate the pager's history rebuild.
    pub fn subregion_seen(&self) -> bool {
        self.margins.subregion_seen()
    }

    /// Where the *workload* believes its cursor is, 0-based `(row, col)`.
    pub fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    /// The screen's row count, i.e. the workload's own last row index + 1.
    pub fn rows(&self) -> u16 {
        self.parser.screen().size().0
    }

    /// The screen's column count. Needed to recognize `vt100`'s pending-wrap
    /// state, which it reports as a cursor column equal to the width (see
    /// `ClientScreen::wrap_would_walk`).
    pub fn cols(&self) -> u16 {
        self.parser.screen().size().1
    }

    /// Escape sequences that put a host terminal's cursor and drawing
    /// attributes back exactly where this model says the workload left them,
    /// **without touching the shared DECSC/DECRC save-cursor register**.
    ///
    /// This is the replacement for the `\x1b7 ... \x1b8` bracket the status
    /// bar used to wrap its redraw in. A terminal has exactly one save-cursor
    /// register; saving into it from a stream we are only relaying silently
    /// destroys whatever the workload put there, and the workload's own later
    /// `\x1b8` then jumps to *our* saved position (Claude Code's startup
    /// `\x1b7\x1b[r\x1b8` and any `tput sc`/`tput rc` progress line are
    /// exactly this shape). Restoring absolutely from the model instead makes
    /// the register the workload's private property again.
    ///
    /// Composition, in `vt100`'s own prescribed order:
    ///
    /// 1. `cursor_state_formatted()` -- cursor visibility plus an absolute
    ///    reposition that also reproduces the *pending-wrap* state (it
    ///    re-draws the cell at the end of the row when the cursor sits past
    ///    the last column), which a bare `CSI row;col H` cannot express.
    /// 2. `attributes_formatted()` -- the workload's current SGR pen. Step 1
    ///    may itself alter the attributes (that re-drawn cell carries its
    ///    own), and the bar's own `\x1b[0m` has already cleared them, so this
    ///    has to come second. Real terminals restore SGR from DECSC; `vt100`
    ///    (and any emulator that saves only the position) does not, so the
    ///    old bracket left the workload's pen reset to default on exactly the
    ///    terminals aplexer models itself on.
    pub fn cursor_restore(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut out = screen.cursor_state_formatted();
        out.extend_from_slice(&screen.attributes_formatted());
        out
    }
}

/// Where in an escape sequence (or a multi-byte UTF-8 character) a relayed
/// byte stream currently sits, plus how deeply nested it is inside a
/// synchronized-output block.
///
/// This exists for one reason: `a attach` is a raw byte relay, and the
/// status bar interjects its own bytes into that relay. A PTY read boundary
/// lands at an arbitrary byte offset, so "between two chunks" is emphatically
/// **not** "between two escape sequences" -- measured against a real
/// continuously-streaming TUI workload, 5 of 10 status-bar redraws were
/// spliced into the middle of an unterminated `CSI` sequence
/// (`...\x1b[38;5;` + our redraw + `91m...`). The host terminal's parser
/// abandons the partial sequence when our `ESC` arrives, and the workload's
/// remaining parameter bytes are then printed as literal text into its own
/// frame -- which is exactly the reported corruption (stray digit/letter
/// runs welded into rows, everything after them shifted along the row).
/// Splitting a multi-byte UTF-8 character is the same failure with a
/// replacement glyph instead of digits.
///
/// So the client asks this type "is the stream at a boundary where an
/// injection is invisible?" before writing anything of its own. Deliberately
/// a boundary recognizer, not a parser: it never interprets a sequence's
/// meaning, only where one begins and ends.
#[derive(Debug, Clone)]
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
    /// Private marker + parameter bytes of the CSI in flight, capped; only
    /// used to recognize `?2026h`/`?2026l`.
    csi_params: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundaryState {
    Ground,
    /// `ESC` seen (or `ESC` plus intermediate bytes).
    Esc,
    /// `ESC [` seen; consuming parameter/intermediate bytes.
    Csi,
    /// Inside an `OSC`/`DCS`/`SOS`/`PM`/`APC` string.
    Str {
        osc: bool,
    },
    /// `ESC` seen inside such a string (candidate `ST`).
    StrEsc {
        osc: bool,
    },
}

/// Cap on the CSI parameter bytes retained for `?2026` recognition. `?2026`
/// is 5 bytes; anything longer cannot be it.
const BOUNDARY_PARAM_CAP: usize = 8;

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
            csi_params: Vec::new(),
        }
    }

    /// Forget everything, for an in-process session switch: the next
    /// session's bytes are a different stream and cannot continue this one's
    /// half-parsed sequence or synchronized-output block.
    pub fn reset(&mut self) {
        self.state = BoundaryState::Ground;
        self.utf8_remaining = 0;
        self.sync_depth = 0;
        self.csi_params.clear();
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
        for &byte in data {
            self.step(byte);
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
    /// printed. Deliberately a probe over a clone of the state machine rather
    /// than a second, subtly-different recognizer.
    pub fn bytes_to_ground(&self, data: &[u8]) -> usize {
        let mut probe = self.clone();
        for (n, &byte) in data.iter().enumerate() {
            probe.step(byte);
            if probe.at_escape_boundary() {
                return n + 1;
            }
        }
        data.len()
    }

    fn step(&mut self, byte: u8) {
        // CAN/SUB abort whatever is in flight, in every state.
        if byte == 0x18 || byte == 0x1a {
            self.state = BoundaryState::Ground;
            self.utf8_remaining = 0;
            self.csi_params.clear();
            return;
        }
        match self.state {
            BoundaryState::Ground => {
                if self.utf8_remaining > 0 {
                    if (0x80..0xc0).contains(&byte) {
                        self.utf8_remaining -= 1;
                    } else {
                        // Malformed continuation: the host terminal gives up
                        // on the character too, so this byte starts fresh.
                        self.utf8_remaining = 0;
                        self.start_ground(byte);
                    }
                } else {
                    self.start_ground(byte);
                }
            }
            BoundaryState::Esc => match byte {
                0x1b => {}
                b'[' => {
                    self.state = BoundaryState::Csi;
                    self.csi_params.clear();
                }
                b']' => self.state = BoundaryState::Str { osc: true },
                b'P' | b'X' | b'^' | b'_' => self.state = BoundaryState::Str { osc: false },
                // Intermediate bytes keep the escape sequence open.
                0x20..=0x2f => {}
                _ => self.state = BoundaryState::Ground,
            },
            BoundaryState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.finish_csi(byte);
                    self.state = BoundaryState::Ground;
                } else if byte == 0x1b {
                    self.state = BoundaryState::Esc;
                    self.csi_params.clear();
                } else {
                    if self.csi_params.len() < BOUNDARY_PARAM_CAP {
                        self.csi_params.push(byte);
                    } else {
                        // Too long to be `?2026`; keep consuming, stop
                        // recording (and make sure a truncated prefix can
                        // never be mistaken for one).
                        self.csi_params.push(b'x');
                        self.csi_params.remove(0);
                    }
                }
            }
            BoundaryState::Str { osc } => {
                if osc && byte == 0x07 {
                    self.state = BoundaryState::Ground;
                } else if byte == 0x1b {
                    self.state = BoundaryState::StrEsc { osc };
                }
            }
            BoundaryState::StrEsc { osc } => {
                if byte == b'\\' {
                    self.state = BoundaryState::Ground;
                } else {
                    self.state = BoundaryState::Str { osc };
                }
            }
        }
    }

    fn start_ground(&mut self, byte: u8) {
        match byte {
            0x1b => self.state = BoundaryState::Esc,
            0xc2..=0xdf => self.utf8_remaining = 1,
            0xe0..=0xef => self.utf8_remaining = 2,
            0xf0..=0xf4 => self.utf8_remaining = 3,
            _ => {}
        }
    }

    fn finish_csi(&mut self, final_byte: u8) {
        if self.csi_params == b"?2026" {
            match final_byte {
                b'h' => self.sync_depth = self.sync_depth.saturating_add(1),
                b'l' => self.sync_depth = self.sync_depth.saturating_sub(1),
                _ => {}
            }
        }
        self.csi_params.clear();
    }
}

/// Filters alt-screen DECSET/DECRST out of bytes written to the *host*
/// terminal, so `a attach` can keep the host on the alternate screen for the
/// whole client lifetime.
///
/// The pre-attach primary screen (the `a` session list, the user's shell)
/// stays frozen underneath. Host scrollback therefore cannot mix those rows
/// into the live view -- the failure in the screenshot of a scrolled-up
/// attach. The workload's own 1049h/1049l still update `ScreenTracker`; they
/// just must not switch the host, or a TUI exiting alt-screen would reveal
/// the primary list mid-attach. Combined DECSET lists keep every other mode
/// (`CSI ? 1049;2004 h` becomes `CSI ? 2004 h`). Incomplete CSIs are held
/// across `push` calls so a split `\x1b[?1049` / `l` cannot leak a 1049l.
#[derive(Debug, Default)]
struct HostAltHold {
    state: HoldState,
    held: Vec<u8>,
    params: Vec<u8>,
    private: bool,
    intermediate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum HoldState {
    #[default]
    Ground,
    Esc,
    Csi,
}

impl HostAltHold {
    fn new() -> Self {
        Self::default()
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn push(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for &byte in data {
            self.step(byte, &mut out);
        }
        out
    }

    fn step(&mut self, byte: u8, out: &mut Vec<u8>) {
        match self.state {
            HoldState::Ground => {
                if byte == 0x1b {
                    self.state = HoldState::Esc;
                    self.held.clear();
                    self.held.push(byte);
                } else {
                    out.push(byte);
                }
            }
            HoldState::Esc => {
                self.held.push(byte);
                if byte == b'[' {
                    self.state = HoldState::Csi;
                    self.params.clear();
                    self.private = false;
                    self.intermediate = false;
                } else if byte == 0x1b {
                    out.extend_from_slice(&self.held[..self.held.len() - 1]);
                    self.held.clear();
                    self.held.push(0x1b);
                } else {
                    out.extend_from_slice(&self.held);
                    self.held.clear();
                    self.state = HoldState::Ground;
                }
            }
            HoldState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.held.push(byte);
                    if self.private && !self.intermediate && (byte == b'h' || byte == b'l') {
                        out.extend_from_slice(&filter_alt_screen_modes(&self.params, byte));
                    } else {
                        out.extend_from_slice(&self.held);
                    }
                    self.held.clear();
                    self.state = HoldState::Ground;
                } else if byte == 0x1b {
                    out.extend_from_slice(&self.held);
                    self.held.clear();
                    self.held.push(0x1b);
                    self.state = HoldState::Esc;
                } else {
                    self.held.push(byte);
                    if byte == b'?' && self.params.is_empty() && !self.intermediate {
                        self.private = true;
                    } else if (0x20..=0x2f).contains(&byte) {
                        self.intermediate = true;
                    } else if self.private && !self.intermediate {
                        self.params.push(byte);
                    }
                }
            }
        }
    }
}

/// Bytes after `CSI ?` in a DECSET/DECRST. Drops 47/1047/1048/1049 and
/// rebuilds the sequence from whatever remains; empty if nothing remains.
fn filter_alt_screen_modes(params: &[u8], final_byte: u8) -> Vec<u8> {
    let mut kept: Vec<&[u8]> = Vec::new();
    for part in params.split(|b| *b == b';') {
        if part.iter().all(|b| b.is_ascii_digit()) && mode_is_alt_screen(part) {
            continue;
        }
        kept.push(part);
    }
    if kept.is_empty() {
        return Vec::new();
    }
    let mut seq = b"\x1b[?".to_vec();
    for (i, part) in kept.iter().enumerate() {
        if i > 0 {
            seq.push(b';');
        }
        seq.extend_from_slice(part);
    }
    seq.push(final_byte);
    seq
}

fn mode_is_alt_screen(num: &[u8]) -> bool {
    let mut n = 0u32;
    for &d in num {
        n = n.saturating_mul(10).saturating_add(u32::from(d - b'0'));
    }
    matches!(n, 47 | 1047 | 1048 | 1049)
}

/// Strip every DECSTBM (`CSI <params> r`) out of a raw history tail before it
/// is replayed into the client's model.
///
/// **This is what makes `Ctrl-b [` show anything at all in an agent session.**
///
/// The mechanism, from `vt100` 0.16.2 `grid.rs::scroll_up`: a row is pushed
/// into the retained scrollback only `if self.scrollback_len > 0 &&
/// !self.scroll_region_active()`. While a DECSTBM sub-range is in force, rows
/// scrolled out of the top of that region are **dropped**, not retained. That
/// is the right behavior for a live pane -- it is tmux's too -- but the seed
/// replay is not a live pane. It is a one-shot pass whose only product is the
/// history; its final grid is thrown away moments later by the reattach
/// snapshot, which repaints the screen from the worker's own model. So a
/// region has nothing to protect here, and honoring one only discards the
/// transcript the user is trying to scroll back to.
///
/// Measured against 4 MiB of real retained history from thirteen live agent
/// sessions (codex, claude, opencode), replayed at 23x100 with a 2000-line
/// grid -- retained lines, before and after this strip:
///
/// ```text
///   0 ->  225   0 ->  2000    53 ->  594   228 ->  570
/// 1004 -> 2000  1269 -> 2000   728 ->  751  1881 -> 2000
/// ```
///
/// Three of those sessions produced a *completely* empty pager before it. The
/// rows recovered are ordinary transcript text, spot-checked at several
/// depths, not the region's static header and footer: an agent CLI reserves
/// its sub-range for the composer at the bottom and scrolls the transcript
/// through the region above, so the rows leaving that region are exactly the
/// ones worth keeping.
///
/// Borrowed, not copied, when the tail holds no DECSTBM at all -- which is
/// every plain shell session, and the case where the seed is already fine.
///
/// Only an unprefixed `CSI <digits and semicolons> r` is removed. `CSI ? Ps r`
/// is XTRESTORE (restore private modes), a different sequence that must
/// survive, so a parameter list containing anything but digits and `;`
/// disqualifies the match.
fn without_scroll_regions(data: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let mut out: Option<Vec<u8>> = None;
    let mut copied = 0usize;
    let mut i = 0usize;
    while i < data.len() {
        if data[i] != 0x1b || i + 1 >= data.len() || data[i + 1] != b'[' {
            i += 1;
            continue;
        }
        let mut end = i + 2;
        while end < data.len() && matches!(data[end], b'0'..=b'9' | b';') {
            end += 1;
        }
        if end < data.len() && data[end] == b'r' {
            let buf = out.get_or_insert_with(|| Vec::with_capacity(data.len()));
            buf.extend_from_slice(&data[copied..i]);
            copied = end + 1;
            i = end + 1;
        } else {
            // Parameter bytes cannot contain ESC, so resuming at `end` cannot
            // skip past the start of another sequence.
            i = end.max(i + 2);
        }
    }
    match out {
        Some(mut buf) => {
            buf.extend_from_slice(&data[copied..]);
            std::borrow::Cow::Owned(buf)
        }
        None => std::borrow::Cow::Borrowed(data),
    }
}

/// The attached client's own copy of the workload's terminal, kept by feeding
/// it the very bytes the client is relaying to the user's terminal.
///
/// The worker has had a live `vt100` model since docs/terminal-state-design.md
/// shipped, but only the *worker* had one: the client stayed a blind relay
/// that hand-injected `\x1b7 ... \x1b8` brackets into someone else's byte
/// stream and hoped. This gives the client the same model, which is what the
/// status bar needs to be able to
///
/// - inject only at a boundary where an injection is invisible
///   (`at_safe_boundary`),
/// - put the cursor and pen back absolutely rather than through the shared
///   DECSC register (`ScreenTracker::cursor_restore`), and
/// - keep the host's cursor on the row the workload believes it is on --
///   line feeds, wraps and downward cursor moves off the workload's last row
///   under a scroll-region sub-range, and the window a workload's own
///   `\x1b[r` opens over the reserved row (`relay`).
///
/// Sized to the workload's geometry -- the physical terminal minus the
/// reserved status row -- so its coordinates are the host's coordinates for
/// every row the workload can reach.
pub struct ClientScreen {
    screen: ScreenTracker,
    boundary: StreamBoundary,
    /// When set, bytes written to the host have alt-screen DECSET/DECRST
    /// stripped so the attach client can own the host's alternate screen.
    host_alt: Option<HostAltHold>,
}

impl ClientScreen {
    pub fn try_new(rows: u16, cols: u16) -> Result<Self> {
        Self::try_new_with_scrollback(rows, cols, 0)
    }

    /// A client model that retains `scrollback` lines of history above the
    /// visible screen -- what `Ctrl-b [` pages through. See
    /// `ScreenTracker::try_new_with_scrollback` for the semantics inherited
    /// from `vt100` (alt-screen applications have no history; retained rows
    /// are never reflowed on resize).
    pub fn try_new_with_scrollback(rows: u16, cols: u16, scrollback: usize) -> Result<Self> {
        Ok(Self {
            screen: ScreenTracker::try_new_with_scrollback(rows, cols, scrollback)?,
            boundary: StreamBoundary::new(),
            host_alt: None,
        })
    }

    /// Prime the scrollback grid from a tail of the worker's retained raw
    /// history, before the first live byte and before the reattach snapshot.
    ///
    /// The model itself is the history (that is the whole tmux-shaped
    /// design), but a *freshly attached* client's model is empty: it has
    /// been parsing this session for zero seconds. Without this, "attach and
    /// scroll up" -- the exact gesture being fixed -- would show nothing at
    /// all until the workload produced a screenful under the new client.
    /// Replaying the worker's byte log once, into the model only, is what
    /// gives the grid a past to page through; from that point on the live
    /// relay maintains it and nothing re-parses anything.
    ///
    /// Deliberately **not** written to the terminal, and deliberately not
    /// allowed to leave state behind:
    ///
    /// - The tail can begin mid-escape-sequence (it is a byte-count slice of
    ///   a log), so `StreamBoundary` is reset afterwards rather than being
    ///   left holding a half-sequence that was never the workload's.
    /// - The epilogue leaves the primary grid selected, full-screen margins,
    ///   and a default pen, and `MarginTracker` is reset to match, so a
    ///   DECSTBM or `?1049h` that happened to be in force at the end of the
    ///   tail cannot outlive the seed and contradict the snapshot that is
    ///   fed next.
    /// - The tail is replayed with its **scroll regions removed**
    ///   (`without_scroll_regions`), which is what makes the replay produce
    ///   history at all for an agent TUI. See that function.
    pub fn seed_history(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.screen.seed(&without_scroll_regions(data));
        self.screen.process(b"\x1b[?1049l\x1b[r\x1b[m");
        self.screen.reset_margins();
        self.boundary.reset();
    }

    /// Whether a DECSTBM sub-range has been seen since this client's history
    /// became authoritative (attach, or the last `refresh_scrollback`) --
    /// `MarginTracker::subregion_seen` delegated. This is the "the live
    /// scrollback may be missing rows" signal: `vt100` drops a row scrolled
    /// out of any sub-range, so a workload that ever holds one (codex TUI
    /// streams its transcript through bottom-anchored sub-ranges) leaves the
    /// live model with an empty pager no matter how long it runs, while a
    /// workload that never does retains everything through the ordinary live
    /// path.
    pub fn subregion_seen(&self) -> bool {
        self.screen.subregion_seen()
    }

    /// Rebuild the pager's history from a fresh tail of the worker's retained
    /// raw bytes, then re-feed the worker's current snapshot so the grid is
    /// the live screen again.
    ///
    /// This is `seed_history` + snapshot re-run on a live attach, and it
    /// exists because the seed's region-stripping only ran at attach. A
    /// session attached *before* its interesting output happened -- `a new`,
    /// the default flow: the client model has been parsing since the first
    /// frame -- accumulated its history through the live path, where vt100
    /// behaves correctly for a real pane and drops every row that scrolls out
    /// of a DECSTBM sub-range. The codex TUI holds a sub-range almost
    /// constantly (289 of them in a 100 KiB sample of one real session, the
    /// transcript scrolling through bottom-anchored ones like `CSI 10;29 r`),
    /// so its pager opened on `SCROLL 0/0` after minutes of visible output.
    /// Re-running the seed at pager entry gives that attach the same past a
    /// fresh attach already gets, which is the only consistency the pager
    /// has ever promised (`scrollback_seed_bytes` in `src/bin/a.rs`).
    ///
    /// Mechanics, in order: replace the inner tracker at the current geometry
    /// and scrollback depth (`reset` -- also keeps the host alt-screen hold),
    /// replay the region-stripped tail into the fresh grid's history
    /// (`seed_history` -- its grid is as disposable as at attach), then feed
    /// the worker's `CaptureScreen` snapshot -- the same paintable bytes the
    /// attach handshake delivers -- so grid, margins, alt-screen state and
    /// input modes describe the live screen again rather than the tail's.
    /// Model-only throughout: nothing here reaches the host terminal.
    ///
    /// Two deliberate losses, both bounded and both self-healing for exactly
    /// the workloads that reach this path: bytes that arrived between the
    /// snapshot being fetched and the tracker swap are missing from the
    /// rebuilt grid until the workload's next repaint (a TUI repaints within
    /// a frame), and rows that scrolled by in that sliver are missing from
    /// the rebuilt history for good. A sub-millisecond window over two
    /// consecutive statements, against holding the model lock across both
    /// RPCs -- this codebase does not hold locks across I/O.
    ///
    /// The gate is the caller's (`subregion_seen`, plus `alternate_screen`:
    /// the alternate grid has no scrollback in vt100 or any real terminal, so
    /// there is nothing to rebuild for a full-screen application). An empty
    /// tail is rejected here rather than by the caller so the invariant is
    /// visible next to the rebuild it protects: a seed of zero bytes would
    /// replace whatever history the live path did retain with nothing.
    ///
    /// So is a tail that rebuilds to *less* history than the model already
    /// holds. The tail is a byte-count slice of a log, and the bytes are not
    /// rows: an agent idling between turns spends them on a spinner --
    /// thousands of absolute cursor addresses, not one line feed -- so the
    /// tail a pager entry happens to catch can replay to zero retained rows
    /// even though the log is far from empty. Adopting that rebuild would
    /// replace real transcript with nothing mid-conversation (`SCROLL 0/0`
    /// one entry, pages of history the next, depending on what the agent was
    /// doing when the user scrolled). tmux never shrinks a pane's history by
    /// re-deriving it, and neither does this: the rebuild is prepared in a
    /// scratch model and swapped in only when it retained at least as much
    /// as the live path currently shows. Costs one extra model allocation,
    /// no extra replay -- the parse was always the price of knowing.
    pub fn refresh_scrollback(&mut self, tail: &[u8], snapshot: &[u8]) {
        if tail.is_empty() {
            return;
        }
        let (rows, cols) = (self.screen.rows(), self.screen.cols());
        let depth = self.screen.scrollback_capacity();
        let mut candidate = match Self::try_new_with_scrollback(rows, cols, depth) {
            Ok(candidate) => candidate,
            Err(_) => return,
        };
        candidate.seed_history(tail);
        candidate.feed(snapshot);
        if candidate.scrollback_available() < self.scrollback_available() {
            return;
        }
        self.screen = candidate.screen;
        // The tail can begin mid-escape-sequence, exactly as at attach
        // (`seed_history` resets the copy it used; this one has not been
        // near the seed). The host-alt hold is untouched: the rebuild wrote
        // nothing to the host, so what the hold tracks is still true.
        self.boundary.reset();
    }

    /// `ScreenTracker::scrolled_frame` -- the pager's view of the history,
    /// with the model left resting at offset 0.
    pub fn scrolled_frame(&mut self, offset: usize) -> (Vec<u8>, usize, usize) {
        self.screen.scrolled_frame(offset)
    }

    /// How many lines of history are available to page back through.
    pub fn scrollback_available(&mut self) -> usize {
        self.screen.scrollback_available()
    }

    /// True while the workload has asked for mouse reporting of its own.
    pub fn workload_wants_mouse(&self) -> bool {
        self.screen.workload_wants_mouse()
    }

    /// True while the workload is on the alternate screen -- where, exactly
    /// as in tmux, there is no retained history to page through because the
    /// application owns the whole screen.
    pub fn alternate_screen(&self) -> bool {
        self.screen.alternate_screen()
    }

    /// An absolute re-assertion of the workload's own mouse modes, used to
    /// hand the mouse back after the client borrowed it for the wheel.
    pub fn workload_mouse_sequence(&self) -> Vec<u8> {
        self.screen.workload_mouse_sequence()
    }

    /// Keep the host terminal on the alternate screen for the rest of this
    /// attach. Call once after construction, before any host write other
    /// than the client's own `\x1b[?1049h`.
    pub fn hold_host_on_alt_screen(&mut self) {
        self.host_alt = Some(HostAltHold::new());
    }

    /// Rewrite `data` for the host terminal: drop alt-screen enter/exit so
    /// they cannot pop the host back to the primary screen (and its
    /// pre-attach scrollback). `None` means the hold is off and `data` can
    /// be written as-is.
    pub fn filter_host(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        self.host_alt.as_mut().map(|hold| hold.push(data))
    }

    /// Feed bytes that are being written to the host terminal *verbatim* --
    /// the attach snapshot and a session switch's replayed screen. These
    /// describe the workload's screen, so the model must see them, but they
    /// are never rewritten (there is no live workload cursor to protect
    /// during a snapshot; the snapshot is itself an absolute repaint).
    pub fn feed(&mut self, data: &[u8]) {
        self.screen.process(data);
        self.boundary.feed(data);
    }

    /// Feed a live PTY chunk and return the bytes the client should actually
    /// write, or `None` when the chunk goes out unchanged (the common case).
    ///
    /// The rewrite exists for exactly one class of bug: the host terminal's
    /// cursor ending up on a **different row** from the workload's own screen
    /// model. Nothing re-aligns that on its own, and every later
    /// column-addressed partial repaint -- Ink paints words at absolute
    /// columns, `\x1b[2GQuick\x1b[8Gsafety\x1b[15Gcheck:` -- then welds onto
    /// whatever the wrong row already held, which is the reported "two frames
    /// interleaved on one row" garbling. Three mechanisms produce it, each
    /// measured against a real `vt100::Parser` at the *host's* geometry (one
    /// row taller than this model, because the client reserved the bottom row
    /// for the status bar):
    ///
    /// 1. **A line feed off the model's last row while a sub-range is in
    ///    force** -- docs/terminal-state-design.md section 7.1. While the
    ///    client is re-asserting a workload's own DECSTBM sub-range, the
    ///    host's bottom row is the *screen* bottom rather than a margin
    ///    boundary, so a line feed on the workload's last row (which is
    ///    outside that sub-range) walks the host cursor down onto the
    ///    reserved row while this model, one row shorter, clamps and stays.
    /// 2. **A wrap off the last column of that same row.** Section 7.1
    ///    recorded this as residue that "self-heals because every status
    ///    redraw restores the cursor absolutely". It does not heal fast
    ///    enough -- up to 450 ms idle, 3 s forced -- and every Ink repaint in
    ///    between lands a row low. Same divergence, same repair, except that
    ///    the reposition is spliced *before* the character that wraps, so the
    ///    character itself also lands on the right row instead of on the bar.
    /// 3. **The workload resetting DECSTBM.** Claude Code opens with
    ///    `ESC 7`, `ESC [ r`, `ESC 8`. That bare `ESC [ r` widens the
    ///    *host's* scroll region back over the reserved row, and the client's
    ///    own re-assert only comes back around one socket round-trip later
    ///    (the worker's `Layout` event, `src/bin/a.rs`). Inside that window
    ///    -- which starts in the middle of the very chunk that reset it --
    ///    every line feed, wrap and downward cursor move on the model's last
    ///    row walks the host onto the reserved row, and this time the host
    ///    also fails to *scroll* where the model does. A cursor repair after
    ///    the fact cannot put back a scroll that never happened, so the
    ///    reservation is re-asserted *in the stream*, immediately after the
    ///    sequence that reset it and before the workload's next byte can use
    ///    it. Re-asserting `1;{rows}` here is the documented rule, not a new
    ///    one: it happens only while the workload is on full-screen margins,
    ///    which is precisely when section 7 says the client's own reservation
    ///    is the region to assert.
    ///
    /// A fourth, `CSI B` / `CSI E` / `CSI e` (and `ESC E`) off the last row
    /// under a sub-range, is mechanism 1's clamp mismatch driven by a
    /// cursor-motion sequence instead of a control, and is repaired the same
    /// way.
    ///
    /// **What is not reachable**, measured rather than assumed: with the
    /// client's own `1;{rows}` reservation in force -- which is what this
    /// method now *guarantees* whenever the workload is on full-screen
    /// margins -- a line feed, a wrap, `CSI B`, `CSI E` and `CSI e` on the
    /// model's last row all leave the host and the model in agreement,
    /// because that row is the bottom of the host's region and both sides
    /// scroll identically. Pinned by
    /// `relay_client_reservation_never_walks_onto_the_reserved_row`.
    ///
    /// Cost. A chunk with no `ESC` in it, arriving on a stream that is
    /// between sequences while no sub-range excludes the last row, cannot
    /// diverge at all and takes the same single bulk `process` call it always
    /// did -- that is bulk program output, the throughput case. Otherwise the
    /// chunk is walked in runs, never byte-by-byte for its own sake: a whole
    /// escape sequence per run (`StreamBoundary::bytes_to_ground`), and
    /// printable text in one run bounded by the number of columns still
    /// between the cursor and the far end of the last row, which is what
    /// makes the model's cursor guaranteed-current at every byte that could
    /// wrap off that row.
    pub fn relay(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if self.boundary.at_escape_boundary() && !self.exposed() && !data.contains(&0x1b) {
            // No `ESC` anywhere and a stream that is between sequences: this
            // chunk cannot set, reset or otherwise move a scroll region, so
            // the host keeps the client's own reservation for all of it and
            // (see above) cannot diverge. One bulk parse, no per-run walk.
            self.boundary.feed(data);
            self.screen.process(data);
            return None;
        }

        let mut out: Option<Vec<u8>> = None;
        // How much of `data` has already been copied into `out`.
        let mut copied = 0usize;
        let mut i = 0usize;
        // Set once the host has been repositioned ahead of a character that
        // is about to wrap, so the guard cannot fire again on that
        // character's UTF-8 continuation bytes.
        let mut wrap_guarded = false;

        while i < data.len() {
            if !wrap_guarded && self.wrap_would_walk(data[i]) {
                let row = self.screen.cursor_position().0;
                // The model will wrap this character to column 1 of the row
                // it is already on (it is outside the sub-range, so it clamps
                // instead of scrolling); the host, one row taller, would wrap
                // it onto the reserved row. Cancel the host's pending wrap by
                // putting it where the model is about to be.
                Self::splice(
                    &mut out,
                    data,
                    &mut copied,
                    i,
                    format!("\x1b[{};1H", row + 1).as_bytes(),
                );
                wrap_guarded = true;
            }

            let run = self.run_len(data, i);
            let end = i + run;
            let line_feed = run == 1 && matches!(data[i], b'\n' | 0x0b | 0x0c);
            let sequence = data[i] == 0x1b || !self.boundary.at_escape_boundary();
            let final_byte = data[end - 1];
            let exposed = self.exposed();
            let before_row = self.screen.cursor_position().0;
            let change = self.screen.process(&data[i..end]);
            self.boundary.feed(&data[i..end]);
            i = end;

            let last_row = self.last_row();
            let (row, col) = self.screen.cursor_position();
            let at_boundary = self.boundary.at_escape_boundary();

            // Mechanisms 1 and 4: the model clamped on its last row where the
            // host, one row taller, walked onto the reserved row.
            let walked = exposed
                && before_row == last_row
                && row == last_row
                && (line_feed
                    || (sequence && at_boundary && matches!(final_byte, b'B' | b'E' | b'e')));
            if walked {
                // Neither a line feed nor a downward cursor move leaves a
                // pending-wrap state, so a plain `CUP` is an exact restore.
                let col = col.min(self.screen.cols().saturating_sub(1));
                Self::splice(
                    &mut out,
                    data,
                    &mut copied,
                    i,
                    format!("\x1b[{};{}H", row + 1, col + 1).as_bytes(),
                );
            }

            if wrap_guarded && at_boundary {
                if row == last_row && col >= self.screen.cols() {
                    // The character attached to the preceding cell -- a
                    // zero-width combining mark -- instead of wrapping, so the
                    // model is still in pending wrap and the host is not. Put
                    // the host's pending wrap back; a `CUP` cannot express it,
                    // `cursor_state_formatted` (inside `cursor_restore`) can.
                    let restore = self.screen.cursor_restore();
                    Self::splice(&mut out, data, &mut copied, i, &restore);
                }
                wrap_guarded = false;
            }

            // Mechanism 3: close the reservation window in the stream itself.
            if at_boundary
                && matches!(change, Some(c) if c.margins_reset)
                && self.screen.margins().is_none()
                && self.screen.rows() >= 2
            {
                let mut seq = format!("\x1b[1;{}r", self.screen.rows()).into_bytes();
                // DECSTBM homes the cursor on a real terminal, so the
                // reposition is not optional. Absolute, from the model, and
                // never through the shared DECSC register.
                seq.extend_from_slice(&self.screen.cursor_restore());
                Self::splice(&mut out, data, &mut copied, i, &seq);
            }
        }

        if let Some(buf) = out.as_mut() {
            buf.extend_from_slice(&data[copied..]);
        }
        out
    }

    /// The model's own last row index -- the row the host terminal has one
    /// more of.
    fn last_row(&self) -> u16 {
        self.screen.rows().saturating_sub(1)
    }

    /// True while a workload DECSTBM sub-range leaves the model's last row
    /// *outside* the scroll region. That is the whole precondition for the
    /// reserved-row walk: the model clamps on its last row while the host,
    /// whose screen is a row taller, has somewhere to go.
    fn exposed(&self) -> bool {
        matches!(self.screen.margins(), Some((_, bottom)) if bottom <= self.last_row())
    }

    /// True when `byte` is the start of a character that the model is about
    /// to wrap off the far end of its last row while that row is outside the
    /// scroll region -- `vt100` reports the pending-wrap state as a cursor
    /// column equal to the screen width.
    fn wrap_would_walk(&self, byte: u8) -> bool {
        if byte < 0x20 || byte == 0x7f || !self.boundary.at_escape_boundary() || !self.exposed() {
            return false;
        }
        let (row, col) = self.screen.cursor_position();
        row == self.last_row() && col >= self.screen.cols()
    }

    /// How many bytes of `data[i..]` can be handed to the model in one
    /// `process` call without the cursor being able to wrap off the last row
    /// unobserved.
    ///
    /// Three shapes, in order: a line-feed control on its own (so the walk
    /// check sees the row immediately before and after it); a whole escape
    /// sequence (nothing prints inside one, and handing it over whole is also
    /// what makes a DECSTBM reset arrive as a single `LayoutChange` at the
    /// byte that caused it); or a run of printable text bounded by the
    /// columns still between the cursor and the far end of the last row.
    /// Bytes are never fewer than columns -- multi-byte and wide characters
    /// consume more of them per column, combining marks consume none -- so
    /// that bound is conservative in the safe direction.
    fn run_len(&self, data: &[u8], i: usize) -> usize {
        let byte = data[i];
        if matches!(byte, b'\n' | 0x0b | 0x0c) {
            return 1;
        }
        if byte == 0x1b || !self.boundary.at_escape_boundary() {
            return self.boundary.bytes_to_ground(&data[i..]).max(1);
        }
        // A printable run cannot change the margins (that needs an `ESC`,
        // which ends the run), so when the last row is not exposed there is
        // nothing to look for and the run is bounded only by the next
        // control.
        let budget = if self.exposed() {
            let (row, col) = self.screen.cursor_position();
            let cols = usize::from(self.screen.cols());
            let rows_left = usize::from(self.last_row().saturating_sub(row));
            rows_left * cols + cols.saturating_sub(usize::from(col))
        } else {
            usize::MAX
        };
        let mut n = 0usize;
        while i + n < data.len() && n < budget {
            // TAB advances by up to a whole tab stop rather than one column
            // per byte, so it ends a budgeted run.
            if matches!(data[i + n], 0x1b | b'\n' | 0x0b | 0x0c | b'\t') {
                break;
            }
            n += 1;
        }
        n.max(1)
    }

    /// Copy `data[copied..upto]` into the rewrite buffer, then `extra`, and
    /// remember how far the copy got.
    fn splice(
        out: &mut Option<Vec<u8>>,
        data: &[u8],
        copied: &mut usize,
        upto: usize,
        extra: &[u8],
    ) {
        let buf = out.get_or_insert_with(Vec::new);
        buf.extend_from_slice(&data[*copied..upto]);
        buf.extend_from_slice(extra);
        *copied = upto;
    }

    /// Re-fit both the model and the tracked margins to a new workload
    /// geometry (the physical terminal minus the reserved row).
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        let _ = self.screen.try_set_size(rows, cols);
    }

    /// Start over for a different session (`Ctrl-b n`): a new workload has
    /// its own screen, its own margins and its own half-parsed sequences.
    pub fn reset(&mut self, rows: u16, cols: u16) {
        // The retained-history depth belongs to the *client*, not to the
        // session it happens to be showing, so a switch rebuilds an equally
        // deep (and equally empty) grid rather than silently dropping to the
        // worker's zero-scrollback shape.
        let scrollback = self.screen.scrollback_capacity();
        if let Ok(fresh) = ScreenTracker::try_new_with_scrollback(rows, cols, scrollback) {
            self.screen = fresh;
        }
        self.boundary.reset();
        if let Some(hold) = self.host_alt.as_mut() {
            hold.reset();
        }
    }

    pub fn margins(&self) -> Option<(u16, u16)> {
        self.screen.margins()
    }

    /// True when the client may write bytes of its own: the relayed stream is
    /// between complete escape sequences and characters. This is a hard
    /// requirement -- injecting anywhere else corrupts the workload's frame.
    pub fn at_escape_boundary(&self) -> bool {
        self.boundary.at_escape_boundary()
    }

    /// True while the workload is part-way through a synchronized-output
    /// frame. A soft preference rather than a hard requirement: waiting for
    /// the frame to close keeps the redraw out of the middle of a repaint,
    /// but a workload that never closes one must not be able to starve the
    /// bar forever (see `STATUS_BAR_SYNC_DEFER_LIMIT` in `src/bin/a.rs`).
    pub fn in_synchronized_update(&self) -> bool {
        self.boundary.in_synchronized_update()
    }

    pub fn cursor_restore(&self) -> Vec<u8> {
        self.screen.cursor_restore()
    }

    /// The same reattach payload `ScreenTracker::snapshot` produces -- used
    /// by `Ctrl-b r` to repaint the host from this model without a round
    /// trip to the worker.
    pub fn snapshot(&self) -> Vec<u8> {
        self.screen.snapshot()
    }

    #[cfg(test)]
    pub fn cursor_position(&self) -> (u16, u16) {
        self.screen.cursor_position()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_dimensions_use_a_non_degenerate_terminal_fallback() {
        assert_eq!(
            validate_size(0, 0).unwrap(),
            (DEFAULT_TERMINAL_ROWS, DEFAULT_TERMINAL_COLS)
        );
        assert_eq!(validate_size(0, 132).unwrap(), (DEFAULT_TERMINAL_ROWS, 132));
        assert_eq!(validate_size(43, 0).unwrap(), (43, DEFAULT_TERMINAL_COLS));

        let mut tracker = ScreenTracker::try_new(0, 0).unwrap();
        tracker.process(b"fallback remains interactive\r\n");
        assert!(tracker.contents().contains("fallback remains interactive"));
    }

    #[test]
    fn screen_dimensions_are_bounded_and_overflow_safe() {
        assert_eq!(
            validate_size(0, 0).unwrap(),
            (DEFAULT_TERMINAL_ROWS, DEFAULT_TERMINAL_COLS)
        );
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
            assert_eq!(
                t.margins(),
                Some((3, 20)),
                "split at {split} lost the margin"
            );
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
        seq.extend(std::iter::repeat_n(b'1', 64));
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
        let change = tracker
            .process(b"\x1b[r")
            .expect("reset should emit a change");
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
        assert_eq!(
            screen_a.contents(),
            screen_b.contents(),
            "contents mismatch"
        );
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
        stream.extend_from_slice(
            b"\x1b[1;36m\xe2\x94\x8c\xe2\x94\x80\xe2\x94\x80\xe2\x94\x90\x1b[0m\r\n",
        );
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
        assert_eq!(
            tracker.parser.screen().cursor_position(),
            b.screen().cursor_position()
        );
    }

    /// The resize every attach performs: the worker's model shrinks one row
    /// to make room for the status bar. `vt100` truncates from the bottom,
    /// which drops the newest line and parks the cursor on the row above it
    /// -- where the shell's WINCH prompt redraw then overwrites what is
    /// left. A real terminal scrolls instead, so the tracker does too: the
    /// cursor's line survives, the top advances, and the last line stays
    /// last.
    #[test]
    fn shrinking_keeps_the_cursor_line_the_way_a_real_terminal_does() {
        let mut t = ScreenTracker::new(24, 80);
        let mut fill = Vec::new();
        for i in 58..=80 {
            fill.extend_from_slice(format!("HISTLINE-{i}\r\n").as_bytes());
        }
        fill.extend_from_slice(b"prompt$ ");
        t.process(&fill);
        assert!(t.contents().contains("HISTLINE-80"));

        t.set_size(23, 80);
        let after = t.contents();
        assert!(
            after.contains("HISTLINE-80"),
            "the newest line must survive the shrink:\n{after}"
        );
        assert!(
            !after.contains("HISTLINE-58"),
            "the freed row comes off the top, not the bottom:\n{after}"
        );
        assert!(
            after.ends_with("prompt$ "),
            "the cursor's line stays the last line:\n{after}"
        );
    }

    /// The compensation's two gates. With a DECSTBM sub-range in force the
    /// workload owns the resize semantics, so the tracker does nothing. With
    /// a half-received sequence in flight the synthetic SU must not be
    /// injected into it -- the shrink then falls back to plain truncation,
    /// and the pending sequence still completes correctly.
    #[test]
    fn shrinking_compensation_is_gated_on_regions_and_mid_sequence_streams() {
        // Region holder: no compensation.
        let mut t = ScreenTracker::new(24, 80);
        let mut fill = Vec::new();
        for i in 58..=80 {
            fill.extend_from_slice(format!("HISTLINE-{i}\r\n").as_bytes());
        }
        t.process(&fill);
        t.process(b"\x1b[3;20r");
        t.set_size(23, 80);
        let region = t.contents();
        assert!(
            region.contains("HISTLINE-58"),
            "a region holder keeps vt100's own truncation behaviour -- the top \
             row must not advance:\n{region}"
        );
        assert_eq!(t.margins(), Some((3, 20)));

        // Mid-sequence: the guard must skip the SU and leave the pending
        // sequence parseable.
        let mut t = ScreenTracker::new(24, 80);
        let mut fill = Vec::new();
        for i in 58..=80 {
            fill.extend_from_slice(format!("HISTLINE-{i}\r\n").as_bytes());
        }
        fill.extend_from_slice(b"prompt$ \x1b[38;5");
        t.process(&fill);
        t.set_size(23, 80);
        t.process(b";1m");
        let midseq = t.contents();
        assert!(
            midseq.contains("HISTLINE-80"),
            "without compensation the newest row is simply truncated, and the \
             partial sequence must still complete into a plain SGR:\n{midseq}"
        );
        assert!(
            !midseq.contains('\u{9b}'),
            "no glued control state may survive:\n{midseq:?}"
        );
    }

    /// Growth never compensates: the cursor is above the new bottom, the
    /// content stays top-anchored, and blank rows appear below -- the same
    /// as before the fix, pinned so a future edit cannot quietly start
    /// scrolling on grow.
    #[test]
    fn growing_never_scrolls_the_content() {
        let mut t = ScreenTracker::new(10, 80);
        t.process(b"top line\r\nsecond line");
        t.set_size(24, 80);
        let after = t.contents();
        assert!(after.starts_with("top line"), "{after:?}");
        assert!(after.contains("second line"));
        assert_eq!(t.rows(), 24);
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

    // -- retained history (the grid `Ctrl-b [` pages through) --------------

    /// Rows scrolled off the top are kept and can be paged back to, and the
    /// rendered frame is what a real host terminal would show -- measured by
    /// feeding it to an actual `vt100` parser rather than by asserting on
    /// escape codes.
    #[test]
    fn client_screen_retains_scrolled_off_rows_and_pages_back_to_them() {
        let mut client = ClientScreen::try_new_with_scrollback(5, 20, 100).unwrap();
        for i in 1..=30 {
            client.feed(format!("LINE-{i:02}\r\n").as_bytes());
        }
        let available = client.scrollback_available();
        assert!(
            available >= 25,
            "30 lines through a 5-row screen must leave ~26 in history, got {available}"
        );

        // Live view: only the last few lines are on screen.
        let live = {
            let mut host = vt100::Parser::new(5, 20, 0);
            host.process(&client.scrolled_frame(0).0);
            host.screen().contents()
        };
        assert!(
            live.contains("LINE-29"),
            "live view lost the newest lines:\n{live}"
        );
        assert!(
            !live.contains("LINE-05"),
            "live view should not reach back:\n{live}"
        );

        // Paged back: the older lines are there, in order.
        let scrolled = {
            let (frame, offset, _) = client.scrolled_frame(20);
            assert_eq!(offset, 20, "the requested offset was reachable");
            let mut host = vt100::Parser::new(5, 20, 0);
            host.process(&frame);
            host.screen().contents()
        };
        assert!(
            scrolled.contains("LINE-07"),
            "paging back 20 lines must reach the LINE-07 window:\n{scrolled}"
        );
        assert!(
            !scrolled.contains("LINE-29"),
            "paging back must actually move the window:\n{scrolled}"
        );

        // An offset past the end is clamped, not an error.
        let (_, clamped, total) = client.scrolled_frame(usize::MAX);
        assert_eq!(clamped, total);
    }

    /// The pager must be a *view*: after rendering a scrolled-back frame the
    /// model still describes the live screen, or the status bar's cursor
    /// restore and `ClientScreen::relay`'s row arithmetic would both be
    /// answering about a screen the workload is not on.
    #[test]
    fn rendering_a_scrolled_frame_leaves_the_model_on_the_live_screen() {
        let mut client = ClientScreen::try_new_with_scrollback(5, 20, 100).unwrap();
        for i in 1..=30 {
            client.feed(format!("LINE-{i:02}\r\n").as_bytes());
        }
        let before = (
            client.snapshot(),
            client.cursor_position(),
            client.margins(),
        );
        let _ = client.scrolled_frame(15);
        let after = (
            client.snapshot(),
            client.cursor_position(),
            client.margins(),
        );
        assert_eq!(before, after, "the pager left the model scrolled back");
    }

    /// Retained history is capped by the configured line count, the oldest
    /// rows falling off the front -- tmux's `history-limit`, not an
    /// unbounded buffer.
    #[test]
    fn retained_history_is_bounded_by_the_configured_line_limit() {
        let mut client = ClientScreen::try_new_with_scrollback(5, 20, 10).unwrap();
        for i in 1..=100 {
            client.feed(format!("LINE-{i:03}\r\n").as_bytes());
        }
        assert_eq!(client.scrollback_available(), 10);
        let oldest = {
            let (frame, _, _) = client.scrolled_frame(10);
            let mut host = vt100::Parser::new(5, 20, 0);
            host.process(&frame);
            host.screen().contents()
        };
        assert!(
            !oldest.contains("LINE-001"),
            "a 10-line limit must have dropped the first lines:\n{oldest}"
        );
    }

    #[test]
    fn scrollback_lines_are_clamped_against_the_cell_budget() {
        assert_eq!(scrollback_lines_for(80, 0), 0);
        assert_eq!(scrollback_lines_for(80, 2000), 2000);
        // A wild request is clamped rather than allocated.
        let clamped = scrollback_lines_for(200, 10_000_000);
        assert!(
            clamped * 200 <= MAX_SCROLLBACK_CELLS,
            "clamp exceeded the budget"
        );
        assert!(clamped > 0);
    }

    /// A full-screen alternate-screen application owns the whole screen and
    /// has no history behind it -- the same thing tmux's copy-mode shows in
    /// an alt-screen pane. Pinned so the scroll-mode status bar's "no
    /// history" note stays truthful.
    #[test]
    fn an_alt_screen_workload_has_no_history_to_page_through() {
        let mut client = ClientScreen::try_new_with_scrollback(5, 20, 100).unwrap();
        for i in 1..=30 {
            client.feed(format!("LINE-{i:02}\r\n").as_bytes());
        }
        assert!(client.scrollback_available() > 0);
        client.feed(b"\x1b[?1049h");
        assert!(client.alternate_screen());
        for i in 1..=30 {
            client.feed(format!("ALT-{i:02}\r\n").as_bytes());
        }
        assert_eq!(
            client.scrollback_available(),
            0,
            "the alternate grid has no scrollback, in vt100 as in every real terminal"
        );
        // ...and leaving it gives the primary screen's history back.
        client.feed(b"\x1b[?1049l");
        assert!(client.scrollback_available() > 0);
    }

    /// **The empty-pager regression.** A tail written under a DECSTBM
    /// sub-range -- the shape every agent CLI holds while it reserves a
    /// composer at the bottom of the screen -- must still leave history to
    /// page through.
    ///
    /// The control half is what makes this a real test rather than an
    /// assertion that the code does what it does: the identical bytes, fed
    /// through the *live* path (`feed`, which is what `relay` uses), retain
    /// nothing at all. That is `vt100` behaving correctly for a live pane and
    /// is deliberately left alone; the seed replay is the one place where a
    /// region has nothing to protect, because its grid is thrown away by the
    /// reattach snapshot moments later and only its history survives.
    #[test]
    fn seeding_retains_lines_that_scrolled_out_of_a_scroll_region() {
        let mut tail = b"\x1b[3;23r".to_vec();
        for i in 1..=200 {
            tail.extend_from_slice(format!("REGION-{i:03}\r\n").as_bytes());
        }

        let mut live = ClientScreen::try_new_with_scrollback(23, 40, 500).unwrap();
        live.feed(&tail);
        assert_eq!(
            live.scrollback_available(),
            0,
            "control: a live pane under a sub-range retains nothing -- if this ever \
             stops being true, the seed no longer needs its own path"
        );

        let mut seeded = ClientScreen::try_new_with_scrollback(23, 40, 500).unwrap();
        seeded.seed_history(&tail);
        let available = seeded.scrollback_available();
        assert!(
            available > 100,
            "seeding under a scroll region retained only {available} lines: this is the \
             empty-pager bug -- `Ctrl-b [` opens on SCROLL 0/0 in every agent session"
        );

        // And the retained rows are the transcript, not blank filler.
        let (frame, _, _) = seeded.scrolled_frame(available);
        let text = String::from_utf8_lossy(&frame).into_owned();
        assert!(
            text.contains("REGION-0"),
            "the retained history must hold the rows that scrolled out of the region:\n{text}"
        );
    }

    /// The strip is narrow on purpose: DECSTBM goes, and the sequence that
    /// merely looks like it -- `CSI ? Ps r`, XTRESTORE -- stays, because
    /// swallowing a workload's private-mode restore would be a new bug in
    /// place of the old one.
    #[test]
    fn stripping_scroll_regions_leaves_every_other_sequence_alone() {
        let kept = b"\x1b[?1049h\x1b[?1000r\x1b[31mred\x1b[0m\rplain text with an r in it";
        assert_eq!(
            without_scroll_regions(kept).as_ref(),
            kept,
            "only DECSTBM may be removed"
        );
        assert!(
            matches!(without_scroll_regions(kept), std::borrow::Cow::Borrowed(_)),
            "a tail with no DECSTBM must not be copied -- that is every plain shell \
             session, on the attach path"
        );

        for (input, want) in [
            (b"a\x1b[3;23rb".as_slice(), b"ab".as_slice()),
            (b"a\x1b[rb".as_slice(), b"ab".as_slice()),
            (b"\x1b[1;24r\x1b[5;10rX".as_slice(), b"X".as_slice()),
            // Truncated at the end of the buffer: a tail is a byte slice of a
            // log and can stop anywhere, so this must be emitted verbatim
            // rather than eaten while waiting for a final byte.
            (b"tail\x1b[3;2".as_slice(), b"tail\x1b[3;2".as_slice()),
        ] {
            assert_eq!(
                without_scroll_regions(input).as_ref(),
                want,
                "stripping {input:?}"
            );
        }
    }

    /// Priming from a raw tail gives a freshly attached client a past to
    /// page through, and leaves nothing of that tail's terminal state behind
    /// to contradict the snapshot fed next.
    #[test]
    fn seeding_history_fills_the_grid_without_leaking_the_tails_state() {
        let mut client = ClientScreen::try_new_with_scrollback(5, 20, 100).unwrap();
        let mut tail = Vec::new();
        for i in 1..=30 {
            tail.extend_from_slice(format!("SEED-{i:02}\r\n").as_bytes());
        }
        // The tail ends inside an alt-screen TUI with a scroll region set
        // and a half-emitted escape sequence, exactly as a byte-count slice
        // of a live log can.
        tail.extend_from_slice(b"\x1b[?1049h\x1b[2;4r\x1b[38;5;");
        client.seed_history(&tail);

        assert!(
            client.scrollback_available() > 0,
            "seeding must leave history to page through"
        );
        assert!(
            !client.alternate_screen(),
            "the tail's alt-screen state must not outlive the seed"
        );
        assert_eq!(
            client.margins(),
            None,
            "the tail's DECSTBM must not outlive the seed"
        );
        assert!(
            client.at_escape_boundary(),
            "the tail's half-emitted sequence must not leave the stream mid-sequence"
        );
    }

    /// The gate for the pager-entry history rebuild: a DECSTBM sub-range
    /// anywhere in the model's lifetime means the live scrollback may be
    /// missing rows, and that must survive the workload later resetting its
    /// margins -- `vt100` does not give the dropped rows back. Only an
    /// explicit rebuild of the measurement (`reset`, which the seed path
    /// drives) clears it, and full-screen-only traffic never sets it.
    #[test]
    fn margin_tracker_subrange_use_is_sticky_until_reset() {
        let mut t = MarginTracker::new(24);
        assert!(!t.subregion_seen());
        t.scan(b"\x1b[3;20r");
        assert!(t.subregion_seen());
        t.scan(b"\x1b[1;24r");
        assert!(
            t.subregion_seen(),
            "the workload resetting its region does not restore the rows dropped while it held one"
        );
        t.reset();
        assert!(!t.subregion_seen());

        let mut t = MarginTracker::new(24);
        t.scan(b"\x1b[r\x1b[1;24r\x1b[?1000r");
        assert!(
            !t.subregion_seen(),
            "full-screen DECSTBM and XTRESTORE are not sub-ranges"
        );
    }

    /// A workload that never sends a sub-range keeps its whole history
    /// through the ordinary live path -- the population the pager-entry
    /// refresh must leave untouched (its history is already complete, and a
    /// byte-capped replay could only shrink it).
    #[test]
    fn full_screen_scrolling_never_flags_subregion_use() {
        let mut client = ClientScreen::try_new_with_scrollback(5, 20, 100).unwrap();
        for i in 1..=30 {
            client.feed(format!("SHELL-{i:02}\r\n").as_bytes());
        }
        assert!(client.scrollback_available() > 0);
        assert!(!client.subregion_seen());
    }

    /// **The live-attach empty pager.** The seed test above
    /// (`seeding_retains_lines_that_scrolled_out_of_a_scroll_region`) fixes a
    /// *fresh* attach; this is the session that was attached all along. Its
    /// model ate every byte through the live path, where a sub-range is
    /// honored and rows scrolled out of one are gone -- so `a new` + a long
    /// codex run opened the pager on `SCROLL 0/0` no matter how much had
    /// streamed past. Re-running the seed against the worker's raw tail
    /// (`refresh_scrollback`) restores exactly what a fresh attach would have
    /// had, and the snapshot fed afterwards leaves the grid describing the
    /// live screen, the workload's own region re-asserted, and the stream at
    /// a boundary.
    #[test]
    fn refresh_scrollback_restores_the_history_a_scroll_region_emptied() {
        let mut region_tail = b"\x1b[3;23r".to_vec();
        for i in 1..=200 {
            region_tail.extend_from_slice(format!("REGION-{i:03}\r\n").as_bytes());
        }

        let mut client = ClientScreen::try_new_with_scrollback(23, 40, 500).unwrap();
        client.feed(&region_tail);
        assert_eq!(
            client.scrollback_available(),
            0,
            "control: the live path under a sub-range retains nothing"
        );
        assert!(
            client.subregion_seen(),
            "this workload is the one the refresh gate exists for"
        );

        // What the worker answers at pager entry: the same raw tail (DECSTBM
        // still in it -- stripping is the seed's job, not the log's), then a
        // snapshot painting the current screen and re-asserting the region
        // the worker's own model holds.
        let snapshot = b"\x1b[2J\x1b[1;1H newest row\x1b[3;23r";
        client.refresh_scrollback(&region_tail, snapshot);

        let available = client.scrollback_available();
        assert!(
            available > 100,
            "replaying the tail must give the pager its past back, got {available} lines"
        );
        let (frame, _, _) = client.scrolled_frame(available);
        let text = String::from_utf8_lossy(&frame).into_owned();
        assert!(
            text.contains("REGION-0"),
            "the rebuilt history must hold the rows that scrolled out of the region:\n{text}"
        );
        let (live_frame, _, _) = client.scrolled_frame(0);
        let live_text = String::from_utf8_lossy(&live_frame).into_owned();
        assert!(
            live_text.contains("newest row"),
            "the snapshot must leave the grid on the live screen:\n{live_text}"
        );
        assert_eq!(
            client.margins(),
            Some((3, 23)),
            "the snapshot re-asserts the workload's own sub-range"
        );
        assert!(client.at_escape_boundary());
        assert!(
            client.subregion_seen(),
            "still region-using after the rebuild, so the next pager entry refreshes again"
        );
    }

    /// An empty tail must not be able to *destroy* history: a seed of zero
    /// bytes replaces whatever the live path did retain with nothing.
    #[test]
    fn refresh_scrollback_rejects_an_empty_tail() {
        let mut client = ClientScreen::try_new_with_scrollback(5, 20, 100).unwrap();
        for i in 1..=30 {
            client.feed(format!("KEEP-{i:02}\r\n").as_bytes());
        }
        let before = client.scrollback_available();
        assert!(before > 0);
        client.refresh_scrollback(b"", b"\x1b[2J\x1b[1;1Hirrelevant");
        assert_eq!(
            client.scrollback_available(),
            before,
            "nothing to seed means nothing to rebuild"
        );
    }

    /// **The spinner-hour wipe.** The tail is a byte budget out of a log,
    /// and bytes are not rows: an agent idling between turns spends the
    /// budget on a spinner -- absolute cursor addresses, not one line feed --
    /// so the tail a pager entry happens to catch can replay to zero
    /// retained rows. The old rebuild reset the tracker and adopted it
    /// anyway, so a session whose pager had pages of history one entry
    /// opened on `SCROLL 0/0` the next, depending on what the agent was
    /// doing when the user scrolled. A rebuild that retained less than the
    /// live model holds must be refused, exactly like the empty tail.
    #[test]
    fn refresh_scrollback_never_trades_history_for_a_tail_that_replayed_to_less() {
        let mut client = ClientScreen::try_new_with_scrollback(5, 20, 100).unwrap();
        for i in 1..=30 {
            client.feed(format!("KEEP-{i:02}\r\n").as_bytes());
        }
        let before = client.scrollback_available();
        assert!(before > 0);
        let mut spinner = Vec::new();
        for _ in 0..2000 {
            spinner.extend_from_slice(b"\x1b[1;1H*");
        }
        client.refresh_scrollback(&spinner, b"\x1b[2J\x1b[1;1H newest");
        assert_eq!(
            client.scrollback_available(),
            before,
            "a rebuild worth fewer rows than the model holds must be refused"
        );
        let (frame, _, _) = client.scrolled_frame(before);
        let text = String::from_utf8_lossy(&frame).into_owned();
        assert!(
            text.contains("KEEP-01"),
            "the refused rebuild must leave the old history in place:\n{text}"
        );
    }

    /// The other side of the guard: a rebuild that retained at least as much
    /// as the model holds is adopted, so the pager's past tracks the worker's
    /// retained tail instead of fossilizing at whatever the attach seed
    /// captured.
    #[test]
    fn refresh_scrollback_adopts_a_rebuild_that_retains_at_least_as_much() {
        let mut client = ClientScreen::try_new_with_scrollback(5, 20, 100).unwrap();
        for i in 1..=10 {
            client.feed(format!("OLD-{i:02}\r\n").as_bytes());
        }
        let before = client.scrollback_available();
        assert!(before > 0);
        let mut fresh_tail = Vec::new();
        for i in 1..=40 {
            fresh_tail.extend_from_slice(format!("NEW-{i:03}\r\n").as_bytes());
        }
        client.refresh_scrollback(&fresh_tail, b"\x1b[2J\x1b[1;1H newest");
        let after = client.scrollback_available();
        assert!(
            after > before,
            "a strictly better rebuild must be adopted, {before} -> {after}"
        );
        let (frame, _, _) = client.scrolled_frame(after);
        let text = String::from_utf8_lossy(&frame).into_owned();
        assert!(
            text.contains("NEW-001") && !text.contains("OLD-"),
            "the adopted rebuild replaces the stale history wholesale:\n{text}"
        );
    }

    /// Handing the mouse back to the workload has to be expressible as one
    /// self-contained write, whatever the client turned on for itself.
    #[test]
    fn workload_mouse_sequence_restores_exactly_what_the_workload_asked_for() {
        for (workload, mode, encoding) in [
            (
                b"".as_slice(),
                vt100::MouseProtocolMode::None,
                vt100::MouseProtocolEncoding::Default,
            ),
            (
                b"\x1b[?1000h\x1b[?1006h".as_slice(),
                vt100::MouseProtocolMode::PressRelease,
                vt100::MouseProtocolEncoding::Sgr,
            ),
            (
                b"\x1b[?1003h".as_slice(),
                vt100::MouseProtocolMode::AnyMotion,
                vt100::MouseProtocolEncoding::Default,
            ),
            (
                b"\x1b[?1002h\x1b[?1005h".as_slice(),
                vt100::MouseProtocolMode::ButtonMotion,
                vt100::MouseProtocolEncoding::Utf8,
            ),
        ] {
            let mut client = ClientScreen::try_new(5, 20).unwrap();
            client.feed(workload);
            assert_eq!(
                client.workload_wants_mouse(),
                mode != vt100::MouseProtocolMode::None
            );

            // A host that the client had already borrowed the mouse on.
            let mut host = vt100::Parser::new(5, 20, 0);
            host.process(b"\x1b[?1000h\x1b[?1006h");
            host.process(&client.workload_mouse_sequence());
            assert_eq!(host.screen().mouse_protocol_mode(), mode);
            assert_eq!(host.screen().mouse_protocol_encoding(), encoding);
        }
    }

    #[test]
    fn client_screen_snapshot_matches_tracker() {
        let mut tracker = ScreenTracker::new(24, 80);
        tracker.process(b"hello there\r\n");
        let mut client = ClientScreen::try_new(24, 80).unwrap();
        client.feed(b"hello there\r\n");
        assert_eq!(client.snapshot(), tracker.snapshot());
        assert!(String::from_utf8_lossy(&client.snapshot()).contains("hello there"));
    }

    #[test]
    fn host_alt_hold_drops_1049_and_keeps_other_modes() {
        let mut hold = HostAltHold::new();
        assert_eq!(hold.push(b"\x1b[?1049hhello\x1b[?1049l"), b"hello");
        assert_eq!(hold.push(b"\x1b[?1049;2004h"), b"\x1b[?2004h");
        assert_eq!(hold.push(b"\x1b[?1000h"), b"\x1b[?1000h");
        assert_eq!(hold.push(b"\x1b[2J"), b"\x1b[2J");
        assert_eq!(hold.push(b"\x1b[?47l\x1b[?1047h"), b"");
    }

    #[test]
    fn host_alt_hold_strips_a_split_1049l() {
        let mut hold = HostAltHold::new();
        assert_eq!(hold.push(b"\x1b[?1049"), b"");
        assert_eq!(hold.push(b"lXYZ"), b"XYZ");
    }

    #[test]
    fn client_screen_filter_host_is_off_until_enabled() {
        let mut client = ClientScreen::try_new(24, 80).unwrap();
        assert_eq!(client.filter_host(b"\x1b[?1049l"), None);
        client.hold_host_on_alt_screen();
        assert_eq!(
            client.filter_host(b"\x1b[?1049ltext"),
            Some(b"text".to_vec())
        );
        client.feed(b"\x1b[?1049h");
        assert!(
            client.snapshot().windows(8).any(|w| w == b"\x1b[?1049h"),
            "the model must still see the workload's alt-screen enter"
        );
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

#[cfg(test)]
mod boundary_tests {
    use super::*;

    fn boundary_after(chunks: &[&[u8]]) -> StreamBoundary {
        let mut b = StreamBoundary::new();
        for c in chunks {
            b.feed(c);
        }
        b
    }

    /// The exact shape measured on a real `a attach` against a
    /// continuously-streaming TUI (issue #5): the PTY read boundary fell
    /// inside `\x1b[38;5;`, and the status bar wrote there. Every one of these
    /// mid-sequence positions has to report "not a boundary", or the client
    /// will splice into a half-emitted sequence again.
    #[test]
    fn mid_sequence_positions_are_not_boundaries() {
        for prefix in [
            &b"\x1b"[..],
            b"\x1b[",
            b"\x1b[38",
            b"\x1b[38;5;",
            b"\x1b[38;5;91",
            b"\x1b[?2026",
            b"\x1b#",
            b"\x1b]0;title",
            b"\x1b]0;title\x1b",
            b"\x1bPtmux;",
            b"\x1b_payload",
        ] {
            assert!(
                !boundary_after(&[prefix]).at_escape_boundary(),
                "{prefix:?} leaves an unterminated sequence in flight"
            );
        }
        for complete in [
            &b"\x1b[38;5;91m"[..],
            b"\x1b[?2026h\x1b[?2026l",
            b"\x1b[H",
            b"\x1b7",
            b"\x1b]0;title\x07",
            b"\x1b]0;title\x1b\\",
            b"\x1bPtmux;x\x1b\\",
            b"plain text",
            b"",
        ] {
            assert!(
                boundary_after(&[complete]).at_escape_boundary(),
                "{complete:?} ends between sequences"
            );
        }
    }

    /// A sequence split across two PTY reads is still one sequence: the state
    /// has to survive the chunk boundary, which is the whole point (the
    /// injection happens *at* chunk boundaries).
    #[test]
    fn state_survives_a_chunk_split() {
        assert!(!boundary_after(&[b"\x1b[38;5;", b"9"]).at_escape_boundary());
        assert!(boundary_after(&[b"\x1b[38;5;", b"91m"]).at_escape_boundary());
        assert!(!boundary_after(&[b"\x1b", b"["]).at_escape_boundary());
    }

    /// Splitting a multi-byte character is the same corruption with a
    /// replacement glyph instead of stray digits, so a partial UTF-8
    /// character is not a boundary either. `\xc2\xb7` (MIDDLE DOT) is exactly
    /// what opencode draws its separators with.
    #[test]
    fn partial_utf8_is_not_a_boundary() {
        assert!(!boundary_after(&[b"\xc2"]).at_escape_boundary());
        assert!(boundary_after(&[b"\xc2\xb7"]).at_escape_boundary());
        assert!(!boundary_after(&[b"\xe2\x94"]).at_escape_boundary());
        assert!(boundary_after(&[b"\xe2\x94\x80"]).at_escape_boundary());
        assert!(!boundary_after(&[b"\xf0\x9f\x92"]).at_escape_boundary());
        assert!(boundary_after(&[b"\xf0\x9f\x92\xa9"]).at_escape_boundary());
    }

    /// `CSI ? 2026 h` / `l` bracket a frame. opencode and codex emit one pair
    /// per redraw (measured: 44 pairs in a 30 s capture), so tracking the
    /// depth hands the client exact frame boundaries for free.
    #[test]
    fn synchronized_update_depth_tracks_frames() {
        let mut b = StreamBoundary::new();
        assert!(!b.in_synchronized_update());
        b.feed(b"\x1b[?2026h");
        assert!(b.in_synchronized_update());
        b.feed(b"\x1b[1;1Hpainting");
        assert!(b.in_synchronized_update());
        b.feed(b"\x1b[?2026l");
        assert!(!b.in_synchronized_update());
        // Split across chunks, and not confused by a same-shaped neighbour.
        b.feed(b"\x1b[?202");
        b.feed(b"6h");
        assert!(b.in_synchronized_update());
        b.feed(b"\x1b[?2004l\x1b[?1049l");
        assert!(b.in_synchronized_update());
        b.feed(b"\x1b[?2026l");
        assert!(!b.in_synchronized_update());
    }

    /// A switch hands the client a different session's stream; a half-parsed
    /// sequence or an open frame from the previous one must not carry over.
    #[test]
    fn reset_clears_in_flight_state() {
        let mut b = StreamBoundary::new();
        b.feed(b"\x1b[?2026h\x1b[38;5;");
        assert!(!b.at_escape_boundary());
        assert!(b.in_synchronized_update());
        b.reset();
        assert!(b.at_escape_boundary());
        assert!(!b.in_synchronized_update());
    }

    /// The reserved-row walk (docs/terminal-state-design.md section 7.1),
    /// measured both ways against real `vt100` parsers: the workload's own
    /// 23-row screen versus the 24-row host running the workload's `5;15`
    /// sub-range. Without the rewrite the host cursor lands on row 24 and the
    /// two disagree forever; with it, the spliced reposition puts the host
    /// back on row 23 where the workload believes it is.
    #[test]
    fn relay_keeps_a_line_feed_off_the_reserved_row_under_a_sub_range() {
        let mut client = ClientScreen::try_new(23, 80).unwrap();
        let mut host = vt100::Parser::new(24, 80, 0);
        host.process(b"\x1b[24;1HBAR-TEXT\x1b[1;23r");
        let mut workload = vt100::Parser::new(23, 80, 0);

        let walk = b"\x1b[5;15r\x1b[23;1HWORKLOAD-LAST-ROW\nWALKED";
        workload.process(walk);
        // The client re-asserts the workload's sub-range on the host, which is
        // what exposes row 24 (see `draw_status_bar`).
        host.process(b"\x1b[5;15r");
        let rewritten = client.relay(walk);
        assert!(
            rewritten.is_some(),
            "a line feed on the last row under a sub-range must be repaired"
        );
        host.process(rewritten.as_deref().unwrap());

        assert_eq!(
            host.screen().cursor_position(),
            workload.screen().cursor_position(),
            "host and workload must agree on the cursor after the walk"
        );
        assert_eq!(
            host.screen().cursor_position().0 + 1,
            23,
            "the cursor must stay on the workload's last row, not walk onto row 24"
        );
        assert_eq!(
            host.screen()
                .contents_between(23, 0, 23, 80)
                .trim_end()
                .to_string(),
            "BAR-TEXT",
            "the reserved row must not be written over"
        );
    }

    /// The rewrite must engage only where it is needed: no sub-range, or a
    /// sub-range that already covers the workload's last row, means the host
    /// and the workload scroll identically and the chunk goes out untouched.
    #[test]
    fn relay_leaves_ordinary_streams_untouched() {
        let mut client = ClientScreen::try_new(23, 80).unwrap();
        assert!(client.relay(b"\x1b[23;1Hline\nmore\n").is_none());
        // A bottom-anchored sub-range: row 23 is inside it, so a line feed
        // scrolls on both sides and needs no repair.
        assert!(client.relay(b"\x1b[5;23r").is_none());
        assert!(client.relay(b"\x1b[23;1Hline\n").is_none());
        // A sub-range that excludes the last row, but no line feed in the
        // chunk: still nothing to repair.
        assert!(client
            .relay(b"\x1b[5;15r\x1b[10;1Hno newline here")
            .is_none());
    }

    /// `cursor_restore` is the replacement for the `\x1b7`/`\x1b8` bracket, so
    /// it must actually restore -- position, pending wrap, and the pen -- and
    /// it must never emit DECSC/DECRC itself.
    #[test]
    fn cursor_restore_reproduces_position_and_pen_without_decsc() {
        let mut client = ClientScreen::try_new(23, 80).unwrap();
        client.feed(b"\x1b[7;13H\x1b[1m\x1b[38;5;42m");
        let restore = client.cursor_restore();
        assert!(
            !restore.windows(2).any(|w| w == b"\x1b7" || w == b"\x1b8"),
            "restore must not touch the shared save-cursor register: {restore:?}"
        );
        let mut host = vt100::Parser::new(23, 80, 0);
        host.process(b"\x1b[1;1H\x1b[0m");
        host.process(&restore);
        assert_eq!(host.screen().cursor_position(), (6, 12));
        host.process(b"X");
        let cell = host.screen().cell(6, 12).unwrap();
        assert!(cell.bold(), "the workload's pen must be restored");
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(42));
    }

    /// The three pieces `relay` sits between, all real: the client's
    /// `ClientScreen` at the workload's geometry, a real `vt100` host at the
    /// *physical* geometry (one row taller) carrying the client's
    /// `1;{rows-1}` reservation and its status-bar text, and a second real
    /// `vt100` standing in for the workload's own screen. Bytes are fed
    /// exactly the way `relay_to_terminal` (src/bin/a.rs) feeds them: the
    /// workload sees the raw chunk, the host sees whatever `relay` returns.
    struct RelayRig {
        client: ClientScreen,
        host: vt100::Parser,
        workload: vt100::Parser,
        rows: u16,
        cols: u16,
    }

    impl RelayRig {
        /// `rows` is the *physical* terminal height; the workload gets
        /// `rows - 1` (`reserved_rows` in src/bin/a.rs).
        fn new(rows: u16, cols: u16) -> Self {
            let mut host = vt100::Parser::new(rows, cols, 0);
            // What `apply_terminal_layout` + the first `draw_status_bar`
            // leave on the host before a single workload byte is relayed.
            host.process(format!("\x1b[{rows};1HBAR\x1b[1;{}r\x1b[1;1H", rows - 1).as_bytes());
            Self {
                client: ClientScreen::try_new(rows - 1, cols).unwrap(),
                host,
                workload: vt100::Parser::new(rows - 1, cols, 0),
                rows,
                cols,
            }
        }

        /// Returns whether `relay` rewrote the chunk.
        fn relay(&mut self, data: &[u8]) -> bool {
            self.workload.process(data);
            let rewritten = self.client.relay(data);
            self.host.process(rewritten.as_deref().unwrap_or(data));
            rewritten.is_some()
        }

        /// The host and the workload must agree on the cursor and on every
        /// row the workload owns -- a one-row offset shows up here as both.
        fn assert_agrees(&self, what: &str) {
            assert_eq!(
                self.host.screen().cursor_position(),
                self.workload.screen().cursor_position(),
                "host and workload must agree on the cursor {what}"
            );
            for row in 0..self.rows - 1 {
                assert_eq!(
                    self.host.screen().contents_between(row, 0, row, self.cols),
                    self.workload
                        .screen()
                        .contents_between(row, 0, row, self.cols),
                    "host and workload must agree on row {} {what}",
                    row + 1
                );
            }
        }

        fn assert_bar_intact(&self, what: &str) {
            assert_eq!(
                self.host
                    .screen()
                    .contents_between(self.rows - 1, 0, self.rows - 1, self.cols)
                    .trim_end(),
                "BAR",
                "the reserved row must not be written over or scrolled {what}"
            );
        }
    }

    /// docs/terminal-state-design.md section 7.1's *residual* case, which the
    /// doc recorded as merely self-healing: a wrap off the last column of the
    /// workload's last row while a sub-range excludes that row. The model
    /// clamps and wraps onto the same row; a host one row taller wraps onto
    /// the reserved row and stays a row low forever after. The repair is
    /// spliced *before* the wrapping character, so the character lands on the
    /// right row too and the two grids stay byte-identical.
    #[test]
    fn relay_keeps_a_wrap_off_the_reserved_row_under_a_sub_range() {
        let mut rig = RelayRig::new(24, 20);
        rig.relay(b"\x1b[5;15r");
        // Exactly one screen width, so the cursor ends in pending wrap on the
        // workload's last row -- which is outside its own scroll region.
        rig.relay(b"\x1b[23;1HABCDEFGHIJKLMNOPQRST");
        assert!(
            rig.relay(b"X"),
            "a wrap off the last row under a sub-range must be repaired"
        );
        rig.assert_agrees("after a wrap off the workload's last row");
        assert_eq!(
            rig.workload.screen().cursor_position(),
            (22, 1),
            "the workload wrapped onto its own last row, not off it"
        );
        rig.assert_bar_intact("by a wrap");
    }

    /// The same wrap, but the character that trips it is multi-byte and
    /// arrives in its own chunk: the pending-wrap state has to survive the
    /// chunk boundary, and the guard must fire once, not once per UTF-8 byte.
    #[test]
    fn relay_keeps_a_multibyte_wrap_off_the_reserved_row_across_chunks() {
        let mut rig = RelayRig::new(24, 20);
        // Split the DECSTBM across chunks too, to pin that `run_len` picks up
        // a half-parsed sequence rather than restarting it.
        rig.relay(b"\x1b[5;1");
        rig.relay(b"5r\x1b[23;1HABCDEFGHIJKLMNOPQRST");
        assert!(
            rig.relay("\u{e9}".as_bytes()),
            "a multi-byte wrap off the last row must be repaired"
        );
        rig.assert_agrees("after a multi-byte wrap off the workload's last row");
        rig.assert_bar_intact("by a multi-byte wrap");
    }

    /// The reported user-visible symptom, end to end: after a wrap has put
    /// the host a row low, Ink's next partial repaint paints words at
    /// absolute columns (`\x1b[2GQuick\x1b[8Gsafety`) and welds them into
    /// whatever the wrong row already held. With the repair the two grids
    /// stay identical, which is what stops the welding.
    #[test]
    fn relay_stops_column_addressed_repaints_welding_onto_the_wrong_row() {
        let mut rig = RelayRig::new(24, 20);
        rig.relay(b"\x1b[5;15r");
        rig.relay(b"\x1b[22;1Hprevious frame");
        rig.relay(b"\x1b[23;1HABCDEFGHIJKLMNOPQRST");
        rig.relay(b"X");
        rig.relay(b"\x1b[2GQuick\x1b[8Gsafe");
        rig.assert_agrees("after a column-addressed repaint following a wrap");
        assert_eq!(
            rig.host
                .screen()
                .contents_between(21, 0, 21, 20)
                .trim_end()
                .to_string(),
            "previous frame",
            "the previous frame's row must not be welded into"
        );
        rig.assert_bar_intact("by a column-addressed repaint");
    }

    /// A downward *cursor move* off the last row under a sub-range is the
    /// same clamp mismatch as the line feed: the model has nowhere to go, the
    /// host has the reserved row.
    #[test]
    fn relay_keeps_a_cursor_down_off_the_reserved_row_under_a_sub_range() {
        for probe in [&b"\x1b[B"[..], &b"\x1b[E"[..], &b"\x1b[3e"[..]] {
            let mut rig = RelayRig::new(24, 20);
            rig.relay(b"\x1b[5;15r\x1b[23;1HX");
            assert!(
                rig.relay(probe),
                "{probe:?} off the last row under a sub-range must be repaired"
            );
            rig.assert_agrees("after a downward cursor move off the last row");
            rig.assert_bar_intact("by a downward cursor move");
        }
    }

    /// Mechanism 3: Claude Code opens with `ESC 7`, `ESC [ r`, `ESC 8`. The
    /// bare `ESC [ r` widens the *host's* scroll region back over the
    /// reserved row, and the client's own re-assert is a socket round-trip
    /// away (the worker's `Layout` event), so everything in between walks --
    /// and, unlike the sub-range case, the host also fails to scroll where
    /// the workload does. `relay` re-asserts the reservation in the stream,
    /// right behind the sequence that reset it.
    #[test]
    fn relay_reasserts_the_reservation_when_the_workload_resets_decstbm() {
        let mut rig = RelayRig::new(24, 20);
        assert!(
            rig.relay(b"\x1b7\x1b[r\x1b8"),
            "a workload margin reset must re-assert the client's reservation"
        );
        // Fill the top row so a scroll is observable, then line-feed off the
        // workload's last row: both sides must scroll rows 1..23 together.
        rig.relay(b"\x1b[1;1Htop row\x1b[23;1Hbottom row\n");
        rig.assert_agrees("after a line feed following a margin reset");
        assert_eq!(
            rig.host.screen().contents_between(0, 0, 0, 20).trim_end(),
            "",
            "the host must have scrolled the top row away like the workload did"
        );
        rig.assert_bar_intact("by a line feed after a margin reset");
    }

    /// The same window, reached by a wrap instead of a line feed, and by
    /// `ESC c` (RIS) instead of `ESC [ r`.
    #[test]
    fn relay_reasserts_the_reservation_after_ris_and_survives_a_wrap() {
        let mut rig = RelayRig::new(24, 20);
        assert!(
            rig.relay(b"\x1bc"),
            "RIS must re-assert the client's reservation"
        );
        rig.relay(b"\x1b[23;1HABCDEFGHIJKLMNOPQRST");
        rig.relay(b"X");
        rig.assert_agrees("after a wrap following RIS");
        // No `assert_bar_intact` here on purpose: RIS clears the *whole*
        // host screen, the reserved row included, exactly like the ED2
        // residue design doc section 7 records. That is the status bar's
        // next redraw to repaint; what matters here is that the cursor and
        // the workload's own rows still line up afterwards.
    }

    /// The audited case that turns out **not** to be reachable, recorded as
    /// evidence rather than as a guess: with the client's own `1;{rows-1}`
    /// reservation in force -- which is what the margin-reset re-assert above
    /// now guarantees whenever the workload is on full-screen margins -- the
    /// workload's last row *is* the bottom of the host's scroll region, so a
    /// line feed, a wrap and every downward cursor move scroll or clamp
    /// identically on both sides. Nothing is rewritten and nothing diverges.
    #[test]
    fn relay_client_reservation_never_walks_onto_the_reserved_row() {
        for probe in [
            &b"\n"[..],
            &b"\x0b"[..],
            &b"\x0c"[..],
            &b"\x1b[B"[..],
            &b"\x1b[E"[..],
            &b"\x1b[3e"[..],
            &b"WRAPS-OFF-THE-FAR-END"[..],
        ] {
            let mut rig = RelayRig::new(24, 20);
            rig.relay(b"\x1b[23;1HX");
            assert!(
                !rig.relay(probe),
                "{probe:?} under the client's own reservation needs no repair"
            );
            rig.assert_agrees("under the client's own reservation");
            rig.assert_bar_intact("under the client's own reservation");
        }
    }

    /// A bottom-anchored sub-range covers the workload's last row, so that
    /// row is inside the region on both sides and nothing can walk.
    #[test]
    fn relay_leaves_a_bottom_anchored_sub_range_alone() {
        let mut rig = RelayRig::new(24, 20);
        rig.relay(b"\x1b[5;23r");
        assert!(!rig.relay(b"\x1b[23;1HABCDEFGHIJKLMNOPQRST"));
        assert!(!rig.relay(b"X"));
        assert!(!rig.relay(b"\n"));
        rig.assert_agrees("under a bottom-anchored sub-range");
        rig.assert_bar_intact("under a bottom-anchored sub-range");
    }

    /// The throughput path must stay a single bulk parse: an escape-free
    /// chunk arriving on a stream that is between sequences cannot diverge,
    /// however many line feeds and wraps it contains.
    #[test]
    fn relay_bulk_escape_free_output_is_never_rewritten() {
        let mut client = ClientScreen::try_new(23, 20).unwrap();
        let bulk: Vec<u8> = (0..200)
            .flat_map(|i| format!("line {i} with enough text to wrap the row\n").into_bytes())
            .collect();
        assert!(client.relay(&bulk).is_none());
        // ...and it still tracks: the model has scrolled to the last row.
        assert_eq!(client.cursor_position().0, 22);
    }
}
