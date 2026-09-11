//! The streaming attach: establishing the subscription and geometry, the
//! output writer thread, and the input/control reader loop.

use super::*;

/// One tick of the streaming writer's stall guard: how long a single write
/// to the attach socket may block before the guard re-checks whether the
/// peer has drained anything at all.
const ATTACH_STALL_TICK: Duration = Duration::from_secs(5);
/// How many consecutive stall ticks with **zero** socket progress a stream
/// may survive. A peer that drains even one byte inside a tick resets the
/// count, so this is not "60 seconds of slowness" -- it is 60 seconds of a
/// send queue that did not move at all, which for a TCP/unix peer means the
/// other end is gone without a FIN (a phone that left a cell tower, a NAT
/// timeout) or would need a machine that is suspended. Reaping here is what
/// keeps a long-lived worker's subscriber and connection tables from filling
/// with zombie attaches that would push every later attach over
/// `MAX_SUBSCRIBERS`/`MAX_CLIENT_CONNECTIONS`.
const ATTACH_STALL_TICKS: u32 = 12;

#[cfg(test)]
/// Tight enough that a dead-peer test finishes in well under a second,
/// loose enough that a peer draining a byte per tick is never reaped.
const TEST_STALL_TICK: Duration = Duration::from_millis(20);
#[cfg(test)]
const TEST_STALL_TICKS: u32 = 3;

