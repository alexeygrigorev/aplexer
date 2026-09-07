//! The wire protocol: length-prefixed frame codec (JSON/data/end), and the
//! request/response/server-event/attach-control message types shared by the
//! CLI, the worker, and external clients.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use uuid::Uuid;

use crate::ExitInfo;

pub const PROTOCOL_VERSION: u16 = 1;

pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Json = 1,
    Data = 2,
    End = 3,
}
#[derive(Debug)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

pub fn write_frame<W: Write>(writer: &mut W, kind: FrameKind, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        bail!("frame too large: {}", payload.len());
    }
    let mut header = [0u8; 12];
    header[..4].copy_from_slice(b"APX1");
    header[4] = kind as u8;
    header[8..12].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Frame>> {
    let mut header = [0u8; 12];
    match reader.read(&mut header[..1]) {
        Ok(0) => return Ok(None),
        Ok(1) => {}
        Ok(_) => unreachable!(),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return read_frame(reader),
        Err(e) => return Err(e.into()),
    }
    reader.read_exact(&mut header[1..])?;
    if &header[..4] != b"APX1" {
        bail!("invalid protocol magic");
    }
    if header[5..8] != [0, 0, 0] {
        bail!("unsupported frame flags");
    }
    let kind = match header[4] {
        1 => FrameKind::Json,
        2 => FrameKind::Data,
        3 => FrameKind::End,
        n => bail!("unknown frame type {n}"),
    };
    let length = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
    if length > MAX_FRAME_BYTES {
        bail!("frame exceeds maximum");
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    Ok(Some(Frame { kind, payload }))
}

pub fn write_json<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    write_frame(writer, FrameKind::Json, &serde_json::to_vec(value)?)
}
pub fn frame_json<T: for<'de> Deserialize<'de>>(frame: Frame) -> Result<T> {
    if frame.kind != FrameKind::Json {
        bail!("expected JSON frame");
    }
    Ok(serde_json::from_slice(&frame.payload)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    pub request_id: String,
    /// Additive protocol binding for daemonless workers that can outlive a
    /// client upgrade. New clients always send it; older workers ignore the
    /// unknown field and remain controllable. New workers reject `None` with
    /// an explicit upgrade error, so outdated clients cannot issue unbound
    /// operations to a newly-created session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(flatten)]
    pub operation: Operation,
}
impl Request {
    pub fn new(session_id: Uuid, operation: Operation) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4().to_string(),
            session_id: Some(session_id),
            operation,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Ping,
    Status,
    Send {
        bytes: usize,
    },
    Capture {
        max_bytes: Option<usize>,
    },
    /// `history_bytes` keeps its original meaning (raw-tail replay size,
    /// used when `want_screen` is false or an old worker doesn't understand
    /// it). `want_screen`/`rows`/`cols` are additive fields (design doc
    /// section 6.1): an old worker's serde simply ignores unknown fields
    /// and falls back to today's raw-tail replay -- no worse than before --
    /// and an old client never sends them, so `want_screen` defaulting to
    /// `false` reproduces today's behavior exactly. `rows`/`cols`, when
    /// given, are the client's real terminal geometry (already
    /// reserved-rows-adjusted by the caller) so the worker can resize the
    /// PTY and the screen model *before* rendering the snapshot -- no
    /// wrong-size frame followed by a SIGWINCH repaint.
    Attach {
        history_bytes: Option<usize>,
        #[serde(default)]
        want_screen: bool,
        #[serde(default)]
        rows: Option<u16>,
        #[serde(default)]
        cols: Option<u16>,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Kill {
        signal: i32,
        grace_ms: u64,
    },
    Rename {
        workspace: PathBuf,
        tag: String,
    },
    /// `a capture --screen` (design doc section 8): the rendered current
    /// screen (`plain: false`, same bytes `Attach`'s snapshot would carry)
    /// or its plain-text contents (`plain: true`, `ScreenTracker::contents`)
    /// -- "richer PocketShell previews" from spec.md section 17.
    CaptureScreen {
        plain: bool,
    },
    /// `a state-report <state>` (docs/pocketshell-integration-plan.md Open
    /// question #2, "Agent-state ingestion"): a hook running inside the
    /// session pushes its own semantic state, the missing half of `a watch
    /// --jsonl`'s `agent.state` PTY-recency heuristic. `state` must be one
    /// of `REPORTED_AGENT_STATES`; the worker validates and rejects
    /// anything else (`WorkerRuntime::report_state`) rather than writing an
    /// unrecognised value the watch merge logic would then have to guess
    /// at.
    ReportState {
        state: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub version: u16,
    pub request_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
impl Response {
    pub fn ok(id: impl Into<String>, value: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: id.into(),
            ok: true,
            result: Some(value),
            error: None,
        }
    }
    pub fn error(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: id.into(),
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
    pub fn into_result(self) -> Result<Value> {
        if self.ok {
            Ok(self.result.unwrap_or(Value::Null))
        } else {
            bail!("{}", self.error.unwrap_or_else(|| "request failed".into()))
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    Exit {
        exit: ExitInfo,
    },
    Error {
        message: String,
    },
    /// The workload did something that invalidates the client's DECSTBM
    /// status-bar reservation: reset its scroll margins (RIS or a bare/
    /// full-range `\x1b[r`), flipped alternate-screen state, or issued an
    /// Erase in Display (`CSI ... J`, which ignores scroll margins per spec
    /// and so can wipe the reserved row even under an active sub-range --
    /// design doc section 7). Sent **only** to subscribers that attached
    /// with `want_screen: true` -- an old client's `serde_json::from_slice`
    /// would hard-fail on an unrecognized `event` tag, so gating this on
    /// the request flag (done at the worker's send site, not here) keeps
    /// old clients safe (design doc section 6.3).
    Layout {
        alt_screen: bool,
        margins_reset: bool,
        // `default` so a new client attaching to an OLD, already-running
        // worker (started before this field existed) doesn't hard-fail
        // deserializing that worker's `Layout` events -- workers are
        // long-lived and outlive a client rebuild, unlike the `event` tag
        // gating described above which only covers new tags, not new fields
        // on an existing one.
        #[serde(default)]
        erase_reset: bool,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AttachControl {
    Resize { rows: u16, cols: u16 },
    Signal { signal: i32 },
    Detach,
}
