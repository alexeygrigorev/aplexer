use crate::api::fence_or_refuse;
use crate::*;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

mod hub;
use hub::*;
mod termination;
use termination::*;
mod procs;
pub(crate) use crate::pidfd::direct_child_pids_in;
use procs::*;
mod lifecycle;
use lifecycle::*;
mod spawn;
use spawn::*;
mod runtime;
use runtime::*;
mod startup;
use startup::*;
mod control_socket;
use control_socket::*;
mod connection;
use connection::*;
mod attach;
use attach::*;
pub(crate) use termination::{disown_child_pid, own_child_pid};

#[derive(Debug, Clone)]
enum OutputEvent {
    /// One PTY read (or a coalesced screen snapshot), shared by every
    /// subscriber it is queued for: the hub allocates it once per read
    /// instead of copying the chunk into each subscriber's queue.
    Data(Arc<[u8]>),
    /// Worker-internal only (docs/terminal-state-design.md section 5.1);
    /// `handle_attach`'s writer thread maps this to a `ServerEvent::Layout`
    /// JSON frame for `want_screen` subscribers and drops it otherwise.
    Layout(screen::LayoutChange),
    Exit(ExitInfo),
    Error(String),
}

/// Bound memory retained on behalf of clients that stop reading.
///
/// The queue is bounded by **bytes first, events second**: a lagging client
/// is evicted once its backlog exceeds roughly 1 MiB of PTY data *or* 1024
/// queued events, whichever comes first. Bytes are the real memory bound
/// (PTY reads are at most 32 KiB); the event cap only bounds per-event
/// overhead for pathological streams of tiny writes.
///
/// An event-count-only bound (the old 32-event queue) evicted clients on
/// bursts that were tiny in bytes but numerous in events -- e.g. a
/// resize-triggered TUI repaint arriving as dozens of few-hundred-byte PTY
/// reads (~30KB total). That made `a attach` print "attached client fell
/// behind live output" and detach immediately on busy sessions, even though
/// the backlog was negligible. A lagging client is still disconnected and
/// can reattach for a fresh tail or screen snapshot, but only when its
/// backlog is actually large.
///
/// Live-screen (`want_screen`) subscribers never take that eviction path in
/// practice: once their backlog passes the coalescing threshold below, the
/// queued backlog is replaced by a fresh screen snapshot -- the current
/// screen, not a fast-forward replay -- so a client that went quiet (tab in
/// the background, slow link, slept laptop) jumps straight to live instead
/// of watching everything it missed at 10x speed. Raw-tail (`--history-bytes`)
/// subscribers keep the old evict-and-reattach contract, since replacing
/// their bytes with a repaint would break byte-exact consumers.
const MAX_SUBSCRIBER_QUEUED_BYTES: usize = 1024 * 1024;
const MAX_SUBSCRIBER_QUEUED_EVENTS: usize = 1024;
/// When a live-screen subscriber falls this far behind, stop queuing raw PTY
/// bytes for it and replace its backlog with the current screen snapshot
/// instead. Must exceed one max-size PTY read (32 KiB) so a single large
/// burst to a caught-up client still streams normally; 64 KiB is roughly two
/// such reads -- a brief stall replays briefly, a sustained stall jumps to
/// live. The event threshold sits halfway to the eviction cap so pathological
/// tiny-write streams coalesce rather than disconnect; the ordinary
/// dozens-of-small-reads TUI repaint (~200 events, ~40KB) stays well under
/// both and still streams.
const COALESCE_SUBSCRIBER_QUEUED_BYTES: usize = 64 * 1024;
const COALESCE_SUBSCRIBER_QUEUED_EVENTS: usize = 512;
const MAX_SUBSCRIBERS: usize = 64;
/// Attach connections are long-lived, while ordinary RPCs are short-lived.
/// Leave room above the subscriber ceiling for status/capture/kill calls,
/// but never let a same-UID peer create worker threads without bound.
const MAX_CLIENT_CONNECTIONS: usize = 128;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(10);
const ACCEPT_RETRY_INITIAL: Duration = Duration::from_millis(25);
const ACCEPT_RETRY_MAX: Duration = Duration::from_secs(1);
const CONTROL_SOCKET_CHECK_INTERVAL: Duration = Duration::from_millis(500);
const HISTORY_RETRY_INITIAL: Duration = Duration::from_millis(500);
const HISTORY_RETRY_MAX: Duration = Duration::from_secs(30);
const DESCENDANT_POLL_INTERVAL: Duration = Duration::from_millis(25);
/// The bound on the SIGKILL sweep that ends every kill path: after the
/// graceful signal's grace window, and immediately for `--signal KILL`. A
/// `Kill` RPC's response is held until this sweep proves the domain empty,
/// so a client must wait at least grace + this before giving up on it
/// (`api::kill_response_timeout`).
pub const DESCENDANT_KILL_TIMEOUT: Duration = Duration::from_secs(2);
/// Kill-path responsiveness (benchmark PLAN P0.2): the graceful-signal wait
/// in `WorkerRuntime::kill` and the post-kill connection drain poll with
/// `DESCENDANT_POLL_INTERVAL` (25 ms) by default, adding up to ~25 ms of
/// quantization after the workload is already gone. The kill path is
/// short-lived and infrequent (one RPC per session teardown), so poll it at
/// 5 ms instead -- ~20 ms saved on every HUP/TERM kill without touching the
/// steady-state lifecycle cadence.
const KILL_POLL_INTERVAL: Duration = Duration::from_millis(5);
/// How long a Rename RPC keeps trying for the registry lock. `a start` holds
/// that lock across its whole spawn and readiness poll (the startup timeout,
/// 10 s by default), while the client waits for the Rename reply under its
/// 3 s control deadline: blocking on the lock meant a rename issued during a
/// start timed out client-side and then applied anyway once the lock came
/// free. Bounded well under the deadline so the refusal reaches the client.
const RENAME_REGISTRY_WAIT: Duration = Duration::from_secs(2);

