//! Process primitives: the SIGCHLD contract for child management,
//! kill-grace validation, `/proc` liveness and identity probing (pid reuse,
//! zombies, boot id), pidfd creation, session-id discovery from ancestor
//! environments, and the PTY/exec helpers used to spawn workers.

use anyhow::{anyhow, bail, Context, Result};
use std::env;
use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

use crate::executable_available;

/// Long enough for graceful shutdown, but bounded so an authenticated local
/// client cannot monopolize a worker's serialized kill path indefinitely.
pub const MAX_KILL_GRACE_MS: u64 = 30_000;

/// Restore the standalone process contract needed by `std::process::Child`.
/// This changes a process-wide disposition and is therefore reserved for the
/// CLI and worker binaries, never the embeddable Rust/Python API.
pub fn normalize_sigchld_for_child_management() -> Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error()).context("restore SIGCHLD default disposition");
        }
    }
    Ok(())
}

/// Validate, without changing it, the embedding process's SIGCHLD contract.
/// Custom handlers remain installed. SIG_IGN and SA_NOCLDWAIT are rejected
/// because either may auto-reap a worker before the API can wait for it.
pub fn ensure_sigchld_compatible_for_child_management() -> Result<()> {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) != 0 {
            return Err(io::Error::last_os_error()).context("inspect SIGCHLD disposition");
        }
        if action.sa_sigaction == libc::SIG_IGN {
            bail!(
                "SIGCHLD disposition is SIG_IGN; in-process session startup requires child wait ownership"
            );
        }
        if action.sa_flags & libc::SA_NOCLDWAIT != 0 {
            bail!(
                "SIGCHLD disposition uses SA_NOCLDWAIT; in-process session startup requires child wait ownership"
            );
        }
    }
    Ok(())
}

pub fn kill_grace_duration(grace_ms: u64) -> Result<Duration> {
    if grace_ms > MAX_KILL_GRACE_MS {
        bail!("kill grace exceeds maximum of {MAX_KILL_GRACE_MS} ms");
    }
    Ok(Duration::from_millis(grace_ms))
}


/// Session identity for `a whoami` / bare `a transcript` / messaging.
///
/// Prefer `APLEXER_SESSION_ID` on this process (the worker stamps it on the
/// workload). If a tool subprocess cleared its environment, walk parent
/// `/proc/<pid>/environ` until we find the stamp -- agent CLIs often spawn
/// `bash`/`env -i` without passing the aplexer vars through.
pub fn discover_session_id() -> Option<Uuid> {
    parse_session_id_env(env::var_os("APLEXER_SESSION_ID"))
        .or_else(session_id_from_ancestor_environ)
}

pub(crate) fn parse_session_id_env(raw: Option<std::ffi::OsString>) -> Option<Uuid> {
    let raw = raw?.into_string().ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    raw.parse().ok()
}

pub(crate) fn session_id_from_ancestor_environ() -> Option<Uuid> {
    let mut pid = proc_ppid(std::process::id())?;
    for _ in 0..64 {
        if pid == 0 {
            break;
        }
        if let Some(id) = session_id_in_proc_environ(pid) {
            return Some(id);
        }
        let next = proc_ppid(pid)?;
        if next == pid {
            break;
        }
        pid = next;
    }
    None
}

pub(crate) fn proc_ppid(pid: u32) -> Option<u32> {
    let text = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

pub(crate) fn session_id_in_proc_environ(pid: u32) -> Option<Uuid> {
    let bytes = fs::read(format!("/proc/{pid}/environ")).ok()?;
    for entry in bytes.split(|b| *b == 0) {
        let Ok(s) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some(val) = s.strip_prefix("APLEXER_SESSION_ID=") {
            if let Ok(id) = val.parse() {
                return Some(id);
            }
        }
    }
    None
}

/// Whether `pid` names a process that can still run code.
///
/// `kill(pid, 0)` alone is NOT that question. It succeeds for a zombie: an
/// exited process whose parent has not yet reaped it still occupies its pid
/// slot and still accepts (and discards) signals. Every liveness decision in
/// aplexer -- `worker_alive`, `workload_leader_alive`, and through them
/// `reap_verdict` and `a prune` -- is really asking "is there anything left
/// that could act", and a zombie's answer is no.
///
/// This matters because aplexer workers are child subreapers
/// (`PR_SET_CHILD_SUBREAPER`), so a session started from inside another
/// aplexer session reparents to that outer worker when its own parent goes
/// away. If the outer worker does not reap it, the dead session's pid stays
/// signalable indefinitely and every probe here reported it alive forever:
/// `a prune` retained records it should have removed, and tests that wait
/// for a pid to die failed with "pid NNNN did not die".
///
/// Uncertainty still fails closed: an unreadable `/proc/<pid>/stat` (a
/// hardened procfs, a racing exit) leaves the answer at the signalable
/// result, so a live process is never mistaken for a dead one.
pub fn process_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    let signalable = rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    signalable && !process_is_zombie(pid)
}

