//! One client connection: the request handshake, the session binding
//! check, and the dispatch of every non-streaming operation.

use super::*;
use serde_json::Value;

/// The peer is the request's first credential: control connections get IO
/// deadlines, and a uid that is not ours never reaches an operation.
fn authorize_peer(stream: &UnixStream) -> Result<()> {
    stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
    let uid = peer_uid(stream.as_raw_fd())?;
    if uid != unsafe { libc::geteuid() } {
        bail!("peer uid {uid} is not authorized");
    }
    Ok(())
}

/// The protocol prelude every operation runs behind: version match and
/// session binding. Returns the error message to send (and stop with) when
/// the prelude fails, None when the request may proceed.
fn handshake_rejection(request: &Request, worker_session_id: Uuid) -> Option<String> {
    if request.version != PROTOCOL_VERSION {
        return Some("unsupported protocol version".into());
    }
    match request.session_id {
        Some(expected) if expected == worker_session_id => None,
        Some(expected) => Some(format!(
            "request targets session {expected}, but this worker owns {worker_session_id}"
        )),
        None => Some("request omitted session_id; upgrade the aplexer client".into()),
    }
}

pub(super) fn handle_connection(mut stream: UnixStream, runtime: Arc<WorkerRuntime>) -> Result<()> {
    authorize_peer(&stream)?;
    let frame = read_frame(&mut stream)?.ok_or_else(|| anyhow!("empty request"))?;
    let request: Request = frame_json(frame)?;
    if let Some(message) = handshake_rejection(&request, runtime.id) {
        return write_json(&mut stream, &Response::error(request.request_id, message));
    }
    let id = request.request_id.clone();
    dispatch_operation(stream, &runtime, request, id)
}

/// One JSON response folding an operation's `Result`: ok carries the
/// payload, error carries the formatted anyhow chain.
fn write_result(stream: &mut UnixStream, id: &str, result: Result<Value>) -> Result<()> {
    match result {
        Ok(value) => write_json(stream, &Response::ok(id.to_owned(), value)),
        Err(e) => write_json(stream, &Response::error(id.to_owned(), format!("{e:#}"))),
    }
}

/// Capture and CaptureScreen share the response shape exactly (design doc
/// section 8): a count frame, then one Data frame of the payload.
fn respond_with_data(stream: &mut UnixStream, id: &str, data: Vec<u8>) -> Result<()> {
    write_json(
        stream,
        &Response::ok(id.to_owned(), json!({"bytes": data.len()})),
    )?;
    write_frame(stream, FrameKind::Data, &data)
}

/// Read the data frame a Send request promised and hand it to the workload.
fn handle_send(
    stream: &mut UnixStream,
    runtime: &Arc<WorkerRuntime>,
    id: &str,
    bytes: usize,
) -> Result<()> {
    let next = read_frame(stream)?.ok_or_else(|| anyhow!("missing data frame"))?;
    if next.kind != FrameKind::Data || next.payload.len() != bytes {
        return write_json(
            stream,
            &Response::error(id.to_owned(), "data length mismatch"),
        );
    }
    match runtime.send(&next.payload) {
        Ok(()) => write_json(
            stream,
            &Response::ok(id.to_owned(), json!({"bytes": bytes})),
        ),
        Err(e) => write_json(stream, &Response::error(id.to_owned(), format!("{e:#}"))),
    }
}

