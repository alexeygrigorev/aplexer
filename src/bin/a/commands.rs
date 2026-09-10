use super::*;

pub(crate) fn run() -> Result<()> {
    // `a` is a standalone process, so it can safely repair an inherited
    // auto-reaping SIGCHLD disposition before any subcommand spawns a child.
    // The embeddable Rust/Python API only validates and preserves its host.
    normalize_sigchld_for_child_management()?;
    let args = rewrite_quick_attach_args(std::env::args().collect());
    let cli = Cli::parse_from(args);
    let paths = Paths::discover()?;
    // Bare `a` with no subcommand defaults to `a list`, matching how tmux
    // and similar tools default to a listing rather than printing usage.
    let command = cli.command.unwrap_or(Commands::List(ListArgs::default()));
    match command {
        Commands::Start(args) => cmd_start(&paths, args, cli.json),
        Commands::New(mut args) => {
            args.attach = true;
            // `new` is the "always creates" verb: a live session holding the
            // tag is a reason to take the next free suffix, never an error.
            // `here`/`a -` stay create-or-attach.
            args.fresh = true;
            cmd_start(&paths, args, cli.json)
        }
        Commands::Here(args) => {
            if cli.json {
                bail!(
                    "`a here` is an interactive create-or-attach command; use \
                     `a --json start --workspace ... --tag ...` for automation"
                );
            }
            cmd_quick_launch(&paths, args)
        }
        Commands::List(args) => cmd_list(&paths, args, cli.json),
        Commands::Snapshot(args) => cmd_list(&paths, args, true),
        Commands::Attach(args) => {
            let record = resolve(&paths, &args.target)?;
            attach(
                &paths,
                &record,
                args.history_bytes,
                args.no_status,
                args.force,
            )
        }
        Commands::Send(args) => cmd_send(&paths, args, cli.json),
        Commands::Capture(args) => cmd_capture(&paths, args, cli.json),
        Commands::Status(target) => cmd_status(&paths, target, cli.json),
        Commands::Kill(args) => cmd_kill(&paths, args, cli.json),
        Commands::Forget(args) => cmd_forget(&paths, args, cli.json),
        Commands::Prune => cmd_prune(&paths, cli.json),
        Commands::Rename(args) => cmd_rename(&paths, args, cli.json),
        Commands::Engines => cmd_engines(&paths, cli.json),
        Commands::Profiles => cmd_profiles(&paths, cli.json),
        Commands::LaunchSpec(args) => cmd_launch_spec(&paths, args, cli.json),
        Commands::LaunchExec(args) => cmd_launch_exec(&paths, args),
        Commands::Whoami => cmd_whoami(&paths, cli.json),
        Commands::StateReport(args) => cmd_state_report(&paths, args.state),
        Commands::Doctor => cmd_doctor(&paths, cli.json),
        Commands::Init(args) => cmd_init(&paths, args, cli.json),
        Commands::Message(args) => cmd_message(&paths, args, cli.json),
        Commands::Watch(args) => cmd_watch(&paths, args),
        Commands::Transcript(args) => cmd_transcript(&paths, args, cli.json),
        Commands::Completions(args) => cmd_completions(args),
        Commands::Hotkeys => cmd_hotkeys(),
        Commands::QuickAttach(args) => cmd_quick_attach(&paths, args),
        Commands::QuickLaunch(args) => cmd_quick_launch(&paths, args),
    }
}

/// `a <N> [session]` is rewritten to `a quick-attach <N> [session]` before
/// clap ever sees it, the same trick tmuxctl's `t` uses in its own
/// argv-rewriting main() (see ~/git/tmuxctl/tmuxctl/cli.py) to let a bare
/// positional number mean "attach" without a subcommand keyword. Only the
/// first argument is inspected, and only when it's non-empty and all
/// digits -- none of `a`'s real subcommand names collide with that.
pub(crate) fn rewrite_quick_attach_args(args: Vec<String>) -> Vec<String> {
    // (hidden subcommand name, how many leading args to drop before it --
    // the "-" marker itself carries no information once rewritten, but a
    // quick-attach index like "1" is itself the first real argument).
    let rewrite = match args.get(1).map(String::as_str) {
        // `a -` / `a - claude` / `a - claude review` / `a - <command...>`,
        // the same "-" marks-current-directory idiom tmuxctl's `t` uses for
        // create-or-attach, adapted to aplexer's engine/tag model in
        // cmd_quick_launch.
        Some("-") => Some(("quick-launch", 2)),
        // `a <N>` / `a <N> <M>` / `a <N> <tag>` -- see rewrite doc below.
        Some(first) if !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit()) => {
            Some(("quick-attach", 1))
        }
        _ => None,
    };
    let Some((hidden_name, skip)) = rewrite else {
        return args;
    };
    let mut rewritten = Vec::with_capacity(args.len() + 1);
    rewritten.push(args[0].clone());
    rewritten.push(hidden_name.to_string());
    rewritten.extend(args.into_iter().skip(skip));
    rewritten
}

