use super::*;

/// Which session a `Ctrl-b` switch chord asks for
/// (docs/fast-session-switching-design.md section 3).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SwitchTarget {
    /// `Ctrl-b Right`: next session in the current workspace.
    Next,
    /// `Ctrl-b Left`: previous session in the current workspace.
    Prev,
    /// `Ctrl-b Down`: the next workspace in `a list` order, entered at its
    /// most recently accessed session.
    NextWorkspace,
    /// `Ctrl-b Up`: the previous workspace, likewise.
    PrevWorkspace,
    /// `Ctrl-b N`: next session across all workspaces (`a list` order).
    NextGlobal,
    /// `Ctrl-b P`: previous session across all workspaces.
    PrevGlobal,
    /// `Ctrl-b l`: toggle back to whatever was attached before this one.
    Last,
    /// `Ctrl-b 1`..`9`: the Nth session
    /// (1-based) of the current workspace, no skipping -- must mean exactly
    /// what the status bar shows.
    Index(usize),
    /// `Ctrl-b n`: create a brand-new session in the attached session's
    /// workspace and switch to it. (Session navigation moved to the arrow
    /// keys, which is what freed `n` to mean "new".) The odd one out -- every
    /// other variant *selects* an existing session, this one *makes* the
    /// session it then selects -- which is why it is resolved by
    /// `create_sibling_session` in `perform_switch` rather than by
    /// `pick_switch_target`. Everything after resolution (establish, swap,
    /// `last` bookkeeping, failure containment) is the ordinary switch path.
    New,
}

/// True iff `check_attachable` would pass; used to skip dead sessions when
/// cycling with n/p/N/P (never for explicit `Index`/`Last` addressing,
/// which report the real error instead of silently hopping past it).
pub(crate) fn is_attachable(r: &SessionRecord) -> bool {
    check_attachable(r).is_ok()
}

/// Walks `group` from `current_id`'s position (or position 0 if the current
/// session isn't in this group -- e.g. it was killed underneath us) by +1
/// (`prev = false`) or -1 (`prev = true`) with wraparound, skipping
/// `current_id` itself and any candidate that fails `is_attachable`.
/// Returns `None` once every other candidate has been tried and rejected.
pub(crate) fn walk_group(
    group: &[SessionRecord],
    current_id: Uuid,
    prev: bool,
) -> Option<SessionRecord> {
    let len = group.len();
    if len == 0 {
        return None;
    }
    let start = group.iter().position(|r| r.id == current_id).unwrap_or(0);
    for step in 1..=len {
        let idx = if prev {
            (start + len - step) % len
        } else {
            (start + step) % len
        };
        let candidate = &group[idx];
        if candidate.id != current_id && is_attachable(candidate) {
            return Some(candidate.clone());
        }
    }
    None
}

/// The session a workspace is *entered* at by `Ctrl-b Down`/`Up`: the one
/// used most recently (`last_accessed_ms`, stamped whenever a client
/// attaches), which is the session a returning user means by "that
/// workspace". Ties and a group where nothing has ever been attached fall
/// back to `a list` order -- the first row, i.e. what the status bar
/// numbers `1`. Unattachable sessions are skipped, so a workspace whose
/// most recent session has since died is entered at its next-best one
/// rather than erroring; `None` means the whole group is dead, and the
/// caller moves on to the next workspace.
pub(crate) fn workspace_entry_session(group: &[SessionRecord]) -> Option<SessionRecord> {
    let mut best: Option<&SessionRecord> = None;
    for candidate in group.iter().filter(|r| is_attachable(r)) {
        let better = match best {
            // Strictly greater: on a tie the earlier (higher in `a list`)
            // row wins, so "never attached" groups enter at row 1.
            Some(current) => {
                candidate.last_accessed_ms.unwrap_or(0) > current.last_accessed_ms.unwrap_or(0)
            }
            None => true,
        };
        if better {
            best = Some(candidate);
        }
    }
    best.cloned()
}

