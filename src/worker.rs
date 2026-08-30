use crate::*;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

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

/// Bound memory retained on behalf of clients that stop reading. PTY reads
/// are at most 32 KiB, so this caps queued data at roughly 1 MiB per client
/// (layout events are small). A lagging client is disconnected and can
/// reattach to obtain a fresh tail or screen snapshot.
const SUBSCRIBER_QUEUE_EVENTS: usize = 32;
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
const DESCENDANT_KILL_TIMEOUT: Duration = Duration::from_secs(2);

static TERMINATION_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

extern "C" fn request_worker_termination(_: libc::c_int) {
    TERMINATION_REQUESTED.store(true, Ordering::SeqCst);
}

/// The launcher blocks TERM/INT before exec so no timeout signal can land in
/// the gap before these handlers exist. Install first, then explicitly
/// unblock; a pending signal is delivered to the handler and becomes a normal
/// startup cancellation whose guard can unwind all resources.
fn install_termination_handlers() -> Result<()> {
    TERMINATION_REQUESTED.store(false, Ordering::SeqCst);
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = request_worker_termination as *const () as usize;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in [libc::SIGTERM, libc::SIGINT] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error())
                    .with_context(|| format!("install signal handler {signal}"));
            }
        }
        let mut unblocked: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut unblocked);
        libc::sigaddset(&mut unblocked, libc::SIGTERM);
        libc::sigaddset(&mut unblocked, libc::SIGINT);
        let rc = libc::pthread_sigmask(libc::SIG_UNBLOCK, &unblocked, std::ptr::null_mut());
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc))
                .context("unblock worker termination signals");
        }
    }
    Ok(())
}

fn startup_checkpoint(point: &str) -> Result<()> {
    if TERMINATION_REQUESTED.load(Ordering::SeqCst) {
        bail!("worker startup cancelled by termination signal");
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
        while !TERMINATION_REQUESTED.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(10));
        }
    }
    #[cfg(not(feature = "startup-test-hooks"))]
    let _ = pid;
    startup_checkpoint("after_workload_spawn")
}

struct SecretBytes(Vec<u8>);

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

struct LaunchEnvironment(BTreeMap<String, String>);

impl Drop for LaunchEnvironment {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            // Overwrite the initialized allocation before String drops it.
            // The temporary non-UTF-8 contents are never observed as text.
            unsafe {
                value.as_bytes_mut().fill(0);
            }
            value.clear();
        }
    }
}

struct ConnectionPermit {
    active: Arc<AtomicUsize>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn try_acquire_connection(active: &Arc<AtomicUsize>) -> Option<ConnectionPermit> {
    active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < MAX_CLIENT_CONNECTIONS).then_some(count + 1)
        })
        .ok()?;
    Some(ConnectionPermit {
        active: Arc::clone(active),
    })
}

/// Resource pressure must not tear down the worker: doing so also closes the
/// PTY master and can SIGHUP an otherwise healthy workload. Existing client
/// threads may release descriptors while the listener backs off, after which
/// accepting can resume normally.
fn transient_accept_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOBUFS) | Some(libc::ENOMEM)
    )
}

fn bounded_history_limit(requested: Option<usize>) -> usize {
    requested.unwrap_or(MAX_FRAME_BYTES).min(MAX_FRAME_BYTES)
}

fn ensure_frame_payload_size(kind: &str, len: usize) -> Result<()> {
    if len > MAX_FRAME_BYTES {
        bail!("{kind} exceeds the maximum frame size of {MAX_FRAME_BYTES} bytes");
    }
    Ok(())
}

/// What a newly-attaching client should be sent as its initial payload
/// (design doc section 6.1/checklist item 4): the live screen snapshot, or
/// the historical raw-tail replay old clients (and `--history-bytes`) still
/// get.
enum AttachPayload {
    Screen,
    Tail(Option<usize>),
}

struct HubInner {
    history: History,
    history_persistence_error: Option<String>,
    history_retry_at: Instant,
    history_retry_delay: Duration,
    screen: screen::ScreenTracker,
    subscribers: HashMap<u64, SubscriberSender>,
    next_id: u64,
    terminal: Option<OutputEvent>,
}

struct SubscriberSender {
    events: mpsc::SyncSender<OutputEvent>,
    terminal: mpsc::SyncSender<OutputEvent>,
}

impl SubscriberSender {
    fn try_event(&self, event: OutputEvent) -> bool {
        match self.events.try_send(event) {
            Ok(()) => true,
            Err(mpsc::TrySendError::Full(_)) => {
                let _ = self.terminal.try_send(OutputEvent::Error(
                    "attached client fell behind live output; reattach for a fresh snapshot".into(),
                ));
                false
            }
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        }
    }

    fn terminate(self, event: OutputEvent) {
        let _ = self.terminal.try_send(event);
    }
}

struct OutputReceiver {
    events: mpsc::Receiver<OutputEvent>,
    terminal: mpsc::Receiver<OutputEvent>,
}