/// The tag the terminal-first vocabulary creates and resolves by default:
/// `a here`, `a -`, and `a start` all mean tag `main` in the current
/// workspace, so "work on the main thing here" is one word in every form.
pub(crate) const DEFAULT_HUMAN_TAG: &str = "main";
/// Sessions created before the terminal-first default stay attachable with
/// no flags: after `main`, a bare `a attach`/`a status` falls back to this
/// pre-UX tag before giving up.
pub(crate) const LEGACY_DEFAULT_TAG: &str = "default";

/// Whether a selector could plausibly be a UUID or UUID prefix: only hex
/// digits and dashes, with 8..=32 digits (a full UUID is 32, the shortest
/// useful prefix `resolve_record` honors is 8). Anything containing a
/// non-hex character is a word -- i.e. a candidate tag -- never a UUID.
pub(crate) fn looks_like_uuid_selector(raw: &str) -> bool {
    let mut hex_digits = 0usize;
    for byte in raw.bytes() {
        if byte == b'-' {
            continue;
        }
        if !byte.is_ascii_hexdigit() {
            return false;
        }
        hex_digits += 1;
    }
    (8..=32).contains(&hex_digits)
}

pub(crate) fn resolve(paths: &Paths, target: &TargetArgs) -> Result<SessionRecord> {
    // `a attach 1`, `a status 1`, `a kill 1`, etc. should mean the same
    // thing as the bare `a 1` shortcut, not just work for `attach`. Only
    // kick in for selectors shorter than 8 characters, the minimum length
    // resolve_record treats as a UUID prefix -- so this can never shadow a
    // real UUID/UUID-prefix selector, and workspace counts realistically
    // never reach 8 digits.
    if target.workspace.is_none() && target.tag.is_none() {
        if let Some(selector) = &target.selector {
            let is_quick_index = !selector.is_empty()
                && selector.len() < 8
                && selector.bytes().all(|b| b.is_ascii_digit());
            if is_quick_index {
                let index: usize = selector.parse().unwrap_or(0);
                return resolve_quick_index(paths, index, None);
            }
        }
    }
    // Terminal-first default target: no selector and no --tag means "the
    // main session here" -- the same session `a here` creates. Prefers
    // `main`, then falls back to the pre-UX `default` tag so existing
    // sessions remain one-command attachable after upgrading. An explicit
    // --workspace pins the lookup; with no flag the workspace is
    // $APLEXER_WORKSPACE (set inside every session) or the cwd, matching
    // `resolve_message_workspace` so all "here" resolution agrees.
    if target.selector.is_none() && target.tag.is_none() {
        let workspace = match &target.workspace {
            Some(ws) => Some(canonical_workspace(ws)?),
            None => resolve_message_workspace(None).ok(),
        };
        if let Some(workspace) = workspace {
            let records = list_records(paths)?;
            if let Some(record) = records
                .iter()
                .find(|r| r.workspace == workspace && r.tag == DEFAULT_HUMAN_TAG)
                .or_else(|| {
                    records
                        .iter()
                        .find(|r| r.workspace == workspace && r.tag == LEGACY_DEFAULT_TAG)
                })
            {
                return Ok(record.clone());
            }
            if target.workspace.is_none() {
                bail!(
                    "no {DEFAULT_HUMAN_TAG} session in {}; run `a here` to create one, or `a` to list every session",
                    display_workspace(&workspace, env::var_os("HOME").as_deref().map(Path::new))
                );
            }
            // Explicit --workspace with no main/default session: fall
            // through to resolve_record's legacy `default`-tag resolution
            // and its own error message.
        }
    }
    // `a open review`, `a show review`: a plain word that is not a candidate
    // UUID resolves as a tag in the current workspace, after UUID and
    // full `workspace:tag` selectors have had their chance -- so existing
    // machine-facing selector semantics can never be shadowed by a tag.
    if let Some(selector) = &target.selector {
        if target.workspace.is_none() && target.tag.is_none() && !looks_like_uuid_selector(selector)
        {
            if let Ok(record) = resolve_record(paths, Some(selector), None, None) {
                return Ok(record);
            }
            if let Ok(workspace) = resolve_message_workspace(None) {
                let matches: Vec<SessionRecord> = list_records(paths)?
                    .into_iter()
                    .filter(|r| r.workspace == workspace && r.tag == selector.as_str())
                    .collect();
                match matches.len() {
                    1 => return Ok(matches.into_iter().next().expect("one match")),
                    0 => bail!(
                        "no session tagged '{selector}' in {}; run `a` to list sessions, \
                         or use a full workspace:tag selector",
                        display_workspace(
                            &workspace,
                            env::var_os("HOME").as_deref().map(Path::new)
                        )
                    ),
                    // A pair can transiently be held by a corpse next to the
                    // live session that took its name (`a rename`, issue
                    // #13); the tag still means the live session until prune
                    // clears the corpse. Same rule as `resolve_record`.
                    _ => {
                        let live: Vec<SessionRecord> = matches
                            .iter()
                            .filter(|r| aplexer::reap_verdict(r).is_none())
                            .cloned()
                            .collect();
                        if live.len() == 1 {
                            return Ok(live.into_iter().next().expect("exactly one live match"));
                        }
                        bail!("tag '{selector}' is ambiguous in this workspace")
                    }
                }
            }
        }
    }
    resolve_record(
        paths,
        target.selector.as_deref(),
        target.workspace.as_deref(),
        target.tag.as_deref(),
    )
}

