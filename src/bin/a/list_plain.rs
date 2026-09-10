use super::*;

/// The redirected rendering of `a list` -- the pre-UX format, unchanged so
/// piped/parsed output is stable across the UX work.
pub(crate) fn cmd_list_plain(paths: &Paths, args: ListArgs) -> Result<()> {
    let mut records = list_records(paths)?;
    if args.running {
        records.retain(|r| r.worker_phase_active() && r.worker_alive());
    }
    // Liveness is probed once per record here and reused for the workspace
    // header (`running_count`/`running_summary`) and every row below.
    // Benchmark PLAN P1.1: the old code called `worker_alive()` three times
    // per record (header count + header summary which recounts + row), and
    // each probe reads /proc, the identity sidecar, and (now cached) boot_id
    // -- with 25 sessions that triple-probe was the ~7 ms table-over-json
    // gap, since `--json` probes once. The map below makes plain rendering
    // probe exactly once per record.
    let alive: BTreeMap<Uuid, bool> = records.iter().map(|r| (r.id, r.worker_alive())).collect();
    let alive_of = |r: &SessionRecord| alive.get(&r.id).copied().unwrap_or(false);
    // Group by workspace as a compact tree -- spec.md's own presentation of
    // the model (sections 2 and 22.1) is a workspace tree with tags
    // underneath, not a flat table repeating the workspace on every row.
    // `a <N>` quick-attach (see cmd_quick_attach/resolve_quick_index) numbers
    // workspaces and sessions using this exact same grouping, so the
    // `[N]`/session-index prefixes printed below are not decoration -- they
    // are the literal numbers `a <N>` and `a <N> <M>` resolve against.
    let sort = load_list_sort(paths);
    let by_workspace = group_by_workspace(records, sort);
    let home = env::var_os("HOME").map(PathBuf::from);
    let color = color_enabled();
    for (workspace_index, (workspace, group)) in by_workspace.iter().enumerate() {
        if workspace_index > 0 {
            println!();
        }
        let (running, total) = running_count(group, &alive);
        let (dot, dot_color) = workspace_glyph(running, total);
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
        let summary = paint(
            color,
            dot_color,
            &format!("{dot} {}", running_summary(group, &alive)),
        );
        println!("{badge} {name} ({summary})");
        let last = group.len().saturating_sub(1);
        for (i, r) in group.iter().enumerate() {
            let connector_raw = if i == last {
                "\u{2514}\u{2500}\u{2500}"
            } else {
                "\u{251c}\u{2500}\u{2500}"
            };
            let connector = paint(color, ANSI_GRAY, connector_raw);
            let idx = paint(color, ANSI_DIM, &format!("{:>2}", i + 1));
            let tag = paint(color, ANSI_BOLD, &format!("{:<14}", r.tag));
            let ep = match &r.profile {
                Some(p) => format!("{}/{}", r.engine, p),
                None => r.engine.clone(),
            };
            let ep = paint(color, ANSI_DIM, &format!("{:<16}", ep));
            let state = derived_liveness(&r.phase, alive_of(r), r.created_at_ms);
            let (sdot, scolor) = state_glyph(state);
            let state = paint(color, scolor, &format!("{sdot} {state}"));
            println!("{connector} {idx}  {tag} {ep} {state}");
        }
    }
    if !by_workspace.is_empty() {
        println!();
        println!(
            "{}",
            paint(color, ANSI_DIM, "Attach: a <workspace#> [session#|tag]")
        );
        println!(
            "{}",
            paint(
                color,
                ANSI_DIM,
                "e.g. a 3 2, a 3 zsp, or a 3 for its first session"
            )
        );
    }
    Ok(())
}