impl OutputReceiver {
    /// Drain already-queued output before reporting the terminal outcome.
    /// If no output is ready, a terminal event wins immediately; otherwise
    /// blocking on the data channel is safe because terminal publication also
    /// drops its sender and wakes this receive.
    fn recv(&self) -> Result<OutputEvent, mpsc::RecvError> {
        match self.events.try_recv() {
            Ok(event) => return Ok(event),
            Err(mpsc::TryRecvError::Disconnected) => return self.terminal.recv(),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        match self.terminal.try_recv() {
            Ok(event) => Ok(event),
            Err(mpsc::TryRecvError::Disconnected) => self.events.recv(),
            Err(mpsc::TryRecvError::Empty) => match self.events.recv() {
                Ok(event) => Ok(event),
                Err(_) => self.terminal.recv(),
            },
        }
    }
}

struct OutputHub {
    inner: Mutex<HubInner>,
    /// Where `finish` writes the final plain-text screen on exit (design
    /// doc section 5.5) -- immutable, so no need to route it through the
    /// lock.
    screen_txt_path: std::path::PathBuf,
}
impl OutputHub {
    fn new(
        history: History,
        rows: u16,
        cols: u16,
        screen_txt_path: std::path::PathBuf,
    ) -> Result<Self> {
        Ok(Self {
            inner: Mutex::new(HubInner {
                history,
                history_persistence_error: None,
                history_retry_at: Instant::now(),
                history_retry_delay: HISTORY_RETRY_INITIAL,
                screen: screen::ScreenTracker::try_new(rows, cols)?,
                subscribers: HashMap::new(),
                next_id: 1,
                terminal: None,
            }),
            screen_txt_path,
        })
    }
    fn append(&self, data: &[u8]) -> Result<()> {
        let mut inner = lock(&self.inner)?;
        inner.history.append(data)?;
        let layout = inner.screen.process(data);
        // Ordering matters and is automatic: both sends go through the same
        // per-subscriber mpsc channel under the same lock hold, so a Layout
        // event always arrives after the Data frame that caused it (design
        // doc section 5.1).
        inner
            .subscribers
            .retain(|_, subscriber| subscriber.try_event(OutputEvent::Data(data.to_vec())));
        if let Some(change) = layout {
            inner
                .subscribers
                .retain(|_, subscriber| subscriber.try_event(OutputEvent::Layout(change)));
        }
        Ok(())
    }
    fn snapshot(&self, max: Option<usize>) -> Result<Vec<u8>> {
        Ok(lock(&self.inner)?
            .history
            .snapshot(Some(bounded_history_limit(max))))
    }
    /// The rendered current-screen snapshot (design doc section 6.2),
    /// shared by attach's `AttachPayload::Screen` and
    /// `Operation::CaptureScreen { plain: false }`.
    fn screen_snapshot(&self) -> Result<Vec<u8>> {
        let data = lock(&self.inner)?.screen.snapshot();
        ensure_frame_payload_size("screen snapshot", data.len())?;
        Ok(data)
    }
    /// Plain text of the current screen (design doc section 8), for
    /// `Operation::CaptureScreen { plain: true }`.
    fn screen_contents(&self) -> Result<String> {
        let data = lock(&self.inner)?.screen.contents();
        ensure_frame_payload_size("plain screen capture", data.len())?;
        Ok(data)
    }
    /// Resizes the live screen model; called by `WorkerRuntime::resize`
    /// before the PTY ioctl (design doc section 5.3).
    fn set_size(&self, rows: u16, cols: u16) -> Result<()> {
        lock(&self.inner)?.screen.try_set_size(rows, cols)
    }
    /// Persists dirty history without ever coupling failure back into PTY
    /// delivery. Repeated failures use capped backoff; `force` is reserved
    /// for the final lifecycle attempt.
    fn flush(&self) -> Result<()> {
        self.flush_history(false)
    }
    fn flush_history(&self, force: bool) -> Result<()> {
        let mut inner = lock(&self.inner)?;
        if !force && Instant::now() < inner.history_retry_at {
            return Ok(());
        }
        match inner.history.flush() {
            Ok(()) => {
                inner.history_persistence_error = None;
                inner.history_retry_at = Instant::now();
                inner.history_retry_delay = HISTORY_RETRY_INITIAL;
                Ok(())
            }
            Err(error) => {
                let message = format!("{error:#}");
                inner.history_persistence_error = Some(message.clone());
                inner.history_retry_at = Instant::now() + inner.history_retry_delay;
                inner.history_retry_delay = inner
                    .history_retry_delay
                    .saturating_mul(2)
                    .min(HISTORY_RETRY_MAX);
                Err(anyhow!(message))
            }
        }
    }
    fn history_persistence_error(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.history_persistence_error.clone())
    }
    fn subscribe(&self, payload: AttachPayload) -> Result<(u64, Vec<u8>, OutputReceiver)> {
        let mut inner = lock(&self.inner)?;
        let initial = match payload {
            AttachPayload::Screen => {
                let data = inner.screen.snapshot();
                ensure_frame_payload_size("screen snapshot", data.len())?;
                data
            }
            AttachPayload::Tail(max) => inner.history.snapshot(Some(bounded_history_limit(max))),
        };
        if inner.terminal.is_none() && inner.subscribers.len() >= MAX_SUBSCRIBERS {
            bail!("too many attached clients");
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let (events_tx, events_rx) = mpsc::sync_channel(SUBSCRIBER_QUEUE_EVENTS);
        let (terminal_tx, terminal_rx) = mpsc::sync_channel(1);
        let subscriber = SubscriberSender {
            events: events_tx,
            terminal: terminal_tx,
        };
        if let Some(terminal) = inner.terminal.clone() {
            subscriber.terminate(terminal);
        } else {
            inner.subscribers.insert(id, subscriber);
        }
        Ok((
            id,
            initial,
            OutputReceiver {
                events: events_rx,
                terminal: terminal_rx,
            },
        ))
    }
    fn unsubscribe(&self, id: u64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.subscribers.remove(&id);
        }
    }
    fn finish(&self, exit: ExitInfo) {
        if let Ok(mut inner) = self.inner.lock() {
            let terminal = inner
                .terminal
                .get_or_insert_with(|| OutputEvent::Exit(exit.clone()))
                .clone();
            if let Err(error) = inner.history.flush() {
                eprintln!("aplexer worker: flush history at exit: {error:#}");
                inner.history_persistence_error = Some(format!("{error:#}"));
            }
            // Cheap post-mortem "what was on screen when it died" fallback
            // for `a capture --screen` on a dead session (design doc
            // section 5.5) -- the live grid itself dies with the worker,
            // this is the durable trace of it. Best-effort: a failure here
            // must not stop the exit event from reaching subscribers.
            if let Err(error) = fs::write(&self.screen_txt_path, inner.screen.contents()) {
                eprintln!("aplexer worker: write screen.txt at exit: {error:#}");
            }
            for (_, subscriber) in inner.subscribers.drain() {
                subscriber.terminate(terminal.clone());
            }
        }
    }
    fn fail_subscribers(&self, message: String) {
        if let Ok(mut inner) = self.inner.lock() {
            let terminal = inner
                .terminal
                .get_or_insert(OutputEvent::Error(message))
                .clone();
            for (_, subscriber) in inner.subscribers.drain() {
                subscriber.terminate(terminal.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_hub(dir: &tempfile::TempDir) -> OutputHub {
        OutputHub::new(
            History::open(dir.path().join("history.bin"), 1024 * 1024).unwrap(),
            24,
            80,
            dir.path().join("screen.txt"),
        )
        .unwrap()
    }

    #[test]
    fn lagging_subscriber_is_evicted_when_queue_fills() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();

        for _ in 0..=SUBSCRIBER_QUEUE_EVENTS {
            hub.append(b"x").unwrap();
        }

        assert!(hub.inner.lock().unwrap().subscribers.is_empty());
        for _ in 0..SUBSCRIBER_QUEUE_EVENTS {
            assert!(matches!(rx.recv().unwrap(), OutputEvent::Data(_)));
        }
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Error(message) if message.contains("fell behind")
        ));
    }

    #[test]
    fn full_subscriber_queue_drains_before_explicit_exit() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();
        for _ in 0..SUBSCRIBER_QUEUE_EVENTS {
            hub.append(b"x").unwrap();
        }
        let exit = ExitInfo {
            code: Some(0),
            signal: None,
            oom_killed: false,
            exited_at_ms: 1,
        };
        hub.finish(exit.clone());

        for _ in 0..SUBSCRIBER_QUEUE_EVENTS {
            assert!(matches!(rx.recv().unwrap(), OutputEvent::Data(_)));
        }
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Exit(received) if received.code == exit.code
        ));
    }

    #[test]
    fn full_subscriber_queue_drains_before_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();
        for _ in 0..SUBSCRIBER_QUEUE_EVENTS {
            hub.append(b"x").unwrap();
        }
        hub.fail_subscribers("PTY failed".into());

        for _ in 0..SUBSCRIBER_QUEUE_EVENTS {
            assert!(matches!(rx.recv().unwrap(), OutputEvent::Data(_)));
        }
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Error(message) if message == "PTY failed"
        ));
    }

    #[test]
    fn subscriber_count_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let mut receivers = Vec::new();
        for _ in 0..MAX_SUBSCRIBERS {
            let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();
            receivers.push(rx);
        }

        assert!(hub.subscribe(AttachPayload::Tail(None)).is_err());
        assert_eq!(receivers.len(), MAX_SUBSCRIBERS);
    }

    #[test]
    fn history_capture_limits_cannot_exceed_one_frame() {
        assert_eq!(bounded_history_limit(None), MAX_FRAME_BYTES);
        assert_eq!(bounded_history_limit(Some(123)), 123);
        assert_eq!(bounded_history_limit(Some(usize::MAX)), MAX_FRAME_BYTES);
        assert!(ensure_frame_payload_size("test", MAX_FRAME_BYTES).is_ok());
        assert!(ensure_frame_payload_size("test", MAX_FRAME_BYTES + 1).is_err());
    }

    #[test]
    fn history_failure_does_not_interrupt_live_output_and_can_recover() {
        let dir = tempfile::tempdir().unwrap();
        let history_path = dir.path().join("history.bin");
        fs::create_dir(&history_path).unwrap();
        let hub = OutputHub::new(
            History::open(history_path.clone(), 1024).unwrap(),
            24,
            80,
            dir.path().join("screen.txt"),
        )
        .unwrap();
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();

        hub.append(b"still-live").unwrap();
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Data(data) if data == b"still-live"
        ));
        assert_eq!(hub.snapshot(None).unwrap(), b"still-live");
        assert!(hub.flush_history(true).is_err());
        assert!(hub.history_persistence_error().is_some());

        fs::remove_dir(&history_path).unwrap();
        hub.flush_history(true).unwrap();
        assert!(hub.history_persistence_error().is_none());
        assert_eq!(fs::read(history_path).unwrap(), b"still-live");
    }

    #[test]
    fn connection_permits_are_bounded_and_release_on_drop() {
        let active = Arc::new(AtomicUsize::new(0));
        let permits: Vec<_> = (0..MAX_CLIENT_CONNECTIONS)
            .map(|_| try_acquire_connection(&active).expect("permit below limit"))
            .collect();
        assert!(try_acquire_connection(&active).is_none());
        assert_eq!(active.load(Ordering::Acquire), MAX_CLIENT_CONNECTIONS);

        drop(permits);
        assert_eq!(active.load(Ordering::Acquire), 0);
        assert!(try_acquire_connection(&active).is_some());

        let _ = std::panic::catch_unwind({
            let active = Arc::clone(&active);
            move || {
                let _permit = try_acquire_connection(&active).unwrap();
                panic!("exercise unwind cleanup");
            }
        });
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn descriptor_pressure_accept_errors_are_retriable() {
        for errno in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert!(transient_accept_error(&io::Error::from_raw_os_error(errno)));
        }
        assert!(!transient_accept_error(&io::Error::from_raw_os_error(
            libc::EBADF
        )));
    }
}