pub(crate) fn cmd_start(paths: &Paths, args: StartArgs, json_output: bool) -> Result<()> {
    if json_output && args.attach {
        bail!(
            "--json cannot be combined with `start --attach`: JSON session metadata and terminal bytes cannot share stdout; run `a --json start ...` and `a attach SESSION` separately"
        );
    }
    let env = parse_env(&args.env)?;
    let command = args
        .command
        .iter()
        .map(|v| os_to_utf8(v, "command argument"))
        .collect::<Result<Vec<_>>>()?;
    let mut worker_rows = None;
    let mut worker_cols = None;
    if args.attach {
        let tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        if let Some((rows, cols)) = tty.then(|| terminal_size(libc::STDIN_FILENO)).flatten() {
            worker_rows = Some(reserved_rows(rows));
            worker_cols = Some(cols);
        }
    }
    let req = aplexer::api::StartRequest {
        workspace: args.workspace,
        tag: args.tag,
        engine: args.engine,
        profile: args.profile,
        cwd: args.cwd,
        env,
        command,
        memory: args.memory,
        pids: args.pids,
        cpu_quota_us: args.cpu_quota_us,
        cpu_period_us: args.cpu_period_us,
        history_bytes: args.history_bytes,
        no_skip_permissions: args.no_skip_permissions,
        startup_timeout_ms: args.startup_timeout_ms,
        worker_rows,
        worker_cols,
        python: None,
        fresh: args.fresh,
    };
    let ready = aplexer::api::start_session(paths, &req)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&ready)?);
    } else {
        println!("{}", ready.id);
        println!("{}", ready.selector());
    }
    if args.attach {
        // `None` here means "use attach()'s small default replay", not
        // "replay the whole configured history buffer" -- ready.history_bytes
        // is the session's *storage capacity* (up to DEFAULT_HISTORY_BYTES =
        // 4MB), an unrelated setting from how much of it a fresh attach
        // should actually replay onto the screen.
        attach(paths, &ready, None, false, false)?;
    }
    Ok(())
}

pub(crate) fn cmd_list(paths: &Paths, args: ListArgs, json_output: bool) -> Result<()> {
    // `--sort` remembers even on the JSON path, so a later human `a list` /
    // `a N` uses the same workspace order. JSON row order itself stays
    // newest-created-first (spec.md §18).
    if args.sort.is_some() {
        resolve_list_sort(paths, args.sort)?;
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&aplexer::api::snapshot_json(paths, args.running)?)?
        );
        return Ok(());
    }
    // Human output is terminal-first: on a real TTY the richer tree below
    // adds semantic state and current-workspace orientation; redirected
    // output keeps the pre-UX plain rendering byte-for-byte, so scripts
    // piping `a list` never see presentation changes.
    if io::stdout().is_terminal() {
        return cmd_list_tty(paths, args);
    }
    cmd_list_plain(paths, args)
}

