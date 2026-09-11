use anyhow::Result;

use super::boundary::Sequence;
use super::host_alt::HostAltHold;
use super::tracker::ScreenTracker;

/// Strip every DECSTBM (`CSI <digits and ;> r`) out of a raw history tail
/// before it is replayed into the client's model -- what makes `Ctrl-b [`
/// show anything at all in an agent session.
///
/// `vt100` retains a row scrolled off the top only while no scroll region
/// is active (`grid.rs::scroll_up`), which is right for a live pane but not
/// for the seed replay: its grid is thrown away by the reattach snapshot
/// moments later and only its history survives, so a region has nothing to
/// protect there and only discards the transcript
/// (docs/terminal-state-design.md section 7.2 has the measurements).
/// Borrowed, not copied, when the tail holds no DECSTBM -- every plain
/// shell session, on the attach path. `CSI ? Ps r` (XTRESTORE) is a
/// different sequence and survives: anything but digits and `;` in the
/// parameters disqualifies the match.
pub(super) fn without_scroll_regions(data: &[u8]) -> std::borrow::Cow<'_, [u8]> {
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
            i = end;
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

/// The attached client's own copy of the workload's terminal, kept by
/// feeding it the very bytes the client relays to the user's terminal, so
/// the status bar can inject only at a boundary where an injection is
/// invisible (`at_escape_boundary`), put the cursor and pen back absolutely
/// rather than through the shared DECSC register (`cursor_restore`), and
/// keep the host's cursor on the row the workload believes it is on
/// (`relay`). Sized to the workload's geometry -- the physical terminal
/// minus the reserved status row -- so its coordinates are the host's for
/// every row the workload can reach. Unlike the worker's model it retains
/// scrollback, which is what `Ctrl-b [` pages through
/// (docs/terminal-state-design.md section 7.2).
pub struct ClientScreen {
    screen: ScreenTracker,
    /// When set, bytes written to the host have alt-screen DECSET/DECRST
    /// stripped so the attach client can own the host's alternate screen.
    host_alt: Option<HostAltHold>,
    /// Set once `relay` has repositioned the host ahead of a character that
    /// is about to wrap, until that character completes: the guard must not
    /// fire again on its UTF-8 continuation bytes, and if it turns out to be
    /// a zero-width combining mark the host's pending wrap has to be put
    /// back. A field rather than a local because the lead byte can be the
    /// last byte of a PTY read.
    wrap_guarded: bool,
}

/// A chunk on its way to the host, with the client's own bytes spliced in at
/// chosen offsets. Nothing is copied until the first splice, so a chunk that
/// needs none costs no allocation and `finish` reports it as unchanged.
struct Rewrite<'a> {
    data: &'a [u8],
    out: Option<Vec<u8>>,
    /// How much of `data` has already been copied into `out`.
    copied: usize,
}