#[derive(Debug)]
struct WorkloadState {
    running: bool,
    pgid: i32,
}

struct WorkerRuntime {
    paths: Paths,
    record_path: std::path::PathBuf,
    runtime_session_dir: std::path::PathBuf,
    socket_path: std::path::PathBuf,
    record: Mutex<SessionRecord>,
    pty_write: Mutex<Option<File>>,
    workload: Mutex<WorkloadState>,
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
        update(&mut record);
        record.updated_at_ms = now_ms();
        match atomic_write_json(&self.record_path, &*record) {
            Ok(()) => {
                *lock(&self.record_persistence_error)? = None;
                Ok(record.clone())
            }
            Err(error) => {
                *lock(&self.record_persistence_error)? = Some(format!("{error:#}"));
                Err(error)
            }
        }
    }
    fn send(&self, data: &[u8]) -> Result<()> {
        if !lock(&self.workload)?.running {
            bail!("workload has exited");
        }
        let mut pty = lock(&self.pty_write)?;
        let file = pty.as_mut().ok_or_else(|| anyhow!("PTY is closed"))?;
        file.write_all(data).context("write PTY")?;
        file.flush()?;
        Ok(())
    }
    /// Resizes the live screen model *before* the PTY ioctl (design doc
    /// section 5.3): output already in flight at the old size is parsed at
    /// the new one -- a transient tmux shares too -- but this ordering
    /// means a subsequent attach's snapshot is never rendered against a
    /// model that's still the wrong shape for the geometry the workload was
    /// just told about.
    fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.output.set_size(rows, cols)?;
        let pty = lock(&self.pty_write)?;
        let file = pty.as_ref().ok_or_else(|| anyhow!("PTY is closed"))?;
        set_winsize(file.as_raw_fd(), rows.max(1), cols.max(1))
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
            return Ok(());
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
        // could be lost entirely).
        while self.workload_populated()? && Instant::now() < grace_deadline {
            thread::sleep(DESCENDANT_POLL_INTERVAL);
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
    fn rename(&self, workspace: std::path::PathBuf, tag: String) -> Result<SessionRecord> {
        validate_tag(&tag)?;
        let workspace = canonical_workspace(&workspace)?;
        let id = lock(&self.record)?.id;
        let _registry = FileLock::exclusive(&self.paths.registry_lock(), false)?;
        if let Some(conflict) = list_records(&self.paths)?
            .into_iter()
            .find(|record| record.id != id && record.workspace == workspace && record.tag == tag)
        {
            bail!("workspace+tag already belongs to session {}", conflict.id);
        }
        self.update_record(|r| {
            r.workspace = workspace;
            r.tag = tag;
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| anyhow!("worker lock poisoned"))
}

/// Make the worker the reparenting boundary for daemonized workload
/// descendants. This is process-wide on Linux and must happen before the
/// workload is spawned. It does not require systemd or cgroup delegation.
fn enable_child_subreaper() -> Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error()).context("enable child subreaper");
    }
    Ok(())
}