/// Exited sessions are corpses: the one listed state that is neither active
/// (`ui_state_is_active`) nor needs-attention (`ui_state_needs_attention`),
/// and `check_attachable` refuses them outright. The terminal list hides
/// them by default -- a workspace whose every session has exited drops out
/// with them -- and `a list --all` brings them back. Before rendering, the
/// default list first sweeps what `a prune` would take outright (see
/// `sweep_prunable_corpses`), so the hide only ever covers the shapes prune
/// itself retains. `resolve_quick_index`
/// shares this so the numbers `a <workspace#>` understands stay the numbers
/// the default list prints; a corpse you found via `a list --all` is
/// addressed by tag or UUID prefix, not by its --all index.
pub(crate) fn session_is_listed(record: &SessionRecord, now: u64) -> bool {
    session_ui_state(record, now).0 != "exited"
}

pub(crate) const ANSI_RESET: &str = "\x1b[0m";
pub(crate) const ANSI_BOLD: &str = "\x1b[1m";
pub(crate) const ANSI_DIM: &str = "\x1b[2m";
pub(crate) const ANSI_CYAN: &str = "\x1b[36m";
pub(crate) const ANSI_GREEN: &str = "\x1b[32m";
pub(crate) const ANSI_YELLOW: &str = "\x1b[33m";
pub(crate) const ANSI_RED: &str = "\x1b[31m";
pub(crate) const ANSI_GRAY: &str = "\x1b[90m";

/// Colors only when stdout is a real terminal and the user hasn't opted out
/// via `NO_COLOR` (https://no-color.org) -- `a list | grep foo` or similar
/// piping must never see escape codes.
pub(crate) fn color_enabled() -> bool {
    io::stdout().is_terminal() && env::var_os("NO_COLOR").is_none()
}

/// Wraps already-padded plain text in `code`/reset -- callers must pad
/// widths (`{:<14}` etc.) on the plain string BEFORE calling this, since
/// padding a string that already contains escape codes counts the invisible
/// bytes toward the width and breaks column alignment.
pub(crate) fn paint(enabled: bool, code: &str, text: &str) -> String {
    if enabled {
        format!("{code}{text}{ANSI_RESET}")
    } else {
        text.to_string()
    }
}

pub(crate) fn state_glyph(state: &str) -> (&'static str, &'static str) {
    match state {
        "running" | "working" | "active" => ("\u{25CF}", ANSI_GREEN),
        "waiting" => ("!", ANSI_YELLOW),
        "starting" | "exiting" | "stopping" => ("\u{25D0}", ANSI_YELLOW),
        "failed" | "broken" | "oom" => ("\u{2717}", ANSI_RED),
        _ => ("\u{25CB}", ANSI_GRAY), // "exited", "idle", "quiet"
    }
}

/// The status bar's animated state glyph: a braille spinner frame while the
/// attached session's state is `working` -- a fresh `a state-report` push,
/// i.e. the agent *said* it is running -- and `None` for every other state,
/// meaning "keep `state_glyph`'s static glyph". Deliberately not `active`:
/// that state is a PTY-recency guess (it fires while the user merely types
/// at an agent TUI's prompt, and for any record with no activity sample at
/// all), so spinning on it would promise work that is not happening. The
/// reported state is the only signal that means "the agent is working" and
/// not just "the terminal is warm"; without hooks installed the bar simply
/// stays on its static glyph. So the bar only ever moves while there is
/// work to point at: an idle, waiting, or dead session renders byte-stable
/// text, and the dirty check in `draw_status_bar` keeps it write-free.
///
/// The frame index is a pure function of the wall clock, deliberately not
/// thread-local counter state: the status thread, the frame loop's pending
/// flush, and the input thread's flash redraw all render the bar
/// independently, and this way any two calls within the same
/// `SPINNER_FRAME_MS` window agree on the frame without sharing anything.
/// (Sanitized-then-padded like all bar text, and the same width-1 as the
/// `●` it replaces, so truncation math is unchanged.)
pub(crate) fn spinner_frame(state: &str, now_ms: u64) -> Option<char> {
    if state != "working" {
        return None;
    }
    let idx = (now_ms / SPINNER_FRAME_MS) as usize % SPINNER_FRAMES.len();
    Some(SPINNER_FRAMES[idx])
}

