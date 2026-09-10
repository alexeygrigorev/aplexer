use super::*;

/// Result of `establish()`: the connected/subscribed stream, its initial
/// payload (either a live-screen snapshot or a raw-tail replay -- see
/// `screen`), and enough of the response to know which one it got.
pub(crate) struct AttachHandshake {
    pub(crate) reader: UnixStream,
    pub(crate) initial: Vec<u8>,
    /// The response's `"screen"` field: `Some(true)`/`Some(false)` from a
    /// worker new enough to report it, `None` from an old worker whose
    /// response predates the field entirely (docs/terminal-state-design.md
    /// section 6.1's compatibility matrix) -- used to decide whether the
    /// explicit post-connect Resize control send is still needed (section
    /// 6.3 step 7).
    pub(crate) screen: Option<bool>,
}

/// Extracted attach handshake (connect + `Operation::Attach` request +
/// response check + initial payload frame), used by both the initial
/// attach and every in-process switch (docs/fast-session-switching-design.md
/// section 3.1).
///
/// `want_screen` requests the live-screen snapshot (docs/terminal-state-design.md
/// section 6.1); `geometry`, when known (a real tty), is `(rows, cols)`
/// already reserved-rows-adjusted by the caller -- sent so the worker can
/// resize the PTY and its screen model *before* rendering the snapshot, so
/// there is no wrong-size frame followed by a SIGWINCH repaint (section
/// 6.3 step 1). An old worker's serde simply ignores these unknown request
/// fields and falls back to today's raw-tail replay -- no worse than
/// before.
pub(crate) fn establish(
    record: &SessionRecord,
    replay_bytes: Option<usize>,
    want_screen: bool,
    geometry: Option<(u16, u16)>,
) -> Result<AttachHandshake> {
    let mut reader = connect(record)?;
    let (rows, cols) = match geometry {
        Some((rows, cols)) => (Some(rows), Some(cols)),
        None => (None, None),
    };
    let request = Request::new(
        record.id,
        Operation::Attach {
            history_bytes: replay_bytes,
            want_screen,
            rows,
            cols,
        },
    );
    let id = request.request_id.clone();
    write_json(&mut reader, &request)?;
    let response: Response =
        frame_json(read_frame(&mut reader)?.ok_or_else(|| anyhow!("missing attach response"))?)?;
    if response.request_id != id {
        bail!("response request id mismatch");
    }
    let result = response.into_result()?;
    let screen = result.get("screen").and_then(|v| v.as_bool());
    let initial = read_frame(&mut reader)?.ok_or_else(|| anyhow!("missing history frame"))?;
    if initial.kind != FrameKind::Data {
        bail!("expected history data");
    }
    // Only the handshake is an RPC. Once subscribed, silence is a normal
    // state for an interactive terminal and must not detach the client.
    clear_streaming_deadlines(&reader)?;
    Ok(AttachHandshake {
        reader,
        initial: initial.payload,
        screen,
    })
}
