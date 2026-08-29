use crate::*;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::json;
use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard};
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
    screen: screen::ScreenTracker,
    subscribers: HashMap<u64, mpsc::SyncSender<OutputEvent>>,
    next_id: u64,
    final_exit: Option<ExitInfo>,
}

struct OutputHub {
    inner: Mutex<HubInner>,
    /// Where `finish` writes the final plain-text screen on exit (design
    /// doc section 5.5) -- immutable, so no need to route it through the
    /// lock.
    screen_txt_path: std::path::PathBuf,
}
impl OutputHub {
    fn new(history: History, rows: u16, cols: u16, screen_txt_path: std::path::PathBuf) -> Self {
        Self {
            inner: Mutex::new(HubInner {
                history,
                screen: screen::ScreenTracker::new(rows, cols),
                subscribers: HashMap::new(),
                next_id: 1,
                final_exit: None,
            }),
            screen_txt_path,
        }
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
            .retain(|_, tx| tx.try_send(OutputEvent::Data(data.to_vec())).is_ok());
        if let Some(change) = layout {
            inner
                .subscribers
                .retain(|_, tx| tx.try_send(OutputEvent::Layout(change)).is_ok());
        }
        Ok(())
    }
    fn snapshot(&self, max: Option<usize>) -> Result<Vec<u8>> {
        Ok(lock(&self.inner)?.history.snapshot(max))
    }
    /// The rendered current-screen snapshot (design doc section 6.2),
    /// shared by attach's `AttachPayload::Screen` and
    /// `Operation::CaptureScreen { plain: false }`.
    fn screen_snapshot(&self) -> Result<Vec<u8>> {
        Ok(lock(&self.inner)?.screen.snapshot())
    }
    /// Plain text of the current screen (design doc section 8), for
    /// `Operation::CaptureScreen { plain: true }`.
    fn screen_contents(&self) -> Result<String> {
        Ok(lock(&self.inner)?.screen.contents())
    }
    /// Resizes the live screen model; called by `WorkerRuntime::resize`
    /// before the PTY ioctl (design doc section 5.3).
    fn set_size(&self, rows: u16, cols: u16) -> Result<()> {
        lock(&self.inner)?.screen.set_size(rows, cols);
        Ok(())
    }
    /// Persists any history bytes the debounced append path hasn't written
    /// yet; driven by a periodic thread so an idle session's tail doesn't
    /// stay memory-only indefinitely, and called on finish for the final
    /// state.
    fn flush(&self) -> Result<()> {
        lock(&self.inner)?.history.flush()
    }
    fn subscribe(
        &self,
        payload: AttachPayload,
    ) -> Result<(u64, Vec<u8>, mpsc::Receiver<OutputEvent>)> {
        let mut inner = lock(&self.inner)?;
        let initial = match payload {
            AttachPayload::Screen => inner.screen.snapshot(),
            AttachPayload::Tail(max) => inner.history.snapshot(max),
        };
        if inner.final_exit.is_none() && inner.subscribers.len() >= MAX_SUBSCRIBERS {
            bail!("too many attached clients");
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let (tx, rx) = mpsc::sync_channel(SUBSCRIBER_QUEUE_EVENTS);
        if let Some(exit) = inner.final_exit.clone() {
            let _ = tx.try_send(OutputEvent::Exit(exit));
        } else {
            inner.subscribers.insert(id, tx);
        }
        Ok((id, initial, rx))
    }
    fn unsubscribe(&self, id: u64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.subscribers.remove(&id);
        }
    }
    fn finish(&self, exit: ExitInfo) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.final_exit = Some(exit.clone());
            if let Err(error) = inner.history.flush() {
                eprintln!("aplexer worker: flush history at exit: {error:#}");
            }
            // Cheap post-mortem "what was on screen when it died" fallback
            // for `a capture --screen` on a dead session (design doc
            // section 5.5) -- the live grid itself dies with the worker,
            // this is the durable trace of it. Best-effort: a failure here
            // must not stop the exit event from reaching subscribers.
            if let Err(error) = fs::write(&self.screen_txt_path, inner.screen.contents()) {
                eprintln!("aplexer worker: write screen.txt at exit: {error:#}");
            }
            for (_, tx) in inner.subscribers.drain() {
                let _ = tx.try_send(OutputEvent::Exit(exit.clone()));
            }
        }
    }
    fn fail_subscribers(&self, message: String) {
        if let Ok(mut inner) = self.inner.lock() {
            for (_, tx) in inner.subscribers.drain() {
                let _ = tx.try_send(OutputEvent::Error(message.clone()));
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
        assert_eq!(rx.iter().count(), SUBSCRIBER_QUEUE_EVENTS);
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
}

#[derive(Debug)]
struct WorkloadState {
    running: bool,
    pgid: i32,
}

struct WorkerRuntime {
    record_path: std::path::PathBuf,
    runtime_session_dir: std::path::PathBuf,
    socket_path: std::path::PathBuf,
    record: Mutex<SessionRecord>,
    pty_write: Mutex<Option<File>>,
    workload: Mutex<WorkloadState>,
    cgroup: Mutex<Option<Cgroup>>,
    kill_gate: Mutex<()>,
    output: OutputHub,
    /// Connections currently being served; the lifecycle thread drains this
    /// (with a timeout) before exiting the worker so in-flight responses
    /// (e.g. the reply to the `kill` that ended the workload) are not lost.
    active_connections: AtomicUsize,
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
        atomic_write_json(&self.record_path, &*record)?;
        Ok(record.clone())
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
        let _serialized = lock(&self.kill_gate)?;
        let (running, pgid) = {
            let state = lock(&self.workload)?;
            (state.running, state.pgid)
        };
        if !running {
            return Ok(());
        }
        let cgroup = lock(&self.cgroup)?.clone();
        if signal == libc::SIGKILL {
            if let Some(cg) = &cgroup {
                cg.kill_all()?;
            } else if unsafe { libc::kill(-pgid, libc::SIGKILL) } != 0 {
                return Err(io::Error::last_os_error()).context("kill process group");
            }
            return Ok(());
        }
        if let Some(cg) = &cgroup {
            cg.signal_all(signal)?;
        } else if unsafe { libc::kill(-pgid, signal) } != 0 {
            return Err(io::Error::last_os_error()).context("signal process group");
        }
        // Poll instead of sleeping the whole grace period: once the workload
        // is gone there is nothing to escalate to SIGKILL, and the response
        // to this request should not be delayed (the worker exits shortly
        // after the workload does, so a response stuck behind a long sleep
        // could be lost entirely).
        let deadline = Instant::now() + Duration::from_millis(grace_ms);
        while lock(&self.workload)?.running && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        let still_running = lock(&self.workload)?.running;
        if still_running {
            if let Some(cg) = &cgroup {
                cg.kill_all()?;
            } else {
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
        Ok(())
    }
    fn rename(&self, workspace: std::path::PathBuf, tag: String) -> Result<SessionRecord> {
        validate_tag(&tag)?;
        let workspace = canonical_workspace(&workspace)?;
        self.update_record(|r| {
            r.workspace = workspace;
            r.tag = tag;
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| anyhow!("worker lock poisoned"))
}

enum LifeEvent {
    PtyEof,
    PtyError(String),
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
    let paths = Paths::discover()?;
    let record_path = paths.record(id);
    let mut record = read_record(&record_path)?;
    ensure_private_dir(&paths.runtime_session(id))?;
    let _worker_lock = FileLock::exclusive(&paths.worker_lock(id), true)
        .with_context(|| format!("worker for {id} is already running"))?;

    record.worker_pid = Some(std::process::id());
    record.updated_at_ms = now_ms();
    atomic_write_json(&record_path, &record)?;

    let socket_path = paths.socket(id);
    if socket_path.exists() {
        fs::remove_file(&socket_path).context("remove stale control socket")?;
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind {}", socket_path.display()))?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))?;

    let cgroup = match Cgroup::create(id, &record.limits) {
        Ok(value) => value,
        Err(error) => return Err(fail_startup(&paths, id, &record_path, &mut record, error)),
    };
    let (rows, cols) = initial_size.unwrap_or((24, 80));
    let (master_read, slave) = match open_pty(rows, cols) {
        Ok(value) => value,
        Err(error) => return Err(fail_startup(&paths, id, &record_path, &mut record, error)),
    };
    let master_write = master_read.try_clone()?;
    let child = match spawn_workload(&record, master_read.as_raw_fd(), slave, cgroup.as_ref()) {
        Ok(child) => child,
        Err(error) => return Err(fail_startup(&paths, id, &record_path, &mut record, error)),
    };
    let pid = child.id();
    record.workload_pid = Some(pid);
    record.phase = Phase::Running;
    record.updated_at_ms = now_ms();
    record.error = None;
    atomic_write_json(&record_path, &record)?;

    let history = History::open(record.history_path.clone(), record.history_bytes)?;
    let screen_txt_path = paths.screen_txt(id);
    let runtime = Arc::new(WorkerRuntime {
        record_path,
        runtime_session_dir: paths.runtime_session(id),
        socket_path,
        record: Mutex::new(record),
        pty_write: Mutex::new(Some(master_write)),
        workload: Mutex::new(WorkloadState {
            running: true,
            pgid: pid as i32,
        }),
        cgroup: Mutex::new(cgroup),
        kill_gate: Mutex::new(()),
        output: OutputHub::new(history, rows, cols, screen_txt_path),
        active_connections: AtomicUsize::new(0),
        last_activity_ms: AtomicU64::new(0),
    });
    let (life_tx, life_rx) = mpsc::channel();
    {
        // Debounced history persistence (see History::append) needs a
        // periodic sweep so output followed by silence still reaches disk.
        // The same tick also persists last_activity_ms (see its doc comment
        // on WorkerRuntime) -- reusing this interval rather than adding a
        // second timer, and only writing the record when the in-memory
        // timestamp actually moved since the previous tick, so an idle
        // session does not get its record rewritten every interval forever.
        let runtime = runtime.clone();
        thread::spawn(move || {
            let mut persisted_activity_ms: u64 = 0;
            loop {
                thread::sleep(HISTORY_FLUSH_INTERVAL);
                if let Err(error) = runtime.output.flush() {
                    eprintln!("aplexer worker: flush history: {error:#}");
                }
                let current = runtime.last_activity_ms.load(Ordering::Relaxed);
                if current != 0 && current != persisted_activity_ms {
                    persisted_activity_ms = current;
                    if let Err(error) =
                        runtime.update_record(|r| r.last_activity_ms = Some(current))
                    {
                        eprintln!("aplexer worker: persist activity: {error:#}");
                    }
                }
            }
        });
    }
    spawn_pty_reader(master_read, runtime.clone(), life_tx.clone());
    spawn_child_waiter(child, life_tx);
    spawn_lifecycle(runtime.clone(), life_rx);

    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let runtime = runtime.clone();
                runtime.active_connections.fetch_add(1, Ordering::SeqCst);
                thread::spawn(move || {
                    if let Err(error) = handle_connection(stream, runtime.clone()) {
                        eprintln!("aplexer connection: {error:#}");
                    }
                    runtime.active_connections.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error).context("accept control connection"),
        }
    }
}

fn spawn_workload(
    record: &SessionRecord,
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
        .envs(&record.env)
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
        cgroup.release_anchor();
    }
    Ok(child)
}

fn spawn_pty_reader(mut master: File, runtime: Arc<WorkerRuntime>, tx: mpsc::Sender<LifeEvent>) {
    thread::spawn(move || {
        let mut buffer = vec![0u8; 32 * 1024];
        loop {
            match master.read(&mut buffer) {
                Ok(0) => {
                    let _ = tx.send(LifeEvent::PtyEof);
                    break;
                }
                Ok(n) => {
                    // Cheap, lock-free activity marker (see WorkerRuntime's
                    // last_activity_ms doc comment) -- deliberately updated
                    // unconditionally here, before the debounced/possibly
                    // I/O-performing append below, so it reflects PTY output
                    // recency even if history persistence is momentarily slow.
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
    });
}

fn spawn_child_waiter(mut child: Child, tx: mpsc::Sender<LifeEvent>) {
    thread::spawn(move || {
        let event = match child.wait() {
            Ok(status) => LifeEvent::ChildExit {
                code: status.code(),
                signal: status.signal(),
            },
            Err(error) => LifeEvent::PtyError(format!("wait workload: {error}")),
        };
        let _ = tx.send(event);
    });
}

fn spawn_lifecycle(runtime: Arc<WorkerRuntime>, rx: mpsc::Receiver<LifeEvent>) {
    thread::spawn(move || {
        let mut pty_eof = false;
        let mut child_exit: Option<(Option<i32>, Option<i32>)> = None;
        let mut fatal: Option<String> = None;
        while let Ok(event) = rx.recv() {
            match event {
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
                LifeEvent::ChildExit { code, signal } => {
                    child_exit = Some((code, signal));
                    if let Ok(mut state) = runtime.workload.lock() {
                        state.running = false;
                    }
                    let _ = runtime.update_record(|r| r.phase = Phase::Exiting);
                }
            }
            if pty_eof && child_exit.is_some() {
                break;
            }
        }
        let (code, signal) = child_exit.unwrap_or((None, None));
        let (oom, cg) = match runtime.cgroup.lock() {
            Ok(mut g) => {
                let cg = g.take();
                let oom = cg.as_ref().map(Cgroup::oom_killed).unwrap_or(false);
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
        let error = fatal.clone();
        let _ = runtime.update_record(|r| {
            r.phase = if error.is_some() {
                Phase::Failed
            } else {
                Phase::Exited
            };
            r.exit = Some(exit.clone());
            r.error = error;
        });
        runtime.output.finish(exit.clone());
        if let Some(cg) = cg {
            cg.cleanup();
        }
        // A workload that exited genuinely cleanly -- phase Exited (not
        // Failed), code 0, no signal, no OOM kill, no fatal I/O error --
        // has nothing left worth debugging, so remove it automatically:
        // matching how a tmux pane vanishes when its shell exits, the user
        // shouldn't have to remember to run `a kill` on a session that ran
        // fine and finished on its own. A non-clean exit (non-zero code,
        // signal, OOM kill, or `Phase::Failed`) deliberately keeps the
        // opposite behavior and stays in `a list`/`a status`/`a capture`:
        // that is exactly the case someone would want to inspect
        // afterwards, and the durable per-session record exists precisely
        // to preserve that diagnostic value -- auto-removing it would
        // destroy the thing it's for. `a kill` remains the manual cleanup
        // path for those (see the "already terminal" branch of `cmd_kill`
        // in src/bin/a.rs, which this mirrors).
        let clean_exit =
            fatal.is_none() && exit.code == Some(0) && exit.signal.is_none() && !exit.oom_killed;
        if clean_exit {
            if let Some(state_dir) = runtime.record_path.parent() {
                let _ = fs::remove_dir_all(state_dir);
            }
        }
        // The workload is gone and the final record/history are persisted;
        // a daemonless design must not leave a worker process (plus its
        // socket and runtime dir) behind for every session that ever ran.
        // Unlink the socket first so new clients fail fast and fall back to
        // the persisted record/history, then give in-flight connections
        // (the `kill` response, attach Exit events) a bounded window to
        // drain before exiting the process.
        let _ = fs::remove_file(&runtime.socket_path);
        let drain_deadline = Instant::now() + Duration::from_secs(3);
        while runtime.active_connections.load(Ordering::SeqCst) > 0
            && Instant::now() < drain_deadline
        {
            thread::sleep(Duration::from_millis(25));
        }
        let _ = fs::remove_dir_all(&runtime.runtime_session_dir);
        std::process::exit(0);
    });
}

fn fail_startup(
    paths: &Paths,
    id: Uuid,
    record_path: &std::path::Path,
    record: &mut SessionRecord,
    error: anyhow::Error,
) -> anyhow::Error {
    record.phase = Phase::Failed;
    record.error = Some(format!("{error:#}"));
    record.updated_at_ms = now_ms();
    if let Err(write_error) = atomic_write_json(record_path, record) {
        eprintln!("aplexer worker: persist failure record: {write_error:#}");
    }
    // A worker that failed to launch its workload must not leave its bound
    // socket (and runtime dir) behind: a present-but-dead socket makes the
    // session look temporarily unavailable instead of failed.
    let _ = fs::remove_dir_all(paths.runtime_session(id));
    error
}

fn handle_connection(mut stream: UnixStream, runtime: Arc<WorkerRuntime>) -> Result<()> {
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
            let mut value = serde_json::to_value(runtime.record()?)?;
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
