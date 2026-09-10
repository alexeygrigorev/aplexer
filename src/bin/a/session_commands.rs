/// `a <N>` / `a <N> <M>` / `a <N> <tag>` -- attach by position in the same
/// workspace tree `a list` prints, or by tag within a chosen workspace.
/// `a -` and friends -- create-or-attach in the current directory, agent
/// engines and tags used the same way spec.md's own worked examples do
/// (workspace ~/git/pocketshell, tags main/review/issue-2294, engines
/// claude/codex). Whether the first word after "-" names a real engine, a
/// shortcut, or is a literal command to run (mirroring tmuxctl's `t -
/// <command>`) is decided against the real engine registry and the
/// `config.shortcuts` map -- never a fixed word list -- and in that
/// precedence order:
///
///   1. real engine id (`config.engines`)
///   2. shortcut id (`config.shortcuts`)
///   3. literal command
///
/// Engines are checked first so a real engine name always means exactly
/// what it says -- `a - claude` must never behave differently just because
/// someone also configured a shortcut named "claude". Shortcuts are checked
/// next, ahead of the literal-command fallback: a shortcut is meant to be a
/// fast path onto exactly what typing the full `--engine`/`--profile` pair
/// would already produce (see spec.md 9/23), so it sits directly below real
/// engine names and above running an arbitrary binary. In practice a
/// shortcut id realistically never collides with a real engine id (they're
/// deliberately short, e.g. "cl"/"coz") or with a command someone would
/// actually type standalone, but the ordering is still deliberate rather
/// than incidental.
///
///   a -                  tag "main", default engine
///   a - claude           tag "claude" (defaults to the engine name), engine claude
///   a - claude review    tag "review", engine claude
///   a - clz              tag "clz" (defaults to the shortcut's own id, not
///                        "claude" -- so `a - cl` and `a - clz` don't
///                        collide on the same tag), engine claude, profile zlaude
///   a - clz review       tag "review", engine claude, profile zlaude
///   a - htop             tag "htop" (defaults to the command name), runs `htop` literally
///
/// Re-running the same shortcut reattaches to a live matching session
/// instead of erroring, like tmuxctl's own create_or_attach.
/// Default tag for a literal-command quick-launch: the command's own base
/// name, normalized to the charset validate_tag accepts. Deliberately NOT
/// "main" for every arbitrary command -- `a - htop` reusing the same tag as
/// `a -`'s plain shell would silently reattach to that shell instead of
/// ever running htop.

fn command_tag(word: &str) -> String {
    let base = Path::new(word)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(word);
    let sanitized: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "cmd".to_string()
    } else {
        sanitized
    }
}

fn cmd_quick_launch(paths: &Paths, args: QuickLaunchArgs) -> Result<()> {
    let workspace = canonical_workspace(Path::new("."))?;
    let config = Config::load(paths)?;
    // See the precedence note on the doc comment above: real engine id,
    // then shortcut id, then literal command.
    let (tag, engine, profile, command): (String, Option<String>, Option<String>, Vec<OsString>) =
        match args.rest.as_slice() {
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
        .find(|r| r.workspace == workspace && r.tag == tag)
    {
        // `a -` attaches only to something that can actually be attached
        // to: a live worker in a non-terminal phase. Everything else falls
        // through to cmd_start, which owns the single claim decision
        // (`reap_verdict`, applied by `start_session`) -- so this is not a
        // second copy of the ownership rule that could drift from it.
        //
        // That means a *broken* holder (non-terminal phase, dead worker,
        // nothing left running) no longer needs an explicit `a kill` or
        // `a prune` first: start reclaims the pair, archives the corpse and
        // creates the session, which is what `a -` promised all along. A
        // holder whose worker or workload is still alive is still refused
        // there, so `a -` can never create a second session for a pair that
        // something is still using.
        if existing.worker_phase_active() && existing.worker_alive() {
            return attach(paths, &existing, None, false, false);
        }
    }
    cmd_start(
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
            startup_timeout_ms: DEFAULT_STARTUP_TIMEOUT_MS,
            no_skip_permissions: false,
            fresh: false,
            command,
        },
        false,
    )
}

