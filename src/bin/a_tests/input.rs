#[test]
fn scan_ctrl_b_n_asks_for_a_new_session() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, b'n']);
    assert!(matches!(
        actions.as_slice(),
        [InputAction::Switch(SwitchTarget::New)]
    ));
    // Split across reads, like every other chord: the prefix state has to
    // survive the read() boundary.
    let mut split = InputScanner::default();
    assert!(split.scan(&[0x02]).is_empty());
    assert!(matches!(
        split.scan(b"n").as_slice(),
        [InputAction::Switch(SwitchTarget::New)]
    ));
    // And the chord bytes never reach the workload -- pressing it must
    // not type an `n` into whatever has the prompt.
    let mut mixed = InputScanner::default();
    let actions = mixed.scan(&[b'a', 0x02, b'n', b'z']);
    assert_eq!(actions.len(), 3);
    match &actions[0] {
        InputAction::Forward(b) => assert_eq!(b, b"a"),
        _ => panic!("expected the pre-chord byte forwarded"),
    }
    assert!(matches!(actions[1], InputAction::Switch(SwitchTarget::New)));
    match &actions[2] {
        InputAction::Forward(b) => assert_eq!(b, b"z"),
        _ => panic!("expected the post-chord byte forwarded"),
    }
}

/// Session navigation lives on Right/Left and workspace navigation on
/// Down/Up, in *both* encodings a terminal can send them in: CSI
/// (`ESC [ C`) in normal cursor mode and SS3 (`ESC O C`) in application
/// cursor mode, which a TUI in the session can turn on at any moment.
#[test]
fn scan_ctrl_b_arrows_navigate_in_both_cursor_modes() {
    for introducer in [b'[', b'O'] {
        for (final_byte, expected) in [
            (b'C', SwitchTarget::Next),
            (b'D', SwitchTarget::Prev),
            (b'B', SwitchTarget::NextWorkspace),
            (b'A', SwitchTarget::PrevWorkspace),
        ] {
            let mut s = InputScanner::default();
            let actions = s.scan(&[0x02, 0x1b, introducer, final_byte]);
            match actions.as_slice() {
                [InputAction::Switch(target)] => assert_eq!(
                    *target, expected,
                    "ESC {} {} should mean {expected:?}",
                    introducer as char, final_byte as char
                ),
                other => panic!(
                    "ESC {} {} was not consumed as a chord ({} action(s))",
                    introducer as char,
                    final_byte as char,
                    other.len()
                ),
            }
        }
    }
}

/// A terminal writes an escape sequence in one `write`, but a PTY read can
/// still split it anywhere -- every prefix has to survive the boundary,
/// including `Ctrl-b` and `ESC` landing in different reads.
#[test]
fn scan_ctrl_b_arrow_split_across_every_read_boundary() {
    let chord: &[u8] = &[0x02, 0x1b, b'[', b'C'];
    for split in 1..chord.len() {
        let mut s = InputScanner::default();
        let first = s.scan(&chord[..split]);
        assert!(
            first.is_empty(),
            "a partial chord split at {split} must emit nothing yet"
        );
        assert!(
            s.awaiting_escape() || split == 1,
            "split at {split} should leave the scanner holding escape bytes"
        );
        assert!(
            matches!(
                s.scan(&chord[split..]).as_slice(),
                [InputAction::Switch(SwitchTarget::Next)]
            ),
            "a chord split at {split} did not resolve"
        );
    }
}

/// `Ctrl-b` then a bare `ESC` is not a chord: once the input thread stops
/// waiting for the rest of an arrow (`CHORD_ESCAPE_TIMEOUT`), both
/// withheld bytes go to the workload, so an editor still gets its Escape.
#[test]
fn scan_ctrl_b_escape_flushes_when_no_arrow_follows() {
    let mut s = InputScanner::default();
    assert!(s.scan(&[0x02, 0x1b]).is_empty());
    assert!(s.awaiting_escape());
    assert_eq!(bytes(&s.flush_pending()), vec![0x02, 0x1b]);
    assert!(!s.awaiting_escape());
    // Nothing is left behind: the next key is scanned from scratch.
    assert!(matches!(
        s.scan(&[0x02, b'n']).as_slice(),
        [InputAction::Switch(SwitchTarget::New)]
    ));

    // A lone `Ctrl-b` is *not* flushed: waiting for its second key is the
    // keymap's contract, and only the multi-byte arrows are ambiguous.
    let mut lone = InputScanner::default();
    assert!(lone.scan(&[0x02]).is_empty());
    assert!(!lone.awaiting_escape());
    assert!(lone.flush_pending().is_empty());
}

