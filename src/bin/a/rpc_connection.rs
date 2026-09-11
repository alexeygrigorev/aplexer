use super::*;
use std::os::fd::RawFd;

#[cfg(not(test))]
pub(crate) const CONTROL_RPC_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(test)]
pub(crate) const CONTROL_RPC_TIMEOUT: Duration = Duration::from_millis(100);

/// The attach handshake's reply budget. Serving an `Attach` does disk-bound
/// work on the worker -- and the hub lock every subscribe must wait behind
/// is the same lock the periodic history flush holds across its fsync -- so
/// on a busy or nearly-full disk the reply can legitimately outrun
/// `CONTROL_RPC_TIMEOUT`. A client that gave up at 3s left the worker
/// writing its handshake into a closed socket ("aplexer connection: Broken
/// pipe" in worker.log) while the user saw the attach die: the recurring
/// "disconnected on attach". `Kill` already needed the same escape hatch
/// (`rpc_call_within`); the handshake gets a wider budget, and `establish`
/// retries deadline expiries on top because they are transient by nature.
#[cfg(not(test))]
pub(crate) const ATTACH_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
#[cfg(test)]
pub(crate) const ATTACH_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(200);

pub(crate) fn set_control_deadlines(stream: &UnixStream) -> Result<()> {
    stream
        .set_read_timeout(Some(CONTROL_RPC_TIMEOUT))
        .context("set worker response deadline")?;
    stream
        .set_write_timeout(Some(CONTROL_RPC_TIMEOUT))
        .context("set worker request deadline")?;
    Ok(())
}

pub(crate) fn clear_streaming_deadlines(stream: &UnixStream) -> Result<()> {
    stream
        .set_read_timeout(None)
        .context("clear attach streaming read deadline")?;
    stream
        .set_write_timeout(None)
        .context("clear attach streaming write deadline")?;
    Ok(())
}

fn connect_timed_out(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("connect {} timed out", path.display()),
    )
}

/// Build the AF_UNIX address for `path`, rejecting interior NUL bytes and
/// `sun_path` overflow before any descriptor exists.
fn sockaddr_un_for_path(path: &Path) -> io::Result<(libc::sockaddr_un, libc::socklen_t)> {
    let path_bytes = path.as_os_str().as_bytes();
    let _ = CString::new(path_bytes)
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
    Ok((address, address_len))
}

/// What the connect loop should do after one poll round on an in-flight
/// (EINPROGRESS/EALREADY) connection attempt.
enum ConnectPoll {
    Connected,
    Retry,
    TimedOut,
    Failed(io::Error),
}

/// Wait until the in-flight connect makes the fd writable, then classify
/// the outcome via SO_ERROR.
fn poll_in_flight_connect(fd: RawFd, deadline: Instant) -> ConnectPoll {
    let now = Instant::now();
    if now >= deadline {
        return ConnectPoll::TimedOut;
    }
    let remaining = deadline.saturating_duration_since(now);
    let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
    if ready < 0 {
        let poll_error = io::Error::last_os_error();
        if poll_error.kind() == io::ErrorKind::Interrupted {
            return ConnectPoll::Retry;
        }
        return ConnectPoll::Failed(poll_error);
    }
    if ready == 0 {
        return ConnectPoll::TimedOut;
    }
    settle_polled_connect(fd)
}

/// Writable does not yet mean connected: read SO_ERROR, and confirm real
/// peer attachment with getpeername before trusting the result.
fn settle_polled_connect(fd: RawFd) -> ConnectPoll {
    let mut socket_error: libc::c_int = 0;
    let mut socket_error_len = std::mem::size_of_val(&socket_error) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&raw mut socket_error).cast::<libc::c_void>(),
            &raw mut socket_error_len,
        )
    } != 0
    {
        return ConnectPoll::Failed(io::Error::last_os_error());
    }
    if socket_error == 0 {
        // EINPROGRESS completes here. EAGAIN can also become writable
        // without having queued a connection, in which case retrying
        // connect above distinguishes success from another EAGAIN.
        let peer_len_result = unsafe {
            let mut peer: libc::sockaddr_un = std::mem::zeroed();
            let mut peer_len = std::mem::size_of_val(&peer) as libc::socklen_t;
            libc::getpeername(
                fd,
                (&raw mut peer).cast::<libc::sockaddr>(),
                &raw mut peer_len,
            )
        };
        if peer_len_result == 0 {
            return ConnectPoll::Connected;
        }
        return ConnectPoll::Retry;
    }
    if socket_error == libc::EAGAIN || socket_error == libc::EINPROGRESS {
        return ConnectPoll::Retry;
    }
    ConnectPoll::Failed(io::Error::from_raw_os_error(socket_error))
}

