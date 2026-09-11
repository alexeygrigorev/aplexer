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
/// How many handshake attempts `establish` makes before giving up, and how
/// long it waits between them. A deadline expiry here is transient by
/// nature -- the worker was busy (a history fsync can hold the hub lock a
/// subscribe waits behind), not gone -- so the handshake is retried under a
/// wider-than-control budget instead of being reported as a disconnect. The
/// abandoned attempt cleans itself up on the worker (the client's socket
/// close drops its `AttachGuard`), so retries leak nothing.
#[cfg(not(test))]
const ATTACH_HANDSHAKE_ATTEMPTS: u32 = 3;
#[cfg(test)]
const ATTACH_HANDSHAKE_ATTEMPTS: u32 = 2;
#[cfg(not(test))]
const ATTACH_HANDSHAKE_RETRY_BACKOFF: Duration = Duration::from_millis(300);
#[cfg(test)]
const ATTACH_HANDSHAKE_RETRY_BACKOFF: Duration = Duration::from_millis(10);

pub(crate) fn establish(
    record: &SessionRecord,
    replay_bytes: Option<usize>,
    want_screen: bool,
    geometry: Option<(u16, u16)>,
) -> Result<AttachHandshake> {
    let mut attempt = 1;
    loop {
        match establish_once(record, replay_bytes, want_screen, geometry) {
            Ok(handshake) => return Ok(handshake),
            Err(error) if attempt < ATTACH_HANDSHAKE_ATTEMPTS && is_deadline_expiry(&error) => {
                attempt += 1;
                thread::sleep(ATTACH_HANDSHAKE_RETRY_BACKOFF);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether `error` is a socket deadline expiry rather than a real answer:
/// an `SO_RCVTIMEO`/`SO_SNDTIMEO` hit surfaces as `WouldBlock`, the connect
/// and poll budgets as `TimedOut`. Everything else -- connection refused, a
/// reset, a worker's explicit error response -- is genuine and must reach
/// the user instead of being retried into it.
pub(crate) fn is_deadline_expiry(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|io_error| {
            matches!(
                io_error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            )
        })
    })
}

fn establish_once(
    record: &SessionRecord,
    replay_bytes: Option<usize>,
    want_screen: bool,
    geometry: Option<(u16, u16)>,
) -> Result<AttachHandshake> {
    let (rows, cols) = match geometry {
        Some((rows, cols)) => (Some(rows), Some(cols)),
        None => (None, None),
    };
    let (mut reader, result) = rpc_call_within(
        record,
        Operation::Attach {
            history_bytes: replay_bytes,
            want_screen,
            rows,
            cols,
        },
        None,
        ATTACH_HANDSHAKE_TIMEOUT,
    )?;
    let screen = result.get("screen").and_then(|v| v.as_bool());
    let initial = read_data_frame(&mut reader, "history data")?;
    // Only the handshake is an RPC. Once subscribed, silence is a normal
    // state for an interactive terminal and must not detach the client.
    clear_streaming_deadlines(&reader)?;
    Ok(AttachHandshake {
        reader,
        initial,
        screen,
    })
}