/// An escape sequence after the prefix that is *not* an arrow (Home, F1,
/// ...) forwards every withheld byte in order rather than swallowing any.
#[test]
fn scan_ctrl_b_non_arrow_escape_forwards_every_byte() {
    let mut s = InputScanner::default();
    assert_eq!(bytes(&s.scan(&[0x02, 0x1b, b'[', b'H'])), {
        let mut expected = vec![0x02, 0x1b];
        expected.extend_from_slice(b"[H");
        expected
    });
    let mut alt = InputScanner::default();
    assert_eq!(
        bytes(&alt.scan(&[0x02, 0x1b, b'x'])),
        vec![0x02, 0x1b, b'x']
    );
}

/// `p` was only ever the other half of `n`/`p`. With `n` now meaning
/// "new", a lone "previous" on `p` would be a trap, so it is unbound and
/// forwards -- and `N`/`P` are untouched.
#[test]
fn scan_ctrl_b_p_is_unbound_but_capital_p_still_switches() {
    let mut s = InputScanner::default();
    assert_eq!(bytes(&s.scan(&[0x02, b'p'])), vec![0x02, b'p']);
    assert!(matches!(
        s.scan(&[0x02, b'N']).as_slice(),
        [InputAction::Switch(SwitchTarget::NextGlobal)]
    ));
    assert!(matches!(
        s.scan(&[0x02, b'P']).as_slice(),
        [InputAction::Switch(SwitchTarget::PrevGlobal)]
    ));
}

/// The arrow chords must not have eaten the bracket-ish keys next to
/// them: `[` is still the pager and the digits still jump.
#[test]
fn scan_ctrl_b_bracket_and_digits_survive_the_arrow_chords() {
    let mut s = InputScanner::default();
    assert!(matches!(
        s.scan(&[0x02, b'[']).as_slice(),
        [InputAction::Scroll]
    ));
    assert!(matches!(
        s.scan(&[0x02, b'4']).as_slice(),
        [InputAction::Switch(SwitchTarget::Index(4))]
    ));
    assert!(matches!(
        s.scan(&[0x02, b'l']).as_slice(),
        [InputAction::Switch(SwitchTarget::Last)]
    ));
    assert!(matches!(
        s.scan(&[0x02, b'r']).as_slice(),
        [InputAction::Redraw]
    ));
    assert!(matches!(
        s.scan(&[0x02, b'?']).as_slice(),
        [InputAction::Help]
    ));
}

/// The keymap has exactly one definition; the three renderings are views
/// of it. This is the guard on that: each must mention every bound key,
/// and the table must still spell the bindings the scanner implements.
#[test]
fn the_key_reference_is_generated_from_the_binding_table() {
    let help = attach_key_help();
    assert!(help.starts_with("Ctrl-b: "));
    // The overlay is the third view. Rendered at a size with room for
    // everything, it has to carry every key the table does -- a binding
    // that only reaches two of the three renderings is exactly the drift
    // this table exists to make impossible.
    let overlay = key_overlay_lines(40, 120).expect("40x120 fits the whole keymap");
    for binding in ATTACH_BINDINGS {
        assert!(
            overlay.iter().any(|line| line.contains(binding.keys)),
            "the key overlay dropped {:?}:\n{}",
            binding.keys,
            overlay.join("\n")
        );
        assert!(
            overlay
                .iter()
                .any(|line| line.contains(binding.description)),
            "the key overlay dropped {:?}:\n{}",
            binding.description,
            overlay.join("\n")
        );
    }
    for binding in ATTACH_BINDINGS {
        if let Some(brief) = binding.brief {
            assert!(
                help.contains(brief),
                "the status-bar reference dropped {brief:?}: {help}"
            );
        }
    }
    // The bindings the scanner actually implements, spelled as the table
    // spells them -- a binding added to the scanner and forgotten here (or
    // the reverse) fails this.
    let keys: Vec<&str> = ATTACH_BINDINGS.iter().map(|b| b.keys).collect();
    assert_eq!(
        keys,
        vec![
            "Right / Left",
            "Down / Up",
            "n",
            "d",
            "[",
            "N / P",
            "1-9",
            "l",
            "r",
            "?"
        ]
    );
}

