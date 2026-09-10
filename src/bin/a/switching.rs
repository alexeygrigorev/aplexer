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

/// Prime a freshly built client model with a tail of the worker's retained
/// raw history, so `Ctrl-b [` has a past to page through from the first
/// second of the attach rather than only what arrives afterwards.
///
/// Failure is not an error the user should see: a worker too old to answer,
/// a session that just exited, or a history file that has been rotated all
/// mean "no history to seed", and the attach continues with an empty grid
/// that fills from the live stream.
pub(crate) fn seed_client_scrollback(
    screen: &Arc<Mutex<aplexer::screen::ClientScreen>>,
    record: &SessionRecord,
) {
    if history_limit() == 0 {
        return;
    }
    let Ok(tail) = rpc_capture(record, Some(scrollback_seed_bytes())) else {
        return;
    };
    screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .seed_history(&tail);
}

/// Rebuild the pager's history from the worker's *current* retained tail
/// before `Ctrl-b [` (or the wheel) opens it.
///
/// The attach-time seed (`seed_client_scrollback`) only helps a client that
/// attached after the output happened. A session attached from before it --
/// `a new`, the default flow -- accumulates history through the live relay,
/// where `vt100` is right to drop every row scrolled out of a DECSTBM
/// sub-range; the codex TUI holds a sub-range almost constantly, so its pager
/// opened on `SCROLL 0/0` no matter how long it had been running. Re-running
/// the seed at entry gives the live attach the same past a fresh attach gets
/// (`ClientScreen::refresh_scrollback` for the model-side mechanics).
///
/// The gate keeps every other workload exactly as it is. `subregion_seen` is
/// false for anything that never sent a sub-range -- shells, logs, tail -f --
/// and those sessions' live-maintained history is complete, so they pay
/// neither the two bounded RPCs (capture + screen, ~13-40 ms of replay
/// measured at `scrollback_seed_bytes`) nor the risk of a byte-capped replay
/// holding fewer rows than their live scrollback already does. The alternate
/// screen skips too: that grid has no scrollback anywhere, so there is
/// nothing to rebuild for a full-screen application, only RPCs to spend.
///
/// The tail is fetched **whole** (`rpc_capture`'s `None`: everything the
/// worker still retains, bounded by `DEFAULT_HISTORY_BYTES`), not at the
/// 2 MiB attach-seed budget. A byte budget is not a row budget -- an agent
/// idling between turns spends its bytes on a spinner that replays to zero
/// rows -- so a budgeted tail can rebuild to *less* than the model already
/// holds, and `refresh_scrollback` rightly refuses to adopt it: the pager
/// would fossilize at whatever its entry happened to catch instead of
/// tracking the transcript. The whole buffer is the most any rebuild can
/// ever know; its parse is bounded by the worker's retention and runs once
/// per explicit pager entry, never on the attach or `Ctrl-b` switch paths
/// (`scrollback_seed_bytes` keeps those cheap). A refusal now costs the
/// replay alone, never the history.
///
/// Failure is invisible, exactly like the attach seed: a worker that has gone
/// away or stopped answering means "page through whatever the live model
/// has", which is the pre-refresh behavior.
pub(crate) fn refresh_pager_history(ctx: &StatusBarCtx) {
    if history_limit() == 0 {
        return;
    }
    let record = ctx
        .record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    {
        let screen = ctx.screen.lock().unwrap_or_else(PoisonError::into_inner);
        if screen.alternate_screen() || !screen.subregion_seen() {
            return;
        }
    }
    let Ok(tail) = rpc_capture(&record, None) else {
        return;
    };
    if tail.is_empty() {
        return;
    }
    let Ok(snapshot) = rpc_capture_screen(&record, false) else {
        return;
    };
    ctx.screen
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .refresh_scrollback(&tail, &snapshot);
}

