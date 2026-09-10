//! The one start path: every verb that creates a session lands here.
//!
//! One reason to exist: claim checks, tag allocation, the supersede/reclaim
//! decision, worker readiness probing, and rollback on failure are one
//! story. Splitting them per CLI verb is how two start paths drift into
//! disagreeing about who owns a workspace+tag.

use super::*;

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

pub(super) fn connect_startup_control(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "startup control connection deadline expired",
        ));
    }
    let path_bytes = path.as_os_str().as_bytes();
    CString::new(path_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket path contains NUL"))?;
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path_bytes.len() >= address.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is too long",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    unsafe {
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr(),
            address.sun_path.as_mut_ptr().cast::<u8>(),
            path_bytes.len(),
        );
    }
    let address_len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path_bytes.len() + 1)
        as libc::socklen_t;
    let raw_fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let connected = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            address_len,
        )
    };
    if connected != 0 {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => {}
            Some(libc::EINPROGRESS) | Some(libc::EALREADY) => {
                let timeout_ms = timeout.as_millis().clamp(1, i32::MAX as u128) as i32;
                let mut poll_fd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                loop {
                    let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
                    if ready > 0 {
                        break;
                    }
                    if ready == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("connect {} timed out", path.display()),
                        ));
                    }
                    let poll_error = io::Error::last_os_error();
                    if poll_error.kind() != io::ErrorKind::Interrupted {
                        return Err(poll_error);
                    }
                }
                let mut socket_error: libc::c_int = 0;
                let mut socket_error_len = std::mem::size_of_val(&socket_error) as libc::socklen_t;
                if unsafe {
                    libc::getsockopt(
                        fd.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_ERROR,
                        (&raw mut socket_error).cast::<libc::c_void>(),
                        &raw mut socket_error_len,
                    )
                } != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if socket_error != 0 {
                    return Err(io::Error::from_raw_os_error(socket_error));
                }
            }
            // Linux AF_UNIX uses EAGAIN for a full listen backlog. Returning
            // immediately lets the outer startup loop retry without ever
            // blocking past its absolute deadline.
            _ => return Err(error),
        }
    }
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { UnixStream::from_raw_fd(fd.into_raw_fd()) })
}

/// A pathname and a persisted phase are not readiness evidence. Complete a
/// framed request/response round trip and require the worker to identify the
/// exact session the launcher just spawned.
fn probe_worker_ready(
    record: &SessionRecord,
    expected_id: Uuid,
    timeout: Duration,
) -> Result<bool> {
    let mut stream = match connect_startup_control(&record.socket_path, timeout) {
        Ok(stream) => stream,
        Err(_) => return Ok(false),
    };
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let request = Request::new(expected_id, Operation::Ping);
    let request_id = request.request_id.clone();
    if write_json(&mut stream, &request).is_err() {
        return Ok(false);
    }
    let frame = match read_frame(&mut stream) {
        Ok(Some(frame)) => frame,
        Ok(None) | Err(_) => return Ok(false),
    };
    let result = response_result(frame, &request_id).context("worker readiness Ping failed")?;
    if result.get("pong").and_then(Value::as_bool) != Some(true) {
        bail!("worker readiness response omitted pong");
    }
    let reported_id = result
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("worker readiness response omitted session id"))?
        .parse::<Uuid>()
        .context("parse worker readiness session id")?;
    if reported_id != expected_id {
        bail!("worker readiness response identified session {reported_id}, expected {expected_id}");
    }
    Ok(true)
}

fn archive_superseded_session(paths: &Paths, id: Uuid) -> Result<PathBuf> {
    let retired_root = paths.state_root.join(RETIRED_SESSIONS_DIR);
    ensure_private_dir(&retired_root)?;
    let source = paths.state_session(id);
    let archived = retired_root.join(id.to_string());
    if archived.try_exists()? {
        bail!(
            "cannot retire superseded session {id}: archive {} already exists",
            archived.display()
        );
    }
    fs::rename(&source, &archived).with_context(|| {
        format!(
            "atomically retire superseded session {id} from {} to {}",
            source.display(),
            archived.display()
        )
    })?;
    if let Err(error) = (|| -> Result<()> {
        File::open(paths.state_root.join("sessions"))?.sync_all()?;
        File::open(&retired_root)?.sync_all()?;
        Ok(())
    })() {
        return match restore_superseded_session(paths, id, &archived) {
            Ok(()) => Err(error).context("sync retired predecessor transaction"),
            Err(restore_error) => Err(anyhow!(
                "sync retired predecessor transaction: {error:#}; restore also failed: {restore_error:#}"
            )),
        };
    }
    Ok(archived)
}

