use super::*;

#[derive(Debug)]
pub(crate) struct CgroupLimitProbe {
    pub(crate) cgroup_v2: bool,
    pub(crate) controllers: Vec<String>,
    pub(crate) delegated_scope: bool,
    pub(crate) detail: String,
}

pub(crate) fn probe_cgroup_limits() -> CgroupLimitProbe {
    if let Err(error) = current_cgroup_identity() {
        return CgroupLimitProbe {
            cgroup_v2: false,
            controllers: Vec::new(),
            delegated_scope: false,
            detail: format!("cgroup v2 unavailable: {error:#}"),
        };
    }

    let controllers_path = Path::new("/sys/fs/cgroup/cgroup.controllers");
    let controllers: Vec<String> = match fs::read_to_string(controllers_path) {
        Ok(value) => value.split_whitespace().map(str::to_string).collect(),
        Err(error) => {
            return CgroupLimitProbe {
                cgroup_v2: true,
                controllers: Vec::new(),
                delegated_scope: false,
                detail: format!("cannot read {}: {error}", controllers_path.display()),
            };
        }
    };
    let required_controllers = ["cpu", "memory", "pids"];
    let missing: Vec<&str> = required_controllers
        .into_iter()
        .filter(|required| !controllers.iter().any(|found| found == required))
        .collect();
    if !missing.is_empty() {
        return CgroupLimitProbe {
            cgroup_v2: true,
            controllers,
            delegated_scope: false,
            detail: format!(
                "cgroup v2 is mounted but required controller(s) are absent: {}",
                missing.join(", ")
            ),
        };
    }

    // Exercise the exact launch implementation with a short-lived placeholder
    // scope: trusted systemd-run/systemctl/sleep discovery, the systemd --user
    // manager, Delegate=yes, all three supported controllers, and write-open
    // access to cgroup.procs. The scope contains only the probe's `sleep`
    // process and is cleaned immediately; no existing cgroup or workload is
    // modified.
    let probe_limits = Limits {
        memory_bytes: Some(64 * 1024 * 1024),
        pids: Some(16),
        cpu_quota_us: Some(10_000),
        cpu_period_us: Some(100_000),
    };
    let probe_result = Cgroup::create(Uuid::new_v4(), &probe_limits, || {});
    match probe_result {
        Ok(Some(cgroup)) => {
            let validation = (|| -> Result<()> {
                let _procs = cgroup.open_procs()?;
                for controller_file in ["memory.max", "pids.max", "cpu.max"] {
                    let path = cgroup.locator().join(controller_file);
                    if !path.is_file() {
                        bail!("delegated scope is missing {}", path.display());
                    }
                }
                Ok(())
            })();
            cgroup.cleanup();
            match validation {
                Ok(()) => CgroupLimitProbe {
                    cgroup_v2: true,
                    controllers,
                    delegated_scope: true,
                    detail: "verified a temporary delegated systemd --user scope with memory, pids, and cpu controls".into(),
                },
                Err(error) => CgroupLimitProbe {
                    cgroup_v2: true,
                    controllers,
                    delegated_scope: false,
                    detail: format!("delegated scope validation failed: {error:#}"),
                },
            }
        }
        Ok(None) => CgroupLimitProbe {
            cgroup_v2: true,
            controllers,
            delegated_scope: false,
            detail: "limit probe unexpectedly created no cgroup".into(),
        },
        Err(error) => CgroupLimitProbe {
            cgroup_v2: true,
            controllers,
            delegated_scope: false,
            detail: format!("delegated systemd --user scope probe failed: {error:#}"),
        },
    }
}

pub(crate) fn cgroup_limits_check(probe: CgroupLimitProbe) -> Value {
    let required_controllers = ["cpu", "memory", "pids"];
    let controllers_ok = required_controllers
        .iter()
        .all(|required| probe.controllers.iter().any(|found| found == required));
    let available = probe.cgroup_v2 && controllers_ok && probe.delegated_scope;
    let detail = if available {
        probe.detail.clone()
    } else {
        format!(
            "{}; resource limits unavailable, but unlimited sessions still work",
            probe.detail
        )
    };
    json!({
        "name": "cgroup_limits",
        "ok": available,
        "severity": if available { "ok" } else { "warning" },
        "required": false,
        "available": available,
        "detail": detail,
        "prerequisites": {
            "cgroup_v2": probe.cgroup_v2,
            "controllers": {
                "ok": controllers_ok,
                "required": required_controllers,
                "available": probe.controllers,
            },
            "delegated_systemd_user_scope": {
                "ok": probe.delegated_scope,
                "detail": probe.detail,
                "method": "temporary_scope_via_launch_path",
                "verifies": [
                    "trusted_systemd_run_systemctl_sleep",
                    "systemd_user_manager",
                    "delegate_yes",
                    "writable_cgroup_procs",
                ],
            },
        },
    })
}