/// Read every child attached to any thread in a process. Reading only the
/// thread-group leader's `children` file can miss children forked by another
/// thread, which would create a containment escape for multi-threaded tools.
fn direct_child_pids(pid: u32) -> Result<Vec<u32>> {
    let tasks_path = format!("/proc/{pid}/task");
    let tasks = match fs::read_dir(&tasks_path) {
        Ok(tasks) => tasks,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {tasks_path}")),
    };
    let mut children = HashSet::new();
    for task in tasks {
        let Ok(task) = task else { continue };
        let Some(tid) = task
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let path = format!("/proc/{pid}/task/{tid}/children");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("read {path}")),
        };
        children.extend(
            text.split_whitespace()
                .filter_map(|value| value.parse::<u32>().ok()),
        );
    }
    Ok(children.into_iter().collect())
}

fn descendant_pids(root: u32) -> Result<Vec<u32>> {
    let mut pending = VecDeque::from([root]);
    let mut seen = HashSet::from([root]);
    let mut descendants = Vec::new();
    while let Some(parent) = pending.pop_front() {
        for child in direct_child_pids(parent)? {
            if seen.insert(child) {
                descendants.push(child);
                pending.push_back(child);
            }
        }
    }
    Ok(descendants)
}

struct DescendantHandle {
    pid: u32,
    pidfd: File,
}

fn open_descendant_handle(pid: u32) -> Result<Option<DescendantHandle>> {
    let start_time = match process_start_time_ticks(pid) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error).with_context(|| format!("open pidfd for descendant {pid}"));
    }
    let pidfd = unsafe { File::from_raw_fd(fd) };
    // Pin first, then re-read identity. A numeric pid recycled between the
    // tree walk and pidfd_open must never redirect a session signal.
    if process_start_time_ticks(pid).ok() != Some(start_time) {
        return Ok(None);
    }
    Ok(Some(DescendantHandle { pid, pidfd }))
}

fn descendant_handles(root: u32) -> Result<Vec<DescendantHandle>> {
    descendant_pids(root)?
        .into_iter()
        .filter_map(|pid| open_descendant_handle(pid).transpose())
        .collect()
}

fn signal_handle(handle: &DescendantHandle, signal: i32) -> Result<()> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            handle.pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error).with_context(|| format!("signal descendant {}", handle.pid));
        }
    }
    Ok(())
}

fn signal_descendants(root: u32, signal: i32) -> Result<usize> {
    let handles = descendant_handles(root)?;
    for handle in &handles {
        signal_handle(handle, signal)?;
    }
    Ok(handles.len())
}

/// Repeated scans close the fork-vs-scan race: the first pass stops the
/// parents, and later passes catch children created immediately before the
/// signal arrived. pidfds make every individual signal immune to pid reuse.
fn kill_descendants(root: u32, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if signal_descendants(root, libc::SIGKILL)? == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out killing contained workload descendants");
        }
        thread::sleep(DESCENDANT_POLL_INTERVAL);
    }
}

