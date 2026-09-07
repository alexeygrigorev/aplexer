//! The worker's process-wide signal and child-reaping contract.
//!
//! One reason to exist: the worker must end only when asked -- SIGTERM and
//! SIGINT land in an eventfd waker instead of racing the poll loop -- and
//! every child it owns must be reaped exactly once, by the one reaper
//! thread that owns waitpid for it, including descendants adopted through
//! the subreaper after their parent died. The pid set here is the boundary
//! the rest of the worker negotiates with (own_child_pid before spawning a
//! helper, disown_child_pid when handing the status back).

use super::*;

pub(super) static TERMINATION_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
pub(super) static TERMINATION_EVENT_FD: AtomicI32 = AtomicI32::new(-1);

pub(super) fn notify_event_fd(fd: RawFd) {
    let value = 1u64;
    unsafe {
        libc::write(
            fd,
            (&value as *const u64).cast::<libc::c_void>(),
            std::mem::size_of::<u64>(),
        );
    }
}

extern "C" fn request_worker_termination(_: libc::c_int) {
    TERMINATION_REQUESTED.store(true, Ordering::SeqCst);
    let fd = TERMINATION_EVENT_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // `write(2)` is async-signal-safe. A nonblocking eventfd coalesces
        // repeated TERM/INT delivery into one readable counter without a
        // mutex, allocator, or periodic polling in the monitor thread.
        notify_event_fd(fd);
    }
}

pub(super) fn create_worker_event_fd(context: &'static str) -> Result<RawFd> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("create worker {context} eventfd"));
    }
    Ok(fd)
}

pub(super) fn wait_for_event_fd(fd: RawFd, context: &'static str) -> Result<()> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let ready = unsafe { libc::poll(&mut pollfd, 1, -1) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).with_context(|| format!("wait for worker {context} event"));
        }
        if pollfd.revents & libc::POLLIN != 0 {
            let mut value = 0u64;
            let read = unsafe {
                libc::read(
                    fd,
                    (&mut value as *mut u64).cast::<libc::c_void>(),
                    std::mem::size_of::<u64>(),
                )
            };
            if read == std::mem::size_of::<u64>() as isize {
                return Ok(());
            }
            if read < 0 {
                let error = io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                ) {
                    continue;
                }
                return Err(error).with_context(|| format!("read worker {context} event"));
            }
            bail!("short read from worker {context} eventfd: {read}");
        }
        if pollfd.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            bail!("worker {context} eventfd became invalid");
        }
    }
}

pub(super) fn wait_for_termination_request() -> Result<()> {
    while !TERMINATION_REQUESTED.load(Ordering::SeqCst) {
        let fd = TERMINATION_EVENT_FD.load(Ordering::SeqCst);
        if fd < 0 {
            bail!("worker termination eventfd is not installed");
        }
        wait_for_event_fd(fd, "termination")?;
    }
    Ok(())
}

/// The launcher blocks TERM/INT before exec so no timeout signal can land in
/// the gap before these handlers exist. Install first, then explicitly
/// unblock; a pending signal is delivered to the handler and becomes a normal
/// startup cancellation whose guard can unwind all resources.
pub(super) fn install_termination_handlers() -> Result<()> {
    TERMINATION_REQUESTED.store(false, Ordering::SeqCst);
    let event_fd = create_worker_event_fd("termination")?;
    TERMINATION_EVENT_FD.store(event_fd, Ordering::SeqCst);
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = request_worker_termination as *const () as usize;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        for signal in [libc::SIGTERM, libc::SIGINT] {
            if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
                TERMINATION_EVENT_FD.store(-1, Ordering::SeqCst);
                libc::close(event_fd);
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
            TERMINATION_EVENT_FD.store(-1, Ordering::SeqCst);
            libc::close(event_fd);
            return Err(io::Error::from_raw_os_error(rc))
                .context("unblock worker termination signals");
        }
    }
    Ok(())
}

