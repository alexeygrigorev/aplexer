use super::*;

/// One session row's precomputed display facts, shared by the workspace
/// summary and the row loop.
struct RowState {
    state: &'static str,
    active: bool,
    attention: bool,
}

impl RowState {
    fn of(record: &SessionRecord, now: u64) -> Self {
        let (state, _) = session_ui_state(record, now);
        Self {
            state,
            active: ui_state_is_active(state),
            attention: ui_state_needs_attention(state),
        }
    }
}

/// Lineage labels: a session started from inside another session (`a
/// start` ran with its parent's APLEXER_SESSION_ID still in the
/// environment) shows where it came from -- the parent's tag while its
/// record exists, a short id once it doesn't, since the recorded lineage
/// deliberately survives a killed or forgotten parent. Computed over the
/// full registry, before the corpse filter the caller applies, so a live
/// child of an exited parent keeps the parent's tag in its `↳` label even
/// though the parent no longer has a row of its own.
fn lineage_labels(records: &[SessionRecord]) -> BTreeMap<Uuid, String> {
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
}

/// Applies the view's record filter in place: `--running` keeps only live
/// workers, the default view additionally hides exited sessions, and
/// `--all` keeps everything. Returns how many exited sessions the default
/// view hid (the `--all` hint in the footer).
fn filter_records_for_view(records: &mut Vec<SessionRecord>, running: bool, all: bool) -> usize {
    if running {
        records.retain(|record| record.worker_phase_active() && record.worker_alive());
        return 0;
    }
    if all {
        return 0;
    }
    let before = records.len();
    let now = now_ms();
    records.retain(|record| session_is_listed(record, now));
    before - records.len()
}

/// The default view is self-cleaning: sweep first, so a corpse a killed
/// worker left behind is gone from the registry rather than merely
/// hidden. Same verdict and locked removal as `a prune`; best-effort,
/// because a list that cannot sweep (registry mid-write, a lost race)
/// must still list. Explicit views opt out: `--all` exists to show
/// post-mortems, and `--running` would throw the sweep's work away
/// unseen.
fn sweep_unless_explicit_view(paths: &Paths, args: &ListArgs) {
    if !args.running && !args.all {
        let _ = sweep_prunable_corpses(paths);
    }
}

fn print_empty_list(running: bool, hidden_exited: usize) {
    if running {
        println!("No running sessions.");
    } else if hidden_exited > 0 {
        println!("No live sessions -- {hidden_exited} exited hidden (`a list --all` shows them).");
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
}

/// One query-time detection walk per record (`api::record_agent`, the same
/// source every `a list --json` row's `agent` field carries), shared by the
/// engine-column width computation and every row below -- the same
/// probe-once shape the liveness map in cmd_list_plain has (PLAN P1.1).
///
/// The walks run fan-out across cores: one session's tree walk is
/// sub-millisecond, but a registry of process-heavy sessions (an agent
/// mid-build has hundreds of descendants, and a no-agent session pays the
/// full walk) made the serial loop the dominant cost of the whole command
/// -- 83 of 95 ms on a live 17-session registry. Chunked scoped threads
/// keep it a fraction of the /proc reads it is made of, with the same
/// per-record answers as the serial order (each row's `agent` is
/// independent of every other's).
fn detect_row_agents(
    paths: &Paths,
    groups: &[(PathBuf, Vec<SessionRecord>)],
) -> BTreeMap<Uuid, Option<aplexer::agent_kind::DetectedAgent>> {
    let variants = Arc::new(match Config::load(paths) {
        Ok(config) => aplexer::agent_kind::profile_variants(&config),
        Err(_) => aplexer::agent_kind::ProfileVariants::new(),
    });
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
                let variants = Arc::clone(&variants);
                scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|record| {
                            (
                                record.id,
                                aplexer::api::record_detected_with(record, &variants),
                            )
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| {
                handle
                    .join()
                    .expect("agent detection worker panicked; detection is infallible by contract")
            })
            .collect()
    })
}

fn workspace_summary(states: &[RowState]) -> String {
    let active = states.iter().filter(|state| state.active).count();
    let attention = states.iter().filter(|state| state.attention).count();
    let stopped = states.len().saturating_sub(active);
    let mut summary = format!("{active} active");
    if attention > 0 {
        summary.push_str(&format!(" · {attention} needs you"));
    }
    if stopped > 0 {
        summary.push_str(&format!(" · {stopped} stopped"));
    }
    summary
}

fn print_workspace_header(
    index: usize,
    workspace: &Path,
    here: bool,
    summary: &str,
    recency: &str,
    color: bool,
) {
    let badge = paint(
        color,
        &format!("{ANSI_BOLD}{ANSI_CYAN}"),
        &format!("[{index}]"),
    );
    let home = env::var_os("HOME").map(PathBuf::from);
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
    let recency = if recency.is_empty() {
        String::new()
    } else {
        format!(" · {recency}")
    };
    println!(
        "{badge} {name}{marker}  {}",
        paint(color, ANSI_DIM, &format!("{summary}{recency}"))
    );
}

/// Column widths adapt to the widest tag/engine actually present, so a
/// registry of long agent tags doesn't force every row to wrap.
fn column_widths(
    sessions: &[SessionRecord],
    agents: &BTreeMap<Uuid, Option<aplexer::agent_kind::DetectedAgent>>,
) -> (usize, usize) {
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
                agents.get(&record.id).and_then(|d| d.as_ref()),
            ))
        })
        .max()
        .unwrap_or(6)
        .clamp(6, 28);
    (tag_width, engine_width)
}

