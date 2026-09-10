use anyhow::Result;

use super::boundary::Sequence;
use super::margins::MarginTracker;
use super::validate_size;

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

    /// Replay bytes for their *history* only -- `ClientScreen::seed_history`'s
    /// inner loop. `process` with the layout change dropped: the seed strips
    /// every DECSTBM out of the tail first (`without_scroll_regions`) and
    /// resets the margins afterwards, so there is nothing to report and
    /// nobody to report it to.
    pub fn seed(&mut self, data: &[u8]) {
        self.process(data);
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