/// Which session a `Ctrl-b` switch chord asks for
/// (docs/fast-session-switching-design.md section 3).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SwitchTarget {
    /// `Ctrl-b Right`: next session in the current workspace.
    Next,
    /// `Ctrl-b Left`: previous session in the current workspace.
    Prev,
    /// `Ctrl-b Down`: the next workspace in `a list` order, entered at its
    /// most recently accessed session.
    NextWorkspace,
    /// `Ctrl-b Up`: the previous workspace, likewise.
    PrevWorkspace,
    /// `Ctrl-b N`: next session across all workspaces (`a list` order).
    NextGlobal,
    /// `Ctrl-b P`: previous session across all workspaces.
    PrevGlobal,
    /// `Ctrl-b l`: toggle back to whatever was attached before this one.
    Last,
    /// `Ctrl-b 1`..`9`: the Nth session
    /// (1-based) of the current workspace, no skipping -- must mean exactly
    /// what the status bar shows.
    Index(usize),
    /// `Ctrl-b n`: create a brand-new session in the attached session's
    /// workspace and switch to it. (Session navigation moved to the arrow
    /// keys, which is what freed `n` to mean "new".) The odd one out -- every
    /// other variant *selects* an existing session, this one *makes* the
    /// session it then selects -- which is why it is resolved by
    /// `create_sibling_session` in `perform_switch` rather than by
    /// `pick_switch_target`. Everything after resolution (establish, swap,
    /// `last` bookkeeping, failure containment) is the ordinary switch path.
    New,
}

/// True iff `check_attachable` would pass; used to skip dead sessions when
/// cycling with n/p/N/P (never for explicit `Index`/`Last` addressing,
/// which report the real error instead of silently hopping past it).
pub(crate) fn is_attachable(r: &SessionRecord) -> bool {
    check_attachable(r).is_ok()
}

/// Walks `group` from `current_id`'s position (or position 0 if the current
/// session isn't in this group -- e.g. it was killed underneath us) by +1
/// (`prev = false`) or -1 (`prev = true`) with wraparound, skipping
/// `current_id` itself and any candidate that fails `is_attachable`.
/// Returns `None` once every other candidate has been tried and rejected.
pub(crate) fn walk_group(
    group: &[SessionRecord],
    current_id: Uuid,
    prev: bool,
) -> Option<SessionRecord> {
    let len = group.len();
    if len == 0 {
        return None;
    }
    let start = group.iter().position(|r| r.id == current_id).unwrap_or(0);
    for step in 1..=len {
        let idx = if prev {
            (start + len - step) % len
        } else {
            (start + step) % len
        };
        let candidate = &group[idx];
        if candidate.id != current_id && is_attachable(candidate) {
            return Some(candidate.clone());
        }
    }
    None
}

/// The session a workspace is *entered* at by `Ctrl-b Down`/`Up`: the one
/// used most recently (`last_accessed_ms`, stamped whenever a client
/// attaches), which is the session a returning user means by "that
/// workspace". Ties and a group where nothing has ever been attached fall
/// back to `a list` order -- the first row, i.e. what the status bar
/// numbers `1`. Unattachable sessions are skipped, so a workspace whose
/// most recent session has since died is entered at its next-best one
/// rather than erroring; `None` means the whole group is dead, and the
/// caller moves on to the next workspace.
pub(crate) fn workspace_entry_session(group: &[SessionRecord]) -> Option<SessionRecord> {
    let mut best: Option<&SessionRecord> = None;
    for candidate in group.iter().filter(|r| is_attachable(r)) {
        let better = match best {
            // Strictly greater: on a tie the earlier (higher in `a list`)
            // row wins, so "never attached" groups enter at row 1.
            Some(current) => {
                candidate.last_accessed_ms.unwrap_or(0) > current.last_accessed_ms.unwrap_or(0)
            }
            None => true,
        };
        if better {
            best = Some(candidate);
        }
    }
    best.cloned()
}

