use crate::api::{fence_pre_pid_worker, PrePidFence};
use crate::*;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
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
pub(crate) use procs::direct_child_pids_in;
use procs::*;
mod lifecycle;
use lifecycle::*;
mod spawn;
use spawn::*;
pub(crate) use termination::{disown_child_pid, own_child_pid};

#[derive(Debug, Clone)]
enum OutputEvent {
    Data(Vec<u8>),
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

type FileIdentity = (u64, u64);
type RecoveredControlSocket = (UnixListener, FileIdentity, Option<FileLock>, FileIdentity);

fn startup_checkpoint(point: &str) -> Result<()> {
    if TERMINATION_REQUESTED.load(Ordering::SeqCst) {
        bail!("worker startup cancelled by termination signal");
    }
    #[cfg(feature = "startup-test-hooks")]
    if let Ok(spec) = env::var("APLEXER_TEST_EXIT_WORKER_AT") {
        // "<checkpoint>:<exit status>". Unlike the failure hook below this
        // leaves through `process::exit`, so the worker's own StartupGuard
        // never runs and the durable record keeps whatever non-terminal
        // phase it had. That is the only way to build the two shapes the
        // API's "worker exited during startup" handling must still reject:
        // a worker gone with no terminal record at all, and one gone
        // cleanly (status 0) that never recorded an exit.
        if let Some((target, status)) = spec.split_once(':') {
            if target == point {
                std::process::exit(status.parse().unwrap_or(1));
            }
        }
    }
    #[cfg(feature = "startup-test-hooks")]
    if env::var("APLEXER_TEST_FAIL_WORKER_STARTUP_AT").as_deref() == Ok(point) {
        bail!("injected worker startup failure at {point}");
    }
    #[cfg(not(feature = "startup-test-hooks"))]
    let _ = point;
    Ok(())
}

fn after_workload_spawn_checkpoint(pid: u32) -> Result<()> {
    #[cfg(feature = "startup-test-hooks")]
    if let Some(marker) = env::var_os("APLEXER_TEST_WORKER_STARTUP_MARKER") {
        atomic_write_bytes(std::path::Path::new(&marker), pid.to_string().as_bytes())
            .context("write worker startup test marker")?;
    }
    #[cfg(feature = "startup-test-hooks")]
    if env::var("APLEXER_TEST_HANG_WORKER_STARTUP_AT").as_deref() == Ok("after_workload_spawn") {
        // Deliberately ignore TERMINATION_REQUESTED. The non-default Cargo
        // feature is the authorization boundary for this destructive hook;
        // default and release builds do not contain the hang path.
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    #[cfg(feature = "startup-test-hooks")]
    if env::var("APLEXER_TEST_PAUSE_WORKER_STARTUP_AT").as_deref() == Ok("after_workload_spawn") {
        wait_for_termination_request()?;
    }
    #[cfg(not(feature = "startup-test-hooks"))]
    let _ = pid;
    startup_checkpoint("after_workload_spawn")
}

#[derive(Debug)]
struct WorkloadState {
    running: bool,
    pgid: i32,
}

#[derive(Debug, Clone, Copy)]
struct AttachedClient {
    geometry: Option<(u16, u16)>,
    last_activity: u64,
}

/// The one PTY has one size, even when several clients are attached. tmux's
/// default `window-size=latest` policy resolves that by giving the most
/// recently active sized client control of the PTY geometry. Keep the client
/// registry and the applied size behind the same mutex so two clients cannot
/// race an older resize past a newer activity event.
struct TerminalState {
    rows: u16,
    cols: u16,
    clients: HashMap<u64, AttachedClient>,
    next_client_id: u64,
    activity_clock: u64,
}

struct WorkerRuntime {
    paths: Paths,
    record_path: std::path::PathBuf,
    runtime_session_dir: std::path::PathBuf,
    socket_path: std::path::PathBuf,
    record: Mutex<SessionRecord>,
    /// The PTY master's write side. `None` once the lifecycle sees EOF.
    /// Held as an `Arc` so `send` can clone the handle and write *outside*
    /// the mutex: a PTY write blocks whenever the tty input queue is full
    /// behind a stopped foreground job, and holding the lock across it
    /// used to block `Status` (foreground_command needs the fd), every
    /// resize, and the lifecycle's PtyEof handler behind one wedged client.
    pty_write: Mutex<Option<Arc<File>>>,
    workload: Mutex<WorkloadState>,
    terminal: Mutex<TerminalState>,
    cgroup: Mutex<Option<Cgroup>>,
    kill_gate: Mutex<()>,
    output: OutputHub,
    /// Most recent failure to durably write the session record. Kept live so
    /// status remains truthful while the lifecycle retries final evidence.
    record_persistence_error: Mutex<Option<String>>,
    /// Connections currently being served; the lifecycle thread drains this
    /// (with a timeout) before exiting the worker so in-flight responses
    /// (e.g. the reply to the `kill` that ended the workload) are not lost.
    active_connections: Arc<AtomicUsize>,
    /// Last PTY-output timestamp (ms since epoch), updated on every PTY read
    /// with a single relaxed atomic store -- no lock, no I/O -- so this can
    /// sit directly in the hot PTY-reader loop without reintroducing the
    /// per-read write amplification the history-persistence debounce fix
    /// (see HISTORY_FLUSH_INTERVAL) already solved once. The periodic flush
    /// thread piggybacks on that same tick to persist this into
    /// `SessionRecord::last_activity_ms`, and only when it actually changed.
    last_activity_ms: AtomicU64,
}

impl WorkerRuntime {
    fn record(&self) -> Result<SessionRecord> {
        Ok(lock(&self.record)?.clone())
    }
    fn update_record<F>(&self, update: F) -> Result<SessionRecord>
    where
        F: FnOnce(&mut SessionRecord),
    {
        let mut record = lock(&self.record)?;
        // Checked under the record lock (see `OutputHub::finalized`): a
        // write that got here first has already landed before the removal,
        // and one that gets here later must not recreate the state dir.
        if self.output.finalized() {
            bail!(
                "session {} is finalized; its durable record has been removed",
                record.id
            );
        }
        // Persist a candidate before publishing it. Otherwise a failed Rename
        // can leak into live Status and an unrelated later activity write can
        // commit that rejected selector outside the registry lock.
        let mut candidate = record.clone();
        update(&mut candidate);
        candidate.updated_at_ms = now_ms();
        match atomic_write_json(&self.record_path, &candidate) {
            Ok(()) => {
                *record = candidate.clone();
                *lock(&self.record_persistence_error)? = None;
                Ok(candidate)
            }
            Err(error) => {
                *lock(&self.record_persistence_error)? = Some(format!("{error:#}"));
                Err(error)
            }
        }
    }
    /// Refuse every later durable write (see `OutputHub::finalized`) and
    /// return the session id the caller removes the state dir under. Both
    /// locks are held while the flag is set so a writer that already holds
    /// either one finishes before the flag is observed, and any later
    /// writer observes it.
    fn mark_finalized(&self) -> Result<Uuid> {
        let _hub = lock(&self.output.inner)?;
        let record = lock(&self.record)?;
        self.output.finalized.store(true, Ordering::SeqCst);
        Ok(record.id)
    }
    fn send(&self, data: &[u8]) -> Result<()> {
        if !lock(&self.workload)?.running {
            bail!("workload has exited");
        }
        let file = lock(&self.pty_write)?
            .clone()
            .ok_or_else(|| anyhow!("PTY is closed"))?;
        // Unlocked: see `pty_write`.
        (&*file).write_all(data).context("write PTY")?;
        (&*file).flush()?;
        Ok(())
    }
    /// Apply a size while `terminal` is held. The shared state check avoids
    /// sending SIGWINCH for every keystroke from the already-active client.
    fn apply_size(&self, terminal: &mut TerminalState, rows: u16, cols: u16) -> Result<()> {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if (terminal.rows, terminal.cols) == (rows, cols) {
            return Ok(());
        }
        let pty = lock(&self.pty_write)?;
        let file = pty.as_ref().ok_or_else(|| anyhow!("PTY is closed"))?;
        let previous_size = (terminal.rows, terminal.cols);
        resize_screen_and_pty(&self.output, previous_size, (rows, cols), || {
            set_winsize(file.as_raw_fd(), rows, cols)
        })?;
        terminal.rows = rows;
        terminal.cols = cols;
        Ok(())
    }

    /// Resizes the live screen model *before* the PTY ioctl (design doc
    /// section 5.3): output already in flight at the old size is parsed at
    /// the new one -- a transient tmux shares too -- but this ordering
    /// means a subsequent attach's snapshot is never rendered against a
    /// model that's still the wrong shape for the geometry the workload was
    /// just told about.
    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        let mut terminal = lock(&self.terminal)?;
        self.apply_size(&mut terminal, rows, cols)
    }

    fn bump_client_activity(terminal: &mut TerminalState, client_id: u64) -> Option<(u16, u16)> {
        let geometry = terminal.clients.get(&client_id)?.geometry;
        if geometry.is_some() {
            terminal.activity_clock = terminal.activity_clock.saturating_add(1);
            if let Some(client) = terminal.clients.get_mut(&client_id) {
                client.last_activity = terminal.activity_clock;
            }
        }
        geometry
    }

    /// Registers a subscriber and renders its initial payload while holding
    /// the client-size mutex. This makes geometry selection + snapshot one
    /// indivisible operation with respect to another client's attach/input/
    /// resize, instead of allowing a concurrent client to change the model's
    /// dimensions between those two steps.
    fn attach_client(
        &self,
        payload: AttachPayload,
        geometry: Option<(u16, u16)>,
    ) -> Result<(u64, u64, Vec<u8>, OutputReceiver)> {
        let mut terminal = lock(&self.terminal)?;
        let previous_size = (terminal.rows, terminal.cols);
        let client_id = terminal.next_client_id;
        terminal.next_client_id += 1;
        terminal.clients.insert(
            client_id,
            AttachedClient {
                geometry,
                last_activity: 0,
            },
        );
        if let Some((rows, cols)) = Self::bump_client_activity(&mut terminal, client_id) {
            // Attaching a real terminal makes it the latest active client,
            // matching tmux. Keep attach best-effort if the PTY is exiting.
            let _ = self.apply_size(&mut terminal, rows, cols);
        }
        match self.output.subscribe(payload) {
            Ok((subscription, initial, rx)) => Ok((client_id, subscription, initial, rx)),
            Err(error) => {
                terminal.clients.remove(&client_id);
                // The attach was never established, so it must not retain
                // ownership of the shared PTY geometry. No other client can
                // race us while `terminal` is held.
                let _ = self.apply_size(&mut terminal, previous_size.0, previous_size.1);
                Err(error)
            }
        }
    }

    /// Input activity transfers size ownership before the bytes reach the
    /// workload. Both operations happen under `terminal`, so another client
    /// cannot slip its resize between this client's activation and input.
    fn send_from_client(&self, client_id: u64, data: &[u8]) -> Result<()> {
        let mut terminal = lock(&self.terminal)?;
        if let Some((rows, cols)) = Self::bump_client_activity(&mut terminal, client_id) {
            // Preserve input delivery if a closing/broken PTY rejects the
            // best-effort geometry update; `send` below remains the source
            // of truth for whether the workload can still accept input.
            let _ = self.apply_size(&mut terminal, rows, cols);
        }
        // PTY writes can block behind a stopped or backpressured workload.
        // Geometry ownership is settled above; never hold the global client
        // registry mutex while waiting for the workload to consume input.
        drop(terminal);
        self.send(data)
    }

    fn resize_client(&self, client_id: u64, rows: u16, cols: u16) -> Result<()> {
        let (rows, cols) = screen::validate_size(rows, cols)?;
        let mut terminal = lock(&self.terminal)?;
        let previous_activity_clock = terminal.activity_clock;
        let client = terminal
            .clients
            .get_mut(&client_id)
            .ok_or_else(|| anyhow!("attached client is gone"))?;
        let previous_client = *client;
        client.geometry = Some((rows, cols));
        let (rows, cols) = Self::bump_client_activity(&mut terminal, client_id)
            .ok_or_else(|| anyhow!("attached client has no geometry"))?;
        if let Err(error) = self.apply_size(&mut terminal, rows, cols) {
            terminal.activity_clock = previous_activity_clock;
            if let Some(client) = terminal.clients.get_mut(&client_id) {
                *client = previous_client;
            }
            return Err(error);
        }
        Ok(())
    }

    fn signal_from_client(&self, client_id: u64, signal: i32) -> Result<()> {
        let mut terminal = lock(&self.terminal)?;
        if let Some((rows, cols)) = Self::bump_client_activity(&mut terminal, client_id) {
            let _ = self.apply_size(&mut terminal, rows, cols);
        }
        self.signal(signal)
    }

    /// If the latest client leaves, fall back to the most recently active
    /// remaining sized client. If no sized client remains, keep the current
    /// PTY size, as tmux does for a detached window.
    fn detach_client(&self, client_id: u64) {
        let Ok(mut terminal) = self.terminal.lock() else {
            return;
        };
        let latest_before = terminal
            .clients
            .iter()
            .filter(|(_, client)| client.geometry.is_some())
            .max_by_key(|(_, client)| client.last_activity)
            .map(|(&id, _)| id);
        terminal.clients.remove(&client_id);
        if latest_before != Some(client_id) {
            return;
        }
        let fallback = terminal
            .clients
            .values()
            .filter_map(|client| {
                client
                    .geometry
                    .map(|geometry| (client.last_activity, geometry))
            })
            .max_by_key(|(activity, _)| *activity)
            .map(|(_, geometry)| geometry);
        if let Some((rows, cols)) = fallback {
            let _ = self.apply_size(&mut terminal, rows, cols);
        }
    }
    fn signal(&self, signal: i32) -> Result<()> {
        let workload = lock(&self.workload)?;
        if !workload.running {
            bail!("workload has exited");
        }
        if unsafe { libc::kill(-workload.pgid, signal) } != 0 {
            return Err(io::Error::last_os_error()).context("signal process group");
        }
        Ok(())
    }
    fn kill(&self, signal: i32, grace_ms: u64) -> Result<()> {
        let grace = kill_grace_duration(grace_ms)?;
        let _serialized = lock(&self.kill_gate)?;
        if !self.workload_populated()? {
            // Already empty: no teardown will start from this call, and the
            // lifecycle thread owns the record from here (its ChildExit
            // handler writes `Exiting`, issue #18). Writing again here would
            // race that finalization's record removal and could resurrect a
            // removed state dir (atomic_write_json recreates parents).
            return Ok(());
        }
        // The kill is accepted and teardown is about to start: persist the
        // transition BEFORE signalling (issue #18). Until now the durable
        // record kept its pre-kill phase, so for the whole window between
        // the accepted Kill RPC and finalization's record removal -- bounded
        // by the CLI's KILL_RECORD_REMOVAL_WAIT, indefinite if finalization
        // wedges -- `a snapshot` reported the dying session byte-for-byte
        // like a healthy one. Writing under the same record mutex
        // `update_record` holds, and strictly before the signal that leads
        // to the ChildExit -> finalization sequence, this write is always
        // first: the removal that follows can only delete it, never be
        // preceded by it. Best-effort on purpose -- a failed write must not
        // stop a kill or flip the accepted RPC to an error; the next writer
        // (ChildExit handler, finalization) retries the transition or
        // removes the record outright, and `record_persistence_error` keeps
        // surfacing the failure meanwhile. The termination monitor's
        // SIGTERM teardown shares this method, which is the same dying
        // fact about that session and gets the same honest record.
        if let Err(error) = self.update_record(|record| record.phase = Phase::Exiting) {
            eprintln!("aplexer worker: mark accepted-kill session exiting: {error:#}");
        }
        let grace_deadline = Instant::now()
            .checked_add(grace)
            .ok_or_else(|| anyhow!("kill grace deadline overflow"))?;
        let cleanup_deadline = grace_deadline
            .checked_add(DESCENDANT_KILL_TIMEOUT)
            .ok_or_else(|| anyhow!("kill cleanup deadline overflow"))?;
        let cgroup = lock(&self.cgroup)?.clone();
        if signal == libc::SIGKILL {
            if let Some(cg) = &cgroup {
                cg.kill_all_until(cleanup_deadline)?;
            } else {
                kill_descendants(std::process::id(), DESCENDANT_KILL_TIMEOUT)?;
            }
            return Ok(());
        }
        if let Some(cg) = &cgroup {
            cg.signal_all_until(signal, cleanup_deadline)?;
        } else {
            signal_descendants(std::process::id(), signal)?;
        }
        // Poll instead of sleeping the whole grace period: once the workload
        // is gone there is nothing to escalate to SIGKILL, and the response
        // to this request should not be delayed (the worker exits shortly
        // after the workload does, so a response stuck behind a long sleep
        // could be lost entirely). Polled at KILL_POLL_INTERVAL (5 ms), not
        // the 25 ms lifecycle cadence, so a workload that dies on the first
        // signal does not pay a quantization delay (benchmark PLAN P0.2).
        while self.workload_populated()? && Instant::now() < grace_deadline {
            thread::sleep(KILL_POLL_INTERVAL);
        }
        if self.workload_populated()? {
            if let Some(cg) = &cgroup {
                cg.kill_all_until(cleanup_deadline)?;
            } else {
                kill_descendants(std::process::id(), DESCENDANT_KILL_TIMEOUT)?;
            }
        }
        Ok(())
    }

    /// Whether any process remains inside this session's containment domain.
    /// A leader exiting is not sufficient: a `setsid` descendant may have
    /// escaped the leader's process group while still belonging to the
    /// session. Limited sessions use the kernel's cgroup membership; ordinary
    /// sessions use the worker's subreaper descendant tree.
    fn workload_populated(&self) -> Result<bool> {
        if let Some(cgroup) = lock(&self.cgroup)?.as_ref() {
            return cgroup.populated();
        }
        Ok(!descendant_pids(std::process::id())?.is_empty())
    }
    /// Rename this session within its workspace (or into a new one).
    ///
    /// The `workspace+tag` claim check answers the same question
    /// `start_session`'s supersede check answers -- "does any record still
    /// own this pair?" -- so it applies the same predicate, `reap_verdict`,
    /// and no third copy (issue #13). `rename` used to refuse on *any*
    /// conflicting record, dead or not, while `a start` reclaimed a pair
    /// held by a dead one: on the same box, the same dead record made one
    /// command succeed and the other fail with "already belongs to session
    /// <uuid>", naming a session `a list` showed as broken and nothing
    /// could attach to.
    ///
    /// What a dead conflict gets from rename is deliberately *not* what
    /// `a start` does to it: reclaiming archives and then deletes the
    /// holder's durable state, and rename is a metadata edit that must
    /// destroy nothing. So rename skips the dead holder and leaves its
    /// record for `a prune`, the routine cleaner of exactly this class.
    /// A live holder keeps its claim, and the refusal names its derived
    /// state and a next step, in `a start`'s own words.
    ///
    /// A pre-PID `Starting` holder is the one "dead"-looking shape that
    /// must still refuse (issue #9): a stub in the spawn-to-worker-lock gap
    /// is a healthy session coming up. Its worker lock is both the detector
    /// and the fence -- held means the session is very much coming up
    /// (refuse), acquired means nothing is behind the stub, and holding the
    /// fence across the update below keeps a worker that has not reached
    /// its acquisition yet from coming up on top of the pair this rename
    /// just handed out.
    fn rename(&self, workspace: std::path::PathBuf, tag: String) -> Result<SessionRecord> {
        validate_tag(&tag)?;
        let workspace = canonical_workspace(&workspace)?;
        let id = lock(&self.record)?.id;
        let _registry = FileLock::exclusive(&self.paths.registry_lock(), false)?;
        let conflicts: Vec<SessionRecord> = list_records(&self.paths)?
            .into_iter()
            .filter(|record| record.id != id && record.workspace == workspace && record.tag == tag)
            .collect();
        // Every conflict must be reclaimable-dead, or the rename refuses:
        // a live holder -- live worker, live workload leader, or a
        // containment domain that still holds something -- owns its pair,
        // exactly as against `a start`.
        if let Some(live) = conflicts
            .iter()
            .find(|record| reap_verdict(record).is_none())
        {
            bail!(
                "workspace+tag already belongs to session {} (state: {}); rename it or choose a different tag",
                live.id,
                live.observed_state()
            );
        }
        // All conflicts are dead by the same verdict `a start` reclaims on.
        // Fence each pre-PID stub before skipping it, and hold the fences
        // across the update: the skip is only safe while nothing can come
        // up behind the stub.
        let _fences = conflicts
            .iter()
            .map(|record| {
                match fence_pre_pid_worker(&self.paths, record).with_context(|| {
                    format!("cannot fence session {}'s pre-PID worker", record.id)
                })? {
                    PrePidFence::Fenced(lock) => Ok(lock),
                    PrePidFence::WorkerHoldsLock(lock_path) => bail!(
                        "workspace+tag already belongs to session {}, whose worker still holds {}; rename it or choose a different tag",
                        record.id,
                        lock_path.display()
                    ),
                }
            })
            .collect::<Result<Vec<Option<FileLock>>>>()?;
        self.update_record(|r| {
            r.workspace = workspace;
            r.tag = tag;
        })
    }
    /// `a state-report <state>` (docs/pocketshell-integration-plan.md Open
    /// question #2): a hook running inside this session pushes its own
    /// semantic state. Validated here (not just at the CLI's `ValueEnum`
    /// layer) so a direct/malformed RPC from any caller can't write an
    /// unrecognised value into the record that `watch.rs`'s merge logic
    /// would then have to guess at -- the same defensive posture `rename`
    /// takes with `validate_tag` above.
    fn report_state(&self, state: String) -> Result<SessionRecord> {
        validate_reported_state(&state)?;
        self.update_record(move |r| {
            r.reported_state = Some(state);
            r.reported_state_at_ms = Some(now_ms());
        })
    }
}

/// Keep the rendered model and kernel PTY geometry transactional. The model
/// must be resized first so concurrent output is parsed at the intended new
/// dimensions, but a rejected ioctl must not leave future snapshots claiming
/// a size the workload never received.
fn resize_screen_and_pty(
    output: &OutputHub,
    previous_size: (u16, u16),
    new_size: (u16, u16),
    resize_pty: impl FnOnce() -> Result<()>,
) -> Result<()> {
    output.set_size(new_size.0, new_size.1)?;
    if let Err(error) = resize_pty() {
        if let Err(rollback_error) = output.set_size(previous_size.0, previous_size.1) {
            eprintln!(
                "aplexer worker: roll back screen after PTY resize failure: {rollback_error:#}"
            );
        }
        return Err(error);
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| anyhow!("worker lock poisoned"))
}

fn record_history_persistence_error(inner: &mut HubInner, error: impl std::fmt::Display) -> String {
    let message = format!("{error:#}");
    inner.history_persistence_error = Some(message.clone());
    inner.history_retry_at = Instant::now() + inner.history_retry_delay;
    inner.history_retry_delay = inner
        .history_retry_delay
        .saturating_mul(2)
        .min(HISTORY_RETRY_MAX);
    message
}

/// Owns every resource created before the worker's accept loop is committed.
/// Drop is a last-resort rollback; normal error paths call `rollback` so the
/// persisted failure contains the original error rather than a generic one.
struct StartupGuard {
    armed: bool,
    record_path: std::path::PathBuf,
    runtime_session_dir: std::path::PathBuf,
    socket_path: std::path::PathBuf,
    failure_record: SessionRecord,
    cgroup: Option<Cgroup>,
    cgroup_setup_started: bool,
    child: Option<Arc<Mutex<Option<Child>>>>,
}

impl StartupGuard {
    fn new(paths: &Paths, record: &SessionRecord) -> Self {
        Self {
            armed: true,
            record_path: paths.record(record.id),
            runtime_session_dir: paths.runtime_session(record.id),
            socket_path: paths.socket(record.id),
            failure_record: record.clone(),
            cgroup: None,
            cgroup_setup_started: false,
            child: None,
        }
    }

