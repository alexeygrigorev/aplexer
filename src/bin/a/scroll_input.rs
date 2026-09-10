use super::*;

/// Byte-level routing of stdin while the client owns the mouse, the pager is
/// up, or both -- the thing that guarantees a keystroke aimed at the pager
/// can never reach the workload.
///
/// Returns the bytes that may still be forwarded. In scroll mode that is
/// empty except while type-through has handed the keyboard to the workload
/// (`i`): every byte is otherwise consumed, navigation or not.
///
/// `pending` exists because a mouse report can be split across two `read()`s
/// exactly like the `Ctrl-b` prefix can. Outside scroll mode it is only ever
/// allowed to hold a buffer that has already produced the full three-byte
/// `\x1b[<` SGR introducer -- no keyboard emits that, so nothing a user
/// types can be delayed by it. A bare `ESC` or `ESC [` at the end of a chunk
/// is forwarded immediately rather than held, because holding it would make
/// the Escape key in the user's editor wait for the next keystroke.
#[derive(Default)]
pub(crate) struct ScrollInput {
    pub(crate) pending: Vec<u8>,
}

impl ScrollInput {
    pub(crate) fn route(&mut self, ctx: &StatusBarCtx, bytes: &[u8]) -> Vec<u8> {
        let client_mouse = matches!(
            *ctx.mouse_owned
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Some(true)
        );
        if !ctx.scroll.is_active() && !client_mouse {
            // Neither the pager nor the wheel is in play: the workload's
            // input path is exactly what it always was.
            let mut out = std::mem::take(&mut self.pending);
            out.extend_from_slice(bytes);
            return out;
        }
        self.pending.extend_from_slice(bytes);
        let mut out: Vec<u8> = Vec::new();
        let mut i = 0;
        while i < self.pending.len() {
            if ctx.scroll.is_active() {
                if ctx.scroll.is_typing() {
                    // Type-through (`i`): the keyboard belongs to the
                    // workload now, so bytes forward verbatim. Two things
                    // are still decoded, because neither is text the user
                    // could mean to type: an SGR mouse report (the client
                    // borrowed the mouse; the workload never asked for it),
                    // and a lone-ESC chunk, which takes the keyboard back
                    // for the pager. Anything else starting with ESC --
                    // arrows, Home, a sequence split across reads -- is
                    // somebody's key, not text, and forwards whole.
                    let rest = &self.pending[i..];
                    if rest[0] == 0x1b {
                        if rest.len() >= 3 && &rest[..3] == b"\x1b[<" {
                            match parse_sgr_mouse(rest) {
                                MouseParse::Complete(_, consumed) => {
                                    i += consumed;
                                    continue;
                                }
                                MouseParse::Incomplete => break,
                                MouseParse::NotMouse => {}
                            }
                        } else if rest.len() == 1 {
                            self.pending.drain(..i + 1);
                            exit_typing(ctx);
                            // Whatever else was typed into this same chunk
                            // was typed blind against a pager that is back
                            // in charge: discard it, exactly like the bytes
                            // that trail a pager Exit below.
                            self.pending.clear();
                            return out;
                        }
                    }
                    out.push(self.pending[i]);
                    i += 1;
                    continue;
                }
                match scroll_keys(&self.pending[i..]) {
                    ScrollKey::Command(command, n) => {
                        i += n;
                        // The buffer is advanced *before* the command runs,
                        // so an Exit landing mid-buffer leaves the bytes
                        // after it to be forwarded normally on the next turn
                        // of this loop rather than being swallowed with it.
                        self.pending.drain(..i);
                        i = 0;
                        apply_scroll_command(ctx, command);
                        if !ctx.scroll.is_active() {
                            // The command closed the pager. Everything left
                            // in this buffer was typed while the pager still
                            // had the keyboard, so it is discarded rather
                            // than forwarded: the user meant it for the
                            // pager, and "the rest of the keystroke you were
                            // reading with lands in your agent's prompt" is
                            // the exact failure this mode exists to prevent.
                            // Bytes from the next read() go to the workload
                            // normally.
                            self.pending.clear();
                            return out;
                        }
                    }
                    ScrollKey::Ignored(n) => i += n,
                    ScrollKey::Incomplete => break,
                }
                continue;
            }
            // Live, with the client holding the mouse: swallow mouse
            // reports (the workload never asked for them, so forwarding
            // would type escape sequences into it) and let a wheel roll up
            // open the pager, which is the gesture the user already has in
            // their fingers from tmux.
            let rest = &self.pending[i..];
            if rest.len() >= 3 && &rest[..3] == b"\x1b[<" {
                match parse_sgr_mouse(rest) {
                    MouseParse::Complete(report, consumed) => {
                        i += consumed;
                        if report.button == MOUSE_WHEEL_UP && report.press {
                            self.pending.drain(..i);
                            i = 0;
                            enter_scroll_mode(ctx, ScrollCommand::Up(WHEEL_LINES));
                        }
                    }
                    MouseParse::Incomplete => break,
                    MouseParse::NotMouse => {
                        out.push(self.pending[i]);
                        i += 1;
                    }
                }
                continue;
            }
            out.push(self.pending[i]);
            i += 1;
        }
        self.pending.drain(..i);
        out
    }
}
