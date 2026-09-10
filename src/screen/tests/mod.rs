//! Inline tests for `aplexer::screen`, one file per unit under test.

mod boundary;
mod client;
mod margins;
mod tracker;

use super::*;

#[test]
fn zero_dimensions_use_a_non_degenerate_terminal_fallback() {
    assert_eq!(
        validate_size(0, 0).unwrap(),
        (DEFAULT_TERMINAL_ROWS, DEFAULT_TERMINAL_COLS)
    );
    assert_eq!(validate_size(0, 132).unwrap(), (DEFAULT_TERMINAL_ROWS, 132));
    assert_eq!(validate_size(43, 0).unwrap(), (43, DEFAULT_TERMINAL_COLS));

    let mut tracker = ScreenTracker::try_new(0, 0).unwrap();
    tracker.process(b"fallback remains interactive\r\n");
    assert!(tracker.contents().contains("fallback remains interactive"));
}

#[test]
fn screen_dimensions_are_bounded_and_overflow_safe() {
    assert_eq!(
        validate_size(0, 0).unwrap(),
        (DEFAULT_TERMINAL_ROWS, DEFAULT_TERMINAL_COLS)
    );
    assert_eq!(validate_size(512, 512).unwrap(), (512, 512));
    assert!(validate_size(513, 512).is_err());
    assert!(validate_size(u16::MAX, u16::MAX).is_err());
}

#[test]
fn scrollback_lines_are_clamped_against_the_cell_budget() {
    assert_eq!(scrollback_lines_for(80, 0), 0);
    assert_eq!(scrollback_lines_for(80, 2000), 2000);
    // A wild request is clamped rather than allocated.
    let clamped = scrollback_lines_for(200, 10_000_000);
    assert!(
        clamped * 200 <= MAX_SCROLLBACK_CELLS,
        "clamp exceeded the budget"
    );
    assert!(clamped > 0);
}
