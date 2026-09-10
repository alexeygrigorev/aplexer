//! The one start path: every verb that creates a session lands here.
//!
//! One reason to exist: claim checks, tag allocation, the supersede/reclaim
//! decision, worker readiness probing, and rollback on failure are one
//! story. Splitting them per CLI verb is how two start paths drift into
//! disagreeing about who owns a workspace+tag.

use super::*;

mod connect;
mod supersede;
mod tag;

pub(super) use connect::connect_startup_control;
use connect::*;
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

/// Forces the fast-workload startup interleaving that `start_session`'s
/// readiness poll can otherwise only lose by chance: block until the worker
/// has finished its job, unlinked its control socket and exited, so the poll
/// loop below can never observe a live socket to Ping.
///
/// Timing alone does not reproduce this on an idle machine -- 25+ repetitions
/// of the fast-workload test pass locally -- which is exactly how the
/// readiness-Ping gate reached a release with this ordering unhandled. The
/// non-default `startup-test-hooks` feature is the authorization boundary for
/// pinning it; default and release builds do not contain this path.
#[cfg(feature = "startup-test-hooks")]
fn await_worker_exit_before_readiness_poll(
    startup: &mut LaunchGuard<'_>,
    paths: &Paths,
    id: Uuid,
) -> Result<()> {
    if std::env::var_os("APLEXER_TEST_AWAIT_WORKER_EXIT_BEFORE_READINESS_POLL").is_none() {
        return Ok(());
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // `Child::try_wait` caches the reaped status, so the poll loop's own
        // `try_wait` still observes this exit rather than an "already reaped"
        // error.
        let exited = startup.child_mut().try_wait()?.is_some();
        if exited && !paths.socket(id).exists() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("test hook timed out waiting for worker {id} to exit before readiness poll");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Whether a worker that has already exited left durable proof that its
/// session started and ran, rather than failing during startup.
///
/// `start_session` commits readiness by Pinging the worker's live control
/// socket. A fast workload (a one-shot shell command, an agent binary that
/// rejects its config immediately) can run to completion, persist its exit,
/// unlink that socket and exit inside a single 25ms poll interval, so the
/// readiness arm never gets a socket to Ping. The durable terminal record is
/// the evidence the vanished socket can no longer provide, and it is strictly
/// stronger: it is the same record a successful Ping would have returned.
///
/// Deliberately narrow -- every other shape still fails startup:
///
/// * a non-terminal phase (`Starting`/`Running`): a worker that vanished
///   without recording that it ever ran;
/// * `Phase::Failed`: the worker's own recorded failure, reported with its
///   reason by the poll loop's `Failed` arm rather than laundered into a
///   completed session here;
/// * a terminal phase with no `exit`: nothing proves the workload ran;
/// * `containment_empty` not explicitly `Some(true)`: no durable proof that
///   the containment domain is empty.
///
/// The caller additionally requires the worker's own exit status to be zero,
/// because a crashed worker is a startup failure whatever its record claims.
///
/// `Phase::Exiting` is accepted for symmetry with the readiness arm above,
/// which treats `Running | Exiting | Exited` alike. That combination is
/// currently unreachable -- `run_lifecycle` writes `Exiting` in exactly one
/// place, with `exit` still `None` -- but it is pinned by the table test
/// below so a future writer cannot change the answer silently.
///
/// The containment conjunct is the local enforcement of this function's
/// safety property. Today it is implied: `run_lifecycle` writes the one and
/// only production record carrying `exit` in a single update that also sets
/// `containment_empty`, and it selects `Phase::Exited` exactly when that
/// lifecycle recorded no error -- which it can only do after observing the
/// domain empty. Asserting it here turns that cross-file induction into an
/// invariant checked on a record already in hand, so a future lifecycle
/// change that wrote `Exited` with an unproven domain would fail startup
/// instead of silently reporting success for a session with an escaped
/// descendant.
///
/// `containment_empty` is `Option<bool>` only for on-disk records written
/// before the field existed (see `SessionRecord::containment_proven_empty`).
/// It cannot be `None` here: `start_session` writes the initial record with
/// an explicit `Some(false)`, and every later writer -- the worker's
/// lifecycle and startup-failure paths, and this process's own
/// `persist_independent_cleanup_proof` -- writes an explicit `Some`. The
/// record read here was written by the worker this same call just spawned,
/// so the legacy shape is unreachable and the strict `Some(true)` comparison
/// cannot reject a genuinely completed session.
pub(super) fn exited_worker_completed_startup(record: &SessionRecord) -> bool {
    matches!(record.phase, Phase::Exiting | Phase::Exited)
        && record.exit.is_some()
        && record.containment_empty == Some(true)
}

/// What a worker that has already exited during startup means for the
/// start: a clean exit whose record is gone is a session that finished and
/// auto-removed itself (that deletion is the proof the lifecycle
/// completed); a clean exit that left a completed record
/// (`exited_worker_completed_startup`) did not fail to start either; every
/// other shape -- a crashed worker, an unreadable-but-present record, a
/// non-terminal one -- is a startup failure named by the exit status
/// rather than by a read error.
fn exited_worker_outcome(
    paths: &Paths,
    id: Uuid,
    status: std::process::ExitStatus,
    last_seen: SessionRecord,
) -> Result<SessionRecord> {
    if status.success() {
        if !paths.record(id).exists() {
            return Ok(auto_removed_completion(last_seen));
        }
        if let Ok(final_record) = read_session_record(paths, id) {
            if exited_worker_completed_startup(&final_record) {
                return Ok(final_record);
            }
        }
    }
    bail!("worker exited during startup: {status}")
}

/// Reconstruct a start response for a worker that finished cleanly and
/// deleted its own record (natural exit, Ctrl-D, or an in-startup kill).
/// The on-disk record is already gone; this is only what `a start --json`
/// returns to the client that launched it.
fn auto_removed_completion(mut record: SessionRecord) -> SessionRecord {
    if !matches!(record.phase, Phase::Exited) {
        record.phase = Phase::Exited;
    }
    record.containment_empty = Some(true);
    if record.exit.is_none() {
        record.exit = Some(crate::ExitInfo {
            code: None,
            signal: None,
            oom_killed: false,
            exited_at_ms: crate::now_ms(),
        });
    }
    record
}

/// The `SessionRecord::parent_session` value for a session being started
/// here: the calling process's ambient `APLEXER_SESSION_ID` stamp (see
/// `discover_session_id`, which also walks ancestor environments), kept only
/// when it names a record that still exists. Deliberately infallible -- a
/// stale stamp (parent already killed/forgotten, a leftover export, an
/// unparsable value) means "no recorded lineage", never a failed start.
fn resolve_parent_session(paths: &Paths) -> Option<Uuid> {
    let parent = crate::discover_session_id()?;
    read_session_record(paths, parent).map(|_| parent).ok()
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
    validate_tag(&req.tag)?;
    let workspace = canonical_workspace(&req.workspace)?;
    let id = Uuid::new_v4();
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
    // Resolved before the registry lock so a missing worker executable
    // fails the start without holding up other commands.
    let mut command = worker_command(id, req.python.as_deref())?;
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
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
    // Held for the rest of the call when the predecessor is a pre-PID stub,
    // so a worker that was spawned into it cannot come up on top of the
    // state we are about to archive and delete.
    let mut _superseded_fence: Option<FileLock> = None;
    let mut reclaim: Option<ContainmentReap> = None;
    if req.fresh {
        // `--fresh` promises "always creates": a requested pair held by
        // something live is not an error, it is a reason to move to the next
        // free suffix. A pair that is free, or held only by a record
        // `reap_verdict` would hand over, keeps the exact requested tag --
        // the reclaim path below already owns taking those.
        let Some(chosen) = pick_fresh_tag(&registry, &workspace, &req.tag) else {
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
        _superseded_fence = fence_or_refuse(paths, existing).with_context(|| {
            format!(
                "workspace+tag already belongs to session {}; rename it or choose a different tag",
                existing.id
            )
        })?;
        reclaim = Some(verdict);
    }
    let mut startup = LaunchGuard::new(paths, id);
    let result = (|| -> Result<SessionRecord> {
        ensure_private_dir(&paths.state_session(id))?;
        ensure_private_dir(&paths.runtime_session(id))?;
        // Environment values may contain credentials. Hand them to the worker
        // through a private, one-shot runtime file instead of placing them in
        // the durable/public session record returned by list/status/watch.
        //
        // Written WITHOUT fsync (benchmark PLAN P0.3): this file is deleted
        // by the guard below as soon as startup completes, so crash
        // durability across it is not required -- a crash before the worker
        // reads it just fails this start, which rollback cleans up. The
        // session record below keeps its full fsync + parent-sync durability
        // (see worker_startup_transaction tests). Saves two fsyncs on every
        // `a start`. Mode 0600: it carries secrets.
        let launch_environment_path = paths.runtime_session(id).join("launch-environment.json");
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&launch_environment_path)
                .with_context(|| format!("create {}", launch_environment_path.display()))?;
            serde_json::to_writer_pretty(&mut file, &launch.env)
                .with_context(|| format!("write {}", launch_environment_path.display()))?;
            use std::io::Write as _;
            file.write_all(b"\n")?;
        }
        let _launch_environment_guard = LaunchEnvironmentGuard(launch_environment_path);
        let now = crate::now_ms();
        let parent_session = resolve_parent_session(paths);
        let record = SessionRecord {
            parent_session,
            schema_version: SCHEMA_VERSION,
            id,
            workspace: workspace.clone(),
            tag,
            engine: launch.engine,
            profile: launch.profile,
            command: launch.command,
            cwd: launch.cwd,
            env: session_metadata_env(&launch.env),
            env_unset: launch.env_unset,
            limits: launch.limits,
            history_bytes: launch.history_bytes,
            created_at_ms: now,
            updated_at_ms: now,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase: Phase::Starting,
            worker_pid: None,
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(false),
            socket_path: paths.socket(id),
            history_path: paths.history(id),
            exit: None,
            error: None,
        };
        atomic_write_json(&paths.record(id), &record)?;
        let worker_log = File::create(paths.state_session(id).join("worker.log"))
            .context("create worker log")?;
        // Opt-in placement escape (issue #1): `setsid()` detaches the worker
        // from the launching terminal but leaves it in the ambient cgroup,
        // which is beneath user@UID.service whenever this launch is. When
        // APLEXER_LAUNCH_SYSTEM_SCOPE=system is set AND the system-scope
        // backend probes as working, spawn the worker inside a system
        // manager scope so the PTY keeper does not share the per-user
        // manager's lifecycle. Any probe/wrap failure degrades to the plain
        // setsid() launch with a printed reason -- the escape must never
        // turn into a broken start, and the honest placement warning from
        // `start_session` covers the degraded shape.
        if crate::placement::system_scope_requested() {
            match crate::system_scope_escape_decision() {
                Ok(true) => {
                    if let Err(error) = crate::wrap_worker_in_system_scope(id, &mut command) {
                        eprintln!(
                            "warning: APLEXER_LAUNCH_SYSTEM_SCOPE=system requested, but the \
                             worker could not be wrapped in a system scope ({error:#}); the \
                             worker stays in the ambient cgroup"
                        );
                    }
                }
                // The decision is `false` only when the escape was not
                // requested, which the branch condition already established.
                Ok(false) => {}
                Err(error) => {
                    eprintln!(
                        "warning: APLEXER_LAUNCH_SYSTEM_SCOPE=system requested, but the \
                         system-scope backend is unavailable ({error:#}); the worker stays \
                         in the ambient cgroup"
                    );
                }
            }
        }
        command
            .env("APLEXER_RUNTIME_DIR", &paths.runtime_root)
            .env("APLEXER_STATE_DIR", &paths.state_root)
            .env("APLEXER_CONFIG", &paths.config_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(worker_log));
        if let (Some(rows), Some(cols)) = (req.worker_rows, req.worker_cols) {
            command
                .arg("--rows")
                .arg(rows.to_string())
                .arg("--cols")
                .arg(cols.to_string());
        }
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                // Close the fork/exec-to-handler race: the worker inherits
                // these blocked signals, installs cancellation handlers as
                // its first startup action, then unblocks them. A timeout can
                // therefore never deliver the default terminating action in
                // the narrow window before the handler exists.
                let mut signals: libc::sigset_t = std::mem::zeroed();
                if libc::sigemptyset(&mut signals) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::sigaddset(&mut signals, libc::SIGTERM) != 0
                    || libc::sigaddset(&mut signals, libc::SIGINT) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                let result = libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut());
                if result != 0 {
                    return Err(io::Error::from_raw_os_error(result));
                }
                Ok(())
            });
        }
        startup.track_child(command.spawn().context("spawn worker")?);
        #[cfg(feature = "startup-test-hooks")]
        await_worker_exit_before_readiness_poll(&mut startup, paths, id)?;
        let started = Instant::now();
        let timeout = Duration::from_millis(req.startup_timeout_ms);
        let mut last_seen = record.clone();
        loop {
            // Check the deadline first so a zero timeout is deterministic,
            // independent of whether the worker wins the scheduling race.
            if started.elapsed() >= timeout {
                bail!(
                    "worker did not become ready within {} ms",
                    req.startup_timeout_ms
                );
            }
            // A clean finish deletes the record (natural exit, Ctrl-D, kill).
            // That is success for this start, not "worker vanished".
            if !paths.record(id).exists() {
                if let Some(status) = startup.child_mut().try_wait()? {
                    return exited_worker_outcome(paths, id, status, last_seen);
                }
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            let current = read_session_record(paths, id).context("read worker startup record")?;
            last_seen = current.clone();
            match current.phase {
                Phase::Running | Phase::Exiting | Phase::Exited if current.socket_path.exists() => {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    let probe_timeout = remaining.min(STARTUP_READY_RPC_SLICE);
                    if probe_worker_ready(&current, id, probe_timeout)? {
                        // The Ping response is the readiness commit. Read once
                        // more so a very short-lived workload can return its
                        // newest durable phase -- or report the auto-removed
                        // completion if the worker already deleted the record.
                        return match read_session_record(paths, id) {
                            Ok(latest) => Ok(latest),
                            Err(_) if !paths.record(id).exists() => {
                                Ok(auto_removed_completion(current))
                            }
                            Err(error) => {
                                Err(error).context("read worker startup record after ready ping")
                            }
                        };
                    }
                }
                Phase::Failed => bail!(
                    "worker startup failed: {}",
                    current.error.unwrap_or_else(|| "unknown error".into())
                ),
                _ => {}
            }
            if let Some(status) = startup.child_mut().try_wait()? {
                return exited_worker_outcome(paths, id, status, current);
            }
            // Polled at 5 ms, not 25 ms (benchmark PLAN P0.3): worker startup
            // is a serial chain (record write -> spawn -> PTY -> workload ->
            // Running record -> Ping), and every 25 ms quantum here is pure
            // launcher idle time on top of it. Short-lived and infrequent --
            // one wait per `a start`.
            thread::sleep(Duration::from_millis(5));
        }
    })();

    match result {
        Ok(record) => {
            // Move the predecessor out of the active registry atomically
            // before handing off the replacement. Until this succeeds the
            // startup guard can still roll the new worker back without ever
            // exposing two durable records for one selector.
            let archived = if let Some(existing) = &superseded {
                match archive_reclaimed_predecessor(paths, existing) {
                    Ok(path) => Some(path),
                    Err(retire_error) => {
                        return match startup.rollback() {
                            Ok(()) => Err(retire_error),
                            Err(rollback_error) => Err(anyhow!(
                                "retire predecessor failed: {retire_error:#}; replacement rollback also failed: {rollback_error:#}"
                            )),
                        };
                    }
                }
            } else {
                None
            };

            if let Err(handoff_error) = startup.hand_off_to_reaper() {
                let restore_error = match (&superseded, &archived) {
                    (Some(existing), Some(archived)) => {
                        restore_superseded_session(paths, existing.id, archived).err()
                    }
                    _ => None,
                };
                let rollback_error = startup.rollback().err();
                let mut message = format!("hand off replacement worker: {handoff_error:#}");
                if let Some(error) = restore_error {
                    message.push_str(&format!("; restore predecessor failed: {error:#}"));
                }
                if let Some(error) = rollback_error {
                    message.push_str(&format!("; replacement rollback failed: {error:#}"));
                }
                bail!(message);
            }

            if let (Some(existing), Some(archived)) = (superseded, archived) {
                if let Err(error) = cleanup_superseded_archive(&archived) {
                    bail!(
                        "replacement {} is ready and manageable by UUID, but superseded session {} cleanup failed; its remaining evidence is retained at {}: {error:#}",
                        record.id,
                        existing.id,
                        archived.display()
                    );
                }
                let _ = fs::remove_dir_all(paths.runtime_session(existing.id));
                // Say plainly when the pair was taken from a record whose
                // worker never proved its containment domain empty, exactly
                // as `a prune` reports the same class of removal. The
                // predecessor's manual-investigation trail is gone with it,
                // and a caller that only ever sees a successful `a start`
                // would otherwise have no way to know that happened.
                if reclaim == Some(ContainmentReap::NoRemainingHandle) {
                    eprintln!(
                        "a: reclaimed workspace+tag from broken session {} without a containment proof; its worker died without recording one and nothing addressable remained",
                        existing.id
                    );
                }
            }
            Ok(record)
        }
        Err(start_error) => match startup.rollback() {
            Ok(()) => Err(start_error),
            Err(rollback_error) => Err(anyhow!(
                "startup failed: {start_error:#}; rollback also failed: {rollback_error:#}"
            )),
        },
    }
}