    fn rollback(&mut self, error: &anyhow::Error) {
        self.cleanup(format!("{error:#}"));
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.child = None;
        self.cgroup = None;
    }

    fn cleanup(&mut self, message: String) {
        if !self.armed {
            return;
        }
        self.armed = false;

        let mut cleanup_failures = Vec::new();
        let deadline = Instant::now() + DESCENDANT_KILL_TIMEOUT;
        if let Some(cgroup) = &self.cgroup {
            if let Err(error) = cgroup.kill_all_until(deadline) {
                cleanup_failures.push(format!("kill startup cgroup: {error:#}"));
            }
        } else if let Err(error) = signal_descendants(std::process::id(), libc::SIGKILL) {
            cleanup_failures.push(format!("kill startup descendants: {error:#}"));
        }

        if let Some(slot) = &self.child {
            match slot.lock() {
                Ok(mut slot) => {
                    if let Some(mut child) = slot.take() {
                        if let Err(error) = child.kill() {
                            if error.kind() != io::ErrorKind::InvalidInput {
                                cleanup_failures
                                    .push(format!("kill startup workload leader: {error}"));
                            }
                        }
                        if let Err(error) = child.wait() {
                            cleanup_failures.push(format!("reap startup workload leader: {error}"));
                        }
                    }
                }
                Err(_) => cleanup_failures.push("startup child lock poisoned".into()),
            }
        }

        // Once the tracked leader has been waited, every remaining process
        // is an adopted child and may safely be reaped here. Repeat signaling
        // to close the final fork-vs-scan window without confusing zombies
        // for live processes.
        loop {
            if let Err(error) = reap_adopted_children() {
                cleanup_failures.push(format!("reap startup descendants: {error:#}"));
            }
            match descendant_pids(std::process::id()) {
                Ok(remaining) if remaining.is_empty() => break,
                Ok(_) if Instant::now() >= deadline => {
                    cleanup_failures.push("timed out proving startup descendants empty".into());
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    cleanup_failures.push(format!("inspect startup descendants: {error:#}"));
                    break;
                }
            }
            if let Err(error) = signal_descendants(std::process::id(), libc::SIGKILL) {
                cleanup_failures.push(format!("kill remaining startup descendants: {error:#}"));
                break;
            }
            thread::sleep(DESCENDANT_POLL_INTERVAL);
        }

        if let Some(cgroup) = &self.cgroup {
            loop {
                match cgroup.populated() {
                    Ok(false) => break,
                    Ok(true) if Instant::now() >= deadline => {
                        cleanup_failures.push("timed out proving startup cgroup empty".into());
                        break;
                    }
                    Ok(true) => thread::sleep(DESCENDANT_POLL_INTERVAL),
                    Err(error) => {
                        cleanup_failures.push(format!("inspect startup cgroup: {error:#}"));
                        break;
                    }
                }
            }
        }
        if self.cgroup_setup_started && self.cgroup.is_none() {
            cleanup_failures.push(
                "cgroup setup spawned a helper but no authoritative locator was recorded".into(),
            );
        }

        self.failure_record.phase = Phase::Failed;
        self.failure_record.containment_empty = Some(cleanup_failures.is_empty());
        self.failure_record.error = Some(if cleanup_failures.is_empty() {
            message
        } else {
            format!(
                "{message}; containment cleanup unproven: {}",
                cleanup_failures.join("; ")
            )
        });
        self.failure_record.updated_at_ms = now_ms();
        match atomic_write_json(&self.record_path, &self.failure_record) {
            Ok(()) if self.failure_record.containment_empty == Some(true) => {
                if let Some(cgroup) = self.cgroup.take() {
                    cgroup.cleanup();
                }
                let _ = fs::remove_file(&self.socket_path);
                let _ = fs::remove_dir_all(&self.runtime_session_dir);
            }
            Ok(()) => {}
            Err(error) => eprintln!("aplexer worker: persist startup rollback: {error:#}"),
        }
    }
}

impl Drop for StartupGuard {
    fn drop(&mut self) {
        self.cleanup("worker startup aborted before commit".into());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ThreadStart {
    Pending,
    Run,
    Abort,
}

type ThreadStartGate = Arc<(Mutex<ThreadStart>, Condvar)>;

fn await_thread_start(gate: &ThreadStartGate) -> bool {
    let (state, ready) = &**gate;
    let Ok(mut state) = state.lock() else {
        return false;
    };
    while *state == ThreadStart::Pending {
        let Ok(next) = ready.wait(state) else {
            return false;
        };
        state = next;
    }
    *state == ThreadStart::Run
}

fn release_startup_threads(gate: &ThreadStartGate, decision: ThreadStart) {
    let (state, ready) = &**gate;
    if let Ok(mut state) = state.lock() {
        *state = decision;
        ready.notify_all();
    }
}

fn load_launch_environment(
    path: &std::path::Path,
    legacy: LaunchEnvironment,
) -> Result<LaunchEnvironment> {
    match fs::read(path) {
        Ok(bytes) => {
            let bytes = SecretBytes(bytes);
            let environment = serde_json::from_slice(&bytes.0)
                .with_context(|| format!("parse private launch environment {}", path.display()))?;
            // Keeping a readable secret file after consumption is not a
            // recoverable warning. Fail startup so the transaction removes
            // the whole private runtime directory.
            fs::remove_file(path).with_context(|| {
                format!("remove consumed launch environment {}", path.display())
            })?;
            Ok(LaunchEnvironment(environment))
        }
        // Compatibility for sessions created by an older client, whose
        // launch values were stored directly in the record.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(legacy),
        Err(error) => Err(error)
            .with_context(|| format!("read private launch environment {}", path.display())),
    }
}

/// The durable record, read only once the worker lock is held.
///
/// Every destroyer of a pre-PID session (`a forget`, `a prune`, `a start`'s
/// reclaim) fences the worker through that lock and removes the durable
/// state while holding it. A record read *before* the lock could therefore
/// be stale: the destroyer unlinks the runtime dir, this worker recreates
/// it and acquires a fresh lock inode unopposed, and its first record
/// write resurrects a session that had just been forgotten. Read after the
/// lock, a record that is gone means the session no longer exists -- fail
/// the start and take the runtime dir this lock lives in back out, so the
/// refusal leaves nothing behind either.
fn read_record_under_worker_lock(paths: &Paths, id: Uuid) -> Result<SessionRecord> {
    match read_session_record(paths, id) {
        Ok(record) => Ok(record),
        Err(error) => {
            if crate::registry::record_is_not_written_yet(&error) {
                let _ = fs::remove_dir_all(paths.runtime_session(id));
            }
            Err(error).with_context(|| {
                format!("session {id} has no durable record; refusing to start its worker")
            })
        }
    }
}

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

fn poll_control_connection(
    listener: &UnixListener,
    timeout: Duration,
) -> io::Result<Option<(UnixStream, std::os::unix::net::SocketAddr)>> {
    let mut poll_fd = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        // SA_RESTART does not restart `poll(2)`, so the worker's SIGCHLD
        // wakeup lands here as EINTR. That is an early return from this
        // interval, not a listener fault: report it as "nothing accepted"
        // so the accept loop takes its normal idle branch instead of
        // logging an error and backing off.
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(error);
    }
    if ready == 0 {
        return Ok(None);
    }
    if poll_fd.revents & libc::POLLNVAL != 0 {
        return Err(io::Error::from_raw_os_error(libc::EBADF));
    }
    if poll_fd.revents & libc::POLLIN == 0 {
        return Ok(None);
    }
    listener.accept().map(Some)
}

fn control_socket_matches_identity(path: &std::path::Path, identity: (u64, u64)) -> bool {
    trusted_socket_identity(path).is_ok_and(|current| current == identity)
}

/// The filesystem socket node and the open listener descriptor do not share
/// an inode on Linux. Capture the pathname's identity immediately after bind
/// and compare later pathname metadata against that stable identity instead
/// of comparing `lstat(path)` with `fstat(listener)` (which always differs and
/// caused an unnecessary rebind every idle health-check interval).
fn trusted_socket_identity(path: &std::path::Path) -> Result<(u64, u64)> {
    let Ok(path_metadata) = fs::symlink_metadata(path) else {
        bail!("control socket path is missing");
    };
    if !path_metadata.file_type().is_socket()
        || path_metadata.uid() != unsafe { libc::geteuid() }
        || path_metadata.permissions().mode() & 0o777 != 0o600
    {
        bail!("control socket path is not a trusted private socket");
    }
    Ok((path_metadata.dev(), path_metadata.ino()))
}

/// Recreate reachability metadata after cleanup software removes a live
/// worker's private runtime session directory. Durable PID identity is the
/// trust anchor: never recreate runtime artifacts if it has disappeared or
/// no longer proves that this process is the recorded worker.
fn trusted_lock_identity(path: &std::path::Path) -> Result<(u64, u64)> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect worker lock {}", path.display()))?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        bail!(
            "worker lock {} is not a trusted private file",
            path.display()
        );
    }
    Ok((metadata.dev(), metadata.ino()))
}

