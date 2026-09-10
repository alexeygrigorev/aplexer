use crate::screen::*;

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

/// `vte` ends an `OSC`/`DCS`/`SOS`/`PM`/`APC` string at *any* `ESC`, not
/// only at the `ESC \\` of a proper `ST`: what follows the `ESC` is a new
/// sequence. A recognizer that fell back into the string on `ESC [` would
/// report "mid-sequence" until the next `BEL`/`ST`, however far away --
/// and stall every status-bar redraw with it.
#[test]
fn a_string_ends_at_esc_like_vte() {
    assert!(boundary_after(&[b"\x1b]0;title\x1b[1m"]).at_escape_boundary());
    assert!(boundary_after(&[b"\x1bPtmux;\x1b[1m"]).at_escape_boundary());
    assert!(!boundary_after(&[b"\x1b]0;title\x1b[1"]).at_escape_boundary());
    assert!(boundary_after(&[b"\x1b]0;title\x1b\\"]).at_escape_boundary());
    // The same, split at the `ESC`.
    assert!(boundary_after(&[b"\x1b]0;title\x1b", b"[1m"]).at_escape_boundary());
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
