/// Filters alt-screen DECSET/DECRST out of bytes written to the *host*
/// terminal, so `a attach` can keep the host on the alternate screen for the
/// whole client lifetime.
///
/// The pre-attach primary screen (the `a` session list, the user's shell)
/// stays frozen underneath. Host scrollback therefore cannot mix those rows
/// into the live view -- the failure in the screenshot of a scrolled-up
/// attach. The workload's own 1049h/1049l still update `ScreenTracker`; they
/// just must not switch the host, or a TUI exiting alt-screen would reveal
/// the primary list mid-attach. Combined DECSET lists keep every other mode
/// (`CSI ? 1049;2004 h` becomes `CSI ? 2004 h`). Incomplete CSIs are held
/// across `push` calls so a split `\x1b[?1049` / `l` cannot leak a 1049l.
#[derive(Debug, Default)]
pub(super) struct HostAltHold {
    state: HoldState,
    held: Vec<u8>,
    params: Vec<u8>,
    private: bool,
    intermediate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum HoldState {
    #[default]
    Ground,
    Esc,
    Csi,
}

impl HostAltHold {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    /// True when `data` would come out of `push` byte-for-byte: nothing is
    /// held from an earlier chunk and nothing in this one can start a
    /// sequence.
    pub(super) fn passes_through(&self, data: &[u8]) -> bool {
        self.state == HoldState::Ground && !data.contains(&0x1b)
    }

    pub(super) fn push(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        for &byte in data {
            self.step(byte, &mut out);
        }
        out
    }

    fn step(&mut self, byte: u8, out: &mut Vec<u8>) {
        match self.state {
            HoldState::Ground => {
                if byte == 0x1b {
                    self.state = HoldState::Esc;
                    self.held.clear();
                    self.held.push(byte);
                } else {
                    out.push(byte);
                }
            }
            HoldState::Esc => {
                self.held.push(byte);
                if byte == b'[' {
                    self.state = HoldState::Csi;
                    self.params.clear();
                    self.private = false;
                    self.intermediate = false;
                } else if byte == 0x1b {
                    out.extend_from_slice(&self.held[..self.held.len() - 1]);
                    self.held.clear();
                    self.held.push(0x1b);
                } else {
                    out.extend_from_slice(&self.held);
                    self.held.clear();
                    self.state = HoldState::Ground;
                }
            }
            HoldState::Csi => {
                if (0x40..=0x7e).contains(&byte) {
                    self.held.push(byte);
                    if self.private && !self.intermediate && (byte == b'h' || byte == b'l') {
                        out.extend_from_slice(&filter_alt_screen_modes(&self.params, byte));
                    } else {
                        out.extend_from_slice(&self.held);
                    }
                    self.held.clear();
                    self.state = HoldState::Ground;
                } else if byte == 0x1b {
                    out.extend_from_slice(&self.held);
                    self.held.clear();
                    self.held.push(0x1b);
                    self.state = HoldState::Esc;
                } else {
                    self.held.push(byte);
                    if byte == b'?' && self.params.is_empty() && !self.intermediate {
                        self.private = true;
                    } else if (0x20..=0x2f).contains(&byte) {
                        self.intermediate = true;
                    } else if self.private && !self.intermediate {
                        self.params.push(byte);
                    }
                }
            }
        }
    }
}

/// Bytes after `CSI ?` in a DECSET/DECRST. Drops 47/1047/1048/1049 and
/// rebuilds the sequence from whatever remains; empty if nothing remains.
fn filter_alt_screen_modes(params: &[u8], final_byte: u8) -> Vec<u8> {
    let mut kept: Vec<&[u8]> = Vec::new();
    for part in params.split(|b| *b == b';') {
        if part.iter().all(|b| b.is_ascii_digit()) && mode_is_alt_screen(part) {
            continue;
        }
        kept.push(part);
    }
    if kept.is_empty() {
        return Vec::new();
    }
    let mut seq = b"\x1b[?".to_vec();
    for (i, part) in kept.iter().enumerate() {
        if i > 0 {
            seq.push(b';');
        }
        seq.extend_from_slice(part);
    }
    seq.push(final_byte);
    seq
}

fn mode_is_alt_screen(num: &[u8]) -> bool {
    let mut n = 0u32;
    for &d in num {
        n = n.saturating_mul(10).saturating_add(u32::from(d - b'0'));
    }
    matches!(n, 47 | 1047 | 1048 | 1049)
}
