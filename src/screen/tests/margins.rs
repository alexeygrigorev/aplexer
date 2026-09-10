use crate::screen::*;

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

/// `vte` abandons a CSI on `ESC` (a new sequence starts) and on
/// `CAN`/`SUB` (back to ground); a recognizer that kept accumulating
/// would read `ESC [ 3 ESC [ 5 ; 2 0 r` as `35;20` where the grid beside
/// it sees `5;20`. Measured against the real crate.
#[test]
fn margin_tracker_aborts_a_csi_the_way_vte_does() {
    for stream in [
        &b"\x1b[3\x1b[5;20r"[..],
        b"\x1b[3;\x1b[5;20r",
        b"\x1b[3\x18\x1b[5;20r",
        b"\x1b[3;9\x1a\x1b[5;20r",
        b"\x1b\x1b[5;20r",
        b"\x1b[?\x1b[5;20r",
        b"\x1b[5;20r\x1b[3\x1a",
        b"\x1b[5;20r\x1b[3\x1bc",
        b"\x1b[5;20r\x1b\x1bc",
    ] {
        let expected = vt100_region(24, stream, &[]);
        assert_eq!(
            tracked(24, stream, &[]).margins().unwrap_or((1, 24)),
            expected,
            "{stream:?}: vt100={expected:?}"
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

/// vt100 does not ignore a malformed DECSTBM: `grid.rs::set_scroll_region`
/// clamps the bottom to the screen and falls back to the full screen
/// unless `top < bottom`. The tracker has to say the same, or the
/// snapshot re-emits a sub-range the grid beside it no longer holds.
#[test]
fn margin_tracker_out_of_range_decstbm_resets_to_full_screen_like_vt100() {
    let mut t = MarginTracker::new(24);
    t.scan(b"\x1b[3;20r");
    // top >= bottom: the full screen.
    let event = t.scan(b"\x1b[20;3r");
    assert!(event.margins_reset);
    assert_eq!(t.margins(), None);
    // bottom > rows: clamped to the screen, so `1;99` is the full screen.
    t.scan(b"\x1b[3;20r");
    let event = t.scan(b"\x1b[1;99r");
    assert!(event.margins_reset);
    assert_eq!(t.margins(), None);
    // ...and `5;99` is a bottom-anchored sub-range.
    assert!(!t.scan(b"\x1b[5;99r").margins_reset);
    assert_eq!(t.margins(), Some((5, 24)));
}

/// The attach race: the client shrinks the PTY by one row to reserve the
/// status bar, and a TUI that has not yet seen the WINCH sends the
/// bottom it knew. vt100 clamps that to the new screen -- here the whole
/// screen -- and so must the tracker, or `ClientScreen::exposed` keeps
/// treating the last row as outside a region that no longer exists.
#[test]
fn margin_tracker_pre_winch_bottom_on_a_shrunk_screen_is_full_screen() {
    let mut t = MarginTracker::new(23);
    let event = t.scan(b"\x1b[1;24r");
    assert!(event.margins_reset);
    assert_eq!(t.margins(), None);
}

/// Every DECSTBM parameter shape vt100's `canonicalize_params_decstbm`
/// gives a meaning to -- `0`, empty, a single value, a third value, a
/// `:` sub-parameter -- read back from the real crate rather than from
/// its source.
#[test]
fn margin_tracker_decstbm_parameter_shapes_match_real_vt100() {
    for params in [
        "", "0;0", "5", ";20", "0;20", "5;0", "5;20;7", "5:3;20", ":5;20", "5;20:3", "20;5", "5;5",
        "1;24", "1;25", "5;25", "24;25", "0;25", "70000;20", "5;70000",
    ] {
        let seq = format!("\x1b[{params}r");
        let expected = vt100_region(24, seq.as_bytes(), &[]);
        let mut tracker = MarginTracker::new(24);
        let event = tracker.scan(seq.as_bytes());
        let actual = tracker.margins().unwrap_or((1, 24));
        assert_eq!(actual, expected, "DECSTBM {params:?}: vt100={expected:?}");
        assert_eq!(
            event.margins_reset,
            expected == (1, 24),
            "DECSTBM {params:?} must report a reset exactly when it is the full screen"
        );
    }
}

/// Exhaustive: every `top;bottom` pair from 0 to two past the screen, at
/// every height from 2 to 24 rows, against the real crate.
#[test]
fn margin_tracker_decstbm_sweep_matches_real_vt100() {
    let mut compared = 0usize;
    for rows in 2u16..=24 {
        for top in 0..=rows + 2 {
            for bottom in 0..=rows + 2 {
                let seq = format!("\x1b[{top};{bottom}r");
                let expected = vt100_region(rows, seq.as_bytes(), &[]);
                let actual = tracked(rows, seq.as_bytes(), &[])
                    .margins()
                    .unwrap_or((1, rows));
                compared += 1;
                assert_eq!(
                    actual, expected,
                    "DECSTBM {top};{bottom} @ {rows} rows: vt100={expected:?} tracker={actual:?}"
                );
            }
        }
    }
    assert!(compared > 5_000, "sweep covered only {compared} cases");
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

/// vt100's own scroll region after `stream` at `rows` rows and the
/// resizes in `sizes`, in the probe's `(top, bottom)` convention.
fn vt100_region(rows: u16, stream: &[u8], sizes: &[u16]) -> (u16, u16) {
    let mut real = vt100::Parser::new(rows, 4, 0);
    real.process(stream);
    for &size in sizes {
        real.screen_mut().set_size(size, 4);
    }
    probe_vt100_scroll_region(&mut real)
}

/// The tracker's side of the same differential.
fn tracked(rows: u16, stream: &[u8], sizes: &[u16]) -> MarginTracker {
    let mut tracker = MarginTracker::new(rows);
    tracker.scan(stream);
    for &size in sizes {
        tracker.set_rows(size);
    }
    tracker
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
        let expected = vt100_region(before, decstbm.as_bytes(), &[after]);
        let actual = tracked(before, decstbm.as_bytes(), &[after])
            .margins()
            .unwrap_or((1, after));

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
                    let expected = vt100_region(before, decstbm.as_bytes(), &[after]);
                    let tracker = tracked(before, decstbm.as_bytes(), &[after]);
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
                        let expected = vt100_region(before, decstbm.as_bytes(), &[mid, after]);
                        let mut tracker = tracked(before, decstbm.as_bytes(), &[mid]);
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
