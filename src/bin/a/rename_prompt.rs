use super::*;

/// The `Ctrl-b R` rename prompt: a tmux-style `rename:` line that takes over
/// the reserved status-bar row until Enter or Esc.
///
/// It is deliberately *not* a second modal like the pager or the which-key
/// overlay. Those own the whole screen and must suspend the relay behind
/// them; the prompt owns only the row the workload can never write to, so
/// the relay keeps streaming underneath it untouched and nothing has to be
/// suspended or restored. The cost is that the prompt must win every race
/// for the row -- which is why it renders through `StatusBarCtx::prompt`
/// (status_bar_text prefers it over everything, including a flash) rather
/// than by drawing once and hoping the status thread never repaints.
///
/// Editing is append-at-the-end plus backspace, like tmux's own prompt in
/// its minimal mood: no cursor motion, so no cursor to manage on a row that
/// three other writers redraw. The caret is drawn as a full-block glyph
/// after the input -- the row itself is reverse video, so the glyph reads as
/// a block in the background color, which is exactly where a text cursor
/// belongs.
#[derive(Default)]
pub(crate) struct RenamePromptState {
    /// The tag being typed, as whole chars (a multi-byte char only enters
    /// once its last byte has arrived).
    pub(crate) input: String,
    /// Bytes of a not-yet-complete UTF-8 char, parked between reads.
    pending_utf8: Vec<u8>,
    /// Why the last submit was refused, shown after the input until the
    /// next edit. The worker's own refusal (a live session already owning
    /// the pair) is the useful one; `validate_tag` just catches the silly
    /// ones without a round-trip.
    pub(crate) error: Option<String>,
}

/// The rendered line. `rename: <input>` + caret, plus the submit error when
/// there is one. Split out for tests -- the prompt loop's only other
/// observable is the terminal.
pub(crate) fn rename_prompt_line(input: &str, error: Option<&str>) -> String {
    let mut line = format!("rename: {input}\u{2588}");
    if let Some(error) = error {
        line.push_str(&format!("  {error}"));
    }
    line
}

/// What one input byte asks the prompt loop to do.
pub(crate) enum PromptKey {
    /// Stay open; the state changed (or deliberately did not) -- repaint.
    Edit,
    /// Enter: commit (or surface the refusal) -- `false` keeps the prompt
    /// open with the error shown, `true` closes it.
    Submit,
    /// Esc or Ctrl-c: abandon the rename.
    Cancel,
    /// Ctrl-b: abandon the rename *and* re-arm the prefix scanner, so
    /// `Ctrl-b R Ctrl-b d` detaches instead of typing `d` into the prompt.
    PrefixThenCancel,
}

impl RenamePromptState {
    pub(crate) fn key(&mut self, byte: u8) -> PromptKey {
        match byte {
            b'\r' | b'\n' => PromptKey::Submit,
            // A bare Esc cancels. An arrow chord also starts with Esc, and
            // with the caret permanently at the end there is nothing for an
            // arrow to do in this prompt anyway: it cancels too, and the
            // rest of the sequence forwards to the workload like any
            // unbound input once the prompt is gone.
            0x1b | 0x03 => PromptKey::Cancel,
            0x02 => PromptKey::PrefixThenCancel,
            0x08 | 0x7f => {
                self.input.pop();
                PromptKey::Edit
            }
            // Ctrl-u clears the line, as every readline-flavored prompt does.
            0x15 => {
                self.input.clear();
                PromptKey::Edit
            }
            // Printable ASCII appends; every other control byte is ignored
            // outright -- a stray Ctrl-a or Tab must neither type into the
            // tag nor look like it did (validate_tag would refuse them at
            // submit, but the prompt row should never show a sanitized '?'
            // in the meantime).
            byte if (0x20..0x7f).contains(&byte) => {
                self.pending_utf8.clear();
                self.input.push(byte as char);
                PromptKey::Edit
            }
            byte if byte < 0x80 => PromptKey::Edit,
            byte => {
                self.pending_utf8.push(byte);
                match std::str::from_utf8(&self.pending_utf8) {
                    Ok(text) => {
                        self.input.push_str(text);
                        self.pending_utf8.clear();
                    }
                    // Still mid-char: keep waiting for the rest, repaint not
                    // needed (handled by the changed-line check).
                    Err(error) if error.error_len().is_none() => {}
                    Err(_) => self.pending_utf8.clear(),
                }
                PromptKey::Edit
            }
        }
    }