/// The overlay is the third *view* of `ATTACH_BINDINGS`, not a third
/// copy of it: on a terminal with room for the whole table, every key and
/// every description the table holds is on screen verbatim. A binding
/// added to the scanner and the table shows up here for free; one written
/// out by hand could not.
#[test]
fn key_overlay_renders_every_binding_from_the_table() {
    let lines = key_overlay_lines(40, 100).expect("40x100 has room for the whole keymap");
    assert_eq!(
        lines.len(),
        ATTACH_BINDINGS.len() + KEY_OVERLAY_CHROME_ROWS,
        "every binding gets a row, plus two borders and the footer: {lines:#?}"
    );
    for (binding, line) in ATTACH_BINDINGS.iter().zip(&lines[1..]) {
        assert!(
            line.contains(binding.keys),
            "the overlay dropped the keys {:?}: {line}",
            binding.keys
        );
        assert!(
            line.contains(binding.description),
            "the overlay truncated {:?} on a terminal with room for it: {line}",
            binding.description
        );
    }
    assert!(
        lines[0].contains("Ctrl-b"),
        "the box has to name the key the user is waiting on: {}",
        lines[0]
    );
}

/// Every row is one uniform width that fits inside the terminal, and the
/// box never claims more rows than it was given. This is the "do not draw
/// outside the screen" guarantee stated over the layout rather than left
/// to the sequence writer, because the sequence addresses rows absolutely
/// and a box one row too tall would land on the status bar.
#[test]
fn key_overlay_lines_are_uniform_and_stay_inside_the_terminal() {
    for rows in [6usize, 9, 13, 23, 40, 200] {
        for cols in [32usize, 40, 46, 80, 100, 200] {
            let Some(lines) = key_overlay_lines(rows, cols) else {
                continue;
            };
            assert!(
                lines.len() <= rows,
                "a {rows}x{cols} box claimed {} rows",
                lines.len()
            );
            let width = terminal_display_width(&lines[0]);
            assert!(width <= cols, "a {rows}x{cols} box is {width} cells wide");
            for line in &lines {
                assert_eq!(
                    terminal_display_width(line),
                    width,
                    "ragged row in a {rows}x{cols} box: {line}"
                );
            }
        }
    }
}

/// Honest degradation, part one: a terminal with room for some of the
/// keymap gets some of it -- trimmed from the end, because the table is
/// ordered most-useful-first -- and is told how much it is not seeing.
#[test]
fn key_overlay_trims_from_the_end_and_says_how_much_it_dropped() {
    let rows = KEY_OVERLAY_CHROME_ROWS + 4;
    let lines = key_overlay_lines(rows, 80).expect("four bindings still fit");
    assert_eq!(lines.len(), rows);
    let dropped = ATTACH_BINDINGS.len() - 4;
    let footer = &lines[lines.len() - 2];
    assert!(
        footer.contains(&format!("{dropped} more")),
        "a trimmed box must say how many bindings it dropped: {footer}"
    );
    for binding in &ATTACH_BINDINGS[..4] {
        assert!(
            lines[1..5].iter().any(|l| l.contains(binding.keys)),
            "the first four table entries are the ones kept, missing {:?}",
            binding.keys
        );
    }
    let full = key_overlay_lines(40, 80).expect("40 rows fit everything");
    assert!(
        full[full.len() - 2].contains("Esc dismiss"),
        "an untrimmed box says how to get out, not how much is missing: {}",
        full[full.len() - 2]
    );
}

