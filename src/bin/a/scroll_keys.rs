use super::*;

/// One navigation step, resolved against the viewport height by
/// `apply_scroll_command`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollCommand {
    /// Enter the pager without moving (`Ctrl-b [`).
    Stay,
    Up(usize),
    Down(usize),
    PageUp,
    PageDown,
    HalfUp,
    HalfDown,
    Top,
    Bottom,
    /// `i`: hand the keyboard to the workload while staying in the pager
    /// (type-through; a lone `Esc` takes it back).
    TypeThrough,
    /// `q`, `Esc` or `Ctrl-C`: back to the live screen.
    Exit,
}

/// What `scroll_keys` made of the bytes at the front of the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollKey {
    /// A navigation command, and how many bytes it consumed.
    Command(ScrollCommand, usize),
    /// Recognized and deliberately swallowed (a non-wheel mouse report, an
    /// unbound key). **Consumed, never forwarded** -- that is the whole
    /// point of the mode: while the pager is up, no keystroke reaches the
    /// workload (until `i` hands the keyboard over; see `TypeThrough`).
    Ignored(usize),
    /// A sequence that has begun but not finished in this buffer. The caller
    /// keeps the bytes and retries when more arrive.
    Incomplete,
}

/// Keyboard and mouse decoding for scroll mode. Pure, so the split-sequence
/// and modifier cases are unit-testable without a terminal.
///
/// Bindings follow tmux copy-mode where tmux has one and `less` elsewhere,
/// because those are the two muscle memories a user arrives with:
/// arrows/`j`/`k` by the line, PageUp/PageDown and Space/`b` by the screen,
/// `Ctrl-U`/`Ctrl-D`/`u`/`d` by the half screen, Home/`g` and End/`G` to the
/// ends, `q`/`Esc`/`Ctrl-C` back to live, and the wheel by
/// `WHEEL_LINES`.
///
/// A lone `ESC` that is the *entire* remaining buffer is read as the Escape
/// key, not as the start of a sequence that has not arrived yet. Real
/// terminals emit `ESC [ A` for an arrow key in one write, so the ambiguity
/// is only theoretically reachable, and resolving it the other way would
/// mean Escape did nothing until the user pressed another key -- much worse
/// than the rare case of a split arrow key exiting the pager.
pub(crate) fn scroll_keys(buf: &[u8]) -> ScrollKey {
    use ScrollCommand::*;
    let Some(&first) = buf.first() else {
        return ScrollKey::Incomplete;
    };
    if first != 0x1b {
        let command = match first {
            b'q' | b'Q' | 0x03 => Some(Exit),
            b'k' | b'y' => Some(Up(1)),
            b'j' | b'e' => Some(Down(1)),
            b' ' | b'f' | 0x06 => Some(PageDown),
            b'b' | 0x02 => Some(PageUp),
            b'u' | 0x15 => Some(HalfUp),
            b'd' | 0x04 => Some(HalfDown),
            b'g' => Some(Top),
            b'G' => Some(Bottom),
            b'i' => Some(TypeThrough),
            _ => None,
        };
        return match command {
            Some(c) => ScrollKey::Command(c, 1),
            None => ScrollKey::Ignored(1),
        };
    }
    if buf.len() == 1 {
        return ScrollKey::Command(Exit, 1);
    }
    match buf[1] {
        b'[' => {
            if buf.len() == 2 {
                return ScrollKey::Incomplete;
            }
            if buf[2] == b'<' {
                return match parse_sgr_mouse(buf) {
                    MouseParse::Complete(report, consumed) => {
                        // Wheel reports repeat on press only; the release
                        // report a terminal may pair with them is swallowed
                        // by the `Ignored` arm below, so one notch moves
                        // WHEEL_LINES exactly once.
                        match (report.button, report.press) {
                            (MOUSE_WHEEL_UP, true) => ScrollKey::Command(Up(WHEEL_LINES), consumed),
                            (MOUSE_WHEEL_DOWN, true) => {
                                ScrollKey::Command(Down(WHEEL_LINES), consumed)
                            }
                            _ => ScrollKey::Ignored(consumed),
                        }
                    }
                    MouseParse::Incomplete => ScrollKey::Incomplete,
                    MouseParse::NotMouse => ScrollKey::Ignored(1),
                };
            }
            // A generic CSI: scan to the final byte, so `\x1b[5;2~`
            // (shifted PageUp) resolves the same as `\x1b[5~`.
            let Some(end) = buf[2..]
                .iter()
                .position(|b| (0x40..=0x7e).contains(b))
                .map(|i| i + 2)
            else {
                // Bounded, so a stray `ESC [` followed by a stream of digits
                // cannot buffer forever.
                return if buf.len() > 32 {
                    ScrollKey::Ignored(buf.len())
                } else {
                    ScrollKey::Incomplete
                };
            };
            let consumed = end + 1;
            let params = &buf[2..end];
            let leading: u32 = params
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .fold(0u32, |acc, b| {
                    acc.saturating_mul(10).saturating_add(u32::from(b - b'0'))
                });
            let command = match (buf[end], leading) {
                (b'A', _) => Some(Up(1)),
                (b'B', _) => Some(Down(1)),
                (b'H', _) => Some(Top),
                (b'F', _) => Some(Bottom),
                (b'~', 1 | 7) => Some(Top),
                (b'~', 4 | 8) => Some(Bottom),
                (b'~', 5) => Some(PageUp),
                (b'~', 6) => Some(PageDown),
                _ => None,
            };
            match command {
                Some(c) => ScrollKey::Command(c, consumed),
                None => ScrollKey::Ignored(consumed),
            }
        }
        b'O' => {
            if buf.len() == 2 {
                return ScrollKey::Incomplete;
            }
            let command = match buf[2] {
                b'A' => Some(Up(1)),
                b'B' => Some(Down(1)),
                b'H' => Some(Top),
                b'F' => Some(Bottom),
                _ => None,
            };
            match command {
                Some(c) => ScrollKey::Command(c, 3),
                None => ScrollKey::Ignored(3),
            }
        }
        _ => ScrollKey::Ignored(2),
    }
}