/// The dispatch of every non-streaming operation; Attach takes the stream
/// because it upgrades the connection into the streaming paths.
fn dispatch_operation(
    stream: UnixStream,
    runtime: &Arc<WorkerRuntime>,
    request: Request,
    id: String,
) -> Result<()> {
    let mut stream = stream;
    match request.operation {
        Operation::Ping => write_json(
            &mut stream,
            &Response::ok(id, json!({"pong":true,"id":runtime.id})),
        )?,
        Operation::Status => write_json(&mut stream, &Response::ok(id, status_value(runtime)?))?,
        Operation::Send { bytes } => handle_send(&mut stream, runtime, &id, bytes)?,
        Operation::Capture { max_bytes } => {
            respond_with_data(&mut stream, &id, runtime.output.snapshot(max_bytes)?)?
        }
        Operation::CaptureScreen { plain } => {
            // Mirrors Operation::Capture's response+Data shape exactly
            // (design doc section 8) -- just a different source for the
            // bytes: the rendered current-screen snapshot, or its
            // plain-text contents.
            let data = if plain {
                runtime.output.screen_contents()?.into_bytes()
            } else {
                runtime.output.screen_snapshot()?
            };
            respond_with_data(&mut stream, &id, data)?
        }
        Operation::Attach {
            history_bytes,
            want_screen,
            rows,
            cols,
        } => handle_attach(
            stream,
            runtime.clone(),
            id,
            history_bytes,
            want_screen,
            rows,
            cols,
        )?,
        Operation::Resize { rows, cols } => write_result(
            &mut stream,
            &id,
            runtime.resize(rows, cols).map(|_| json!({})),
        )?,
        Operation::Kill { signal, grace_ms } => match runtime.kill(signal, grace_ms) {
            // Accepted before the response is written: from here the
            // lifecycle finalization owns removing this session's
            // durable record (see run_lifecycle), so `a kill` leaves
            // nothing behind in `a list`. The record also already says
            // `phase: exiting` by this point -- `runtime.kill` persists
            // that before teardown, so a client that gets this ok and
            // immediately snapshots sees a dying session, never the
            // pre-kill phase (issue #18). A failed kill never reaches
            // that path -- the record stays as evidence for the
            // client-side recovery paths.
            Ok(()) => write_json(&mut stream, &Response::ok(id, json!({"signalled":true})))?,
            Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
        },
        Operation::Rename { workspace, tag } => match runtime.rename(workspace, tag) {
            Ok(record) => write_json(
                &mut stream,
                &Response::ok(id, serde_json::to_value(record)?),
            )?,
            Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
        },
        Operation::ReportState { state } => match runtime.report_state(state) {
            Ok(record) => write_json(
                &mut stream,
                &Response::ok(id, serde_json::to_value(record)?),
            )?,
            Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
        },
    }
    Ok(())
}

/// The Status payload: the public record plus the live-only facts a
/// persisted record cannot carry (persistence errors, cgroup stats, the
/// foreground command).
fn status_value(runtime: &WorkerRuntime) -> Result<Value> {
    // One clone (inside public_session_record), not a second one to
    // get the record out of its mutex first.
    let mut value = {
        let record = lock(&runtime.record)?;
        serde_json::to_value(public_session_record(&record))?
    };
    if let Some(error) = runtime.output.history_persistence_error() {
        value["history_persistence_error"] = json!(error);
    }
    if let Some(error) = lock(&runtime.record_persistence_error)?.clone() {
        value["record_persistence_error"] = json!(error);
    }
    if let Some(cgroup) = lock(&runtime.cgroup)?.as_ref() {
        value["cgroup"] = cgroup.stats();
    }
    // Live-only, never persisted (see foreground_command's doc
    // comment on WorkerRuntime -- deliberately not a SessionRecord
    // field): what's actually in the foreground of the pty right
    // now, which can differ from `engine`/`command` the moment the
    // workload execs or forks something new (e.g. a plain `shell`
    // session where the user manually ran another program). Merged
    // into the Status response the same way `cgroup` is above,
    // rather than added to the persisted record, so this never
    // costs a disk write and an old client's `serde_json` simply
    // ignores the unrecognized field.
    if let Some(fd) = lock(&runtime.pty_write)?.as_ref().map(|f| f.as_raw_fd()) {
        if let Some(cmd) = foreground_command(fd) {
            value["foreground_command"] = json!(cmd);
        }
    }
    Ok(value)
}
