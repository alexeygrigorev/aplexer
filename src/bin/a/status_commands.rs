use super::*;

pub(crate) struct StatusData {
    pub(crate) current: SessionRecord,
    pub(crate) raw: Value,
    pub(crate) worker_reachable: bool,
    pub(crate) rpc_error: Option<String>,
    pub(crate) cgroup_stats: Option<Value>,
    pub(crate) history_persistence_error: Option<String>,
    pub(crate) record_persistence_error: Option<String>,
    pub(crate) foreground_command: Option<String>,
}

impl StatusData {
    fn load(record: SessionRecord) -> Self {
        // Process existence and control-plane reachability are separate facts:
        // a wedged worker can still have a live pid, while a successfully
        // reached worker is stronger evidence than a stale persisted pid.
        let (raw, worker_reachable, rpc_error) = match rpc_simple(&record, Operation::Status, None)
        {
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
        let foreground_command = raw
            .get("foreground_command")
            .and_then(Value::as_str)
            .map(str::to_string);
        Self {
            worker_reachable,
            rpc_error,
            cgroup_stats,
            history_persistence_error,
            record_persistence_error,
            foreground_command,
            current,
            raw,
        }
    }

    fn worker_alive(&self) -> bool {
        self.current.worker_alive()
    }

    pub(crate) fn json_value(&self) -> Result<Value> {
        let mut value = serde_json::to_value(public_session_record(&self.current))?;
        if let Some(stats) = &self.cgroup_stats {
            value["cgroup"] = stats.clone();
        }
        if let Some(foreground) = &self.foreground_command {
            value["foreground_command"] = json!(foreground);
        }
        if let Some(error) = &self.history_persistence_error {
            value["history_persistence_error"] = json!(error);
        }
        if let Some(error) = &self.record_persistence_error {
            value["record_persistence_error"] = json!(error);
        }
        value["worker_alive"] = json!(self.worker_alive());
        value["state"] = json!(derived_liveness(
            &self.current.phase,
            self.worker_alive(),
            self.current.created_at_ms
        ));
        let detected = aplexer::api::record_detected(&self.current);
        value["agent"] = json!(detected.as_ref().map(|d| d.kind));
        value["agent_profile"] = json!(detected.as_ref().map(|d| d.profile_label()));
        value["worker_placement"] =
            aplexer::placement::placement_summary(self.current.worker_cgroup.as_deref());
        value["workload_placement"] =
            aplexer::placement::placement_summary(self.current.workload_cgroup.as_deref());
        value["worker_reachable"] = json!(self.worker_reachable);
        if let Some(error) = &self.rpc_error {
            value["rpc_error"] = json!(error);
        }
        Ok(value)
    }

    fn print_json(&self) -> Result<()> {
        println!("{}", serde_json::to_string_pretty(&self.json_value()?)?);
        Ok(())
    }

    fn print_plain(&self) {
        let current = &self.current;
        println!("id: {}", current.id);
        println!("selector: {}", current.selector());
        println!(
            "state: {}",
            derived_liveness(&current.phase, self.worker_alive(), current.created_at_ms)
        );
        let engine_profile = engine_profile(current);
        // Match the attach status bar's foreground filtering so a bare
        // interactive shell or the engine's own launch command is not
        // mislabeled as a surprising foreground process.
        match foreground_override(current, &self.raw) {
            Some(foreground) => println!("engine: {engine_profile} (foreground: {foreground})"),
            None => println!("engine: {engine_profile}"),
        }
        println!(
            "worker_pid: {}",
            current
                .worker_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".into())
        );
        println!("worker_alive: {}", self.worker_alive());
        println!("worker_reachable: {}", self.worker_reachable);
        if let Some(error) = &self.rpc_error {
            println!("rpc_error: {error}");
        }
        println!(
            "workload_pid: {}",
            current
                .workload_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".into())
        );
        println!(
            "command: {}",
            current
                .command
                .iter()
                .map(|value| shell_quote(value))
                .collect::<Vec<_>>()
                .join(" ")
        );
        if let Some(exit) = &current.exit {
            println!(
                "exit: code={:?} signal={:?} oom_killed={}",
                exit.code, exit.signal, exit.oom_killed
            );
        }
        if let Some(stats) = &self.cgroup_stats {
            println!("cgroup: {stats}");
        }
        if let Some(error) = &current.error {
            println!("error: {error}");
        }
        if let Some(error) = &self.history_persistence_error {
            println!("history_persistence_error: {error}");
        }
        if let Some(error) = &self.record_persistence_error {
            println!("record_persistence_error: {error}");
        }
    }
}

pub(crate) fn cmd_status(paths: &Paths, target: TargetArgs, json_output: bool) -> Result<()> {
    let status = StatusData::load(resolve(paths, &target)?);
    if json_output {
        status.print_json()?;
    } else if io::stdout().is_terminal() {
        cmd_status_tty(paths, &status)?;
    } else {
        status.print_plain();
    }
    Ok(())
}

/// The terminal rendering of `a status` -- task-first (tag and state lead;
/// pids and sockets are evidence at the bottom), state qualified by its
/// source, and exactly one next action chosen from lifecycle/reachability/
/// containment evidence rather than a generic "try these commands" list.
/// The redirected rendering above stays byte-identical to the pre-UX format.
fn cmd_status_tty(paths: &Paths, status: &StatusData) -> Result<()> {
    let current = &status.current;
    let raw = &status.raw;
    let worker_reachable = status.worker_reachable;
    let rpc_error = status.rpc_error.as_deref();
    let history_persistence_error = status.history_persistence_error.as_deref();
    let record_persistence_error = status.record_persistence_error.as_deref();
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
    let engine = engine_profile(current);

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
    // name it (`api::record_detected`, the pair `a status --json` reports as
    // `agent`/`agent_profile`).
    if let Some(agent) = extra_agent_label(current, aplexer::api::record_detected(current).as_ref())
    {
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