impl<'a> Rewrite<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            out: None,
            copied: 0,
        }
    }

    /// Copy `data` up to `upto`, then `extra`.
    fn splice(&mut self, upto: usize, extra: &[u8]) {
        let buf = self.out.get_or_insert_with(Vec::new);
        buf.extend_from_slice(&self.data[self.copied..upto]);
        buf.extend_from_slice(extra);
        self.copied = upto;
    }

    /// The rewritten chunk, or `None` when nothing was spliced in.
    fn finish(self) -> Option<Vec<u8>> {
        let mut buf = self.out?;
        buf.extend_from_slice(&self.data[self.copied..]);
        Some(buf)
    }
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
            host_alt: None,
            wrap_guarded: false,
        })
    }

    /// Prime the scrollback grid from a tail of the worker's retained raw
    /// history, before the first live byte and before the reattach
    /// snapshot: a freshly attached client's model is empty, and without
    /// this "attach and scroll up" would show nothing until the workload
    /// produced a screenful. Model-only, and allowed to leave no state
    /// behind: the tail is replayed through `ScreenTracker::seed`, which
    /// strips the tail's scroll regions for the parse but converts each
    /// region scroll a real terminal would have performed into a full-grid
    /// scroll, and pre-scrolls past repaint-driven row overwrites, so a
    /// region-holding or diff-rendering TUI's transcript enters the
    /// retained history instead of overwriting itself row after row; the
    /// epilogue leaves the primary grid, full-screen margins and a default
    /// pen, and the margins and scanner are reset (`reset_margins`) so
    /// neither a DECSTBM or `?1049h` in force at the end of the tail nor
    /// its mid-sequence ending can outlive the seed and contradict the
    /// snapshot fed next.
    pub fn seed_history(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.screen.seed(data);
        self.screen.process(b"\x1b[?1049l\x1b[r\x1b[m");
        self.screen.reset_margins();
        self.wrap_guarded = false;
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
    /// the live screen again -- `seed_history` re-run on a live attach.
    ///
    /// Needed because the live path honors DECSTBM sub-ranges and `vt100`
    /// drops every row that scrolls out of one, so a session attached
    /// *before* its output happened (`a new`, the default) accumulates no
    /// history under a region-holding TUI (docs/terminal-state-design.md
    /// section 7.2). The caller gates on `subregion_seen` and
    /// `!alternate_screen`; an empty tail is rejected here, next to the
    /// invariant it protects, because a seed of zero bytes would replace
    /// whatever the live path retained with nothing. Model-only: nothing
    /// reaches the host. Bytes that arrive between the caller fetching the
    /// snapshot and this swap are missing until the workload's next repaint,
    /// and rows scrolled by in that sliver are missing from the history for
    /// good. The attach pager avoids that sliver by passing *this* client's
    /// own `snapshot()` under the same lock as the swap (`refresh_pager_history`),
    /// so the live grid the exit repaint shows is the one the relay has been
    /// feeding, not a worker photograph from one RPC ago.
    ///
    /// A tail that rebuilds to *less* history than the model already holds
    /// is refused too: the tail is a byte budget out of a log and bytes are
    /// not rows -- an agent idling between turns spends them on a spinner --
    /// so the rebuild is prepared in a scratch model and swapped in only
    /// when it retained at least as much as the live path shows (section
    /// 7.2; tmux never shrinks a pane's history by re-deriving it either).
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
        // The candidate's tracker comes with its own scanner, reset after the
        // seed (the tail can begin mid-escape-sequence, exactly as at attach)
        // and at a boundary after the snapshot. The host-alt hold is
        // untouched: the rebuild wrote nothing to the host, so what the hold
        // tracks is still true. `wrap_guarded` is a live-stream latch for a
        // cursor this grid no longer has.
        self.screen = candidate.screen;
        self.wrap_guarded = false;
    }

    /// `ScreenTracker::scrolled_frame` -- the pager's view of the history,
    /// with the model left resting at offset 0.
    pub fn scrolled_frame(&mut self, offset: usize) -> (Vec<u8>, usize, usize) {
        self.screen.scrolled_frame(offset)
    }

    /// `scrolled_frame`, with the frame passed through `filter_host` --
    /// the bytes to actually write to the host terminal.
    pub fn host_scrolled_frame(&mut self, offset: usize) -> (Vec<u8>, usize, usize) {
        let (frame, offset, available) = self.scrolled_frame(offset);
        let frame = self.filter_host(&frame).unwrap_or(frame);
        (frame, offset, available)
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
    /// pre-attach scrollback), and drop alternateScroll (1007) so a
    /// workload cannot turn the wheel into arrow keys. `None` means `data`
    /// can be written as-is -- the hold is off, or nothing in the chunk
    /// could need rewriting (no `ESC` in it and no sequence held over from
    /// the previous one), which is bulk output and is not copied.
    pub fn filter_host(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        self.host_alt
            .as_mut()
            .filter(|hold| !hold.passes_through(data))
            .map(|hold| hold.push(data))
    }

    /// Feed bytes that are being written to the host terminal *verbatim* --
    /// the attach snapshot and a session switch's replayed screen. These
    /// describe the workload's screen, so the model must see them, but they
    /// are never rewritten (there is no live workload cursor to protect
    /// during a snapshot; the snapshot is itself an absolute repaint).
    pub fn feed(&mut self, data: &[u8]) {
        self.screen.process(data);
    }

    /// Feed a live PTY chunk and return the bytes the client should actually
    /// write, or `None` when the chunk goes out unchanged (the common case).
    ///
    /// The rewrite exists for one class of bug: the host terminal's cursor
    /// ending up on a **different row** from the workload's own model, after
    /// which every column-addressed partial repaint (Ink paints words at
    /// absolute columns) welds onto the wrong row. The host is one row
    /// taller than this model -- the client reserved the bottom row for the
    /// status bar -- and four things walk it onto that row where the model
    /// clamps or scrolls (docs/terminal-state-design.md section 7.2, each
    /// measured against a real `vt100::Parser` at the host's geometry):
    ///
    /// 1. a line feed on the model's last row while a workload sub-range
    ///    excludes that row (`exposed`) -- repaired with an absolute
    ///    reposition spliced in right after it (`clamp_repair`);
    /// 2. a wrap off the last column of that row -- the reposition is
    ///    spliced *before* the wrapping character, so the character lands
    ///    on the right row too (`wrap_would_walk`, `wrap_guarded`);
    /// 3. the workload resetting DECSTBM (`ESC [ r`, RIS), which widens the
    ///    host's region over the reserved row until the client's own
    ///    re-assert comes back around a socket round-trip later, and inside
    ///    that window the host also fails to *scroll* where the model does
    ///    -- so the reservation is re-asserted in the stream, right behind
    ///    the sequence that reset it (`reservation_reassert`);
    /// 4. a relative cursor-down (`CSI B`/`E`/`e`, `ESC E`) on the exposed
    ///    last row -- mechanism 1 by another sequence (`moves_down`).
    ///
    /// With the client's own `1;{rows}` reservation in force -- which 3
    /// guarantees whenever the workload is on full-screen margins -- none of
    /// these can diverge, because the model's last row is the bottom of the
    /// host's region and both sides scroll identically
    /// (`relay_client_reservation_never_walks_onto_the_reserved_row`).
    ///
    /// Cost: a chunk with no `ESC`, arriving between sequences while the last
    /// row is not exposed, is one bulk `process` -- the throughput case.
    /// Otherwise the chunk is walked in runs (`run_len`): a whole escape
    /// sequence per run, or printable text bounded by the columns left
    /// before the far end of the last row, so the model's cursor is current
    /// at every byte that could wrap off it.
    pub fn relay(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        if self.screen.at_escape_boundary() && !self.exposed() && !data.contains(&0x1b) {
            // No `ESC` anywhere and a stream that is between sequences: this
            // chunk cannot set, reset or otherwise move a scroll region, so
            // the host keeps the client's own reservation for all of it and
            // (see above) cannot diverge. One bulk parse, no per-run walk.
            self.screen.process(data);
            return None;
        }

        let mut rewrite = Rewrite::new(data);
        let mut i = 0usize;

        while i < data.len() {
            if !self.wrap_guarded && self.wrap_would_walk(data[i]) {
                // The model will wrap this character to column 1 of the row
                // it is already on (it is outside the sub-range, so it clamps
                // instead of scrolling); the host, one row taller, would wrap
                // it onto the reserved row. Cancel the host's pending wrap by
                // putting it where the model is about to be.
                let row = self.screen.cursor_position().0;
                rewrite.splice(i, format!("\x1b[{};1H", row + 1).as_bytes());
                self.wrap_guarded = true;
            }

            let end = i + self.run_len(data, i);
            let run = &data[i..end];
            let mut moved_down = run.len() == 1 && matches!(run[0], b'\n' | 0x0b | 0x0c);
            let exposed = self.exposed();
            let before_row = self.screen.cursor_position().0;
            let change = self.screen.process_with(run, |sequence| {
                moved_down |= Self::moves_down(sequence);
            });
            i = end;

            let last_row = self.last_row();
            let row = self.screen.cursor_position().0;
            let at_boundary = self.screen.at_escape_boundary();

            // Mechanisms 1 and 4: the model clamped on its last row where the
            // host, one row taller, walked onto the reserved row.
            if exposed && before_row == last_row && row == last_row && moved_down {
                rewrite.splice(i, &self.clamp_repair());
            }

            if self.wrap_guarded && at_boundary {
                if self.pending_wrap_on_last_row() {
                    // The character attached to the preceding cell -- a
                    // zero-width combining mark -- instead of wrapping, so the
                    // model is still in pending wrap and the host is not. Put
                    // the host's pending wrap back; a `CUP` cannot express it,
                    // `cursor_state_formatted` (inside `cursor_restore`) can.
                    rewrite.splice(i, &self.screen.cursor_restore());
                }
                self.wrap_guarded = false;
            }

            // Mechanism 3: close the reservation window in the stream itself.
            if at_boundary && matches!(change, Some(c) if c.margins_reset) {
                if let Some(seq) = self.reservation_reassert() {
                    rewrite.splice(i, &seq);
                }
            }
        }
        rewrite.finish()
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

    /// True while the model's cursor sits past the far end of its last row
    /// -- `vt100` reports the pending-wrap state as a cursor column equal to
    /// the screen width.
    fn pending_wrap_on_last_row(&self) -> bool {
        let (row, col) = self.screen.cursor_position();
        row == self.last_row() && col >= self.screen.cols()
    }

    /// True when `byte` is the start of a character that the model is about
    /// to wrap off the far end of its last row while that row is outside the
    /// scroll region.
    fn wrap_would_walk(&self, byte: u8) -> bool {
        byte >= 0x20
            && byte != 0x7f
            && self.screen.at_escape_boundary()
            && self.exposed()
            && self.pending_wrap_on_last_row()
    }

    /// A sequence that moves the cursor down by a count rather than to an
    /// absolute row -- CUD, CNL, VPR (`CSI B`/`E`/`e`, plain parameters
    /// only) and NEL (`ESC E`). Matched on the parsed shape rather than the
    /// final byte, so `ESC ( B` (a charset designation, in every `tput sgr0`)
    /// is not mistaken for a `CSI B`, and a sequence split across chunks is
    /// still recognized when it completes.
    fn moves_down(sequence: Sequence<'_>) -> bool {
        matches!(
            sequence,
            Sequence::Esc(b'E')
                | Sequence::Csi {
                    final_byte: b'B' | b'E' | b'e',
                    plain: true,
                    ..
                }
        )
    }

    /// The repair for a downward move the model clamped: put the host back
    /// on the model's cursor. A plain `CUP` when the model's column is a
    /// real cell; `cursor_restore` when it is past the last one, because
    /// `vt100` keeps a pending wrap across `CUD` and a `CUP` would clamp it
    /// away on the host.
    fn clamp_repair(&self) -> Vec<u8> {
        let (row, col) = self.screen.cursor_position();
        if col < self.screen.cols() {
            format!("\x1b[{};{}H", row + 1, col + 1).into_bytes()
        } else {
            self.screen.cursor_restore()
        }
    }

    /// The client's own `1;{rows}` reservation plus an absolute cursor
    /// restore (DECSTBM homes the cursor on a real terminal, so the
    /// reposition is not optional -- and never through the shared DECSC
    /// register), when the workload is on full-screen margins and the
    /// reservation is expressible at all.
    fn reservation_reassert(&self) -> Option<Vec<u8>> {
        if self.screen.margins().is_some() || self.screen.rows() < 2 {
            return None;
        }
        let mut seq = format!("\x1b[1;{}r", self.screen.rows()).into_bytes();
        seq.extend_from_slice(&self.screen.cursor_restore());
        Some(seq)
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
        if byte == 0x1b || !self.screen.at_escape_boundary() {
            return self.screen.bytes_to_ground(&data[i..]).max(1);
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

    /// Re-fit both the model and the tracked margins to a new workload
    /// geometry (the physical terminal minus the reserved row). Fails, with
    /// the model left at its previous geometry, on a size `validate_size`
    /// rejects.
    pub fn try_set_size(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.screen.try_set_size(rows, cols)
    }

    /// `try_set_size` with the failure dropped: the model then stays at its
    /// previous geometry and its coordinates no longer match the host's.
    /// Callers that can surface the error should use `try_set_size`.
    pub fn set_size(&mut self, rows: u16, cols: u16) {
        let _ = self.try_set_size(rows, cols);
    }

    /// Start over for a different session (`Ctrl-b n`): a new workload has
    /// its own screen, its own margins and its own half-parsed sequences.
    /// Fails, with nothing changed, on a size `validate_size` rejects.
    pub fn try_reset(&mut self, rows: u16, cols: u16) -> Result<()> {
        // The retained-history depth belongs to the *client*, not to the
        // session it happens to be showing, so a switch rebuilds an equally
        // deep (and equally empty) grid rather than silently dropping to the
        // worker's zero-scrollback shape.
        let scrollback = self.screen.scrollback_capacity();
        self.screen = ScreenTracker::try_new_with_scrollback(rows, cols, scrollback)?;
        if let Some(hold) = self.host_alt.as_mut() {
            hold.reset();
        }
        self.wrap_guarded = false;
        Ok(())
    }

    /// `try_reset` with the failure dropped: the previous session's screen
    /// then stays in the model. Callers that can surface the error should
    /// use `try_reset`.
    pub fn reset(&mut self, rows: u16, cols: u16) {
        let _ = self.try_reset(rows, cols);
    }

    pub fn margins(&self) -> Option<(u16, u16)> {
        self.screen.margins()
    }

    /// True when the client may write bytes of its own: the relayed stream is
    /// between complete escape sequences and characters. This is a hard
    /// requirement -- injecting anywhere else corrupts the workload's frame.
    pub fn at_escape_boundary(&self) -> bool {
        self.screen.at_escape_boundary()
    }

    /// True while the workload is part-way through a synchronized-output
    /// frame. A soft preference rather than a hard requirement: waiting for
    /// the frame to close keeps the redraw out of the middle of a repaint,
    /// but a workload that never closes one must not be able to starve the
    /// bar forever (see `STATUS_BAR_SYNC_DEFER_LIMIT` in `src/bin/a.rs`).
    pub fn in_synchronized_update(&self) -> bool {
        self.screen.in_synchronized_update()
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

    /// `snapshot`, passed through `filter_host` -- the bytes to actually
    /// write to the host terminal to repaint it from this model.
    ///
    /// Always ends with `?1007l`. `state_formatted()` does not mention
    /// alternateScroll, and a live repaint that restored the workload's
    /// input modes used to leave the host on its alt-screen default --
    /// wheel becomes cursor-up/down, which types prompt history into the
    /// agent. Pinning it here means every snapshot write holds the mode
    /// the attach client took at start, not the terminal's default.
    pub fn host_snapshot(&mut self) -> Vec<u8> {
        let snapshot = self.snapshot();
        let mut out = self.filter_host(&snapshot).unwrap_or(snapshot);
        out.extend_from_slice(b"\x1b[?1007l");
        out
    }

    #[cfg(test)]
    pub fn cursor_position(&self) -> (u16, u16) {
        self.screen.cursor_position()
    }
}
