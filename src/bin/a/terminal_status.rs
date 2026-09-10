use super::*;

pub(crate) fn format_bytes(bytes: u64) -> String {
    const KI: u64 = 1024;
    const MI: u64 = KI * 1024;
    const GI: u64 = MI * 1024;
    if bytes >= GI {
        format!("{:.1}G", bytes as f64 / GI as f64)
    } else if bytes >= MI {
        format!("{:.0}M", bytes as f64 / MI as f64)
    } else if bytes >= KI {
        format!("{:.0}K", bytes as f64 / KI as f64)
    } else {
        format!("{bytes}B")
    }
}

/// One `Operation::Status` round-trip per status-bar redraw, shared by the
/// memory and foreground-command indicators below so a single bar refresh
/// costs one worker round-trip, not one per indicator. `None` on any RPC
/// failure (worker briefly unreachable) -- every indicator built from this
/// just degrades to "omitted" in that case, same as before this was
/// shared.
pub(crate) fn live_status(record: &SessionRecord) -> Option<Value> {
    rpc_simple(record, Operation::Status, None).ok()
}

/// The attached session's record as the state derivation should see it.
///
/// `ctx.record` is a snapshot from attach/switch time, but state-report
/// pushes land in the worker's in-memory record (and on disk) with no event
/// reaching the attached client -- deriving the state from the snapshot
/// alone would trust a push that is minutes old and miss every push made
/// after attach, which is exactly the "agent started working while I
/// watched" case the spinner exists for. The Status answer already
/// serializes the worker's live record (`public_session_record`), so
/// overlay its reported-state pair and its activity stamp onto the
/// snapshot: the activity stamp is half of the `idle` push's validity rule
/// (`watch::fresh_reported_state` retracts a resting push once newer PTY
/// output appears), so deriving from the attach-time stamp would judge
/// every post-attach rest against pre-attach output -- an agent that went
/// back to work after attach would keep its stale `idle` claim forever
/// from the bar's point of view. A missing field (older worker) or a
/// failed RPC (`raw` None) leaves the snapshot untouched, same degradation
/// as the memory indicator.
pub(crate) fn overlay_reported_state(record: &SessionRecord, raw: Option<&Value>) -> SessionRecord {
    let mut fresh = record.clone();
    let Some(raw) = raw else {
        return fresh;
    };
    if let Some(s) = raw.get("reported_state").and_then(Value::as_str) {
        fresh.reported_state = Some(s.to_string());
    }
    if let Some(ms) = raw.get("reported_state_at_ms").and_then(Value::as_u64) {
        fresh.reported_state_at_ms = Some(ms);
    }
    if let Some(ms) = raw.get("last_activity_ms").and_then(Value::as_u64) {
        fresh.last_activity_ms = Some(ms);
    }
    fresh
}

/// Live memory indicator from the session's cgroup, if it has one -- a
/// small "useful for our application" touch given aplexer's whole reason
/// for existing is resource-isolated agent sessions. Best-effort: absence
/// of cgroup stats in `raw` (no cgroup configured) just omits the
/// indicator rather than disrupting the status bar.
pub(crate) fn memory_indicator(record: &SessionRecord, raw: &Value) -> Option<String> {
    let current = raw.get("cgroup")?.get("memory_current")?.as_u64()?;
    let used = format_bytes(current);
    Some(match record.limits.memory_bytes {
        Some(max) => format!("{used}/{}", format_bytes(max)),
        None => used,
    })
}

/// Plain interactive shells: showing e.g. `[shell -> bash]` for an ordinary
/// shell session would be redundant noise (that's what `shell` already
/// means), not information. Only an actually interesting foreground
/// program -- something manually run inside the session that isn't just
/// its own shell -- is worth surfacing.
pub(crate) const PLAIN_SHELLS: &[&str] =
    &["sh", "bash", "zsh", "dash", "fish", "ksh", "tcsh", "csh"];

/// The live foreground-command override for the status bar, if there's
/// anything worth showing beyond `record.engine` alone (see
/// `foreground_command` in lib.rs and `Operation::Status`'s worker-side
/// handler for where `raw["foreground_command"]` comes from -- a live,
/// never-persisted read of the pty's current foreground process, the same
/// mechanism tmux uses for `pane_current_command`). `None` when: the
/// worker didn't report one (RPC failure, no foreground process group
/// yet); it's a bare interactive shell (`PLAIN_SHELLS`); or it's just the
/// engine's own launch command running as expected (e.g. a `codex`-engine
/// session actually running `codex` shouldn't redundantly show
/// `[codex -> codex]`).
pub(crate) fn foreground_override(record: &SessionRecord, raw: &Value) -> Option<String> {
    let fg = raw.get("foreground_command")?.as_str()?;
    if PLAIN_SHELLS.contains(&fg) {
        return None;
    }
    let launched = record
        .command
        .first()
        .and_then(|c| Path::new(c).file_name())
        .and_then(|n| n.to_str());
    if launched == Some(fg) {
        return None;
    }
    Some(fg.to_string())
}