/// Pure candidate selection over the same groups `a list` prints (see
/// `group_by_workspace`). Split from `resolve_switch_target` (the
/// paths-touching wrapper) so it is unit-testable without a filesystem.
/// Semantics are docs/fast-session-switching-design.md section 3.2:
///
/// - `Next`/`Prev`: candidates are the current session's own workspace
///   group; skips dead sessions; wraps; errors if nothing else is
///   attachable there.
/// - `NextWorkspace`/`PrevWorkspace`: candidates are whole *groups*, in that
///   same `a list` order, skipping the current one and any group with
///   nothing attachable in it; the chosen group is entered at
///   `workspace_entry_session`.
/// - `NextGlobal`/`PrevGlobal`: candidates are every group flattened in
///   `a list` workspace order (the remembered `--sort`), then list order
///   inside each group -- exactly the top-to-bottom order of `a list`.
/// - `Index(n)`: 1-based, into the current workspace group only, **no**
///   skipping of dead sessions -- the number must mean exactly what the
///   status bar shows (`workspace_summary`); an unattachable target is
///   still returned here and rejected later by `perform_switch`'s
///   `check_attachable` call, so the error names the actual session.
/// - `Last`: resolved by UUID against every group (survives renames, works
///   across workspaces).
pub(crate) fn pick_switch_target(
    groups: &[(PathBuf, Vec<SessionRecord>)],
    current_workspace: &Path,
    current_id: Uuid,
    target: SwitchTarget,
    last: Option<Uuid>,
) -> Result<SessionRecord> {
    let current_group = || -> Result<&[SessionRecord]> {
        groups
            .iter()
            .find(|(ws, _)| ws == current_workspace)
            .map(|(_, g)| g.as_slice())
            .ok_or_else(|| anyhow!("current workspace has no sessions"))
    };
    match target {
        SwitchTarget::Next | SwitchTarget::Prev => {
            let group = current_group()?;
            walk_group(group, current_id, target == SwitchTarget::Prev)
                .ok_or_else(|| anyhow!("no other running session in this workspace"))
        }
        SwitchTarget::NextWorkspace | SwitchTarget::PrevWorkspace => {
            // Workspace-level cycling, over the same top-level order `a list`
            // prints (the remembered `--sort`). The current workspace is
            // skipped, so this is always a real move; a workspace with
            // nothing attachable left in it is stepped over rather than
            // becoming an error the user has to press through.
            let len = groups.len();
            if len == 0 {
                bail!("no sessions to switch to");
            }
            let backwards = target == SwitchTarget::PrevWorkspace;
            let start = groups
                .iter()
                .position(|(ws, _)| ws == current_workspace)
                .unwrap_or(0);
            for step in 1..=len {
                let index = if backwards {
                    (start + len - step) % len
                } else {
                    (start + step) % len
                };
                let (workspace, group) = &groups[index];
                // Comparing the path (not the index) is what makes the
                // "current workspace not in the list" case -- it was just
                // killed underneath us -- consider every group, including
                // index 0, instead of silently skipping one.
                if workspace == current_workspace {
                    continue;
                }
                if let Some(entry) = workspace_entry_session(group) {
                    return Ok(entry);
                }
            }
            bail!("no other workspace has a running session")
        }
        SwitchTarget::NextGlobal | SwitchTarget::PrevGlobal => {
            let flat: Vec<SessionRecord> = groups.iter().flat_map(|(_, g)| g.clone()).collect();
            walk_group(&flat, current_id, target == SwitchTarget::PrevGlobal)
                .ok_or_else(|| anyhow!("no other running session"))
        }
        SwitchTarget::Index(n) => {
            let group = current_group()?;
            if n < 1 || n > group.len() {
                bail!(
                    "no session {n} here: this workspace has {} session(s)",
                    group.len()
                );
            }
            Ok(group[n - 1].clone())
        }
        // Not reachable through `perform_switch`, which resolves `New` by
        // *creating* the session before it ever gets here (see the variant's
        // doc comment). Spelled out rather than folded into another arm so a
        // future caller that forgets gets a named error instead of silently
        // switching somewhere arbitrary.
        SwitchTarget::New => bail!("new-session target is created, not selected"),
        SwitchTarget::Last => {
            let id = last.ok_or_else(|| anyhow!("no previous session"))?;
            groups
                .iter()
                .flat_map(|(_, g)| g.iter())
                .find(|r| r.id == id)
                .cloned()
                .ok_or_else(|| anyhow!("previous session is gone"))
        }
    }
}