fn open_nonblocking_socket() -> io::Result<OwnedFd> {
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
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}

/// Retry the (non-blocking) connect until it succeeds or the deadline runs
/// out, dispatching the in-flight states to [`poll_in_flight_connect`].
fn drive_connect(
    fd: &OwnedFd,
    address: &libc::sockaddr_un,
    address_len: libc::socklen_t,
    path: &Path,
    deadline: Instant,
) -> io::Result<()> {
    loop {
        let connected = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&raw const *address).cast::<libc::sockaddr>(),
                address_len,
            )
        };
        if connected == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => return Ok(()),
            // AF_UNIX reports EAGAIN rather than EINPROGRESS when its listen
            // backlog is full. In that case no connection attempt is queued,
            // so retry with a small backoff until the same absolute deadline.
            // Polling this unconnected fd can report POLLOUT immediately and
            // would otherwise turn a stopped worker into a busy-spin.
            Some(libc::EAGAIN) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(connect_timed_out(path));
                }
                thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Some(libc::EINPROGRESS) | Some(libc::EALREADY) => {
                match poll_in_flight_connect(fd.as_raw_fd(), deadline) {
                    ConnectPoll::Connected => return Ok(()),
                    ConnectPoll::Retry => {}
                    ConnectPoll::TimedOut => return Err(connect_timed_out(path)),
                    ConnectPoll::Failed(error) => return Err(error),
                }
            }
            _ => return Err(error),
        }
    }
}

/// The control socket is only ever used synchronously after connect.
fn set_blocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn connect_with_timeout(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    let (address, address_len) = sockaddr_un_for_path(path)?;
    let fd = open_nonblocking_socket()?;
    let deadline = Instant::now() + timeout;
    drive_connect(&fd, &address, address_len, path, deadline)?;
    set_blocking(fd.as_raw_fd())?;
    Ok(unsafe { UnixStream::from_raw_fd(fd.into_raw_fd()) })
}

pub(crate) fn connect(record: &SessionRecord) -> Result<UnixStream> {
    let stream = connect_with_timeout(&record.socket_path, CONTROL_RPC_TIMEOUT)
        .with_context(|| format!("connect {}", record.socket_path.display()))?;
    set_control_deadlines(&stream)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_address_carries_the_path_and_family() {
        let path = "/tmp/aplexer-test.sock";
        let (address, len) = sockaddr_un_for_path(Path::new(path)).unwrap();
        assert_eq!(address.sun_family, libc::AF_UNIX as libc::sa_family_t);
        let stored: Vec<u8> = address.sun_path[..path.len()]
            .iter()
            .map(|&byte| byte as u8)
            .collect();
        assert_eq!(stored, path.as_bytes());
        assert_eq!(
            len as usize,
            std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1
        );
    }

    #[test]
    fn socket_address_rejects_an_interior_nul() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(std::ffi::OsString::from_vec(b"/tmp/sock\0hidden".to_vec()));
        let error = sockaddr_un_for_path(&path).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn socket_address_rejects_an_oversized_path() {
        let long = format!("/tmp/{}", "segment/".repeat(20));
        let error = sockaddr_un_for_path(Path::new(&long)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn connect_timeout_error_names_the_socket() {
        let error = connect_timed_out(Path::new("/run/aplexer/missing.sock"));
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("/run/aplexer/missing.sock"));
    }
}