/// Retire the predecessor that `start_session` decided it could take the
/// `workspace+tag` from, re-deciding against the record as it stands on disk
/// right now.
///
/// The verdict formed before the spawn is advisory by construction: a worker
/// startup can take seconds, and `reap_verdict` is built out of live probes
/// (`/proc` liveness for the worker and the workload leader, and the
/// kernel's own view of a recorded cgroup), none of which the registry lock
/// freezes. It holds off other aplexer commands, not the world: a recycled
/// pid can make a dead `workload_pid` read alive again, and a containment
/// domain the caller could not inspect a moment ago may answer now. So
/// re-read and re-run the same predicate before destroying anything -- the
/// same rule `a prune`'s `reap_session_state` follows, for the same reason.
///
/// Refusing here is a start FAILURE, not a silent downgrade: the caller
/// rolls the freshly started replacement back rather than leaving two
/// durable records claiming one selector.
fn archive_reclaimed_predecessor(paths: &Paths, existing: &SessionRecord) -> Result<PathBuf> {
    let current = read_session_record(paths, existing.id).with_context(|| {
        format!(
            "re-read superseded session {} before retiring it",
            existing.id
        )
    })?;
    if reap_verdict(&current).is_none() {
        bail!(
            "superseded session {} is live again (state: {}); refusing to retire it",
            current.id,
            current.observed_state()
        );
    }
    archive_superseded_session(paths, existing.id)
}