fn recover_control_socket(
    runtime: &WorkerRuntime,
    held_lock_identity: (u64, u64),
) -> Result<RecoveredControlSocket> {
    let record = read_record(&runtime.record_path).context("read durable record for recovery")?;
    if record.worker_pid != Some(std::process::id()) {
        bail!("durable record does not identify this worker");
    }
    signal_recorded_worker(&record, 0).context("validate durable worker identity")?;

    ensure_private_dir(&runtime.runtime_session_dir)?;
    let lock_path = runtime.paths.worker_lock(record.id);
    let current_lock_identity = match trusted_lock_identity(&lock_path) {
        Ok(identity) => Some(identity),
        Err(error)
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let (replacement_lock, lock_identity) = if current_lock_identity == Some(held_lock_identity) {
        (None, held_lock_identity)
    } else {
        let lock =
            FileLock::exclusive(&lock_path, true).context("reacquire recovered worker lock")?;
        let identity = trusted_lock_identity(&lock_path)?;
        (Some(lock), identity)
    };
    match fs::symlink_metadata(&runtime.socket_path) {
        Ok(metadata)
            if metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() } =>
        {
            fs::remove_file(&runtime.socket_path).context("remove displaced control socket")?;
        }
        Ok(_) => bail!(
            "refusing to replace untrusted control socket path {}",
            runtime.socket_path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect control socket path for recovery"),
    }
    let listener = UnixListener::bind(&runtime.socket_path)
        .with_context(|| format!("rebind {}", runtime.socket_path.display()))?;
    if let Err(error) = fs::set_permissions(&runtime.socket_path, fs::Permissions::from_mode(0o600))
    {
        let _ = fs::remove_file(&runtime.socket_path);
        return Err(error).context("secure recovered control socket");
    }
    let socket_identity = trusted_socket_identity(&runtime.socket_path)?;
    Ok((listener, socket_identity, replacement_lock, lock_identity))
}

fn handle_connection(mut stream: UnixStream, runtime: Arc<WorkerRuntime>) -> Result<()> {
    stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
    let uid = peer_uid(stream.as_raw_fd())?;
    if uid != unsafe { libc::geteuid() } {
        bail!("peer uid {uid} is not authorized");
    }
    let frame = read_frame(&mut stream)?.ok_or_else(|| anyhow!("empty request"))?;
    let request: Request = frame_json(frame)?;
    if request.version != PROTOCOL_VERSION {
        write_json(
            &mut stream,
            &Response::error(request.request_id, "unsupported protocol version"),
        )?;
        return Ok(());
    }
    let id = request.request_id.clone();
    let worker_session_id = runtime.record()?.id;
    match request.session_id {
        Some(expected) if expected == worker_session_id => {}
        Some(expected) => {
            write_json(
                &mut stream,
                &Response::error(
                    id,
                    format!(
                        "request targets session {expected}, but this worker owns {worker_session_id}"
                    ),
                ),
            )?;
            return Ok(());
        }
        None => {
            write_json(
                &mut stream,
                &Response::error(id, "request omitted session_id; upgrade the aplexer client"),
            )?;
            return Ok(());
        }
    }
    match request.operation {
        Operation::Ping => write_json(
            &mut stream,
            &Response::ok(id, json!({"pong":true,"id":worker_session_id})),
        )?,
        Operation::Status => {
            let mut value = serde_json::to_value(public_session_record(&runtime.record()?))?;
            if let Some(error) = runtime.output.history_persistence_error() {
                value["history_persistence_error"] = json!(error);
            }
            if let Some(error) = lock(&runtime.record_persistence_error)?.clone() {
                value["record_persistence_error"] = json!(error);
            }
            if let Some(cgroup) = lock(&runtime.cgroup)?.as_ref() {
                value["cgroup"] = cgroup.stats();
            }
            // Live-only, never persisted (see foreground_command's doc
            // comment on WorkerRuntime -- deliberately not a SessionRecord
            // field): what's actually in the foreground of the pty right
            // now, which can differ from `engine`/`command` the moment the
            // workload execs or forks something new (e.g. a plain `shell`
            // session where the user manually ran another program). Merged
            // into the Status response the same way `cgroup` is above,
            // rather than added to the persisted record, so this never
            // costs a disk write and an old client's `serde_json` simply
            // ignores the unrecognized field.
            if let Some(fd) = lock(&runtime.pty_write)?.as_ref().map(|f| f.as_raw_fd()) {
                if let Some(cmd) = foreground_command(fd) {
                    value["foreground_command"] = json!(cmd);
                }
            }
            write_json(&mut stream, &Response::ok(id, value))?;
        }
        Operation::Send { bytes } => {
            let next = read_frame(&mut stream)?.ok_or_else(|| anyhow!("missing data frame"))?;
            if next.kind != FrameKind::Data || next.payload.len() != bytes {
                write_json(&mut stream, &Response::error(id, "data length mismatch"))?;
            } else {
                match runtime.send(&next.payload) {
                    Ok(()) => write_json(&mut stream, &Response::ok(id, json!({"bytes":bytes})))?,
                    Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
                }
            }
        }
        Operation::Capture { max_bytes } => {
            let data = runtime.output.snapshot(max_bytes)?;
            write_json(&mut stream, &Response::ok(id, json!({"bytes":data.len()})))?;
            write_frame(&mut stream, FrameKind::Data, &data)?;
        }
        Operation::CaptureScreen { plain } => {
            // Mirrors Operation::Capture's response+Data shape exactly
            // (design doc section 8) -- just a different source for the
            // bytes: the rendered current-screen snapshot, or its
            // plain-text contents.
            let data = if plain {
                runtime.output.screen_contents()?.into_bytes()
            } else {
                runtime.output.screen_snapshot()?
            };
            write_json(&mut stream, &Response::ok(id, json!({"bytes":data.len()})))?;
            write_frame(&mut stream, FrameKind::Data, &data)?;
        }
        Operation::Attach {
            history_bytes,
            want_screen,
            rows,
            cols,
        } => handle_attach(stream, runtime, id, history_bytes, want_screen, rows, cols)?,
        Operation::Resize { rows, cols } => match runtime.resize(rows, cols) {
            Ok(()) => write_json(&mut stream, &Response::ok(id, json!({})))?,
            Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
        },
        Operation::Kill { signal, grace_ms } => match runtime.kill(signal, grace_ms) {
            Ok(()) => {
                // Accepted before the response is written: from here the
                // lifecycle finalization owns removing this session's
                // durable record (see run_lifecycle), so `a kill` leaves
                // nothing behind in `a list`. The record also already says
                // `phase: exiting` by this point -- `runtime.kill` persists
                // that before teardown, so a client that gets this ok and
                // immediately snapshots sees a dying session, never the
                // pre-kill phase (issue #18). A failed kill never reaches
                // that path -- the record stays as evidence for the
                // client-side recovery paths.
                write_json(&mut stream, &Response::ok(id, json!({"signalled":true})))?
            }
            Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
        },
        Operation::Rename { workspace, tag } => match runtime.rename(workspace, tag) {
            Ok(record) => write_json(
                &mut stream,
                &Response::ok(id, serde_json::to_value(record)?),
            )?,
            Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
        },
        Operation::ReportState { state } => match runtime.report_state(state) {
            Ok(record) => write_json(
                &mut stream,
                &Response::ok(id, serde_json::to_value(record)?),
            )?,
            Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
        },
    }
    Ok(())
}

/// Everything an attach needs before its handshake can be answered: the
/// validated geometry, the hub subscription (already wrapped in its
/// cleanup guard, so a later failure here releases it), and the writer
/// half of the socket. Kept as one fallible step so `handle_attach` can
/// turn any refusal into a `Response::error` the client actually sees --
/// a bare early `?` here used to close the socket before any response
/// frame, and every cause ("too many attached clients", an oversized
/// geometry, a closed PTY) reached the client as "missing attach response".
fn establish_attach(
    runtime: &Arc<WorkerRuntime>,
    reader: &UnixStream,
    history_bytes: Option<usize>,
    want_screen: bool,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<(AttachGuard, Vec<u8>, OutputReceiver, UnixStream)> {
    let geometry = match (rows, cols) {
        (Some(rows), Some(cols)) => Some(screen::validate_size(rows, cols)?),
        _ => None,
    };
    // Geometry-first (design doc section 6.1): resize the PTY and the
    // screen model to the client's real terminal size *before* rendering
    // the snapshot below, so there is no wrong-size frame followed by a
    // SIGWINCH repaint. `WorkerRuntime::resize` itself resizes the model
    // before the ioctl (section 5.3), so this one call gets both in the
    // right order. Best-effort: a resize failure here (e.g. the PTY is
    // already closing) must not block the attach -- the pre-existing
    // client-side post-connect Resize control frame remains the fallback
    // (section 6.3 step 7).
    let payload = if want_screen {
        AttachPayload::Screen
    } else {
        AttachPayload::Tail(history_bytes)
    };
    let (client_id, subscription, initial, rx) = runtime.attach_client(payload, geometry)?;
    let guard = AttachGuard {
        runtime: Arc::clone(runtime),
        client_id,
        subscription,
    };
    let writer = reader
        .try_clone()
        .context("clone attach socket for output")?;
    Ok((guard, initial, rx, writer))
}

fn handle_attach(
    mut reader: UnixStream,
    runtime: Arc<WorkerRuntime>,
    request_id: String,
    history_bytes: Option<usize>,
    want_screen: bool,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<()> {
    let (attach_guard, initial, rx, writer_stream) =
        match establish_attach(&runtime, &reader, history_bytes, want_screen, rows, cols) {
            Ok(established) => established,
            Err(error) => {
                // Still under the handshake's worker-wide write deadline, so
                // a peer that stopped reading cannot hold its slot with this.
                write_json(
                    &mut reader,
                    &Response::error(request_id, format!("{error:#}")),
                )?;
                return Ok(());
            }
        };
    let client_id = attach_guard.client_id;
    let subscription = attach_guard.subscription;
    // An established attach is intentionally long-lived. Before this point,
    // the handshake used the worker-wide deadline so a peer cannot reserve a
    // connection slot forever with a partial frame.
    reader.set_read_timeout(None)?;
    // Best-effort: attach is the "someone looked at this" event used by
    // `a list --sort accessed`. A persist failure must not refuse the
    // attach; the next successful attach (or a later record write that
    // races this one) will stamp it.
    //
    // Throttled to one durable write per minute per session (benchmark PLAN
    // P1.2): the old code fsync'd the record on EVERY attach, putting a
    // 5-20 ms fsync plus its variance directly on the attach handshake's
    // critical path -- the p90 tail the benchmark flagged. Recency sorting
    // only needs coarse granularity, so repeat attaches within the window
    // skip the write entirely after the first stamps it.
    {
        let now = now_ms();
        let stale = runtime
            .record()
            .map(|record| {
                record
                    .last_accessed_ms
                    .is_none_or(|at| now.saturating_sub(at) >= 60_000)
            })
            .unwrap_or(true);
        if stale {
            let _ = runtime.update_record(|record| {
                record.last_accessed_ms = Some(now);
            });
        }
    }
    let _attach_guard = attach_guard;
    let writer = Arc::new(Mutex::new(writer_stream));
    {
        let mut out = lock(&writer)?;
        write_json(
            &mut *out,
            &Response::ok(
                request_id,
                json!({"attached":true,"history_bytes":initial.len(),"screen":want_screen}),
            ),
        )?;
        write_frame(&mut *out, FrameKind::Data, &initial)?;
    }
    // The handshake writes above still run under the worker-wide deadline, so
    // a peer that stops reading mid-handshake cannot hold a connection slot
    // forever. An established attach must not: its client routinely stops
    // reading for a while (a background tab, a slow link, a slept laptop),
    // and coalescing (MAX_SUBSCRIBER_QUEUED_BYTES) exists precisely to make
    // that survivable. But coalescing only bounds the hub queue -- once the
    // socket buffer itself (~200 KB) fills behind a paused client, a residual
    // SO_SNDTIMEO fails the streaming writer's write() after
    // CLIENT_IO_TIMEOUT of zero drain, the worker closes the socket, and the
    // client is silently disconnected the moment its terminal wakes
    // ("Connection to ... lost") -- observed on busy codex sessions, whose
    // continuous TUI repaints fill the buffer fastest. Silence in both
    // directions is a normal state of a long-lived attach; the read deadline
    // below is already cleared for the same reason. A dead client is still
    // detected without it: writes fail with EPIPE/ECONNRESET once the peer's
    // socket closes, and the reader loop sees EOF.
    lock(&writer)?
        .set_write_timeout(None)
        .context("clear attach streaming write deadline")?;
    let output_writer = writer.clone();
    let output_runtime = runtime.clone();
    thread::spawn(move || {
        while let Ok(event) = rx.recv() {
            let result = (|| -> Result<bool> {
                let mut out = lock(&output_writer)?;
                match event {
                    OutputEvent::Data(data) => {
                        write_frame(&mut *out, FrameKind::Data, &data)?;
                        Ok(true)
                    }
                    OutputEvent::Layout(change) => {
                        // Old clients' serde_json::from_slice::<ServerEvent>
                        // would hard-fail on an unrecognized `event` tag --
                        // only forward this to subscribers that opted in by
                        // attaching with want_screen (design doc section
                        // 6.3); drop it otherwise.
                        if want_screen {
                            write_json(
                                &mut *out,
                                &ServerEvent::Layout {
                                    alt_screen: change.alt_screen,
                                    margins_reset: change.margins_reset,
                                    erase_reset: change.erase_reset,
                                },
                            )?;
                        }
                        Ok(true)
                    }
                    OutputEvent::Exit(exit) => {
                        write_json(&mut *out, &ServerEvent::Exit { exit })?;
                        Ok(false)
                    }
                    OutputEvent::Error(message) => {
                        write_json(&mut *out, &ServerEvent::Error { message })?;
                        Ok(false)
                    }
                }
            })();
            if !matches!(result, Ok(true)) {
                break;
            }
        }
        output_runtime.output.unsubscribe(subscription);
        if let Ok(out) = output_writer.lock() {
            let _ = out.shutdown(std::net::Shutdown::Both);
        }
    });
    loop {
        let frame = match read_frame(&mut reader) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => break,
        };
        match frame.kind {
            FrameKind::Data => {
                if runtime.send_from_client(client_id, &frame.payload).is_err() {
                    break;
                }
            }
            FrameKind::End => break,
            FrameKind::Json => {
                let control: AttachControl = serde_json::from_slice(&frame.payload)?;
                match control {
                    AttachControl::Resize { rows, cols } => {
                        let _ = runtime.resize_client(client_id, rows, cols);
                    }
                    AttachControl::Signal { signal } => {
                        let _ = runtime.signal_from_client(client_id, signal);
                    }
                    AttachControl::Detach => break,
                }
            }
        }
    }
    let _ = reader.shutdown(std::net::Shutdown::Both);
    Ok(())
}

/// Ensures every attach exit path (EOF, explicit detach, malformed control
/// frame, or socket error) removes both the output subscription and its
/// geometry entry. `OutputHub::unsubscribe` is idempotent, so it is safe for
/// the writer thread to race this cleanup after a failed write.
struct AttachGuard {
    runtime: Arc<WorkerRuntime>,
    client_id: u64,
    subscription: u64,
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        self.runtime.output.unsubscribe(self.subscription);
        self.runtime.detach_client(self.client_id);
    }
}