/// The single-character run state from field 3 of `/proc/<pid>/stat`
/// (`R` running, `S`/`D` sleeping, `T` stopped, `Z` zombie, `X` dead).
pub fn process_state(pid: u32) -> Result<char> {
    process_state_in(Path::new(crate::agent_kind::DEFAULT_PROC_ROOT), pid)
}

/// `process_state` against an arbitrary `/proc` root, so the zombie rules
/// below can be pinned by ordinary unit tests on a synthetic tree instead of
/// requiring a real process in a specific state -- the same split
/// `direct_child_pids_in` uses.
pub(crate) fn process_state_in(proc_root: &Path, pid: u32) -> Result<char> {
    let stat_path = proc_root.join(pid.to_string()).join("stat");
    let stat =
        fs::read_to_string(&stat_path).with_context(|| format!("read {}", stat_path.display()))?;
    // The parenthesized comm field may itself contain spaces or `)`, so the
    // state is the first token after its final close-paren, never field 3 of
    // a naive whitespace split.
    stat.rfind(')')
        .and_then(|end| stat.get(end + 1..))
        .and_then(|after_comm| after_comm.split_whitespace().next())
        .and_then(|state| state.chars().next())
        .ok_or_else(|| anyhow!("malformed {}", stat_path.display()))
}

/// Whether `pid` has exited but has not been reaped by its parent.
///
/// The `Z` in `/proc/<pid>/stat` is necessary but not sufficient. A thread
/// group leader that called `pthread_exit` (or bare `exit(2)`) while its
/// sibling threads keep running also reads as `Z`, and that process is very
/// much still executing code -- measured, not assumed: such a leader shows
/// `state=Z` with two entries under `/proc/<pid>/task`. Calling it dead
/// would let a multi-threaded workload be declared contained while its
/// threads ran on. So the thread group must also be down to nothing but the
/// leader's corpse, which is exactly the state `waitpid` will return for.
///
/// Every unreadable answer is reported as "not a zombie": callers use this
/// to subtract the dead from a liveness answer, and an unknown state must
/// never subtract a process that may still be running.
pub fn process_is_zombie(pid: u32) -> bool {
    process_is_zombie_in(Path::new(crate::agent_kind::DEFAULT_PROC_ROOT), pid)
}

pub(crate) fn process_is_zombie_in(proc_root: &Path, pid: u32) -> bool {
    matches!(process_state_in(proc_root, pid), Ok('Z'))
        && thread_group_holds_only_the_leader(proc_root, pid)
}

/// Whether `<proc>/<pid>/task` contains exactly one entry, i.e. no sibling
/// thread of `pid` is left. Any read failure answers `false`, keeping the
/// caller on the "may still be running" side.
pub(crate) fn thread_group_holds_only_the_leader(proc_root: &Path, pid: u32) -> bool {
    let Ok(tasks) = fs::read_dir(proc_root.join(pid.to_string()).join("task")) else {
        return false;
    };
    let mut seen = 0_usize;
    for task in tasks {
        if task.is_err() {
            return false;
        }
        seen += 1;
        if seen > 1 {
            return false;
        }
    }
    seen == 1
}

/// Linux process start time (field 22 of `/proc/<pid>/stat`), measured in
/// clock ticks since boot. Combined with the pid, this distinguishes a
/// persisted process from a later process that reused its numeric pid.
pub fn process_start_time_ticks(pid: u32) -> Result<u64> {
    let stat_path = format!("/proc/{pid}/stat");
    let stat = fs::read_to_string(&stat_path).with_context(|| format!("read {stat_path}"))?;
    // The parenthesized comm field may itself contain spaces or `)`, so split
    // after its final close-paren rather than tokenizing the whole line.
    let after_comm = stat
        .rfind(')')
        .and_then(|end| stat.get(end + 1..))
        .ok_or_else(|| anyhow!("malformed {stat_path}"))?;
    after_comm
        .split_whitespace()
        .nth(19) // field 3 is index 0 here; starttime is field 22
        .ok_or_else(|| anyhow!("{stat_path} has no process start time"))?
        .parse()
        .with_context(|| format!("parse process start time from {stat_path}"))
}