fn restore_superseded_session(paths: &Paths, id: Uuid, archived: &Path) -> Result<()> {
    let destination = paths.state_session(id);
    fs::rename(archived, &destination).with_context(|| {
        format!(
            "restore superseded session {id} from {} to {}",
            archived.display(),
            destination.display()
        )
    })?;
    File::open(paths.state_root.join("sessions"))?.sync_all()?;
    if let Some(parent) = archived.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn cleanup_superseded_archive(path: &Path) -> Result<()> {
    #[cfg(feature = "startup-test-hooks")]
    if std::env::var_os("APLEXER_TEST_FAIL_SUPERSEDED_CLEANUP").is_some() {
        bail!("injected superseded-session cleanup failure");
    }
    fs::remove_dir_all(path).with_context(|| format!("remove archive {}", path.display()))?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
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

/// The tag a `--fresh` start should claim: the requested base itself while
/// nothing live holds it, otherwise the first `<base>-2`, `<base>-3`, …
/// suffix no live session holds either. "Live" here means exactly what the
/// supersede check in `start_session` refuses to take -- a holder
/// `reap_verdict` would hand over does not count, so a dead `main-2` is
/// reclaimed under its own name rather than skipped. `None` means no
/// candidate fits `validate_tag` any more, which for a valid base can only
/// be the 64-byte length cap.
pub fn pick_fresh_tag(records: &[SessionRecord], workspace: &Path, base: &str) -> Option<String> {
    // Any live holder counts, not just the first record with the pair: a
    // rename that took a dead holder's name leaves the corpse next to the
    // live session (issue #13), and `--fresh` must read that pair as taken.
    let live_holder = |tag: &str| {
        records
            .iter()
            .filter(|r| r.workspace == workspace && r.tag == tag)
            .any(|r| crate::reap_verdict(r).is_none())
    };
    // Suffixes start at 2: a bare `main` plus `main-2` reads as "the main
    // one and its first sibling", not as an off-by-one list. A base that
    // already ends in `-<number>` (or cannot be suffixed numerically at all)
    // simply continues from the next integer.
    let mut candidate = base.to_string();
    while live_holder(&candidate) {
        candidate = match candidate.rsplit_once('-').and_then(|(stem, n)| {
            let next = n.parse::<u64>().ok()?.checked_add(1)?;
            Some(format!("{stem}-{next}"))
        }) {
            Some(next) => next,
            None => format!("{base}-2"),
        };
        if validate_tag(&candidate).is_err() {
            return None;
        }
    }
    Some(candidate)
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
mod fresh_tag_tests {
    use super::*;

    /// A record liveness is decided by pid probes (`reap_verdict`), so a
    /// "live" holder only needs a pid that exists -- the test process's own
    /// -- and a reclaimable one needs no pids plus an empty-containment
    /// shape, exactly like `mod reclaim_tests`' zombie fixture.
    fn record(workspace: &str, tag: &str, worker_pid: Option<u32>) -> SessionRecord {
        let mut record = SessionRecord::fixture(workspace, tag);
        record.worker_pid = worker_pid;
        record
    }

    fn live(workspace: &str, tag: &str) -> SessionRecord {
        record(workspace, tag, Some(std::process::id()))
    }

    fn dead(workspace: &str, tag: &str) -> SessionRecord {
        record(workspace, tag, None)
    }

    #[test]
    fn free_base_is_used_verbatim() {
        let ws = Path::new("/ws");
        assert_eq!(pick_fresh_tag(&[], ws, "main"), Some("main".into()));
    }

    #[test]
    fn live_base_moves_to_the_next_free_suffix() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "main")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main-2".into()));
    }

    #[test]
    fn suffix_walk_skips_taken_numbers() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "main"), live("/ws", "main-2")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main-3".into()));
    }

    #[test]
    fn reclaimable_holder_keeps_the_requested_tag() {
        // A dead `main` is not "someone else's session": the ordinary
        // reclaim path takes the exact name, so `--fresh` must not skip it.
        let ws = Path::new("/ws");
        let records = vec![dead("/ws", "main")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main".into()));
    }

    #[test]
    fn reclaimable_suffix_is_taken_under_its_own_name() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "main"), dead("/ws", "main-2")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main-2".into()));
    }

    #[test]
    fn other_workspaces_do_not_count() {
        let ws = Path::new("/ws");
        let records = vec![live("/elsewhere", "main"), live("/elsewhere", "main-2")];
        assert_eq!(pick_fresh_tag(&records, ws, "main"), Some("main".into()));
    }

    #[test]
    fn base_with_trailing_number_increments_from_it() {
        let ws = Path::new("/ws");
        let records = vec![live("/ws", "review-2")];
        assert_eq!(
            pick_fresh_tag(&records, ws, "review-2"),
            Some("review-3".into())
        );
    }

    #[test]
    fn saturated_numeric_suffix_restarts_from_the_base() {
        // `n + 1` on a parsed u64 suffix overflowed for `x-18446744073709551615`;
        // an unsuffixable candidate falls back to `<base>-2` like any other.
        let ws = Path::new("/ws");
        let base = format!("x-{}", u64::MAX);
        let records = vec![live("/ws", &base)];
        assert_eq!(
            pick_fresh_tag(&records, ws, &base),
            Some(format!("{base}-2"))
        );
    }

    #[test]
    fn length_capped_base_reports_no_candidate() {
        let ws = Path::new("/ws");
        let base = "a".repeat(64);
        let records = vec![live("/ws", &base)];
        assert_eq!(pick_fresh_tag(&records, ws, &base), None);
    }
}

#[cfg(test)]
mod reclaim_tests {
    use super::*;
    use crate::{atomic_write_json, ContainmentReap};

    /// A registry containing exactly one record, with its paths wired to the
    /// throwaway state/runtime roots so `read_session_record`'s identity
    /// checks accept it.
    fn seeded_registry(
        record: &mut SessionRecord,
    ) -> (Paths, tempfile::TempDir, tempfile::TempDir) {
        let state_dir = tempfile::tempdir().unwrap();
        let runtime_dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: runtime_dir.path().to_path_buf(),
            state_root: state_dir.path().to_path_buf(),
            config_file: state_dir.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        record.socket_path = paths.socket(record.id);
        record.history_path = paths.history(record.id);
        fs::create_dir_all(paths.state_session(record.id)).unwrap();
        fs::create_dir_all(paths.runtime_session(record.id)).unwrap();
        atomic_write_json(&paths.record(record.id), record).unwrap();
        (paths, state_dir, runtime_dir)
    }

    /// The reported zombie shape: worker dead, `phase` stuck at `running`,
    /// nothing left running.
    fn zombie_record() -> SessionRecord {
        SessionRecord::fixture("/ws/zombie", "zt")
    }