/// The detected agent's display name when it adds information beyond the
/// declared engine, `None` when it doesn't. One display rule for every
/// human surface (list rows, `a status`, the attach status bar): a session
/// declared `engine: "claude"` that is running claude says "claude" once;
/// a `shell` session running claude, or a `claude` session someone started
/// codex inside, gets the detected name appended. The engine compares by
/// family (`engine_family`): a `zcodex`-engine session running
/// zcodex says codex once, because zcodex is a codex variant, not a second
/// agent.
pub(crate) fn extra_agent_label(
    record: &SessionRecord,
    detected: Option<aplexer::agent_kind::AgentKind>,
) -> Option<&'static str> {
    let agent = detected?;
    (agent.name() != aplexer::engine_family(&record.engine)).then_some(agent.name())
}

/// The list/status engine cell. A plain `shell` workload that detection
/// found an agent inside is labeled by the agent alone: `shell` is the
/// absence of a choice, so `shell -> codex` spent the column on noise when
/// `codex` is the fact. A declared engine keeps the `engine -> agent` form,
/// where the base carries real information (a claude session someone
/// started codex inside).
pub(crate) fn engine_label(
    record: &SessionRecord,
    detected: Option<aplexer::agent_kind::AgentKind>,
) -> String {
    let agent = extra_agent_label(record, detected);
    if record.engine == "shell" {
        if let Some(agent) = agent {
            return agent.to_string();
        }
    }
    let base = match &record.profile {
        Some(profile) => format!("{}/{}", record.engine, profile),
        None => record.engine.clone(),
    };
    match agent {
        Some(agent) => format!("{base} -> {agent}"),
        None => base,
    }
}

/// `{i}:{tag}[*][({state})]` for every session in the current workspace,
/// mirroring how `a list`'s tree groups sessions by workspace (see
/// `group_by_workspace`) -- a live glance at what else is running here
/// without detaching, and (unlike the old `sibling_summary` it replaces)
/// self-documenting: `i` is exactly the number `Ctrl-b 1`..`9` jumps to
/// (`pick_switch_target`'s `Index` arm), because both walk the same
/// `list_records` order (`Reverse(created_at_ms)`) that `group_by_workspace`
/// preserves within a group -- see the equivalence note on
/// `resolve_quick_index`. `*` marks the currently attached session;
/// `(state)` is appended only when the state is not "running" (the common
/// case needs no label). Lists **all** sessions including the current one
/// (the old version listed only "the others") because the numbering only
/// makes sense as a complete index. Example: `1:main* 2:review
/// 3:build(broken)`. A single-session workspace omits the segment (empty
/// string), same as before.
pub(crate) fn workspace_summary(ctx: &StatusBarCtx, record: &SessionRecord) -> String {
    let records = match list_records(&ctx.paths) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    let siblings: Vec<SessionRecord> = records
        .into_iter()
        .filter(|r| r.workspace == record.workspace)
        .collect();
    if siblings.len() <= 1 {
        return String::new();
    }
    siblings
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let (state, _) = session_ui_state(r, now_ms());
            let mut part = format!("{}:{}", i + 1, r.tag);
            if r.id == record.id {
                part.push('*');
            }
            // Running-ish states are the expected background; anything else
            // (a reported wait, a death, a broken worker) is worth seeing
            // while attached. The same rule `workspace_summary_regions`
            // mirrors for the click map.
            if !matches!(state, "running" | "working" | "active" | "quiet") {
                part.push_str(&format!("({state})"));
            }
            part
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Makes plain status-bar data safe to interpolate into terminal output.
/// Session records and transient errors can contain arbitrary persisted or
/// remote text; C0/C1 controls (including ESC, BEL, CR, and LF) must never be
/// allowed to become terminal instructions when the bar is drawn.
pub(crate) fn sanitize_terminal_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .collect()
}

pub(crate) fn terminal_display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Pads or truncates to exactly `cols` terminal display cells without
/// splitting an extended grapheme cluster. This keeps wide glyphs, combining
/// sequences, and emoji aligned while the reverse-video bar spans the full
/// terminal width like tmux's own.
pub(crate) fn pad_or_truncate(text: &str, cols: usize) -> String {
    let cols = cols.max(1);
    let mut rendered = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = terminal_display_width(grapheme);
        if grapheme_width > cols.saturating_sub(width) {
            break;
        }
        rendered.push_str(grapheme);
        width += grapheme_width;
    }
    rendered.push_str(&" ".repeat(cols - width));
    rendered
}
