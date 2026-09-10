use super::*;

#[cfg(not(test))]
pub(crate) const CONTROL_RPC_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(test)]
pub(crate) const CONTROL_RPC_TIMEOUT: Duration = Duration::from_millis(100);

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

pub(crate) fn connect_with_timeout(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
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
    let deadline = Instant::now() + timeout;
    loop {
        let connected = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&raw const address).cast::<libc::sockaddr>(),
                address_len,
            )
        };
        if connected == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EISCONN) => break,
            // AF_UNIX reports EAGAIN rather than EINPROGRESS when its listen
            // backlog is full. In that case no connection attempt is queued,
            // so retry with a small backoff until the same absolute deadline.
            // Polling this unconnected fd can report POLLOUT immediately and
            // would otherwise turn a stopped worker into a busy-spin.
            Some(libc::EAGAIN) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("connect {} timed out", path.display()),
                    ));
                }
                thread::sleep(remaining.min(Duration::from_millis(10)));
                continue;
            }
            Some(libc::EINPROGRESS) | Some(libc::EALREADY) => {}
            _ => return Err(error),
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("connect {} timed out", path.display()),
            ));
        }
        let remaining = deadline.saturating_duration_since(now);
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut poll_fd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
        if ready < 0 {
            let poll_error = io::Error::last_os_error();
            if poll_error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(poll_error);
        }
        if ready == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("connect {} timed out", path.display()),
            ));
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
        if socket_error == 0 {
            // EINPROGRESS completes here. EAGAIN can also become writable
            // without having queued a connection, in which case retrying
            // connect above distinguishes success from another EAGAIN.
            let peer_len_result = unsafe {
                let mut peer: libc::sockaddr_un = std::mem::zeroed();
                let mut peer_len = std::mem::size_of_val(&peer) as libc::socklen_t;
                libc::getpeername(
                    fd.as_raw_fd(),
                    (&raw mut peer).cast::<libc::sockaddr>(),
                    &raw mut peer_len,
                )
            };
            if peer_len_result == 0 {
                break;
            }
        } else if socket_error != libc::EAGAIN && socket_error != libc::EINPROGRESS {
            return Err(io::Error::from_raw_os_error(socket_error));
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

pub(crate) fn connect(record: &SessionRecord) -> Result<UnixStream> {
    let stream = connect_with_timeout(&record.socket_path, CONTROL_RPC_TIMEOUT)
        .with_context(|| format!("connect {}", record.socket_path.display()))?;
    set_control_deadlines(&stream)?;
    Ok(stream)
}
pub(crate) fn rpc_simple(
    record: &SessionRecord,
    operation: Operation,
    data: Option<&[u8]>,
) -> Result<Value> {
    let mut stream = connect(record)?;
    let request = Request::new(record.id, operation);
    let id = request.request_id.clone();
    write_json(&mut stream, &request)?;
    if let Some(bytes) = data {
        write_frame(&mut stream, FrameKind::Data, bytes)?;
    }
    let frame = read_frame(&mut stream)?.ok_or_else(|| anyhow!("worker closed connection"))?;
    let response: Response = frame_json(frame)?;
    if response.request_id != id {
        bail!("response request id mismatch");
    }
    response.into_result()
}
pub(crate) fn rpc_send(record: &SessionRecord, data: &[u8]) -> Result<()> {
    rpc_simple(record, Operation::Send { bytes: data.len() }, Some(data))?;
    Ok(())
}
pub(crate) fn rpc_capture(record: &SessionRecord, max: Option<usize>) -> Result<Vec<u8>> {
    let mut stream = connect(record)?;
    let request = Request::new(record.id, Operation::Capture { max_bytes: max });
    let id = request.request_id.clone();
    write_json(&mut stream, &request)?;
    let response: Response =
        frame_json(read_frame(&mut stream)?.ok_or_else(|| anyhow!("missing response"))?)?;
    if response.request_id != id {
        bail!("response request id mismatch");
    }
    response.into_result()?;
    let frame = read_frame(&mut stream)?.ok_or_else(|| anyhow!("missing capture data"))?;
    if frame.kind != FrameKind::Data {
        bail!("expected capture data");
    }
    Ok(frame.payload)
}
/// `a capture --screen [--plain]` (docs/terminal-state-design.md section 8):
/// mirrors `rpc_capture`'s shape exactly, against `Operation::CaptureScreen`.
pub(crate) fn rpc_capture_screen(record: &SessionRecord, plain: bool) -> Result<Vec<u8>> {
    let mut stream = connect(record)?;
    let request = Request::new(record.id, Operation::CaptureScreen { plain });
    let id = request.request_id.clone();
    write_json(&mut stream, &request)?;
    let response: Response =
        frame_json(read_frame(&mut stream)?.ok_or_else(|| anyhow!("missing response"))?)?;
    if response.request_id != id {
        bail!("response request id mismatch");
    }
    response.into_result()?;
    let frame = read_frame(&mut stream)?.ok_or_else(|| anyhow!("missing capture data"))?;
    if frame.kind != FrameKind::Data {
        bail!("expected capture data");
    }
    Ok(frame.payload)
}
