use super::*;

/// `a completions <shell>` -- writes the clap_complete-generated script for
/// the given shell to stdout, completing for the `a` binary name itself
/// (from `#[command(name = "a")]` on `Cli` above, not the `aplexer` package
/// name), so callers just redirect it into whatever path their shell's
/// completion loader scans.
pub(crate) fn cmd_completions(args: CompletionsArgs) -> Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    generate(args.shell, &mut cmd, name, &mut io::stdout());
    Ok(())
}

/// `a hotkeys` -- a lookup command for the attach-mode Ctrl-b chords,
/// rendered from `ATTACH_BINDINGS`, the same table the attach status bar's
/// `?` flash (`attach_key_help`) renders. There is one authoritative keymap
/// and one place it is written down; this just prints it somewhere you can
/// look it up without already being attached.
pub(crate) fn cmd_hotkeys() -> Result<()> {
    println!("Attach-mode keys (press Ctrl-b, then one of these):");
    println!();
    let width = ATTACH_BINDINGS
        .iter()
        .map(|b| b.keys.len())
        .max()
        .unwrap_or(0);
    for binding in ATTACH_BINDINGS {
        println!(
            "  {:width$}  {}",
            binding.keys,
            binding.description,
            width = width
        );
    }
    println!();
    println!("Hold Ctrl-b without pressing anything and this list appears on screen;");
    println!("the next key runs its binding and takes it away again.");
    println!();
    println!("Any other key after Ctrl-b is forwarded through untouched.");
    println!();
    println!("Scrolling back (aplexer's copy-mode, like tmux's Ctrl-b [):");
    println!();
    println!("  the mouse wheel enters it on its own, with no prefix -- unless the");
    println!("  workload has asked the terminal for the mouse itself, in which case");
    println!("  the wheel belongs to the workload and Ctrl-b [ is the way in.");
    println!("  While the pager is up the wheel stays with the pager, so a TUI that");
    println!("  later enables the mouse cannot turn the next notch into prompt history.");
    println!();
    println!("  PgUp/PgDn  a screen at a time      Up/Down, k/j   a line at a time");
    println!("  Home / End top / back to live      g / G          the same");
    println!("  Space / b  a screen at a time      u / d          half a screen");
    println!("  q, Esc     back to the live screen");
    println!();
    println!("  While scrolling, keys go to the pager and never to the session --");
    println!("  press i to hand the keyboard to the session anyway (Esc pages again).");
    println!(
        "  History is {} lines by default (APLEXER_HISTORY_LIMIT);",
        aplexer::screen::DEFAULT_SCROLLBACK_LINES
    );
    println!("  APLEXER_MOUSE=off leaves the mouse to the terminal for selection.");
    Ok(())
}

/// `a watch --jsonl [--all] [--workspace PATH]` -- see src/watch.rs for the
/// poll/diff loop and the heru UnifiedEvent mapping it emits.
pub(crate) fn cmd_watch(paths: &Paths, args: WatchArgs) -> Result<()> {
    if !args.jsonl {
        bail!("a watch currently requires --jsonl (no other output format is implemented yet)");
    }
    let workspace = args
        .workspace
        .as_deref()
        .map(canonical_workspace)
        .transpose()?;
    aplexer::watch::run(paths, args.all, workspace.as_deref())
}

/// `a transcript [SESSION] [--last N] [--after SEQ] [--before SEQ]
/// [--kind K] [--follow] [--json]` -- parse the native conversation log of
/// an aplexer session (the JSONL the engine CLI already writes) into heru
/// UnifiedEvent JSONL. PocketShell's conversation pane is the consumer:
/// last-N for the initial view, `--before` for older pages, `--after` plus
/// `--follow` for live tail. See src/agent_events.rs for capture/bind.
///
/// With no SESSION, `--workspace`, or `--tag`, falls back to
/// `$APLEXER_SESSION_ID` (`a whoami`) so an agent or hook inside a session
/// can dump its own log without addressing itself.
pub(crate) fn cmd_transcript(paths: &Paths, args: TranscriptArgs, json_output: bool) -> Result<()> {
    let record = resolve_transcript_target(paths, &args)?;
    let bind_path = paths.state_session(record.id).join("transcript.json");
    let located = aplexer::agent_events::resolve_transcript(&record, &bind_path)?;
    let path = located.path;
    if !json_output && !args.follow {
        println!("transcript: {} (engine {})", path.display(), record.engine);
    }
    aplexer::agent_events::run_transcript(
        &record,
        &path,
        aplexer::agent_events::TranscriptQuery {
            last: args.last,
            kind: args.kind.clone(),
            after: args.after,
            before: args.before,
            follow: args.follow,
            max_line_bytes: args.max_line_bytes,
        },
        json_output,
    )
}