fn cmd_quick_attach(paths: &Paths, args: QuickAttachArgs) -> Result<()> {
    let record = resolve_quick_index(paths, args.workspace_index, args.session.as_deref())?;
    attach(paths, &record, None, false, false)
}

/// Total wall-clock budget one `a prune` run may spend waiting for workers
/// that its own record says are on their way out. Shared across every
/// record in the run so a registry full of dying sessions cannot make prune
/// hang: `a kill` only returns once the workload's containment domain is
/// empty, so the worker it leaves behind is milliseconds from exiting, not
/// seconds -- this budget is sized for a saturated box, and burning all of
/// it can only produce the pre-existing "retained" answer, never a wrong
/// removal.
const PRUNE_TERMINATION_BUDGET: Duration = Duration::from_secs(5);
const PRUNE_TERMINATION_POLL: Duration = Duration::from_millis(25);

struct PruneOutcome {
    removed: Vec<Uuid>,
    removed_without_containment_proof: Vec<Uuid>,
    retained_count: usize,
}

enum ReapResult {
    Removed {
        containment_proven: bool,
    },
    Retained,
    /// The record disappeared between the registry scan and the lock --
    /// another `a prune`/`a kill`/`a forget` got there first. Neither
    /// removed by us nor still present to retain.
    Vanished,
}

/// Wait, within the run's shared budget, for a worker whose own record says
/// it is terminating. Returns the record as it stands afterwards: the worker
/// finishes its lifecycle while we wait (writing its exit, its terminal
/// phase and its containment proof), so the stale in-memory copy must not be
/// the one the reap decision is made from.
fn settle_terminating_record(
    paths: &Paths,
    record: SessionRecord,
    deadline: Instant,
) -> SessionRecord {
    if !record.worker_alive() || !record.worker_is_terminating() {
        return record;
    }
    let mut current = record;
    while Instant::now() < deadline {
        thread::sleep(PRUNE_TERMINATION_POLL);
        match read_session_record(paths, current.id) {
            Ok(fresh) => current = fresh,
            // Vanished or unreadable mid-flight: hand back what we have and
            // let the locked re-read below decide.
            Err(_) => return current,
        }
        if !current.worker_alive() {
            break;
        }
    }
    current
}

/// Remove one record's durable state, re-deciding under the registry lock.
///
/// The scan-time verdict is advisory: `start_session` holds this same lock
/// across the whole spawn, so a record that looked like a dead `Starting`
/// stub during the scan can be a fully live session by the time the lock is
/// ours. Re-read and re-check before destroying anything, and fence a
/// pre-PID worker the same way `a forget` does, so a worker spawned but not
/// yet registered cannot come up on top of a removed record.
fn reap_session_state(paths: &Paths, id: Uuid) -> Result<ReapResult> {
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    let current = match read_session_record(paths, id) {
        Ok(record) => record,
        Err(_) if !paths.record(id).exists() => return Ok(ReapResult::Vanished),
        Err(error) => return Err(error).with_context(|| format!("re-read session {id}")),
    };
    let Some(verdict) = reap_verdict(&current) else {
        return Ok(ReapResult::Retained);
    };
    let _startup_absence_lock = if current.worker_phase_active() && current.worker_pid.is_none() {
        let lock_path = paths.worker_lock(current.id);
        match FileLock::exclusive(&lock_path, true) {
            Ok(lock) => Some(lock),
            // Held: a worker exists for this record even though it has not
            // registered a pid yet. Not ours to remove.
            Err(_) => return Ok(ReapResult::Retained),
        }
    } else {
        None
    };
    fs::remove_dir_all(paths.state_session(id))
        .with_context(|| format!("remove stale session {id} durable state"))?;
    let _ = fs::remove_dir_all(paths.runtime_session(id));
    Ok(ReapResult::Removed {
        containment_proven: verdict == ContainmentReap::Proven,
    })
}

fn prune_dead_sessions(paths: &Paths) -> Result<PruneOutcome> {
    reap_sweep(paths, true)
}

/// The opportunistic sweep the default list runs before rendering: the same
/// verdict and locked-removal machinery as `a prune`, minus the wait for an
/// in-flight worker teardown. A worker that is still alive is retained here
/// and its own teardown (or a later sweep) decides the outcome, so nothing
/// a later `a prune` would have kept can be removed early.
fn sweep_prunable_corpses(paths: &Paths) -> Result<PruneOutcome> {
    reap_sweep(paths, false)
}