pub(crate) fn doctor_checks_ok(checks: &[Value]) -> bool {
    checks
        .iter()
        .all(|check| check["ok"].as_bool().unwrap_or(false) || check["severity"] == "warning")
}

/// The `launch_placement` doctor check (issue #1). Two questions in one:
/// (1) which service manager owns the cgroup this process is running in --
/// the placement every `a start` launched from this context hands its
/// worker, since setsid() changes session, not cgroup -- and (2) how many
/// active recorded sessions sit in the per-user manager's exit.target
/// failure domain, from the `worker_cgroup` evidence the worker now records
/// at launch. Warning-severity by design: the issue asks aplexer to warn
/// clearly, and a vulnerable placement has actionable workarounds (launch
/// context, or the opt-in system scope), so it must not fail the host.
pub(crate) fn launch_placement_check(paths: &Paths) -> Value {
    let own_cgroup = aplexer::placement::read_process_cgroup(std::process::id());
    let own_placement = own_cgroup
        .as_deref()
        .map(aplexer::placement::classify_cgroup_path);
    let vulnerable = own_placement
        .map(|placement| placement.vulnerable_to_user_manager_exit())
        .unwrap_or(false);
    let mut vulnerable_sessions: Vec<Value> = Vec::new();
    if let Ok(records) = list_records(paths) {
        for record in records {
            if !record.worker_phase_active() {
                continue;
            }
            let session_vulnerable = record
                .worker_cgroup
                .as_deref()
                .map(|cgroup| {
                    aplexer::placement::classify_cgroup_path(cgroup)
                        .vulnerable_to_user_manager_exit()
                })
                .unwrap_or(false);
            if session_vulnerable {
                vulnerable_sessions.push(json!({
                    "id": record.id.to_string(),
                    "selector": record.selector(),
                    "worker_cgroup": record.worker_cgroup,
                }));
            }
        }
    }
    let placement_name = own_placement.map(|placement| placement.name());
    let advice = own_placement.and_then(|placement| placement.advice());
    let mut detail = format!(
        "aplexer commands launched here run in cgroup {} ({})",
        own_cgroup.as_deref().unwrap_or("<unknown>"),
        placement_name.unwrap_or("unknown"),
    );
    if vulnerable {
        if let Some(advice) = advice {
            detail.push_str(&format!(
                "; sessions started here will die at `systemctl --user exit`; {advice}"
            ));
        }
    } else if let Some(advice) = advice {
        detail.push_str(&format!("; note: {advice}"));
    }
    if !vulnerable_sessions.is_empty() {
        detail.push_str(&format!(
            "; {} active session(s) recorded inside the per-user manager failure domain",
            vulnerable_sessions.len()
        ));
    }
    json!({
        "name": "launch_placement",
        "ok": !vulnerable,
        "severity": if vulnerable { "warning" } else { "ok" },
        "required": false,
        "detail": detail,
        "own_cgroup": own_cgroup,
        "own_placement": placement_name,
        "vulnerable_to_user_manager_exit": vulnerable,
        "vulnerable_sessions": vulnerable_sessions,
        "advice": advice,
        // Doctor only reads /proc and session records; it never probes the
        // system-scope backend (that would create a transient scope just by
        // asking for a checkup). The escape is documented here, and its
        // availability is proven at the opted-in `a start` that uses it.
        "escape": {
            "env": aplexer::placement::LAUNCH_SYSTEM_SCOPE_ENV,
            "value": aplexer::placement::LAUNCH_SYSTEM_SCOPE_VALUE,
            "requested": aplexer::placement::system_scope_requested(),
        },
    })
}

