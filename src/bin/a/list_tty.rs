use super::*;

/// The terminal rendering of `a list` -- see cmd_list's redirect contract.
pub(crate) fn cmd_list_tty(paths: &Paths, args: ListArgs) -> Result<()> {
    // The default view is self-cleaning: sweep first, so a corpse a killed
    // worker left behind is gone from the registry rather than merely
    // hidden. Same verdict and locked removal as `a prune`; best-effort,
    // because a list that cannot sweep (registry mid-write, a lost race)
    // must still list. Explicit views opt out: `--all` exists to show
    // post-mortems, and `--running` would throw the sweep's work away
    // unseen.
    if !args.running && !args.all {
        let _ = sweep_prunable_corpses(paths);
    }
    let mut records = list_records(paths)?;
    // Lineage labels: a session started from inside another session (`a
    // start` ran with its parent's APLEXER_SESSION_ID still in the
    // environment) shows where it came from -- the parent's tag while its
    // record exists, a short id once it doesn't, since the recorded lineage
    // deliberately survives a killed or forgotten parent. Computed over the
    // full registry, before the corpse filter below, so a live child of an
    // exited parent keeps the parent's tag in its `↳` label even though the
    // parent no longer has a row of its own.
    let lineage_labels: BTreeMap<Uuid, String> = {
        let by_id: BTreeMap<Uuid, &SessionRecord> =
            records.iter().map(|record| (record.id, record)).collect();
        records
            .iter()
            .filter_map(|record| {
                let parent = record.parent_session?;
                let label = match by_id.get(&parent) {
                    Some(parent_record) => parent_record.tag.clone(),
                    None => parent.to_string()[..8].to_string(),
                };
                Some((record.id, format!(" ↳ {label}")))
            })
            .collect()
    };
    let mut hidden_exited = 0usize;
    if args.running {
        records.retain(|record| record.worker_phase_active() && record.worker_alive());
    } else if !args.all {
        let before = records.len();
        let now = now_ms();
        records.retain(|record| session_is_listed(record, now));
        hidden_exited = before - records.len();
    }
    if records.is_empty() {
        if args.running {
            println!("No running sessions.");
        } else if hidden_exited > 0 {
            println!(
                "No live sessions -- {hidden_exited} exited hidden (`a list --all` shows them)."
            );
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

    let sort = load_list_sort(paths);
    let groups = group_by_workspace(records, sort);
    let home = env::var_os("HOME").map(PathBuf::from);
    let current_workspace = resolve_message_workspace(None).ok();
    let color = color_enabled();
    let now = now_ms();

    // One query-time detection walk per record (`api::record_agent`, the same
    // source every `a list --json` row's `agent` field carries), shared by the
    // engine-column width computation and every row below -- the same
    // probe-once shape the liveness map in cmd_list_plain has (PLAN P1.1).
    //
    // The walks run fan-out across cores: one session's tree walk is
    // sub-millisecond, but a registry of process-heavy sessions (an agent
    // mid-build has hundreds of descendants, and a no-agent session pays the
    // full walk) made the serial loop the dominant cost of the whole command
    // -- 83 of 95 ms on a live 17-session registry. Chunked scoped threads
    // keep it a fraction of the /proc reads it is made of, with the same
    // per-record answers as the serial order (each row's `agent` is
    // independent of every other's).
    let agents: BTreeMap<Uuid, Option<aplexer::agent_kind::AgentKind>> = {
        let records: Vec<&SessionRecord> = groups
            .iter()
            .flat_map(|(_, sessions)| sessions.iter())
            .collect();
        let worker_count = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, records.len().max(1));
        let chunk_size = records.len().div_ceil(worker_count);
        thread::scope(|scope| {
            let handles: Vec<_> = records
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .map(|record| (record.id, aplexer::api::record_agent(record)))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| {
                    handle.join().expect(
                        "agent detection worker panicked; detection is infallible by contract",
                    )
                })
                .collect()
        })
    };

    for (workspace_index, (workspace, sessions)) in groups.iter().enumerate() {
        if workspace_index > 0 {
            println!();
        }
        let states: Vec<(&'static str, bool, bool)> = sessions
            .iter()
            .map(|record| {
                let (state, _) = session_ui_state(record, now);
                (
                    state,
                    ui_state_is_active(state),
                    ui_state_needs_attention(state),
                )
            })
            .collect();
        let active = states.iter().filter(|(_, active, _)| *active).count();
        let attention = states.iter().filter(|(_, _, attention)| *attention).count();
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
            paint(color, ANSI_CYAN, "  ← here")
        } else {
            String::new()
        };
        let recency = workspace_recency_label(sort, sessions, now);
        let recency = if recency.is_empty() {
            String::new()
        } else {
            format!(" · {recency}")
        };
        println!(
            "{badge} {name}{marker}  {}",
            paint(color, ANSI_DIM, &format!("{summary}{recency}"))
        );

        // Column widths adapt to the widest tag/engine actually present, so
        // a registry of long agent tags doesn't force every row to wrap.
        let tag_width = sessions
            .iter()
            .map(|record| terminal_display_width(&record.tag))
            .max()
            .unwrap_or(3)
            .clamp(6, 20);
        let engine_width = sessions
            .iter()
            .map(|record| {
                terminal_display_width(&engine_label(
                    record,
                    agents.get(&record.id).copied().flatten(),
                ))
            })
            .max()
            .unwrap_or(6)
            .clamp(6, 28);
        let last = sessions.len().saturating_sub(1);
        for (index, record) in sessions.iter().enumerate() {
            let (state, _, attention) = states[index];
            let connector = if index == last { "└─" } else { "├─" };
            let engine = engine_label(record, agents.get(&record.id).copied().flatten());
            let tag = paint(color, ANSI_BOLD, &fit_column(&record.tag, tag_width));
            let engine = paint(color, ANSI_DIM, &fit_column(&engine, engine_width));
            let (sdot, scolor) = state_glyph(state);
            let state_text = fit_column(&format!("{sdot} {state}"), 11);
            let state_text = paint(color, scolor, &state_text);
            let timestamp = state_timestamp(record, state, now);
            let age = paint(
                color,
                ANSI_DIM,
                &format!("{:>12}", human_age_phrase(now.saturating_sub(timestamp))),
            );
            let attention_mark = if attention {
                paint(color, ANSI_YELLOW, " !")
            } else {
                String::new()
            };
            let lineage = match lineage_labels.get(&record.id) {
                Some(label) => paint(color, ANSI_DIM, label),
                None => String::new(),
            };
            println!(
                "{} {:>2}  {}  {}  {}{} {}{}",
                paint(color, ANSI_GRAY, connector),
                index + 1,
                tag,
                engine,
                state_text,
                attention_mark,
                age,
                lineage
            );
        }
    }

    println!();
    println!(
        "{}",
        paint(
            color,
            ANSI_DIM,
            "Attach: a <workspace#> [session#|tag] · Here: a here [engine] [tag] · Another: a new · Help: a help"
        )
    );
    println!(
        "{}",
        paint(
            color,
            ANSI_DIM,
            &format!(
                "Sort: {} · a list --sort name|created|accessed|activity",
                sort.as_str()
            )
        )
    );
    if hidden_exited > 0 {
        println!(
            "{}",
            paint(
                color,
                ANSI_DIM,
                &format!("{hidden_exited} exited hidden · a list --all shows them")
            )
        );
    }
    Ok(())
}
