// -- scroll mode: key decoding and offset arithmetic -------------------

/// The property the whole mode rests on: in scroll mode `scroll_keys`
/// classifies every byte as either a command or `Ignored`, and both are
/// *consumed*. There is no third answer that could let a keystroke reach
/// the workload.
#[test]
fn scroll_mode_consumes_every_byte_it_is_given() {
    // Ordinary typing, control characters, a paste, an unknown CSI, a
    // non-wheel mouse report, UTF-8.
    for chunk in [
        b"hello world".as_slice(),
        b"\x7f\x0d\x0a\x09".as_slice(),
        b"\x1b[200~pasted text\x1b[201~".as_slice(),
        b"\x1b[1;2R".as_slice(),
        b"\x1b[<0;10;5M".as_slice(),
        "naïve — ünïcode".as_bytes(),
        b"\x1b[Z\x1bOP\x1b[15~".as_slice(),
    ] {
        let mut i = 0;
        let mut guard = 0;
        while i < chunk.len() {
            guard += 1;
            assert!(guard < 1000, "scroll_keys made no progress on {chunk:?}");
            match scroll_keys(&chunk[i..]) {
                ScrollKey::Command(_, n) | ScrollKey::Ignored(n) => {
                    assert!(n > 0, "a zero-length consume would spin forever");
                    i += n;
                }
                ScrollKey::Incomplete => panic!(
                    "a complete chunk must never be Incomplete: {:?} at {i}",
                    String::from_utf8_lossy(chunk)
                ),
            }
        }
        assert_eq!(i, chunk.len(), "consumed past the end of {chunk:?}");
    }
}

#[test]
fn scroll_keys_navigation_bindings() {
    use ScrollCommand::*;
    for (bytes, expected) in [
        (b"\x1b[5~".as_slice(), PageUp),
        (b"\x1b[6~".as_slice(), PageDown),
        (b"\x1b[5;2~".as_slice(), PageUp), // shifted PageUp
        (b"\x1b[A".as_slice(), Up(1)),
        (b"\x1b[B".as_slice(), Down(1)),
        (b"\x1bOA".as_slice(), Up(1)),
        (b"\x1bOB".as_slice(), Down(1)),
        (b"\x1b[H".as_slice(), Top),
        (b"\x1b[F".as_slice(), Bottom),
        (b"\x1b[1~".as_slice(), Top),
        (b"\x1b[4~".as_slice(), Bottom),
        (b"k".as_slice(), Up(1)),
        (b"j".as_slice(), Down(1)),
        (b" ".as_slice(), PageDown),
        (b"b".as_slice(), PageUp),
        (b"u".as_slice(), HalfUp),
        (b"d".as_slice(), HalfDown),
        (b"g".as_slice(), Top),
        (b"G".as_slice(), Bottom),
        (b"q".as_slice(), Exit),
        (b"\x03".as_slice(), Exit),
        (b"\x1b".as_slice(), Exit),
        (b"\x1b[<64;10;5M".as_slice(), Up(WHEEL_LINES)),
        (b"\x1b[<65;10;5M".as_slice(), Down(WHEEL_LINES)),
    ] {
        match scroll_keys(bytes) {
            ScrollKey::Command(command, consumed) => {
                assert_eq!(
                    command,
                    expected,
                    "for {:?}",
                    String::from_utf8_lossy(bytes)
                );
                assert_eq!(
                    consumed,
                    bytes.len(),
                    "for {:?}",
                    String::from_utf8_lossy(bytes)
                );
            }
            other => panic!("{:?} gave {other:?}", String::from_utf8_lossy(bytes)),
        }
    }
}

/// A wheel *release* report (`m`) must not move the view a second time,
/// or one notch would scroll twice as far as tmux's.
#[test]
fn scroll_keys_wheel_release_is_swallowed_not_repeated() {
    assert_eq!(
        scroll_keys(b"\x1b[<64;10;5m"),
        ScrollKey::Ignored(b"\x1b[<64;10;5m".len())
    );
}

/// An arrow key or mouse report arriving in two `read()`s is held, not
/// mistaken for something else.
#[test]
fn scroll_keys_split_sequences_are_incomplete_not_misread() {
    assert_eq!(scroll_keys(b"\x1b["), ScrollKey::Incomplete);
    assert_eq!(scroll_keys(b"\x1bO"), ScrollKey::Incomplete);
    assert_eq!(scroll_keys(b"\x1b[<64;10"), ScrollKey::Incomplete);
    // ...but not forever: a stray `ESC [` followed by junk is bounded.
    let long = [b"\x1b[".as_slice(), &[b'1'; 40]].concat();
    assert_eq!(scroll_keys(&long), ScrollKey::Ignored(long.len()));
}