#[cfg(test)]
mod startup_acceptance_tests {
    use super::*;
    use crate::ExitInfo;

    pub(super) fn startup_record(
        phase: Phase,
        exit: Option<ExitInfo>,
        containment_empty: Option<bool>,
    ) -> SessionRecord {
        let mut record = SessionRecord::fixture("/ws", "main");
        record.phase = phase;
        record.worker_pid = Some(1);
        record.containment_empty = containment_empty;
        record.exit = exit;
        record
    }

    /// `(description, phase, exit, containment_empty, expected_accept)`.
    type AcceptanceCase = (&'static str, Phase, Option<ExitInfo>, Option<bool>, bool);

    pub(super) fn clean_exit() -> Option<ExitInfo> {
        Some(ExitInfo {
            code: Some(0),
            signal: None,
            oom_killed: false,
            exited_at_ms: 2,
        })
    }

    /// Exhaustive matrix for the accept condition applied to an exited
    /// worker's durable record. Every clause of
    /// `exited_worker_completed_startup` is exercised in both directions:
    /// deleting any one of the three conjuncts turns at least one `false` row
    /// green, which is what makes this table load-bearing rather than
    /// decorative.
    #[test]
    pub(super) fn exited_worker_startup_acceptance_matrix() {
        let cases: &[AcceptanceCase] = &[
            // The fast-workload shape the readiness Ping can never observe:
            // ran to completion, recorded its exit, proved containment empty.
            (
                "exited with exit info and proven-empty containment",
                Phase::Exited,
                clean_exit(),
                Some(true),
                true,
            ),
            // Terminal phase, but nothing proves the workload ever ran.
            (
                "exited without exit info",
                Phase::Exited,
                None,
                Some(true),
                false,
            ),
            // The only shape the `exit.is_some()` conjunct guards on its own.
            (
                "exiting without exit info",
                Phase::Exiting,
                None,
                Some(true),
                false,
            ),
            // Pinned decision: symmetric with the readiness arm's
            // `Running | Exiting | Exited`, currently unreachable in practice.
            (
                "exiting with exit info and proven-empty containment",
                Phase::Exiting,
                clean_exit(),
                Some(true),
                true,
            ),
            // A worker that vanished mid-run never became ready, whatever
            // exit info happens to be on the record.
            (
                "running with exit info",
                Phase::Running,
                clean_exit(),
                Some(true),
                false,
            ),
            // The exact initial record `start_session` writes before the
            // worker registers itself.
            (
                "starting, as start_session first writes it",
                Phase::Starting,
                None,
                Some(false),
                false,
            ),
            // Same phase, but with every other clause satisfied, so this row
            // isolates the phase guard rather than riding on `exit`.
            (
                "starting with exit info and proven-empty containment",
                Phase::Starting,
                clean_exit(),
                Some(true),
                false,
            ),
            // The worker's own recorded failure is never laundered into a
            // completed session here.
            (
                "failed with exit info and proven-empty containment",
                Phase::Failed,
                clean_exit(),
                Some(true),
                false,
            ),
            // The safety clause: a terminal record whose containment domain
            // is NOT proven empty may have an escaped descendant, so
            // reporting startup success would be exactly the laundering this
            // predicate exists to prevent.
            (
                "exited with exit info but containment not proven empty",
                Phase::Exited,
                clean_exit(),
                Some(false),
                false,
            ),
            // Legacy/absent proof is not proof. Unreachable for a record
            // written by the worker this call spawned, but the predicate
            // must not silently widen if that ever stops holding.
            (
                "exited with exit info but no containment field",
                Phase::Exited,
                clean_exit(),
                None,
                false,
            ),
        ];
        // Collect every mismatch instead of stopping at the first, so
        // deleting a conjunct names the whole set of rows it breaks.
        let mut mismatches = Vec::new();
        for (name, phase, exit, containment_empty, expected) in cases {
            let record = startup_record(phase.clone(), exit.clone(), *containment_empty);
            let actual = exited_worker_completed_startup(&record);
            if actual != *expected {
                mismatches.push(format!("{name}: expected {expected}, got {actual}"));
            }
        }
        assert!(
            mismatches.is_empty(),
            "exited-worker acceptance matrix regressed:\n  {}",
            mismatches.join("\n  ")
        );
    }
}