fn reap_sweep(paths: &Paths, wait_for_terminating: bool) -> Result<PruneOutcome> {
    let deadline = Instant::now() + PRUNE_TERMINATION_BUDGET;
    let mut outcome = PruneOutcome {
        removed: Vec::new(),
        removed_without_containment_proof: Vec::new(),
        retained_count: 0,
    };
    for record in list_records(paths)? {
        let record = if wait_for_terminating {
            settle_terminating_record(paths, record, deadline)
        } else {
            record
        };
        if reap_verdict(&record).is_none() {
            outcome.retained_count += 1;
            continue;
        }
        match reap_session_state(paths, record.id)? {
            ReapResult::Removed { containment_proven } => {
                outcome.removed.push(record.id);
                if !containment_proven {
                    outcome.removed_without_containment_proof.push(record.id);
                }
            }
            ReapResult::Retained => outcome.retained_count += 1,
            ReapResult::Vanished => {}
        }
    }
    Ok(outcome)
}

fn cmd_prune(paths: &Paths, json_output: bool) -> Result<()> {
    let outcome = prune_dead_sessions(paths)?;
    // Say plainly which reaps rested on "nothing left to hold on to" rather
    // than on a worker's own proof that its containment domain was empty --
    // the same distinction `a forget --force` reports, minus its scarier
    // wording, which was never accurate for a record whose leader is also
    // provably gone.
    for id in &outcome.removed_without_containment_proof {
        eprintln!(
            "a: removed broken session {id} without a containment proof; its worker died without recording one and nothing addressable remained"
        );
    }
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "removed": outcome.removed,
                "removed_without_containment_proof": outcome.removed_without_containment_proof,
                "retained_count": outcome.retained_count,
            }))?
        );
    } else if outcome.removed.is_empty() {
        println!("no dead sessions to prune");
    } else {
        for id in &outcome.removed {
            println!("removed {id}");
        }
        println!("removed {} session(s)", outcome.removed.len());
    }
    Ok(())
}

fn cmd_forget(paths: &Paths, args: ForgetArgs, json_output: bool) -> Result<()> {
    // Only the CLI's target spellings (quick index, tag, `workspace:tag`) and
    // its presentation live here. The destructive body -- force gate,
    // live-worker refusal, pre-PID fence, both removals, and the survival
    // warning -- is `api::forget_session`, shared with the Python binding so
    // the two cannot diverge (issue #11). Re-resolving by id there is cheap
    // and keeps the record re-read under the registry lock where it belongs.
    let selected = resolve(paths, &args.target)?;
    let value = aplexer::api::forget_session(paths, &selected.id.to_string(), args.force)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!("forgotten {}", selected.id);
    }
    Ok(())
}

/// Shared by the bare `a <N>` shortcut and by `resolve()` (so `a attach 1`,
/// `a status 1`, `a kill 1`, etc. all understand the same numbers `a list`
/// prints, not just the no-subcommand form). Exited sessions are skipped
/// (session_is_listed), so the numbers track the default list's rows rather
/// than the full registry -- a corpse found via `a list --all` is addressed
/// by tag or UUID prefix, not by its --all index.
fn resolve_quick_index(
    paths: &Paths,
    workspace_index: usize,
    session: Option<&str>,
) -> Result<SessionRecord> {
    let mut records = list_records(paths)?;
    let now = now_ms();
    records.retain(|record| session_is_listed(record, now));
    let groups = group_by_workspace(records, load_list_sort(paths));
    if groups.is_empty() {
        bail!("no sessions found (see `a start`)");
    }
    if workspace_index < 1 || workspace_index > groups.len() {
        bail!(
            "workspace index {workspace_index} out of range: {} workspace(s) found (see `a list`)",
            groups.len()
        );
    }
    let (workspace, sessions) = &groups[workspace_index - 1];
    if sessions.is_empty() {
        bail!("workspace {} has no sessions", workspace.display());
    }
    match session {
        None => Ok(sessions[0].clone()),
        Some(selector) if !selector.is_empty() && selector.bytes().all(|b| b.is_ascii_digit()) => {
            let index: usize = selector.parse().unwrap_or(0);
            if index < 1 || index > sessions.len() {
                bail!(
                    "session index {index} out of range: workspace {} has {} session(s)",
                    workspace.display(),
                    sessions.len()
                );
            }
            Ok(sessions[index - 1].clone())
        }
        Some(tag) => sessions
            .iter()
            .find(|r| r.tag == tag)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "no session tagged {tag:?} in workspace {}",
                    workspace.display()
                )
            }),
    }
}

