//! One client connection: the request handshake, the session binding
//! check, and the dispatch of every non-streaming operation.

use super::*;

pub(super) fn handle_connection(mut stream: UnixStream, runtime: Arc<WorkerRuntime>) -> Result<()> {
    stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT))?;
    stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
    let uid = peer_uid(stream.as_raw_fd())?;
    if uid != unsafe { libc::geteuid() } {
        bail!("peer uid {uid} is not authorized");
    }
    let frame = read_frame(&mut stream)?.ok_or_else(|| anyhow!("empty request"))?;
    let request: Request = frame_json(frame)?;
    if request.version != PROTOCOL_VERSION {
        write_json(
            &mut stream,
            &Response::error(request.request_id, "unsupported protocol version"),
        )?;
        return Ok(());
    }
    let id = request.request_id.clone();
    let worker_session_id = runtime.id;
    match request.session_id {
        Some(expected) if expected == worker_session_id => {}
        Some(expected) => {
            write_json(
                &mut stream,
                &Response::error(
                    id,
                    format!(
                        "request targets session {expected}, but this worker owns {worker_session_id}"
                    ),
                ),
            )?;
            return Ok(());
        }
        None => {
            write_json(
                &mut stream,
                &Response::error(id, "request omitted session_id; upgrade the aplexer client"),
            )?;
            return Ok(());
        }
    }
    match request.operation {
        Operation::Ping => write_json(
            &mut stream,
            &Response::ok(id, json!({"pong":true,"id":worker_session_id})),
        )?,
        Operation::Status => {
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
            write_json(&mut stream, &Response::ok(id, value))?;
        }
        Operation::Send { bytes } => {
            let next = read_frame(&mut stream)?.ok_or_else(|| anyhow!("missing data frame"))?;
            if next.kind != FrameKind::Data || next.payload.len() != bytes {
                write_json(&mut stream, &Response::error(id, "data length mismatch"))?;
            } else {
                match runtime.send(&next.payload) {
                    Ok(()) => write_json(&mut stream, &Response::ok(id, json!({"bytes":bytes})))?,
                    Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
                }
            }
        }
        Operation::Capture { max_bytes } => {
            let data = runtime.output.snapshot(max_bytes)?;
            write_json(&mut stream, &Response::ok(id, json!({"bytes":data.len()})))?;
            write_frame(&mut stream, FrameKind::Data, &data)?;
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
            write_json(&mut stream, &Response::ok(id, json!({"bytes":data.len()})))?;
            write_frame(&mut stream, FrameKind::Data, &data)?;
        }
        Operation::Attach {
            history_bytes,
            want_screen,
            rows,
            cols,
        } => handle_attach(stream, runtime, id, history_bytes, want_screen, rows, cols)?,
        Operation::Resize { rows, cols } => match runtime.resize(rows, cols) {
            Ok(()) => write_json(&mut stream, &Response::ok(id, json!({})))?,
            Err(e) => write_json(&mut stream, &Response::error(id, format!("{e:#}")))?,
        },
        Operation::Kill { signal, grace_ms } => match runtime.kill(signal, grace_ms) {
            Ok(()) => {
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
                write_json(&mut stream, &Response::ok(id, json!({"signalled":true})))?
            }
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
