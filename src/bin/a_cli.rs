mod legacy {
    #![allow(dead_code)]
    #![allow(clippy::items_after_test_module)]

    include!("a.rs");

    const REPORTED_STATE_STALE_MS_UX: u64 = 8_000;
    const ACTIVITY_THRESHOLD_MS_UX: u64 = 3_000;
    const UX_HOTKEYS: &str =
        "Ctrl-b: ? help · d detach · n/p next/previous · N/P global · 1-9 jump · l last";

    pub fn entry() {
        if let Err(error) = run_ux() {
            eprintln!("a: {error:#}");
            std::process::exit(1);
        }
    }

    fn run_ux() -> Result<()> {
        normalize_sigchld_for_child_management()?;
        let raw_args: Vec<String> = std::env::args().collect();
        if top_level_help_requested(&raw_args) {
            return print_ux_help();
        }
        let args = rewrite_quick_attach_args(rewrite_ux_aliases(raw_args));
        let cli = Cli::parse_from(args);
        let paths = Paths::discover()?;
        let json_output = cli.json;
        let command = cli
            .command
            .unwrap_or(Commands::List(ListArgs { running: false }));

        match command {
            Commands::Start(args) if args.attach => {
                cmd_start_attaching_ux(&paths, args, json_output)
            }
            Commands::Start(args) => cmd_start(&paths, args, json_output),
            Commands::List(args) => cmd_list_ux(&paths, args, json_output),
            Commands::Snapshot(args) => cmd_list(&paths, args, true),
            Commands::Attach(args) => {
                let record = resolve(&paths, &args.target)?;
                attach_ux(&paths, &record, args.history_bytes)
            }
            Commands::Send(args) => cmd_send(&paths, args, json_output),
            Commands::Capture(args) => cmd_capture(&paths, args, json_output),
            Commands::Status(target) if !json_output && io::stdout().is_terminal() => {
                cmd_status_ux(&paths, target)
            }
            Commands::Status(target) => cmd_status(&paths, target, json_output),
            Commands::Kill(args) => cmd_kill(&paths, args, json_output),
            Commands::Forget(args) => cmd_forget(&paths, args, json_output),
            Commands::Prune => cmd_prune(&paths, json_output),
            Commands::Rename(args) => cmd_rename(&paths, args, json_output),
            Commands::Engines => cmd_engines(&paths, json_output),
            Commands::Profiles => cmd_profiles(&paths, json_output),
            Commands::LaunchSpec(args) => cmd_launch_spec(&paths, args, json_output),
            Commands::LaunchExec(args) => cmd_launch_exec(&paths, args),
            Commands::Whoami => cmd_whoami(&paths, json_output),
            Commands::StateReport(args) => cmd_state_report(&paths, args.state),
            Commands::Doctor => cmd_doctor(&paths, json_output),
            Commands::Message(args) => cmd_message(&paths, args, json_output),
            Commands::Watch(args) => cmd_watch(&paths, args),
            Commands::Transcript(args) => cmd_transcript(&paths, args, json_output),
            Commands::Completions(args) => cmd_completions(args),
            Commands::Hotkeys => cmd_hotkeys_ux(),
            Commands::QuickAttach(args) => cmd_quick_attach_ux(&paths, args),
            Commands::QuickLaunch(args) => cmd_quick_launch_ux(&paths, args),
        }
    }

    fn top_level_help_requested(args: &[String]) -> bool {
        matches!(args, [_, flag] if flag == "--help" || flag == "-h" || flag == "help")
    }

    fn print_ux_help() -> Result<()> {
        let mut command = Cli::command();
        command.print_long_help()?;
        println!();
        println!();
        println!("QUICK WORKFLOWS:");
        println!("  a                         Sessions at a glance");
        println!("  a here                    Create-or-attach here with the default engine");
        println!("  a here codex review       Create-or-attach codex here, tagged review");
        println!("  a new --engine shell      Start and attach using full start options");
        println!("  a 2 1                     Attach workspace 2, session 1");
        println!("  a open SESSION            Alias for `a attach SESSION`");
        println!("  a status SESSION          Human summary; add --json for the stable API");
        println!();
        println!("Inside a session, press Ctrl-b ? for the live key reference.");
        Ok(())
    }

    fn rewrite_ux_aliases(mut args: Vec<String>) -> Vec<String> {
        let command_index = args
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, value)| value.as_str() != "--json")
            .map(|(index, _)| index);
        let Some(index) = command_index else {
            return args;
        };

        match args[index].as_str() {
            "help" if args.len() > index + 1 => {
                args.remove(index);
                args.push("--help".to_string());
            }
            "here" => args[index] = "-".to_string(),
            "open" => args[index] = "attach".to_string(),
            "ps" => args[index] = "list".to_string(),
            "current" => args[index] = "whoami".to_string(),
            "keys" => args[index] = "hotkeys".to_string(),
            "check" => args[index] = "doctor".to_string(),
            "new" => {
                args[index] = "start".to_string();
                if !args.iter().any(|arg| arg == "--attach") {
                    args.insert(index + 1, "--attach".to_string());
                }
            }
            _ => {}
        }
        args
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct UxState {
        label: &'static str,
        glyph: &'static str,
        color: &'static str,
        attention: bool,
        active: bool,
    }

    fn ux_state(record: &SessionRecord, raw: Option<&Value>, now: u64) -> UxState {
        let worker_alive = record.worker_alive();
        if matches!(
            record.phase,
            Phase::Starting | Phase::Running | Phase::Exiting
        ) && !worker_alive
        {
            return UxState {
                label: "broken",
                glyph: "✗",
                color: ANSI_RED,
                attention: true,
                active: false,
            };
        }

        match record.phase {
            Phase::Starting => UxState {
                label: "starting",
                glyph: "◐",
                color: ANSI_YELLOW,
                attention: false,
                active: true,
            },
            Phase::Exiting => UxState {
                label: "stopping",
                glyph: "◐",
                color: ANSI_YELLOW,
                attention: false,
                active: true,
            },
            Phase::Exited => {
                let oom = record
                    .exit
                    .as_ref()
                    .map(|exit| exit.oom_killed)
                    .unwrap_or(false);
                UxState {
                    label: if oom { "oom" } else { "exited" },
                    glyph: if oom { "✗" } else { "○" },
                    color: if oom { ANSI_RED } else { ANSI_GRAY },
                    attention: oom,
                    active: false,
                }
            }
            Phase::Failed => UxState {
                label: "failed",
                glyph: "✗",
                color: ANSI_RED,
                attention: true,
                active: false,
            },
            Phase::Running => {
                if record.engine == "shell" {
                    return UxState {
                        label: "running",
                        glyph: "●",
                        color: ANSI_GREEN,
                        attention: false,
                        active: true,
                    };
                }

                let reported_state = raw
                    .and_then(|value| value.get("reported_state"))
                    .and_then(Value::as_str)
                    .or(record.reported_state.as_deref());
                let reported_at = raw
                    .and_then(|value| value.get("reported_state_at_ms"))
                    .and_then(Value::as_u64)
                    .or(record.reported_state_at_ms);
                if let (Some(state), Some(at)) = (reported_state, reported_at) {
                    if now.saturating_sub(at) <= REPORTED_STATE_STALE_MS_UX {
                        return match state {
                            "waiting" => UxState {
                                label: "waiting",
                                glyph: "!",
                                color: ANSI_YELLOW,
                                attention: true,
                                active: true,
                            },
                            "idle" => UxState {
                                label: "idle",
                                glyph: "○",
                                color: ANSI_GRAY,
                                attention: false,
                                active: true,
                            },
                            "working" => UxState {
                                label: "working",
                                glyph: "●",
                                color: ANSI_GREEN,
                                attention: false,
                                active: true,
                            },
                            _ => heuristic_agent_state(record, raw, now),
                        };
                    }
                }
                heuristic_agent_state(record, raw, now)
            }
        }
    }

    fn heuristic_agent_state(record: &SessionRecord, raw: Option<&Value>, now: u64) -> UxState {
        let last_activity = raw
            .and_then(|value| value.get("last_activity_ms"))
            .and_then(Value::as_u64)
            .or(record.last_activity_ms);
        let active = last_activity
            .map(|at| now.saturating_sub(at) < ACTIVITY_THRESHOLD_MS_UX)
            .unwrap_or(true);
        if active {
            UxState {
                label: "active",
                glyph: "●",
                color: ANSI_GREEN,
                attention: false,
                active: true,
            }
        } else {
            // Quiet is deliberately not called waiting. Without a fresh
            // state-report, terminal silence is only an activity heuristic.
            UxState {
                label: "quiet",
                glyph: "○",
                color: ANSI_GRAY,
                attention: false,
                active: true,
            }
        }
    }

    fn human_age(now: u64, timestamp: Option<u64>) -> String {
        let Some(timestamp) = timestamp else {
            return "—".to_string();
        };
        let seconds = now.saturating_sub(timestamp) / 1_000;
        if seconds < 3 {
            "now".to_string()
        } else if seconds < 60 {
            format!("{seconds}s")
        } else if seconds < 3_600 {
            format!("{}m", seconds / 60)
        } else if seconds < 86_400 {
            format!("{}h", seconds / 3_600)
        } else {
            format!("{}d", seconds / 86_400)
        }
    }

    fn human_age_phrase(now: u64, timestamp: Option<u64>) -> String {
        match human_age(now, timestamp).as_str() {
            "now" => "just now".to_string(),
            "—" => "unknown".to_string(),
            age => format!("{age} ago"),
        }
    }

    fn fit_column(text: &str, width: usize) -> String {
        if terminal_display_width(text) <= width {
            return format!("{text}{}", " ".repeat(width - terminal_display_width(text)));
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

    fn cmd_list_ux(paths: &Paths, args: ListArgs, json_output: bool) -> Result<()> {
        if json_output || !io::stdout().is_terminal() {
            return cmd_list(paths, args, json_output);
        }

        let mut records = list_records(paths)?;
        if args.running {
            records.retain(|record| record.worker_phase_active() && record.worker_alive());
        }
        if records.is_empty() {
            if args.running {
                println!("No running sessions.");
            } else {
                println!("No aplexer sessions yet.");
                println!();
                println!("Start and attach in this directory:");
                println!("  a here                 default engine, tag main");
                println!("  a here codex review    codex, tag review");
                println!("  a new --engine shell   full start options, attached");
                println!();
                println!("Discover: a engines · a profiles · a help");
            }
            return Ok(());
        }

        let groups = group_by_workspace(records);
        let home = env::var_os("HOME").map(PathBuf::from);
        let current_workspace = canonical_workspace(Path::new(".")).ok();
        let color = color_enabled();
        let now = now_ms();

        for (workspace_index, (workspace, sessions)) in groups.iter().enumerate() {
            if workspace_index > 0 {
                println!();
            }
            let states: Vec<UxState> = sessions
                .iter()
                .map(|record| ux_state(record, None, now))
                .collect();
            let active = states.iter().filter(|state| state.active).count();
            let attention = states.iter().filter(|state| state.attention).count();
            let stopped = sessions.len().saturating_sub(active);
            let mut summary = format!("{active} active");
            if attention > 0 {
                summary.push_str(&format!(" · {attention} needs you"));
            }
            if stopped > 0 {
                summary.push_str(&format!(" · {stopped} stopped"));
            }
            let here = current_workspace.as_deref() == Some(workspace.as_path());
            let badge = paint(
                color,
                &format!("{ANSI_BOLD}{ANSI_CYAN}"),
                &format!("[{}]", workspace_index + 1),
            );
            let name = paint(
                color,
                ANSI_BOLD,
                &display_workspace(workspace, home.as_deref()),
            );
            let marker = if here {
                paint(color, ANSI_CYAN, "  here")
            } else {
                String::new()
            };
            println!(
                "{badge} {name}{marker}  {}",
                paint(color, ANSI_DIM, &summary)
            );

            let tag_width = sessions
                .iter()
                .map(|record| terminal_display_width(&record.tag))
                .max()
                .unwrap_or(3)
                .clamp(6, 20);
            let engine_width = sessions
                .iter()
                .map(|record| {
                    terminal_display_width(&match &record.profile {
                        Some(profile) => format!("{}/{}", record.engine, profile),
                        None => record.engine.clone(),
                    })
                })
                .max()
                .unwrap_or(6)
                .clamp(6, 22);
            let last = sessions.len().saturating_sub(1);
            for (index, record) in sessions.iter().enumerate() {
                let state = states[index];
                let connector = if index == last { "└─" } else { "├─" };
                let engine = match &record.profile {
                    Some(profile) => format!("{}/{}", record.engine, profile),
                    None => record.engine.clone(),
                };
                let tag = paint(color, ANSI_BOLD, &fit_column(&record.tag, tag_width));
                let engine = paint(color, ANSI_DIM, &fit_column(&engine, engine_width));
                let state_text = fit_column(&format!("{} {}", state.glyph, state.label), 11);
                let state_text = paint(color, state.color, &state_text);
                let timestamp = record
                    .reported_state_at_ms
                    .filter(|at| now.saturating_sub(*at) <= REPORTED_STATE_STALE_MS_UX)
                    .or(record.last_activity_ms)
                    .or(Some(record.updated_at_ms));
                let age = paint(
                    color,
                    ANSI_DIM,
                    &format!("{:>4}", human_age(now, timestamp)),
                );
                println!(
                    "{} {:>2}  {}  {}  {} {}",
                    paint(color, ANSI_GRAY, connector),
                    index + 1,
                    tag,
                    engine,
                    state_text,
                    age
                );
            }
        }

        println!();
        println!(
            "{}",
            paint(
                color,
                ANSI_DIM,
                "Attach: a <workspace#> [session#|tag] · Start here: a here [engine] [tag] · Help: a help"
            )
        );
        Ok(())
    }

    fn cmd_status_ux(paths: &Paths, target: TargetArgs) -> Result<()> {
        let record = resolve(paths, &target)?;
        let (raw, reachable, rpc_error) = match rpc_simple(&record, Operation::Status, None) {
            Ok(raw) => (raw, true, None),
            Err(error) => (
                serde_json::to_value(public_session_record(&record)).unwrap_or(Value::Null),
                false,
                Some(format!("{error:#}")),
            ),
        };
        let current: SessionRecord = serde_json::from_value(raw.clone()).unwrap_or(record);
        let now = now_ms();
        let mut state = ux_state(&current, Some(&raw), now);
        if current.worker_alive() && !reachable {
            state = UxState {
                label: "unreachable",
                glyph: "✗",
                color: ANSI_RED,
                attention: true,
                active: false,
            };
        }
        let color = color_enabled();
        let workspace = display_workspace(
            &current.workspace,
            env::var_os("HOME").as_deref().map(Path::new),
        );
        let engine = match &current.profile {
            Some(profile) => format!("{}/{}", current.engine, profile),
            None => current.engine.clone(),
        };
        println!(
            "{}  {}",
            paint(color, ANSI_BOLD, &current.tag),
            paint(
                color,
                state.color,
                &format!("{} {}", state.glyph, state.label)
            )
        );
        println!("  workspace   {workspace}");
        println!("  engine      {engine}");
        println!("  session     {}", current.id);
        if let Some(foreground) = foreground_override(&current, &raw) {
            println!("  foreground  {foreground}");
        }
        let activity = raw
            .get("last_activity_ms")
            .and_then(Value::as_u64)
            .or(current.last_activity_ms);
        println!("  activity    {}", human_age_phrase(now, activity));
        println!(
            "  command     {}",
            current
                .command
                .iter()
                .map(|value| shell_quote(value))
                .collect::<Vec<_>>()
                .join(" ")
        );
        println!(
            "  processes   worker {} ({}) · workload {}",
            current
                .worker_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "—".to_string()),
            if reachable {
                "reachable"
            } else {
                "unreachable"
            },
            current
                .workload_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "—".to_string())
        );
        if let Some(exit) = &current.exit {
            println!(
                "  exit        code={:?} signal={:?} oom={}",
                exit.code, exit.signal, exit.oom_killed
            );
        }
        if let Some(error) = current.error.as_deref() {
            println!("  error       {error}");
        }
        if let Some(error) = rpc_error {
            println!("  rpc         {error}");
        }
        if let Some(cgroup) = raw.get("cgroup") {
            println!("  resources   {cgroup}");
        }
        println!();
        if state.active && reachable {
            println!("Attach: a open {}", &current.id.to_string()[..8]);
        } else {
            println!(
                "Inspect output: a capture {} --screen --plain",
                &current.id.to_string()[..8]
            );
            println!("Remove record:  a kill {}", &current.id.to_string()[..8]);
        }
        Ok(())
    }

    fn cmd_hotkeys_ux() -> Result<()> {
        println!("Attach-mode keys (press Ctrl-b, then):");
        println!();
        println!("  ?        show this reference in the status bar");
        println!("  d        detach and leave the session running");
        println!("  n / p    next / previous session in this workspace");
        println!("  N / P    next / previous session across workspaces");
        println!("  1-9      jump to the numbered session in the status bar");
        println!("  l        return to the previously attached session");
        println!();
        println!("Any other key after Ctrl-b is forwarded unchanged.");
        Ok(())
    }

    fn cmd_start_attaching_ux(paths: &Paths, args: StartArgs, json_output: bool) -> Result<()> {
        if json_output {
            bail!(
                "--json cannot be combined with `start --attach`: JSON session metadata and terminal bytes cannot share stdout; run `a --json start ...` and `a attach SESSION` separately"
            );
        }
        let env = parse_env(&args.env)?;
        let command = args
            .command
            .iter()
            .map(|value| os_to_utf8(value, "command argument"))
            .collect::<Result<Vec<_>>>()?;
        let mut worker_rows = None;
        let mut worker_cols = None;
        let tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        if let Some((rows, cols)) = tty.then(|| terminal_size(libc::STDIN_FILENO)).flatten() {
            worker_rows = Some(reserved_rows(rows));
            worker_cols = Some(cols);
        }
        let request = aplexer::api::StartRequest {
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
        };
        let ready = aplexer::api::start_session(paths, &request)?;
        println!("{}", ready.id);
        println!("{}", ready.selector());
        attach_ux(paths, &ready, None)
    }

    fn cmd_quick_attach_ux(paths: &Paths, args: QuickAttachArgs) -> Result<()> {
        let record = resolve_quick_index(paths, args.workspace_index, args.session.as_deref())?;
        attach_ux(paths, &record, None)
    }

    fn cmd_quick_launch_ux(paths: &Paths, args: QuickLaunchArgs) -> Result<()> {
        let workspace = canonical_workspace(Path::new("."))?;
        let config = Config::load(paths)?;
        let (tag, engine, profile, command): (
            String,
            Option<String>,
            Option<String>,
            Vec<OsString>,
        ) = match args.rest.as_slice() {
            [] => ("main".to_string(), None, None, vec![]),
            [engine] if config.engines.contains_key(engine) => {
                (engine.clone(), Some(engine.clone()), None, vec![])
            }
            [engine, tag] if config.engines.contains_key(engine) => {
                (tag.clone(), Some(engine.clone()), None, vec![])
            }
            [word] if config.shortcuts.contains_key(word) => {
                let shortcut = &config.shortcuts[word];
                (
                    word.clone(),
                    Some(shortcut.engine.clone()),
                    shortcut.profile.clone(),
                    vec![],
                )
            }
            [word, tag] if config.shortcuts.contains_key(word) => {
                let shortcut = &config.shortcuts[word];
                (
                    tag.clone(),
                    Some(shortcut.engine.clone()),
                    shortcut.profile.clone(),
                    vec![],
                )
            }
            words => (
                command_tag(&words[0]),
                None,
                None,
                words.iter().map(OsString::from).collect(),
            ),
        };
        if let Some(existing) = list_records(paths)?
            .into_iter()
            .find(|record| record.workspace == workspace && record.tag == tag)
        {
            if existing.worker_phase_active() && existing.worker_alive() {
                return attach_ux(paths, &existing, None);
            }
        }
        cmd_start_attaching_ux(
            paths,
            StartArgs {
                workspace: PathBuf::from("."),
                tag,
                engine,
                profile,
                cwd: None,
                env: vec![],
                memory: None,
                pids: None,
                cpu_quota_us: None,
                cpu_period_us: 100_000,
                history_bytes: None,
                attach: true,
                startup_timeout_ms: 10_000,
                no_skip_permissions: false,
                command,
            },
            false,
        )
    }

    fn status_bar_text_ux(ctx: &StatusBarCtx, cols: usize) -> String {
        {
            let mut flash = ctx.flash.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some((message, at)) = flash.clone() {
                if at.elapsed() < FLASH_DURATION {
                    return pad_or_truncate(&sanitize_terminal_text(&format!("[{message}]")), cols);
                }
                *flash = None;
            }
        }

        let record = ctx
            .record
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let raw = live_status(&record);
        let state = ux_state(&record, raw.as_ref(), now_ms());
        let state = format!("{} {}", state.glyph, state.label.to_uppercase());
        let workspace = display_workspace(
            &record.workspace,
            env::var_os("HOME").as_deref().map(Path::new),
        );
        let mut engine = match &record.profile {
            Some(profile) => format!("{}/{}", record.engine, profile),
            None => record.engine.clone(),
        };
        if let Some(foreground) = raw
            .as_ref()
            .and_then(|value| foreground_override(&record, value))
        {
            engine.push_str(&format!(" → {foreground}"));
        }
        let memory = raw
            .as_ref()
            .and_then(|value| memory_indicator(&record, value));
        let siblings = workspace_summary(ctx, &record);

        let mut full = format!("{workspace}:{}  {state}  {engine}", record.tag);
        if let Some(memory) = &memory {
            full.push_str(&format!("  mem {memory}"));
        }
        if !siblings.is_empty() {
            full.push_str("  |  ");
            full.push_str(&siblings);
        }
        full.push_str("  |  ^b ?");

        let mut medium = format!("{}  {state}  {engine}", record.tag);
        if !siblings.is_empty() {
            medium.push_str("  |  ");
            medium.push_str(&siblings);
        }
        medium.push_str("  |  ^b ?");

        let compact = format!("{}  {state}  ^b ?", record.tag);
        let minimum = format!("{state}  ^b ?");
        for candidate in [&full, &medium, &compact, &minimum] {
            let candidate = sanitize_terminal_text(candidate);
            if terminal_display_width(&candidate) <= cols {
                return pad_or_truncate(&candidate, cols);
            }
        }
        pad_or_truncate(&sanitize_terminal_text(&minimum), cols)
    }

    fn draw_status_bar_ux(ctx: &StatusBarCtx, force: bool) -> bool {
        let geom = match ctx.term.lock() {
            Ok(geometry) => *geometry,
            Err(_) => return false,
        };
        if !geom.reserved {
            return false;
        }
        let workload_margins = ctx
            .workload_margins
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .margins();
        let text = status_bar_text_ux(ctx, geom.cols as usize);
        {
            let mut last = ctx
                .last_drawn
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let key = (text.clone(), geom.rows, geom.cols, workload_margins);
            if !force && last.as_ref() == Some(&key) {
                return false;
            }
            *last = Some(key);
        }
        let mut sequence = Vec::new();
        sequence.extend_from_slice(b"\x1b7");
        sequence.extend_from_slice(
            match workload_margins {
                Some((top, bottom)) => format!("\x1b[{top};{bottom}r"),
                None => format!("\x1b[1;{}r", geom.rows - 1),
            }
            .as_bytes(),
        );
        sequence.extend_from_slice(format!("\x1b[{};1H", geom.rows).as_bytes());
        sequence.extend_from_slice(b"\x1b[2K\x1b[7m");
        sequence.extend_from_slice(text.as_bytes());
        sequence.extend_from_slice(b"\x1b[0m\x1b8");
        let _ = write_locked(&ctx.stdout, &sequence);
        true
    }

    enum UxInputAction {
        Forward(Vec<u8>),
        Detach,
        Switch(SwitchTarget),
        Help,
    }

    #[derive(Default)]
    struct UxInputScanner {
        pending_ctrl_b: bool,
    }

    impl UxInputScanner {
        fn scan(&mut self, buffer: &[u8]) -> Vec<UxInputAction> {
            let mut actions = Vec::new();
            let mut output = Vec::new();
            let mut index = 0usize;
            while index < buffer.len() {
                let byte = buffer[index];
                if self.pending_ctrl_b {
                    self.pending_ctrl_b = false;
                    let action = match byte {
                        b'd' => Some(UxInputAction::Detach),
                        b'?' => Some(UxInputAction::Help),
                        b'n' => Some(UxInputAction::Switch(SwitchTarget::Next)),
                        b'p' => Some(UxInputAction::Switch(SwitchTarget::Prev)),
                        b'N' => Some(UxInputAction::Switch(SwitchTarget::NextGlobal)),
                        b'P' => Some(UxInputAction::Switch(SwitchTarget::PrevGlobal)),
                        b'l' => Some(UxInputAction::Switch(SwitchTarget::Last)),
                        b'1'..=b'9' => Some(UxInputAction::Switch(SwitchTarget::Index(
                            (byte - b'0') as usize,
                        ))),
                        _ => None,
                    };
                    if let Some(action) = action {
                        if !output.is_empty() {
                            actions.push(UxInputAction::Forward(std::mem::take(&mut output)));
                        }
                        let detach = matches!(&action, UxInputAction::Detach);
                        actions.push(action);
                        index += 1;
                        if detach {
                            return actions;
                        }
                        continue;
                    }
                    output.push(0x02);
                    continue;
                }
                if byte == 0x02 {
                    self.pending_ctrl_b = true;
                    index += 1;
                    continue;
                }
                output.push(byte);
                index += 1;
            }
            if !output.is_empty() {
                actions.push(UxInputAction::Forward(output));
            }
            actions
        }
    }

    fn set_status_flash(ctx: &StatusBarCtx, message: impl Into<String>) {
        if let Ok(mut flash) = ctx.flash.lock() {
            *flash = Some((message.into(), Instant::now()));
        }
    }

    fn attach_ux(
        paths: &Paths,
        record: &SessionRecord,
        history_bytes: Option<usize>,
    ) -> Result<()> {
        if !io::stdout().is_terminal() {
            return attach(paths, record, history_bytes);
        }
        check_attachable(record)?;
        let explicit_history = history_bytes.is_some();
        let replay_bytes = Some(history_bytes.unwrap_or(DEFAULT_ATTACH_REPLAY_BYTES));
        let input_tty = unsafe { libc::isatty(libc::STDIN_FILENO) } == 1;
        let display_tty = unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
        let initial_geometry = if display_tty {
            terminal_size(libc::STDOUT_FILENO)
        } else if input_tty {
            terminal_size(libc::STDIN_FILENO)
        } else {
            None
        };
        let worker_geometry = initial_geometry.map(|(rows, cols)| {
            (
                if display_tty {
                    reserved_rows(rows)
                } else {
                    rows
                },
                cols,
            )
        });
        let handshake = establish(record, replay_bytes, !explicit_history, worker_geometry)?;
        let mut reader = handshake.reader;
        let stdout = Arc::new(Mutex::new(io::stdout()));
        let _raw = if input_tty {
            Some(RawMode::enter(libc::STDIN_FILENO)?)
        } else {
            None
        };
        let _ui_guard = if display_tty {
            Some(TerminalUiGuard {
                stdout: stdout.clone(),
            })
        } else {
            None
        };
        let writer = Arc::new(Mutex::new(reader.try_clone()?));
        let active = Arc::new(AtomicBool::new(true));
        let detached_by_client = Arc::new(AtomicBool::new(false));
        let mut signal_bridge = if input_tty || display_tty {
            Some(AttachSignalBridge::install(writer.clone(), active.clone())?)
        } else {
            None
        };
        let term = Arc::new(Mutex::new(TermGeom {
            rows: 0,
            cols: 0,
            reserved: false,
        }));
        let last_activity = Arc::new(Mutex::new(Instant::now()));
        let shared_record = Arc::new(Mutex::new(record.clone()));
        let pending_switch: Arc<Mutex<Option<SwitchOutcome>>> = Arc::new(Mutex::new(None));
        let switch_in_progress = Arc::new(AtomicBool::new(false));
        let last_session: Arc<Mutex<Option<Uuid>>> = Arc::new(Mutex::new(None));
        let switch_replay_bytes = Some(history_bytes.unwrap_or(SWITCH_REPLAY_BYTES));
        let workload_margins = Arc::new(Mutex::new(aplexer::screen::MarginTracker::new(
            worker_geometry.map(|(rows, _)| rows).unwrap_or(24),
        )));
        let status_ctx = StatusBarCtx {
            stdout: stdout.clone(),
            term: term.clone(),
            paths: paths.clone(),
            record: shared_record.clone(),
            flash: Arc::new(Mutex::new(None)),
            last_drawn: Arc::new(Mutex::new(None)),
            workload_margins: workload_margins.clone(),
        };

        if display_tty {
            if let Some((rows, cols)) = initial_geometry {
                apply_terminal_layout(&stdout, &term, rows, cols);
            }
        }
        scan_workload_margins(&workload_margins, &handshake.initial);
        write_locked(&stdout, &handshake.initial)?;
        if display_tty {
            set_status_flash(&status_ctx, "attached · Ctrl-b ? help · Ctrl-b d detach");
            draw_status_bar_ux(&status_ctx, true);
        }
        if handshake.screen.is_none() {
            if let Some((rows, cols)) = worker_geometry {
                send_control(&writer, &AttachControl::Resize { rows, cols })?;
            }
        }

        let input_writer = writer.clone();
        let input_active = active.clone();
        let input_detached = detached_by_client.clone();
        let input_paths = paths.clone();
        let input_term = term.clone();
        let input_shared_record = shared_record.clone();
        let input_last_session = last_session.clone();
        let input_pending_switch = pending_switch.clone();
        let input_switch_in_progress = switch_in_progress.clone();
        let input_status_ctx = status_ctx.clone();
        let input_want_screen = !explicit_history;
        thread::spawn(move || {
            let mut input = io::stdin();
            let mut buffer = [0u8; 8192];
            let mut scanner = UxInputScanner::default();
            'outer: while input_active.load(Ordering::Relaxed) {
                let count = match input.read(&mut buffer) {
                    Ok(0) => {
                        input_detached.store(true, Ordering::Relaxed);
                        detach_attached_client(&input_writer, &input_active);
                        break;
                    }
                    Ok(count) => count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        input_detached.store(true, Ordering::Relaxed);
                        detach_attached_client(&input_writer, &input_active);
                        break;
                    }
                };
                if !input_tty {
                    if send_data(&input_writer, &buffer[..count]).is_err() {
                        detach_attached_client(&input_writer, &input_active);
                        break;
                    }
                    continue;
                }
                for action in scanner.scan(&buffer[..count]) {
                    match action {
                        UxInputAction::Forward(bytes) => {
                            if send_data(&input_writer, &bytes).is_err() {
                                detach_attached_client(&input_writer, &input_active);
                                break 'outer;
                            }
                        }
                        UxInputAction::Detach => {
                            input_detached.store(true, Ordering::Relaxed);
                            detach_attached_client(&input_writer, &input_active);
                            break 'outer;
                        }
                        UxInputAction::Help => {
                            set_status_flash(&input_status_ctx, UX_HOTKEYS);
                            draw_status_bar_ux(&input_status_ctx, true);
                        }
                        UxInputAction::Switch(target) => {
                            let result = perform_switch(
                                &input_paths,
                                target,
                                switch_replay_bytes,
                                input_want_screen,
                                &input_term,
                                &input_shared_record,
                                &input_last_session,
                                &input_writer,
                                &input_pending_switch,
                                &input_switch_in_progress,
                            );
                            if let Err(error) = result {
                                set_status_flash(&input_status_ctx, format!("{error:#}"));
                                draw_status_bar_ux(&input_status_ctx, true);
                            }
                        }
                    }
                }
            }
        });

        if display_tty {
            let resize_writer = writer.clone();
            let resize_active = active.clone();
            let resize_stdout = stdout.clone();
            let resize_term = term.clone();
            let resize_margins = workload_margins.clone();
            let resize_initial = initial_geometry;
            thread::spawn(move || {
                let mut last = resize_initial;
                while resize_active.load(Ordering::Relaxed) {
                    let size = terminal_size(libc::STDOUT_FILENO);
                    if size != last {
                        if let Some((rows, cols)) = size {
                            if let Ok(mut margins) = resize_margins.lock() {
                                margins.set_rows(reserved_rows(rows));
                            }
                            apply_terminal_layout(&resize_stdout, &resize_term, rows, cols);
                            if send_control(
                                &resize_writer,
                                &AttachControl::Resize {
                                    rows: reserved_rows(rows),
                                    cols,
                                },
                            )
                            .is_err()
                            {
                                last = size;
                                continue;
                            }
                        }
                        last = size;
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            });

            let status_active = active.clone();
            let status_last_activity = last_activity.clone();
            let thread_status_ctx = status_ctx.clone();
            thread::spawn(move || {
                let mut last_draw = Instant::now();
                let mut last_seen_activity = status_last_activity
                    .lock()
                    .map(|time| *time)
                    .unwrap_or_else(|_| Instant::now());
                let mut drawn_for_current_idle = false;
                while status_active.load(Ordering::Relaxed) {
                    thread::sleep(STATUS_BAR_POLL_INTERVAL);
                    let activity = match status_last_activity.lock() {
                        Ok(time) => *time,
                        Err(_) => continue,
                    };
                    if activity != last_seen_activity {
                        last_seen_activity = activity;
                        drawn_for_current_idle = false;
                    }
                    let overdue = last_draw.elapsed() >= STATUS_BAR_MAX_INTERVAL;
                    if (activity.elapsed() >= STATUS_BAR_IDLE_GAP && !drawn_for_current_idle)
                        || overdue
                    {
                        if draw_status_bar_ux(&thread_status_ctx, overdue) {
                            last_draw = Instant::now();
                        }
                        drawn_for_current_idle = true;
                    }
                }
            });
        }

        let mut session_ended = false;
        'session: loop {
            loop {
                let frame = match read_frame(&mut reader) {
                    Ok(Some(frame)) => frame,
                    Ok(None) => break,
                    Err(error)
                        if error
                            .downcast_ref::<io::Error>()
                            .map(|error| {
                                matches!(
                                    error.kind(),
                                    io::ErrorKind::ConnectionReset | io::ErrorKind::UnexpectedEof
                                )
                            })
                            .unwrap_or(false) =>
                    {
                        break
                    }
                    Err(error) => return Err(error),
                };
                match frame.kind {
                    FrameKind::Data => {
                        scan_workload_margins(&workload_margins, &frame.payload);
                        write_locked(&stdout, &frame.payload)?;
                        if let Ok(mut time) = last_activity.lock() {
                            *time = Instant::now();
                        }
                    }
                    FrameKind::End => {
                        session_ended = !detached_by_client.load(Ordering::Relaxed);
                        break;
                    }
                    FrameKind::Json => {
                        let event: ServerEvent = serde_json::from_slice(&frame.payload)?;
                        match event {
                            ServerEvent::Exit { .. } => {
                                session_ended = true;
                                break;
                            }
                            ServerEvent::Error { message } => {
                                eprintln!("[aplexer: {message}]");
                                break;
                            }
                            ServerEvent::Layout { .. } => {
                                draw_status_bar_ux(&status_ctx, true);
                            }
                        }
                    }
                }
            }

            let Some(outcome) = take_pending_switch(&pending_switch, &switch_in_progress) else {
                break;
            };
            *shared_record.lock().unwrap_or_else(PoisonError::into_inner) = outcome.record;
            reader = outcome.reader;
            let mut sequence = TERMINAL_RESET_SEQUENCE.to_vec();
            sequence.extend_from_slice(&outcome.history);
            reset_workload_margins(&workload_margins, &term);
            scan_workload_margins(&workload_margins, &outcome.history);
            let _ = write_locked(&stdout, &sequence);
            if let Ok(mut time) = last_activity.lock() {
                *time = Instant::now();
            }
            let geometry = term.lock().map(|geometry| *geometry).unwrap_or(TermGeom {
                rows: 0,
                cols: 0,
                reserved: false,
            });
            if geometry.rows > 0 {
                let _ = send_control(
                    &writer,
                    &AttachControl::Resize {
                        rows: reserved_rows(geometry.rows),
                        cols: geometry.cols,
                    },
                );
            }
            draw_status_bar_ux(&status_ctx, true);
            continue 'session;
        }

        active.store(false, Ordering::Relaxed);
        if let Ok(stream) = writer.lock() {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        let final_record = shared_record
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        drop(_ui_guard);
        drop(_raw);
        if let Some(signal) = signal_bridge.take().and_then(AttachSignalBridge::finish) {
            unsafe {
                libc::raise(signal);
            }
        }
        if display_tty {
            if session_ended {
                eprintln!(
                    "Session ended: {}. Inspect with `a status {}`.",
                    final_record.selector(),
                    &final_record.id.to_string()[..8]
                );
            } else {
                eprintln!("Detached from {}.", final_record.selector());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod ux_tests {
        use super::*;

        fn args(values: &[&str]) -> Vec<String> {
            values.iter().map(|value| value.to_string()).collect()
        }

        #[test]
        fn here_is_a_discoverable_name_for_quick_launch() {
            let rewritten = rewrite_ux_aliases(args(&["a", "here", "codex", "review"]));
            assert_eq!(rewritten, args(&["a", "-", "codex", "review"]));
        }

        #[test]
        fn new_starts_attached() {
            let rewritten = rewrite_ux_aliases(args(&["a", "new", "--engine", "shell"]));
            assert_eq!(
                rewritten,
                args(&["a", "start", "--attach", "--engine", "shell"])
            );
        }

        #[test]
        fn help_alias_targets_subcommand_help() {
            let rewritten = rewrite_ux_aliases(args(&["a", "help", "capture"]));
            assert_eq!(rewritten, args(&["a", "capture", "--help"]));
        }

        #[test]
        fn ctrl_b_question_mark_is_help_not_terminal_input() {
            let mut scanner = UxInputScanner::default();
            let actions = scanner.scan(&[0x02, b'?']);
            assert!(matches!(actions.as_slice(), [UxInputAction::Help]));
        }

        #[test]
        fn human_age_uses_compact_units() {
            assert_eq!(human_age(10_000, Some(9_000)), "now");
            assert_eq!(human_age(70_000, Some(10_000)), "1m");
            assert_eq!(human_age(7_210_000, Some(10_000)), "2h");
        }

        #[test]
        fn human_age_phrase_reads_naturally() {
            assert_eq!(human_age_phrase(10_000, Some(9_000)), "just now");
            assert_eq!(human_age_phrase(70_000, Some(10_000)), "1m ago");
            assert_eq!(human_age_phrase(70_000, None), "unknown");
        }

        #[test]
        fn fit_column_marks_truncation() {
            assert_eq!(fit_column("abcdefgh", 5), "abcd…");
            assert_eq!(terminal_display_width(&fit_column("界界界", 5)), 5);
        }
    }
}

fn main() {
    legacy::entry();
}