pub(crate) fn linux_boot_id() -> Result<String> {
    // Cached: the boot id cannot change without a reboot, and every
    // `worker_alive` probe (i.e. every `a list` row, twice per row in the old
    // plain rendering) read it from disk. Benchmark PLAN P1.1: `a list` with
    // dozens of sessions did dozens of redundant reads of this one tiny
    // file; cache it process-wide after the first successful read.
    static CACHED_BOOT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if let Some(cached) = CACHED_BOOT_ID.get() {
        return Ok(cached.clone());
    }
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .context("read Linux boot identity")?;
    let boot_id = boot_id.trim();
    if boot_id.is_empty() {
        bail!("Linux boot identity is empty");
    }
    let owned = boot_id.to_owned();
    let _ = CACHED_BOOT_ID.set(owned.clone());
    Ok(owned)
}

pub(crate) fn pidfd_open(pid: u32) -> io::Result<File> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}


pub fn command_exists(command: &[String]) -> bool {
    command
        .first()
        .map(|p| executable_available(p))
        .unwrap_or(false)
}

pub fn worker_executable() -> Result<PathBuf> {
    if let Some(path) = env::var_os("APLEXER_WORKER") {
        return Ok(PathBuf::from(path));
    }
    let current = env::current_exe()?;
    if let Some(parent) = current.parent() {
        let sibling = parent.join("aplexer");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    Ok(PathBuf::from("aplexer"))
}

pub fn set_cloexec(fd: RawFd, enabled: bool) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let next = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, next) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn open_pty(rows: u16, cols: u16) -> Result<(File, File)> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(io::Error::last_os_error()).context("posix_openpt");
    }
    let cleanup_master = || unsafe {
        libc::close(master);
    };
    if unsafe { libc::grantpt(master) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("grantpt");
    }
    if unsafe { libc::unlockpt(master) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("unlockpt");
    }
    // `libc::c_char` is unsigned on some Linux architectures (including
    // aarch64), so keep the buffer's element type aligned with libc rather
    // than assuming x86_64's signed `char`.
    let mut name = vec![0 as libc::c_char; 256];
    if unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("ptsname_r");
    }
    let slave = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave < 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("open PTY slave");
    }
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
    }
    Ok(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
}

pub fn set_winsize(fd: RawFd, rows: u16, cols: u16) -> Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(io::Error::last_os_error()).context("TIOCSWINSZ");
    }
    Ok(())
}

/// The name (from `/proc/<pgid>/comm`) of whatever is currently in the
/// foreground of the pty referred to by `fd` -- the same mechanism tmux
/// uses for `pane_current_command`: `tcgetpgrp(fd)` to get the foreground
/// process group of the pty (this updates automatically as the shell
/// forks/foregrounds jobs, standard POSIX job control -- no polling of the
/// workload itself needed), then read that pgid's name straight out of
/// procfs. `comm` is used over parsing `/proc/<pid>/stat`'s second field
/// because it's already a single line stripped of parens and args.
///
/// `fd` need not be `fd`'s own controlling terminal -- this is exactly how
/// tmux's server (which is not part of the pane's session) queries a pty
/// it merely holds the master side of. Best-effort throughout: any failure
/// (no foreground group yet, the process exited between the two syscalls,
/// procfs unmounted) yields `None` rather than an error, since this is a
/// cosmetic status-bar signal, never something worth failing a request or
/// blocking a hot loop over.
pub fn foreground_command(fd: RawFd) -> Option<String> {
    let pgid = unsafe { libc::tcgetpgrp(fd) };
    if pgid <= 0 {
        return None;
    }
    let comm = fs::read_to_string(format!("/proc/{pgid}/comm")).ok()?;
    let trimmed = comm.trim_end_matches('\n');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn peer_uid(fd: RawFd) -> Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut _,
            &mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("SO_PEERCRED");
    }
    Ok(cred.uid)
}

pub fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

pub fn c_string(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("path contains NUL")
}

