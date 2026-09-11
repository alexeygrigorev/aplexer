use anyhow::Result;

use super::boundary::Sequence;
use super::client::without_scroll_regions;
use super::margins::MarginTracker;
use super::validate_size;

/// Seed-replay piece cap: the longest stretch replayed between cuts when
/// the workload sends no synchronized-output frames to split on. Detection
/// groups (`SEED_CAPTURE_MIN_GROUP_BYTES`) are assembled from these pieces.
const SEED_SEGMENT_MAX_BYTES: usize = 64 * 1024;

/// `CSI ? 2026 l` -- the end of a synchronized-output frame. Agent TUIs
/// wrap every repaint in one, so frame ends are the natural cut points for
/// the seed's detection groups: no group boundary can land mid-repaint
/// unless the workload sends no frames at all (then the piece cap cuts).
const SYNC_FRAME_END: &[u8] = b"\x1b[?2026l";

/// Repaint-displacement checks run on *groups* of consecutive segments
/// totaling at least this many bytes, never on the segments alone. The
/// check costs a before/after snapshot of every row's text, so it must not
/// run per synchronized frame -- and it would not see a scroll there
/// anyway: a streaming agent TUI repaints its transcript a few rows per
/// frame (a real main-2 session: 7,120 of 7,143 frames under 512 bytes,
/// median 208), so one scroll event is spread across a dozen frames and
/// only the net shift over a group of them reads as a shift. Grouped at
/// this size the same session yields a few hundred checks, each covering a
/// few scrolled rows; spinner-only groups change one or two rows and never
/// match the shift shape. A group is still cut at `SEED_SEGMENT_MAX_BYTES`.
const SEED_CAPTURE_MIN_GROUP_BYTES: usize = 4096;

/// The most entering rows one detected shift may claim. A streaming reply
/// scrolls several transcript lines per ~4 KiB detection group; more than
/// this many reads as a full-screen relayout, which is not a scroll and
/// must not be mistaken for one.
const SEED_MAX_ENTERING_ROWS: usize = 24;

/// Grid-top rows a pre-scroll has already carried into the history, so a
/// repaint that displaces the same content again and again (a transcript
/// toggling between two layouts) scrolls in once. Bounded and deliberately
/// lossy: the same row text repeating after this many distinct retained
/// rows scrolls in again, which is the honest outcome, not deduplication
/// of history.
const SEED_DISPLACED_DEDUP_CAP: usize = 1024;

/// How far a shift may miss before it is not a shift: at most one changed
/// row may break the alignment for every four that keep it. The misses are
/// the TUI's non-transcript chrome repainting in the same frame as the
/// transcript scrolls -- a spinner cell, the composer line -- and the ratio
/// keeps a genuine scroll with a couple of chrome rows detected while a
/// full-screen relayout (where most rows change to unrelated content) is
/// not.
const SEED_SHIFT_MISS_DENOMINATOR: usize = 4;

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Whether the row-text diff `before -> after` reads as the grid content
/// having scrolled up by some rows, and if so `(topmost changed row, how
/// many rows entered at the bottom)`: `before[top..top + entering]` are
/// the rows the repaint overwrote, and the caller pre-scrolls the real
/// parser by exactly that many rows before the group's bytes so they are
/// what the pre-scroll carries into the retained history.
///
/// The test: the changed rows between the topmost and bottommost difference
/// must align against themselves `entering` rows up -- `after[r] ==
/// before[r + entering]` for (almost) every changed row except the
/// `entering` at the span's bottom, which hold new content by definition.
/// That shape is what a transcript scroll leaves behind no matter how the
/// TUI paints it (ratatui diffs arrive as absolute-addressed row rewrites,
/// but the row contents still move up); it is what a spinner frame, a
/// composer edit or a cursor-blink cell cannot produce (one or two changed
/// rows, nothing to align), and what a full-screen relayout fails (few
/// aligned pairs, many misses).
fn seed_repaint_shift(before: &[String], after: &[String]) -> Option<(usize, usize)> {
    debug_assert_eq!(before.len(), after.len());
    let changed: Vec<usize> = before
        .iter()
        .zip(after.iter())
        .enumerate()
        .filter_map(|(row, (old, new))| (old != new).then_some(row))
        .collect();
    // A scroll moves every row in its span; a single rewritten row is an
    // edit or an animation cell, and one row has nothing to displace.
    if changed.len() < 2 {
        return None;
    }
    let top = changed[0];
    let span_end = changed[changed.len() - 1] + 1;
    let max_entering = (span_end - top - 1).min(SEED_MAX_ENTERING_ROWS);
    for entering in 1..=max_entering {
        let mut aligned = 0usize;
        let mut missed = 0usize;
        for &row in &changed {
            if row + entering >= span_end {
                continue;
            }
            if after[row] == before[row + entering] {
                aligned += 1;
            } else {
                missed += 1;
            }
        }
        if aligned >= 1 && missed * SEED_SHIFT_MISS_DENOMINATOR <= aligned {
            return Some((top, entering));
        }
    }
    None
}