/// The leader has its own `Child::wait` thread. Only after that waiter has
/// reported completion may the lifecycle thread reap any other child,
/// avoiding a waitpid(-1) race that could steal the leader's exit status.
fn reap_adopted_children() -> Result<usize> {
    let mut reaped = 0;
    loop {
        let mut status = 0;
        let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if pid > 0 {
            reaped += 1;
            continue;
        }
        if pid == 0 {
            return Ok(reaped);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(reaped);
        }
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(error).context("reap adopted workload descendant");
    }
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

enum LifeEvent {
    PtyEof,
    PtyError(String),
    WaiterError(String),
    ChildExit {
        code: Option<i32>,
        signal: Option<i32>,
    },
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
    let mut record = read_record(&record_path)?;
    ensure_private_dir(&paths.runtime_session(id))?;
    let mut _worker_lock = FileLock::exclusive(&paths.worker_lock(id), true)
        .with_context(|| format!("worker for {id} is already running"))?;
    let mut worker_lock_identity = trusted_lock_identity(&paths.worker_lock(id))?;
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
        record.containment_cgroup = cgroup.as_ref().map(|cgroup| cgroup.locator().to_path_buf());
        record.containment_cgroup_identity =
            cgroup.as_ref().map(|cgroup| cgroup.identity().clone());
        startup.failure_record = record.clone();
        atomic_write_json(&record_path, &record)?;
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
        let child_slot = Arc::new(Mutex::new(Some(child)));
        startup.child = Some(Arc::clone(&child_slot));
        record.workload_pid = Some(pid);
        startup.failure_record = record.clone();
        // Publish the leader and cgroup locator before any injected or real
        // post-spawn failure. The launcher must never have to infer a
        // containment domain from an unpersisted in-memory PID.
        atomic_write_json(&record_path, &record)?;
        after_workload_spawn_checkpoint(pid)?;

        record.phase = Phase::Running;
        record.updated_at_ms = now_ms();
        record.error = None;
        startup.failure_record = record.clone();
        atomic_write_json(&record_path, &record)?;
        startup_checkpoint("after_running_record")?;

        startup_checkpoint("before_history_open")?;
        let history = History::open(record.history_path.clone(), record.history_bytes)?;
        startup_checkpoint("before_output_hub")?;
        let output = OutputHub::new(history, rows, cols, paths.screen_txt(id))?;
        let runtime = Arc::new(WorkerRuntime {
            paths: paths.clone(),
            record_path: record_path.clone(),
            runtime_session_dir: paths.runtime_session(id),
            socket_path,
            record: Mutex::new(record.clone()),
            pty_write: Mutex::new(Some(master_write)),
            workload: Mutex::new(WorkloadState {
                running: true,
                pgid: pid as i32,
            }),
            cgroup: Mutex::new(cgroup),
            kill_gate: Mutex::new(()),
            output,
            record_persistence_error: Mutex::new(None),
            active_connections: Arc::new(AtomicUsize::new(0)),
            last_activity_ms: AtomicU64::new(0),
        });
        start_worker_threads(Arc::clone(&runtime), master_read, Arc::clone(&child_slot))?;
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
        return Err(io::Error::last_os_error());
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
) -> Result<(UnixListener, (u64, u64), Option<FileLock>, (u64, u64))> {
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

fn spawn_workload(
    record: &SessionRecord,
    launch_environment: &std::collections::BTreeMap<String, String>,
    master_fd: RawFd,
    slave: File,
    cgroup: Option<&Cgroup>,
) -> Result<Child> {
    let program = record
        .command
        .first()
        .ok_or_else(|| anyhow!("empty workload command"))?;
    let slave_fd = slave.as_raw_fd();
    // The child attaches itself to the cgroup from inside pre_exec, before
    // it execs the real program. Any process may write its own pid into a
    // cgroup.procs it has access to, so this needs no rendezvous with the
    // parent after fork -- see Cgroup::open_procs for why a post-fork
    // handshake would deadlock here.
    let cgroup_procs = cgroup.map(Cgroup::open_procs).transpose()?;
    let cgroup_procs_fd = cgroup_procs.as_ref().map(|f| f.as_raw_fd());
    let mut command = Command::new(program);
    command
        .args(&record.command[1..])
        .current_dir(&record.cwd)
        .envs(launch_environment)
        .env("APLEXER_SESSION_ID", record.id.to_string())
        .env("APLEXER_WORKSPACE", &record.workspace)
        .env("APLEXER_TAG", &record.tag)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Put the `a` next to this worker first on PATH so `a whoami` inside
    // the session is the same CLI that started it, not some other `a` later
    // on PATH.
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let mut path = dir.as_os_str().to_os_string();
            path.push(":");
            if let Some(existing) = env::var_os("PATH") {
                path.push(existing);
            }
            command.env("PATH", path);
        }
    }
    // Provider-key safety strip (pocketshell-integration-plan.md 0.2): the
    // workload must not inherit these vars from the WORKER's own process
    // environment either, not just avoid getting them freshly set above --
    // `Command` starts from a clone of this process's environment, so an
    // ambient `ANTHROPIC_API_KEY` etc in the worker's own env would
    // otherwise leak straight into the spawned agent. Removed last, after
    // `.envs(&record.env)`, so the strip always wins even over a profile
    // that (deliberately or not) tries to set one of these names --
    // matches pocketshell's own `agents.py::build_env` ordering.
    for name in &record.env_unset {
        command.env_remove(name);
    }
    unsafe {
        command.pre_exec(move || {
            // A worker can be launched from an embedded, multi-threaded host
            // whose spawning thread blocks signals or whose process ignores
            // them. Both states survive fork, and ignored dispositions even
            // survive exec. Give the workload the same clean signal baseline
            // it would get from a normal interactive shell instead of leaking
            // host-library policy into the session.
            let mut default_action: libc::sigaction = std::mem::zeroed();
            default_action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut default_action.sa_mask);
            for signal in 1..=libc::SIGRTMAX() {
                if signal == libc::SIGKILL || signal == libc::SIGSTOP {
                    continue;
                }
                if libc::sigaction(signal, &default_action, std::ptr::null_mut()) != 0 {
                    let error = io::Error::last_os_error();
                    // glibc reserves a couple of real-time signal numbers for
                    // NPTL and rejects attempts to change them.
                    if error.raw_os_error() != Some(libc::EINVAL) {
                        return Err(error);
                    }
                }
            }
            let mut empty_mask: libc::sigset_t = std::mem::zeroed();
            if libc::sigemptyset(&mut empty_mask) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::sigprocmask(libc::SIG_SETMASK, &empty_mask, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
            libc::close(master_fd);
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            for target in 0..=2 {
                if libc::dup2(slave_fd, target) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if slave_fd > 2 {
                libc::close(slave_fd);
            }
            let pgid = libc::getpid();
            libc::tcsetpgrp(0, pgid);
            if let Some(fd) = cgroup_procs_fd {
                let text = pgid.to_string();
                let bytes = text.as_bytes();
                let n = libc::write(fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
                if n < 0 || n as usize != bytes.len() {
                    return Err(io::Error::last_os_error());
                }
                libc::close(fd);
            }
            Ok(())
        });
    }
    let child = command.spawn().context("spawn workload")?;
    drop(slave);
    drop(cgroup_procs);
    if let Some(cgroup) = cgroup {
        cgroup.release_anchor()?;
    }
    Ok(child)
}

fn spawn_startup_thread<F>(
    name: &str,
    index: usize,
    gate: &ThreadStartGate,
    handles: &mut Vec<thread::JoinHandle<()>>,
    job: F,
) -> Result<()>
where
    F: FnOnce() + Send + 'static,
{
    startup_checkpoint(name)?;
    startup_checkpoint(&format!("thread_{index}"))?;
    let gate = Arc::clone(gate);
    handles.push(
        thread::Builder::new()
            .name(format!("aplexer-{name}"))
            .spawn(move || {
                if await_thread_start(&gate) {
                    job();
                }
            })
            .with_context(|| format!("spawn {name} thread"))?,
    );
    Ok(())
}

fn start_worker_threads(
    runtime: Arc<WorkerRuntime>,
    master_read: File,
    child_slot: Arc<Mutex<Option<Child>>>,
) -> Result<()> {
    let gate = Arc::new((Mutex::new(ThreadStart::Pending), Condvar::new()));
    let mut handles = Vec::new();
    let (life_tx, life_rx) = mpsc::channel();
    let setup = (|| -> Result<()> {
        let periodic_runtime = Arc::clone(&runtime);
        spawn_startup_thread("history-flush", 1, &gate, &mut handles, move || {
            run_periodic_flush(periodic_runtime)
        })?;

        let reader_runtime = Arc::clone(&runtime);
        let reader_tx = life_tx.clone();
        spawn_startup_thread("pty-reader", 2, &gate, &mut handles, move || {
            run_pty_reader(master_read, reader_runtime, reader_tx)
        })?;

        let waiter_tx = life_tx;
        spawn_startup_thread("child-waiter", 3, &gate, &mut handles, move || {
            let child = match child_slot.lock() {
                Ok(mut slot) => slot.take(),
                Err(_) => {
                    let _ = waiter_tx.send(LifeEvent::PtyError(
                        "workload child slot lock poisoned".into(),
                    ));
                    return;
                }
            };
            if let Some(child) = child {
                #[cfg(feature = "startup-test-hooks")]
                if let Some(marker) = env::var_os("APLEXER_TEST_FAIL_WAITER_AFTER_FILE") {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let marker = std::path::PathBuf::from(marker);
                    while !marker.exists() && Instant::now() < deadline {
                        thread::sleep(DESCENDANT_POLL_INTERVAL);
                    }
                    drop(child);
                    let _ = waiter_tx.send(LifeEvent::WaiterError(format!(
                        "injected workload waiter failure after {}",
                        marker.display()
                    )));
                    return;
                }
                run_child_waiter(child, waiter_tx);
            }
        })?;

        let lifecycle_runtime = Arc::clone(&runtime);
        spawn_startup_thread("lifecycle", 4, &gate, &mut handles, move || {
            run_lifecycle(lifecycle_runtime, life_rx)
        })?;

        let termination_runtime = Arc::clone(&runtime);
        spawn_startup_thread("termination", 5, &gate, &mut handles, move || {
            run_termination_monitor(termination_runtime)
        })?;
        startup_checkpoint("after_thread_setup")?;
        Ok(())
    })();
    if let Err(error) = setup {
        release_startup_threads(&gate, ThreadStart::Abort);
        for handle in handles {
            let _ = handle.join();
        }
        return Err(error);
    }
    release_startup_threads(&gate, ThreadStart::Run);
    Ok(())
}

fn run_periodic_flush(runtime: Arc<WorkerRuntime>) {
    // Debounced history persistence (see History::append) needs a periodic
    // sweep so output followed by silence still reaches disk. The same tick
    // persists last_activity_ms, but only when it has changed.
    let mut persisted_activity_ms: u64 = 0;
    loop {
        thread::sleep(HISTORY_FLUSH_INTERVAL);
        if let Err(error) = runtime.output.flush() {
            eprintln!("aplexer worker: flush history: {error:#}");
        }
        let current = runtime.last_activity_ms.load(Ordering::Relaxed);
        if current != 0 && current != persisted_activity_ms {
            persisted_activity_ms = current;
            if let Err(error) = runtime.update_record(|r| r.last_activity_ms = Some(current)) {
                eprintln!("aplexer worker: persist activity: {error:#}");
            }
        }
    }
}

fn run_termination_monitor(runtime: Arc<WorkerRuntime>) {
    while !TERMINATION_REQUESTED.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(10));
    }
    if let Err(error) = runtime.kill(libc::SIGTERM, 500) {
        eprintln!("aplexer worker: terminate contained workload: {error:#}");
        let _ = runtime.kill(libc::SIGKILL, 0);
    }
}

