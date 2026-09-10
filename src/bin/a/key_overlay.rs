use super::*;

/// Whether the key overlay currently owns the host terminal.
///
/// An atomic for exactly the reason `ScrollMode::active` is one: the relay
/// reads it on every chunk, under the stdout lock, purely to decide whether
/// to write.
#[derive(Default)]

pub(crate) struct KeyOverlay {
    pub(crate) active: AtomicBool,
}

impl KeyOverlay {
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
}

/// Rows the overlay may draw into: the physical terminal less the status
/// bar's reserved row, which the box must never write over.
pub(crate) fn key_overlay_rows(geom: TermGeom) -> u16 {
    if geom.reserved {
        geom.rows.saturating_sub(1)
    } else {
        geom.rows
    }
}

/// Fit `text` into exactly `width` display cells, ellipsing rather than
/// silently amputating when it is too long.
///
/// `pad_or_truncate` is the right tool for the status bar, where a cut line
/// is obviously cut because it runs to the edge of the terminal. Inside a box
/// with a border on the right there is no such cue, so an over-long
/// description would read as a complete sentence that happens to be wrong.
pub(crate) fn fit_overlay_cell(text: &str, width: usize) -> String {
    let text = sanitize_terminal_text(text);
    if terminal_display_width(&text) <= width || width < 2 {
        return pad_or_truncate(&text, width);
    }
    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let cells = terminal_display_width(grapheme);
        if used + cells > width - 1 {
            break;
        }
        out.push_str(grapheme);
        used += cells;
    }
    out.push('\u{2026}');
    used += 1;
    out.push_str(&" ".repeat(width - used));
    out
}

/// The box, as text rows already padded to a uniform display width -- or
/// `None` when this terminal cannot hold one worth drawing, which is the
/// caller's cue to fall back to the status-bar flash.
///
/// `rows` is `key_overlay_rows`, i.e. the status row is already excluded, so
/// the box can never be laid out over the bar. Bindings come from
/// `ATTACH_BINDINGS` in table order, and that order is already "most worth
/// seeing first" (it is the order the one-line flash truncates from the right
/// of), so a short terminal trims from the end and the footer says how many
/// went.
pub(crate) fn key_overlay_lines(rows: usize, cols: usize) -> Option<Vec<String>> {
    let capacity = rows.checked_sub(KEY_OVERLAY_CHROME_ROWS)?;
    if capacity < KEY_OVERLAY_MIN_BINDINGS {
        return None;
    }
    let shown = ATTACH_BINDINGS.len().min(capacity);
    let hidden = ATTACH_BINDINGS.len() - shown;
    let bindings = &ATTACH_BINDINGS[..shown];

    let keys_width = bindings
        .iter()
        .map(|b| terminal_display_width(b.keys))
        .max()
        .unwrap_or(0);
    // "|" + " " + keys + "  " + description + " " + "|"
    let chrome = keys_width + 6;
    if cols < chrome + KEY_OVERLAY_MIN_DESC {
        return None;
    }
    let widest = bindings
        .iter()
        .map(|b| terminal_display_width(b.description))
        .max()
        .unwrap_or(0);
    let desc_width = widest.max(KEY_OVERLAY_MIN_DESC).min(cols - chrome);
    let width = chrome + desc_width;
    let inner = width - 2;

    let mut lines = Vec::with_capacity(shown + KEY_OVERLAY_CHROME_ROWS);
    // The title doubles as the answer to "what is this box": it names the key
    // the user just pressed and is waiting on.
    let title = " Ctrl-b ";
    let title_cells = terminal_display_width(title) + 1;
    lines.push(format!(
        "\u{250c}\u{2500}{title}{}\u{2510}",
        "\u{2500}".repeat(inner - title_cells)
    ));
    for binding in bindings {
        lines.push(format!(
            "\u{2502} {}  {} \u{2502}",
            pad_or_truncate(binding.keys, keys_width),
            fit_overlay_cell(binding.description, desc_width)
        ));
    }
    let footer = if hidden > 0 {
        format!("{hidden} more \u{b7} ? for all \u{b7} Esc dismiss")
    } else {
        "Esc dismiss \u{b7} any other key passes through".to_string()
    };
    lines.push(format!(
        "\u{2502} {} \u{2502}",
        fit_overlay_cell(&footer, inner - 2)
    ));
    lines.push(format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(inner)));
    Some(lines)
}

/// Position the box on the host: bottom-left, its last row immediately above
/// the status bar, which is where which-key puts it and where it covers the
/// least of what a user is usually reading.
///
/// Absolute row addressing, so the caller must have the client's own
/// full-height reservation in force -- see `paint_key_overlay`, which
/// re-asserts it for exactly this reason.
pub(crate) fn key_overlay_sequence(geom: TermGeom, lines: &[String]) -> Vec<u8> {
    let mut seq = Vec::new();
    let top = key_overlay_rows(geom).saturating_sub(lines.len() as u16) + 1;
    seq.extend_from_slice(b"\x1b[?25l");
    for (offset, line) in lines.iter().enumerate() {
        seq.extend_from_slice(format!("\x1b[{};1H", top + offset as u16).as_bytes());
        seq.extend_from_slice(b"\x1b[0m\x1b[7m");
        seq.extend_from_slice(line.as_bytes());
        seq.extend_from_slice(b"\x1b[0m");
    }
    seq.extend_from_slice(b"\x1b[?25l");
    seq
}