/// The bar has to name the mode and the position at every width, because
/// it is the only thing telling the user where their keystrokes go.
#[test]
fn scroll_bar_text_keeps_the_mode_and_position_at_every_width() {
    let view = ScrollView {
        offset: 12,
        available: 2000,
    };
    for cols in [10usize, 20, 40, 80, 200] {
        let text = scroll_bar_text(view, cols, false);
        assert_eq!(
            terminal_display_width(&text),
            cols,
            "the bar must fill exactly its row at {cols} columns"
        );
        assert!(
            text.contains("SCROLL"),
            "the mode must be named at {cols} columns, got {text:?}"
        );
        if cols >= 20 {
            assert!(
                text.contains("12/2000"),
                "the position must survive at {cols} columns, got {text:?}"
            );
        }
    }
}

/// The pager must stay anchored to its content while the workload
/// streams behind it. `offset` counts lines above the live screen, so
/// without compensation every line the agent scrolls off drags the page
/// the user is reading that much closer to the bottom -- a streaming
/// reply yanks the reader back down mid-conversation, which is the
/// tmux-parity complaint that opened the pager in the first place.
#[test]
fn reanchor_view_grows_the_offset_by_lines_that_arrived_behind_the_pager() {
    let mut view = ScrollView {
        offset: 5,
        available: 10,
    };
    reanchor_view(&mut view, 17);
    assert_eq!(
        view.offset, 12,
        "seven new lines must push the view seven lines further back"
    );
    assert_eq!(view.available, 17);
}

/// The live screen is the anchor itself: at offset 0 the pager shows
/// whatever is newest, and no compensation is due.
#[test]
fn reanchor_view_leaves_the_live_offset_at_the_bottom() {
    let mut view = ScrollView {
        offset: 0,
        available: 10,
    };
    reanchor_view(&mut view, 25);
    assert_eq!(view.offset, 0);
    assert_eq!(view.available, 25);
}

/// `available` can also shrink under the view (a rebuild the guard
/// adopted, a resize); the baseline must follow it without inventing
/// compensation out of a negative growth. The offset is left alone --
/// `scrolled_frame` clamps it at render time.
#[test]
fn reanchor_view_tolerates_available_shrinking() {
    let mut view = ScrollView {
        offset: 5,
        available: 10,
    };
    reanchor_view(&mut view, 4);
    assert_eq!(view.offset, 5);
    assert_eq!(view.available, 4);
}

/// An empty pager must say *why* it is empty, in both of the two ways a
/// pager can be empty. The primary-screen case used to show the generic
/// "PgUp/PgDn" hint over a pager that could not move, which reads as a
/// broken feature rather than an answer -- and it is the case a
/// full-screen TUI that repaints in place produces, which is most of what
/// runs under aplexer.
#[test]
fn an_empty_pager_says_why_it_is_empty() {
    let empty = ScrollView {
        offset: 0,
        available: 0,
    };
    let alt = scroll_bar_text(empty, 120, true);
    assert!(
        alt.contains("no history: the workload owns the screen"),
        "an alt-screen workload's empty pager must name the reason: {alt:?}"
    );
    let primary = scroll_bar_text(empty, 120, false);
    assert!(
        primary.contains("no history"),
        "an empty pager on the primary screen must say so too, not offer \
             navigation keys that cannot do anything: {primary:?}"
    );
    assert!(
        !primary.contains("the workload owns the screen"),
        "...and must not blame the alternate screen when it is not in use: {primary:?}"
    );
    // The reason has to survive an ordinary terminal, not just a wide one.
    // It used to be the first thing the width ladder dropped, which left
    // exactly the row the user reported: `SCROLL 0/0 · q live`, with no
    // hint that the emptiness was the answer rather than a failure.
    for cols in [80usize, 100, 200] {
        for alt in [true, false] {
            let text = scroll_bar_text(empty, cols, alt);
            assert!(
                text.contains("no history"),
                "the reason must fit a {cols}-column terminal (alt={alt}): {text:?}"
            );
            assert!(
                !text.contains("PgUp"),
                "a pager that cannot move must not offer keys to move it: {text:?}"
            );
            assert_eq!(terminal_display_width(&text), cols);
        }
    }
    // A pager with history says nothing of the sort, at any width.
    for cols in [40usize, 80, 200] {
        let full = scroll_bar_text(
            ScrollView {
                offset: 0,
                available: 900,
            },
            cols,
            false,
        );
        assert!(
            !full.contains("no history"),
            "a pager with 900 lines behind it must not apologise: {full:?}"
        );
    }
}

