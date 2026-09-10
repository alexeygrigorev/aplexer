//! Reaching a worker that is still coming up: a bounded non-blocking
//! connect to its control socket, and the readiness Ping that requires it
//! to identify the exact session the launcher just spawned.

use super::*;

/// One readiness probe's share of the startup budget: a single bounded
/// connect-and-Ping so the poll loop keeps re-reading the record between
/// attempts instead of blocking on one socket.
pub(super) const STARTUP_READY_RPC_SLICE: Duration = Duration::from_millis(100);

pub(crate) fn connect_startup_control(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
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
pub(super) fn probe_worker_ready(
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