    /// Enter. `true` closes the prompt. Empty input closes without a
    /// round-trip (tmux's own "renamed to nothing is no rename"); a refused
    /// rename keeps the prompt open with the reason on the line, so fixing
    /// a colliding tag does not start over.
    fn submit(&mut self, config: &InputThreadConfig) -> bool {
        let tag = self.input.clone();
        if tag.is_empty() {
            return true;
        }
        if let Err(error) = validate_tag(&tag) {
            self.error = Some(format!("{error}"));
            return false;
        }
        let current = config
            .status
            .record
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        // Same workspace, new tag: the prompt renames within the workspace,
        // the way `Ctrl-b R` reads. Moving a session between workspaces is
        // `a rename --workspace`, not a typing exercise.
        let operation = Operation::Rename {
            workspace: current.workspace.clone(),
            tag,
        };
        match rpc_simple(&current, operation, None) {
            Ok(value) => {
                let renamed: SessionRecord = match serde_json::from_value(value) {
                    Ok(record) => record,
                    Err(error) => {
                        self.error = Some(format!("rename reply unreadable: {error}"));
                        return false;
                    }
                };
                // The bar renders identity from this record; swap it before
                // the closing redraw so the tag never flashes back to the
                // old one.
                *config
                    .status
                    .record
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = renamed.clone();
                *config
                    .status
                    .prompt
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                flash_status(&config.status, format!("renamed to {}", renamed.selector()));
                true
            }
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                false
            }
        }
    }
}

/// Run the prompt to completion on the input thread. Takes over stdin reads
/// for the duration -- the scanner is not consulted, so no keystroke
/// reaches the workload and none of the chords fire -- and always leaves
/// the row back in the status thread's hands.
///
/// `scanner` is only touched for `PrefixThenCancel`, which re-arms the
/// prefix so the key *after* a mid-prompt `Ctrl-b` is read as a chord, not
/// typed into anything.
pub(crate) fn run_rename_prompt(config: &InputThreadConfig, scanner: &mut InputScanner) {
    let mut state = RenamePromptState::default();
    draw_prompt(&state, &config.status);
    let mut input = io::stdin();
    let mut buffer = [0u8; 256];
    'prompt: loop {
        let n = match input.read(&mut buffer) {
            Ok(0) => break 'prompt,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break 'prompt,
            Ok(n) => n,
        };
        for &byte in &buffer[..n] {
            match state.key(byte) {
                PromptKey::Edit => draw_prompt(&state, &config.status),
                PromptKey::Submit => {
                    if state.submit(config) {
                        break 'prompt;
                    }
                    // Refused: the error is on the line now.
                    draw_prompt(&state, &config.status);
                }
                PromptKey::Cancel => break 'prompt,
                PromptKey::PrefixThenCancel => {
                    scanner.pending_ctrl_b = true;
                    break 'prompt;
                }
            }
        }
    }
    // Every exit path hands the row back -- including EOF/error, where the
    // outer loop is about to detach anyway but the row must not keep
    // showing a prompt nobody can edit.
    *config
        .status
        .prompt
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    draw_status_bar(&config.status, true);
}

/// Publish the prompt's line and force a bar redraw. The changed-line check
/// keeps no-op keys (an ignored control byte, mid-char UTF-8 bytes) from
/// writing an identical row over and over.
fn draw_prompt(state: &RenamePromptState, status: &StatusBarCtx) {
    let line = rename_prompt_line(&state.input, state.error.as_deref());
    let mut slot = status.prompt.lock().unwrap_or_else(PoisonError::into_inner);
    if slot.as_deref() == Some(line.as_str()) {
        return;
    }
    *slot = Some(line);
    drop(slot);
    draw_status_bar(status, true);
}
