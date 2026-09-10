/// One button press/release reported by xterm's SGR extended mouse mode
/// (`CSI ?1006h`, paired with `CSI ?1000h` click tracking) --
/// docs/clickable-status-bar-design.md section 2. `col`/`row` are 1-based,
/// matching the wire format, so callers subtract 1 to index into
/// `BarRegion` column ranges or compare against `TermGeom.rows`.
///
/// **Not yet wired into `InputScanner`/`attach()`** -- see the design doc
/// section 7 for why this is landing as a standalone, unit-tested primitive
/// ahead of the riskier live-input-thread integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MouseReport {
    pub(crate) button: u32,
    pub(crate) press: bool,
    pub(crate) col: u16,
    pub(crate) row: u16,
}

/// Result of attempting to parse an SGR mouse report off the front of a
/// buffer: a real hit (with the byte length consumed), "not this at all"
/// (any other byte sequence, including ordinary CSI sequences like arrow
/// keys -- `ESC [ <` is not a prefix any keyboard-generated input or other
/// terminal report uses, so this is an unambiguous, fast rejection), or
/// "looks like the start of one but the buffer ends before `M`/`m`" -- the
/// signal a live scanner needs to keep buffering across `read()` calls, the
/// same role `pending_ctrl_b` plays for the one-byte `Ctrl-b` prefix.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseParse {
    NotMouse,
    Incomplete,
    Complete(MouseReport, usize),
}

/// Pure parser for `ESC [ < Cb ; Cx ; Cy [Mm]` at the start of `buf`
/// (docs/clickable-status-bar-design.md section 2/4.4). Never panics on
/// malformed input; malformed-but-prefix-matching input that can't
/// possibly resolve (non-digit where a number is expected, once the `<`
/// has been seen) is reported `NotMouse` rather than `Incomplete`, so a
/// caller doesn't buffer forever waiting for a `M`/`m` that will never
/// come.
#[allow(dead_code)]
pub(crate) fn parse_sgr_mouse(buf: &[u8]) -> MouseParse {
    const PREFIX: &[u8] = b"\x1b[<";
    if buf.len() < PREFIX.len() {
        if PREFIX.starts_with(buf) {
            return MouseParse::Incomplete;
        }
        return MouseParse::NotMouse;
    }
    if &buf[..PREFIX.len()] != PREFIX {
        return MouseParse::NotMouse;
    }
    // Three ';'-separated decimal fields, terminated by 'M' (press) or 'm'
    // (release). Parse by scanning for the terminator rather than
    // pre-splitting, so a genuinely truncated buffer (no terminator yet)
    // is correctly reported Incomplete instead of NotMouse.
    let rest = &buf[PREFIX.len()..];
    let mut fields: [u32; 3] = [0; 3];
    let mut field_idx = 0;
    let mut cur: u32 = 0;
    let mut have_digit = false;
    for (i, &b) in rest.iter().enumerate() {
        match b {
            b'0'..=b'9' => {
                have_digit = true;
                cur = cur.saturating_mul(10).saturating_add((b - b'0') as u32);
            }
            b';' => {
                if !have_digit || field_idx >= 2 {
                    return MouseParse::NotMouse;
                }
                fields[field_idx] = cur;
                field_idx += 1;
                cur = 0;
                have_digit = false;
            }
            b'M' | b'm' => {
                if !have_digit || field_idx != 2 {
                    return MouseParse::NotMouse;
                }
                fields[2] = cur;
                let consumed = PREFIX.len() + i + 1;
                let row = u16::try_from(fields[2]).unwrap_or(u16::MAX);
                let col = u16::try_from(fields[1]).unwrap_or(u16::MAX);
                return MouseParse::Complete(
                    MouseReport {
                        button: fields[0],
                        press: b == b'M',
                        col,
                        row,
                    },
                    consumed,
                );
            }
            _ => return MouseParse::NotMouse,
        }
    }
    // Ran out of buffer with no terminator yet, but every byte seen so far
    // was a valid digit/`;` -- genuinely incomplete, keep buffering.
    MouseParse::Incomplete
}