/// Walk `segment` in chunks cut at `\n`, handing each chunk before the next
/// line feed (then the trailing remainder) to `sink`; `at_line_feed` says
/// whether a `\n` follows the chunk, so the caller can run the seed's
/// per-line-feed region synthesis between the chunk and nothing else.
fn for_each_line_feed_chunk(segment: &[u8], mut sink: impl FnMut(&[u8], bool)) {
    let mut rest = segment;
    loop {
        match rest.iter().position(|&byte| byte == b'\n') {
            Some(at) => {
                sink(&rest[..at], true);
                rest = &rest[at + 1..];
            }
            None => {
                sink(rest, false);
                return;
            }
        }
    }
}

/// Replay one seed segment into a bare parser -- the probe's side of
/// `ScreenTracker::seed_feed_segment`: same walk, same region synthesis, no
/// margin scanner or flip tracking, because a probe decision never leaves
/// the seed.
fn seed_feed_parser(
    parser: &mut vt100::Parser,
    segment: &[u8],
    mirror: &mut MarginTracker,
    rows: u16,
) {
    for_each_line_feed_chunk(segment, |chunk, at_line_feed| {
        mirror.scan(chunk);
        let clean = without_scroll_regions(chunk);
        parser.process(&clean);
        if at_line_feed {
            if let Some((1, bottom)) = mirror.margins() {
                if bottom < rows {
                    let (row, col) = parser.screen().cursor_position();
                    if row == bottom - 1 {
                        let synth = format!("\x1b[{rows};1H\n\x1b[{};{}H", row + 1, col + 1);
                        parser.process(synth.as_bytes());
                        return;
                    }
                }
            }
            parser.process(b"\n");
        }
    });
}

fn parser_row_texts(parser: &vt100::Parser) -> Vec<String> {
    let (_, cols) = parser.screen().size();
    parser.screen().rows(0, cols).collect()
}