// --- Adopted-descendant reaping -------------------------------------------
//
// The worker is a child subreaper (`enable_child_subreaper`), so every
// process in the workload's tree that is orphaned reparents to *this*
// process instead of to init. Until this reaper existed the worker only
// waited for those adoptees on its own way out, so a long-lived session --
// an agent session that lives for days, running one orphaning helper after
// another -- accumulated `Z` zombies for its entire lifetime. Measured on a
// developer machine: 5728 zombies across the running workers, one worker
// holding 1162 of them, growing by hundreds per hour. Each one holds a pid
// slot, and each one is a process that `kill(pid, 0)` still reports as
// signalable, which is how they corrupted liveness answers as well (see
// `process_alive`).

pub(super) static CHILD_EVENT_FD: AtomicI32 = AtomicI32::new(-1);

/// Pids of children this worker spawned and intends to wait on itself.
///
/// The reaper must never consume one of these. `run_child_waiter`'s
/// `Child::wait` is what turns the workload leader's death into the
/// session's recorded exit code and signal; a `waitpid` that got there first
/// would hand that status to a thread that discards it and leave the real
/// owner with ECHILD, so the session would finish with no exit status at
/// all. That would be a far worse bug than the leak this fixes.
///
/// So the reaper is targeted, not clever: it never calls `waitpid(-1)`. It
/// walks its own children in procfs, skips every pid registered here, and
/// waits only on a specific unowned pid. A `waitpid(-1)` sweep -- even one
/// that peeked with `WNOWAIT` first -- would have to be sequenced against
/// every helper wait in the process to stay safe; refusing to ever name -1
/// removes the class of bug instead of arguing about it.
///
/// The worker's other self-waited children (the `systemd-run` scope anchor
/// and the `systemctl` query helpers in `Cgroup::create`) are all spawned
/// *and* waited during startup, before `start_worker_threads` creates the
/// reaper thread, so they can never be observed by it. They are registered
/// anyway, so the invariant does not silently depend on that ordering.
pub(super) static OWNED_CHILD_PIDS: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());

/// Claim `pid` before anything can wait on it. Must be called on the
/// spawning thread, between `Command::spawn` returning and the pid becoming
/// reachable by the reaper.
pub(crate) fn own_child_pid(pid: u32) {
    if let Ok(mut owned) = OWNED_CHILD_PIDS.lock() {
        owned.insert(pid);
    }
}

/// Release `pid` only *after* its owner's `wait` has returned. Releasing
/// earlier would reopen exactly the status-stealing race this set prevents;
/// releasing at all matters because pids are recycled, and a later adopted
/// descendant that reuses this number must still be reapable.
pub(crate) fn disown_child_pid(pid: u32) {
    if let Ok(mut owned) = OWNED_CHILD_PIDS.lock() {
        owned.remove(&pid);
    }
}

/// A poisoned registry reports every pid as owned: the failure mode of
/// leaking a zombie is recoverable, and the failure mode of eating the
/// workload's exit status is not.
pub(super) fn child_pid_is_owned(pid: u32) -> bool {
    OWNED_CHILD_PIDS
        .lock()
        .map(|owned| owned.contains(&pid))
        .unwrap_or(true)
}

/// SIGCHLD is the wakeup, never the work. The handler does one
/// async-signal-safe `write(2)` to a nonblocking eventfd; all waiting and
/// procfs reading happens on the reaper thread.
extern "C" fn note_child_state_change(_: libc::c_int) {
    let fd = CHILD_EVENT_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        notify_event_fd(fd);
    }
}

