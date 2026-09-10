use super::*;

/// `a whoami` -- lets an agent or script running INSIDE a session (or a
/// human at its prompt) ask "am I in an aplexer session, and if so which
/// one" without hand-parsing environment variables. Every workload already
/// has APLEXER_SESSION_ID/WORKSPACE/TAG injected (see spawn_workload in
/// worker.rs) -- this just resolves the id against the session's persisted
/// record for the fuller picture (engine, profile, phase) and gives a
/// stable, scriptable "nothing/non-zero if not inside one" contract, the
/// same shape `$TMUX` serves for tmux but structured instead of a bare path.
pub(crate) fn cmd_whoami(paths: &Paths, json_output: bool) -> Result<()> {
    let Some(id) = discover_session_id() else {
        // Deliberately silent on stdout either way -- a script doing
        // `id=$(a whoami --json)` should see empty output and rely on the
        // exit code, not have to filter out a "not in a session" sentence.
        if !json_output {
            eprintln!("not inside an aplexer session");
        }
        std::process::exit(1);
    };
    let record = read_record(&paths.record(id)).with_context(|| {
        format!("session {id} (from APLEXER_SESSION_ID) has no persisted record")
    })?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&public_session_record(&record))?
        );
    } else {
        println!("id: {}", record.id);
        println!("selector: {}", record.selector());
        println!("engine: {}", record.engine);
        if let Some(profile) = &record.profile {
            println!("profile: {profile}");
        }
        // On a terminal, lead with the honest semantic state and show the
        // underlying lifecycle when it differs; redirected output keeps the
        // raw phase word the pre-UX build printed.
        if io::stdout().is_terminal() {
            let (state, source) = session_ui_state(&record, now_ms());
            println!("state: {state}{}", state_source_suffix(source));
            let lifecycle =
                derived_liveness(&record.phase, record.worker_alive(), record.created_at_ms);
            if lifecycle != state {
                println!("lifecycle: {lifecycle}");
            }
        } else {
            println!("state: {}", record.phase.name());
        }
    }
    Ok(())
}

/// `a state-report <idle|waiting|working>`
/// (docs/pocketshell-integration-plan.md Open question #2, "Agent-state
/// ingestion"): lets a hook running INSIDE a session push its own semantic
/// state -- the missing half of `a watch --jsonl`'s `agent.state` event,
/// which otherwise only has a coarse PTY-recency heuristic to go on (see
/// watch.rs's `fresh_reported_state`/`derive_agent_state_with_source` for
/// exactly how a push is merged with that heuristic and for how long it
/// stays authoritative).
///
/// Resolves its target exactly like `a whoami` -- via the injected
/// `APLEXER_SESSION_ID`, never a selector -- because a hook script has no
/// notion of "which session" other than the one it is running inside; see
/// `cmd_whoami`'s doc comment for the shared mechanism (`discover_session_id`,
/// the same env var `worker.rs::spawn_workload` injects into every
/// session). Same exit-code contract as `a whoami`: a plain `exit(1)` with
/// one stderr line when `APLEXER_SESSION_ID` is unset (so a hook wired as
/// `a state-report waiting || true` degrades silently outside aplexer);
/// any other failure (record missing, worker dead/unreachable, invalid
/// state rejected by the worker) propagates through `?` to `main`'s
/// generic `a: {error}` / exit(1) handler, same as every other subcommand.
///
/// What this repo does NOT do here (deliberately): install the hooks that
/// call this command. That wiring lives in `a init` (`aplexer::hooks`),
/// which merges a `state-report` hook into every configured engine
/// (Claude Stop/Notification, Codex hooks/notify, OpenCode plugin, Grok
/// and Gemini hooks) — this command is the ingestion primitive it builds
/// on. `a init --check --json` is the machine-readable way to verify the
/// wiring is present.
pub(crate) fn cmd_state_report(paths: &Paths, state: ReportedState) -> Result<()> {
    let Some(id) = discover_session_id() else {
        eprintln!("a state-report: not inside an aplexer session (APLEXER_SESSION_ID not set)");
        std::process::exit(1);
    };
    let record = read_record(&paths.record(id)).with_context(|| {
        format!("session {id} (from APLEXER_SESSION_ID) has no persisted record")
    })?;
    rpc_simple(
        &record,
        Operation::ReportState {
            state: state.as_str().to_string(),
        },
        None,
    )?;
    Ok(())
}

