use super::*;

pub(crate) fn cmd_status(paths: &Paths, target: TargetArgs, json_output: bool) -> Result<()> {
    let record = resolve(paths, &target)?;
    // Process existence and control-plane reachability are separate facts:
    // a wedged worker can still have a live pid, while a successfully reached
    // worker is stronger evidence than a stale persisted pid. Preserve both
    // instead of folding them into one optimistic `worker_alive` bit, and
    // surface the actual RPC failure so recovery tooling has evidence to act
    // on rather than a mysteriously stale record.
    let (raw, worker_reachable, rpc_error) = match rpc_simple(&record, Operation::Status, None) {
        Ok(raw) => (raw, true, None),
        Err(error) => (
            serde_json::to_value(public_session_record(&record)).unwrap_or(Value::Null),
            false,
            Some(format!("{error:#}")),
        ),
    };
    let current: SessionRecord = serde_json::from_value(raw.clone()).unwrap_or(record);
    let cgroup_stats = raw.get("cgroup").cloned();
    let history_persistence_error = raw
        .get("history_persistence_error")
        .and_then(Value::as_str)
        .map(str::to_string);
    let record_persistence_error = raw
        .get("record_persistence_error")
        .and_then(Value::as_str)
        .map(str::to_string);
    // Live-only (see foreground_command in lib.rs / Operation::Status):
    // never persisted to session.json, so this is only available while the
    // worker is reachable -- absent on a dead/unreachable session, same as
    // cgroup_stats above.
    let foreground_command = raw
        .get("foreground_command")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let worker_alive = current.worker_alive();
    if json_output {
        let mut value = serde_json::to_value(public_session_record(&current))?;
        if let Some(stats) = cgroup_stats {
            value["cgroup"] = stats;
        }
        if let Some(fg) = &foreground_command {
            value["foreground_command"] = json!(fg);
        }
        if let Some(error) = &history_persistence_error {
            value["history_persistence_error"] = json!(error);
        }
        if let Some(error) = &record_persistence_error {
            value["record_persistence_error"] = json!(error);
        }
        value["worker_alive"] = json!(worker_alive);
        // The same derived fact the human branch prints as `state:` and
        // every `a list --json`/`a snapshot` row carries, from the same
        // helper so the three can never disagree: a SIGKILLed worker
        // leaves `phase` at "running" forever, so a machine consumer of
        // `status` reading `phase` alone could not tell a zombie record
        // from a live session -- while the same command was telling a
        // human "broken".
        value["state"] = json!(derived_liveness(
            &current.phase,
            worker_alive,
            current.created_at_ms
        ));
        // Which agent is running inside the session's workload tree right
        // now, from the same query-time detection every `a list --json` row
        // carries (`api::record_agent`). Always present; `null` when no
        // agent is detectable.
        value["agent"] = json!(aplexer::api::record_agent(&current));
        // Same derived placement facts every `a list --json`/`a snapshot`
        // row carries, from the same helper so no two commands can
        // disagree about whether a session shares the per-user manager's
        // failure domain (issue #1).
        value["worker_placement"] =
            aplexer::placement::placement_summary(current.worker_cgroup.as_deref());
        value["workload_placement"] =
            aplexer::placement::placement_summary(current.workload_cgroup.as_deref());
        value["worker_reachable"] = json!(worker_reachable);
        if let Some(error) = &rpc_error {
            value["rpc_error"] = json!(error);
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else if io::stdout().is_terminal() {
        cmd_status_tty(
            paths,
            &current,
            &raw,
            worker_reachable,
            rpc_error.as_deref(),
            history_persistence_error.as_deref(),
            record_persistence_error.as_deref(),
        )?;
    } else {
        println!("id: {}", current.id);
        println!("selector: {}", current.selector());
        println!(
            "state: {}",
            derived_liveness(&current.phase, worker_alive, current.created_at_ms)
        );
        let ep = match &current.profile {
            Some(p) => format!("{}/{p}", current.engine),
            None => current.engine.clone(),
        };
        // Filtered the same way the attach status bar filters it
        // (`foreground_override`): omit a bare interactive shell or a
        // foreground command that's just the engine's own launch command
        // running as expected, so this line matches what `a status` calls
        // out as "different from what you started."
        match foreground_override(&current, &raw) {
            Some(fg) => println!("engine: {ep} (foreground: {fg})"),
            None => println!("engine: {ep}"),
        }
        println!(
            "worker_pid: {}",
            current
                .worker_pid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        );
        println!("worker_alive: {worker_alive}");
        println!("worker_reachable: {worker_reachable}");
        if let Some(error) = rpc_error {
            println!("rpc_error: {error}");
        }
        println!(
            "workload_pid: {}",
            current
                .workload_pid
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "command: {}",
            current
                .command
                .iter()
                .map(|v| shell_quote(v))
                .collect::<Vec<_>>()
                .join(" ")
        );
        if let Some(exit) = current.exit {
            println!(
                "exit: code={:?} signal={:?} oom_killed={}",
                exit.code, exit.signal, exit.oom_killed
            );
        }
        if let Some(stats) = cgroup_stats {
            println!("cgroup: {stats}");
        }
        if let Some(error) = current.error {
            println!("error: {error}");
        }
        if let Some(error) = history_persistence_error {
            println!("history_persistence_error: {error}");
        }
        if let Some(error) = record_persistence_error {
            println!("record_persistence_error: {error}");
        }
    }
    Ok(())
}

/// The terminal rendering of `a status` -- task-first (tag and state lead;
/// pids and sockets are evidence at the bottom), state qualified by its
/// source, and exactly one next action chosen from lifecycle/reachability/
/// containment evidence rather than a generic "try these commands" list.
/// The redirected rendering above stays byte-identical to the pre-UX format.
pub(crate) fn cmd_status_tty(
    paths: &Paths,
    current: &SessionRecord,
    raw: &Value,
    worker_reachable: bool,
    rpc_error: Option<&str>,
    history_persistence_error: Option<&str>,
    record_persistence_error: Option<&str>,
) -> Result<()> {
    let now = now_ms();
    let (mut state, mut source) = session_ui_state(current, now);
    // A live worker that will not answer is its own condition -- more
    // specific than any state the record could claim.
    if current.worker_alive() && !worker_reachable {
        state = "unreachable";
        source = "lifecycle";
    }
    let color = color_enabled();
    let short_id = current.id.to_string()[..8].to_string();
    let workspace = display_workspace(
        &current.workspace,
        env::var_os("HOME").as_deref().map(Path::new),
    );
    let engine = match &current.profile {
        Some(profile) => format!("{}/{}", current.engine, profile),
        None => current.engine.clone(),
    };

    let (glyph, glyph_color) = state_glyph(state);
    println!(
        "{}  {}",
        paint(color, ANSI_BOLD, &current.tag),
        paint(color, glyph_color, &format!("{glyph} {state}"))
    );
    let suffix = state_source_suffix(source);
    if !suffix.is_empty() {
        println!("  {}", paint(color, ANSI_DIM, suffix.trim()));
    }
    let lifecycle = derived_liveness(
        &current.phase,
        current.worker_alive(),
        current.created_at_ms,
    );
    if lifecycle != state {
        println!(
            "  {}",
            paint(color, ANSI_DIM, &format!("lifecycle: {lifecycle}"))
        );
    }
    println!("  workspace   {workspace}");
    println!("  engine      {engine}");
    // Same display rule as the list and the attach status bar: the detected
    // agent gets its own line only when the declared engine doesn't already
    // name it (`api::record_agent`, the value `a status --json` reports as
    // `agent`).
    if let Some(agent) = extra_agent_label(current, aplexer::api::record_agent(current)) {
        println!("  agent       {agent}");
    }
    println!("  session     {}", current.id);
    if let Some(parent) = current.parent_session {
        // Same rendering rule as `a list`: the parent's tag while its
        // record exists, a short id once it doesn't.
        let label = read_record(&paths.record(parent))
            .map(|record| record.tag)
            .unwrap_or_else(|_| parent.to_string()[..8].to_string());
        println!(
            "  {}",
            paint(color, ANSI_DIM, &format!("parent      {label}"))
        );
    }
    if let Some(foreground) = foreground_override(current, raw) {
        println!("  foreground  {foreground}");
    }
    let activity = raw
        .get("last_activity_ms")
        .and_then(Value::as_u64)
        .or(current.last_activity_ms);
    let activity_text = match activity {
        Some(at) if at <= now => human_age_phrase(now - at),
        _ => "unknown".to_string(),
    };
    println!("  activity    {activity_text}");
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
        if worker_reachable {
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
    if let Some(error) = history_persistence_error {
        println!("  history     {error}");
    }
    if let Some(error) = record_persistence_error {
        println!("  record      {error}");
    }
    if let Some(cgroup) = raw.get("cgroup") {
        if !cgroup.is_null() {
            println!("  resources   {cgroup}");
        }
    }
    println!();

    // One next action, chosen from the same evidence model `a doctor` uses:
    // attach what is live, capture what is over, and get dead records out of
    // the way by the cheapest safe route (`a prune` when the record is one
    // it can reap, else `a kill` -- whose own refusal message is the right
    // teacher for the rare uncontainable case). A live-but-unreachable
    // worker gets a diagnosis pointer, not a destructive command.
    let attachable = matches!(
        current.phase,
        Phase::Starting | Phase::Running | Phase::Exiting
    ) && current.worker_alive()
        && worker_reachable;
    if attachable {
        println!("Attach: a open {short_id}");
        return Ok(());
    }
    if matches!(current.phase, Phase::Exited | Phase::Failed) || state == "broken" {
        println!("Inspect output: a capture {short_id} --screen --plain");
    }
    if !current.worker_alive() {
        if reap_verdict(current).is_some() {
            println!("Remove record:  a prune");
        } else {
            println!("Remove record:  a kill {short_id}");
        }
    } else if !worker_reachable {
        println!("Diagnose:       a check");
    }
    Ok(())
}