/// A pager whose "back to live" gesture needs a second keystroke to
/// actually hand the keyboard back is the confusion this mode exists to
/// avoid, so scrolling down past the live screen leaves the mode.
#[test]
fn scrolling_down_past_the_live_screen_leaves_scroll_mode() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    ctx.scroll.active.store(true, Ordering::SeqCst);
    *ctx.scroll
        .view
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = ScrollView {
        offset: 2,
        available: 100,
    };
    apply_scroll_command(&ctx, ScrollCommand::Down(1));
    assert!(
        ctx.scroll.is_active(),
        "one line up from the bottom is still the pager"
    );
    apply_scroll_command(&ctx, ScrollCommand::Down(5));
    assert!(
        !ctx.scroll.is_active(),
        "hitting the bottom hands the keyboard back to the session"
    );
}

/// `Ctrl-b [` opens the pager without moving it, and without immediately
/// closing it again -- the `Stay` command exists for exactly that.
#[test]
fn ctrl_b_bracket_opens_the_pager_at_the_live_screen() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    enter_scroll_mode(&ctx, ScrollCommand::Stay);
    assert!(ctx.scroll.is_active());
    assert_eq!(
        ctx.scroll
            .view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .offset,
        0
    );
    apply_scroll_command(&ctx, ScrollCommand::Exit);
    assert!(!ctx.scroll.is_active());
}

#[test]
fn scan_ctrl_b_bracket_opens_scroll_mode() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, b'[']);
    assert!(matches!(actions.as_slice(), [InputAction::Scroll]));
}

/// The routing layer, not just the decoder: with the pager up, a chunk
/// of ordinary typing comes back empty -- nothing to send to the
/// workload.
#[test]
fn scroll_input_forwards_nothing_while_the_pager_is_up() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    let mut input = ScrollInput::default();
    assert_eq!(
        input.route(&ctx, b"ls -la\r"),
        b"ls -la\r".to_vec(),
        "with the pager down and no mouse borrowed, input is untouched"
    );
    enter_scroll_mode(&ctx, ScrollCommand::Stay);
    for chunk in [
        b"ls -la\r".as_slice(),
        b"\x1b[A".as_slice(),
        b"XXNOTINPUTXX".as_slice(),
        b"\x1b[<0;3;4M".as_slice(),
        b"rm -rf /\r".as_slice(),
    ] {
        assert!(
            input.route(&ctx, chunk).is_empty(),
            "{:?} must not reach the workload",
            String::from_utf8_lossy(chunk)
        );
        if !ctx.scroll.is_active() {
            // A chunk containing a downward move at the live screen
            // (Space is PageDown) legitimately closes the pager -- and
            // the assertion above is the important half: the *rest* of
            // that chunk is discarded rather than typed into the
            // session. Reopen for the next case.
            enter_scroll_mode(&ctx, ScrollCommand::Stay);
        }
    }
}

/// A failed history refresh must not take the pager down with it. The
/// model here has seen a DECSTBM sub-range (the gate fires) and the test
/// record's socket path is a regular file (every RPC fails fast), which
/// is exactly the "worker went away between the keystroke and the
/// rebuild" case: `Ctrl-b [` still opens the pager on whatever the live
/// model has, which is the pre-refresh behavior.
#[test]
fn pager_entry_survives_a_failed_history_refresh() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    feed_test_screen(&ctx.screen, b"\x1b[3;23r");
    assert!(
        ctx.screen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .subregion_seen(),
        "precondition: the gate has to fire, or this tests nothing"
    );
    enter_scroll_mode(&ctx, ScrollCommand::Stay);
    assert!(ctx.scroll.is_active());
    apply_scroll_command(&ctx, ScrollCommand::Exit);
    assert!(!ctx.scroll.is_active());
}

