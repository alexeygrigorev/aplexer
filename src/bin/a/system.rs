fn send_data(writer: &Arc<Mutex<UnixStream>>, data: &[u8]) -> Result<()> {
    let mut stream = writer.lock().map_err(|_| anyhow!("socket lock poisoned"))?;
    write_frame(&mut *stream, FrameKind::Data, data)
}
fn send_control(writer: &Arc<Mutex<UnixStream>>, control: &AttachControl) -> Result<()> {
    let mut stream = writer.lock().map_err(|_| anyhow!("socket lock poisoned"))?;
    write_json(&mut *stream, control)
}

const ATTACH_CLEANUP_SIGNALS: [i32; 4] = [libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT, libc::SIGINT];
static ATTACH_SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Async-signal-safe half of attach cleanup. The handler deliberately does
/// nothing except write one byte to a nonblocking self-pipe. Terminal I/O,
/// socket locking, termios restoration, and allocation all remain on normal
/// Rust threads.
extern "C" fn attach_cleanup_signal(signal: i32) {
    let fd = ATTACH_SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = signal as u8;
        unsafe {
            libc::write(fd, std::ptr::from_ref(&byte).cast(), 1);
        }
    }
}

struct AttachSignalBridge {
    read_fd: i32,
    write_fd: i32,
    previous: Vec<(i32, libc::sigaction)>,
    watcher: Option<thread::JoinHandle<()>>,
    caught: Arc<AtomicI32>,
}

impl AttachSignalBridge {
    fn install(writer: Arc<Mutex<UnixStream>>, active: Arc<AtomicBool>) -> Result<Self> {
        let mut fds = [-1; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error()).context("create attach signal pipe");
        }
        let flags = unsafe { libc::fcntl(fds[1], libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
        {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(error).context("make attach signal pipe nonblocking");
        }

        ATTACH_SIGNAL_WRITE_FD.store(fds[1], Ordering::Release);
        let mut previous = Vec::with_capacity(ATTACH_CLEANUP_SIGNALS.len());
        for signal in ATTACH_CLEANUP_SIGNALS {
            let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
            action.sa_sigaction = attach_cleanup_signal as *const () as usize;
            unsafe { libc::sigemptyset(&mut action.sa_mask) };
            let mut old = unsafe { std::mem::zeroed::<libc::sigaction>() };
            if unsafe { libc::sigaction(signal, &action, &mut old) } != 0 {
                let error = io::Error::last_os_error();
                for (installed, prior) in previous.iter().rev() {
                    unsafe {
                        libc::sigaction(*installed, prior, std::ptr::null_mut());
                    }
                }
                ATTACH_SIGNAL_WRITE_FD.store(-1, Ordering::Release);
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(error).with_context(|| format!("install attach signal {signal}"));
            }
            previous.push((signal, old));
        }

        let caught = Arc::new(AtomicI32::new(0));
        let watcher_caught = caught.clone();
        let read_fd = fds[0];
        let watcher = thread::spawn(move || {
            let mut byte = 0u8;
            loop {
                let read = unsafe { libc::read(read_fd, std::ptr::from_mut(&mut byte).cast(), 1) };
                if read == 1 {
                    if byte == 0 {
                        return;
                    }
                    watcher_caught
                        .compare_exchange(0, i32::from(byte), Ordering::SeqCst, Ordering::Relaxed)
                        .ok();
                    active.store(false, Ordering::Relaxed);
                    if let Ok(stream) = writer.lock() {
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                    }
                    return;
                }
                if read < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return;
            }
        });
        Ok(Self {
            read_fd: fds[0],
            write_fd: fds[1],
            previous,
            watcher: Some(watcher),
            caught,
        })
    }

    fn finish(mut self) -> Option<i32> {
        self.stop_and_restore();
        match self.caught.load(Ordering::SeqCst) {
            0 => None,
            signal => Some(signal),
        }
    }

    fn stop_and_restore(&mut self) {
        if self.write_fd < 0 {
            return;
        }
        let stop = 0u8;
        unsafe {
            libc::write(self.write_fd, std::ptr::from_ref(&stop).cast(), 1);
        }
        if let Some(watcher) = self.watcher.take() {
            let _ = watcher.join();
        }
        ATTACH_SIGNAL_WRITE_FD.store(-1, Ordering::Release);
        for (signal, prior) in self.previous.iter().rev() {
            unsafe {
                libc::sigaction(*signal, prior, std::ptr::null_mut());
            }
        }
        unsafe {
            libc::close(self.read_fd);
            libc::close(self.write_fd);
        }
        self.read_fd = -1;
        self.write_fd = -1;
    }
}

impl Drop for AttachSignalBridge {
    fn drop(&mut self) {
        self.stop_and_restore();
    }
}

struct RawMode {
    fd: i32,
    old: libc::termios,
}
impl RawMode {
    fn enter(fd: i32) -> Result<Self> {
        let mut old = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, old.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error()).context("tcgetattr");
        }
        let old = unsafe { old.assume_init() };
        let mut raw = unsafe { std::ptr::read(&old) };
        unsafe {
            libc::cfmakeraw(&mut raw);
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } < 0 {
            return Err(io::Error::last_os_error()).context("tcsetattr");
        }
        Ok(Self { fd, old })
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.old);
        }
    }
}
fn terminal_size(fd: i32) -> Option<(u16, u16)> {
    let mut ws = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, ws.as_mut_ptr()) } < 0 {
        return None;
    }
    let ws = unsafe { ws.assume_init() };
    // A newly-created or deliberately-unsized PTY reports 0x0. Treat it as
    // "geometry unknown", using the same conventional fallback as the
    // worker's screen model; 1x1 is a degenerate vt100 grid and is not a
    // useful representation of any interactive terminal.
    Some((
        if ws.ws_row == 0 {
            aplexer::screen::DEFAULT_TERMINAL_ROWS
        } else {
            ws.ws_row
        },
        if ws.ws_col == 0 {
            aplexer::screen::DEFAULT_TERMINAL_COLS
        } else {
            ws.ws_col
        },
    ))
}

fn parse_signal(raw: &str) -> Result<i32> {
    let upper = raw.trim().trim_start_matches("SIG").to_ascii_uppercase();
    let value = match upper.as_str() {
        "TERM" => libc::SIGTERM,
        "KILL" => libc::SIGKILL,
        "INT" => libc::SIGINT,
        "HUP" => libc::SIGHUP,
        "QUIT" => libc::SIGQUIT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        _ => upper.parse::<i32>().context("unknown signal")?,
    };
    if !(1..=64).contains(&value) {
        bail!("signal out of range");
    }
    Ok(value)
}
fn parse_hex(input: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(input)?
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>();
    if !text.is_ascii() {
        bail!("hex input must contain only ASCII hexadecimal digits");
    }
    if text.len() % 2 != 0 {
        bail!("hex input must contain an even number of digits");
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(Into::into))
        .collect()
}