fn cmd_status(paths: &Paths, target: TargetArgs, json_output: bool) -> Result<()> {
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
fn cmd_status_tty(
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

fn cmd_send(paths: &Paths, mut args: SendArgs, json_output: bool) -> Result<()> {
    // `a send --workspace W --tag T "text"` parses "text" into the flattened
    // TargetArgs selector positional (clap fills positionals in declaration
    // order), which then fails to resolve as a session -- or worse, silently
    // matches one. When the target is already fully named by flags, a lone
    // positional can only have been meant as the text.
    if args.text.is_none()
        && !args.stdin
        && args.target.selector.is_some()
        && (args.target.workspace.is_some() || args.target.tag.is_some())
    {
        args.text = args.target.selector.take();
    }
    let record = resolve(paths, &args.target)?;
    check_attachable(&record)?;
    let mut data = if args.stdin {
        let mut v = Vec::new();
        io::stdin().read_to_end(&mut v)?;
        v
    } else {
        args.text.unwrap_or_default().into_bytes()
    };
    if args.hex {
        data = parse_hex(&data)?;
    }
    if args.enter {
        data.push(b'\n');
    }
    if data.is_empty() {
        bail!("no bytes to send");
    }
    let mut sent = 0usize;
    for chunk in data.chunks(MAX_FRAME_BYTES) {
        rpc_send(&record, chunk)?;
        sent += chunk.len();
    }
    if json_output {
        println!("{}", json!({"id":record.id,"bytes":sent}));
    }
    Ok(())
}

fn base64_standard(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(data.len().div_ceil(3).saturating_mul(4));
    for chunk in data.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn capture_json_value(record: &SessionRecord, data: &[u8]) -> Value {
    let mut value = json!({
        "id": record.id,
        "bytes": data.len(),
        "encoding": "base64",
        "data": base64_standard(data),
    });
    // Preserve the old ergonomic field for text consumers, but only when it
    // is exact. `from_utf8_lossy` corrupted arbitrary PTY bytes while still
    // presenting the replacement-filled string as if it were authoritative.
    if let Ok(text) = std::str::from_utf8(data) {
        value["utf8"] = json!(text);
    }
    value
}

fn cmd_capture(paths: &Paths, args: CaptureArgs, json_output: bool) -> Result<()> {
    let record = resolve(paths, &args.target)?;
    let data = if args.screen {
        match rpc_capture_screen(&record, args.plain) {
            Ok(data) => data,
            // Dead-session fallback (design doc section 5.5/8): screen.txt
            // is the plain-text screen as it looked the moment the worker
            // exited, written once by OutputHub::finish. Unlike the raw
            // history fallback below, there is no paintable-form fallback
            // for a dead session -- the live grid died with the worker, and
            // only the plain text was preserved -- so --screen without
            // --plain against a dead session still surfaces the "worker
            // unavailable" error rather than silently downgrading to text.
            Err(_) if args.plain => match fs::read(paths.screen_txt(record.id)) {
                Ok(bytes) => bytes,
                Err(read_error) => {
                    check_attachable(&record)?;
                    return Err(read_error)
                        .context("worker unavailable and persisted screen.txt cannot be read");
                }
            },
            Err(error) => {
                check_attachable(&record)?;
                return Err(error).context("worker unavailable");
            }
        }
    } else {
        match rpc_capture(&record, args.bytes) {
            Ok(data) => data,
            // Persisted history is authoritative post-mortem data only once
            // the record is terminal or the worker process is known gone. A
            // live process returning an RPC error may merely be wedged or
            // temporarily unreachable; silently returning an older file in
            // that case makes stale output look current and hides the actual
            // operational failure.
            Err(_)
                if matches!(record.phase, Phase::Exited | Phase::Failed)
                    || !record.worker_alive() =>
            {
                match read_persisted_history_tail(&record.history_path, args.bytes) {
                    Ok(bytes) => bytes,
                    Err(read_error) => {
                        check_attachable(&record)?;
                        return Err(read_error)
                            .context("worker unavailable and persisted history cannot be read");
                    }
                }
            }
            Err(error) => {
                return Err(error).context(
                    "capture RPC failed while the worker process is still alive; refusing to return potentially stale persisted history",
                );
            }
        }
    };
    if let Some(path) = args.output {
        fs::write(&path, &data).with_context(|| format!("write {}", path.display()))?;
    } else if json_output {
        println!("{}", capture_json_value(&record, &data));
    } else {
        io::stdout().write_all(&data)?;
    }
    Ok(())
}

/// Deletes a session's full on-disk state: the state dir holding
/// `session.json` (the record itself), plus a best-effort cleanup of its
/// runtime dir (control socket, worker lock -- already gone or about to be,
/// in every caller). Takes the registry lock the same way `cmd_start`'s
/// superseding logic does, to avoid racing a concurrent `a start` that
/// might be reclaiming the same workspace+tag at the same moment. Shared by
/// every `a kill` path that actually retires a session's record, so
/// "removed" means the same thing everywhere instead of each call site
/// growing its own slightly-different deletion routine.
fn remove_session_state(paths: &Paths, id: Uuid) -> Result<()> {
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    fs::remove_dir_all(paths.state_session(id))?;
    let _ = fs::remove_dir_all(paths.runtime_session(id));
    Ok(())
}

/// How long `cmd_kill` waits, after an accepted kill RPC, for the worker to
/// remove the killed session's durable record itself. Normal finalization
/// lands within milliseconds (bounded above by the worker's attach-drain
/// window), so this is a settling pause, not a retry campaign; the deadline
/// only bounds the pathological cases, which are reported, never looped on.
const KILL_RECORD_REMOVAL_WAIT: Duration = Duration::from_secs(5);

/// Outcome of waiting for a killed session's record to disappear. The
/// worker that accepted the kill RPC removes the record during
/// finalization, but only when finalization ran clean and proved the
/// containment domain empty -- so `Kept` (worker exited, record stayed)
/// means the worker had something to say about this exit, and `Pending`
/// (worker still alive at the deadline) means the removal is still in
/// flight or the worker is holding the evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillRecordOutcome {
    Removed,
    Kept,
    Pending,
}

/// After an accepted kill RPC, watch the record directory until the worker
/// deletes it (or until [`KILL_RECORD_REMOVAL_WAIT`] runs out). Only the
/// worker may remove a record whose worker process is still finishing --
/// deleting it client-side would race the worker's own final record write
/// into a persist-error retry loop -- so this observes instead of acting.
fn wait_for_kill_record_removal(paths: &Paths, id: Uuid) -> KillRecordOutcome {
    let deadline = Instant::now() + KILL_RECORD_REMOVAL_WAIT;
    // Polled at 5 ms, not 25 ms: the worker's fast-path finalization for an
    // accepted kill (benchmark PLAN P0.2) removes the record in tens of
    // milliseconds, so a 25 ms quantum here is a large fraction of the whole
    // `a kill` latency. Short-lived and infrequent -- one wait per kill.
    while Instant::now() < deadline {
        if !paths.record(id).exists() {
            return KillRecordOutcome::Removed;
        }
        thread::sleep(Duration::from_millis(5));
    }
    match read_record(&paths.record(id)) {
        Ok(record) if record.worker_alive() => KillRecordOutcome::Pending,
        _ => KillRecordOutcome::Kept,
    }
}

/// A worker pid may still exist even though its control socket is gone.
/// Only this one rare case counts as "force-cleanable": a live, reachable
/// worker can also fail an RPC, but then it must not be signalled directly.
/// ESRCH is success because the process may exit between checks.
fn force_kill_stale_worker(record: &SessionRecord) -> Result<()> {
    signal_recorded_worker(record, libc::SIGKILL).context("force-kill unreachable worker")
}

fn cmd_kill(paths: &Paths, args: KillArgs, json_output: bool) -> Result<()> {
    let record = resolve(paths, &args.target)?;
    let signal = parse_signal(&args.signal)?;
    kill_grace_duration(args.grace_ms)?;
    let rpc = rpc_simple(
        &record,
        Operation::Kill {
            signal,
            grace_ms: args.grace_ms,
        },
        None,
    );
    if let Err(error) = rpc {
        let worker_alive = record.worker_alive();
        // A missing socket file, or a leftover socket with no listener
        // (SIGKILL leaves the file; connect then fails with
        // ConnectionRefused), proves an "alive" pid is unreachable. A
        // mere RPC timeout/reset can be transient, so those still return.
        let socket_missing = worker_alive && !record.socket_path.exists();
        let stale_socket = error.chain().any(|cause| {
            cause
                .downcast_ref::<io::Error>()
                .is_some_and(|cause| cause.kind() == io::ErrorKind::ConnectionRefused)
        });
        if worker_alive && !socket_missing && !stale_socket {
            return Err(error);
        }
        if socket_missing || stale_socket {
            preflight_broken_containment_recovery(&record)?;
            force_kill_stale_worker(&record)?;
            if record.containment_proven_empty() {
                remove_session_state(paths, record.id)
                    .with_context(|| format!("remove stale session {}", record.id))?;
                eprintln!(
                    "a: removed session {} after stopping unreachable worker pid {}",
                    record.id,
                    record.worker_pid.unwrap_or(0),
                );
                if json_output {
                    println!("{}", json!({"id":record.id,"signal":signal}));
                }
                return Ok(());
            }
            recover_broken_containment(&record, signal, args.grace_ms)?;
            mark_broken_workload_killed(paths, &record)?;
            eprintln!(
                "a: killed session {} (worker pid {} was unreachable; containment cleanup confirmed)",
                record.id,
                record.worker_pid.unwrap_or(0),
            );
            if json_output {
                println!("{}", json!({"id":record.id,"signal":signal}));
            }
            return Ok(());
        }
        if !record.worker_finished() {
            recover_broken_containment(&record, signal, args.grace_ms)?;
            mark_broken_workload_killed(paths, &record)?;
            // That finalization was client-side and deliberately kept the
            // evidence for a broken workload; the worker is already gone,
            // so there is no worker-side removal to wait for below.
            if json_output {
                println!(
                    "{}",
                    json!({"id":record.id,"signal":signal,"record_removed":false})
                );
            }
            return Ok(());
        }
        if !record.containment_proven_empty() {
            recover_broken_containment(&record, signal, args.grace_ms)?;
        }
        remove_session_state(paths, record.id)
            .with_context(|| format!("remove finished session {}", record.id))?;
        eprintln!("a: removed {} session {}", record.phase.name(), record.id);
        if json_output {
            println!(
                "{}",
                json!({"id":record.id,"signal":signal,"record_removed":true})
            );
        }
        return Ok(());
    }
    // The RPC was accepted, so the worker removes the record itself during
    // finalization. Give it a moment so `a kill` returns with the session
    // already gone from `a list` (a client that kills-then-lists must never
    // observe the exited corpse the old behavior left behind), and say so
    // plainly on the two outcomes where the record is still there.
    let removed = wait_for_kill_record_removal(paths, record.id);
    match removed {
        KillRecordOutcome::Removed => {}
        KillRecordOutcome::Kept => eprintln!(
            "a: killed session {}, but its worker kept the record (a finalize failure worth inspecting: `a status {}`)",
            record.id, record.id
        ),
        KillRecordOutcome::Pending => eprintln!(
            "a: killed session {}; its worker is still finalizing, the record disappears on its own unless the worker failed",
            record.id
        ),
    }
    if json_output {
        println!(
            "{}",
            json!({
                "id": record.id,
                "signal": signal,
                "record_removed": removed == KillRecordOutcome::Removed,
            })
        );
    }
    Ok(())
}

fn preflight_broken_containment_recovery(record: &SessionRecord) -> Result<()> {
    if record.containment_proven_empty() {
        return Ok(());
    }
    let Some(locator) = record.containment_cgroup.as_deref() else {
        bail!(
            "session {} has no authoritative containment locator; refusing to stop its worker or remove runtime evidence",
            record.id
        );
    };
    validate_recorded_cgroup_locator(
        record.id,
        locator,
        record.containment_cgroup_identity.as_ref(),
    )
    .context("validate recorded cgroup before stopping unreachable worker")
}

/// Record that the client killed an orphaned workload after its worker died.
fn mark_broken_workload_killed(paths: &Paths, record: &SessionRecord) -> Result<()> {
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    let mut current = read_record(&paths.record(record.id)).unwrap_or_else(|_| record.clone());
    current.phase = Phase::Failed;
    current.containment_empty = Some(true);
    current.error =
        Some("worker died without recording workload exit; workload killed by `a kill`".into());
    current.updated_at_ms = now_ms();
    atomic_write_json(&paths.record(record.id), &current)?;
    let _ = fs::remove_dir_all(paths.runtime_session(record.id));
    Ok(())
}

/// Recover a session whose worker can no longer perform containment cleanup.
/// A leader PID or process group is intentionally insufficient: a workload
/// may daemonize through `setsid`, and after the subreaper worker dies there
/// is no complete process-tree root left to inspect. Resource-limited
/// sessions retain an authoritative cgroup locator; every other broken
/// session is preserved for manual investigation rather than reporting a
/// false cleanup success.
///
/// That preservation is `a kill`'s rule and is unchanged. It is NOT a
/// promise that the record survives forever: once the workload leader is
/// also gone, `a prune` reaps such a record on the grounds that no
/// programmatic handle to a survivor remains (see
/// `aplexer::containment_reap_verdict`, and
/// `tests/prune_dead_records.rs::prune_reaps_a_record_whose_setsid_descendant_escaped`,
/// which pins the case where an escaped `setsid` descendant outlives the
/// reap). `a kill` never does that: it still refuses, and still preserves
/// both directories, because unlike prune it would be claiming a cleanup.
fn recover_broken_containment(record: &SessionRecord, signal: i32, grace_ms: u64) -> Result<()> {
    let grace = kill_grace_duration(grace_ms)?;
    if record.containment_proven_empty() {
        return Ok(());
    }
    let Some(locator) = record.containment_cgroup.as_deref() else {
        bail!(
            "session {} has no authoritative containment locator; refusing to claim cleanup or remove its runtime evidence",
            record.id
        );
    };
    cleanup_recorded_cgroup(
        record.id,
        locator,
        record.containment_cgroup_identity.as_ref(),
        signal,
        grace,
    )
    .context("recover recorded cgroup containment")
}

fn cmd_rename(paths: &Paths, args: RenameArgs, json_output: bool) -> Result<()> {
    let old = resolve_record(paths, Some(&args.selector), None, None)?;
    let workspace = canonical_workspace(args.workspace.as_deref().unwrap_or(&old.workspace))?;
    let tag = args.tag.unwrap_or_else(|| old.tag.clone());
    validate_tag(&tag)?;
    let result = rpc_simple(&old, Operation::Rename { workspace, tag }, None)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        let record: SessionRecord = serde_json::from_value(result)?;
        println!("{}", record.selector());
    }
    Ok(())
}

fn cmd_engines(paths: &Paths, json_output: bool) -> Result<()> {
    let values = aplexer::api::engines_json(paths)?;
    let values = values.as_array().cloned().unwrap_or_default();
    if json_output {
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else {
        for v in values {
            println!(
                "{:<16} {:<9} {}",
                v["name"].as_str().unwrap(),
                if v["available"].as_bool().unwrap() {
                    "available"
                } else {
                    "missing"
                },
                v["command"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|x| shell_quote(x.as_str().unwrap()))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }
    Ok(())
}

fn cmd_profiles(paths: &Paths, json_output: bool) -> Result<()> {
    let profiles = aplexer::api::profiles_json(paths)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&profiles)?);
    } else if profiles.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        println!("no configured profiles");
    } else {
        let config_profiles: BTreeMap<String, aplexer::ProfileConfig> =
            serde_json::from_value(profiles)?;
        for (name, p) in config_profiles {
            println!(
                "{:<20} engine={}",
                name,
                p.engine.as_deref().unwrap_or("(default)")
            );
        }
    }
    Ok(())
}