/// The single human-facing state derivation: lifecycle facts first (a dead
/// worker behind a non-terminal phase is `broken` regardless of anything the
/// record claims), then the authoritative agent-state derivation shared with
/// `a watch` (`aplexer::watch::derive_agent_state_with_source` -- one set of
/// freshness thresholds, never a copy), mapped onto the honest human
/// vocabulary from docs/cli-ux.md section 4:
///
/// - a fresh `a state-report` push is semantic fact: `working`/`waiting`/`idle`
///   -- including for `shell`-engine sessions, where the agent was started
///   by hand inside the shell and the push is the only semantic signal
/// - PTY-recency inference never claims semantics: recent output is
///   `active`, silence is `quiet` -- deliberately NOT `waiting`, because
///   "the terminal went quiet" cannot tell a blocked agent from a long
///   compute step
/// - a plain shell that never reported any agent state is just `running`
///   no matter how quiet its PTY is; a shell an agent has lived in (any
///   state-report push in its history) gets the same activity words as a
///   first-class engine once nothing is fresh
///
/// Returns `(state, source)` where source is `reported`, `activity`, or
/// `lifecycle`, so callers can qualify inferred states instead of faking
/// certainty.
/// `observed_state` against the wall clock, for the query-time commands that
/// have no injected clock of their own. Every derived `state` a CLI command
/// prints goes through here or through `observed_state` directly, so none of
/// them can disagree about the startup window (issue #9).
pub(crate) fn derived_liveness(
    phase: &Phase,
    worker_alive: bool,
    created_at_ms: u64,
) -> &'static str {
    observed_state(phase, worker_alive, created_at_ms, now_ms())
}

pub(crate) fn session_ui_state(record: &SessionRecord, now: u64) -> (&'static str, &'static str) {
    // Deferred to `observed_state` rather than repeating its predicate, so
    // the TTY UI cannot go on painting a mid-create session `broken` after
    // the derived state stopped saying so.
    if observed_state(
        &record.phase,
        record.worker_alive(),
        record.created_at_ms,
        now,
    ) == "broken"
    {
        return ("broken", "lifecycle");
    }
    match record.phase {
        Phase::Starting => ("starting", "lifecycle"),
        Phase::Exiting => ("stopping", "lifecycle"),
        Phase::Exited => {
            let oom = record
                .exit
                .as_ref()
                .map(|exit| exit.oom_killed)
                .unwrap_or(false);
            (if oom { "oom" } else { "exited" }, "lifecycle")
        }
        Phase::Failed => ("failed", "lifecycle"),
        Phase::Running => {
            // A fresh state-report push is what the agent says it is --
            // checked before the shell early return below, because a hook
            // firing inside a shell session (the normal case: the agent was
            // launched by hand, `APLEXER_SESSION_ID` is still injected) is
            // fact, not a guess. Without it, an idle opencode/grok inside a
            // shell session would show `running` forever.
            let (state, source) = aplexer::watch::derive_agent_state_with_source(record, now);
            if source == "reported" {
                return match state {
                    "running" => ("working", "reported"),
                    "waiting" => ("waiting", "reported"),
                    "idle" => ("idle", "reported"),
                    // Defensive only: fresh_reported_state only ever
                    // produces the three values above.
                    _ => (state, source),
                };
            }
            // A shell an agent has lived in (any state-report push in the
            // record's history) is not a "plain shell": when nothing is
            // fresh, its quiet is an agent sitting at a prompt or thinking,
            // not a shell doing work, so it gets the same honest activity
            // words as a first-class engine. Only a shell that never
            // reported anything keeps the lifecycle `running` -- for a bare
            // prompt (or `tail -f`) that really is all that is known.
            if record.engine == "shell" && record.reported_state.is_none() {
                return ("running", "lifecycle");
            }
            match (state, source) {
                // The heuristic's "running/waiting" words imply agent
                // semantics the PTY cannot actually know; translate to
                // activity words that don't.
                ("running", _) => ("active", "activity"),
                ("waiting", _) => ("quiet", "activity"),
                (state, source) => (state, source),
            }
        }
    }
}