/// Pure candidate selection over the same groups `a list` prints (see
/// `group_by_workspace`). Split from `resolve_switch_target` (the
/// paths-touching wrapper) so it is unit-testable without a filesystem.
/// Semantics are docs/fast-session-switching-design.md section 3.2:
///
/// - `Next`/`Prev`: candidates are the current session's own workspace
///   group; skips dead sessions; wraps; errors if nothing else is
///   attachable there.
/// - `NextWorkspace`/`PrevWorkspace`: candidates are whole *groups*, in that
///   same `a list` order, skipping the current one and any group with
///   nothing attachable in it; the chosen group is entered at
///   `workspace_entry_session`.
/// - `NextGlobal`/`PrevGlobal`: candidates are every group flattened in
///   `a list` workspace order (the remembered `--sort`), then list order
///   inside each group -- exactly the top-to-bottom order of `a list`.
/// - `Index(n)`: 1-based, into the current workspace group only, **no**
///   skipping of dead sessions -- the number must mean exactly what the
///   status bar shows (`workspace_summary`); an unattachable target is
///   still returned here and rejected later by `perform_switch`'s
///   `check_attachable` call, so the error names the actual session.
/// - `Last`: resolved by UUID against every group (survives renames, works
///   across workspaces).
pub(crate) fn pick_switch_target(
    groups: &[(PathBuf, Vec<SessionRecord>)],
    current_workspace: &Path,
    current_id: Uuid,
    target: SwitchTarget,
    last: Option<Uuid>,
) -> Result<SessionRecord> {
    let current_group = || -> Result<&[SessionRecord]> {
        groups
            .iter()
            .find(|(ws, _)| ws == current_workspace)
            .map(|(_, g)| g.as_slice())
            .ok_or_else(|| anyhow!("current workspace has no sessions"))
    };
    match target {
        SwitchTarget::Next | SwitchTarget::Prev => {
            let group = current_group()?;
            walk_group(group, current_id, target == SwitchTarget::Prev)
                .ok_or_else(|| anyhow!("no other running session in this workspace"))
        }
        SwitchTarget::NextWorkspace | SwitchTarget::PrevWorkspace => {
            // Workspace-level cycling, over the same top-level order `a list`
            // prints (the remembered `--sort`). The current workspace is
            // skipped, so this is always a real move; a workspace with
            // nothing attachable left in it is stepped over rather than
            // becoming an error the user has to press through.
            let len = groups.len();
            if len == 0 {
                bail!("no sessions to switch to");
            }
            let backwards = target == SwitchTarget::PrevWorkspace;
            let start = groups
                .iter()
                .position(|(ws, _)| ws == current_workspace)
                .unwrap_or(0);
            for step in 1..=len {
                let index = if backwards {
                    (start + len - step) % len
                } else {
                    (start + step) % len
                };
                let (workspace, group) = &groups[index];
                // Comparing the path (not the index) is what makes the
                // "current workspace not in the list" case -- it was just
                // killed underneath us -- consider every group, including
                // index 0, instead of silently skipping one.
                if workspace == current_workspace {
                    continue;
                }
                if let Some(entry) = workspace_entry_session(group) {
                    return Ok(entry);
                }
            }
            bail!("no other workspace has a running session")
        }
        SwitchTarget::NextGlobal | SwitchTarget::PrevGlobal => {
            let flat: Vec<SessionRecord> = groups.iter().flat_map(|(_, g)| g.clone()).collect();
            walk_group(&flat, current_id, target == SwitchTarget::PrevGlobal)
                .ok_or_else(|| anyhow!("no other running session"))
        }
        SwitchTarget::Index(n) => {
            let group = current_group()?;
            if n < 1 || n > group.len() {
                bail!(
                    "no session {n} here: this workspace has {} session(s)",
                    group.len()
                );
            }
            Ok(group[n - 1].clone())
        }
        // Not reachable through `perform_switch`, which resolves `New` by
        // *creating* the session before it ever gets here (see the variant's
        // doc comment). Spelled out rather than folded into another arm so a
        // future caller that forgets gets a named error instead of silently
        // switching somewhere arbitrary.
        SwitchTarget::New => bail!("new-session target is created, not selected"),
        SwitchTarget::Last => {
            let id = last.ok_or_else(|| anyhow!("no previous session"))?;
            groups
                .iter()
                .flat_map(|(_, g)| g.iter())
                .find(|r| r.id == id)
                .cloned()
                .ok_or_else(|| anyhow!("previous session is gone"))
        }
    }
}

