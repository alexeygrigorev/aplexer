use crate::screen::*;

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

    let screen_a = a.parser().screen();
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
    assert_eq!(
        screen_a.attributes_formatted(),
        screen_b.attributes_formatted(),
        "drawing attributes mismatch"
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

/// The cursor re-fix after the DECSTBM has to reproduce a *pending
/// wrap*: with the workload's cursor past the last column `vt100` reports
/// `col == cols`, which a plain `CUP` clamps to the last cell -- so the
/// restored terminal put the workload's next character on the same row
/// where the workload's own screen wrapped it onto the next.
#[test]
fn round_trip_decstbm_with_the_cursor_in_pending_wrap() {
    let mut stream = b"\x1b[3;20r\x1b[1;31m".to_vec();
    stream.extend(std::iter::repeat_n(b'W', 80));
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
        tracker.parser().screen().cursor_position(),
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
/// `HISTLINE-{i}\r\n` for every `i` in `range` -- the fill the shrink
/// tests scroll through.
fn histlines(range: std::ops::RangeInclusive<u16>) -> Vec<u8> {
    range
        .flat_map(|i| format!("HISTLINE-{i}\r\n").into_bytes())
        .collect()
}

#[test]
fn shrinking_keeps_the_cursor_line_the_way_a_real_terminal_does() {
    let mut t = ScreenTracker::new(24, 80);
    let mut fill = histlines(58..=80);
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
    t.process(&histlines(58..=80));
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
    let mut fill = histlines(58..=80);
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
        a.margins(),
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
        a.margins(),
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
