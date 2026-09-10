//! The control socket: accepting one connection at a time under a poll
//! timeout, and proving the socket path (and the worker lock it depends on)
//! still belong to this worker after cleanup software has been through the
//! runtime directory.

use super::*;

pub(super) type FileIdentity = (u64, u64);
pub(super) type RecoveredControlSocket =
    (UnixListener, FileIdentity, Option<FileLock>, FileIdentity);
pub(super) fn poll_control_connection(
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

pub(super) fn control_socket_matches_identity(
    path: &std::path::Path,
    identity: (u64, u64),
) -> bool {
    trusted_socket_identity(path).is_ok_and(|current| current == identity)
}

/// The filesystem socket node and the open listener descriptor do not share
/// an inode on Linux. Capture the pathname's identity immediately after bind
/// and compare later pathname metadata against that stable identity instead
/// of comparing `lstat(path)` with `fstat(listener)` (which always differs and
/// caused an unnecessary rebind every idle health-check interval).
pub(super) fn trusted_socket_identity(path: &std::path::Path) -> Result<(u64, u64)> {
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
pub(super) fn trusted_lock_identity(path: &std::path::Path) -> Result<(u64, u64)> {
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

pub(super) fn recover_control_socket(
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
        Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => None,
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
