//! The streaming attach: establishing the subscription and geometry, the
//! output writer thread, and the input/control reader loop.

use super::*;

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
    // forever. An established attach must not: its client routinely stops
    // reading for a while (a background tab, a slow link, a slept laptop),
    // and coalescing (MAX_SUBSCRIBER_QUEUED_BYTES) exists precisely to make
    // that survivable. But coalescing only bounds the hub queue -- once the
    // socket buffer itself (~200 KB) fills behind a paused client, a residual
    // SO_SNDTIMEO fails the streaming writer's write() after
    // CLIENT_IO_TIMEOUT of zero drain, the worker closes the socket, and the
    // client is silently disconnected the moment its terminal wakes
    // ("Connection to ... lost") -- observed on busy codex sessions, whose
    // continuous TUI repaints fill the buffer fastest. Silence in both
    // directions is a normal state of a long-lived attach; the read deadline
    // below is already cleared for the same reason. A dead client is still
    // detected without it: writes fail with EPIPE/ECONNRESET once the peer's
    // socket closes, and the reader loop sees EOF.
    lock(&writer)?
        .set_write_timeout(None)
        .context("clear attach streaming write deadline")?;
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
    let _ = reader.shutdown(std::net::Shutdown::Both);
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