pub(crate) fn resolve_switch_target(
    paths: &Paths,
    current: &SessionRecord,
    target: SwitchTarget,
    last: Option<Uuid>,
) -> Result<SessionRecord> {
    let groups = group_by_workspace(list_records(paths)?, load_list_sort(paths));
    pick_switch_target(&groups, &current.workspace, current.id, target, last)
}

/// How long `Ctrl-b c` waits for the new session's workload to come up before
/// giving up -- `a start`'s own `--startup-timeout-ms` default, because this
/// chord is `a new` with the CLI trip removed and must not be quietly less
/// patient than typing it.
pub(crate) const NEW_SESSION_STARTUP_TIMEOUT_MS: u64 = 10_000;

/// `Ctrl-b c`'s half of the chord: create another session in the attached
/// session's workspace, the way `a new` (i.e. `a start --fresh --attach`)
/// would if the user had detached to run it.
///
/// Deliberately *not* a clone of `current`: the promise is "what `a start`
/// gives me in this workspace", so engine/profile are left `None` for
/// `Config::resolve` to fill from the configured default engine (and its
/// default profile), the tag base is `DEFAULT_HUMAN_TAG` -- the same base
/// `a start`/`a new`/`a here` use -- and cwd defaults to the workspace.
/// Inheriting the attached session's engine instead would make the chord mean
/// "another one of these", which is a different (and unrequested) feature, and
/// would be surprising the moment the user is attached to a `--` command
/// session that was never meant to be spawned twice.
///
/// Tag allocation is `--fresh`'s, not a reimplementation: `start_session`
/// picks the first free `<tag>`/`<tag>-2`/`<tag>-3` … *under the registry
/// lock*, so two clients pressing `Ctrl-b c` at the same instant cannot claim
/// the same suffix, and a dead holder is still reclaimed under its own name
/// rather than skipped (tests/fresh_start.rs).
///
/// `geometry` is the host's workload-sized `(rows, cols)` (already
/// reserved-rows-adjusted, exactly what the following `establish` sends), so
/// the new worker's PTY is born at the right size and its first snapshot needs
/// no SIGWINCH repaint -- the same thing `a start --attach` does with the
/// terminal it was typed into.
///
/// Errors propagate to `perform_switch`'s caller untouched: nothing here has
/// touched the live attachment, so a bad config or an exhausted tag space is a
/// status-bar flash and the user stays exactly where they were.
pub(crate) fn create_sibling_session(
    paths: &Paths,
    current: &SessionRecord,
    geometry: Option<(u16, u16)>,
) -> Result<SessionRecord> {
    let request = aplexer::api::StartRequest {
        workspace: current.workspace.clone(),
        tag: DEFAULT_HUMAN_TAG.to_string(),
        engine: None,
        profile: None,
        cwd: None,
        env: BTreeMap::new(),
        command: Vec::new(),
        memory: None,
        pids: None,
        cpu_quota_us: None,
        cpu_period_us: 100_000,
        history_bytes: None,
        no_skip_permissions: false,
        startup_timeout_ms: NEW_SESSION_STARTUP_TIMEOUT_MS,
        worker_rows: geometry.map(|(rows, _)| rows),
        worker_cols: geometry.map(|(_, cols)| cols),
        python: None,
        // The whole point: never fail because this workspace already has a
        // `main`, take `main-2` instead.
        fresh: true,
    };
    aplexer::api::start_session(paths, &request).context("create a session in this workspace")
}

/// Result of `establish()`: the connected/subscribed stream, its initial
/// payload (either a live-screen snapshot or a raw-tail replay -- see
/// `screen`), and enough of the response to know which one it got.
pub(crate) struct AttachHandshake {
    pub(crate) reader: UnixStream,
    pub(crate) initial: Vec<u8>,
    /// The response's `"screen"` field: `Some(true)`/`Some(false)` from a
    /// worker new enough to report it, `None` from an old worker whose
    /// response predates the field entirely (docs/terminal-state-design.md
    /// section 6.1's compatibility matrix) -- used to decide whether the
    /// explicit post-connect Resize control send is still needed (section
    /// 6.3 step 7).
    pub(crate) screen: Option<bool>,
}

