//! JSONL payload assembly -- tolerant of one-JSON-object-per-line AND of a
//! JSON object split across multiple lines (heru's `_codex_impl.py::
//! iter_codex_payloads` brace/bracket/string balance tracking).

use super::*;

// ---------------------------------------------------------------------
// JSONL payload assembly -- tolerant of one-JSON-object-per-line AND of a
// JSON object split across multiple lines (heru's `_codex_impl.py::
// iter_codex_payloads` brace/bracket/string balance tracking).
// ---------------------------------------------------------------------

/// Most bytes a multi-line value may accumulate before the assembler
/// concludes its opening line was corrupt and drops it.
const MAX_ASSEMBLY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct JsonAssembler {
    buffer: String,
    braces: i64,
    brackets: i64,
    in_string: bool,
    escaped: bool,
}

/// What one fed line produced: possibly a discarded corrupt prefix (its
/// byte length), possibly a complete payload -- both when a fresh record
/// starts right after a line that never balanced.
#[derive(Default)]
pub(crate) struct Fed {
    pub(crate) discarded: Option<usize>,
    pub(crate) payload: Option<Value>,
}

impl JsonAssembler {
    /// Feed one line (no trailing newline). Yields a complete JSON value
    /// once enough lines have been buffered to balance braces/brackets
    /// outside of any string.
    ///
    /// A corrupt line (an unterminated string, a missing brace) would
    /// otherwise leave the balance off forever and swallow every later
    /// line unbounded. So a buffered prefix is discarded, and reported,
    /// when it outgrows `MAX_ASSEMBLY_BYTES` or when a line that is a whole
    /// record by itself (opens with `{` at column 0 and balances alone)
    /// arrives on top of it.
    pub(crate) fn feed(&mut self, line: &str) -> Fed {
        let mut fed = Fed::default();
        if line.trim().is_empty() && self.buffer.is_empty() {
            return fed;
        }
        if !self.buffer.is_empty()
            && (self.buffer.len() > MAX_ASSEMBLY_BYTES || Self::is_whole_record(line))
        {
            fed.discarded = Some(self.buffer.len());
            self.clear();
        }
        if !self.buffer.is_empty() {
            self.buffer.push('\n');
        }
        self.buffer.push_str(line);
        self.update_balance(line);
        if self.is_complete() {
            let text = std::mem::take(&mut self.buffer);
            self.clear();
            fed.payload = serde_json::from_str::<Value>(text.trim()).ok();
        }
        fed
    }

    fn clear(&mut self) {
        self.buffer.clear();
        self.braces = 0;
        self.brackets = 0;
        self.in_string = false;
        self.escaped = false;
    }

    fn is_whole_record(line: &str) -> bool {
        if !line.starts_with('{') {
            return false;
        }
        let mut probe = JsonAssembler::default();
        probe.update_balance(line);
        probe.is_complete()
    }

    fn is_complete(&self) -> bool {
        !self.in_string && !self.escaped && self.braces == 0 && self.brackets == 0
    }

    fn update_balance(&mut self, text: &str) {
        for ch in text.chars() {
            if self.in_string {
                if self.escaped {
                    self.escaped = false;
                } else if ch == '\\' {
                    self.escaped = true;
                } else if ch == '"' {
                    self.in_string = false;
                }
                continue;
            }
            match ch {
                '"' => self.in_string = true,
                '{' => self.braces += 1,
                '}' => self.braces -= 1,
                '[' => self.brackets += 1,
                ']' => self.brackets -= 1,
                _ => {}
            }
        }
    }
}