/// `a init [--check] [--uninstall] [--engine NAME]`
///
/// Machine-wide agent-state hook installation: merges an `a state-report`
/// hook into every agent engine aplexer knows how to launch (claude, codex
/// — which also covers the zcodex variant via shared `CODEX_HOME` — grok,
/// gemini, opencode), including each configured profile's config dir, so a
/// session reports `working`/`waiting`/`idle` instead of leaving every
/// consumer to guess from PTY-output recency. See `aplexer::hooks` for the
/// per-engine mechanisms and the merge-never-clobber rules.
///
/// Modes (exactly one):
///
/// - default: install (idempotent; only writes files that change).
/// - `--check`: touch nothing; print per-engine status and exit 0 when
///   fully initialized, 1 otherwise. With `--json` this prints
///   `{"initialized": bool, "engines": [...]}` — the machine contract the
///   PocketShell host CLI automates against (run `a init --check --json`;
///   when it reports `initialized: false`, run `a init`).
/// - `--uninstall`: remove our hooks again.
///
/// `--engine` limits any mode to one engine (`zcodex` maps onto `codex`).
pub(crate) fn cmd_init(paths: &Paths, args: InitArgs, json_output: bool) -> Result<()> {
    if args.check && args.uninstall {
        bail!("`a init --check` and `a init --uninstall` cannot be combined");
    }
    let filter = args
        .engine
        .as_deref()
        .map(aplexer::hooks::normalize_engine_filter)
        .transpose()?;
    // Profile config dirs (CLAUDE_CONFIG_DIR / CODEX_HOME) extend the
    // install targets past the default homes, so a profile session reports
    // state just like a default one. A broken user config fails here the
    // same way it fails every other command.
    let config = Config::load(paths)?;
    let profile_envs: Vec<BTreeMap<String, String>> = config
        .profiles
        .values()
        .map(|profile| profile.env.clone())
        .collect();
    let targets = aplexer::hooks::resolve_targets_from_env(&profile_envs)?;
    let a_bin = aplexer::hooks::resolve_a_bin();

    if args.check {
        let statuses = aplexer::hooks::check(&targets, filter);
        let initialized = statuses.iter().all(|status| status.installed);
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "initialized": initialized,
                    "engines": statuses,
                }))?
            );
        } else {
            for status in &statuses {
                println!(
                    "{} {:<9} {}",
                    if status.installed { "OK  " } else { "MISS" },
                    status.engine,
                    status.message
                );
            }
            if initialized {
                println!("hooks initialized for all engines");
            } else {
                println!("hooks missing for some engines; run `a init` to install");
            }
        }
        if !initialized {
            bail!("agent-state hooks are not fully installed");
        }
        return Ok(());
    }

    if args.uninstall {
        let statuses = aplexer::hooks::uninstall(&targets, filter);
        if json_output {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "engines": statuses,
                }))?
            );
        } else {
            for status in &statuses {
                println!("{}: {} — {}", status.engine, status.action, status.message);
            }
        }
        return Ok(());
    }

    let statuses = aplexer::hooks::install(&targets, &a_bin, filter);
    let ok = statuses.iter().all(|status| status.action != "error");
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": ok,
                "engines": statuses,
            }))?
        );
    } else {
        for status in &statuses {
            println!("{}: {} — {}", status.engine, status.action, status.message);
        }
    }
    if !ok {
        bail!("agent-state hook installation hit errors");
    }
    Ok(())
}