/// Extracted attach handshake (connect + `Operation::Attach` request +
/// response check + initial payload frame), used by both the initial
/// attach and every in-process switch (docs/fast-session-switching-design.md
/// section 3.1).
///
/// `want_screen` requests the live-screen snapshot (docs/terminal-state-design.md
/// section 6.1); `geometry`, when known (a real tty), is `(rows, cols)`
/// already reserved-rows-adjusted by the caller -- sent so the worker can
/// resize the PTY and its screen model *before* rendering the snapshot, so
/// there is no wrong-size frame followed by a SIGWINCH repaint (section
/// 6.3 step 1). An old worker's serde simply ignores these unknown request
/// fields and falls back to today's raw-tail replay -- no worse than
/// before.
pub(crate) fn establish(
    record: &SessionRecord,
    replay_bytes: Option<usize>,
    want_screen: bool,
    geometry: Option<(u16, u16)>,
) -> Result<AttachHandshake> {
    let mut reader = connect(record)?;
    let (rows, cols) = match geometry {
        Some((rows, cols)) => (Some(rows), Some(cols)),
        None => (None, None),
    };
    let request = Request::new(
        record.id,
        Operation::Attach {
            history_bytes: replay_bytes,
            want_screen,
            rows,
            cols,
        },
    );
    let id = request.request_id.clone();
    write_json(&mut reader, &request)?;
    let response: Response =
        frame_json(read_frame(&mut reader)?.ok_or_else(|| anyhow!("missing attach response"))?)?;
    if response.request_id != id {
        bail!("response request id mismatch");
    }
    let result = response.into_result()?;
    let screen = result.get("screen").and_then(|v| v.as_bool());
    let initial = read_frame(&mut reader)?.ok_or_else(|| anyhow!("missing history frame"))?;
    if initial.kind != FrameKind::Data {
        bail!("expected history data");
    }
    // Only the handshake is an RPC. Once subscribed, silence is a normal
    // state for an interactive terminal and must not detach the client.
    clear_streaming_deadlines(&reader)?;
    Ok(AttachHandshake {
        reader,
        initial: initial.payload,
        screen,
    })
}

/// Default amount of history replayed on an in-process switch
/// (`Ctrl-b n/p/N/P/l/1-9`), separate from `DEFAULT_ATTACH_REPLAY_BYTES`
/// used by a fresh `a attach`.
///
/// Deviation from docs/fast-session-switching-design.md section 3.1, which
/// specifies reusing the exact same replay budget as a fresh attach: a
/// switch target is a session the user was just attached to, or explicitly
/// picked off the status bar's own sibling list -- not a cold, unfamiliar
/// session -- so the "show what's currently on its screen" justification
/// for a full 32KB tail is considerably weaker than on first attach, and
/// every byte here sits on the hot path the user actually experiences as
/// switch latency (it's written to the real terminal, and the terminal
/// emulator parsing/painting it dominates the whole switch -- see the
/// design doc's own section 8 budget). 4KB is still ~2-4 screens of typical
/// tail -- comfortably enough for a shell prompt or an agent CLI's last few
/// lines -- at 1/8th the bytes and, per informal measurement during
/// implementation, a visibly snappier repaint than 32KB on a small
/// terminal. An explicit `--history-bytes` from the CLI is still honored
/// (see `switch_replay_bytes` in `attach()`) -- this only changes the
/// *default*, the same way `DEFAULT_ATTACH_REPLAY_BYTES` is only a default.
pub(crate) const SWITCH_REPLAY_BYTES: usize = 4 * 1024;

/// A fully established connection to the new session, handed from the
/// input thread (which runs `perform_switch`) to the main frame loop
/// (which installs it -- see the `'session` loop in `attach()`).
pub(crate) struct SwitchOutcome {
    pub(crate) record: SessionRecord,
    /// Attach handshake already completed on this socket.
    pub(crate) reader: UnixStream,
    /// The replay tail read during that handshake.
    pub(crate) history: Vec<u8>,
}