/// Paint the overlay: the live screen from the model, then the box on top of
/// it, then the ordinary status bar.
///
/// Repainting the whole screen first rather than only the box's rows is what
/// makes this idempotent, which is what lets the resize thread simply call it
/// again at the new geometry instead of having to know what the old box
/// covered.
///
/// The DECSTBM re-assertion after the snapshot mirrors `paint_scroll_view`'s
/// and is there for the same reason: the box addresses rows absolutely, and
/// the snapshot may have just restored a workload sub-range (and, with it,
/// an origin mode that would make those rows relative to it). The dismissal
/// repaint puts the workload's own region back.
pub(crate) fn paint_key_overlay(ctx: &StatusBarCtx) -> bool {
    let geom = match ctx.term.lock() {
        Ok(g) => *g,
        Err(_) => return false,
    };
    let Some(lines) = key_overlay_lines(key_overlay_rows(geom) as usize, geom.cols as usize) else {
        return false;
    };
    // Reads session records off disk; must not happen under the stdout lock.
    let bar = status_bar_render(ctx);
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let snapshot = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = screen.snapshot();
        screen.filter_host(&snapshot).unwrap_or(snapshot)
    };
    let mut seq = SCROLL_CANCEL.to_vec();
    seq.extend_from_slice(&snapshot);
    if geom.reserved {
        seq.extend_from_slice(format!("\x1b[1;{}r", geom.rows - 1).as_bytes());
    }
    seq.extend_from_slice(&key_overlay_sequence(geom, &lines));
    if let Some((bar_geom, text)) = bar {
        // The pager's flavour of the bar row: no workload cursor restore, and
        // the cursor left hidden. The workload's cursor is not what the user
        // is looking at while a modal is up, and parking it inside the box
        // would just make the box look broken.
        seq.extend_from_slice(&scroll_bar_sequence(bar_geom, &text));
        *ctx.last_drawn
            .lock()
            .unwrap_or_else(PoisonError::into_inner) =
            Some((text, bar_geom.rows, bar_geom.cols, None));
    }
    write_client_locked(
        &mut *out,
        &ctx.screen,
        &seq,
        BoundaryPolicy::StreamSuspended,
    )
}

/// Put the keymap on screen for a `Ctrl-b` the user is still thinking about,
/// and suspend the relay behind it.
///
/// Returns whether the box actually went up. `false` means this terminal is
/// too small for one and the one-line reference was flashed on the status bar
/// instead -- the honest degradation, decided *before* anything is suspended
/// so the fallback path never suspends the relay at all.
pub(crate) fn show_key_overlay(ctx: &StatusBarCtx) -> bool {
    if ctx.scroll.is_active() {
        // The pager has its own key routing (`ScrollInput::route`) and its own
        // full-screen view; a second modal on top of it would describe keys
        // that are not the ones in force.
        return false;
    }
    let fits = match ctx.term.lock() {
        Ok(geom) => {
            key_overlay_lines(key_overlay_rows(*geom) as usize, geom.cols as usize).is_some()
        }
        Err(_) => false,
    };
    if !fits {
        flash_status(ctx, attach_key_help());
        return false;
    }
    {
        // Flipped under the stdout lock -- the same lock `relay_to_terminal`
        // reads it under -- so a workload chunk cannot be half-written across
        // the overlay's first frame. Exactly `enter_scroll_mode`'s reasoning.
        let _held = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
        if ctx.overlay.active.swap(true, Ordering::SeqCst) {
            return true;
        }
    }
    // The bar is about to be drawn by a writer that is not the dirty-check's
    // usual one, and again on the way out.
    *ctx.last_drawn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    if paint_key_overlay(ctx) {
        true
    } else {
        // Never leave the relay suspended behind a box that did not get
        // drawn: the user would be looking at a frozen terminal.
        dismiss_key_overlay(ctx);
        false
    }
}

/// Take the overlay down, put the screen back, and let the relay resume.
/// Returns whether there was an overlay to take down.
///
/// The restore is `ClientScreen::snapshot` -- the same repaint `Ctrl-b r` and
/// `exit_scroll_mode` write, and deliberately not a saved rectangle of cells.
/// A rectangle would be a photograph of the screen as it was when the box
/// went up; the model has been fed every byte that arrived since, so the
/// snapshot is the screen as it *is*. The bar is rewritten in the same
/// sequence because the snapshot's `ED2` blanks its reserved row.
pub(crate) fn dismiss_key_overlay(ctx: &StatusBarCtx) -> bool {
    if !ctx.overlay.is_active() {
        return false;
    }
    // Reads session records off disk; must not happen under the stdout lock.
    let bar = status_bar_render(ctx);
    let mut out = ctx.stdout.lock().unwrap_or_else(PoisonError::into_inner);
    let (snapshot, restore, margins) = {
        let mut screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let snapshot = screen.snapshot();
        let snapshot = screen.filter_host(&snapshot).unwrap_or(snapshot);
        (snapshot, screen.cursor_restore(), screen.margins())
    };
    let mut seq = SCROLL_CANCEL.to_vec();
    seq.extend_from_slice(&snapshot);
    if let Some((geom, text)) = bar {
        seq.extend_from_slice(&status_bar_sequence(geom, &text, margins, &restore));
    }
    write_client_locked(
        &mut *out,
        &ctx.screen,
        &seq,
        BoundaryPolicy::StreamSuspended,
    );
    *ctx.last_drawn
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = None;
    ctx.overlay.active.store(false, Ordering::SeqCst);
    true
}