    /// A reclaimable predecessor is retired by the ordinary archive
    /// transaction: durable state moves to `retired-sessions/<id>`, nothing
    /// is deleted yet, so the caller can still restore it.
    #[test]
    fn a_reclaimable_predecessor_is_archived_not_destroyed() {
        let mut record = zombie_record();
        let (paths, _state, _runtime) = seeded_registry(&mut record);
        assert!(reap_verdict(&record).is_some());

        let archived = archive_reclaimed_predecessor(&paths, &record).expect("archive predecessor");
        assert!(archived.join("session.json").exists(), "archive is empty");
        assert!(!paths.state_session(record.id).exists());

        restore_superseded_session(&paths, record.id, &archived).expect("restore predecessor");
        assert!(paths.record(record.id).exists());
    }

    /// The verdict `start_session` forms before it spawns is stale by
    /// construction: worker startup takes time, and `reap_verdict` is built
    /// from live probes the registry lock does not freeze (`/proc` liveness
    /// and the kernel's view of a cgroup). So the record is re-read and
    /// re-judged immediately before it is retired.
    ///
    /// Driven here through the fact that can genuinely change under a held
    /// registry lock: the workload leader pid coming back alive (a recycled
    /// pid). The caller's copy still says "dead, reclaimable"; disk says a
    /// process is running; the retire must refuse and leave the predecessor
    /// exactly where it was.
    #[test]
    fn retiring_a_predecessor_re_reads_the_record_before_destroying_it() {
        let stale = zombie_record();
        let mut on_disk = stale.clone();
        let mut leader = Command::new("sleep").arg("30").spawn().unwrap();
        on_disk.workload_pid = Some(leader.id());
        let (paths, _state, _runtime) = seeded_registry(&mut on_disk);

        // What the caller believes, formed before the spawn.
        assert_eq!(
            reap_verdict(&stale),
            Some(ContainmentReap::NoRemainingHandle)
        );

        let error = archive_reclaimed_predecessor(&paths, &stale)
            .expect_err("retire must refuse a predecessor that is live on disk");
        let error = format!("{error:#}");
        assert!(error.contains("is live again"), "{error}");
        assert!(error.contains(&stale.id.to_string()), "{error}");
        assert!(
            paths.record(stale.id).exists(),
            "a refused retire still moved the predecessor's durable state"
        );
        assert!(
            !paths
                .state_root
                .join(RETIRED_SESSIONS_DIR)
                .join(stale.id.to_string())
                .exists(),
            "a refused retire stranded the predecessor in the archive"
        );
        assert!(
            leader.try_wait().unwrap().is_none(),
            "the retire path must never signal anything"
        );

        // Same record, leader gone: reclaimable again. Proves the refusal
        // came from the re-read and not from a blanket refusal.
        leader.kill().unwrap();
        leader.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while crate::process_alive(on_disk.workload_pid.unwrap()) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        archive_reclaimed_predecessor(&paths, &stale).expect("archive once the leader is gone");
        assert!(!paths.state_session(stale.id).exists());
    }

    /// The fence itself, without a spawn: a record in the spawn-to-worker-lock
    /// gap (`phase: starting, worker_pid: null`) reads as `worker_alive:
    /// false` and is otherwise perfectly reclaimable, so only the worker
    /// lock stands between a live spawn and having its state taken.
    #[test]
    fn a_pre_pid_record_is_fenced_by_its_worker_lock() {
        let mut record = zombie_record();
        record.phase = Phase::Starting;
        let (paths, _state, _runtime) = seeded_registry(&mut record);
        assert!(!record.worker_alive());
        assert!(reap_verdict(&record).is_some());

        let held = FileLock::exclusive(&paths.worker_lock(record.id), true).unwrap();
        assert!(matches!(
            fence_pre_pid_worker(&paths, &record).unwrap(),
            PrePidFence::WorkerHoldsLock(_)
        ));
        drop(held);
        assert!(matches!(
            fence_pre_pid_worker(&paths, &record).unwrap(),
            PrePidFence::Fenced(Some(_))
        ));

        // Past the gap, the pid is the authority and no fence is taken --
        // otherwise every ordinary reclaim would contend on a lock the live
        // worker legitimately holds.
        let mut registered = record.clone();
        registered.worker_pid = Some(std::process::id());
        assert!(matches!(
            fence_pre_pid_worker(&paths, &registered).unwrap(),
            PrePidFence::Fenced(None)
        ));
    }
}