/// One button press/release reported by xterm's SGR extended mouse mode
/// (`CSI ?1006h`, paired with `CSI ?1000h` click tracking) --
/// docs/clickable-status-bar-design.md section 2. `col`/`row` are 1-based,
/// matching the wire format, so callers subtract 1 to index into
/// `BarRegion` column ranges or compare against `TermGeom.rows`.
///
/// **Not yet wired into `InputScanner`/`attach()`** -- see the design doc
/// section 7 for why this is landing as a standalone, unit-tested primitive
/// ahead of the riskier live-input-thread integration.
#[allow(dead_code)]
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

/// A clickable span of the rendered status-bar line
/// (docs/clickable-status-bar-design.md section 4.1), in 0-based display-cell
/// columns `[start, end)` -- the same units `pad_or_truncate` counts in, so
/// a click's 1-based `Cx` maps in with a single `- 1`.
///
/// **Not yet wired into `draw_status_bar`** -- see the design doc section 7.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BarRegion {
    pub(crate) cols: std::ops::Range<usize>,
    pub(crate) action: BarClick,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BarClick {
    /// Click a sibling's `{i}:{tag}` token: switch to it (`i` is exactly
    /// the digit `Ctrl-b <i>` would send, `SwitchTarget::Index`).
    Sibling(usize),
    /// Click the cross-workspace picker indicator (design doc section 5.2).
    WorkspacePicker,
    /// Click a session while browsing another workspace's sibling list
    /// (design doc section 5.2/5.3): jump straight to it by identity.
    RemoteSession(Uuid),
}

/// Pure builder mirroring `workspace_summary`'s rendering
/// (`{i}:{tag}[*][(state)]`, space-joined, `list_records` order) but also
/// returning the column range each token occupies, for the status-bar
/// click map (docs/clickable-status-bar-design.md section 4.2). `siblings`
/// must already be filtered to one workspace and ordered the way
/// `workspace_summary` expects; kept pure (no `Paths`/filesystem access)
/// so it's testable without touching disk, the same split
/// `pick_switch_target`/`resolve_switch_target` already use.
///
/// **Not yet called from `draw_status_bar`** -- see the design doc
/// section 7; `workspace_summary` (the currently-live renderer) is
/// untouched by this addition.
#[allow(dead_code)]
pub(crate) fn workspace_summary_regions(
    siblings: &[SessionRecord],
    current_id: Uuid,
) -> (String, Vec<BarRegion>) {
    let mut text = String::new();
    let mut regions = Vec::new();
    for (i, r) in siblings.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        let start = terminal_display_width(&text);
        let (state, _) = session_ui_state(r, now_ms());
        text.push_str(&format!("{}:{}", i + 1, sanitize_terminal_text(&r.tag)));
        if r.id == current_id {
            text.push('*');
        }
        if !matches!(state, "running" | "working" | "active" | "quiet") {
            text.push_str(&format!("({state})"));
        }
        let end = terminal_display_width(&text);
        regions.push(BarRegion {
            cols: start..end,
            action: BarClick::Sibling(i + 1),
        });
    }
    (text, regions)
}

