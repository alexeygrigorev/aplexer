use super::*;

/// One control round-trip: connect, send `operation` (plus an optional data
/// frame), and read the matching response. Returns the stream too, for the
/// operations whose answer continues as a data frame (`read_data_frame`)
/// or as a subscription (`establish`).
pub(crate) fn rpc_call(
    record: &SessionRecord,
    operation: Operation,
    data: Option<&[u8]>,
) -> Result<(UnixStream, Value)> {
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
    let result = response.into_result()?;
    Ok((stream, result))
}

/// The data frame a response promised, named by `what` in the error.
pub(crate) fn read_data_frame(stream: &mut UnixStream, what: &str) -> Result<Vec<u8>> {
    let frame = read_frame(stream)?.ok_or_else(|| anyhow!("missing {what}"))?;
    if frame.kind != FrameKind::Data {
        bail!("expected {what}");
    }
    Ok(frame.payload)
}

pub(crate) fn rpc_simple(
    record: &SessionRecord,
    operation: Operation,
    data: Option<&[u8]>,
) -> Result<Value> {
    rpc_call(record, operation, data).map(|(_, result)| result)
}
pub(crate) fn rpc_send(record: &SessionRecord, data: &[u8]) -> Result<()> {
    rpc_simple(record, Operation::Send { bytes: data.len() }, Some(data))?;
    Ok(())
}
pub(crate) fn rpc_capture(record: &SessionRecord, max: Option<usize>) -> Result<Vec<u8>> {
    let (mut stream, _) = rpc_call(record, Operation::Capture { max_bytes: max }, None)?;
    read_data_frame(&mut stream, "capture data")
}
/// `a capture --screen [--plain]` (docs/terminal-state-design.md section 8):
/// `rpc_capture`'s shape exactly, against `Operation::CaptureScreen`.
pub(crate) fn rpc_capture_screen(record: &SessionRecord, plain: bool) -> Result<Vec<u8>> {
    let (mut stream, _) = rpc_call(record, Operation::CaptureScreen { plain }, None)?;
    read_data_frame(&mut stream, "screen capture data")
}