/// Whether a state word counts as "alive/working" in workspace summaries --
/// everything a live worker can be in, including the merely-quiet.
pub(crate) fn ui_state_is_active(state: &str) -> bool {
    matches!(
        state,
        "working" | "waiting" | "idle" | "active" | "quiet" | "running" | "starting" | "stopping"
    )
}

/// Whether a state word means "the human should look at this": reported
/// waits and every failure/health condition. Inferred quiet is deliberately
/// not attention -- it is usually just a long-running command.
pub(crate) fn ui_state_needs_attention(state: &str) -> bool {
    matches!(state, "waiting" | "broken" | "failed" | "oom")
}

/// How long a state word's evidence is stale-able, for the list's age
/// column: the state-report push for semantic states, last PTY output for
/// activity states, the exit for terminal ones. Falls back to the record's
/// own update time so the column always has something honest to show.
pub(crate) fn state_timestamp(record: &SessionRecord, state: &str, now: u64) -> u64 {
    let candidate = match state {
        "working" | "waiting" | "idle" => record.reported_state_at_ms,
        "active" | "quiet" => record.last_activity_ms,
        "exited" | "oom" => record.exit.as_ref().map(|exit| exit.exited_at_ms),
        _ => None,
    };
    candidate
        .filter(|at| *at <= now)
        .unwrap_or(record.updated_at_ms)
}

