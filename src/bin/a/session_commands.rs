use super::*;

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
/// The dash can also carry the tag itself -- tmuxctl's `-suffix` idiom,
/// rewritten to `--tag` in main()'s rewrite_quick_attach_args -- which
/// pins the session name while the remaining words still pick the engine:
///
///   a -review            tag "review", default engine
///   a -review claude     tag "review", engine claude
///
/// Re-running the same shortcut reattaches to a live matching session
/// instead of erroring, like tmuxctl's own create_or_attach.
/// Default tag for a literal-command quick-launch: the command's own base
/// name, normalized to the charset validate_tag accepts. Deliberately NOT
/// "main" for every arbitrary command -- `a - htop` reusing the same tag as
/// `a -`'s plain shell would silently reattach to that shell instead of
/// ever running htop.
pub(crate) fn command_tag(word: &str) -> String {
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

pub(crate) fn cmd_quick_launch(paths: &Paths, args: QuickLaunchArgs) -> Result<()> {
    let workspace = canonical_workspace(Path::new("."))?;
    let config = Config::load(paths)?;
    // See the precedence note on the doc comment above: real engine id,
    // then shortcut id, then literal command.
    let (derived_tag, engine, profile, command): (
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
    // An explicit tag -- `a -review`, rewritten to `--tag review` in main()
    // -- names the session directly, tmuxctl's dash-suffix idiom; the words
    // after it still decide engine vs shortcut vs literal command exactly
    // as they do for bare `a -` (`a -review claude` = session "review",
    // engine claude).
    let tag = args.tag.unwrap_or(derived_tag);
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

pub(crate) fn cmd_quick_attach(paths: &Paths, args: QuickAttachArgs) -> Result<()> {
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
pub(crate) const PRUNE_TERMINATION_BUDGET: Duration = Duration::from_secs(5);
pub(crate) const PRUNE_TERMINATION_POLL: Duration = Duration::from_millis(25);

pub(crate) struct PruneOutcome {
    pub(crate) removed: Vec<Uuid>,
    pub(crate) removed_without_containment_proof: Vec<Uuid>,
    pub(crate) retained_count: usize,
}

pub(crate) enum ReapResult {
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
pub(crate) fn settle_terminating_record(
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
pub(crate) fn reap_session_state(paths: &Paths, id: Uuid) -> Result<ReapResult> {
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    let current = match read_session_record(paths, id) {
        Ok(record) => record,
        Err(_) if !paths.record(id).exists() => return Ok(ReapResult::Vanished),
        Err(error) => return Err(error).with_context(|| format!("re-read session {id}")),
    };
    let Some(verdict) = reap_verdict(&current) else {
        return Ok(ReapResult::Retained);
    };
    // Held, or unfenceable: a worker may exist for this record even though
    // it has not registered a pid yet. Not ours to remove.
    let Ok(_startup_absence_lock) = aplexer::api::fence_or_refuse(paths, &current) else {
        return Ok(ReapResult::Retained);
    };
    fs::remove_dir_all(paths.state_session(id))
        .with_context(|| format!("remove stale session {id} durable state"))?;
    let _ = fs::remove_dir_all(paths.runtime_session(id));
    Ok(ReapResult::Removed {
        containment_proven: verdict == ContainmentReap::Proven,
    })
}

pub(crate) fn prune_dead_sessions(paths: &Paths) -> Result<PruneOutcome> {
    reap_sweep(paths, true)
}

/// The opportunistic sweep the default list runs before rendering: the same
/// verdict and locked-removal machinery as `a prune`, minus the wait for an
/// in-flight worker teardown. A worker that is still alive is retained here
/// and its own teardown (or a later sweep) decides the outcome, so nothing
/// a later `a prune` would have kept can be removed early.
pub(crate) fn sweep_prunable_corpses(paths: &Paths) -> Result<PruneOutcome> {
    reap_sweep(paths, false)
}

pub(crate) fn reap_sweep(paths: &Paths, wait_for_terminating: bool) -> Result<PruneOutcome> {
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

pub(crate) fn cmd_prune(paths: &Paths, json_output: bool) -> Result<()> {
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

pub(crate) fn cmd_forget(paths: &Paths, args: ForgetArgs, json_output: bool) -> Result<()> {
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
pub(crate) fn resolve_quick_index(
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

pub(crate) fn cmd_send(paths: &Paths, mut args: SendArgs, json_output: bool) -> Result<()> {
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

/// Deletes a session's full on-disk state: the state dir holding
/// `session.json` (the record itself), plus a best-effort cleanup of its
/// runtime dir (control socket, worker lock -- already gone or about to be,
/// in every caller). Takes the registry lock the same way `cmd_start`'s
/// superseding logic does, to avoid racing a concurrent `a start` that
/// might be reclaiming the same workspace+tag at the same moment. Shared by
/// every `a kill` path that actually retires a session's record, so
/// "removed" means the same thing everywhere instead of each call site
/// growing its own slightly-different deletion routine.
pub(crate) fn cmd_rename(paths: &Paths, args: RenameArgs, json_output: bool) -> Result<()> {
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

pub(crate) fn cmd_engines(paths: &Paths, json_output: bool) -> Result<()> {
    let values = aplexer::api::engines_json(paths)?;
    let values = values.as_array().cloned().unwrap_or_default();
    if json_output {
        println!("{}", serde_json::to_string_pretty(&values)?);
    } else {
        for v in values {
            let name = v["name"]
                .as_str()
                .ok_or_else(|| anyhow!("engine entry without a name: {v}"))?;
            let available = if v["available"].as_bool().unwrap_or(false) {
                "available"
            } else {
                "missing"
            };
            let command = v["command"]
                .as_array()
                .map(|argv| {
                    argv.iter()
                        .filter_map(Value::as_str)
                        .map(shell_quote)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            println!("{name:<16} {available:<9} {command}");
        }
    }
    Ok(())
}

pub(crate) fn cmd_profiles(paths: &Paths, json_output: bool) -> Result<()> {
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
