use super::*;

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
