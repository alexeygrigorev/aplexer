//! The launch pipeline between the claim and the commit: write the initial
//! record, spawn the worker, poll it to readiness, and retire the
//! predecessor once the replacement is proven up.

use super::*;
use crate::ResolvedLaunch;

#[cfg(feature = "startup-test-hooks")]
use super::acceptance::await_worker_exit_before_readiness_poll;
use super::acceptance::{auto_removed_completion, exited_worker_outcome, resolve_parent_session};
use super::claim::PairClaim;

/// Create the session's directories, hand the worker its one-shot launch
/// environment, and persist the `Starting` record. The returned guard
/// removes the launch environment file once startup is over, whichever
/// way it ended.
pub(super) fn write_initial_record(
    paths: &Paths,
    id: Uuid,
    workspace: &Path,
    tag: String,
    launch: ResolvedLaunch,
) -> Result<(SessionRecord, LaunchEnvironmentGuard)> {
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
    let launch_environment_guard = LaunchEnvironmentGuard(launch_environment_path);
    let now = crate::now_ms();
    let parent_session = resolve_parent_session(paths);
    let record = SessionRecord {
        parent_session,
        schema_version: SCHEMA_VERSION,
        id,
        workspace: workspace.to_path_buf(),
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
    Ok((record, launch_environment_guard))
}

/// Spawn the worker process for `id` into the guard's custody.
pub(super) fn spawn_worker_process(
    paths: &Paths,
    req: &StartRequest,
    id: Uuid,
    mut command: Command,
    startup: &mut LaunchGuard<'_>,
) -> Result<()> {
    let worker_log =
        File::create(paths.state_session(id).join("worker.log")).context("create worker log")?;
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
    Ok(())
}

/// Poll until the worker proves readiness (`probe_worker_ready`), finishes
/// cleanly on its own (`exited_worker_outcome`), records a startup failure,
/// or the startup budget runs out.
pub(super) fn await_worker_ready(
    paths: &Paths,
    req: &StartRequest,
    id: Uuid,
    record: SessionRecord,
    startup: &mut LaunchGuard<'_>,
) -> Result<SessionRecord> {
    #[cfg(feature = "startup-test-hooks")]
    await_worker_exit_before_readiness_poll(startup, paths, id)?;
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
}

/// The replacement is ready: retire the predecessor (if any), hand the
/// worker to the detached reaper, and only then delete the predecessor's
/// archive. Any failure before the hand-off rolls the replacement back so
/// two durable records never claim one selector.
pub(super) fn commit_replacement(
    paths: &Paths,
    startup: &mut LaunchGuard<'_>,
    claim: PairClaim,
    record: SessionRecord,
) -> Result<SessionRecord> {
    // Move the predecessor out of the active registry atomically
    // before handing off the replacement. Until this succeeds the
    // startup guard can still roll the new worker back without ever
    // exposing two durable records for one selector.
    let archived = if let Some(existing) = &claim.superseded {
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
        let restore_error = match (&claim.superseded, &archived) {
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

    if let (Some(existing), Some(archived)) = (claim.superseded, archived) {
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
        if claim.reclaim == Some(ContainmentReap::NoRemainingHandle) {
            eprintln!(
                "a: reclaimed workspace+tag from broken session {} without a containment proof; its worker died without recording one and nothing addressable remained",
                existing.id
            );
        }
    }
    Ok(record)
}
