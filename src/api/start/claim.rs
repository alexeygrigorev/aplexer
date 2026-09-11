//! Claiming a `workspace+tag` for a new session, and resolving the request
//! into the concrete workload the worker will spawn. Both run before
//! anything is written.

use super::*;
use crate::ResolvedLaunch;

/// The `workspace+tag` a start has claimed for its session, decided under
/// the registry lock. `fence` is the pre-PID worker fence over a superseded
/// stub (`_fence`) and must stay alive until `commit_replacement` has archived and
/// deleted that predecessor, so a worker spawned into the stub cannot come
/// up on top of state that is about to be destroyed.
pub(super) struct PairClaim {
    pub(super) tag: String,
    pub(super) superseded: Option<SessionRecord>,
    pub(super) reclaim: Option<ContainmentReap>,
    pub(super) _fence: Option<FileLock>,
}

/// Validate the request and resolve it against the config into the
/// workload the worker will spawn. Nothing here touches the registry.
pub(super) fn resolve_launch(
    paths: &Paths,
    req: &StartRequest,
) -> Result<(PathBuf, ResolvedLaunch)> {
    validate_tag(&req.tag)?;
    let workspace = canonical_workspace(&req.workspace)?;
    let limits = Limits {
        memory_bytes: req.memory.as_deref().map(parse_byte_size).transpose()?,
        pids: req.pids,
        cpu_quota_us: req.cpu_quota_us,
        cpu_period_us: req.cpu_quota_us.map(|_| req.cpu_period_us),
    };
    let config = Config::load(paths)?;
    let mut launch = config.resolve(
        req.command.clone(),
        req.engine.as_deref(),
        req.profile.as_deref(),
        &workspace,
        req.cwd.as_deref(),
        &req.env,
        &limits,
        req.history_bytes,
    )?;
    if req.command.is_empty() && !req.no_skip_permissions {
        launch
            .command
            .extend(launch.skip_permissions_argv.iter().cloned());
    }
    if !command_exists(&launch.command) {
        bail!(
            "command is not executable or was not found in PATH: {}",
            launch
                .command
                .first()
                .map(String::as_str)
                .unwrap_or("<empty>")
        );
    }
    Ok((workspace, launch))
}

/// Decide which pair this start owns and who, if anyone, it supersedes.
/// Must run with the registry lock held: the read here IS the locked read,
/// and the lock stays held through the whole spawn so no other aplexer
/// command can modify the registry until the start returns.
pub(super) fn claim_pair(paths: &Paths, req: &StartRequest, workspace: &Path) -> Result<PairClaim> {
    // Read under the registry lock taken above, and keep holding it through
    // the whole spawn: this read IS the locked read, and no other aplexer
    // command can modify the registry until this call returns.
    let registry = list_records(paths)?;
    // The pair can be held by more than one record: `a rename` takes a pair
    // from a dead holder but leaves the corpse in place for `a prune`
    // (issue #13), so "the holder" must not be whoever `read_dir` lists
    // first. A live holder always wins -- it is who the supersede check
    // below refuses to displace -- and only when every holder is reclaimable
    // does the first dead one become the predecessor this start archives.
    let holder_of = |tag: &str| {
        let mut holders = registry
            .iter()
            .filter(|r| r.workspace == workspace && r.tag == tag);
        holders
            .find(|r| crate::reap_verdict(r).is_none())
            .or_else(|| {
                registry
                    .iter()
                    .find(|r| r.workspace == workspace && r.tag == tag)
            })
    };
    let mut tag = req.tag.clone();
    let mut fence: Option<FileLock> = None;
    let mut reclaim: Option<ContainmentReap> = None;
    if req.fresh {
        // `--fresh` promises "always creates": a requested pair held by
        // something live is not an error, it is a reason to move to the next
        // free suffix. A pair that is free, or held only by a record
        // `reap_verdict` would hand over, keeps the exact requested tag --
        // the reclaim path below already owns taking those.
        let Some(chosen) = pick_fresh_tag(&registry, workspace, &req.tag) else {
            bail!(
                "no free tag: every `{0}`, `{0}-2`, `{0}-3`, … candidate in this \
                 workspace is taken or would exceed the tag length limit",
                req.tag
            );
        };
        if chosen != req.tag {
            tag = chosen;
        }
    }
    let superseded = holder_of(&tag).cloned();
    if let Some(existing) = &superseded {
        // Taking this pair means archiving and then DELETING the holder's
        // durable state -- the same destruction `a prune` performs -- so it
        // must clear the same bar, `reap_verdict`. `worker_finished()`, the
        // old test, required a terminal phase that a SIGKILLed worker never
        // gets to write, so a zombie (worker dead, `phase` stuck at
        // `running`) held its `workspace+tag` forever and `a start` could
        // only succeed if something else pruned it first.
        let Some(verdict) = reap_verdict(existing) else {
            bail!(
                "workspace+tag already belongs to session {} (state: {}); rename it or choose a different tag",
                existing.id,
                existing.observed_state()
            );
        };
        fence = fence_or_refuse(paths, existing).with_context(|| {
            format!(
                "workspace+tag already belongs to session {}; rename it or choose a different tag",
                existing.id
            )
        })?;
        reclaim = Some(verdict);
    }
    Ok(PairClaim {
        tag,
        superseded,
        reclaim,
        _fence: fence,
    })
}