/// Prefer an explicit selector; otherwise the session this process is
/// running inside (`APLEXER_SESSION_ID` from worker spawn / `a whoami`).
pub(crate) fn resolve_transcript_target(
    paths: &Paths,
    args: &TranscriptArgs,
) -> Result<SessionRecord> {
    let targeted = args.target.selector.is_some()
        || args.target.workspace.is_some()
        || args.target.tag.is_some();
    if !targeted {
        if let Some(id) = discover_session_id() {
            return read_record(&paths.record(id)).with_context(|| {
                format!("session {id} (from APLEXER_SESSION_ID) has no persisted record")
            });
        }
    }
    resolve(paths, &args.target)
}

pub(crate) fn path_check(name: &str, path: &Path) -> Value {
    match fs::metadata(path) {
        Ok(meta) => json!({"name":name,"ok":meta.is_dir(),"detail":path.display().to_string()}),
        Err(e) => json!({"name":name,"ok":false,"detail":format!("{}: {e}",path.display())}),
    }
}
/// Attach/send/capture have no sensible action against a session with no
/// live worker other than saying so plainly -- left to `connect()`, a
/// terminal-phase session (worker gone, socket removed on its way out) or a
/// broken one (worker dead, socket simply not listening) both surface as a
/// bare `UnixStream::connect` OS error, e.g. "No such file or directory",
/// which reads like a bug rather than "this session is done". `a kill` is
/// deliberately exempt: for a terminal-phase session it now has a real
/// action to take (removing the state, see cmd_kill), and it already
/// handles the broken case itself via `recover_broken_containment`.
///
/// A third, rarer case: `phase` is non-terminal and `worker_pid` is alive,
/// but `socket_path` doesn't exist on disk. This happens when the worker's
/// runtime directory (which holds `control.sock`) got removed out from under
/// it. The worker process is technically still running, but it's
/// unreachable, so treating it as attachable would just trade the clear
/// checks above for the same bare `UnixStream::connect` OS error this
/// function exists to avoid. `a kill` again has a real action to take here
/// (see cmd_kill's socket-missing force-clean path), so it's not exempted
/// from this check the way the other two cases exempt it -- `a kill` relies
/// on `rpc_simple` failing and inspects the socket itself rather than going
/// through `check_attachable`.
///
/// That third case used to swallow a fourth that is not a fault at all: the
/// worker binds `control.sock` some milliseconds after it registers its pid,
/// so a client racing a healthy `a start` saw the same missing socket and
/// was told the runtime directory had been destroyed and to run `a kill` --
/// on a session that was about to come up. Both that and the missing-pid
/// window before it are now answered by `state == "starting"` (issue #9),
/// which is bounded by `DEFAULT_STARTUP_TIMEOUT_MS`: past the startup
/// budget the record really is a crashed start and the advice above applies
/// again.
pub(crate) fn check_attachable(record: &SessionRecord) -> Result<()> {
    if matches!(record.phase, Phase::Exited | Phase::Failed) {
        bail!(
            "session {} has already exited (see `a status {}` for details); run `a kill {}` to remove it",
            record.id,
            record.id,
            record.id
        );
    }
    let worker_alive = record.worker_alive();
    let state = derived_liveness(&record.phase, worker_alive, record.created_at_ms);
    let socket_missing = !record.socket_path.exists();
    // A session still inside its startup budget is coming up, not wreckage.
    // The worker writes the record, then its pid, then binds the socket, and
    // only then sets `phase: running` -- so `Starting` plus a missing pid or
    // a missing socket is exactly `a start` in flight. Both bails below used
    // to send the user at `a kill` for a perfectly healthy start that had
    // simply been raced (issue #9); the socket bail's own doc comment names
    // that race and then advised killing it anyway. Past the budget the
    // record is a crashed start and the original advice is right again.
    //
    // The liveness conjunct matters: `Starting` is still `Starting` after
    // the worker is up and listening, and that session is perfectly
    // attachable -- only a missing pid or a missing socket is a reason to
    // refuse at all.
    let still_starting = within_startup_window(&record.phase, record.created_at_ms, now_ms());
    if still_starting && (!worker_alive || socket_missing) {
        bail!(
            "session {} is still starting (its worker has not finished coming up); \
             retry in a moment, or run `a status {}` if it never does",
            record.id,
            record.id
        );
    }
    if !worker_alive {
        bail!(
            "session {}'s worker is not running (state: {}); run `a status` for details, `a kill` to reclaim it",
            record.id,
            state
        );
    }
    if socket_missing {
        bail!(
            "session {} looks alive (worker pid {} running) but its control socket is gone \
             ({}); this usually means the worker's runtime directory was removed out from \
             under it -- run `a kill {}` to force-clean the record, or investigate why that \
             directory disappeared",
            record.id,
            record.worker_pid.unwrap_or(0),
            record.socket_path.display(),
            record.id
        );
    }
    Ok(())
}
