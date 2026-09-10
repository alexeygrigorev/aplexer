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
    // (hidden subcommand name, how many leading args to drop before it,
    // flags injected between the hidden name and the remaining args --
    // a quick-attach index like "1" is itself the first real argument,
    // but `a -review` must smuggle its tag past the positional-only
    // QuickLaunchArgs surface as a flag).
    let rewrite = match args.get(1).map(String::as_str) {
        // `a -` / `a - claude` / `a - claude review` / `a - <command...>`,
        // the same "-" marks-current-directory idiom tmuxctl's `t` uses for
        // create-or-attach, adapted to aplexer's engine/tag model in
        // cmd_quick_launch.
        Some("-") => Some(("quick-launch", 2, Vec::new())),
        // `a -review` / `a -review claude` -- tmuxctl's dash-suffix idiom
        // (see is_dash_tag_arg), create-or-attach this workspace's
        // `<tag>` session, with the rest parsed exactly like `a -`'s.
        // Skip 2: the program name and the `-review` word itself, which
        // re-enters as the injected `--tag review`.
        Some(first) if is_dash_tag_arg(first) => Some((
            "quick-launch",
            2,
            vec!["--tag".to_string(), first[1..].to_string()],
        )),
        // `a <N>` / `a <N> <M>` / `a <N> <tag>` -- see rewrite doc below.
        Some(first) if !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit()) => {
            Some(("quick-attach", 1, Vec::new()))
        }
        _ => None,
    };
    let Some((hidden_name, skip, inject)) = rewrite else {
        return args;
    };
    let mut rewritten = Vec::with_capacity(args.len() + 1 + inject.len());
    rewritten.push(args[0].clone());
    rewritten.push(hidden_name.to_string());
    rewritten.extend(inject);
    rewritten.extend(args.into_iter().skip(skip));
    rewritten
}

/// Whether a first argument means tmuxctl's dash-suffix create-or-attach
/// (`a -review` -> the session tagged `review` in this workspace): one
/// dash, then a non-empty word. Long options stay flags, and so do the
/// two short flags clap auto-generates for the root command (`-h`, `-V`)
/// -- they predate this idiom and `a -h` must keep printing help.
fn is_dash_tag_arg(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-') && !arg.starts_with("--") && arg != "-h" && arg != "-V"
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