#[allow(clippy::too_many_arguments)]
fn print_session_row(
    index: usize,
    last: bool,
    record: &SessionRecord,
    state: &RowState,
    engine: &str,
    tag_width: usize,
    engine_width: usize,
    lineage: Option<&String>,
    now: u64,
    color: bool,
) {
    let connector = if last { "└─" } else { "├─" };
    let tag = paint(color, ANSI_BOLD, &fit_column(&record.tag, tag_width));
    let engine = paint(color, ANSI_DIM, &fit_column(engine, engine_width));
    let (sdot, scolor) = state_glyph(state.state);
    let state_text = fit_column(&format!("{sdot} {}", state.state), 11);
    let state_text = paint(color, scolor, &state_text);
    let timestamp = state_timestamp(record, state.state, now);
    let age = paint(
        color,
        ANSI_DIM,
        &format!("{:>12}", human_age_phrase(now.saturating_sub(timestamp))),
    );
    let attention_mark = if state.attention {
        paint(color, ANSI_YELLOW, " !")
    } else {
        String::new()
    };
    let lineage = match lineage {
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

fn print_list_footer(sort: ListSort, hidden_exited: usize, color: bool) {
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
}

/// The terminal rendering of `a list` -- see cmd_list's redirect contract.
pub(crate) fn cmd_list_tty(paths: &Paths, args: ListArgs) -> Result<()> {
    sweep_unless_explicit_view(paths, &args);
    let mut records = list_records(paths)?;
    let lineage = lineage_labels(&records);
    let hidden_exited = filter_records_for_view(&mut records, args.running, args.all);
    if records.is_empty() {
        print_empty_list(args.running, hidden_exited);
        return Ok(());
    }

    let sort = load_list_sort(paths);
    let groups = group_by_workspace(records, sort);
    let agents = detect_row_agents(paths, &groups);
    let current_workspace = resolve_message_workspace(None).ok();
    let color = color_enabled();
    let now = now_ms();

    for (workspace_index, (workspace, sessions)) in groups.iter().enumerate() {
        if workspace_index > 0 {
            println!();
        }
        let states: Vec<RowState> = sessions
            .iter()
            .map(|record| RowState::of(record, now))
            .collect();
        let here = current_workspace.as_deref() == Some(workspace.as_path());
        let recency = workspace_recency_label(sort, sessions, now);
        print_workspace_header(
            workspace_index + 1,
            workspace,
            here,
            &workspace_summary(&states),
            &recency,
            color,
        );

        let (tag_width, engine_width) = column_widths(sessions, &agents);
        let last = sessions.len().saturating_sub(1);
        for (index, record) in sessions.iter().enumerate() {
            let engine = engine_label(record, agents.get(&record.id).and_then(|d| d.as_ref()));
            print_session_row(
                index,
                index == last,
                record,
                &states[index],
                &engine,
                tag_width,
                engine_width,
                lineage.get(&record.id),
                now,
                color,
            );
        }
    }

    print_list_footer(sort, hidden_exited, color);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The minimal record the display helpers need. The library's own
    /// `SessionRecord::fixture` is `#[cfg(test)]` on the lib, invisible to
    /// the binary's test build, so this mirrors it.
    fn record(tag: &str, parent: Option<Uuid>) -> SessionRecord {
        SessionRecord {
            parent_session: parent,
            schema_version: aplexer::SCHEMA_VERSION,
            id: Uuid::new_v4(),
            workspace: PathBuf::from("/tmp/ws"),
            tag: tag.to_string(),
            engine: "shell".to_string(),
            profile: None,
            command: Vec::new(),
            cwd: PathBuf::from("/tmp/ws"),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: 0,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase: Phase::Running,
            worker_pid: None,
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(false),
            socket_path: PathBuf::from("/nonexistent"),
            history_path: PathBuf::from("/nonexistent"),
            exit: None,
            error: None,
        }
    }

    #[test]
    fn lineage_label_is_the_parent_tag_while_its_record_exists() {
        let parent = record("main", None);
        let parent_id = parent.id;
        let mut child = record("review", Some(parent_id));
        child.id = Uuid::new_v4();
        let child_id = child.id;
        let labels = lineage_labels(&[parent, child]);
        assert_eq!(labels.get(&child_id).map(String::as_str), Some(" ↳ main"));
    }

    #[test]
    fn lineage_label_falls_back_to_a_short_id_for_a_gone_parent() {
        let gone_parent = Uuid::nil();
        let mut child = record("review", Some(gone_parent));
        child.id = Uuid::max();
        let child_id = child.id;
        let labels = lineage_labels(&[child]);
        let expected = format!(" ↳ {}", gone_parent.to_string().get(..8).unwrap());
        assert_eq!(
            labels.get(&child_id).map(String::as_str),
            Some(expected.as_str())
        );
    }

    #[test]
    fn workspace_summary_counts_states() {
        let states = vec![
            RowState {
                state: "working",
                active: true,
                attention: false,
            },
            RowState {
                state: "needs you",
                active: false,
                attention: true,
            },
            RowState {
                state: "stopped",
                active: false,
                attention: false,
            },
        ];
        assert_eq!(
            workspace_summary(&states),
            "1 active · 1 needs you · 2 stopped"
        );
        let all_active = vec![RowState {
            state: "working",
            active: true,
            attention: false,
        }];
        assert_eq!(workspace_summary(&all_active), "1 active");
    }

    #[test]
    fn column_widths_clamp_to_the_display_bounds() {
        let mut session = record("ab", None);
        session.tag = "a-very-long-agent-tag-that-exceeds-every-bound".into();
        let agents = BTreeMap::new();
        let (tag_width, engine_width) = column_widths(&[session], &agents);
        assert_eq!(tag_width, 20);
        assert_eq!(engine_width, 6);
    }
}