/// Atomic switch-or-stay, run on the input thread
/// (docs/fast-session-switching-design.md section 3.3). The critical
/// ordering property: the new connection is fully established *before* the
/// old one is touched, so any failure (resolution, `check_attachable`, or
/// `establish` itself) leaves the attachment to the current session
/// completely undisturbed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn perform_switch(
    paths: &Paths,
    target: SwitchTarget,
    replay_bytes: Option<usize>,
    want_screen: bool,
    term: &Arc<Mutex<TermGeom>>,
    shared_record: &Arc<Mutex<SessionRecord>>,
    last_session: &Arc<Mutex<Option<Uuid>>>,
    writer: &Arc<Mutex<UnixStream>>,
    pending_switch: &Arc<Mutex<Option<SwitchOutcome>>>,
    switch_in_progress: &Arc<AtomicBool>,
) -> Result<()> {
    let current = shared_record
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let last = *last_session.lock().unwrap_or_else(PoisonError::into_inner);
    // Current terminal geometry (docs/fast-session-switching-design.md
    // section 5.2 / docs/terminal-state-design.md section 10.2): passed
    // into the Attach so the new session's snapshot renders at the right
    // size immediately, rather than relying solely on the post-switch
    // explicit Resize send below. Read before resolution because
    // `SwitchTarget::New` also hands it to the worker it starts.
    let geometry = term
        .lock()
        .ok()
        .map(|g| *g)
        .filter(|g| g.rows > 0)
        .map(|g| (reserved_rows(g.rows), g.cols));
    // `New` makes its target instead of picking one; everything below is the
    // same for both, which is what keeps "which key creates a session" a
    // one-line change in `InputScanner::scan`. Creation happens here, still
    // before the current attachment has been touched, so a failed create is
    // indistinguishable from a failed resolve: an Err, and the user stays put.
    let next = match target {
        SwitchTarget::New => create_sibling_session(paths, &current, geometry)?,
        _ => resolve_switch_target(paths, &current, target, last)?,
    };
    if next.id == current.id {
        return Ok(()); // switching to yourself: silent no-op
    }
    check_attachable(&next)?;
    switch_in_progress.store(true, Ordering::Relaxed);
    let result = (|| -> Result<()> {
        let handshake = establish(&next, replay_bytes, want_screen, geometry)?;
        let reader = handshake.reader;
        let history = handshake.initial;
        let writer_clone = reader.try_clone()?; // before mutating anything
                                                // Repoint every forwarding thread (input, resize) at B, then retire
                                                // A's stream. From this instant keystrokes land in B.
        let old = {
            let mut w = writer.lock().unwrap_or_else(PoisonError::into_inner);
            std::mem::replace(&mut *w, writer_clone)
        };
        *pending_switch
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(SwitchOutcome {
            record: next.clone(),
            reader,
            history,
        });
        *last_session.lock().unwrap_or_else(PoisonError::into_inner) = Some(current.id);
        // Polite detach from A, then shutdown so the main loop's blocked
        // read_frame on A's socket returns immediately. shutdown() is
        // socket-wide, so it also unblocks the reader fd cloned from this
        // stream -- the same mechanism the existing detach path relies on.
        let mut old = old;
        let _ = write_json(&mut old, &AttachControl::Detach);
        let _ = old.shutdown(std::net::Shutdown::Both);
        Ok(())
    })();
    switch_in_progress.store(false, Ordering::Relaxed);
    result
}

/// Closes the race where the frame loop breaks because A's worker died at
/// the same moment the user pressed a switch chord, *before* the input
/// thread finished storing the outcome: if `pending_switch` is `None` but
/// `switch_in_progress` is true, poll briefly for the outcome before giving
/// up and treating it as a normal exit (docs/fast-session-switching-design.md
/// section 5.2).
pub(crate) fn take_pending_switch(
    pending: &Arc<Mutex<Option<SwitchOutcome>>>,
    in_progress: &Arc<AtomicBool>,
) -> Option<SwitchOutcome> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if let Some(o) = pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            return Some(o);
        }
        if !in_progress.load(Ordering::Relaxed) || Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Ends the current attach from the input side. Sending the polite control