/// Runs the worker for session `id`.
///
/// `initial_size`, when given, is the (rows, cols) to open the workload's
/// PTY at from the very first moment it's spawned, instead of the
/// hard-coded 24x80 default. This closes a startup race: previously every
/// session's PTY was opened at a fixed 24x80 regardless of the attaching
/// client's real terminal size, and the correction only arrived later as a
/// SIGWINCH-driven `AttachControl::Resize` once the client finished
/// connecting -- microseconds to milliseconds after the workload was
/// already running. A full-screen TUI that reads terminal geometry at
/// startup (ncurses `initscr()`) initializes against the wrong size, and
/// ncurses' resize handling does not always cleanly re-layout after a
/// startup-time resize, producing visibly garbled output (footer/rows
/// interleaved, stale leftover text) even though the PTY's winsize ends up
/// numerically correct moments later.
///
/// The caller (`cmd_start` in src/bin/a.rs) supplies this only for the
/// common immediate-attach case (`a start --attach`, `a -`), where it
/// already knows the attaching client's terminal size before the worker is
/// even spawned -- so the workload can be started at its true final size
/// with no resize-after-spawn step needed at all. A session started
/// detached, with no client attaching yet, has no size to offer and falls
/// back to the 24x80 default here; that session still gets resized
/// normally the first time someone does attach (see `attach()` in
/// src/bin/a.rs), the same as before this fix -- this only eliminates the
/// race for the case where the size is already known at spawn time.
pub fn run_worker(id: Uuid, initial_size: Option<(u16, u16)>) -> Result<()> {
    normalize_sigchld_for_child_management()?;
    install_termination_handlers()?;
    // This is process-wide and must precede every helper or workload spawn.
    // In particular, Cgroup::create invokes systemd-run and a waiter thread;
    // enabling the subreaper afterwards leaves a startup-time escape window.
    enable_child_subreaper()?;
    let paths = Paths::discover()?;
    let record_path = paths.record(id);
    ensure_private_dir(&paths.runtime_session(id))?;
    let mut _worker_lock = FileLock::exclusive(&paths.worker_lock(id), true)
        .with_context(|| format!("worker for {id} is already running"))?;
    let mut worker_lock_identity = trusted_lock_identity(&paths.worker_lock(id))?;
    let mut record = read_record_under_worker_lock(&paths, id)?;
    let legacy_environment = LaunchEnvironment(std::mem::take(&mut record.env));
    record.env = session_metadata_env(&legacy_environment.0);
    let mut startup = StartupGuard::new(&paths, &record);
    let setup = (|| -> Result<(UnixListener, (u64, u64), Arc<WorkerRuntime>)> {
        startup_checkpoint("after_worker_lock")?;
        let launch_environment_path = paths.runtime_session(id).join("launch-environment.json");
        let launch_environment =
            load_launch_environment(&launch_environment_path, legacy_environment)?;
        // Migrate a legacy record before exposing any further worker state,
        // retaining only non-secret roots needed for transcript discovery.
        record.worker_pid = Some(std::process::id());
        // Placement evidence (issue #1): the fork's pre_exec setsid() gave
        // this process a new session but left it in the ambient cgroup, so
        // whatever manager owns that cgroup can still kill this session
        // wholesale. Record where we actually are while we can still read
        // it -- after a manager-wide kill the path is gone and the failure
        // is unprovable, exactly the incident's `yolo` post-mortem problem.
        record.worker_cgroup = crate::placement::read_process_cgroup(std::process::id());
        record.updated_at_ms = now_ms();
        startup.failure_record = record.clone();
        atomic_write_json(&record_path, &record)?;
        startup_checkpoint("after_worker_record")?;

        let socket_path = paths.socket(id);
        if socket_path.exists() {
            fs::remove_file(&socket_path).context("remove stale control socket")?;
        }
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind {}", socket_path.display()))?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;
        let socket_identity = trusted_socket_identity(&socket_path)?;
        startup_checkpoint("after_control_socket")?;

        let requested_size = initial_size.unwrap_or((24, 80));
        let (rows, cols) = screen::validate_size(requested_size.0, requested_size.1)?;
        let cgroup = Cgroup::create(id, &record.limits, || {
            startup.cgroup_setup_started = true;
        })?;
        startup.cgroup = cgroup.clone();
        // Unlimited sessions (the common case: no memory/pids/cpu limits) have
        // no cgroup, so this second record write would persist byte-identical
        // containment fields plus a fresh timestamp -- a full fsync + parent
        // fsync for no new information (benchmark PLAN P0.3). Skip the write
        // and keep the already-persisted worker_pid record as the durable
        // state; the in-memory failure record is still updated for rollback.
        if cgroup.is_some() {
            record.containment_cgroup =
                cgroup.as_ref().map(|cgroup| cgroup.locator().to_path_buf());
            record.containment_cgroup_identity =
                cgroup.as_ref().map(|cgroup| cgroup.identity().clone());
            startup.failure_record = record.clone();
            atomic_write_json(&record_path, &record)?;
        } else {
            startup.failure_record = record.clone();
        }
        startup_checkpoint("after_cgroup")?;
        let (master_read, slave) = open_pty(rows, cols)?;
        let master_write = master_read.try_clone()?;
        let child_result = spawn_workload(
            &record,
            &launch_environment.0,
            master_read.as_raw_fd(),
            slave,
            cgroup.as_ref(),
        );
        // Launch values are one-shot: overwrite them as soon as spawn has
        // either succeeded or failed, never retaining them in the accept
        // loop or its background threads.
        drop(launch_environment);
        let child = child_result?;
        let pid = child.id();
        // Claim the leader before any code path can wait on it. The reaper
        // thread does not exist yet, but the claim is what documents (and
        // enforces) that `run_child_waiter` owns this pid's exit status.
        own_child_pid(pid);
        let child_slot = Arc::new(Mutex::new(Some(child)));
        startup.child = Some(Arc::clone(&child_slot));
        record.workload_pid = Some(pid);
        // Launch-time cgroup validation (issue #1): read where the workload
        // leader actually landed and, for a limited session, check that
        // against the scope systemd was asked to create for it. The
        // pre_exec cgroup.procs write is supposed to make a mismatch
        // impossible; if the two sources of truth ever disagree, say so in
        // worker.log instead of silently trusting the persisted locator.
        record.workload_cgroup = crate::placement::read_process_cgroup(pid);
        if let (Some(cgroup), Some(actual)) = (cgroup.as_ref(), record.workload_cgroup.as_deref()) {
            let expected = cgroup.proc_path();
            if actual != expected {
                eprintln!(
                    "warning: workload pid {pid} is in cgroup {actual}, not the recorded \
                     containment scope {expected}; resource limits may not apply to the \
                     workload's real location"
                );
            }
        }
        startup.failure_record = record.clone();
        // Publish the leader and cgroup locator before any injected or real
        // post-spawn failure. The launcher must never have to infer a
        // containment domain from an unpersisted in-memory PID.
        atomic_write_json(&record_path, &record)?;
        after_workload_spawn_checkpoint(pid)?;

        startup_checkpoint("before_history_open")?;
        validate_existing_history_node(&record.history_path)?;
        let history = History::open(record.history_path.clone(), record.history_bytes)?;
        startup_checkpoint("before_output_hub")?;
        let output = OutputHub::new(history, rows, cols, paths.screen_txt(id))?;
        record.phase = Phase::Running;
        record.updated_at_ms = now_ms();
        record.error = None;
        startup.failure_record = record.clone();
        let runtime = Arc::new(WorkerRuntime {
            id,
            paths: paths.clone(),
            record_path: record_path.clone(),
            runtime_session_dir: paths.runtime_session(id),
            socket_path,
            record: Mutex::new(record.clone()),
            pty_write: Mutex::new(Some(Arc::new(master_write))),
            workload: Mutex::new(WorkloadState {
                running: true,
                pgid: pid as i32,
            }),
            terminal: Mutex::new(TerminalState {
                rows,
                cols,
                clients: HashMap::new(),
                next_client_id: 1,
                activity_clock: 0,
            }),
            cgroup: Mutex::new(cgroup),
            kill_gate: Mutex::new(()),
            output,
            record_persistence_error: Mutex::new(None),
            active_connections: Arc::new(AtomicUsize::new(0)),
            last_activity_ms: AtomicU64::new(0),
        });
        start_worker_threads(
            Arc::clone(&runtime),
            master_read,
            Arc::clone(&child_slot),
            || {
                atomic_write_json(&record_path, &record)?;
                startup_checkpoint("after_running_record")
            },
        )?;
        Ok((listener, socket_identity, runtime))
    })();
    let (mut listener, mut control_socket_identity, runtime) = match setup {
        Ok(value) => value,
        Err(error) => {
            startup.rollback(&error);
            return Err(error);
        }
    };
    startup.disarm();

    let mut accept_retry = ACCEPT_RETRY_INITIAL;
    loop {
        match poll_control_connection(&listener, CONTROL_SOCKET_CHECK_INTERVAL) {
            Ok(Some((stream, _))) => {
                accept_retry = ACCEPT_RETRY_INITIAL;
                let Some(permit) = try_acquire_connection(&runtime.active_connections) else {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    continue;
                };
                let runtime = runtime.clone();
                let spawn = thread::Builder::new()
                    .name("aplexer-client".into())
                    .spawn(move || {
                        let _permit = permit;
                        if let Err(error) = handle_connection(stream, runtime) {
                            eprintln!("aplexer connection: {error:#}");
                        }
                    });
                if let Err(error) = spawn {
                    eprintln!("aplexer worker: spawn client thread: {error}");
                }
            }
            Ok(None) => {
                if !control_socket_matches_identity(&runtime.socket_path, control_socket_identity) {
                    match recover_control_socket(&runtime, worker_lock_identity) {
                        Ok((
                            replacement,
                            replacement_socket_identity,
                            replacement_lock,
                            replacement_lock_identity,
                        )) => {
                            listener = replacement;
                            control_socket_identity = replacement_socket_identity;
                            if let Some(replacement_lock) = replacement_lock {
                                _worker_lock = replacement_lock;
                            }
                            worker_lock_identity = replacement_lock_identity;
                            eprintln!(
                                "aplexer worker: recovered control socket {}",
                                runtime.socket_path.display()
                            );
                        }
                        Err(error) => {
                            eprintln!(
                                "aplexer worker: control socket recovery deferred: {error:#}"
                            );
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if transient_accept_error(&error) => {
                eprintln!(
                    "aplexer worker: transient control accept failure: {error}; retrying in {}ms",
                    accept_retry.as_millis()
                );
                thread::sleep(accept_retry);
                accept_retry = accept_retry.saturating_mul(2).min(ACCEPT_RETRY_MAX);
            }
            Err(error) => return Err(error).context("accept control connection"),
        }
    }
}