fn run_pty_reader(mut master: File, runtime: Arc<WorkerRuntime>, tx: mpsc::Sender<LifeEvent>) {
    let mut buffer = vec![0u8; 32 * 1024];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => {
                let _ = tx.send(LifeEvent::PtyEof);
                break;
            }
            Ok(n) => {
                runtime.last_activity_ms.store(now_ms(), Ordering::Relaxed);
                if let Err(error) = runtime.output.append(&buffer[..n]) {
                    let _ = tx.send(LifeEvent::PtyError(format!("persist output: {error:#}")));
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                let _ = tx.send(LifeEvent::PtyEof);
                break;
            }
            Err(error) => {
                let _ = tx.send(LifeEvent::PtyError(format!("read PTY: {error}")));
                break;
            }
        }
    }
}

fn run_child_waiter(mut child: Child, tx: mpsc::Sender<LifeEvent>) {
    let event = match child.wait() {
        Ok(status) => LifeEvent::ChildExit {
            code: status.code(),
            signal: status.signal(),
        },
        Err(error) => LifeEvent::WaiterError(format!("wait workload: {error}")),
    };
    let _ = tx.send(event);
}

/// A waiter failure means nobody owns the tracked Child any longer. Before
/// the subreaper is allowed to exit, repeatedly kill, reap, and inspect its
/// complete containment domain. Only an observed empty domain is proof.
fn cleanup_after_lifecycle_failure(runtime: &WorkerRuntime) -> Result<()> {
    let _serialized = lock(&runtime.kill_gate)?;
    let cgroup = lock(&runtime.cgroup)?.clone();
    let deadline = Instant::now() + DESCENDANT_KILL_TIMEOUT;
    loop {
        if let Some(cgroup) = &cgroup {
            cgroup
                .kill_all_until(deadline)
                .context("kill failed lifecycle cgroup")?;
        } else {
            signal_descendants(std::process::id(), libc::SIGKILL)
                .context("kill failed lifecycle descendants")?;
        }
        reap_adopted_children().context("reap failed lifecycle descendants")?;
        if !runtime.workload_populated()? {
            if let Ok(mut state) = runtime.workload.lock() {
                state.running = false;
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out proving failed lifecycle containment empty");
        }
        thread::sleep(DESCENDANT_POLL_INTERVAL);
    }
}

fn run_lifecycle(runtime: Arc<WorkerRuntime>, rx: mpsc::Receiver<LifeEvent>) {
    let mut pty_eof = false;
    let mut child_exit: Option<(Option<i32>, Option<i32>)> = None;
    let mut fatal: Option<String> = None;
    let mut containment_empty = false;
    loop {
        match rx.recv_timeout(DESCENDANT_POLL_INTERVAL) {
            Ok(event) => match event {
                LifeEvent::PtyEof => {
                    pty_eof = true;
                    if let Ok(mut pty) = runtime.pty_write.lock() {
                        *pty = None;
                    }
                }
                LifeEvent::PtyError(message) => {
                    pty_eof = true;
                    fatal = Some(message.clone());
                    if let Ok(mut pty) = runtime.pty_write.lock() {
                        *pty = None;
                    }
                    runtime.output.fail_subscribers(message);
                }
                LifeEvent::WaiterError(message) => {
                    fatal = Some(message.clone());
                    if let Ok(mut pty) = runtime.pty_write.lock() {
                        *pty = None;
                    }
                    runtime.output.fail_subscribers(message);
                    break;
                }
                LifeEvent::ChildExit { code, signal } => {
                    child_exit = Some((code, signal));
                    let _ = runtime.update_record(|r| r.phase = Phase::Exiting);
                }
            },
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) if child_exit.is_some() => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                fatal = Some("workload lifecycle channel disconnected".into());
                break;
            }
        }

        if child_exit.is_some() {
            if let Err(error) = reap_adopted_children() {
                fatal.get_or_insert_with(|| format!("reap descendants: {error:#}"));
            }
            match runtime.workload_populated() {
                Ok(populated) => {
                    if let Ok(mut state) = runtime.workload.lock() {
                        state.running = populated;
                    }
                    if pty_eof && !populated {
                        containment_empty = true;
                        break;
                    }
                }
                Err(error) => {
                    // Fail closed: never finalize evidence while we cannot
                    // establish that the containment domain is empty.
                    fatal.get_or_insert_with(|| format!("inspect descendants: {error:#}"));
                }
            }
        }
    }
    if !containment_empty && fatal.is_some() {
        match cleanup_after_lifecycle_failure(&runtime) {
            Ok(()) => containment_empty = true,
            Err(error) => {
                let message = format!("containment cleanup unproven: {error:#}");
                fatal = Some(match fatal {
                    Some(existing) => format!("{existing}; {message}"),
                    None => message,
                });
            }
        }
    }
    let (code, signal) = child_exit.unwrap_or((None, None));
    let (oom, mut cg) = match runtime.cgroup.lock() {
        Ok(mut g) => {
            let oom = g.as_ref().map(Cgroup::oom_killed).unwrap_or(false);
            let cg = if containment_empty { g.take() } else { None };
            (oom, cg)
        }
        Err(_) => (false, None),
    };
    let exit = ExitInfo {
        code,
        signal,
        oom_killed: oom,
        exited_at_ms: now_ms(),
    };
    if let Err(history_error) = runtime.output.flush_history(true) {
        let message = format!("persist final history: {history_error:#}");
        fatal = Some(match fatal {
            Some(existing) => format!("{existing}; {message}"),
            None => message,
        });
    }
    let error = fatal.clone();
    let mut record_retry = HISTORY_RETRY_INITIAL;
    loop {
        let final_error = error.clone();
        match runtime.update_record(|r| {
            r.phase = if final_error.is_some() {
                Phase::Failed
            } else {
                Phase::Exited
            };
            r.containment_empty = Some(containment_empty);
            r.exit = Some(exit.clone());
            r.error = final_error;
        }) {
            Ok(_) => break,
            Err(persist_error) => {
                // Never exit with a durable record that still claims this
                // worker/workload is running. Keep the control socket alive
                // so Status can expose `record_persistence_error` while the
                // lifecycle retries.
                eprintln!(
                    "aplexer worker: persist final session state: {persist_error:#}; retrying in {}ms",
                    record_retry.as_millis()
                );
                thread::sleep(record_retry);
                record_retry = record_retry.saturating_mul(2).min(HISTORY_RETRY_MAX);
            }
        }
    }
    if !containment_empty {
        runtime
            .output
            .fail_subscribers(fatal.unwrap_or_else(|| "containment cleanup was not proven".into()));
        // Retain the worker as the subreaper boundary, along with its socket,
        // cgroup handle, and runtime evidence. A later `a kill` can retry;
        // this monitor will finalize only after it independently observes the
        // resulting domain empty and durably records that proof.
        loop {
            if let Err(error) = reap_adopted_children() {
                eprintln!("aplexer worker: reap after lifecycle failure: {error:#}");
            }
            match runtime.workload_populated() {
                Ok(false) => {
                    if let Err(error) =
                        runtime.update_record(|record| record.containment_empty = Some(true))
                    {
                        eprintln!("aplexer worker: persist delayed containment proof: {error:#}");
                    } else {
                        cg = runtime
                            .cgroup
                            .lock()
                            .ok()
                            .and_then(|mut cgroup| cgroup.take());
                        break;
                    }
                }
                Ok(true) => {}
                Err(error) => {
                    eprintln!("aplexer worker: inspect failed lifecycle containment: {error:#}")
                }
            }
            thread::sleep(DESCENDANT_POLL_INTERVAL);
        }
    }
    runtime.output.finish(exit.clone());
    if let Some(cg) = cg {
        cg.cleanup();
    }
    // Keep the terminal record, history, final screen, and transcript
    // binding for successful exits too. Besides enabling post-mortem
    // capture/status, this gives polling watchers a durable transition
    // to observe. `a kill` and `a prune` remain explicit cleanup paths.
    // The workload is gone and the final record/history are persisted;
    // a daemonless design must not leave a worker process (plus its
    // socket and runtime dir) behind for every session that ever ran.
    // Unlink the socket first so new clients fail fast and fall back to
    // the persisted record/history, then give in-flight connections
    // (the `kill` response, attach Exit events) a bounded window to
    // drain before exiting the process.
    let _ = fs::remove_file(&runtime.socket_path);
    let drain_deadline = Instant::now() + Duration::from_secs(3);
    while runtime.active_connections.load(Ordering::SeqCst) > 0 && Instant::now() < drain_deadline {
        thread::sleep(Duration::from_millis(25));
    }
    let _ = fs::remove_dir_all(&runtime.runtime_session_dir);
    std::process::exit(0);
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
    match request.operation {
        Operation::Ping => write_json(&mut stream, &Response::ok(id, json!({"pong":true})))?,
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
            Ok(()) => write_json(&mut stream, &Response::ok(id, json!({"signalled":true})))?,
            Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
        },
        Operation::Rename { workspace, tag } => match runtime.rename(workspace, tag) {
            Ok(record) => write_json(
                &mut stream,
                &Response::ok(id, serde_json::to_value(record)?),
            )?,
            Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
        },
    }
    Ok(())
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
    if let (Some(rows), Some(cols)) = (rows, cols) {
        screen::validate_size(rows, cols)?;
    }
    // An established attach is intentionally long-lived. Before this point,
    // the handshake used the worker-wide deadline so a peer cannot reserve a
    // connection slot forever with a partial frame.
    reader.set_read_timeout(None)?;
    // Geometry-first (design doc section 6.1): resize the PTY and the
    // screen model to the client's real terminal size *before* rendering
    // the snapshot below, so there is no wrong-size frame followed by a
    // SIGWINCH repaint. `WorkerRuntime::resize` itself resizes the model
    // before the ioctl (section 5.3), so this one call gets both in the
    // right order. Best-effort: a resize failure here (e.g. the PTY is
    // already closing) must not block the attach -- the pre-existing
    // client-side post-connect Resize control frame remains the fallback
    // (section 6.3 step 7).
    if let (Some(rows), Some(cols)) = (rows, cols) {
        let _ = runtime.resize(rows, cols);
    }
    let payload = if want_screen {
        AttachPayload::Screen
    } else {
        AttachPayload::Tail(history_bytes)
    };
    let (subscription, initial, rx) = runtime.output.subscribe(payload)?;
    let writer_stream = reader.try_clone()?;
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
                if runtime.send(&frame.payload).is_err() {
                    break;
                }
            }
            FrameKind::End => break,
            FrameKind::Json => {
                let control: AttachControl = serde_json::from_slice(&frame.payload)?;
                match control {
                    AttachControl::Resize { rows, cols } => {
                        let _ = runtime.resize(rows, cols);
                    }
                    AttachControl::Signal { signal } => {
                        let _ = runtime.signal(signal);
                    }
                    AttachControl::Detach => break,
                }
            }
        }
    }
    runtime.output.unsubscribe(subscription);
    let _ = reader.shutdown(std::net::Shutdown::Both);
    Ok(())
}