/// `i` in the pager hands the keyboard to the workload -- the thing
/// tmux copy-mode cannot do: text forwards verbatim, mouse reports stay
/// swallowed (the client borrowed the mouse; the workload never asked
/// for it), and a lone Esc takes the keyboard back with the pager still
/// up, paging again.
#[test]
fn type_through_forwards_text_until_esc_returns_to_paging() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    enter_scroll_mode(&ctx, ScrollCommand::Stay);
    let mut input = ScrollInput::default();
    assert!(
        input.route(&ctx, b"i").is_empty(),
        "the i that opens type-through is consumed, not sent"
    );
    assert!(ctx.scroll.is_typing(), "i must enter type-through");
    assert!(ctx.scroll.is_active(), "typing must not close the pager");
    assert_eq!(
        input.route(&ctx, b"hi there\r"),
        b"hi there\r".to_vec(),
        "while typing, text goes to the workload"
    );
    assert!(
        input.route(&ctx, b"\x1b[<0;3;4M").is_empty(),
        "mouse reports stay swallowed during type-through"
    );
    assert!(
        input.route(&ctx, b"\x1b").is_empty(),
        "the Esc that ends type-through is consumed"
    );
    assert!(!ctx.scroll.is_typing());
    assert!(
        ctx.scroll.is_active(),
        "Esc returns to paging, not to the live screen"
    );
    assert!(
        input.route(&ctx, b"xx").is_empty(),
        "once back in the pager, keys are swallowed again"
    );
    apply_scroll_command(&ctx, ScrollCommand::Exit);
    assert!(!ctx.scroll.is_active());
}

/// The type-through bar keeps the pager's position readout (the offset is
/// still where the user left it) and names the mode; narrow widths
/// degrade without ever exceeding the row.
#[test]
fn typing_bar_names_the_mode_and_keeps_the_position() {
    let view = ScrollView {
        offset: 12,
        available: 240,
    };
    let text = scroll_bar_typing_text(view, 120);
    assert!(text.contains("SCROLL 12/240"), "{text:?}");
    assert!(text.contains("TYPE"), "{text:?}");
    assert!(text.chars().count() <= 120);
    let medium = scroll_bar_typing_text(view, 20);
    assert!(
        medium.contains("TYPE") && medium.chars().count() <= 20,
        "{medium:?}"
    );
    assert_eq!(scroll_bar_typing_text(view, 4), "TYPE");
}

#[test]
fn scroll_keys_binds_i_to_type_through() {
    assert_eq!(
        scroll_keys(b"i"),
        ScrollKey::Command(ScrollCommand::TypeThrough, 1)
    );
}

/// Pane delivery appends the return by default (the tmuxctl behavior) in
/// both framed and raw form, and `--no-enter` drops it in both.
#[test]
fn pane_delivery_appends_enter_by_default_and_no_enter_drops_it() {
    assert_eq!(
        pane_input_bytes("ship it", Some("review"), false, false),
        b"[aplexer message from review] ship it\r"
    );
    assert_eq!(
        pane_input_bytes("ship it", Some("review"), true, false),
        b"ship it\r"
    );
    assert_eq!(
        pane_input_bytes("hold", Some("review"), false, true),
        b"[aplexer message from review] hold"
    );
    assert_eq!(pane_input_bytes("hold", None, true, true), b"hold");
}

/// While the client holds the mouse and the pager is *down*, mouse
/// reports are swallowed (the workload never asked for them) and a wheel
/// roll up opens the pager -- with no `Ctrl-b` first, which is the
/// gesture the user actually reported as broken. Ordinary typing in the
/// same chunk still gets through.
#[test]
fn wheel_up_opens_the_pager_with_no_prefix_and_typing_still_passes() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    *ctx.mouse_owned
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(true);
    let mut input = ScrollInput::default();
    assert_eq!(input.route(&ctx, b"ab"), b"ab".to_vec());
    assert!(!ctx.scroll.is_active());
    // A left click: swallowed, never typed into the workload.
    assert!(input.route(&ctx, b"\x1b[<0;5;5M").is_empty());
    assert!(!ctx.scroll.is_active());
    // The wheel: straight into the pager.
    assert!(input.route(&ctx, b"\x1b[<64;5;5M").is_empty());
    assert!(ctx.scroll.is_active());
}

/// A mouse report split across two reads is reassembled rather than
/// leaking its tail into the workload as text.
#[test]
fn a_split_mouse_report_is_buffered_not_leaked_to_the_workload() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    *ctx.mouse_owned
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(true);
    let mut input = ScrollInput::default();
    assert!(input.route(&ctx, b"\x1b[<64;5").is_empty());
    assert!(!ctx.scroll.is_active());
    assert!(input.route(&ctx, b";5M").is_empty());
    assert!(ctx.scroll.is_active());
}

/// The counterpart guarantee: a bare `ESC` at the end of a chunk is
/// forwarded immediately while the pager is down, so pressing Escape in
/// an editor inside the session does not wait for the next keystroke.
#[test]
fn a_bare_escape_is_never_held_back_from_the_workload() {
    let ctx = status_ctx_for_test(true);
    let _null = StdoutToDevNull::new();
    *ctx.mouse_owned
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(true);
    let mut input = ScrollInput::default();
    assert_eq!(input.route(&ctx, b"\x1b"), b"\x1b".to_vec());
    assert_eq!(input.route(&ctx, b"\x1b["), b"\x1b[".to_vec());
}