/// Honest degradation, part two: below a box worth drawing there is no
/// box. `show_key_overlay` reads this `None` as "flash the one-line
/// reference instead", which is the whole of the small-terminal story --
/// no half-drawn border, no writing past the last column.
#[test]
fn key_overlay_refuses_a_terminal_it_cannot_fit() {
    // Too short: chrome plus fewer than KEY_OVERLAY_MIN_BINDINGS rows.
    for rows in 0..KEY_OVERLAY_CHROME_ROWS + KEY_OVERLAY_MIN_BINDINGS {
        assert!(
            key_overlay_lines(rows, 200).is_none(),
            "{rows} rows is not enough for a box worth reading"
        );
    }
    assert!(key_overlay_lines(KEY_OVERLAY_CHROME_ROWS + KEY_OVERLAY_MIN_BINDINGS, 200).is_some());
    // Too narrow: the description column would stop being sentences.
    let keys_width = ATTACH_BINDINGS[..KEY_OVERLAY_MIN_BINDINGS]
        .iter()
        .map(|b| terminal_display_width(b.keys))
        .max()
        .unwrap();
    let narrowest = keys_width + 6 + KEY_OVERLAY_MIN_DESC;
    assert!(key_overlay_lines(40, narrowest - 1).is_none());
    assert!(key_overlay_lines(40, narrowest).is_some());
    assert!(key_overlay_lines(40, 0).is_none());
}

/// The sequence addresses rows absolutely, so the rows it addresses are
/// the guarantee: never row 0, never the reserved status row, never past
/// the bottom of the terminal.
#[test]
fn key_overlay_sequence_never_addresses_the_status_bar_row() {
    for (rows, reserved) in [(24u16, true), (24, false), (13, true), (40, true)] {
        let geom = TermGeom {
            rows,
            cols: 80,
            reserved,
        };
        let usable = key_overlay_rows(geom);
        let lines = key_overlay_lines(usable as usize, 80).expect("80 columns fit a box");
        let seq = key_overlay_sequence(geom, &lines);
        let text = String::from_utf8(seq).expect("the sequence is utf-8");
        let mut addressed: Vec<u16> = Vec::new();
        let mut rest = text.as_str();
        while let Some(at) = rest.find("\x1b[") {
            rest = &rest[at + 2..];
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() && rest[digits.len()..].starts_with(";1H") {
                addressed.push(digits.parse().expect("a row number"));
            }
        }
        assert_eq!(
            addressed.len(),
            lines.len(),
            "one absolute address per row, got {addressed:?}"
        );
        assert_eq!(*addressed.first().unwrap(), usable - lines.len() as u16 + 1);
        assert_eq!(
            *addressed.last().unwrap(),
            usable,
            "the box sits directly above the status bar"
        );
        for row in addressed {
            assert!(
                (1..=usable).contains(&row),
                "the box addressed row {row} on a {rows}-row terminal (usable {usable})"
            );
        }
    }
}

/// The two deadlines a held prefix can be under -- `KEY_OVERLAY_DELAY`
/// and `CHORD_ESCAPE_TIMEOUT` -- are armed by mutually exclusive scanner
/// states, which is what stops them fighting: a partial arrow chord can
/// never pop the overlay, and a bare `Ctrl-b ESC` never waits for one
/// deadline plus the other.
#[test]
fn the_overlay_deadline_and_the_chord_deadline_are_never_armed_together() {
    let mut s = InputScanner::default();
    assert!(s.settled(), "an idle scanner is under neither deadline");
    assert!(!s.awaiting_key() && !s.awaiting_escape());

    assert!(s.scan(&[0x02]).is_empty());
    assert!(
        s.awaiting_key(),
        "a lone Ctrl-b arms the overlay's deadline"
    );
    assert!(!s.awaiting_escape());
    assert!(!s.settled());

    // The moment the arrow's ESC arrives the overlay's deadline is gone
    // and the chord's is the only one left.
    assert!(s.scan(&[0x1b]).is_empty());
    assert!(s.awaiting_escape());
    assert!(!s.awaiting_key());
    assert!(!s.settled());

    // ... and completing the chord leaves neither armed.
    assert!(matches!(
        s.scan(b"[C").as_slice(),
        [InputAction::Switch(SwitchTarget::Next)]
    ));
    assert!(s.settled());
    assert!(!s.awaiting_key() && !s.awaiting_escape());

    // A bound key resolves the prefix in one step, which is what makes
    // the input thread take the overlay down before running its action.
    let mut fast = InputScanner::default();
    assert!(matches!(
        fast.scan(&[0x02, b'd']).as_slice(),
        [InputAction::Detach]
    ));
    assert!(fast.settled());
    // So does an unbound one, which also still falls through.
    let mut through = InputScanner::default();
    assert_eq!(bytes(&through.scan(&[0x02, b'p'])), vec![0x02, b'p']);
    assert!(through.settled());
}