pub(crate) fn resolve_switch_target(
    paths: &Paths,
    current: &SessionRecord,
    target: SwitchTarget,
    last: Option<Uuid>,
) -> Result<SessionRecord> {
    let groups = group_by_workspace(list_records(paths)?, load_list_sort(paths));
    pick_switch_target(&groups, &current.workspace, current.id, target, last)
}

/// How long `Ctrl-b c` waits for the new session's workload to come up before
/// giving up -- `a start`'s own `--startup-timeout-ms` default, because this
/// chord is `a new` with the CLI trip removed and must not be quietly less
/// patient than typing it.
pub(crate) const NEW_SESSION_STARTUP_TIMEOUT_MS: u64 = 10_000;

/// `Ctrl-b c`'s half of the chord: create another session in the attached
/// session's workspace, the way `a new` (i.e. `a start --fresh --attach`)
/// would if the user had detached to run it.
///
/// Deliberately *not* a clone of `current`: the promise is "what `a start`
/// gives me in this workspace", so engine/profile are left `None` for
/// `Config::resolve` to fill from the configured default engine (and its
/// default profile), the tag base is `DEFAULT_HUMAN_TAG` -- the same base
/// `a start`/`a new`/`a here` use -- and cwd defaults to the workspace.
/// Inheriting the attached session's engine instead would make the chord mean
/// "another one of these", which is a different (and unrequested) feature, and
/// would be surprising the moment the user is attached to a `--` command
/// session that was never meant to be spawned twice.
///
/// Tag allocation is `--fresh`'s, not a reimplementation: `start_session`
/// picks the first free `<tag>`/`<tag>-2`/`<tag>-3` … *under the registry
/// lock*, so two clients pressing `Ctrl-b c` at the same instant cannot claim
/// the same suffix, and a dead holder is still reclaimed under its own name
/// rather than skipped (tests/fresh_start.rs).
///
/// `geometry` is the host's workload-sized `(rows, cols)` (already
/// reserved-rows-adjusted, exactly what the following `establish` sends), so
/// the new worker's PTY is born at the right size and its first snapshot needs
/// no SIGWINCH repaint -- the same thing `a start --attach` does with the
/// terminal it was typed into.
///
/// Errors propagate to `perform_switch`'s caller untouched: nothing here has
/// touched the live attachment, so a bad config or an exhausted tag space is a
/// status-bar flash and the user stays exactly where they were.
pub(crate) fn create_sibling_session(
    paths: &Paths,
    current: &SessionRecord,
    geometry: Option<(u16, u16)>,
) -> Result<SessionRecord> {
    let request = aplexer::api::StartRequest {
        workspace: current.workspace.clone(),
        tag: DEFAULT_HUMAN_TAG.to_string(),
        engine: None,
        profile: None,
        cwd: None,
        env: BTreeMap::new(),
        command: Vec::new(),
        memory: None,
        pids: None,
        cpu_quota_us: None,
        cpu_period_us: 100_000,
        history_bytes: None,
        no_skip_permissions: false,
        startup_timeout_ms: NEW_SESSION_STARTUP_TIMEOUT_MS,
        worker_rows: geometry.map(|(rows, _)| rows),
        worker_cols: geometry.map(|(_, cols)| cols),
        python: None,
        // The whole point: never fail because this workspace already has a
        // `main`, take `main-2` instead.
        fresh: true,
    };
    aplexer::api::start_session(paths, &request).context("create a session in this workspace")
}
