use crate::screen::client::without_scroll_regions;
use crate::screen::host_alt::HostAltHold;
use crate::screen::*;

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

/// A geometry the model cannot hold is reported, and the model is left
/// exactly as it was -- not half-resized, not silently stale.
#[test]
fn client_screen_rejects_an_oversized_geometry_and_keeps_its_own() {
    let mut client = ClientScreen::try_new(24, 80).unwrap();
    client.feed(b"kept");
    assert!(client.try_set_size(1000, 1000).is_err());
    assert!(client.try_reset(1000, 1000).is_err());
    assert!(client.snapshot().windows(4).any(|w| w == b"kept"));
    assert_eq!(client.cursor_position(), (0, 4));
    client.set_size(30, 100);
    assert!(client.try_reset(30, 100).is_ok());
    assert!(!client.snapshot().windows(4).any(|w| w == b"kept"));
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
    assert_eq!(
        client.filter_host(b"plain text needs no copy"),
        None,
        "a chunk with nothing to rewrite goes out as-is"
    );
    // A sequence held over from the previous chunk still has to be
    // completed, whatever the next chunk holds.
    assert_eq!(client.filter_host(b"\x1b[?1049"), Some(Vec::new()));
    assert_eq!(client.filter_host(b"l"), Some(Vec::new()));
    assert_eq!(client.filter_host(b"\x1b[?1049"), Some(Vec::new()));
    assert_eq!(client.filter_host(b"hXYZ"), Some(b"XYZ".to_vec()));
    client.feed(b"\x1b[?1049h");
    assert!(
        client.snapshot().windows(8).any(|w| w == b"\x1b[?1049h"),
        "the model must still see the workload's alt-screen enter"
    );
    assert!(
        !client
            .host_snapshot()
            .windows(8)
            .any(|w| w == b"\x1b[?1049h"),
        "the host-bound snapshot must not switch the host's screen"
    );
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

/// The wrap guard fires on a character's lead byte, before the character
/// can be known to be zero-width. When it turns out to be a combining
/// mark (`U+0301`, `\xcc\x81`) the model stays in pending wrap, and the
/// host's pending wrap -- which the guard's reposition cancelled -- has
/// to be put back. Across a chunk boundary too: `relay` runs once per
/// PTY read, and the lead byte can be a read's last byte.
#[test]
fn relay_restores_a_pending_wrap_a_combining_mark_did_not_use() {
    for chunks in [&[&b"\xcc\x81"[..]][..], &[&b"\xcc"[..], &b"\x81"[..]][..]] {
        let mut rig = RelayRig::new(24, 20);
        rig.relay(b"\x1b[5;15r\x1b[23;1HABCDEFGHIJKLMNOPQRST");
        for chunk in chunks {
            rig.relay(chunk);
        }
        assert_eq!(
            rig.workload.screen().cursor_position(),
            (22, 20),
            "the combining mark attached to the last cell instead of wrapping"
        );
        rig.assert_agrees("after a combining mark in pending wrap");
        // ...and the next character wraps on both sides.
        rig.relay(b"X");
        rig.assert_agrees("after the character following the combining mark");
        rig.assert_bar_intact("by a combining mark");
    }
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

/// `ESC ( B` -- the charset designation `tput sgr0` and every bash
/// prompt end with -- has the final byte of `CSI B` but moves nothing.
/// It must not be repaired as a cursor-down: the repair is a `CUP` that
/// cannot express the pending wrap the model may be in, so a misfire
/// there clamps the host's wrap away. Neither whole nor split across
/// chunks, and the real `CSI B` split the same way is still repaired.
#[test]
fn relay_does_not_mistake_a_charset_designation_for_cursor_down() {
    for chunks in [&[&b"\x1b(B"[..]][..], &[&b"\x1b("[..], &b"B"[..]][..]] {
        let mut rig = RelayRig::new(24, 20);
        rig.relay(b"\x1b[5;15r\x1b[23;1HABCDEFGHIJKLMNOPQRST");
        for chunk in chunks {
            assert!(
                !rig.relay(chunk),
                "{chunk:?} moves nothing and must not be rewritten"
            );
        }
        rig.relay(b"X");
        rig.assert_agrees("after a charset designation in pending wrap");
        rig.assert_bar_intact("by a charset designation");
    }

    let mut rig = RelayRig::new(24, 20);
    rig.relay(b"\x1b[5;15r\x1b[23;1HX");
    rig.relay(b"\x1b[");
    assert!(
        rig.relay(b"B"),
        "a CSI B split across chunks is still a cursor-down"
    );
    rig.assert_agrees("after a split cursor-down off the last row");
}

/// `vt100` keeps a pending wrap across `CUD` (the column is untouched),
/// so the repair after a cursor-down on the last row has to reproduce
/// it; a `CUP` clamps the host to the last cell instead, and the host's
/// next character then overwrites that cell where the model wraps.
#[test]
fn relay_cursor_down_repair_keeps_a_pending_wrap() {
    let mut rig = RelayRig::new(24, 20);
    rig.relay(b"\x1b[5;15r\x1b[23;1HABCDEFGHIJKLMNOPQRST");
    assert!(rig.relay(b"\x1b[B"));
    rig.assert_agrees("after a cursor-down in pending wrap");
    assert_eq!(
        rig.workload.screen().cursor_position(),
        (22, 20),
        "the workload is in pending wrap after the cursor-down"
    );
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