/// frame lets the worker release its subscriber immediately; shutting down
/// our socket locally guarantees the main frame loop wakes even if the peer
/// cannot read the control frame. This is shared by explicit detach, stdin
/// EOF, and terminal read/write failure so none can strand `attach()` in its
/// blocking socket read.
pub(crate) fn detach_attached_client(writer: &Arc<Mutex<UnixStream>>, active: &Arc<AtomicBool>) {
    let _ = send_control(writer, &AttachControl::Detach);
    active.store(false, Ordering::Relaxed);
    let stream = writer.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Why the attach frame loop stopped. The goodbye line is the user's
/// diagnosis of which layer to look at, so "Detached" is reserved for a
/// client that left on purpose -- a worker-side error or a dropped socket
/// must not borrow that word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachStop {
    /// Ctrl-b d, stdin EOF, or a terminal read/write failure that made this
    /// client tear its own attach down.
    ClientDetached,
    /// The worker reported the workload gone (End frame we did not cause, or
    /// `ServerEvent::Exit`).
    SessionEnded,
    /// The worker sent `ServerEvent::Error`: a PTY/waiter failure, a
    /// containment cleanup that could not be proven, or a raw-tail
    /// (`--history-bytes`) subscriber evicted for falling behind. Live-screen
    /// subscribers coalesce instead of being evicted (issue #16), so for a
    /// plain `a attach` this now means a genuine worker-side failure.
    WorkerError,
    /// The socket ended under us with no explanation: EOF, ConnectionReset,
    /// or UnexpectedEof. The worker is unreachable; the session may well
    /// still be fine.
    SocketLost,
}

/// Client intent wins over everything but a session that actually ended:
/// Ctrl-b d shuts our own stream down, so the frame loop very often then
/// observes a reset that must not be reported as a connection loss.
pub(crate) fn classify_attach_stop(
    session_ended: bool,
    detached_by_client: bool,
    worker_error: bool,
) -> AttachStop {
    if session_ended {
        AttachStop::SessionEnded
    } else if detached_by_client {
        AttachStop::ClientDetached
    } else if worker_error {
        AttachStop::WorkerError
    } else {
        AttachStop::SocketLost
    }
}

/// `inspect_id` is the short session id when a record survived the session's
/// end (Failed/OOM leftovers); a clean exit removes the record and leaves
/// nothing to point the user at.
pub(crate) fn attach_goodbye_line(
    stop: AttachStop,
    selector: &str,
    inspect_id: Option<&str>,
) -> String {
    match stop {
        AttachStop::ClientDetached => format!("Detached from {selector}."),
        AttachStop::SessionEnded => match inspect_id {
            Some(id) => format!(
                "Session ended: {selector}. Inspect output with `a capture {id} --screen --plain`."
            ),
            None => format!("Session ended: {selector}."),
        },
        AttachStop::WorkerError => format!("Attach dropped: {selector}."),
        AttachStop::SocketLost => format!("Connection to {selector} lost."),
    }
}

/// Refuse to render a session inside another live aplexer session's pane.
/// Two full-screen clients on one terminal fight over the DECSTBM margins,
/// alternate-screen state, and the `Ctrl-b` chord, and the inner session's
/// output replaces what the outer session's agent believes is on screen --
/// the same reason tmux routes `switch-client` through its server. The
/// aplexer-shaped answer is already shipped: detach (`Ctrl-]`), then let
/// the now-outer client do the in-process `Ctrl-b` switch. The inner id
/// comes from `discover_session_id` (the env var, or the ancestor /proc
/// walk when an agent runs `a attach` with a scrubbed env) and is only
/// acted on when it resolves to a live worker in *this* runtime dir, so a
/// stale id inherited from elsewhere costs one failed record read and the
/// common not-in-a-session attach pays one `getenv`. `--force` opts in for
/// a deliberate peek at the cost of the nesting above.
pub(crate) fn nested_attach_conflict(paths: &Paths) -> Result<()> {
    nested_attach_conflict_for(paths, discover_session_id())
}

/// `nested_attach_conflict` with the inner session id injected, so tests
/// never mutate process-global state.
pub(crate) fn nested_attach_conflict_for(paths: &Paths, inner: Option<Uuid>) -> Result<()> {
    let Some(inner) = inner else {
        return Ok(());
    };
    let inner_record = match read_record(&paths.record(inner)) {
        Ok(record) => record,
        Err(_) => return Ok(()),
    };
    if !inner_record.worker_alive() {
        return Ok(());
    }
    bail!(
        "already inside session {}/{}, refusing to render another nested in \
         its pane -- detach first (Ctrl-]), then switch with Ctrl-b arrows; \
         or pass --force to attach nested anyway",
        inner_record.workspace.display(),
        inner_record.tag
    )
}