/// Arm SIGCHLD delivery for the reaper thread.
///
/// A SIGCHLD-driven drain is chosen over a periodic `waitpid` sweep because
/// an idle worker must stay genuinely idle: `tests/worker_idle_wakeups.rs`
/// measures voluntary context switches of the worker's resident threads, and
/// the benchmark budget counts every timer. A sweep on a timer would either
/// wake thousands of times an hour to find nothing (the thing that test
/// exists to prevent) or run rarely enough that a fork-heavy session still
/// accumulates zombies between sweeps. Reaping on the signal costs exactly
/// one wakeup per adopted descendant that dies and nothing at all otherwise,
/// which is the correct shape for an event that is genuinely an event.
pub(super) fn install_child_reaper_handler() -> Result<RawFd> {
    let event_fd = create_worker_event_fd("child exit")?;
    CHILD_EVENT_FD.store(event_fd, Ordering::SeqCst);
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = note_child_state_change as *const () as usize;
        // SA_NOCLDSTOP: only exits are interesting. Without it a workload
        // that is merely stopped and continued (Ctrl-Z at a shell, a
        // debugger) would wake the reaper for nothing.
        //
        // SA_RESTART: the worker's other threads sit in blocking reads on
        // the PTY master and on client sockets. A descendant dying must not
        // surface there as a spurious EINTR.
        action.sa_flags = libc::SA_RESTART | libc::SA_NOCLDSTOP;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
            CHILD_EVENT_FD.store(-1, Ordering::SeqCst);
            libc::close(event_fd);
            return Err(io::Error::last_os_error()).context("install SIGCHLD handler");
        }
        let mut unblocked: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut unblocked);
        libc::sigaddset(&mut unblocked, libc::SIGCHLD);
        let rc = libc::pthread_sigmask(libc::SIG_UNBLOCK, &unblocked, std::ptr::null_mut());
        if rc != 0 {
            CHILD_EVENT_FD.store(-1, Ordering::SeqCst);
            libc::close(event_fd);
            return Err(io::Error::from_raw_os_error(rc)).context("unblock SIGCHLD");
        }
    }
    Ok(event_fd)
}

/// Wait for one specific child, without blocking and without ever naming
/// `-1`. Returns whether a zombie was actually consumed.
pub(super) fn reap_child_pid(pid: u32) -> Result<bool> {
    loop {
        let mut status = 0;
        let rc = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
        if rc > 0 {
            return Ok(true);
        }
        // Still running: its eventual exit raises SIGCHLD, which brings us
        // back here. Nothing to wait for now.
        if rc == 0 {
            return Ok(false);
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        // ECHILD: the pid stopped being our child between the procfs walk
        // and this call -- one of the worker's own post-exit drains got
        // there first. Not an error, just a lost race with ourselves.
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(false);
        }
        return Err(error).with_context(|| format!("reap adopted descendant {pid}"));
    }
}

/// Consume every dead direct child that this worker does not own.
///
/// Enumerating procfs on each pass rather than looping on a single wait is
/// what makes signal coalescing harmless: several descendants dying at once
/// may raise a single SIGCHLD, and this still finds all of them.
pub(super) fn reap_adopted_descendants() -> Result<usize> {
    let mut reaped = 0;
    for pid in direct_child_pids(std::process::id())? {
        if child_pid_is_owned(pid) {
            continue;
        }
        if reap_child_pid(pid)? {
            reaped += 1;
        }
    }
    Ok(reaped)
}

/// Sweep, then block. The order is the correctness argument:
///
///   * a descendant adopted and dead before this thread was released from
///     the startup gate is caught by the first sweep;
///   * a descendant that dies *during* a sweep, after its own pid was
///     already passed over, has still written to the eventfd by then, so the
///     following `wait_for_event_fd` returns immediately and the next sweep
///     sees it.
///
/// There is therefore no timer and no backstop poll: an idle worker with no
/// dying descendants performs zero wakeups here, which is what
/// `tests/worker_idle_wakeups.rs` asserts.
pub(super) fn run_child_reaper(event_fd: RawFd) {
    loop {
        if let Err(error) = reap_adopted_descendants() {
            eprintln!("aplexer worker: reap adopted descendants: {error:#}");
        }
        if let Err(error) = wait_for_event_fd(event_fd, "child exit") {
            eprintln!("aplexer worker: wait for child exit event: {error:#}");
            return;
        }
    }
}