/// How many rows a parser retains right now -- the clamped-read dance of
/// `ScreenTracker::scrollback_available` in helper form.
fn parser_retained_scrollback(parser: &mut vt100::Parser) -> usize {
    let screen = parser.screen_mut();
    screen.set_scrollback(usize::MAX);
    let retained = screen.scrollback();
    screen.set_scrollback(0);
    retained
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
/// the one gap in what `vt100` exposes -- and whose scanner doubles as the
/// stream's boundary tracker (`at_escape_boundary`), so every chunk is
/// walked once besides the parse. See design doc sections 4-6.
pub struct ScreenTracker {
    parser: vt100::Parser,
    margins: MarginTracker,
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

    #[cfg(test)]
    pub(super) fn parser(&self) -> &vt100::Parser {
        &self.parser
    }

    /// Feed PTY bytes; returns `Some(LayoutChange)` when the workload did
    /// something the attached client must react to.
    pub fn process(&mut self, data: &[u8]) -> Option<LayoutChange> {
        self.process_with(data, |_| {})
    }

    /// `process`, also handing every completed `ESC`/`CSI` sequence to
    /// `observer` (`ClientScreen::relay` asks what a run *was* after seeing
    /// what it did to the cursor).
    pub(crate) fn process_with(
        &mut self,
        data: &[u8],
        observer: impl FnMut(Sequence<'_>),
    ) -> Option<LayoutChange> {
        let csi = self.margins.scan_with(data, observer);
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

    /// Replay raw history bytes for their *history* only --
    /// `ClientScreen::seed_history`'s inner loop. `process` with the layout
    /// change dropped: the seed replays a margin-stripped view of the tail
    /// (`without_scroll_regions`) and the caller resets the margins
    /// afterwards, so there is nothing to report and nobody to report it to.
    ///
    /// The stripping alone cannot recover a region-holding workload's
    /// transcript, though. An agent TUI grows its transcript by scrolling a
    /// DECSTBM sub-range -- in the byte log that is one `\n` while the cursor
    /// sits at the region's bottom row, followed by an absolute reposition
    /// and the new row's text. Replayed without margins, that `\n` merely
    /// walks the cursor down; every transcript row lands on the same grid
    /// row and overwrites its predecessor, so a session with an hour of
    /// output seeds a pager with one screenful. A real terminal at that
    /// `\n` scrolls the region and discards the region-top line -- and the
    /// discarding is the one thing a *throwaway* grid must not do. So each
    /// `\n` is replayed against the region the raw stream had in force
    /// (mirrored through a private `MarginTracker` over the unstripped
    /// bytes): when it would have scrolled a region anchored at the grid's
    /// top, the feed synthesizes a full-grid scroll instead, which carries
    /// that top line into the retained history the pager pages through.
    /// Rows below the region shift up by one against what the workload
    /// meant, which is acceptable exactly because this grid is throwaway:
    /// region-relative painting continues from the restored cursor, a
    /// region's own rows move as the workload intended, the rows below it
    /// are the constantly-repainted status/input area, and the caller
    /// overwrites the whole grid with the live snapshot when the seed ends.
    ///
    /// Regions anchored below the grid top are left alone: a full-grid
    /// scroll would push unrelated row-one content into the history without
    /// saving the region's top line, and those regions belong to transient
    /// panels rather than to a transcript.
    ///
    /// There is a second class of lost transcript that neither stripping nor
    /// the region synthesis can see, because the grid never scrolls at all:
    /// a diff-rendering TUI (ratatui-based agents, codex among them) grows
    /// its transcript by *repainting rows in place* -- a real main-2 session
    /// logged 1.8 MiB with 45,231 absolute cursor addresses and 113 line
    /// feeds. Nothing was ever scrolled off anything; each new transcript
    /// line was written over an old one, and both the live parse and every
    /// replay of the log agree on the one screenful that survives. Those
    /// overwritten rows are the bulk of what the user watched scroll by.
    ///
    /// Recovering them needs the row the repaint is *about to overwrite* to
    /// be off the grid while its replacement is painted. The only lever
    /// `vt100` offers is a real grid scroll -- the one thing that moves a row
    /// into the retained history -- so the seed replays in two passes. Pass
    /// one replays the stripped tail on a throwaway probe parser and, per
    /// detection group (`SEED_CAPTURE_MIN_GROUP_BYTES`), compares the row
    /// texts on each side of it: when the changed rows read as a vertical
    /// shift (`seed_repaint_shift`), the group is a repaint scroll and the
    /// plan records how many rows it displaced. Pass two replays into the
    /// real parser, and before each planned group scrolls the grid by
    /// exactly that many rows: the displaced rows enter the retained
    /// history natively, in order, with no fragments -- and the group's own
    /// shifted rows then land precisely where the pre-scroll put the grid,
    /// because that is what the shift means. Spinner frames, cursor-blink
    /// cells and composer edits are single-row changes and never match the
    /// shift shape, so animation floods nothing; a group that already
    /// scrolled natively (shell output, the region synthesis, `CSI S`)
    /// retains its rows on the probe and is left alone, so nothing is
    /// captured twice; the alternate screen has no history anywhere, so it
    /// is skipped too. Rows a repeated repaint displaces again and again
    /// scroll in once (`SEED_DISPLACED_DEDUP_CAP`). Measured A/B over
    /// twelve real retained tails (release build): the capture adds rows on
    /// two of them (+27 on a mixed paint/scroll session, +5 on another) and
    /// none on the rest -- the codex TUI scrolls its transcript through
    /// DECSTBM region scrolls, which stripping and the `\n` synthesis
    /// already recover -- for ~40% more seed time (~30 ms per dense MiB).
    /// It stays because the region and line-feed paths have no answer for a
    /// TUI that repaint-shifts, and the shift test is what pins that case.
    ///
    /// The cost is a second parse of the tail, paid only on the attach-seed
    /// and pager-rebuild paths, which already exist to spend parse time on
    /// history quality. The probe parser's grid trails the real one by at
    /// most the planned pre-scrolls on rows a frame did not repaint --
    /// enough for shift detection, which is content- not position-keyed --
    /// and both grids are discarded by the caller's trailing snapshot.
    pub fn seed(&mut self, data: &[u8]) {
        let rows = self.rows();
        let cols = self.cols();
        // Detection groups: byte ranges ending at synchronized-output frame
        // ends (or the cap, for workloads that send no frames), each at
        // least `SEED_CAPTURE_MIN_GROUP_BYTES` unless the stream ends first.
        let mut segments: Vec<&[u8]> = Vec::new();
        let next_cut = |start: usize| {
            let window_end = (start + SEED_SEGMENT_MAX_BYTES).min(data.len());
            match find_subslice(&data[start..window_end], SYNC_FRAME_END) {
                Some(at) => start + at + SYNC_FRAME_END.len(),
                None => window_end,
            }
        };
        let mut start = 0usize;
        while start < data.len() {
            let group_start = start;
            let mut end = next_cut(start);
            while end - group_start < SEED_CAPTURE_MIN_GROUP_BYTES && end < data.len() {
                start = end;
                end = next_cut(start);
            }
            segments.push(&data[group_start..end]);
            start = end;
        }

        // Pass one: plan the pre-scrolls on a probe parser, never the real
        // one -- the decision needs the group's after-state, which cannot be
        // taken on the parser that has to receive the pre-scroll first.
        let mut plan = Vec::with_capacity(segments.len());
        let mut probe = vt100::Parser::new(rows, cols, self.scrollback_capacity());
        let mut probe_mirror = MarginTracker::new(rows);
        for segment in &segments {
            let entering = if segment.len() >= SEED_CAPTURE_MIN_GROUP_BYTES
                && !probe.screen().alternate_screen()
            {
                let retained_before = parser_retained_scrollback(&mut probe);
                let before = parser_row_texts(&probe);
                seed_feed_parser(&mut probe, segment, &mut probe_mirror, rows);
                let scrolled_natively = parser_retained_scrollback(&mut probe) > retained_before;
                (!scrolled_natively)
                    .then(|| seed_repaint_shift(&before, &parser_row_texts(&probe)))
                    .and_then(|shift| shift)
                    .map_or(0, |(_, entering)| entering)
            } else {
                seed_feed_parser(&mut probe, segment, &mut probe_mirror, rows);
                0
            };
            plan.push(entering);
        }

        // Pass two: the real feed. A planned group first scrolls its
        // displaced rows into history (deduped against what earlier
        // pre-scrolls already retained), then replays normally on top.
        let mut mirror = MarginTracker::new(rows);
        let mut displaced_seen: std::collections::HashSet<Vec<u8>> =
            std::collections::HashSet::new();
        for (segment, &entering) in segments.iter().zip(&plan) {
            if entering > 0 && !self.alternate_screen() {
                let leaving: Vec<String> = self.row_texts().into_iter().take(entering).collect();
                let all_seen = leaving
                    .iter()
                    .all(|text| text.trim().is_empty() || displaced_seen.contains(text.as_bytes()));
                if !all_seen {
                    for text in &leaving {
                        if text.trim().is_empty() {
                            continue;
                        }
                        if displaced_seen.len() >= SEED_DISPLACED_DEDUP_CAP {
                            displaced_seen.clear();
                        }
                        displaced_seen.insert(text.as_bytes().to_vec());
                    }
                    self.prescroll(entering);
                }
            }
            self.seed_feed_segment(segment, &mut mirror, rows);
        }
    }

    /// Scroll the grid itself `count` rows into the retained history,
    /// before feeding bytes that will repaint over them. Synthetic bytes:
    /// parser-direct, so the margin/boundary state that belongs to the
    /// *stream* never sees them.
    fn prescroll(&mut self, count: usize) {
        let mut seq = format!("\x1b[{};1H", self.rows()).into_bytes();
        seq.extend(std::iter::repeat_n(b'\n', count));
        self.parser.process(&seq);
    }

    /// Replay one seed segment into this tracker: the margin-stripped view
    /// goes to the parse, the raw bytes to the region mirror, and each `\n`
    /// gets the region-synthesis treatment (`seed_line_feed`).
    fn seed_feed_segment(&mut self, segment: &[u8], mirror: &mut MarginTracker, rows: u16) {
        for_each_line_feed_chunk(segment, |chunk, at_line_feed| {
            mirror.scan(chunk);
            let clean = without_scroll_regions(chunk);
            self.process(&clean);
            if at_line_feed {
                self.seed_line_feed(mirror, rows);
            }
        });
    }

    /// The text of every visible row, as `Screen::rows` sees it at offset 0
    /// -- the resting state throughout a seed.
    fn row_texts(&self) -> Vec<String> {
        parser_row_texts(&self.parser)
    }

    /// Replay one `\n` of a seed: a full-grid scroll when the raw stream's
    /// region says a real terminal would have scrolled its sub-range here
    /// (see `seed`), the bare line feed otherwise.
    fn seed_line_feed(&mut self, mirror: &mut MarginTracker, rows: u16) {
        if let Some((1, bottom)) = mirror.margins() {
            if bottom < rows {
                let (row, col) = self.cursor_position();
                if row == bottom - 1 {
                    let synth = format!("\x1b[{rows};1H\n\x1b[{};{}H", row + 1, col + 1);
                    self.process(synth.as_bytes());
                    return;
                }
            }
        }
        self.process(b"\n");
    }

    /// Resizes the grid (content-preserving) and re-fits the tracked margins
    /// the way the grid re-fits its own scroll region
    /// (`MarginTracker::set_rows`).
    ///
    /// One departure from `vt100` toward a real terminal: on a shrink that
    /// would push the cursor's row off the bottom, `Grid::set_size`
    /// truncates rows from the bottom -- the newest line falls off and the
    /// cursor clamps onto the row above, where the workload's WINCH repaint
    /// then overwrites it (an idle shell visibly lost its last output line
    /// on every attach, and every later snapshot carried the loss). xterm
    /// scrolls the content up by the excess instead, so a synthetic
    /// `CSI n S` does the same first -- gated on no DECSTBM sub-range being
    /// in force (a region holder owns its own resize semantics) and on the
    /// stream not being mid-sequence (an SU glued into a half-received
    /// sequence corrupts the parse it was meant to protect). Applied to
    /// whichever grid is active: it is about the cursor's line, not
    /// scrollback, and a skipped or superfluous compensation costs nothing
    /// permanent because SIGWINCH makes the workload repaint anyway.
    pub fn try_set_size(&mut self, rows: u16, cols: u16) -> Result<()> {
        let (rows, cols) = validate_size(rows, cols)?;
        if rows < self.rows() && self.margins.margins().is_none() && self.at_escape_boundary() {
            let (cursor_row, _) = self.cursor_position();
            let excess = i32::from(cursor_row) - i32::from(rows) + 1;
            if excess > 0 {
                let su = format!("\x1b[{excess}S").into_bytes();
                self.margins.scan(&su);
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
    ///    sequence, followed by `cursor_restore` (DECSTBM homes the cursor
    ///    on a real terminal; a plain `CUP` would put it back but cannot
    ///    express a pending wrap, which `cursor_restore` can). Skipped when
    ///    margins are default, leaving the client's own status-bar
    ///    reservation in force.
    pub fn snapshot(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut out = Vec::new();
        if screen.alternate_screen() {
            out.extend_from_slice(b"\x1b[?1049h");
        }
        out.extend_from_slice(&screen.state_formatted());
        if let Some((top, bottom)) = self.margins.margins() {
            out.extend_from_slice(format!("\x1b[{top};{bottom}r").as_bytes());
            out.extend_from_slice(&self.cursor_restore());
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
    /// replays a raw tail, whose trailing state was never this client's:
    /// the tail's mid-sequence ending was the log's, not the live stream's.
    pub fn reset_margins(&mut self) {
        self.margins.reset();
    }

    /// True when the stream this tracker has consumed is between complete
    /// sequences and characters -- `StreamBoundary::at_escape_boundary`.
    pub fn at_escape_boundary(&self) -> bool {
        self.margins.boundary().at_escape_boundary()
    }

    /// True while the workload has an unclosed `CSI ? 2026 h` frame --
    /// `StreamBoundary::in_synchronized_update`.
    pub fn in_synchronized_update(&self) -> bool {
        self.margins.boundary().in_synchronized_update()
    }

    /// How many leading bytes of `data` the stream needs before it is back
    /// at a boundary -- `StreamBoundary::bytes_to_ground`.
    pub fn bytes_to_ground(&self, data: &[u8]) -> usize {
        self.margins.boundary().bytes_to_ground(data)
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
