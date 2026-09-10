use super::boundary::{Sequence, StreamBoundary};

/// Recovers the one piece of terminal state `vt100::Screen` parses during
/// `process()` but neither exposes nor re-emits in `state_formatted()`: the
/// current DECSTBM scroll region (docs/terminal-state-design.md section
/// 5.4).
///
/// Built on `StreamBoundary`'s scanner (state lives in `self`, so a
/// sequence split across two PTY reads is handled by construction) and
/// interprets exactly three of the sequences it reports, passing everything
/// else through unexamined:
///
/// - `ESC c` (RIS): margins reset to full-screen, reported as
///   `margins_reset`.
/// - `ESC [ params r` with no private marker (`?`/`<`/`=`/`>`) and no
///   intermediate byte: DECSTBM, canonicalized exactly as vt100 0.16 does
///   (`perform.rs::canonicalize_params_decstbm` and
///   `grid.rs::set_scroll_region`): the first two `;`-separated values,
///   digits before any `:` only, `0` or empty meaning the default (`1` /
///   `rows`), bottom clamped to `rows`. A result with `top < bottom` short of
///   the whole screen is stored as a sub-range and *not* reported, because
///   the client re-asserts a sub-range rather than replacing it (design doc
///   section 7). Everything else -- empty, full-range, `top >= bottom` -- is
///   the full screen and is reported. An out-of-range DECSTBM is therefore a
///   reset rather than a no-op: the grid beside this tracker treats it as
///   one, and `ScreenTracker::snapshot` pairs the grid's contents with
///   *these* margins, so the two must agree.
/// - `ESC [ ... J` (Erase in Display), any parameter and any private marker:
///   reported as `erase` unconditionally. ED ignores scroll margins, so even
///   a DECSTBM sub-range does not protect the client's reserved bottom row,
///   and Ink-based TUIs send `2J` on nearly every redraw. Over-triggering
///   costs one harmless status-bar redraw; under-triggering leaves the bar
///   wiped until the next timer tick.
#[derive(Debug, Clone)]
pub struct MarginTracker {
    rows: u16,
    /// The scanner this tracker reads `RIS`/`DECSTBM`/`ED` out of -- and,
    /// through `boundary()`, the stream's boundary state for everyone else,
    /// so a single pass over the bytes serves both.
    scanner: StreamBoundary,
    /// Current scroll region as *tracked*, 1-based inclusive `(top, bottom)`;
    /// `None` is full-screen. Can hold the one-row region a resize collapses
    /// onto (`top == bottom`), which `margins()` filters out at emission.
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
            scanner: StreamBoundary::new(),
            region: None,
            subregion_seen: false,
        }
    }

    /// The stream's boundary state, as of the last byte scanned.
    pub fn boundary(&self) -> &StreamBoundary {
        &self.scanner
    }

    /// The current scroll region as it should be **emitted**: a proper
    /// sub-range, or `None` for "no sub-range -- leave the default, or the
    /// client's own status-bar reservation, in force".
    ///
    /// Filters out the one-row region `set_rows` can leave behind: it is not
    /// expressible as a DECSTBM (`top < bottom` is required, here and in
    /// vt100), so emitting it would be ignored by the host and leave a stale
    /// region in force -- worse than saying nothing.
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
    pub(super) fn tracked_region(&self) -> Option<(u16, u16)> {
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
        self.scanner.reset();
        self.subregion_seen = false;
    }

    /// Re-fits the tracked region to a new row count exactly the way
    /// `vt100::Screen::set_size` re-fits the grid's own scroll region
    /// (0.16.2 `grid.rs::set_size`) -- because `snapshot()` pairs that grid's
    /// `state_formatted()` with *these* margins and the two must agree --
    /// and not the way a real terminal does (xterm resets margins on resize;
    /// docs/terminal-state-design.md section 5.3 records why that was wrong
    /// here). In order:
    ///
    /// 1. A bottom-anchored region (`bottom == old rows`) follows the screen
    ///    in both directions: `(3,23)` at 23 rows becomes `(3,39)` at 39.
    /// 2. Otherwise a bottom past the new end is clamped to it, top kept.
    /// 3. A top no longer below the clamped bottom degenerates to the full
    ///    screen.
    ///
    /// A region that still fits is untouched. The clamps can leave a one-row
    /// region (`top == bottom == rows`); it is kept, as the grid keeps it, so
    /// a later enlargement grows it back through rule 1 -- `margins()` is
    /// what declines to emit it. An in-flight partial sequence survives the
    /// resize: a DECSTBM split across two PTY reads with a resize between
    /// them is still a DECSTBM. Pinned against the real crate by the
    /// `margin_tracker_resize_*` sweeps.
    pub fn set_rows(&mut self, rows: u16) {
        let rows = rows.max(1);
        let old_rows = self.rows;
        self.rows = rows;
        self.region = self.region.and_then(|(top, bottom)| {
            // Rule 1, else rule 2.
            let bottom = if bottom == old_rows {
                rows
            } else {
                bottom.min(rows)
            };
            // Rule 3 (`top > bottom` can only mean the full screen after the
            // clamps) plus the whole-screen normalization `apply` uses.
            // `top == bottom` is deliberately kept -- see the doc comment.
            (top <= bottom && !(top == 1 && bottom == rows)).then_some((top, bottom))
        });
    }

    /// Feed a chunk of raw PTY bytes. Returns the triggers this chunk
    /// caused that the client should react to (re-assert its own
    /// status-bar reservation) -- see design doc section 7 and `CsiEvent`.
    pub fn scan(&mut self, data: &[u8]) -> CsiEvent {
        self.scan_with(data, |_| {})
    }

    /// `scan`, also handing every completed `ESC`/`CSI` sequence to
    /// `observer` -- the one pass over the bytes, shared.
    pub(crate) fn scan_with(
        &mut self,
        data: &[u8],
        mut observer: impl FnMut(Sequence<'_>),
    ) -> CsiEvent {
        let mut result = CsiEvent::default();
        let Self {
            rows,
            scanner,
            region,
            subregion_seen,
        } = self;
        scanner.feed_with(data, |sequence| {
            observer(sequence);
            let event = Self::apply(*rows, region, subregion_seen, sequence);
            result.margins_reset |= event.margins_reset;
            result.erase |= event.erase;
        });
        result
    }

    /// Fold one complete sequence into the tracked region. Takes the fields
    /// rather than `self` because it runs inside `scanner`'s callback.
    fn apply(
        rows: u16,
        region: &mut Option<(u16, u16)>,
        subregion_seen: &mut bool,
        sequence: Sequence<'_>,
    ) -> CsiEvent {
        let reset = CsiEvent {
            margins_reset: true,
            erase: false,
        };
        let params = match sequence {
            Sequence::Esc(b'c') => {
                *region = None;
                return reset;
            }
            // Erase in Display -- see the type's doc comment: unconditional,
            // whatever the parameter or private marker.
            Sequence::Csi {
                final_byte: b'J', ..
            } => {
                return CsiEvent {
                    margins_reset: false,
                    erase: true,
                }
            }
            Sequence::Csi {
                params,
                final_byte: b'r',
                plain: true,
            } => params,
            _ => return CsiEvent::default(),
        };
        let (top, bottom) = decstbm_params(params, rows);
        // `grid.rs::set_scroll_region`: the bottom is clamped to the screen
        // and only `top < bottom` sets a region; anything else is the full
        // screen (and homes the cursor, which `snapshot` re-fixes).
        let bottom = bottom.min(rows);
        if top < bottom && !(top == 1 && bottom == rows) {
            *region = Some((top, bottom));
            *subregion_seen = true;
            CsiEvent::default()
        } else {
            *region = None;
            reset
        }
    }
}

/// vt100's `canonicalize_params_decstbm`, over the raw parameter bytes: the
/// first two `;`-separated values, each read as the digits before any `:`
/// sub-parameter (vt100 takes a parameter's first sub-parameter), saturating
/// like `vte`'s accumulator; `0` or empty means the default -- `1` for the
/// top, `rows` for the bottom. No clamping here: that is the grid's job.
fn decstbm_params(params: &[u8], rows: u16) -> (u16, u16) {
    let mut values = params.split(|&b| b == b';').map(|param| {
        param
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .fold(0u16, |n, &d| {
                n.saturating_mul(10).saturating_add(u16::from(d - b'0'))
            })
    });
    let top = values.next().unwrap_or(0);
    let bottom = values.next().unwrap_or(0);
    (
        if top == 0 { 1 } else { top },
        if bottom == 0 { rows } else { bottom },
    )
}

/// What a scanned chunk of PTY bytes did that `ScreenTracker::process`
/// should fold into a `LayoutChange` -- see `MarginTracker`'s doc comment
/// for exactly which sequences set which field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CsiEvent {
    pub margins_reset: bool,
    pub erase: bool,
}
