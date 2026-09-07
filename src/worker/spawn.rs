//! Bringing a session up: the workload spawn, and the runtime threads that
//! start once it exists.
//!
//! One reason to exist: the spawn sequence has an order that must never
//! shuffle -- secret env is loaded and dropped, the PTY is created, the
//! pre-exec gate arms, the workload enters its containment domain, and only
//! then do the flusher, termination monitor, PTY reader, and child waiter
//! threads start. Everything that runs "for the workload" from spawn until
//! the lifecycle takes over lives here.

use super::*;

/// The terminal aplexer presents to every workload it spawns.
///
/// A session outlives any particular client, and different clients with
/// different terminals attach to the same session over its life, so the
/// workload cannot be told "the terminal you have right now". It is told
/// what aplexer itself guarantees to relay faithfully, which the
/// `xterm-256color` terminfo entry describes and which is present on
/// effectively every system a workload runs on. Overridable per session
/// with `--env TERM=...` or a profile `env` entry.
pub(super) const WORKLOAD_TERM: &str = "xterm-256color";

/// Direct-color advertisement for the same relay. Kept separate from
/// `WORKLOAD_TERM` because it is a separate, independently overridable
/// claim: TERM names the terminfo entry, COLORTERM says the terminal
/// understands `CSI 38;2;r;g;b m`.
pub(super) const WORKLOAD_COLORTERM: &str = "truecolor";

pub(super) fn spawn_workload(
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
        // aplexer owns the workload's PTY, so aplexer -- not whatever shell
        // happened to run `a start` -- is the terminal the workload is
        // talking to. Inheriting the launcher's TERM is therefore always
        // wrong, and wrong in both directions: `a start` run from a cron
        // job, a desktop launcher, or another agent's non-interactive shell
        // leaks `dumb` (or nothing at all) into a session that a real
        // 256-color terminal later attaches to, and every TUI workload
        // downgrades itself to monochrome for the rest of the session's
        // life. Declare our own emulation instead, exactly as tmux and
        // screen do.
        //
        // `xterm-256color` is the honest declaration: `a attach` is a raw
        // byte relay (see StreamBoundary in src/screen.rs), so the
        // workload's escape sequences reach the attached terminal
        // untouched, and the vt100 grid that repaints the screen on
        // reattach models indexed and RGB color alike. COLORTERM says the
        // same thing about direct color, which is the difference between a
        // workload picking 256 palette entries and picking true RGB.
        //
        // Set BEFORE `.envs(launch_environment)` so a profile or an
        // explicit `--env TERM=...` still wins: later inserts of the same
        // key replace earlier ones.
        .env("TERM", WORKLOAD_TERM)
        .env("COLORTERM", WORKLOAD_COLORTERM)
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

pub(super) fn spawn_startup_thread<F>(
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

pub(super) fn start_worker_threads(
    runtime: Arc<WorkerRuntime>,
    master_read: File,
    child_slot: Arc<Mutex<Option<Child>>>,
    commit_ready: impl FnOnce() -> Result<()>,
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
                    let pid = child.id();
                    drop(child);
                    // Dropping a `Child` does not reap it, and this thread
                    // is about to stop owning it, so hand the pid back.
                    disown_child_pid(pid);
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

        // Armed here, after every startup helper (`systemd-run`, `systemctl`)
        // has already been spawned *and* waited, and after the workload
        // leader has been registered as owned. From this point every child
        // that appears under this worker is an adopted descendant.
        let child_event_fd = install_child_reaper_handler()?;
        spawn_startup_thread("reaper", 6, &gate, &mut handles, move || {
            run_child_reaper(child_event_fd)
        })?;
        startup_checkpoint("after_thread_setup")?;
        // Every required thread now exists but is still held behind `gate`.
        // Publish Running only at this commit point; a failed commit aborts
        // and joins the complete pending set before any thread can act.
        commit_ready()?;
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

/// History is durable local state, not an IPC endpoint. Reject accidental
/// FIFOs, directories, devices, and symlinks before `History::open` can block
/// or consume an unrelated file while worker startup is still in flight.
pub(super) fn validate_existing_history_node(path: &std::path::Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => bail!(
            "history path {} exists but is not a regular file",
            path.display()
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspect history path {}", path.display()))
        }
    }
}

pub(super) fn run_periodic_flush(runtime: Arc<WorkerRuntime>) {
    // Debounced history persistence (see History::append) needs a periodic
    // sweep so output followed by silence still reaches disk. The same tick
    // persists last_activity_ms, but only when it has changed.
    let mut persisted_activity_ms: u64 = 0;
    loop {
        thread::sleep(HISTORY_FLUSH_INTERVAL);
        if let Err(error) = runtime.output.flush() {
            eprintln!("aplexer worker: flush history: {error:#}");
        }
        if let Err(error) = persist_activity_checkpoint(&runtime, &mut persisted_activity_ms) {
            eprintln!("aplexer worker: persist activity: {error:#}");
        }
    }
}

/// Publish the debounce checkpoint only after the matching record update is
/// durable. Leaving `persisted_activity_ms` unchanged on failure makes the
/// next periodic tick retry even when no further PTY output arrives.
pub(super) fn persist_activity_checkpoint(
    runtime: &WorkerRuntime,
    persisted_activity_ms: &mut u64,
) -> Result<()> {
    let current = runtime.last_activity_ms.load(Ordering::Relaxed);
    if current == 0 || current == *persisted_activity_ms {
        return Ok(());
    }
    runtime.update_record(|record| record.last_activity_ms = Some(current))?;
    *persisted_activity_ms = current;
    Ok(())
}

pub(super) fn run_termination_monitor(runtime: Arc<WorkerRuntime>) {
    if let Err(error) = wait_for_termination_request() {
        eprintln!("aplexer worker: wait for termination request: {error:#}");
        return;
    }
    if let Err(error) = runtime.kill(libc::SIGTERM, 500) {
        eprintln!("aplexer worker: terminate contained workload: {error:#}");
        let _ = runtime.kill(libc::SIGKILL, 0);
    }
}

pub(super) fn run_pty_reader(
    mut master: File,
    runtime: Arc<WorkerRuntime>,
    tx: mpsc::Sender<LifeEvent>,
) {
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
                    // Hub lock poison is the only remaining append error;
                    // history write failures stay on history_persistence_error.
                    // Never treat either as a PTY/workload failure.
                    eprintln!("aplexer worker: append output: {error:#}");
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

pub(super) fn run_child_waiter(mut child: Child, tx: mpsc::Sender<LifeEvent>) {
    let pid = child.id();
    let event = match child.wait() {
        Ok(status) => LifeEvent::ChildExit {
            code: status.code(),
            signal: status.signal(),
        },
        Err(error) => LifeEvent::WaiterError(format!("wait workload: {error}")),
    };
    // Only now: this pid's exit status has been collected (or the wait
    // failed, so nobody owns it any more) and the number is free to be
    // recycled by a descendant the reaper must be able to consume.
    disown_child_pid(pid);
    let _ = tx.send(event);
}
