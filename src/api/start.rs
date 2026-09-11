//! The one start path: every verb that creates a session lands here.
//!
//! One reason to exist: claim checks, tag allocation, the supersede/reclaim
//! decision, worker readiness probing, and rollback on failure are one
//! story. Splitting them per CLI verb is how two start paths drift into
//! disagreeing about who owns a workspace+tag.

use super::*;

mod acceptance;
mod claim;
mod connect;
mod launch;
mod supersede;
mod tag;

use claim::{claim_pair, resolve_launch};
pub(super) use connect::connect_startup_control;
use connect::*;
use launch::{await_worker_ready, commit_replacement, spawn_worker_process, write_initial_record};
use supersede::*;
pub use tag::pick_fresh_tag;

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub workspace: PathBuf,
    pub tag: String,
    pub engine: Option<String>,
    pub profile: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub command: Vec<String>,
    pub memory: Option<String>,
    pub pids: Option<u64>,
    pub cpu_quota_us: Option<u64>,
    pub cpu_period_us: u64,
    pub history_bytes: Option<usize>,
    pub no_skip_permissions: bool,
    pub startup_timeout_ms: u64,
    pub worker_rows: Option<u16>,
    pub worker_cols: Option<u16>,
    /// When set, spawn the worker as `python -m aplexer worker --id …`
    /// (Python bindings). Otherwise spawn the `aplexer` worker binary.
    pub python: Option<PathBuf>,
    /// Never fail because the requested `workspace+tag` is live: when that
    /// pair is held by a session `start_session` would refuse to supersede,
    /// claim the next free `<tag>-2`, `<tag>-3`, … suffix instead. This is
    /// what makes `a new` mean "another session in this workspace" (where
    /// `a here` means create-or-attach), and it is decided under the registry
    /// lock, so the caller cannot race another start into its suffix.
    pub fresh: bool,
}

/// The one public start entry point: the launch itself
/// (`start_session_launch`) plus the launch-placement advisories. This is
/// deliberately the choke point -- every start path (`a start`, `a new`,
/// `a here`'s create arm, fast-session-switch's sibling creation, the
/// Python binding) answers the same way about the fresh session's
/// placement (issue #1: warn clearly). Advisories go to stderr so JSON on
/// stdout stays machine-clean; a warning names the session's recorded
/// cgroup, the manager exit that kills it, and one actionable next step.
pub fn start_session(paths: &Paths, req: &StartRequest) -> Result<SessionRecord> {
    let record = start_session_launch(paths, req)?;
    if let Some(warning) = crate::placement::start_placement_warning(&record) {
        eprintln!("{warning}");
    }
    Ok(record)
}

fn start_session_launch(paths: &Paths, req: &StartRequest) -> Result<SessionRecord> {
    ensure_sigchld_compatible_for_child_management()?;
    let id = Uuid::new_v4();
    let (workspace, launch) = resolve_launch(paths, req)?;
    // Resolved before the registry lock so a missing worker executable
    // fails the start without holding up other commands.
    let command = worker_command(id, req.python.as_deref())?;
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    let claim = claim_pair(paths, req, &workspace)?;
    let mut startup = LaunchGuard::new(paths, id);
    let result = (|| -> Result<SessionRecord> {
        let (record, _launch_environment_guard) =
            write_initial_record(paths, id, &workspace, claim.tag.clone(), launch)?;
        spawn_worker_process(paths, req, id, command, &mut startup)?;
        await_worker_ready(paths, req, id, record, &mut startup)
    })();
    match result {
        Ok(record) => commit_replacement(paths, &mut startup, claim, record),
        Err(start_error) => match startup.rollback() {
            Ok(()) => Err(start_error),
            Err(rollback_error) => Err(anyhow!(
                "startup failed: {start_error:#}; rollback also failed: {rollback_error:#}"
            )),
        },
    }
}