// -- parse_sgr_mouse (docs/clickable-status-bar-design.md section 2) --

#[test]
fn parse_sgr_mouse_left_click_press() {
    let buf = b"\x1b[<0;10;5M";
    match parse_sgr_mouse(buf) {
        MouseParse::Complete(report, consumed) => {
            assert_eq!(
                report,
                MouseReport {
                    button: 0,
                    press: true,
                    col: 10,
                    row: 5,
                }
            );
            assert_eq!(consumed, buf.len());
        }
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[test]
fn parse_sgr_mouse_release() {
    let buf = b"\x1b[<0;10;5m";
    match parse_sgr_mouse(buf) {
        MouseParse::Complete(report, consumed) => {
            assert!(!report.press);
            assert_eq!(consumed, buf.len());
        }
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[test]
fn parse_sgr_mouse_large_coordinates_no_1006_overflow() {
    // The entire point of SGR (?1006h) over legacy (?1000h alone) mode:
    // no 223-column/row ceiling.
    let buf = b"\x1b[<2;9999;500M";
    match parse_sgr_mouse(buf) {
        MouseParse::Complete(report, _) => {
            assert_eq!(report.col, 9999);
            assert_eq!(report.row, 500);
        }
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[test]
fn parse_sgr_mouse_trailing_bytes_only_consumes_the_report() {
    let buf = b"\x1b[<0;10;5Mrest-of-buffer";
    match parse_sgr_mouse(buf) {
        MouseParse::Complete(_, consumed) => assert_eq!(consumed, 10),
        other => panic!("expected Complete, got {other:?}"),
    }
}

#[test]
fn parse_sgr_mouse_incomplete_at_every_prefix_length() {
    let full = b"\x1b[<0;10;5M";
    for split in 1..full.len() {
        let partial = &full[..split];
        assert_eq!(
            parse_sgr_mouse(partial),
            MouseParse::Incomplete,
            "prefix of length {split} should be Incomplete"
        );
    }
}

#[test]
fn parse_sgr_mouse_rejects_ordinary_csi_sequences() {
    // Arrow keys, cursor reports, colors, etc. -- none start with the
    // `ESC [ <` mouse prefix, so these must be an immediate NotMouse,
    // never treated as "keep buffering".
    assert_eq!(parse_sgr_mouse(b"\x1b[A"), MouseParse::NotMouse); // up arrow
    assert_eq!(parse_sgr_mouse(b"\x1b[31m"), MouseParse::NotMouse); // SGR color
    assert_eq!(parse_sgr_mouse(b"hello"), MouseParse::NotMouse);
}

#[test]
fn parse_sgr_mouse_empty_buffer_is_incomplete_not_rejected() {
    // Zero bytes seen yet can't be ruled out as the start of a mouse
    // report -- a caller with nothing buffered should keep reading,
    // not treat an empty read as "definitely not a mouse sequence".
    assert_eq!(parse_sgr_mouse(b""), MouseParse::Incomplete);
}

#[test]
fn parse_sgr_mouse_malformed_after_prefix_is_not_mouse_not_incomplete() {
    // A non-digit, non-';' byte right where a field is expected can
    // never resolve into a valid report -- must not be reported
    // Incomplete (that would make a caller buffer forever).
    assert_eq!(parse_sgr_mouse(b"\x1b[<x;10;5M"), MouseParse::NotMouse);
    assert_eq!(parse_sgr_mouse(b"\x1b[<0;;5M"), MouseParse::NotMouse);
    assert_eq!(parse_sgr_mouse(b"\x1b[<0;10;5X"), MouseParse::NotMouse);
}

#[test]
fn parse_sgr_mouse_split_across_two_reads_reassembles() {
    // Mirrors the Ctrl-b split-read tests above: a caller buffering
    // bytes across scan() calls must see Incomplete on the first half
    // and Complete once the second half is appended.
    let full: &[u8] = b"\x1b[<0;10;5M";
    let split = 5;
    assert_eq!(parse_sgr_mouse(&full[..split]), MouseParse::Incomplete);
    let mut buffered = full[..split].to_vec();
    buffered.extend_from_slice(&full[split..]);
    match parse_sgr_mouse(&buffered) {
        MouseParse::Complete(_, consumed) => assert_eq!(consumed, full.len()),
        other => panic!("expected Complete, got {other:?}"),
    }
}