/// Compact age: `now`, `30s`, `5m`, `5h 1m`, `5d 5h`. Two units once the
/// span is at least an hour, so a week-old session is not just `5d`.
pub(crate) fn compact_elapsed(ms: u64) -> String {
    let seconds = ms / 1_000;
    if seconds < 5 {
        return "now".to_string();
    }
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let secs = seconds % 60;
    if days > 0 {
        if hours > 0 {
            format!("{days}d {hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 {
        if minutes > 0 {
            format!("{hours}h {minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{secs}s")
    }
}

/// The phrase form for prose contexts (`a status`): "just now", "4m ago".
pub(crate) fn human_age_phrase(ms: u64) -> String {
    match compact_elapsed(ms).as_str() {
        "now" => "just now".to_string(),
        age => format!("{age} ago"),
    }
}

/// Qualifier appended to a semantic state so a human can tell what kind of
/// fact they are looking at; empty for authoritative sources.
pub(crate) fn state_source_suffix(source: &str) -> &'static str {
    match source {
        "activity" => " (inferred from output activity)",
        _ => "",
    }
}

/// Pads or truncates to exactly `width` display cells, marking a truncation
/// with `…` (counted against the width), Unicode-width safe: CJK-width
/// glyphs and combining sequences never split mid-cluster or misalign the
/// columns built from `fit_column` calls.
pub(crate) fn fit_column(text: &str, width: usize) -> String {
    let display_width = terminal_display_width(text);
    if display_width <= width {
        return format!("{text}{}", " ".repeat(width - display_width));
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut result = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let grapheme_width = terminal_display_width(grapheme);
        if used + grapheme_width > width - 1 {
            break;
        }
        result.push_str(grapheme);
        used += grapheme_width;
    }
    result.push('…');
    result.push_str(&" ".repeat(width.saturating_sub(used + 1)));
    result
}

pub(crate) fn workspace_glyph(running: usize, total: usize) -> (&'static str, &'static str) {
    if running == total {
        ("\u{25CF}", ANSI_GREEN)
    } else if running == 0 {
        ("\u{25CB}", ANSI_GRAY)
    } else {
        ("\u{25D0}", ANSI_YELLOW)
    }
}

/// Shortens a workspace path under $HOME to `~/...`, matching spec.md's own
/// display examples (e.g. section 2's `~/git/pocketshell` tree).
pub(crate) fn display_workspace(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if let Ok(rest) = path.strip_prefix(home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    path.display().to_string()
}

pub(crate) fn running_count(
    group: &[SessionRecord],
    alive: &BTreeMap<Uuid, bool>,
) -> (usize, usize) {
    let running = group
        .iter()
        .filter(|r| {
            derived_liveness(
                &r.phase,
                alive.get(&r.id).copied().unwrap_or(false),
                r.created_at_ms,
            ) == "running"
        })
        .count();
    (running, group.len())
}

pub(crate) fn running_summary(group: &[SessionRecord], alive: &BTreeMap<Uuid, bool>) -> String {
    let (running, total) = running_count(group, alive);
    if running == total {
        format!("running {running}")
    } else if running == 0 {
        format!("stopped {total}")
    } else {
        format!("running {running}/{total}")
    }
}

pub(crate) fn group_by_workspace(
    records: Vec<SessionRecord>,
    sort: ListSort,
) -> Vec<(PathBuf, Vec<SessionRecord>)> {
    let mut groups: Vec<(PathBuf, Vec<SessionRecord>)> = Vec::new();
    for r in records {
        match groups.iter_mut().find(|(ws, _)| *ws == r.workspace) {
            Some((_, group)) => group.push(r),
            None => groups.push((r.workspace.clone(), vec![r])),
        }
    }
    groups.sort_by(|a, b| compare_workspaces(a, b, sort));
    groups
}

pub(crate) fn compare_workspaces(
    left: &(PathBuf, Vec<SessionRecord>),
    right: &(PathBuf, Vec<SessionRecord>),
    sort: ListSort,
) -> std::cmp::Ordering {
    let time_order =
        |left_ms: u64, right_ms: u64| right_ms.cmp(&left_ms).then_with(|| left.0.cmp(&right.0));
    match sort {
        ListSort::Name => left.0.cmp(&right.0),
        ListSort::Created => time_order(
            workspace_created_ms(&left.1),
            workspace_created_ms(&right.1),
        ),
        ListSort::Accessed => time_order(
            workspace_accessed_ms(&left.1),
            workspace_accessed_ms(&right.1),
        ),
        ListSort::Activity => time_order(
            workspace_activity_ms(&left.1),
            workspace_activity_ms(&right.1),
        ),
    }
}

pub(crate) fn workspace_created_ms(sessions: &[SessionRecord]) -> u64 {
    sessions.iter().map(|s| s.created_at_ms).max().unwrap_or(0)
}

/// Recency of human access: last attach, falling back to created so
/// never-attached records (including those from before `last_accessed_ms`
/// existed) still have a stable place in the order.
pub(crate) fn workspace_accessed_ms(sessions: &[SessionRecord]) -> u64 {
    sessions
        .iter()
        .map(|s| s.last_accessed_ms.unwrap_or(s.created_at_ms))
        .max()
        .unwrap_or(0)
}

pub(crate) fn last_agent_activity_ms(record: &SessionRecord) -> Option<u64> {
    match (record.last_activity_ms, record.reported_state_at_ms) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

pub(crate) fn workspace_activity_ms(sessions: &[SessionRecord]) -> u64 {
    sessions
        .iter()
        .filter_map(last_agent_activity_ms)
        .max()
        .unwrap_or(0)
}

pub(crate) fn workspace_recency_label(
    sort: ListSort,
    sessions: &[SessionRecord],
    now: u64,
) -> String {
    let ago = |at: u64| human_age_phrase(now.saturating_sub(at));
    match sort {
        ListSort::Name => String::new(),
        ListSort::Created => format!("created {}", ago(workspace_created_ms(sessions))),
        ListSort::Accessed => match sessions.iter().filter_map(|s| s.last_accessed_ms).max() {
            Some(at) => format!("opened {}", ago(at)),
            None => "never opened".to_string(),
        },
        ListSort::Activity => match workspace_activity_ms(sessions) {
            0 => "no activity".to_string(),
            at => format!("active {}", ago(at)),
        },
    }
}

pub(crate) fn list_sort_path(paths: &Paths) -> PathBuf {
    paths.state_root.join("list-sort")
}

pub(crate) fn load_list_sort(paths: &Paths) -> ListSort {
    fs::read_to_string(list_sort_path(paths))
        .ok()
        .and_then(|text| ListSort::parse(text.trim()))
        .unwrap_or(ListSort::Name)
}

pub(crate) fn save_list_sort(paths: &Paths, sort: ListSort) -> Result<()> {
    fs::write(list_sort_path(paths), format!("{}\n", sort.as_str()))
        .with_context(|| format!("write {}", list_sort_path(paths).display()))
}

/// `--sort KEY` both applies and remembers; a bare `a list` (and `a N`)
/// reuse the last choice so the numbers on the tree stay stable.
pub(crate) fn resolve_list_sort(paths: &Paths, requested: Option<ListSort>) -> Result<ListSort> {
    if let Some(sort) = requested {
        save_list_sort(paths, sort)?;
        return Ok(sort);
    }
    Ok(load_list_sort(paths))
}