pub(crate) fn cmd_doctor(paths: &Paths, json_output: bool) -> Result<()> {
    let mut checks = Vec::<Value>::new();
    checks.push(json!({"name":"linux","ok":true,"detail":std::env::consts::OS}));
    checks.push(path_check("runtime_root", &paths.runtime_root));
    checks.push(path_check("state_root", &paths.state_root));
    let sample = paths.socket(Uuid::nil());
    checks.push(json!({"name":"unix_socket_path","ok":sample.as_os_str().len()<108,"detail":sample.display().to_string()}));
    checks.push(cgroup_limits_check(probe_cgroup_limits()));
    checks.push(launch_placement_check(paths));
    match Config::load(paths){Ok(config)=>checks.push(json!({"name":"config","ok":true,"detail":format!("{} engines, {} profiles",config.engines.len(),config.profiles.len())})),Err(e)=>checks.push(json!({"name":"config","ok":false,"detail":format!("{e:#}")}))}
    match list_records(paths) {
        Ok(records) => {
            let record_count = records.len();
            let mut reapable_count = 0usize;
            let broken: Vec<Value> = records
                .into_iter()
                .filter_map(|record| {
                    if !record.worker_phase_active() {
                        return None;
                    }
                    let worker_alive = record.worker_alive();
                    let rpc_error = rpc_simple(&record, Operation::Status, None)
                        .err()
                        .map(|error| format!("{error:#}"));
                    let worker_reachable = rpc_error.is_none();
                    if worker_alive && worker_reachable {
                        return None;
                    }
                    let state = derived_liveness(&record.phase, worker_alive, record.created_at_ms);
                    // A `Starting` record inside the startup window has no
                    // worker pid yet and no socket to answer an RPC: that is
                    // `a start` in flight, not wreckage. Reporting it here
                    // sent the user at `a prune` / `a kill` for a session
                    // that was about to come up on its own (issue #9).
                    if state == "starting" {
                        return None;
                    }
                    // Recovery advice has to follow the same predicate prune
                    // actually uses, or doctor sends the user at a command
                    // that hard-fails. `a kill` on a broken unlimited record
                    // exits 1 with "no authoritative containment locator",
                    // and `a forget --force`'s "workload processes may
                    // survive" warning is not what this needs -- for a
                    // record prune can reap, `a prune` is the whole answer.
                    let recovery = if reap_verdict(&record).is_some() {
                        reapable_count += 1;
                        json!({ "prune": "a prune" })
                    } else {
                        json!({
                            "kill": format!("a kill {}", record.id),
                            "forget": format!("a forget {} --force", record.id),
                        })
                    };
                    Some(json!({
                        "id": record.id,
                        "selector": record.selector(),
                        "phase": record.phase.name(),
                        "state": state,
                        "worker_alive": worker_alive,
                        "worker_reachable": worker_reachable,
                        "rpc_error": rpc_error,
                        "recovery": recovery,
                    }))
                })
                .collect();
            let detail = if broken.is_empty() {
                format!("{record_count} session record(s), none broken")
            } else if reapable_count == broken.len() {
                format!(
                    "{} broken/stale session(s), all reapable; run `a prune`",
                    broken.len()
                )
            } else if reapable_count == 0 {
                format!(
                    "{} broken/stale session(s); run `a kill SESSION`, or if safe recovery is refused, `a forget SESSION --force`",
                    broken.len()
                )
            } else {
                format!(
                    "{} broken/stale session(s); `a prune` removes {reapable_count} of them, for the rest run `a kill SESSION`, or if safe recovery is refused, `a forget SESSION --force`",
                    broken.len()
                )
            };
            checks.push(json!({
                "name": "sessions",
                "ok": broken.is_empty(),
                "detail": detail,
                "broken_sessions": broken,
            }));
        }
        Err(error) => checks.push(json!({
            "name": "sessions",
            "ok": false,
            "detail": format!("cannot inspect session records: {error:#}"),
            "broken_sessions": [],
        })),
    }
    let warnings = checks
        .iter()
        .filter(|check| check["severity"] == "warning")
        .count();
    let ok = doctor_checks_ok(&checks);
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"ok":ok,"warnings":warnings,"checks":checks}))?
        );
    } else {
        for check in &checks {
            let label = if check["severity"] == "warning" {
                "WARN"
            } else if check["ok"].as_bool().unwrap_or(false) {
                "OK"
            } else {
                "FAIL"
            };
            println!(
                "{:<5} {:<20} {}",
                label,
                check["name"].as_str().unwrap(),
                check["detail"].as_str().unwrap_or("")
            );
        }
    }
    if !ok {
        bail!("one or more doctor checks failed");
    }
    Ok(())
}

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