/// Everything an attach needs before its handshake can be answered: the
/// validated geometry, the hub subscription (already wrapped in its
/// cleanup guard, so a later failure here releases it), and the writer
/// half of the socket. Kept as one fallible step so `handle_attach` can
/// turn any refusal into a `Response::error` the client actually sees --
/// a bare early `?` here used to close the socket before any response
/// frame, and every cause ("too many attached clients", an oversized
/// geometry, a closed PTY) reached the client as "missing attach response".
pub(super) fn establish_attach(
    runtime: &Arc<WorkerRuntime>,
    reader: &UnixStream,
    history_bytes: Option<usize>,
    want_screen: bool,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<(AttachGuard, Vec<u8>, OutputReceiver, UnixStream)> {
    let geometry = match (rows, cols) {
        (Some(rows), Some(cols)) => Some(screen::validate_size(rows, cols)?),
        _ => None,
    };
    // Geometry-first (design doc section 6.1): resize the PTY and the
    // screen model to the client's real terminal size *before* rendering
    // the snapshot below, so there is no wrong-size frame followed by a
    // SIGWINCH repaint. `WorkerRuntime::resize` itself resizes the model
    // before the ioctl (section 5.3), so this one call gets both in the
    // right order. Best-effort: a resize failure here (e.g. the PTY is
    // already closing) must not block the attach -- the pre-existing
    // client-side post-connect Resize control frame remains the fallback
    // (section 6.3 step 7).
    let payload = if want_screen {
        AttachPayload::Screen
    } else {
        AttachPayload::Tail(history_bytes)
    };
    let (client_id, subscription, initial, rx) = runtime.attach_client(payload, geometry)?;
    let guard = AttachGuard {
        runtime: Arc::clone(runtime),
        client_id,
        subscription,
    };
    let writer = reader
        .try_clone()
        .context("clone attach socket for output")?;
    Ok((guard, initial, rx, writer))
}

pub(super) fn handle_attach(
    mut reader: UnixStream,
    runtime: Arc<WorkerRuntime>,
    request_id: String,
    history_bytes: Option<usize>,
    want_screen: bool,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<()> {
    let (attach_guard, initial, rx, writer_stream) =
        match establish_attach(&runtime, &reader, history_bytes, want_screen, rows, cols) {
            Ok(established) => established,
            Err(error) => {
                // Still under the handshake's worker-wide write deadline, so
                // a peer that stopped reading cannot hold its slot with this.
                write_json(
                    &mut reader,
                    &Response::error(request_id, format!("{error:#}")),
                )?;
                return Ok(());
            }
        };
    let client_id = attach_guard.client_id;
    let subscription = attach_guard.subscription;
    // An established attach is intentionally long-lived. Before this point,
    // the handshake used the worker-wide deadline so a peer cannot reserve a
    // connection slot forever with a partial frame.
    reader.set_read_timeout(None)?;
    // Best-effort: attach is the "someone looked at this" event used by
    // `a list --sort accessed`. A persist failure must not refuse the
    // attach; the next successful attach (or a later record write that
    // races this one) will stamp it.
    //
    // Throttled to one durable write per minute per session (benchmark PLAN
    // P1.2): the old code fsync'd the record on EVERY attach, putting a
    // 5-20 ms fsync plus its variance directly on the attach handshake's
    // critical path -- the p90 tail the benchmark flagged. Recency sorting
    // only needs coarse granularity, so repeat attaches within the window
    // skip the write entirely after the first stamps it.
    {
        let now = now_ms();
        let stale = runtime
            .record()
            .map(|record| {
                record
                    .last_accessed_ms
                    .is_none_or(|at| now.saturating_sub(at) >= 60_000)
            })
            .unwrap_or(true);
        if stale {
            let _ = runtime.update_record(|record| {
                record.last_accessed_ms = Some(now);
            });
        }
    }
    let _attach_guard = attach_guard;
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
    // The handshake writes above still run under the worker-wide deadline, so
    // a peer that stops reading mid-handshake cannot hold a connection slot
    // forever. An established attach must survive a client that stops
    // reading for a while (a background tab, a slow link, a slept laptop) --
    // coalescing (MAX_SUBSCRIBER_QUEUED_BYTES) bounds the hub queue for it --
    // so the streaming write gets no per-write deadline that could fail a
    // merely slow peer. What it gets instead is the stall guard below: writes
    // run under a tick-long SO_SNDTIMEO and `pump_output` gives up on a peer
    // whose socket send queue has not drained a single byte across
    // ATTACH_STALL_TICKS consecutive ticks. Without that bound a peer that
    // vanished without a FIN (cellular drop, NAT timeout -- the normal death
    // of a phone client) blocked `pump_output` in write() forever, and every
    // such zombie held a subscriber slot, a connection slot, and a thread
    // until the worker exited; enough of them and every later attach failed
    // with "too many attached clients" or a silently closed socket. Silence
    // in both directions is still a normal state of a long-lived attach (the
    // read deadline below stays cleared); a dead client whose socket closed
    // cleanly is still detected the same way as before -- EPIPE/ECONNRESET
    // on write, EOF on read.
    lock(&writer)?
        .set_write_timeout(Some(ATTACH_STALL_TICK))
        .context("set attach streaming stall tick")?;
    thread::spawn({
        let writer = Arc::clone(&writer);
        let runtime = Arc::clone(&runtime);
        move || pump_output(writer, runtime, rx, subscription, want_screen)
    });
    pump_input(&mut reader, &runtime, &writer, client_id)?;
    let _ = reader.shutdown(std::net::Shutdown::Both);
    Ok(())
}

/// The output half of an established attach: relay every hub event to the
/// client until a terminal event or a failed write, then release the
/// subscription and close the socket so the input half sees EOF too.
fn pump_output(
    writer: Arc<Mutex<UnixStream>>,
    runtime: Arc<WorkerRuntime>,
    rx: OutputReceiver,
    subscription: u64,
    want_screen: bool,
) {
    let mut stall = StallGuard::new(ATTACH_STALL_TICKS);
    while let Ok(event) = rx.recv() {
        let outcome = (|| -> Result<PumpOutcome> {
            let mut out = lock(&writer)?;
            match event {
                OutputEvent::Data(data) => stall
                    .write(&mut out, |out| {
                        FrameTransfer::new(FrameKind::Data, &data)?.pump(out)
                    })?
                    .into_outcome(),
                OutputEvent::Layout(change) => {
                    // Old clients' serde_json::from_slice::<ServerEvent>
                    // would hard-fail on an unrecognized `event` tag --
                    // only forward this to subscribers that opted in by
                    // attaching with want_screen (design doc section
                    // 6.3); drop it otherwise.
                    if !want_screen {
                        return Ok(PumpOutcome::Continue);
                    }
                    let payload = serde_json::to_vec(&ServerEvent::Layout {
                        alt_screen: change.alt_screen,
                        margins_reset: change.margins_reset,
                        erase_reset: change.erase_reset,
                    })?;
                    stall
                        .write(&mut out, |out| {
                            FrameTransfer::new(FrameKind::Json, &payload)?.pump(out)
                        })?
                        .into_outcome()
                }
                OutputEvent::Exit(exit) => {
                    let payload = serde_json::to_vec(&ServerEvent::Exit { exit })?;
                    stall
                        .write(&mut out, |out| {
                            FrameTransfer::new(FrameKind::Json, &payload)?.pump(out)
                        })?
                        .into_outcome_terminal()
                }
                OutputEvent::Error(message) => {
                    let payload = serde_json::to_vec(&ServerEvent::Error { message })?;
                    stall
                        .write(&mut out, |out| {
                            FrameTransfer::new(FrameKind::Json, &payload)?.pump(out)
                        })?
                        .into_outcome_terminal()
                }
            }
        })();
        match outcome {
            Ok(PumpOutcome::Continue) => {}
            Ok(PumpOutcome::PeerGone) => {
                eprintln!(
                    "aplexer attach: peer made no socket progress for {}s; closing the attach",
                    u64::from(ATTACH_STALL_TICKS).saturating_mul(ATTACH_STALL_TICK.as_secs())
                );
                break;
            }
            Ok(PumpOutcome::Terminal) | Err(_) => break,
        }
    }
    runtime.output.unsubscribe(subscription);
    if let Ok(out) = writer.lock() {
        let _ = out.shutdown(std::net::Shutdown::Both);
    }
}

/// What one relayed event did to the output pump.
enum PumpOutcome {
    /// Keep relaying.
    Continue,
    /// The peer stopped draining entirely; close the attach.
    PeerGone,
    /// A terminal event was delivered; this pump is done.
    Terminal,
}

trait StallWriteOutcome {
    fn into_outcome(self) -> Result<PumpOutcome>;
    /// A terminal event delivered after a stall is still terminal, not
    /// `PeerGone`: the payload went out in full, so the pump's job is done.
    fn into_outcome_terminal(self) -> Result<PumpOutcome>;
}
impl StallWriteOutcome for bool {
    fn into_outcome(self) -> Result<PumpOutcome> {
        if self {
            Ok(PumpOutcome::Continue)
        } else {
            Ok(PumpOutcome::PeerGone)
        }
    }
    fn into_outcome_terminal(self) -> Result<PumpOutcome> {
        Ok(PumpOutcome::Terminal)
    }
}

/// One length-prefixed frame mid-flight, resumable from where the socket
/// left off. A timed-out write under `SO_SNDTIMEO` may still have written a
/// partial prefix, so the frame cannot simply be rebuilt and retried the way
/// a blocked-blocking `write_all` caller could: the retry must continue at
/// the exact byte the peer has not seen, or the stream would carry the same
/// bytes twice and every later frame would parse as garbage.
struct FrameTransfer<'a> {
    header: [u8; 12],
    payload: &'a [u8],
    /// Absolute bytes already accepted by the socket, across header + payload.
    sent: usize,
}

impl<'a> FrameTransfer<'a> {
    fn new(kind: FrameKind, payload: &'a [u8]) -> Result<Self> {
        if payload.len() > MAX_FRAME_BYTES {
            bail!("frame too large: {}", payload.len());
        }
        let mut header = [0u8; 12];
        header[..4].copy_from_slice(b"APX1");
        header[4] = kind as u8;
        header[8..12].copy_from_slice(&(payload.len() as u32).to_be_bytes());
        Ok(Self {
            header,
            payload,
            sent: 0,
        })
    }

    /// Write as much of the frame as the socket accepts right now.
    /// `Ok(true)`: the frame is out in full. `Ok(false)`: the socket's send
    /// queue filled (possibly after a partial write); call again after the
    /// guard has seen a tick.
    fn pump(&mut self, out: &mut UnixStream) -> Result<bool> {
        while self.sent < self.header.len() + self.payload.len() {
            let (buf, offset) = if self.sent < self.header.len() {
                (&self.header[..], self.sent)
            } else {
                (self.payload, self.sent - self.header.len())
            };
            match out.write(&buf[offset..]) {
                Ok(0) => {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0").into())
                }
                Ok(n) => self.sent += n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(true)
    }
}

/// Zero-progress detector for the streaming attach writer. The socket runs
/// under a tick-long `SO_SNDTIMEO`; a frame that cannot get out is retried
/// forever **unless** the peer stops draining the send queue entirely (see
/// [`ATTACH_STALL_TICKS`]). Progress is measured with `SIOCOUTQ`: if the
/// kernel's unsent-byte count for this socket dropped since the previous
/// tick, the peer consumed something -- however slowly -- and the count of
/// barren ticks resets. This is what keeps a backgrounded tab (its kernel
/// still ACKs, so the queue drains) and a phone rendering a backlog a row at
/// a time (slow drain) alive, while a peer that died mid-cell, or a client
/// wedged writing into a dead PTY, whose queue cannot move, is reaped
/// instead of blocking this thread and its slots until the worker exits.
struct StallGuard {
    ticks: u32,
    since_progress: u32,
    last_outq: Option<usize>,
}

impl StallGuard {
    fn new(ticks: u32) -> Self {
        Self {
            ticks,
            since_progress: 0,
            last_outq: None,
        }
    }

    /// Deliver one transfer under the guard. `Ok(true)`: the transfer
    /// completed. `Ok(false)`: the peer made no progress across `ticks`
    /// consecutive timed-out writes and the attach should be closed.
    /// `Err`: a real write failure (EPIPE, ECONNRESET, ...).
    fn write(
        &mut self,
        out: &mut UnixStream,
        mut body: impl FnMut(&mut UnixStream) -> Result<bool>,
    ) -> Result<bool> {
        loop {
            match body(out) {
                Ok(true) => {
                    self.since_progress = 0;
                    return Ok(true);
                }
                Ok(false) => {
                    if self.tick(out) {
                        return Ok(false);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Account one timed-out tick. Returns true when the peer has run out of
    /// credit and the attach should be reaped.
    fn tick(&mut self, out: &UnixStream) -> bool {
        let outq = send_queue_bytes(out);
        match (self.last_outq, outq) {
            (Some(previous), Some(current)) if current < previous => self.since_progress = 0,
            _ => self.since_progress += 1,
        }
        self.last_outq = outq;
        self.since_progress >= self.ticks
    }
}

/// Bytes the kernel still holds unsent for this socket (`SIOCOUTQ`, spelled
/// `TIOCOUTQ` in the libc crate on Linux). For an AF_UNIX stream socket this
/// is what the peer has not yet consumed of what we wrote; `None` when the
/// kernel will not say (the guard then falls back to counting bare
/// timed-out ticks).
fn send_queue_bytes(stream: &UnixStream) -> Option<usize> {
    const SIOCOUTQ: libc::c_ulong = libc::TIOCOUTQ;
    let mut value: libc::c_int = 0;
    let rc = unsafe { libc::ioctl(stream.as_raw_fd(), SIOCOUTQ, &mut value) };
    (rc == 0).then(|| usize::try_from(value.max(0)).unwrap_or(0))
}

/// The input half of an established attach: workload input, resize and
/// signal controls, and the explicit detach, until EOF, a closed workload,
/// or a protocol violation (reported to the client, then an error).
fn pump_input(
    reader: &mut UnixStream,
    runtime: &WorkerRuntime,
    writer: &Arc<Mutex<UnixStream>>,
    client_id: u64,
) -> Result<()> {
    loop {
        let frame = match read_frame(reader) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => break,
        };
        match frame.kind {
            FrameKind::Data => {
                if runtime.send_from_client(client_id, &frame.payload).is_err() {
                    break;
                }
            }
            FrameKind::End => break,
            FrameKind::Json => {
                let control: AttachControl = match serde_json::from_slice(&frame.payload) {
                    Ok(control) => control,
                    Err(error) => {
                        // A protocol violation ends the attach, but the
                        // client is told why before the socket closes
                        // instead of seeing a bare EOF.
                        let message = format!("malformed attach control frame: {error}");
                        if let Ok(mut out) = writer.lock() {
                            let _ = write_json(
                                &mut *out,
                                &ServerEvent::Error {
                                    message: message.clone(),
                                },
                            );
                        }
                        bail!(message);
                    }
                };
                // Control requests are best-effort and the attach outlives
                // them (the PTY may be closing, the geometry may be
                // rejected); the protocol has no non-terminal error frame,
                // so the refusal goes to worker.log rather than vanishing.
                match control {
                    AttachControl::Resize { rows, cols } => {
                        if let Err(error) = runtime.resize_client(client_id, rows, cols) {
                            eprintln!(
                                "aplexer attach: resize to {rows}x{cols} rejected: {error:#}"
                            );
                        }
                    }
                    AttachControl::Signal { signal } => {
                        if let Err(error) = runtime.signal_from_client(client_id, signal) {
                            eprintln!("aplexer attach: signal {signal} rejected: {error:#}");
                        }
                    }
                    AttachControl::Detach => break,
                }
            }
        }
    }
    Ok(())
}

/// Ensures every attach exit path (EOF, explicit detach, malformed control
/// frame, or socket error) removes both the output subscription and its
/// geometry entry. `OutputHub::unsubscribe` is idempotent, so it is safe for
/// the writer thread to race this cleanup after a failed write.
pub(super) struct AttachGuard {
    pub(super) runtime: Arc<WorkerRuntime>,
    pub(super) client_id: u64,
    pub(super) subscription: u64,
}

impl Drop for AttachGuard {
    fn drop(&mut self) {
        self.runtime.output.unsubscribe(self.subscription);
        self.runtime.detach_client(self.client_id);
    }
}

#[cfg(test)]
mod stall_guard_tests {
    use super::*;

    /// A socket whose peer never reads: fill it until one write blocks for a
    /// whole tick, so the guard's next attempts all time out.
    fn fill_until_blocked(out: &UnixStream) {
        out.set_write_timeout(Some(TEST_STALL_TICK)).unwrap();
        let chunk = [0u8; 4096];
        let mut out = out;
        loop {
            match out.write(&chunk) {
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return,
                Err(error) => panic!("fill write failed: {error}"),
            }
        }
    }

    #[test]
    fn a_peer_that_never_drains_is_reaped() {
        let (mut writer, _peer) = UnixStream::pair().unwrap();
        fill_until_blocked(&writer);
        let mut guard = StallGuard::new(TEST_STALL_TICKS);
        let payload = vec![7u8; 64];
        let delivered = guard.write(&mut writer, |out| {
            FrameTransfer::new(FrameKind::Data, &payload)?.pump(out)
        });
        // The send queue never moved a byte, so the guard must give up after
        // TEST_STALL_TICKS ticks rather than block this thread forever.
        assert!(!delivered.unwrap());
    }

    #[test]
    fn a_slowly_draining_peer_is_never_reaped() {
        let (mut writer, peer) = UnixStream::pair().unwrap();
        fill_until_blocked(&writer);
        // Drain everything available every few ms -- `TIOCOUTQ` only drops
        // when whole skbs are consumed, so this is the "slow but real" peer
        // the guard must never reap.
        let drainer = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut buffer = [0u8; 8192];
            let mut peer = &peer;
            peer.set_read_timeout(Some(Duration::from_millis(2)))
                .unwrap();
            for _ in 0..200 {
                while let Ok(n @ 1..) = peer.read(&mut buffer) {
                    if n == 0 {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let mut guard = StallGuard::new(TEST_STALL_TICKS);
        let payload = b"APX1".to_vec();
        let delivered = guard.write(&mut writer, |out| {
            FrameTransfer::new(FrameKind::Data, &payload)?.pump(out)
        });
        // A few bytes get through within a few ticks, and every tick with a
        // drained skb resets the barren count, so the guard must deliver
        // rather than reap.
        assert!(delivered.unwrap());
        drainer.join().unwrap();
    }

    #[test]
    fn a_frame_split_across_stalled_writes_is_never_duplicated() {
        // The corruption this guards against: a timed-out write may have
        // written a partial prefix, so blindly retrying the whole frame would
        // carry those bytes twice and every later frame would parse as
        // garbage. The transfer must resume exactly where the socket left
        // off, and the peer must read header+payload exactly once.
        let (mut writer, peer) = UnixStream::pair().unwrap();
        writer.set_write_timeout(Some(TEST_STALL_TICK)).unwrap();
        let payload: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
        let mut transfer = FrameTransfer::new(FrameKind::Data, &payload).unwrap();
        let expected_len = transfer.header.len() + payload.len();
        let reader = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut received = Vec::new();
            let mut buffer = [0u8; 8192];
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while received.len() < expected_len && std::time::Instant::now() < deadline {
                match (&peer).read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => received.extend_from_slice(&buffer[..n]),
                    Err(_) => break,
                }
            }
            received
        });
        let mut guard = StallGuard::new(TEST_STALL_TICKS);
        let delivered = guard.write(&mut writer, |out| transfer.pump(out)).unwrap();
        assert!(delivered);
        let received = reader.join().unwrap();
        assert_eq!(received.len(), expected_len, "byte count drifted");
        // A resumed transfer means the peer saw the frame's bytes in order,
        // exactly once: the cyclic payload is its own checksum.
        assert_eq!(&received[..8], b"APX1\x02\0\0\0");
        for (index, byte) in received[12..].iter().enumerate() {
            assert_eq!(*byte, (index % 256) as u8, "payload drifted at {index}");
        }
    }
}