/// The delay has to be long enough that a chord typed from muscle memory
/// resolves first, and its whole point is that it is a *different* wait
/// from the arrow chord's -- long enough to read as hesitation where 100ms
/// reads as a split escape sequence.
#[test]
fn the_overlay_delay_is_a_hesitation_not_a_chord_gap() {
    assert!(
        KEY_OVERLAY_DELAY > CHORD_ESCAPE_TIMEOUT,
        "an overlay that can fire inside the arrow chord's own deadline \
             would flicker on every Ctrl-b Left"
    );
    assert!(
        KEY_OVERLAY_DELAY < FLASH_DURATION,
        "hesitation has to be answered faster than a message is read"
    );
}

/// `SwitchTarget::New` is created, never selected: `pick_switch_target`
/// must say so rather than quietly resolving somewhere.
#[test]
fn pick_switch_target_refuses_to_select_a_new_session() {
    let ws = PathBuf::from("/ws/new");
    let a = mk_record("/ws/new", "a", Phase::Running);
    let groups = vec![(ws.clone(), vec![a.clone()])];
    let error = pick_switch_target(&groups, &ws, a.id, SwitchTarget::New, None)
        .expect_err("New must not be selectable");
    assert!(
        format!("{error:#}").contains("created, not selected"),
        "unexpected error: {error:#}"
    );
}

#[test]
fn scan_ctrl_q_through_ctrl_y_are_forwarded() {
    let mut s = InputScanner::default();
    for byte in 0x11u8..=0x19 {
        let actions = s.scan(&[byte]);
        assert_eq!(bytes(&actions), vec![byte]);
    }
}

#[test]
fn scan_control_bytes_preserve_input_order() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[b'a', 0x13, b'z']);
    assert_eq!(bytes(&actions), vec![b'a', 0x13, b'z']);
}

#[test]
fn scan_non_shortcut_control_bytes_still_forward() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x10, 0x1a, b'1', b'9']);
    assert_eq!(bytes(&actions), vec![0x10, 0x1a, b'1', b'9']);
    assert!(actions
        .iter()
        .all(|action| matches!(action, InputAction::Forward(_))));
}

#[test]
fn scan_split_across_reads() {
    let mut s = InputScanner::default();
    assert!(s.scan(&[0x02]).is_empty());
    let actions = s.scan(b"N");
    assert!(matches!(
        actions.as_slice(),
        [InputAction::Switch(SwitchTarget::NextGlobal)]
    ));
}

#[test]
fn scan_unbound_ctrl_b_forwards_both_bytes() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, b'x']);
    match actions.as_slice() {
        [InputAction::Forward(b)] => assert_eq!(b, &[0x02, b'x']),
        other => panic!("unexpected: {}", other.len()),
    }
}

#[test]
fn scan_forward_switch_forward() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[b'a', 0x02, b'3', b'z']);
    assert_eq!(actions.len(), 3);
    match &actions[0] {
        InputAction::Forward(b) => assert_eq!(b, b"a"),
        _ => panic!("expected Forward"),
    }
    assert!(matches!(
        &actions[1],
        InputAction::Switch(SwitchTarget::Index(3))
    ));
    match &actions[2] {
        InputAction::Forward(b) => assert_eq!(b, b"z"),
        _ => panic!("expected Forward"),
    }
}

#[test]
fn scan_ctrl_b_d_detaches() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, b'd']);
    assert!(matches!(actions.as_slice(), [InputAction::Detach]));
}

#[test]
fn scan_ctrl_bracket_forwards_rest() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[b'a', 0x1d, b'b', b'c']);
    match &actions[0] {
        InputAction::Forward(b) => assert_eq!(b, &[b'a', 0x1d, b'b', b'c']),
        _ => panic!("expected Forward"),
    }
}

#[test]
fn scan_double_ctrl_b_then_d() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, 0x02, b'd']);
    assert_eq!(actions.len(), 2);
    match &actions[0] {
        InputAction::Forward(b) => assert_eq!(b, &[0x02]),
        _ => panic!("expected Forward"),
    }
    assert!(matches!(&actions[1], InputAction::Detach));
}

#[test]
fn scan_ctrl_b_zero_forwards_both() {
    let mut s = InputScanner::default();
    let actions = s.scan(&[0x02, b'0']);
    assert_eq!(bytes(&actions), vec![0x02, b'0']);
}
