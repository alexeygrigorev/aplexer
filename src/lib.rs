#![cfg(target_os = "linux")]

pub mod agent_events;
pub mod agent_kind;
pub mod api;
pub mod hooks;
pub mod messaging;
pub mod placement;
pub mod screen;
pub mod watch;
pub mod worker;

mod config;
pub use config::*;

mod util;
pub use util::*;

mod process;
pub use process::*;

mod record;
pub use record::*;

mod registry;
pub use registry::{read_record, read_session_record, list_records, resolve_record};

mod paths;
pub use paths::{Paths, ensure_private_dir, canonical_workspace};

mod persist;
pub use persist::{atomic_write_json, atomic_write_bytes, FileLock};

#[cfg(feature = "python")]
mod python;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, VecDeque};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;
pub const DEFAULT_HISTORY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Global per-session raw-history ceiling. The ring is resident in every
/// worker and periodically copied for atomic persistence, while protocol and
/// post-mortem capture can expose at most one frame, so retaining more than a
/// maximum-sized frame adds memory/write amplification without a usable read
/// path.
pub const MAX_HISTORY_BYTES: usize = MAX_FRAME_BYTES;
const MAX_CGROUP_RECOVERY_MEMBERS: usize = 4096;
const MAX_CGROUP_PROCS_BYTES: u64 = 128 * 1024;
const CGROUP_RECOVERY_FD_RESERVE: u64 = 16;
pub fn validate_history_bytes(value: usize) -> Result<usize> {
    if value > MAX_HISTORY_BYTES {
        bail!("history_bytes {value} exceeds the maximum of {MAX_HISTORY_BYTES} bytes (16 MiB)");
    }
    Ok(value)
}

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

/// How stale the persisted history file may get behind the in-memory ring.
/// Live reads (capture/attach) are always served from memory; the file only
/// matters after the worker is gone, so a worker crash loses at most this
/// much of the tail.
pub const HISTORY_FLUSH_INTERVAL: Duration = Duration::from_millis(500);

const HISTORY_FORMAT_VERSION: u32 = 2;
const HISTORY_BANK_MAGIC: &[u8; 8] = b"APLXH2D\0";
const HISTORY_BANK_HEADER_PREFIX_BYTES: usize = 72;
const HISTORY_BANK_HEADER_BYTES: usize = HISTORY_BANK_HEADER_PREFIX_BYTES + 32;
const HISTORY_COMMIT_MAX_BYTES: usize = 4096;
const HISTORY_MARKER_MAX_BYTES: usize = 4096;
const HISTORY_BANK_COUNT: u8 = 2;
const HISTORY_COMMIT_COUNT: u8 = 2;

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn history_sidecar_path(path: &Path, kind: &str, slot: u8) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("history.bin"))
        .to_os_string();
    name.push(format!(".v2.{kind}.{slot}"));
    path.with_file_name(name)
}

fn history_data_path(path: &Path, slot: u8) -> PathBuf {
    history_sidecar_path(path, "data", slot)
}

fn history_commit_path(path: &Path, slot: u8) -> PathBuf {
    history_sidecar_path(path, "commit", slot)
}

fn history_marker_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("history.bin"))
        .to_os_string();
    name.push(".v2.marker");
    path.with_file_name(name)
}

fn history_session_id(path: &Path) -> Uuid {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse().ok())
        .unwrap_or_else(Uuid::nil)
}

fn validate_optional_history_node(path: &Path, label: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => bail!("{label} {} is not a regular file", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {label} {}", path.display())),
    }
}

fn open_optional_history_file(path: &Path, label: &str, write: bool) -> Result<Option<File>> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("open {label} {}", path.display()))
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("{label} {} is not a trusted regular file", path.display());
    }
    Ok(Some(file))
}

fn validate_history_artifacts(path: &Path) -> Result<()> {
    validate_optional_history_node(path, "legacy history")?;
    validate_optional_history_node(&history_marker_path(path), "history marker")?;
    for slot in 0..HISTORY_BANK_COUNT {
        let data_path = history_data_path(path, slot);
        if validate_optional_history_node(&data_path, "history data bank")? {
            let length = fs::symlink_metadata(&data_path)?.len();
            let hard_cap = HISTORY_BANK_HEADER_BYTES as u64 + 2 * MAX_HISTORY_BYTES as u64;
            if length > hard_cap {
                bail!(
                    "history data bank {} exceeds the {}-byte hard cap",
                    data_path.display(),
                    hard_cap
                );
            }
        }
    }
    for slot in 0..HISTORY_COMMIT_COUNT {
        validate_optional_history_node(&history_commit_path(path, slot), "history commit")?;
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HistoryMarker {
    format_version: u32,
    store_id: Uuid,
    session_id: Uuid,
    metadata_sha256: String,
}

impl HistoryMarker {
    fn seal(mut self) -> Result<Self> {
        self.metadata_sha256.clear();
        self.metadata_sha256 = sha256_hex(&serde_json::to_vec(&self)?);
        Ok(self)
    }

    fn validate(&self, path: &Path) -> Result<()> {
        if self.format_version != HISTORY_FORMAT_VERSION {
            bail!("unsupported history marker format {}", self.format_version);
        }
        if self.store_id.is_nil() {
            bail!("history marker has a nil store id");
        }
        if self.session_id != history_session_id(path) {
            bail!("history marker belongs to a different session");
        }
        let mut unsigned = self.clone();
        let supplied = std::mem::take(&mut unsigned.metadata_sha256);
        let expected = sha256_hex(&serde_json::to_vec(&unsigned)?);
        if supplied != expected {
            bail!("history marker metadata checksum mismatch");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct HistoryCommit {
    format_version: u32,
    store_id: Uuid,
    session_id: Uuid,
    commit_generation: u64,
    bank_generation: u64,
    data_slot: u8,
    capacity: u64,
    committed_len: u64,
    stream_end: u64,
    data_sha256: String,
    metadata_sha256: String,
}

impl HistoryCommit {
    fn seal(mut self) -> Result<Self> {
        self.metadata_sha256.clear();
        self.metadata_sha256 = sha256_hex(&serde_json::to_vec(&self)?);
        Ok(self)
    }

    fn validate(&self, path: &Path, metadata_slot: u8) -> Result<()> {
        if self.format_version != HISTORY_FORMAT_VERSION {
            bail!("unsupported history format {}", self.format_version);
        }
        if self.data_slot >= HISTORY_BANK_COUNT {
            bail!("history commit names invalid data slot {}", self.data_slot);
        }
        if self.commit_generation % HISTORY_COMMIT_COUNT as u64 != metadata_slot as u64 {
            bail!("history commit is stored in the wrong metadata slot");
        }
        if self.session_id != history_session_id(path) {
            bail!("history commit belongs to a different session");
        }
        if self.commit_generation == 0 || self.bank_generation == 0 {
            bail!("history generation counters must be positive");
        }
        let capacity = usize::try_from(self.capacity).context("history capacity does not fit")?;
        validate_history_bytes(capacity)?;
        let max_payload = self
            .capacity
            .checked_mul(2)
            .ok_or_else(|| anyhow!("history bank size overflow"))?;
        if self.committed_len > max_payload {
            bail!("history committed length exceeds its bounded bank size");
        }
        if self.committed_len > self.stream_end {
            bail!("history committed length exceeds its logical stream position");
        }
        if self.data_sha256.len() != 64
            || !self
                .data_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("history commit has an invalid data checksum");
        }
        let mut unsigned = self.clone();
        let supplied = std::mem::take(&mut unsigned.metadata_sha256);
        let expected = sha256_hex(&serde_json::to_vec(&unsigned)?);
        if supplied != expected {
            bail!("history commit metadata checksum mismatch");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct HistoryBankHeader {
    store_id: Uuid,
    session_id: Uuid,
    bank_generation: u64,
    data_slot: u8,
    capacity: u64,
}

impl HistoryBankHeader {
    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HISTORY_BANK_HEADER_BYTES);
        bytes.extend_from_slice(HISTORY_BANK_MAGIC);
        bytes.extend_from_slice(&HISTORY_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(HISTORY_BANK_HEADER_BYTES as u32).to_le_bytes());
        bytes.extend_from_slice(self.store_id.as_bytes());
        bytes.extend_from_slice(self.session_id.as_bytes());
        bytes.extend_from_slice(&self.bank_generation.to_le_bytes());
        bytes.push(self.data_slot);
        bytes.extend_from_slice(&[0; 7]);
        bytes.extend_from_slice(&self.capacity.to_le_bytes());
        debug_assert_eq!(bytes.len(), HISTORY_BANK_HEADER_PREFIX_BYTES);
        let checksum = Sha256::digest(&bytes);
        bytes.extend_from_slice(&checksum);
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != HISTORY_BANK_HEADER_BYTES {
            bail!("history bank header has the wrong length");
        }
        if &bytes[..8] != HISTORY_BANK_MAGIC {
            bail!("history bank magic mismatch");
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != HISTORY_FORMAT_VERSION {
            bail!("unsupported history bank format {version}");
        }
        let header_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        if header_len != HISTORY_BANK_HEADER_BYTES {
            bail!("history bank header length is invalid");
        }
        let expected = Sha256::digest(&bytes[..HISTORY_BANK_HEADER_PREFIX_BYTES]);
        if expected.as_slice() != &bytes[HISTORY_BANK_HEADER_PREFIX_BYTES..] {
            bail!("history bank header checksum mismatch");
        }
        let store_id = Uuid::from_slice(&bytes[16..32]).context("parse history store id")?;
        let session_id = Uuid::from_slice(&bytes[32..48]).context("parse history session id")?;
        let bank_generation = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
        let data_slot = bytes[56];
        if bytes[57..64].iter().any(|byte| *byte != 0) {
            bail!("history bank reserved header bytes are nonzero");
        }
        let capacity = u64::from_le_bytes(bytes[64..72].try_into().unwrap());
        Ok(Self {
            store_id,
            session_id,
            bank_generation,
            data_slot,
            capacity,
        })
    }
}

struct RecoveredHistory {
    commit: HistoryCommit,
    tail: Vec<u8>,
    data_hasher: Sha256,
}

fn read_history_commit(path: &Path, slot: u8) -> Result<Option<HistoryCommit>> {
    let commit_path = history_commit_path(path, slot);
    let Some(file) = open_optional_history_file(&commit_path, "history commit", false)? else {
        return Ok(None);
    };
    let length = file.metadata()?.len();
    if length > HISTORY_COMMIT_MAX_BYTES as u64 {
        bail!(
            "history commit {} exceeds the {}-byte cap",
            commit_path.display(),
            HISTORY_COMMIT_MAX_BYTES
        );
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(HISTORY_COMMIT_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read history commit {}", commit_path.display()))?;
    if bytes.len() > HISTORY_COMMIT_MAX_BYTES {
        bail!("history commit {} exceeds its cap", commit_path.display());
    }
    let commit: HistoryCommit = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse history commit {}", commit_path.display()))?;
    commit
        .validate(path, slot)
        .with_context(|| format!("validate history commit {}", commit_path.display()))?;
    Ok(Some(commit))
}

fn read_history_marker(path: &Path) -> Result<Option<HistoryMarker>> {
    let marker_path = history_marker_path(path);
    let Some(file) = open_optional_history_file(&marker_path, "history marker", false)? else {
        return Ok(None);
    };
    let length = file.metadata()?.len();
    if length > HISTORY_MARKER_MAX_BYTES as u64 {
        bail!(
            "history marker {} exceeds the {}-byte cap",
            marker_path.display(),
            HISTORY_MARKER_MAX_BYTES
        );
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(HISTORY_MARKER_MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read history marker {}", marker_path.display()))?;
    if bytes.len() > HISTORY_MARKER_MAX_BYTES {
        bail!("history marker {} exceeds its cap", marker_path.display());
    }
    let marker: HistoryMarker = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse history marker {}", marker_path.display()))?;
    marker
        .validate(path)
        .with_context(|| format!("validate history marker {}", marker_path.display()))?;
    Ok(Some(marker))
}

fn publish_history_marker(path: &Path, commit: &HistoryCommit) -> Result<()> {
    if let Some(marker) = read_history_marker(path)? {
        if marker.store_id != commit.store_id || marker.session_id != commit.session_id {
            bail!("history marker does not match the committed history store");
        }
        return Ok(());
    }
    let marker = HistoryMarker {
        format_version: HISTORY_FORMAT_VERSION,
        store_id: commit.store_id,
        session_id: commit.session_id,
        metadata_sha256: String::new(),
    }
    .seal()?;
    let serialized = serde_json::to_vec(&marker)?;
    if serialized.len() > HISTORY_MARKER_MAX_BYTES {
        bail!("history marker exceeds its bounded metadata size");
    }
    let marker_path = history_marker_path(path);
    atomic_write_json(&marker_path, &marker)
        .with_context(|| format!("publish history marker {}", marker_path.display()))
}

fn recover_history_candidate(
    path: &Path,
    commit: HistoryCommit,
    tail_limit: usize,
) -> Result<RecoveredHistory> {
    let data_path = history_data_path(path, commit.data_slot);
    let mut file = open_optional_history_file(&data_path, "history data bank", false)?
        .ok_or_else(|| anyhow!("history data bank is missing: {}", data_path.display()))?;
    let physical_len = file.metadata()?.len();
    let max_payload = commit
        .capacity
        .checked_mul(2)
        .ok_or_else(|| anyhow!("history bank size overflow"))?;
    let max_physical = (HISTORY_BANK_HEADER_BYTES as u64)
        .checked_add(max_payload)
        .ok_or_else(|| anyhow!("history physical size overflow"))?;
    let committed_physical = (HISTORY_BANK_HEADER_BYTES as u64)
        .checked_add(commit.committed_len)
        .ok_or_else(|| anyhow!("history committed size overflow"))?;
    if physical_len < committed_physical {
        bail!("history data bank is shorter than its committed prefix");
    }
    if physical_len > max_physical {
        bail!("history data bank exceeds its bounded physical size");
    }
    let mut header_bytes = vec![0; HISTORY_BANK_HEADER_BYTES];
    file.read_exact(&mut header_bytes)
        .context("read history bank header")?;
    let header = HistoryBankHeader::decode(&header_bytes)?;
    if header.store_id != commit.store_id
        || header.session_id != commit.session_id
        || header.bank_generation != commit.bank_generation
        || header.data_slot != commit.data_slot
        || header.capacity != commit.capacity
    {
        bail!("history data bank does not match its commit metadata");
    }
    let committed_len = usize::try_from(commit.committed_len)
        .context("history committed length does not fit memory")?;
    let mut payload = Vec::with_capacity(committed_len);
    file.take(commit.committed_len)
        .read_to_end(&mut payload)
        .context("read committed history payload")?;
    if payload.len() != committed_len {
        bail!("history data bank ended inside its committed prefix");
    }
    let mut hasher = Sha256::new();
    hasher.update(&payload);
    if format!("{:x}", hasher.clone().finalize()) != commit.data_sha256 {
        bail!("history data checksum mismatch");
    }
    let count = tail_limit.min(commit.capacity as usize).min(payload.len());
    let tail = payload[payload.len() - count..].to_vec();
    Ok(RecoveredHistory {
        commit,
        tail,
        data_hasher: hasher,
    })
}

fn recover_v2_history(path: &Path, tail_limit: usize) -> Result<(Option<RecoveredHistory>, bool)> {
    validate_history_artifacts(path)?;
    let marker = read_history_marker(path)?;
    let mut metadata_seen = false;
    let mut valid = Vec::new();
    let mut failures = Vec::new();
    for slot in 0..HISTORY_COMMIT_COUNT {
        match read_history_commit(path, slot) {
            Ok(Some(commit)) => {
                metadata_seen = true;
                if marker.as_ref().is_some_and(|marker| {
                    marker.store_id != commit.store_id || marker.session_id != commit.session_id
                }) {
                    failures.push(format!(
                        "commit slot {slot}: history commit does not match the history marker"
                    ));
                    continue;
                }
                match recover_history_candidate(path, commit, tail_limit) {
                    Ok(candidate) => valid.push(candidate),
                    Err(error) => failures.push(format!("commit slot {slot}: {error:#}")),
                }
            }
            Ok(None) => {}
            Err(error) => {
                metadata_seen = true;
                failures.push(format!("commit slot {slot}: {error:#}"));
            }
        }
    }
    if valid.is_empty() {
        if metadata_seen || marker.is_some() {
            bail!(
                "no valid committed history generation remains: {}",
                failures.join("; ")
            );
        }
        return Ok((None, false));
    }
    if marker.is_none()
        && valid
            .iter()
            .any(|candidate| candidate.commit.store_id != valid[0].commit.store_id)
    {
        bail!("valid history commits belong to different stores");
    }
    valid.sort_by_key(|candidate| candidate.commit.commit_generation);
    if valid.len() >= 2 {
        let newest = &valid[valid.len() - 1];
        let previous = &valid[valid.len() - 2];
        if newest.commit.commit_generation == previous.commit.commit_generation
            && newest.commit != previous.commit
        {
            bail!("conflicting history commits have the same generation");
        }
    }
    Ok((valid.pop(), marker.is_some()))
}

struct LegacyHistory {
    tail: Vec<u8>,
    total_len: u64,
    present: bool,
}

fn read_legacy_history_tail(path: &Path, limit: usize) -> Result<LegacyHistory> {
    let Some(mut file) = open_optional_history_file(path, "legacy history", false)? else {
        return Ok(LegacyHistory {
            tail: Vec::new(),
            total_len: 0,
            present: false,
        });
    };
    let total_len = file.metadata()?.len();
    let count = total_len.min(limit as u64);
    file.seek(SeekFrom::Start(total_len - count))
        .with_context(|| format!("seek legacy history {}", path.display()))?;
    let mut tail = Vec::with_capacity(count as usize);
    file.take(count)
        .read_to_end(&mut tail)
        .with_context(|| format!("read legacy history tail {}", path.display()))?;
    Ok(LegacyHistory {
        tail,
        total_len,
        present: true,
    })
}

/// Read a byte-exact, frame-bounded persisted tail from either the v2
/// generation store or a legacy raw `history.bin`. Once the v2 presence
/// marker is published, corruption fails closed instead of falling back to
/// potentially stale raw bytes.
pub fn read_persisted_history_tail(path: &Path, requested: Option<usize>) -> Result<Vec<u8>> {
    let limit = requested.unwrap_or(MAX_FRAME_BYTES).min(MAX_FRAME_BYTES);
    if let (Some(recovered), _) = recover_v2_history(path, limit)? {
        return Ok(recovered.tail);
    }
    Ok(read_legacy_history_tail(path, limit)?.tail)
}

/// A raw byte log -- no line/wrap-flag structure, and it must stay that way.
/// If a future feature wants to render captured history at a specific width,
/// implement it by replaying these bytes into a *fresh* `vt100::Parser`
/// constructed at that width (re-parse from scratch), never by calling
/// resize/`set_size` on a parser that already processed content at a
/// different width -- re-parsing is deterministic, in-place reflow of a
/// populated grid is exactly the class of bug that garbles tmux scrollback.
pub struct History {
    path: PathBuf,
    cap: usize,
    bytes: VecDeque<u8>,
    pending: VecDeque<u8>,
    dirty: bool,
    observed_end: u64,
    durable_end: u64,
    compatibility_end: u64,
    compatibility_len: u64,
    compatibility_known: bool,
    persisted: Option<RecoveredHistory>,
    #[cfg(test)]
    data_bytes_written: u64,
    #[cfg(test)]
    append_failure: Option<i32>,
}
impl History {
    pub fn open(path: PathBuf, cap: usize) -> Result<Self> {
        validate_history_bytes(cap)?;
        let (recovered, marker_present) = recover_v2_history(&path, cap)?;
        let had_v2 = recovered.is_some();
        let legacy = if recovered.is_none() {
            Some(read_legacy_history_tail(&path, cap)?)
        } else {
            None
        };
        let (bytes, observed_end, persisted, legacy_present, legacy_total_len) =
            if let Some(recovered) = recovered {
                let stream_end = recovered.commit.stream_end;
                (
                    recovered.tail.iter().copied().collect(),
                    stream_end,
                    Some(recovered),
                    false,
                    0,
                )
            } else {
                let legacy = legacy.expect("legacy state is loaded without v2 metadata");
                (
                    legacy.tail.iter().copied().collect(),
                    legacy.total_len,
                    None,
                    legacy.present,
                    legacy.total_len,
                )
            };
        let mut history = Self {
            path,
            cap,
            bytes,
            pending: VecDeque::new(),
            dirty: false,
            observed_end,
            durable_end: observed_end,
            compatibility_end: if legacy_present { observed_end } else { 0 },
            compatibility_len: if legacy_present { legacy_total_len } else { 0 },
            compatibility_known: !had_v2,
            persisted,
            #[cfg(test)]
            data_bytes_written: 0,
            #[cfg(test)]
            append_failure: None,
        };
        if had_v2 && !marker_present {
            let commit = &history
                .persisted
                .as_ref()
                .expect("v2 recovery has a committed generation")
                .commit;
            publish_history_marker(&history.path, commit)?;
        }
        let needs_capacity_migration = history
            .persisted
            .as_ref()
            .is_some_and(|persisted| persisted.commit.capacity != cap as u64);
        if legacy_present && legacy_total_len > cap as u64 {
            history.repair_legacy_compatibility()?;
        }
        if legacy_present || needs_capacity_migration {
            history.publish_compaction()?;
        }
        if had_v2 {
            // V2 is authoritative after a crash or partial compatibility
            // write. Rebuild the raw view once at worker restart so an old
            // client cannot observe a duplicated or stale suffix.
            history.repair_legacy_compatibility()?;
        }
        Ok(history)
    }
    #[cfg(test)]
    pub(crate) fn inject_append_failure(&mut self, errno: i32) {
        self.append_failure = Some(errno);
    }
    /// Appends only to the in-memory ring. Persisting from this hot path can
    /// both throttle PTY output and turn a disk failure into a PTY failure,
    /// so the worker owns periodic and final `flush()` attempts separately.
    pub fn append(&mut self, data: &[u8]) -> Result<()> {
        #[cfg(test)]
        if let Some(errno) = self.append_failure {
            return Err(io::Error::from_raw_os_error(errno)).context("append history");
        }
        let added = u64::try_from(data.len()).context("history append length does not fit u64")?;
        let next_end = self
            .observed_end
            .checked_add(added)
            .ok_or_else(|| anyhow!("history stream position overflow"))?;
        if self.cap > 0 {
            if data.len() >= self.cap {
                self.bytes.clear();
                self.bytes
                    .extend(data[data.len() - self.cap..].iter().copied());
                self.pending.clear();
                self.pending
                    .extend(data[data.len() - self.cap..].iter().copied());
            } else {
                self.bytes.extend(data.iter().copied());
                self.pending.extend(data.iter().copied());
                while self.bytes.len() > self.cap {
                    self.bytes.pop_front();
                }
                while self.pending.len() > self.cap {
                    self.pending.pop_front();
                }
            }
        }
        self.observed_end = next_end;
        self.dirty = true;
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        // Publish and sync the old-client raw view before committing v2. If
        // this fails, v2 remains at the prior generation and dirty state is
        // retained for retry. If v2 then fails, compatibility_end prevents a
        // retry from appending the same bytes twice.
        self.flush_legacy_compatibility()?;
        let Some(persisted) = self.persisted.as_ref() else {
            return self.publish_compaction();
        };
        let pending_span = self
            .observed_end
            .checked_sub(self.durable_end)
            .ok_or_else(|| anyhow!("history durable position exceeds observed position"))?;
        let pending_is_contiguous = self.cap == 0 || pending_span == self.pending.len() as u64;
        let resulting_len = persisted
            .commit
            .committed_len
            .checked_add(self.pending.len() as u64)
            .ok_or_else(|| anyhow!("history committed length overflow"))?;
        if !pending_is_contiguous || resulting_len > (self.cap as u64).saturating_mul(2) {
            return self.publish_compaction();
        }
        self.publish_append()
    }

    /// Finalize v2 first, then publish one raw tail snapshot for older clients
    /// that know only `history.bin`. This full-tail write happens once at a
    /// clean terminal transition, never on the 500 ms periodic path.
    pub fn flush_final(&mut self) -> Result<()> {
        self.flush()?;
        self.repair_legacy_compatibility()
    }

    fn repair_legacy_compatibility(&mut self) -> Result<()> {
        let present = validate_optional_history_node(&self.path, "legacy history")?;
        if self.cap == 0 {
            if present {
                fs::remove_file(&self.path).with_context(|| {
                    format!("remove disabled legacy history {}", self.path.display())
                })?;
                if let Some(parent) = self.path.parent() {
                    File::open(parent)?.sync_all()?;
                }
            }
            self.compatibility_end = self.observed_end;
            self.compatibility_len = 0;
            self.compatibility_known = true;
            return Ok(());
        }
        let contiguous: Vec<u8> = self.bytes.iter().copied().collect();
        let result = atomic_write_bytes(&self.path, &contiguous)
            .with_context(|| format!("publish legacy history tail {}", self.path.display()));
        match result {
            Ok(()) => {
                self.compatibility_end = self.observed_end;
                self.compatibility_len = contiguous.len() as u64;
                self.compatibility_known = true;
                Ok(())
            }
            Err(error) => {
                self.compatibility_known = false;
                Err(error)
            }
        }
    }

    fn flush_legacy_compatibility(&mut self) -> Result<()> {
        let delta = self
            .observed_end
            .checked_sub(self.compatibility_end)
            .ok_or_else(|| anyhow!("legacy history position exceeds observed history"))?;
        if delta == 0 && self.compatibility_known {
            return Ok(());
        }
        if self.cap == 0 {
            return self.repair_legacy_compatibility();
        }
        let delta_len = usize::try_from(delta).unwrap_or(usize::MAX);
        let resulting_len = self.compatibility_len.checked_add(delta);
        if !self.compatibility_known
            || delta_len > self.bytes.len()
            || resulting_len.is_none_or(|length| length > (self.cap as u64).saturating_mul(2))
            || !validate_optional_history_node(&self.path, "legacy history")?
        {
            return self.repair_legacy_compatibility();
        }
        let mut file = open_optional_history_file(&self.path, "legacy history", true)?
            .ok_or_else(|| anyhow!("legacy history disappeared: {}", self.path.display()))?;
        if file.metadata()?.len() != self.compatibility_len {
            return self.repair_legacy_compatibility();
        }
        let bytes: Vec<u8> = self
            .bytes
            .iter()
            .skip(self.bytes.len() - delta_len)
            .copied()
            .collect();
        file.seek(SeekFrom::End(0))?;
        let result = file
            .write_all(&bytes)
            .and_then(|()| file.sync_data())
            .with_context(|| format!("append legacy history {}", self.path.display()));
        match result {
            Ok(()) => {
                self.compatibility_end = self.observed_end;
                self.compatibility_len += delta;
                Ok(())
            }
            Err(error) => {
                self.compatibility_known = false;
                Err(error)
            }
        }
    }

    fn next_commit_generation(&self) -> Result<u64> {
        self.persisted
            .as_ref()
            .map(|persisted| {
                persisted
                    .commit
                    .commit_generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("history commit generation overflow"))
            })
            .unwrap_or(Ok(1))
    }

    fn publish_commit(&self, commit: HistoryCommit) -> Result<HistoryCommit> {
        let commit = commit.seal()?;
        let slot = (commit.commit_generation % HISTORY_COMMIT_COUNT as u64) as u8;
        let path = history_commit_path(&self.path, slot);
        validate_optional_history_node(&path, "history commit")?;
        let serialized = serde_json::to_vec(&commit)?;
        if serialized.len() > HISTORY_COMMIT_MAX_BYTES {
            bail!("history commit exceeds its bounded metadata size");
        }
        atomic_write_json(&path, &commit)
            .with_context(|| format!("publish history commit {}", path.display()))?;
        Ok(commit)
    }

    fn publish_append(&mut self) -> Result<()> {
        let persisted = self
            .persisted
            .as_ref()
            .ok_or_else(|| anyhow!("history append has no committed bank"))?;
        let data_path = history_data_path(&self.path, persisted.commit.data_slot);
        let mut file = open_optional_history_file(&data_path, "history data bank", true)?
            .ok_or_else(|| anyhow!("history data bank disappeared: {}", data_path.display()))?;
        let committed_physical = (HISTORY_BANK_HEADER_BYTES as u64)
            .checked_add(persisted.commit.committed_len)
            .ok_or_else(|| anyhow!("history append offset overflow"))?;
        let physical_len = file.metadata()?.len();
        if physical_len < committed_physical {
            bail!("history data bank is shorter than its committed prefix");
        }
        let max_physical = (HISTORY_BANK_HEADER_BYTES as u64)
            .checked_add((self.cap as u64).saturating_mul(2))
            .ok_or_else(|| anyhow!("history bank bound overflow"))?;
        if physical_len > max_physical {
            bail!("history data bank exceeds its bounded physical size");
        }
        if physical_len != committed_physical {
            file.set_len(committed_physical)
                .context("discard uncommitted history suffix")?;
        }
        file.seek(SeekFrom::Start(committed_physical))?;
        let pending: Vec<u8> = self.pending.iter().copied().collect();
        file.write_all(&pending).context("append history payload")?;
        file.sync_data().context("sync appended history payload")?;

        let mut hasher = persisted.data_hasher.clone();
        hasher.update(&pending);
        let committed_len = persisted
            .commit
            .committed_len
            .checked_add(pending.len() as u64)
            .ok_or_else(|| anyhow!("history committed length overflow"))?;
        let commit = HistoryCommit {
            format_version: HISTORY_FORMAT_VERSION,
            store_id: persisted.commit.store_id,
            session_id: persisted.commit.session_id,
            commit_generation: self.next_commit_generation()?,
            bank_generation: persisted.commit.bank_generation,
            data_slot: persisted.commit.data_slot,
            capacity: self.cap as u64,
            committed_len,
            stream_end: self.observed_end,
            data_sha256: format!("{:x}", hasher.clone().finalize()),
            metadata_sha256: String::new(),
        };
        let commit = self.publish_commit(commit)?;
        publish_history_marker(&self.path, &commit)?;
        #[cfg(test)]
        {
            self.data_bytes_written = self.data_bytes_written.saturating_add(pending.len() as u64);
        }
        self.persisted = Some(RecoveredHistory {
            commit,
            tail: Vec::new(),
            data_hasher: hasher,
        });
        self.durable_end = self.observed_end;
        self.pending.clear();
        self.dirty = false;
        Ok(())
    }

    fn publish_compaction(&mut self) -> Result<()> {
        let store_id = self
            .persisted
            .as_ref()
            .map(|persisted| persisted.commit.store_id)
            .unwrap_or_else(Uuid::new_v4);
        let bank_generation = self
            .persisted
            .as_ref()
            .map(|persisted| {
                persisted
                    .commit
                    .bank_generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("history bank generation overflow"))
            })
            .unwrap_or(Ok(1))?;
        let data_slot = self
            .persisted
            .as_ref()
            .map(|persisted| (persisted.commit.data_slot + 1) % HISTORY_BANK_COUNT)
            .unwrap_or(0);
        let header = HistoryBankHeader {
            store_id,
            session_id: history_session_id(&self.path),
            bank_generation,
            data_slot,
            capacity: self.cap as u64,
        };
        let snapshot: Vec<u8> = self.bytes.iter().copied().collect();
        let mut bank = header.encode();
        bank.extend_from_slice(&snapshot);
        let data_path = history_data_path(&self.path, data_slot);
        validate_optional_history_node(&data_path, "history data bank")?;
        atomic_write_bytes(&data_path, &bank)
            .with_context(|| format!("publish history data bank {}", data_path.display()))?;
        let mut hasher = Sha256::new();
        hasher.update(&snapshot);
        let commit = HistoryCommit {
            format_version: HISTORY_FORMAT_VERSION,
            store_id,
            session_id: header.session_id,
            commit_generation: self.next_commit_generation()?,
            bank_generation,
            data_slot,
            capacity: self.cap as u64,
            committed_len: snapshot.len() as u64,
            stream_end: self.observed_end,
            data_sha256: format!("{:x}", hasher.clone().finalize()),
            metadata_sha256: String::new(),
        };
        let commit = self.publish_commit(commit)?;
        publish_history_marker(&self.path, &commit)?;
        #[cfg(test)]
        {
            self.data_bytes_written = self
                .data_bytes_written
                .saturating_add(snapshot.len() as u64);
        }
        self.persisted = Some(RecoveredHistory {
            commit,
            tail: Vec::new(),
            data_hasher: hasher,
        });
        self.durable_end = self.observed_end;
        self.pending.clear();
        self.dirty = false;
        Ok(())
    }

    pub fn snapshot(&self, max: Option<usize>) -> Vec<u8> {
        let count = max.unwrap_or(self.bytes.len()).min(self.bytes.len());
        self.bytes
            .iter()
            .skip(self.bytes.len() - count)
            .copied()
            .collect()
    }
}

const CGROUP_V2_ROOT: &str = "/sys/fs/cgroup";
const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;
const TRUSTED_HELPER_DIRS: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/local/bin",
    "/run/current-system/sw/bin",
];

fn namespace_coordinates(path: &Path, label: &str) -> Result<(u64, u64)> {
    let metadata = fs::metadata(path).with_context(|| format!("inspect {label} namespace"))?;
    Ok((metadata.dev(), metadata.ino()))
}

fn ensure_cgroup2_filesystem(path: &Path) -> Result<()> {
    let encoded = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("encode cgroup path {}", path.display()))?;
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(encoded.as_ptr(), &mut stats) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("inspect filesystem for {}", path.display()));
    }
    if stats.f_type != CGROUP2_SUPER_MAGIC {
        bail!("{} is not on a cgroup-v2 filesystem", path.display());
    }
    Ok(())
}

fn mount_id_for_file(file: &File) -> Result<u64> {
    let path = format!("/proc/self/fdinfo/{}", file.as_raw_fd());
    let info = fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .map(str::trim)
        .ok_or_else(|| anyhow!("{path} has no mount identity"))?
        .parse()
        .with_context(|| format!("parse mount identity from {path}"))
}

/// Capture the kernel domain that gives a persisted cgroup locator meaning.
/// The root checks also keep resource-limit setup from accepting a lookalike
/// directory mounted at `/sys/fs/cgroup`.
pub fn current_cgroup_identity() -> Result<CgroupIdentity> {
    let root = Path::new(CGROUP_V2_ROOT);
    let root_handle = File::open(root).context("open cgroup-v2 root")?;
    let root_metadata = root_handle.metadata().context("inspect cgroup-v2 root")?;
    if !root_metadata.is_dir() {
        bail!("{CGROUP_V2_ROOT} is not a directory");
    }
    ensure_cgroup2_filesystem(root)?;
    let controllers = root.join("cgroup.controllers");
    if !fs::metadata(&controllers)
        .with_context(|| format!("inspect {}", controllers.display()))?
        .is_file()
    {
        bail!(
            "{} is not a cgroup-v2 controllers file",
            controllers.display()
        );
    }
    let (cgroup_namespace_device, cgroup_namespace_inode) =
        namespace_coordinates(Path::new("/proc/self/ns/cgroup"), "cgroup")?;
    let (mount_namespace_device, mount_namespace_inode) =
        namespace_coordinates(Path::new("/proc/self/ns/mnt"), "mount")?;
    Ok(CgroupIdentity {
        boot_id: linux_boot_id()?,
        cgroup_namespace_device,
        cgroup_namespace_inode,
        mount_namespace_device,
        mount_namespace_inode,
        cgroup_mount_id: mount_id_for_file(&root_handle)?,
        cgroup_root_device: root_metadata.dev(),
        cgroup_root_inode: root_metadata.ino(),
    })
}

fn verify_recorded_cgroup_identity(recorded: Option<&CgroupIdentity>) -> Result<CgroupIdentity> {
    let recorded = recorded.ok_or_else(|| {
        anyhow!(
            "recorded cgroup has no boot/namespace/mount identity; refusing legacy destructive recovery"
        )
    })?;
    let current = current_cgroup_identity()?;
    if recorded != &current {
        bail!(
            "recorded cgroup identity does not match the current boot, cgroup namespace, mount namespace, or cgroup-v2 root"
        );
    }
    Ok(current)
}

fn validate_trusted_helper(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("helper path is not absolute: {}", path.display());
    }
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("resolve helper executable {}", path.display()))?;
    let metadata = fs::metadata(&canonical)
        .with_context(|| format!("inspect helper executable {}", canonical.display()))?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
    {
        bail!(
            "untrusted helper executable {} (must be root-owned, executable, and not group/world writable)",
            canonical.display()
        );
    }
    let mut parent = canonical.parent();
    while let Some(directory) = parent {
        let metadata = fs::metadata(directory)
            .with_context(|| format!("inspect helper directory {}", directory.display()))?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!(
                "untrusted helper directory {} (must be root-owned and not group/world writable)",
                directory.display()
            );
        }
        parent = directory.parent();
    }
    Ok(canonical)
}

fn trusted_system_helper(name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains('/') {
        bail!("invalid system helper name {name:?}");
    }
    let mut failures = Vec::new();
    for directory in TRUSTED_HELPER_DIRS {
        let candidate = Path::new(directory).join(name);
        match validate_trusted_helper(&candidate) {
            Ok(path) => return Ok(path),
            Err(error) => failures.push(format!("{}: {error:#}", candidate.display())),
        }
    }
    bail!(
        "no trusted absolute {name} helper was found; {}",
        failures.join("; ")
    )
}

/// End-to-end probe of the system-manager scope backend behind the
/// `APLEXER_LAUNCH_SYSTEM_SCOPE=system` escape (issue #1): resolves the same
/// trusted helpers the real launch path resolves, then creates and collects
/// one trivial transient scope (`-- true`) on the system manager. This is
/// the only way to know the backend actually works -- as a regular user it
/// usually does not (`org.freedesktop.systemd1.manage-units` needs root or
/// a polkit authorization), and guessing would turn the opt-in escape into
/// a broken start. Probing creates no lasting state: the scope runs `true`,
/// exits, and `--collect` garbage-collects it. Never called unless the env
/// opt-in is set.
pub fn probe_system_scope_backend() -> Result<()> {
    let systemd_run = trusted_system_helper("systemd-run")?;
    let true_binary = trusted_system_helper("true")?;
    let unit = format!("aplexer-escape-probe-{}", Uuid::new_v4().simple());
    let mut command = Command::new(systemd_run);
    command
        .args([
            "--system",
            "--scope",
            "--collect",
            "--quiet",
            &format!("--unit={unit}"),
            "--",
        ])
        .arg(&true_binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut probe = command
        .spawn()
        .with_context(|| format!("spawn system-scope probe from {}", true_binary.display()))?;
    // Same discipline as every other helper child: register the pid so the
    // worker's descendant reaper cannot consume its status, and always reap
    // it here before dropping.
    let probe_pid = probe.id();
    crate::worker::own_child_pid(probe_pid);
    let probe_result = (|| -> Result<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match probe.try_wait()? {
                Some(status) => return Ok(status),
                None if Instant::now() >= deadline => {
                    let _ = probe.kill();
                    let _ = probe.wait();
                    bail!("systemd-run system-scope probe timed out after 10s");
                }
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
    })();
    crate::worker::disown_child_pid(probe_pid);
    let status = probe_result?;
    if !status.success() {
        bail!(
            "systemd-run --system --scope probe exited with {status}; creating system \
             manager scopes needs root or polkit authorization \
             (org.freedesktop.systemd1.manage-units)"
        );
    }
    Ok(())
}

/// Decide, once per launch, whether the opt-in escape backend is usable.
/// `Ok(true)`/`Ok(false)` mean "system scope"/"user manager fallback" for
/// the caller's placement decision; the error is the probe failure, for the
/// caller to report honestly instead of silently pretending the escape
/// happened (issue #1: warn or fail clearly).
pub fn system_scope_escape_decision() -> Result<bool> {
    if !crate::placement::system_scope_requested() {
        return Ok(false);
    }
    match probe_system_scope_backend() {
        Ok(()) => Ok(true),
        Err(error) => Err(error),
    }
}

/// Rewrite `worker` (already carrying the worker program and its initial
/// argv from `worker_command`) so spawning it creates the worker inside a
/// system-manager scope (`systemd-run --system --scope --collect
/// --unit=aplexer-worker-<id>`) instead of bare `setsid()` in the ambient
/// cgroup (issue #1). Everything configured on the command afterwards --
/// per-session env, `--rows/--cols` -- lands on the systemd-run wrapper and
/// is passed through to the worker child: systemd-run shares its own
/// environment with the scope's process, and argv after `--` is the
/// worker's argv verbatim.
///
/// The `pre_exec` closure the caller installs afterwards (setsid + signal
/// blocking) then applies to systemd-run itself; the worker inherits the
/// blocked-signal baseline it unblocks at startup and needs no session of
/// its own (the workload's spawn does its own `setsid` + `TIOCSCTTY`).
/// The wrapper stays the worker's parent for the worker's whole life --
/// one extra small process per escaped session -- and `--collect` removes
/// the scope as soon as the worker exits.
///
/// `Ok(())` means the command now spawns into the escape scope; the error
/// is the reason the escape was not applied, for the caller to surface.
/// The caller falls back to the plain (setsid-only, ambient-cgroup) spawn
/// in that case: the issue asks for honest degradation with a warning,
/// never for a broken start.
fn wrap_worker_in_system_scope(id: Uuid, worker: &mut Command) -> Result<()> {
    let systemd_run = trusted_system_helper("systemd-run")?;
    let program = worker.get_program().to_os_string();
    let worker_args: Vec<OsString> = worker.get_args().map(|arg| arg.to_os_string()).collect();
    let mut command = Command::new(systemd_run);
    command.args([
        "--system",
        "--scope",
        "--collect",
        "--quiet",
        &format!("--unit=aplexer-worker-{id}"),
        "--",
    ]);
    command.arg(&program);
    command.args(&worker_args);
    *worker = command;
    Ok(())
}

fn control_group_locator(id: Uuid, value: &str) -> Result<PathBuf> {
    let value = value.trim();
    let reported = Path::new(value);
    if value.is_empty() || value == "/" || !reported.is_absolute() {
        bail!("systemd returned invalid ControlGroup value {value:?}");
    }
    let mut relative = PathBuf::new();
    for component in reported.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => relative.push(value),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                bail!("systemd ControlGroup escapes cgroup root: {value:?}")
            }
        }
    }
    let expected = format!("aplexer-workload-{id}.scope");
    if relative.file_name() != Some(OsStr::new(&expected)) {
        bail!("systemd ControlGroup does not belong to session {id}: {value:?}");
    }
    Ok(Path::new(CGROUP_V2_ROOT).join(relative))
}

#[derive(Debug, Clone)]
pub struct Cgroup {
    path: PathBuf,
    identity: CgroupIdentity,
    /// Keep exclusive ownership of the unreaped child until release. An
    /// unreaped child reserves its pid, so Child::kill cannot be redirected
    /// to a recycled process; clones serialize the single kill+wait through
    /// this shared slot.
    anchor: Arc<Mutex<Option<std::process::Child>>>,
    initial_oom_kill: u64,
}

fn release_anchor_child(anchor: &mut std::process::Child) -> Result<()> {
    let pid = anchor.id();
    match anchor.kill() {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
        Err(error) => return Err(error).context("kill systemd-run anchor"),
    }
    let waited = anchor.wait().context("reap systemd-run anchor");
    // Released only after the wait has returned, so the worker's descendant
    // reaper can never consume this status first.
    crate::worker::disown_child_pid(pid);
    waited?;
    Ok(())
}

fn release_anchor_slot<T>(
    slot: &mut Option<T>,
    release: impl FnOnce(&mut T) -> Result<()>,
) -> Result<()> {
    if let Some(anchor) = slot.as_mut() {
        release(anchor)?;
        *slot = None;
    }
    Ok(())
}

fn cleanup_anchor_after_failure(
    anchor: &mut std::process::Child,
    error: anyhow::Error,
) -> anyhow::Error {
    match release_anchor_child(anchor) {
        Ok(()) => error,
        Err(cleanup_error) => {
            anyhow!("{error:#}; systemd-run anchor cleanup failed: {cleanup_error:#}")
        }
    }
}

impl Cgroup {
    // A worker's own ambient cgroup (inherited from whatever spawned `a start`,
    // e.g. a tmux pane or SSH session) is never a safe place to nest a
    // resource-limited child: cgroup v2 refuses to enable controllers in
    // cgroup.subtree_control while the parent still has processes attached
    // directly ("no internal process" constraint) -- and the worker, plus
    // everything else in that ambient session, is exactly such a process.
    // Writing to memory.max there fails closed with EACCES rather than
    // applying a limit; forcing it through would risk taking down unrelated
    // sessions sharing that ambient cgroup, which is the one failure mode
    // this project exists to prevent.
    //
    // Instead we ask systemd-run to create a fresh, independently delegated
    // scope (a sibling, not a nested child, of the ambient cgroup) and hold
    // it open with a placeholder process until the real workload can be
    // moved in.
    //
    // Which manager owns the new scope is a placement decision with a real
    // failure-domain consequence (issue #1): the default `--user` scope
    // lives beneath user@UID.service and dies with the per-user manager's
    // exit.target; the opt-in `--system` scope (APLEXER_LAUNCH_SYSTEM_SCOPE
    // = system, probed first via `system_scope_escape_decision`) lives under
    // the system manager and survives it. Probe failure downgrades to the
    // user manager with a printed warning -- limits still apply either way;
    // only the survival domain differs. A failure *after* a successful probe
    // (spawn, scope wait, controller delegation) fails closed exactly as the
    // `--user` path always has: a validated backend that then breaks is a
    // real error, not a placement preference to silently swap.
    pub fn create<F>(id: Uuid, limits: &Limits, setup_started: F) -> Result<Option<Self>>
    where
        F: FnOnce(),
    {
        ensure_sigchld_compatible_for_child_management()?;
        if !limits.requested() {
            return Ok(None);
        }
        let system_scope = match system_scope_escape_decision() {
            Ok(system_scope) => system_scope,
            Err(error) => {
                eprintln!(
                    "warning: APLEXER_LAUNCH_SYSTEM_SCOPE=system requested, but the \
                     system-scope backend is unavailable ({error:#}); the workload scope \
                     falls back to the per-user manager and inherits its exit.target \
                     failure domain"
                );
                false
            }
        };
        let bus_flag = if system_scope { "--system" } else { "--user" };
        let identity = current_cgroup_identity()?;
        // Resolve every executable before starting the scope. Ambient PATH is
        // intentionally irrelevant: a user-controlled shadow helper must not
        // choose or fabricate the containment domain we later trust.
        let systemd_run = trusted_system_helper("systemd-run")?;
        let systemctl = trusted_system_helper("systemctl")?;
        let sleep = trusted_system_helper("sleep")?;
        let unit = format!("aplexer-workload-{id}");
        let mut command = Command::new(systemd_run);
        command
            .arg(bus_flag)
            .arg("--scope")
            .arg("--collect")
            .arg(format!("--unit={unit}"))
            .arg("-p")
            .arg("Delegate=yes");
        if let Some(value) = limits.memory_bytes {
            command.arg("-p").arg(format!("MemoryMax={value}"));
            // Without a swap cap, hitting MemoryMax doesn't OOM-kill the
            // workload -- it swaps unboundedly instead, which both defeats
            // the purpose of a memory limit and risks host-wide I/O
            // pressure that *would* leak into unrelated sessions. A
            // memory-limited session gets no swap; a configurable swap
            // allowance is not yet exposed by the CLI.
            command.arg("-p").arg("MemorySwapMax=0");
        }
        if let Some(value) = limits.pids {
            command.arg("-p").arg(format!("TasksMax={value}"));
        }
        if let Some(quota) = limits.cpu_quota_us {
            let period = limits.cpu_period_us.unwrap_or(100_000);
            let percent = ((quota as f64 / period as f64) * 100.0).ceil().max(1.0) as u64;
            command.arg("-p").arg(format!("CPUQuota={percent}%"));
        }
        command
            .arg("--")
            .arg(sleep)
            .arg("infinity")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut anchor = command
            .spawn()
            .context("spawn systemd-run anchor; limits fail closed")?;
        // The worker waits on this pid itself (`release_anchor_child`), so
        // register it before anything else in the process can observe it as
        // a child. See `worker::OWNED_CHILD_PIDS`.
        crate::worker::own_child_pid(anchor.id());
        // From this point, systemd may own a scope member outside the worker's
        // procfs descendant tree. Let the caller preserve recovery evidence
        // until an authoritative cgroup path has been recorded.
        setup_started();
        let path = match wait_for_scope_cgroup(
            id,
            &unit,
            &identity,
            &systemctl,
            bus_flag,
            Duration::from_secs(5),
        ) {
            Ok(path) => path,
            Err(error) => {
                return Err(cleanup_anchor_after_failure(
                    &mut anchor,
                    error.context("limits fail closed"),
                ));
            }
        };
        if limits.memory_bytes.is_some() && !path.join("memory.max").exists() {
            return Err(cleanup_anchor_after_failure(
                &mut anchor,
                anyhow!("systemd did not delegate the memory controller; limits fail closed"),
            ));
        }
        if limits.pids.is_some() && !path.join("pids.max").exists() {
            return Err(cleanup_anchor_after_failure(
                &mut anchor,
                anyhow!("systemd did not delegate the pids controller; limits fail closed"),
            ));
        }
        let initial_oom_kill = read_counter(&path.join("memory.events"), "oom_kill").unwrap_or(0);
        Ok(Some(Self {
            path,
            identity,
            anchor: Arc::new(Mutex::new(Some(anchor))),
            initial_oom_kill,
        }))
    }
    /// Opens `cgroup.procs` for writing so the not-yet-exec'd workload child
    /// can move itself into the cgroup from inside a `pre_exec` closure
    /// (any process may write its own pid into a cgroup it has access to;
    /// this needs no cooperation from the parent after fork).
    ///
    /// We deliberately do not have the parent write the child's pid into
    /// `cgroup.procs` after `Command::spawn()` returns: `spawn()` itself
    /// blocks in the parent until the child either execs or reports a
    /// pre_exec failure, so any post-spawn, pre-exec rendezvous between
    /// parent and child (e.g. a gate the child waits on) deadlocks --
    /// the parent can never reach the code that would release it.
    pub fn open_procs(&self) -> Result<File> {
        OpenOptions::new()
            .write(true)
            .open(self.path.join("cgroup.procs"))
            .with_context(|| format!("open {}/cgroup.procs", self.path.display()))
    }
    pub fn locator(&self) -> &Path {
        &self.path
    }
    /// The same cgroup in `/proc/<pid>/cgroup` form (`/<relative>` under the
    /// cgroup-v2 root), so launch-time validation can compare what systemd
    /// was asked to create against what the workload actually reports being
    /// in (issue #1).
    pub fn proc_path(&self) -> String {
        let relative = self
            .path
            .strip_prefix(CGROUP_V2_ROOT)
            .unwrap_or(&self.path)
            .to_string_lossy()
            .to_string();
        format!("/{}", relative.trim_start_matches('/'))
    }
    pub fn identity(&self) -> &CgroupIdentity {
        &self.identity
    }
    /// Kills the placeholder process that was keeping the delegated scope
    /// alive. Call this only after the real workload pid has been added to
    /// the cgroup, so the cgroup never goes empty (and gets garbage
    /// collected by systemd) before the real workload takes residence.
    pub fn release_anchor(&self) -> Result<()> {
        let mut slot = self
            .anchor
            .lock()
            .map_err(|_| anyhow!("systemd-run anchor lock poisoned"))?;
        release_anchor_slot(&mut slot, release_anchor_child)
    }
    pub fn signal_all_until(&self, signal: i32, deadline: Instant) -> Result<()> {
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        verify_recorded_cgroup_identity(Some(&self.identity))?;
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        signal_cgroup_path_until(&self.path, signal, deadline)
    }
    pub fn kill_all_until(&self, deadline: Instant) -> Result<()> {
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        verify_recorded_cgroup_identity(Some(&self.identity))?;
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        kill_cgroup_path_until(&self.path, deadline)
    }
    pub fn populated(&self) -> Result<bool> {
        live_cgroup_populated_with(&self.identity, || {
            read_counter(&self.path.join("cgroup.events"), "populated")
        })
    }
    pub fn oom_killed(&self) -> bool {
        read_counter(&self.path.join("memory.events"), "oom_kill").unwrap_or(0)
            > self.initial_oom_kill
    }
    /// Live telemetry for a still-running cgroup. A workload's own OOM kill
    /// only shows up in the session record's `exit` field once the tracked
    /// PTY-owning process itself exits -- a subprocess it launched can be
    /// OOM-killed by the kernel while the shell survives, which is common
    /// and otherwise invisible. `a status` surfaces this live instead of
    /// only at session exit.
    pub fn stats(&self) -> serde_json::Value {
        let read_value = |name: &str| -> Option<u64> {
            fs::read_to_string(self.path.join(name))
                .ok()
                .and_then(|text| text.trim().parse().ok())
        };
        let oom_kill_total =
            read_counter(&self.path.join("memory.events"), "oom_kill").unwrap_or(0);
        serde_json::json!({
            "memory_current": read_value("memory.current"),
            "memory_peak": read_value("memory.peak"),
            "memory_swap_current": read_value("memory.swap.current"),
            "oom_kill_count": oom_kill_total,
            "oom_kill_count_since_start": oom_kill_total.saturating_sub(self.initial_oom_kill),
            // Status telemetry is explicitly best-effort; lifecycle and kill
            // paths call `populated` directly and propagate every error.
            "populated": self.populated().ok(),
        })
    }
    pub fn cleanup(&self) {
        let _ = self.release_anchor();
        let _ = fs::remove_dir(&self.path);
    }
}

/// Validate and recover a resource-limited session through its recorded
/// kernel containment domain. A path that has disappeared after it was
/// durably recorded is empty by construction: cgroup v2 cannot remove a
/// populated cgroup. Every other inspection error fails closed.
pub fn cleanup_recorded_cgroup(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
    signal: i32,
    grace: Duration,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(grace)
        .and_then(|deadline| deadline.checked_add(Duration::from_secs(2)))
        .ok_or_else(|| anyhow!("cgroup cleanup deadline overflow"))?;
    cleanup_recorded_cgroup_until(id, locator, identity, signal, grace, deadline)
}

/// Preflight a durable locator before destroying a broken session's worker
/// subreaper. This performs no signalling; it only establishes that later
/// cgroup recovery will operate inside the expected kernel domain.
pub fn validate_recorded_cgroup_locator(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
) -> Result<()> {
    validate_recorded_cgroup(id, locator, identity).map(|_| ())
}

/// Deadline-sharing variant for startup rollback, where cgroup recovery must
/// consume the same wall-clock budget as procfs discovery and pidfd cleanup.
pub fn cleanup_recorded_cgroup_until(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
    signal: i32,
    grace: Duration,
    deadline: Instant,
) -> Result<()> {
    check_cgroup_cleanup_deadline(deadline, "validating recorded cgroup")?;
    let Some(path) = validate_recorded_cgroup(id, locator, identity)? else {
        check_cgroup_cleanup_deadline(deadline, "validating recorded cgroup")?;
        return Ok(());
    };
    check_cgroup_cleanup_deadline(deadline, "validating recorded cgroup")?;

    if signal == libc::SIGKILL {
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        kill_cgroup_path_until(&path, deadline)?;
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
    } else {
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup")?;
        signal_cgroup_path_until(&path, signal, deadline)?;
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup")?;
        let grace_deadline = Instant::now()
            .checked_add(grace)
            .ok_or_else(|| anyhow!("cgroup cleanup grace deadline overflow"))?
            .min(deadline);
        while cgroup_path_populated_until(&path, deadline)? && Instant::now() < grace_deadline {
            sleep_until_cgroup_deadline(grace_deadline, "waiting for recorded cgroup grace")?;
        }
        if cgroup_path_populated_until(&path, deadline)? {
            check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
            kill_cgroup_path_until(&path, deadline)?;
            check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        }
    }

    while cgroup_path_populated_until(&path, deadline)? {
        // Older cgroup-v2 mounts may not expose cgroup.kill. Repeat the
        // identity-pinned cgroup.procs fallback so a member that forked
        // between the first read and signal cannot escape cleanup.
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        kill_cgroup_path_until(&path, deadline)?;
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        sleep_until_cgroup_deadline(deadline, "proving recorded cgroup empty")?;
    }
    Ok(())
}

fn check_cgroup_cleanup_deadline(deadline: Instant, operation: &str) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("timed out {operation}");
    }
    Ok(())
}

fn sleep_until_cgroup_deadline(deadline: Instant, operation: &str) -> Result<()> {
    check_cgroup_cleanup_deadline(deadline, operation)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    thread::sleep(Duration::from_millis(25).min(remaining));
    check_cgroup_cleanup_deadline(deadline, operation)
}

fn validate_recorded_cgroup(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
) -> Result<Option<PathBuf>> {
    // This comparison intentionally precedes canonicalizing the leaf. A
    // missing leaf proves emptiness only inside the exact kernel domain in
    // which it was durably recorded.
    let current_identity = verify_recorded_cgroup_identity(identity)?;
    let root = Path::new(CGROUP_V2_ROOT);
    let expected = format!("aplexer-workload-{id}.scope");
    if !locator.is_absolute()
        || !locator.starts_with(root)
        || locator.file_name() != Some(OsStr::new(&expected))
        || locator
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!(
            "untrusted recorded cgroup locator for session {id}: {}",
            locator.display()
        );
    }
    let canonical = match fs::canonicalize(locator) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("resolve recorded cgroup {}", locator.display()))
        }
    };
    let canonical_root = fs::canonicalize(root).context("resolve cgroup v2 root")?;
    if !canonical.starts_with(&canonical_root)
        || canonical.file_name() != Some(OsStr::new(&expected))
    {
        bail!(
            "recorded cgroup for session {id} escaped the cgroup root: {}",
            canonical.display()
        );
    }
    if !fs::metadata(&canonical)?.is_dir() {
        bail!(
            "recorded cgroup is not a directory: {}",
            canonical.display()
        );
    }
    ensure_cgroup2_filesystem(&canonical)?;
    let metadata = fs::metadata(&canonical)?;
    if metadata.dev() != current_identity.cgroup_root_device {
        bail!(
            "recorded cgroup {} is on a different cgroup-v2 mount",
            canonical.display()
        );
    }
    let procs = canonical.join("cgroup.procs");
    if !fs::metadata(&procs)
        .with_context(|| format!("inspect {}", procs.display()))?
        .is_file()
    {
        bail!("{} is not a cgroup member file", procs.display());
    }
    Ok(Some(canonical))
}

fn cgroup_path_populated(path: &Path) -> Result<bool> {
    match read_counter(&path.join("cgroup.events"), "populated") {
        Ok(value) => Ok(value != 0),
        Err(error) if error_is_not_found(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn error_is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
    })
}

/// Read live membership only while the cgroup pathname still belongs to the
/// exact kernel domain captured at creation. A collected scope loses both its
/// directory and `cgroup.events`; ENOENT therefore means empty, but only when
/// the domain matches both before and after that observation.
fn live_cgroup_populated_with(
    identity: &CgroupIdentity,
    read_populated: impl FnOnce() -> Result<u64>,
) -> Result<bool> {
    verify_recorded_cgroup_identity(Some(identity))
        .context("validate live cgroup identity before reading membership")?;
    let populated = match read_populated() {
        Ok(value) => Some(value != 0),
        Err(error) if error_is_not_found(&error) => None,
        Err(error) => return Err(error).context("read live cgroup membership"),
    };
    verify_recorded_cgroup_identity(Some(identity))
        .context("validate live cgroup identity after reading membership")?;
    Ok(populated.unwrap_or(false))
}

fn cgroup_path_populated_until(path: &Path, deadline: Instant) -> Result<bool> {
    check_cgroup_cleanup_deadline(deadline, "inspecting recorded cgroup")?;
    let populated = cgroup_path_populated(path)?;
    check_cgroup_cleanup_deadline(deadline, "inspecting recorded cgroup")?;
    Ok(populated)
}

fn read_cgroup_pids_until(path: &Path, deadline: Instant) -> Result<BTreeSet<i32>> {
    check_cgroup_cleanup_deadline(deadline, "reading recorded cgroup members")?;
    let procs = path.join("cgroup.procs");
    let file = match File::open(&procs) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", procs.display())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CGROUP_PROCS_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", procs.display()))?;
    check_cgroup_cleanup_deadline(deadline, "reading recorded cgroup members")?;
    if bytes.len() as u64 > MAX_CGROUP_PROCS_BYTES {
        bail!("recorded cgroup member list exceeds safe byte limit of {MAX_CGROUP_PROCS_BYTES}");
    }
    let text =
        std::str::from_utf8(&bytes).with_context(|| format!("decode {}", procs.display()))?;
    let mut pids = BTreeSet::new();
    for value in text.lines() {
        check_cgroup_cleanup_deadline(deadline, "parsing recorded cgroup members")?;
        if pids.len() >= MAX_CGROUP_RECOVERY_MEMBERS {
            bail!("recorded cgroup exceeds safe member limit of {MAX_CGROUP_RECOVERY_MEMBERS}");
        }
        let pid = value
            .parse::<i32>()
            .with_context(|| format!("parse pid in {}/cgroup.procs", path.display()))?;
        if pid <= 0 {
            bail!("invalid pid {pid} in {}/cgroup.procs", path.display());
        }
        pids.insert(pid);
    }
    Ok(pids)
}

struct CgroupMemberHandle {
    pid: i32,
    pidfd: File,
}

fn signal_cgroup_path_until(path: &Path, signal: i32, deadline: Instant) -> Result<()> {
    let candidates = read_cgroup_pids_until(path, deadline)?;
    let capacity = cgroup_recovery_pidfd_capacity(deadline)?;
    if candidates.len() > capacity {
        bail!(
            "recorded cgroup has {} members but only {capacity} pidfds can be opened safely",
            candidates.len()
        );
    }
    let mut members = Vec::with_capacity(candidates.len());
    for pid in candidates {
        check_cgroup_cleanup_deadline(deadline, "pinning recorded cgroup members")?;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(error).with_context(|| format!("open pidfd for cgroup member {pid}"));
        }
        members.push(CgroupMemberHandle {
            pid,
            pidfd: unsafe { File::from_raw_fd(fd) },
        });
    }

    // A pidfd pins process identity; this second membership snapshot ensures
    // each pinned identity still belongs to the recorded domain before it is
    // signalled. New forks are handled by the repeated populated/kill loop.
    let current = read_cgroup_pids_until(path, deadline)?;
    for member in members {
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup members")?;
        if !current.contains(&member.pid) {
            continue;
        }
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                member.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).with_context(|| format!("signal cgroup member {}", member.pid));
            }
        }
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup members")?;
    }
    Ok(())
}

fn cgroup_recovery_pidfd_capacity(deadline: Instant) -> Result<usize> {
    check_cgroup_cleanup_deadline(deadline, "preflighting cgroup recovery descriptors")?;
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error()).context("read RLIMIT_NOFILE for cgroup recovery");
    }
    check_cgroup_cleanup_deadline(deadline, "preflighting cgroup recovery descriptors")?;
    let descriptors = fs::read_dir("/proc/self/fd").context("count open recovery descriptors")?;
    let mut open = 0_u64;
    for descriptor in descriptors {
        check_cgroup_cleanup_deadline(deadline, "counting open recovery descriptors")?;
        descriptor.context("enumerate open recovery descriptors")?;
        open = open
            .checked_add(1)
            .ok_or_else(|| anyhow!("open recovery descriptor count overflow"))?;
    }
    let soft_limit = if limit.rlim_cur == libc::RLIM_INFINITY {
        u64::MAX
    } else {
        limit.rlim_cur
    };
    Ok(cgroup_recovery_pidfd_capacity_from_counts(soft_limit, open))
}

fn cgroup_recovery_pidfd_capacity_from_counts(soft_limit: u64, open: u64) -> usize {
    let available = soft_limit
        .saturating_sub(open)
        .saturating_sub(CGROUP_RECOVERY_FD_RESERVE);
    usize::try_from(available)
        .unwrap_or(usize::MAX)
        .min(MAX_CGROUP_RECOVERY_MEMBERS)
}

fn kill_cgroup_path_until(path: &Path, deadline: Instant) -> Result<()> {
    check_cgroup_cleanup_deadline(deadline, "checking recorded cgroup kill support")?;
    let kill = path.join("cgroup.kill");
    if kill.exists() {
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        let result = match fs::write(&kill, "1") {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("write {}", kill.display())),
        };
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        result
    } else {
        signal_cgroup_path_until(path, libc::SIGKILL, deadline)
    }
}
fn wait_for_scope_cgroup(
    id: Uuid,
    unit: &str,
    identity: &CgroupIdentity,
    systemctl: &Path,
    bus_flag: &str,
    timeout: Duration,
) -> Result<PathBuf> {
    wait_for_scope_cgroup_with(id, unit, systemctl, bus_flag, timeout, |path| {
        validate_recorded_cgroup(id, path, Some(identity))
    })
}

fn wait_for_scope_cgroup_with(
    id: Uuid,
    unit: &str,
    systemctl: &Path,
    bus_flag: &str,
    timeout: Duration,
    mut validate: impl FnMut(&Path) -> Result<Option<PathBuf>>,
) -> Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut command = Command::new(systemctl);
        command.args([
            bus_flag,
            "show",
            &format!("{unit}.scope"),
            "-p",
            "ControlGroup",
            "--value",
        ]);
        let output = command_output_until(&mut command, deadline, "query systemd scope")?;
        if output.status.success() {
            let value = std::str::from_utf8(&output.stdout)
                .context("decode systemd ControlGroup output")?;
            // systemd may publish the unit before assigning its ControlGroup.
            // Empty and root are transient "not assigned yet" values; every
            // other malformed, escaping, or wrong-session value is hostile
            // evidence and must fail closed rather than being retried.
            if !matches!(value.trim(), "" | "/") {
                let path = control_group_locator(id, value)?;
                if let Some(path) = validate(&path)? {
                    return Ok(path);
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for systemd scope {unit}.scope to appear");
        }
        thread::sleep(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

/// Reap a startup helper off the caller's critical path. The pid stays
/// registered as worker-owned (see `worker::OWNED_CHILD_PIDS`) until this
/// thread's `wait` returns, so the worker's descendant reaper cannot take
/// the status out from under it and cannot be handed a recycled pid early.
fn reap_helper_child_async(mut child: std::process::Child) {
    let pid = child.id();
    thread::spawn(move || {
        let _ = child.wait();
        crate::worker::disown_child_pid(pid);
    });
}

/// Run a small setup query without allowing a wedged helper to defeat the
/// caller's wall-clock timeout. Stdout is intentionally bounded: systemctl's
/// ControlGroup value is one short path, and anything larger is malformed.
fn command_output_until(
    command: &mut Command,
    deadline: Instant,
    operation: &str,
) -> Result<std::process::Output> {
    if Instant::now() >= deadline {
        bail!("timed out before {operation}");
    }
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn helper to {operation}"))?;
    // This helper's status belongs to this function (or to the detached
    // waiter `reap_helper_child_async` starts), never to the worker's
    // descendant reaper.
    let helper_pid = child.id();
    crate::worker::own_child_pid(helper_pid);
    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("{operation} helper has no stdout"))?;
    let flags = unsafe { libc::fcntl(child_stdout.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe {
            libc::fcntl(
                child_stdout.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        } < 0
    {
        let error = io::Error::last_os_error();
        let _ = child.kill();
        reap_helper_child_async(child);
        return Err(error).with_context(|| format!("make {operation} output nonblocking"));
    }
    let mut stdout = Vec::new();
    let mut stdout_eof = false;
    let mut status = None;
    loop {
        loop {
            let mut buffer = [0_u8; 4096];
            match child_stdout.read(&mut buffer) {
                Ok(0) => {
                    stdout_eof = true;
                    break;
                }
                Ok(count) => {
                    stdout.extend_from_slice(&buffer[..count]);
                    if stdout.len() > 64 * 1024 {
                        let _ = child.kill();
                        reap_helper_child_async(child);
                        bail!("output from {operation} exceeds 64 KiB");
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = child.kill();
                    reap_helper_child_async(child);
                    return Err(error).with_context(|| format!("read output from {operation}"));
                }
            }
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(Some(result)) => status = Some(result),
                Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = child.kill();
                    reap_helper_child_async(child);
                    return Err(error).with_context(|| format!("wait for {operation}"));
                }
            }
        }
        if let (Some(status), true) = (status, stdout_eof) {
            crate::worker::disown_child_pid(helper_pid);
            return Ok(std::process::Output {
                status,
                stdout,
                stderr: Vec::new(),
            });
        }

        if Instant::now() >= deadline {
            if status.is_none() {
                let _ = child.kill();
                // A helper stuck in uninterruptible sleep must not extend the
                // startup deadline. Reap asynchronously once the kernel permits.
                reap_helper_child_async(child);
            } else {
                crate::worker::disown_child_pid(helper_pid);
            }
            bail!("timed out waiting to {operation}");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(Duration::from_millis(20).min(remaining));
    }
}
fn read_counter(path: &Path, key: &str) -> Result<u64> {
    let text = fs::read_to_string(path)?;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some(key) {
            let value = parts
                .next()
                .ok_or_else(|| anyhow!("counter {key} in {} has no value", path.display()))?;
            return value
                .parse()
                .with_context(|| format!("parse counter {key} in {}", path.display()));
        }
    }
    bail!("counter {key} not found in {}", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn load_config_text(text: &str) -> Result<Config> {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        fs::write(&paths.config_file, text).unwrap();
        Config::load(&paths)
    }

    fn registry_record(paths: &Paths, id: Uuid) -> SessionRecord {
        SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id,
            workspace: paths.state_root.clone(),
            tag: "registry-test".into(),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/true".into()],
            cwd: paths.state_root.clone(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: DEFAULT_HISTORY_BYTES,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase: Phase::Exited,
            worker_pid: None,
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(true),
            socket_path: paths.socket(id),
            history_path: paths.history(id),
            exit: None,
            error: None,
        }
    }

    #[test]
    fn zcodex_is_a_built_in_codex_variant() {
        let config = load_config_text("").unwrap();
        let zcodex = config
            .engines
            .get("zcodex")
            .expect("built-in zcodex engine");
        assert_eq!(
            zcodex.command,
            vec![
                "zcodex".to_string(),
                "-c".to_string(),
                "check_for_update_on_startup=false".to_string(),
            ]
        );
        assert_eq!(
            zcodex.skip_permissions_argv,
            vec!["--dangerously-bypass-approvals-and-sandbox".to_string()]
        );
        assert_eq!(engine_family("zcodex"), "codex");
        assert_eq!(engine_family("codex"), "codex");
        assert_eq!(engine_family("claude"), "claude");
    }

    /// `config_keep_exited` is a second reader of the same setting, chosen
    /// so the worker's exit path does not depend on the whole config file
    /// validating. It must agree with `Config::load` on every shape that
    /// matters, or the escape hatch would silently mean different things to
    /// `a` and to the worker that acts on it.
    #[test]
    fn config_keep_exited_matches_full_config_load() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };

        // No config file at all: the documented default.
        assert!(!config_keep_exited(&paths));

        for (text, expected) in [
            ("version = 1\n", false),
            ("version = 1\nkeep_exited = false\n", false),
            ("version = 1\nkeep_exited = true\n", true),
        ] {
            fs::write(&paths.config_file, text).unwrap();
            assert_eq!(
                Config::load(&paths).unwrap().keep_exited,
                expected,
                "Config::load disagreed for {text:?}"
            );
            assert_eq!(
                config_keep_exited(&paths),
                expected,
                "config_keep_exited disagreed for {text:?}"
            );
        }

        // An unrelated invalid entry fails `Config::load` outright. The
        // worker's reader must not treat that as "keep records": a typo in
        // an engine definition is not a retention decision.
        fs::write(
            &paths.config_file,
            "version = 1\nkeep_exited = true\ndefault_engine = \"nope\"\n",
        )
        .unwrap();
        assert!(Config::load(&paths).is_err());
        assert!(config_keep_exited(&paths));

        // Unparsable or unreadable config: default, never a panic.
        fs::write(&paths.config_file, "this is not toml {{{").unwrap();
        assert!(!config_keep_exited(&paths));
    }

    #[test]
    fn registry_enumeration_reports_corrupt_records() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();
        fs::write(paths.record(id), b"{truncated").unwrap();

        let error = list_records(&paths).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(&id.to_string()), "{message}");
        assert!(message.contains("parse"), "{message}");
    }

    /// The window `start_session` opens between creating a session directory
    /// and writing that session's first record. Any reader that does not hold
    /// the registry lock can land in it, and treating it as corruption killed
    /// `a watch` outright (see `list_records`). The same fixture must still be
    /// reported once the record appears, so the entry is skipped, not
    /// blacklisted.
    #[test]
    fn registry_enumeration_skips_a_session_whose_record_is_not_written_yet() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let pending = Uuid::new_v4();
        fs::create_dir(paths.state_session(pending)).unwrap();
        let written = Uuid::new_v4();
        fs::create_dir(paths.state_session(written)).unwrap();
        atomic_write_json(&paths.record(written), &registry_record(&paths, written)).unwrap();

        let records = list_records(&paths).unwrap();
        assert_eq!(
            records.iter().map(|record| record.id).collect::<Vec<_>>(),
            vec![written],
            "a session mid-creation must be skipped, not reported and not fatal"
        );

        // ... and picked up as soon as its record lands.
        atomic_write_json(&paths.record(pending), &registry_record(&paths, pending)).unwrap();
        let mut ids = list_records(&paths)
            .unwrap()
            .iter()
            .map(|record| record.id)
            .collect::<Vec<_>>();
        ids.sort();
        let mut expected = vec![pending, written];
        expected.sort();
        assert_eq!(ids, expected);
    }

    /// The complement of the test above: skipping a missing record must not
    /// weaken the fail-closed contract for a record that is present and wrong.
    #[test]
    fn registry_enumeration_still_fails_closed_on_an_empty_record_file() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();
        fs::write(paths.record(id), b"").unwrap();

        let error = list_records(&paths).unwrap_err();
        assert!(format!("{error:#}").contains("parse"), "{error:#}");
    }

    #[test]
    fn registry_enumeration_reports_unsupported_schema() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();
        let mut record = registry_record(&paths, id);
        record.schema_version = SCHEMA_VERSION + 1;
        atomic_write_json(&paths.record(id), &record).unwrap();

        let error = list_records(&paths).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported session schema"));
    }

    #[test]
    fn registry_enumeration_validates_directory_id_and_paths() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();

        let mut record = registry_record(&paths, id);
        record.id = Uuid::new_v4();
        atomic_write_json(&paths.record(id), &record).unwrap();
        assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("directory id"));

        record = registry_record(&paths, id);
        record.socket_path = paths.socket(Uuid::new_v4());
        atomic_write_json(&paths.record(id), &record).unwrap();
        assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("socket path"));

        record = registry_record(&paths, id);
        record.history_path = paths.history(Uuid::new_v4());
        atomic_write_json(&paths.record(id), &record).unwrap();
        assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("history path"));
    }

    #[test]
    fn registry_enumeration_grandfathers_legacy_history_capacity() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let id = Uuid::new_v4();
        fs::create_dir(paths.state_session(id)).unwrap();

        let mut record = registry_record(&paths, id);
        record.history_bytes = MAX_HISTORY_BYTES + 1;
        atomic_write_json(&paths.record(id), &record).unwrap();

        let records = list_records(&paths).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].history_bytes, MAX_HISTORY_BYTES + 1);
    }

    #[test]
    fn registry_enumeration_rejects_unexpected_entries() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        paths.ensure().unwrap();
        let unexpected = paths.state_root.join("sessions").join("leftover");
        fs::write(&unexpected, b"not a session directory").unwrap();

        let error = list_records(&paths).unwrap_err();
        assert!(format!("{error:#}").contains("is not a directory"));
    }

    #[test]
    fn ensure_private_dir_rejects_leaf_symlink_without_chmodding_target() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let link = root.path().join("link");
        symlink(&target, &link).unwrap();

        let error = ensure_private_dir(&link).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("without following symbolic links"),
            "{error:#}"
        );
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn ensure_private_dir_rejects_symlink_ancestor_without_creating_beneath_it() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = root.path().join("link");
        symlink(&target, &link).unwrap();

        assert!(ensure_private_dir(&link.join("child")).is_err());
        assert!(!target.join("child").exists());
    }

    #[test]
    fn ensure_private_dir_validates_type_before_chmod() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("ordinary-file");
        fs::write(&file, b"not a directory").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(ensure_private_dir(&file).is_err());
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn ensure_private_dir_chmods_verified_directory() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("private");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_dir(&directory).unwrap();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn atomic_write_json_removes_temp_after_rename_failure() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("record.json");
        fs::create_dir(&destination).unwrap();

        assert!(atomic_write_json(&destination, &serde_json::json!({"secret": "value"})).is_err());
        assert_no_atomic_temps(root.path());
    }

    #[test]
    fn atomic_write_bytes_removes_temp_after_rename_failure() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("history.bin");
        fs::create_dir(&destination).unwrap();

        assert!(atomic_write_bytes(&destination, b"secret bytes").is_err());
        assert_no_atomic_temps(root.path());
    }

    fn assert_no_atomic_temps(directory: &Path) {
        let leftovers = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }

    #[test]
    fn session_record_write_persists_worker_start_identity_once() {
        let root = tempfile::tempdir().unwrap();
        let record_path = root.path().join("session.json");
        let pid = std::process::id();
        atomic_write_json(
            &record_path,
            &serde_json::json!({"worker_pid": pid, "value": 1}),
        )
        .unwrap();
        let identity_path = root.path().join(WORKER_IDENTITY_FILE);
        let original: ProcessIdentity =
            serde_json::from_slice(&fs::read(&identity_path).unwrap()).unwrap();
        assert_eq!(original.pid, pid);
        assert_eq!(original.boot_id, linux_boot_id().unwrap());
        assert_eq!(
            original.start_time_ticks,
            process_start_time_ticks(pid).unwrap()
        );

        // A later write must not refresh the immutable registration.
        atomic_write_json(
            &record_path,
            &serde_json::json!({"worker_pid": pid, "value": 2}),
        )
        .unwrap();
        let after: ProcessIdentity =
            serde_json::from_slice(&fs::read(&identity_path).unwrap()).unwrap();
        assert_eq!(after.pid, original.pid);
        assert_eq!(after.start_time_ticks, original.start_time_ticks);
    }

    fn liveness_record(state_dir: &Path) -> SessionRecord {
        let pid = std::process::id();
        SessionRecord {
            parent_session: None,
            schema_version: SCHEMA_VERSION,
            id: Uuid::new_v4(),
            workspace: state_dir.to_path_buf(),
            tag: "identity-test".into(),
            engine: "shell".into(),
            profile: None,
            command: vec!["/bin/true".into()],
            cwd: state_dir.to_path_buf(),
            env: BTreeMap::new(),
            env_unset: Vec::new(),
            limits: Limits::default(),
            history_bytes: DEFAULT_HISTORY_BYTES,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_activity_ms: None,
            last_accessed_ms: None,
            reported_state: None,
            reported_state_at_ms: None,
            phase: Phase::Running,
            worker_pid: Some(pid),
            workload_pid: None,
            worker_cgroup: None,
            workload_cgroup: None,
            containment_cgroup: None,
            containment_cgroup_identity: None,
            containment_empty: Some(false),
            socket_path: state_dir.join("control.sock"),
            history_path: state_dir.join("history.bin"),
            exit: None,
            error: None,
        }
    }

    /// The two proof shapes that short-circuit before the kernel is ever
    /// consulted, and the no-locator shape that is the reported zombie.
    #[test]
    fn containment_reap_verdict_reads_durable_proof_without_probing() {
        let state = tempfile::tempdir().unwrap();
        let base = liveness_record(state.path());
        let refuse = |_: Uuid, _: &Path, _: Option<&CgroupIdentity>| -> Result<bool> {
            panic!("probe must not run when the record already answers the question")
        };

        let mut proven = base.clone();
        proven.containment_empty = Some(true);
        assert_eq!(
            containment_reap_verdict_with(&proven, refuse),
            ContainmentReap::Proven,
            "a worker's own durable proof must still be trusted"
        );

        let mut legacy_exit = base.clone();
        legacy_exit.containment_empty = None;
        legacy_exit.exit = Some(ExitInfo {
            code: Some(0),
            signal: None,
            oom_killed: false,
            exited_at_ms: 2,
        });
        assert_eq!(
            containment_reap_verdict_with(&legacy_exit, refuse),
            ContainmentReap::Proven,
            "the legacy pre-field ExitInfo proof must still be trusted"
        );

        // The reported zombie shape: unlimited session, worker SIGKILLed
        // before it could prove anything. No locator, so nothing to probe.
        let unlimited = base.clone();
        assert_eq!(unlimited.containment_cgroup, None);
        assert_eq!(unlimited.containment_empty, Some(false));
        assert_eq!(
            containment_reap_verdict_with(&unlimited, refuse),
            ContainmentReap::NoRemainingHandle
        );
    }

    /// Every outcome the kernel probe can return, including the one arm that
    /// stands between `a prune` and deleting the last handle to a live
    /// containment domain: a locator that validates and is still POPULATED
    /// must retain. Injected rather than staged on a real cgroup so this
    /// runs everywhere, on every `cargo test`, with no delegation needed;
    /// `recorded_cgroup_observed_empty_tracks_a_real_delegated_cgroup`
    /// covers the probe itself against a real one.
    #[test]
    fn containment_reap_verdict_maps_every_cgroup_probe_outcome() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_empty = Some(false);
        record.containment_cgroup = Some(PathBuf::from(format!(
            "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
            record.id
        )));

        assert_eq!(
            containment_reap_verdict_with(&record, |_, _, _| Ok(true)),
            ContainmentReap::Proven,
            "an observed-empty domain is proof at least as strong as the persisted bit"
        );
        assert_eq!(
            containment_reap_verdict_with(&record, |_, _, _| Ok(false)),
            ContainmentReap::Retain,
            "a populated containment domain must keep its locator"
        );
        assert_eq!(
            containment_reap_verdict_with(&record, |_, _, _| bail!("cgroup inspection failed")),
            ContainmentReap::Retain,
            "an unreadable containment domain must fail closed"
        );

        // The probe is handed the record's own identity triple -- a mixed-up
        // locator would validate against the wrong domain.
        let mut seen = None;
        containment_reap_verdict_with(&record, |id, locator, identity| {
            seen = Some((id, locator.to_path_buf(), identity.cloned()));
            Ok(false)
        });
        let (id, locator, identity) = seen.expect("probe ran");
        assert_eq!(id, record.id);
        assert_eq!(Some(locator), record.containment_cgroup);
        assert!(identity.is_none());
    }

    /// A recorded cgroup with no identity cannot be validated, so it cannot
    /// be declared empty either -- keep the locator.
    #[test]
    fn containment_reap_verdict_retains_an_unvalidatable_locator() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_cgroup = Some(PathBuf::from(format!(
            "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
            record.id
        )));
        assert!(record.containment_cgroup_identity.is_none());
        assert_eq!(
            containment_reap_verdict(&record),
            ContainmentReap::Retain,
            "an unvalidatable containment locator must fail closed"
        );
    }

    /// A cgroup recorded under a different boot cannot hold a live process:
    /// the hierarchy and every task in it ceased to exist at reboot. Without
    /// this, `validate_recorded_cgroup`'s (correct, for destructive
    /// recovery) refusal to touch a foreign-boot identity would make a
    /// rebooted-away record permanently unreapable.
    #[test]
    fn containment_reap_verdict_treats_a_previous_boot_as_empty() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_cgroup = Some(PathBuf::from(format!(
            "{CGROUP_V2_ROOT}/aplexer-workload-{}.scope",
            record.id
        )));
        let mut identity = current_cgroup_identity().unwrap_or(CgroupIdentity {
            boot_id: String::new(),
            cgroup_namespace_device: 0,
            cgroup_namespace_inode: 0,
            mount_namespace_device: 0,
            mount_namespace_inode: 0,
            cgroup_mount_id: 0,
            cgroup_root_device: 0,
            cgroup_root_inode: 0,
        });
        identity.boot_id = "00000000-0000-0000-0000-000000000000".into();
        assert_ne!(identity.boot_id, linux_boot_id().unwrap());
        record.containment_cgroup_identity = Some(identity);
        assert_eq!(
            containment_reap_verdict(&record),
            ContainmentReap::Proven,
            "a cgroup from a previous boot cannot hold a live process"
        );
    }

    /// The membership half of the real probe, without needing a real
    /// cgroup: `cgroup.events` says `populated 1` while tasks remain, and a
    /// collected cgroup loses the file entirely (ENOENT means empty).
    #[test]
    fn cgroup_path_populated_reads_the_kernel_counter_and_treats_enoent_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            !cgroup_path_populated(dir.path()).unwrap(),
            "a collected cgroup (no cgroup.events) is empty, not an error"
        );
        fs::write(dir.path().join("cgroup.events"), "populated 1\nfrozen 0\n").unwrap();
        assert!(cgroup_path_populated(dir.path()).unwrap());
        fs::write(dir.path().join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        assert!(!cgroup_path_populated(dir.path()).unwrap());
        fs::write(dir.path().join("cgroup.events"), "frozen 0\n").unwrap();
        assert!(
            cgroup_path_populated(dir.path()).is_err(),
            "a cgroup.events with no populated key must fail closed, not read as empty"
        );
    }

    /// A cgroup created inside the caller's own delegated subtree, named
    /// exactly the way a real session's containment scope is named, so
    /// `validate_recorded_cgroup`'s full chain (locator shape, cgroup-v2
    /// filesystem, mount device, identity triple) runs for real. Returns
    /// None when the environment has no writable cgroup-v2 parent.
    struct DelegatedCgroup {
        path: PathBuf,
        members: Vec<std::process::Child>,
    }

    impl DelegatedCgroup {
        fn create(id: Uuid) -> Option<Self> {
            let own = fs::read_to_string("/proc/self/cgroup").ok()?;
            let relative = own
                .lines()
                .find_map(|line| line.strip_prefix("0::"))?
                .trim()
                .trim_start_matches('/')
                .to_string();
            let mut candidate = Path::new(CGROUP_V2_ROOT).join(&relative);
            let leaf = format!("aplexer-workload-{id}.scope");
            // Walk up until a parent accepts a new child cgroup: the leaf a
            // test process sits in is usually not delegated, its user@.service
            // ancestor is.
            loop {
                let path = candidate.join(&leaf);
                if fs::create_dir(&path).is_ok() {
                    return Some(Self {
                        path,
                        members: Vec::new(),
                    });
                }
                candidate = candidate.parent()?.to_path_buf();
                if !candidate.starts_with(CGROUP_V2_ROOT) || candidate == Path::new(CGROUP_V2_ROOT)
                {
                    return None;
                }
            }
        }

        fn populate(&mut self) -> u32 {
            let child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn cgroup member");
            let pid = child.id();
            self.members.push(child);
            fs::write(self.path.join("cgroup.procs"), format!("{pid}\n"))
                .expect("move member into the delegated cgroup");
            pid
        }

        /// Stop every member and reap it, so the cgroup can be collected and
        /// no `sleep` outlives the test.
        fn drain_members(&mut self) {
            for mut member in self.members.drain(..) {
                let _ = member.kill();
                let _ = member.wait();
            }
        }
    }

    impl Drop for DelegatedCgroup {
        fn drop(&mut self) {
            self.drain_members();
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.path.exists() && Instant::now() < deadline {
                if fs::remove_dir(&self.path).is_ok() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    }

    /// The real kernel probe, end to end, against a genuinely delegated
    /// cgroup: empty, then POPULATED (the arm that must retain), then empty
    /// again, then collected. `#[ignore]`d for the same reason
    /// `tests/oom_isolation.rs`'s destructive tests are -- it needs a
    /// cgroup-v2 tree with delegation to the running user, which a CI
    /// container generally lacks. Run it explicitly:
    ///
    ///   cargo test --lib recorded_cgroup_observed_empty -- --ignored --nocapture
    ///
    /// The decision arms it feeds are pinned unconditionally by
    /// `containment_reap_verdict_maps_every_cgroup_probe_outcome`, and the
    /// membership read by
    /// `cgroup_path_populated_reads_the_kernel_counter_and_treats_enoent_as_empty`;
    /// this test is what proves those two meet reality.
    #[test]
    #[ignore = "needs cgroup-v2 delegation to the running user; run explicitly"]
    fn recorded_cgroup_observed_empty_tracks_a_real_delegated_cgroup() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        record.containment_empty = Some(false);
        let mut cgroup = DelegatedCgroup::create(record.id)
            .expect("this environment has no writable cgroup-v2 parent");
        record.containment_cgroup = Some(cgroup.path.clone());
        record.containment_cgroup_identity = Some(current_cgroup_identity().unwrap());
        let probe = || {
            recorded_cgroup_observed_empty(
                record.id,
                record.containment_cgroup.as_deref().unwrap(),
                record.containment_cgroup_identity.as_ref(),
            )
            .unwrap()
        };

        assert!(probe(), "a freshly created cgroup is empty");
        assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);

        let pid = cgroup.populate();
        assert!(
            !probe(),
            "a cgroup holding a live process must not read as empty"
        );
        assert_eq!(
            containment_reap_verdict(&record),
            ContainmentReap::Retain,
            "prune must keep the locator of a populated containment domain"
        );
        assert!(process_alive(pid), "probing must not signal anything");

        cgroup.drain_members();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !probe() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(probe(), "an emptied cgroup must read as empty again");
        assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);

        // Collected: cgroup v2 cannot remove a populated cgroup, so a
        // durably recorded locator that has since disappeared is empty by
        // construction.
        fs::remove_dir(&cgroup.path).expect("remove the now-empty cgroup");
        assert!(probe(), "a collected cgroup is empty by construction");
        assert_eq!(containment_reap_verdict(&record), ContainmentReap::Proven);
    }

    /// `state` is derived from both facts and rewrites neither.
    #[test]
    fn observed_state_reports_broken_only_for_a_contradicted_phase() {
        let now = DEFAULT_STARTUP_TIMEOUT_MS * 2;
        let aged = |age: u64| now - age;
        // A `Starting` record with no live worker is the shape
        // `start_session` persists before the worker registers its pid, and
        // also the shape a crashed start leaves behind. Only age tells them
        // apart, and the boundary is exactly the startup budget.
        assert_eq!(
            observed_state(&Phase::Starting, false, aged(0), now),
            "starting"
        );
        assert_eq!(
            observed_state(
                &Phase::Starting,
                false,
                aged(DEFAULT_STARTUP_TIMEOUT_MS - 1),
                now
            ),
            "starting"
        );
        assert_eq!(
            observed_state(
                &Phase::Starting,
                false,
                aged(DEFAULT_STARTUP_TIMEOUT_MS),
                now
            ),
            "broken",
            "past the startup budget a pre-PID record is a crashed start"
        );
        // A record whose clock ran backwards (or was written by a machine
        // with a different clock) must not become permanently `starting`.
        assert_eq!(
            observed_state(&Phase::Starting, false, now + 1_000, now),
            "starting"
        );
        assert_eq!(observed_state(&Phase::Starting, true, 0, now), "starting");
        // Running/Exiting are only ever written by a worker that already
        // registered, so a dead worker there is broken at any age.
        for phase in [Phase::Running, Phase::Exiting] {
            assert_eq!(observed_state(&phase, false, aged(0), now), "broken");
            assert_eq!(observed_state(&phase, false, aged(1), now), "broken");
            assert_eq!(observed_state(&phase, true, aged(0), now), phase.name());
        }
        for phase in [Phase::Exited, Phase::Failed] {
            assert_eq!(observed_state(&phase, false, aged(0), now), phase.name());
            assert_eq!(observed_state(&phase, true, aged(0), now), phase.name());
        }
    }

    #[test]
    fn worker_liveness_rejects_recycled_pid_identity() {
        let state = tempfile::tempdir().unwrap();
        let mut record = liveness_record(state.path());
        let pid = record.worker_pid.unwrap();
        let identity = ProcessIdentity {
            pid,
            start_time_ticks: process_start_time_ticks(pid).unwrap() + 1,
            boot_id: linux_boot_id().unwrap(),
        };
        fs::write(
            state.path().join(WORKER_IDENTITY_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();

        assert!(!record.worker_alive());
        record.phase = Phase::Failed;
        assert!(record.worker_finished());
    }

    #[test]
    fn worker_liveness_uses_safe_legacy_fallback_for_missing_or_corrupt_identity() {
        let state = tempfile::tempdir().unwrap();
        let record = liveness_record(state.path());
        assert!(record.worker_alive(), "missing sidecar uses numeric pid");

        fs::write(state.path().join(WORKER_IDENTITY_FILE), b"not-json").unwrap();
        assert!(record.worker_alive(), "corrupt sidecar fails closed");

        let identity = ProcessIdentity {
            pid: record.worker_pid.unwrap() + 1,
            start_time_ticks: 0,
            boot_id: "corrupt".into(),
        };
        fs::write(
            state.path().join(WORKER_IDENTITY_FILE),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        assert!(record.worker_alive(), "pid mismatch fails closed");
    }

    #[test]
    fn frame_round_trip() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, FrameKind::Data, b"a\0b").unwrap();
        let mut cursor = io::Cursor::new(bytes);
        let frame = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(frame.kind, FrameKind::Data);
        assert_eq!(frame.payload, b"a\0b");
    }

    #[test]
    fn bound_request_remains_readable_by_legacy_workers() {
        #[derive(Deserialize)]
        struct LegacyRequest {
            version: u16,
            request_id: String,
            #[serde(flatten)]
            operation: Operation,
        }

        let request = Request::new(Uuid::new_v4(), Operation::Ping);
        let legacy: LegacyRequest =
            serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert_eq!(legacy.version, PROTOCOL_VERSION);
        assert_eq!(legacy.request_id, request.request_id);
        assert!(matches!(legacy.operation, Operation::Ping));
    }

    #[test]
    fn bounded_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::open(dir.path().join("h"), 4).unwrap();
        h.append(b"abcdef").unwrap();
        assert_eq!(h.snapshot(None), b"cdef");
    }

    #[test]
    fn history_incremental_flush_writes_only_delta_and_recovers_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 8).unwrap();

        history.append(b"abcdef").unwrap();
        history.flush().unwrap();
        assert_eq!(history.data_bytes_written, 6);
        history.append(b"\0g").unwrap();
        history.flush().unwrap();
        assert_eq!(history.data_bytes_written, 8);
        history.append(b"hi").unwrap();
        history.flush().unwrap();
        assert_eq!(history.data_bytes_written, 10);
        assert_eq!(history.snapshot(None), b"cdef\0ghi");

        let reopened = History::open(path.clone(), 8).unwrap();
        assert_eq!(reopened.snapshot(None), b"cdef\0ghi");
        assert_eq!(
            read_persisted_history_tail(&path, None).unwrap(),
            b"cdef\0ghi"
        );
    }

    #[test]
    fn history_uncommitted_suffix_is_ignored_and_truncated_on_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"safe").unwrap();
        history.flush().unwrap();

        let data_path = history_data_path(&path, 0);
        OpenOptions::new()
            .append(true)
            .open(&data_path)
            .unwrap()
            .write_all(b"torn")
            .unwrap();
        let mut reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"safe");
        reopened.append(b"-next").unwrap();
        reopened.flush().unwrap();
        assert_eq!(
            History::open(path, 16).unwrap().snapshot(None),
            b"safe-next"
        );
    }

    #[test]
    fn history_corrupt_newest_commit_recovers_previous_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"prior").unwrap();
        history.flush().unwrap();
        history.append(b"-newest").unwrap();
        history.flush().unwrap();

        fs::write(history_commit_path(&path, 0), b"{torn").unwrap();
        let reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"prior");
        assert_eq!(read_persisted_history_tail(&path, None).unwrap(), b"prior");
    }

    #[test]
    fn history_corrupt_v2_pair_never_falls_back_to_stale_raw_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"prior").unwrap();
        history.flush().unwrap();
        history.append(b"-newest").unwrap();
        history.flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"prior-newest");

        fs::write(history_commit_path(&path, 0), b"{torn-newest").unwrap();
        fs::write(history_commit_path(&path, 1), b"{torn-prior").unwrap();
        let read_error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{read_error:#}").contains("no valid committed history generation"),
            "{read_error:#}"
        );
        assert!(History::open(path.clone(), 16).is_err());
        assert_eq!(
            fs::read(path).unwrap(),
            b"prior-newest",
            "fail-closed v2 recovery mutated the raw compatibility evidence"
        );
    }

    #[test]
    fn history_marker_prevents_raw_fallback_when_all_commits_disappear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"v2-authoritative").unwrap();
        history.flush().unwrap();
        assert!(history_marker_path(&path).is_file());
        assert!(history_data_path(&path, 0).is_file());

        fs::write(&path, b"stale-raw").unwrap();
        for slot in 0..HISTORY_COMMIT_COUNT {
            let commit = history_commit_path(&path, slot);
            if commit.exists() {
                fs::remove_file(commit).unwrap();
            }
        }

        let read_error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{read_error:#}").contains("no valid committed history generation"),
            "{read_error:#}"
        );
        assert!(History::open(path.clone(), 16).is_err());
        assert_eq!(fs::read(path).unwrap(), b"stale-raw");
    }

    #[test]
    fn history_unpublished_first_bank_without_marker_still_recovers_legacy_raw() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"raw-precommit").unwrap();
        let blocked_commit = history_commit_path(&path, 1);
        fs::create_dir(&blocked_commit).unwrap();

        assert!(history.flush().is_err());
        assert!(history_data_path(&path, 0).is_file());
        assert!(!history_marker_path(&path).exists());
        assert_eq!(fs::read(&path).unwrap(), b"raw-precommit");
        drop(history);
        fs::remove_dir(blocked_commit).unwrap();

        assert_eq!(
            read_persisted_history_tail(&path, None).unwrap(),
            b"raw-precommit"
        );
        let reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"raw-precommit");
        assert!(history_marker_path(&path).is_file());
    }

    #[test]
    fn history_markerless_v2_is_readable_and_next_writable_open_publishes_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"pre-marker-v2").unwrap();
        history.flush().unwrap();
        fs::remove_file(history_marker_path(&path)).unwrap();

        assert_eq!(
            read_persisted_history_tail(&path, None).unwrap(),
            b"pre-marker-v2"
        );
        assert!(!history_marker_path(&path).exists());
        let reopened = History::open(path.clone(), 16).unwrap();
        assert_eq!(reopened.snapshot(None), b"pre-marker-v2");
        assert!(history_marker_path(&path).is_file());
    }

    #[test]
    fn history_marker_is_bounded_checksummed_and_a_safe_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let marker_path = history_marker_path(&path);
        let mut history = History::open(path.clone(), 16).unwrap();
        history.append(b"committed").unwrap();
        history.flush().unwrap();
        let valid_marker = fs::read(&marker_path).unwrap();

        let mut bad_checksum: HistoryMarker = serde_json::from_slice(&valid_marker).unwrap();
        bad_checksum.store_id = Uuid::new_v4();
        fs::write(&marker_path, serde_json::to_vec(&bad_checksum).unwrap()).unwrap();
        let error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("checksum mismatch"),
            "{error:#}"
        );

        let wrong_store = bad_checksum.seal().unwrap();
        fs::write(&marker_path, serde_json::to_vec(&wrong_store).unwrap()).unwrap();
        let error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("no valid committed history generation"),
            "{error:#}"
        );

        fs::write(&marker_path, vec![b'x'; HISTORY_MARKER_MAX_BYTES + 1]).unwrap();
        let error = read_persisted_history_tail(&path, None).unwrap_err();
        assert!(format!("{error:#}").contains("exceeds the"), "{error:#}");

        fs::remove_file(&marker_path).unwrap();
        let target = dir.path().join("marker-target");
        fs::write(&target, b"unrelated").unwrap();
        symlink(&target, &marker_path).unwrap();
        assert!(read_persisted_history_tail(&path, None).is_err());
        fs::remove_file(&marker_path).unwrap();

        let marker_c = CString::new(marker_path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(marker_c.as_ptr(), 0o600) }, 0);
        assert!(read_persisted_history_tail(&path, None).is_err());
        assert!(History::open(path.clone(), 16).is_err());
        assert_eq!(fs::read(target).unwrap(), b"unrelated");
    }

    #[test]
    fn history_compaction_is_bounded_and_amortized_by_new_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        let mut history = History::open(path.clone(), 4).unwrap();
        history.append(b"abcd").unwrap();
        history.flush().unwrap();
        history.append(b"efgh").unwrap();
        history.flush().unwrap();
        history.append(b"i").unwrap();
        history.flush().unwrap();

        assert_eq!(history.snapshot(None), b"fghi");
        assert_eq!(history.data_bytes_written, 12);
        assert_eq!(fs::read(&path).unwrap(), b"fghi");
        for slot in 0..HISTORY_BANK_COUNT {
            let data_path = history_data_path(&path, slot);
            if let Ok(metadata) = fs::metadata(data_path) {
                assert!(
                    metadata.len() <= HISTORY_BANK_HEADER_BYTES as u64 + 2 * history.cap as u64
                );
            }
        }
        assert_eq!(History::open(path, 4).unwrap().snapshot(None), b"fghi");
    }

    #[test]
    fn history_legacy_migration_and_capacity_changes_keep_only_exact_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.bin");
        fs::write(&path, b"0123456789").unwrap();

        let mut migrated = History::open(path.clone(), 4).unwrap();
        assert_eq!(migrated.snapshot(None), b"6789");
        assert_eq!(fs::read(&path).unwrap(), b"6789");
        migrated.append(b"AB").unwrap();
        migrated.flush().unwrap();
        assert_eq!(read_persisted_history_tail(&path, None).unwrap(), b"89AB");
        assert_eq!(fs::read(&path).unwrap(), b"6789AB");
        migrated.flush_final().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"89AB");

        let shrunk = History::open(path.clone(), 3).unwrap();
        assert_eq!(shrunk.snapshot(None), b"9AB");
        let mut grown = History::open(path.clone(), 6).unwrap();
        assert_eq!(grown.snapshot(None), b"9AB");
        grown.append(b"CD").unwrap();
        grown.flush().unwrap();
        assert_eq!(History::open(path, 6).unwrap().snapshot(None), b"9ABCD");
    }

    #[test]
    fn history_special_files_fail_without_becoming_persistence_inputs() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::write(&target, b"unrelated").unwrap();
        let legacy = dir.path().join("history.bin");
        symlink(&target, &legacy).unwrap();
        assert!(History::open(legacy.clone(), 8).is_err());
        assert!(read_persisted_history_tail(&legacy, None).is_err());
        fs::remove_file(&legacy).unwrap();

        let commit = history_commit_path(&legacy, 0);
        let commit_c = CString::new(commit.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(commit_c.as_ptr(), 0o600) }, 0);
        assert!(History::open(legacy.clone(), 8).is_err());
        assert!(read_persisted_history_tail(&legacy, None).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unrelated");
    }

    #[test]
    fn history_capacity_has_one_global_limit_and_zero_stays_disabled() {
        for value in [0, 1, DEFAULT_HISTORY_BYTES, MAX_HISTORY_BYTES] {
            assert_eq!(validate_history_bytes(value).unwrap(), value);
        }
        for value in [MAX_HISTORY_BYTES + 1, usize::MAX] {
            let error = validate_history_bytes(value).unwrap_err().to_string();
            assert!(error.contains("history_bytes"), "{error}");
            assert!(error.contains(&MAX_HISTORY_BYTES.to_string()), "{error}");
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disabled-history.bin");
        let mut history = History::open(path.clone(), 0).unwrap();
        history.append(b"not retained").unwrap();
        history.flush().unwrap();
        assert!(history.snapshot(None).is_empty());
        assert!(!path.exists());
        assert!(read_persisted_history_tail(&path, None).unwrap().is_empty());
        assert!(History::open(dir.path().join("too-large"), MAX_HISTORY_BYTES + 1).is_err());
    }

    #[test]
    fn config_rejects_oversized_profile_history() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            runtime_root: root.path().join("runtime"),
            state_root: root.path().join("state"),
            config_file: root.path().join("config.toml"),
        };
        fs::write(
            &paths.config_file,
            format!(
                "version = 1\n[profiles.too_large]\nhistory_bytes = {}\n",
                MAX_HISTORY_BYTES + 1
            ),
        )
        .unwrap();

        let error = Config::load(&paths).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("profile \"too_large\" history_bytes"),
            "{message}"
        );
        assert!(
            message.contains(&MAX_HISTORY_BYTES.to_string()),
            "{message}"
        );
    }

    #[test]
    fn config_schema_rejects_unknown_fields_at_every_level() {
        for (label, text, unknown) in [
            (
                "root",
                "version = 1\ndefualt_engine = \"shell\"\n",
                "defualt_engine",
            ),
            (
                "engine",
                "version = 1\n[engines.custom]\ncommand = [\"true\"]\ncomand = [\"false\"]\n",
                "comand",
            ),
            (
                "profile",
                "version = 1\n[profiles.review]\nhistroy_bytes = 1024\n",
                "histroy_bytes",
            ),
            (
                "profile limits",
                "version = 1\n[profiles.review.limits]\nmemroy_bytes = 1024\n",
                "memroy_bytes",
            ),
            (
                "shortcut",
                "version = 1\n[shortcuts.review]\nengine = \"shell\"\nprofiel = \"review\"\n",
                "profiel",
            ),
        ] {
            let message = format!("{:#}", load_config_text(text).unwrap_err());
            assert!(message.contains("unknown field"), "{label}: {message}");
            assert!(message.contains(unknown), "{label}: {message}");
        }
    }

    #[test]
    fn config_semantics_reject_dangling_references_and_invalid_commands() {
        for (label, text, expected) in [
            (
                "default engine",
                "version = 1\ndefault_engine = \"missing\"\n",
                "default_engine \"missing\"",
            ),
            (
                "default profile",
                "version = 1\ndefault_profile = \"missing\"\n",
                "default_profile \"missing\"",
            ),
            (
                "profile engine",
                "version = 1\n[profiles.review]\nengine = \"missing\"\n",
                "profile \"review\" engine \"missing\"",
            ),
            (
                "shortcut engine",
                "version = 1\n[shortcuts.review]\nengine = \"missing\"\n",
                "shortcut \"review\" engine \"missing\"",
            ),
            (
                "shortcut profile",
                "version = 1\n[shortcuts.review]\nengine = \"shell\"\nprofile = \"missing\"\n",
                "shortcut \"review\" profile \"missing\"",
            ),
            (
                "shortcut profile engine",
                "version = 1\n[profiles.review]\nengine = \"claude\"\n[shortcuts.review]\nengine = \"codex\"\nprofile = \"review\"\n",
                "selects engine \"codex\", but profile \"review\" selects engine \"claude\"",
            ),
            (
                "empty engine command",
                "version = 1\n[engines.shell]\ncommand = []\n",
                "engine \"shell\" command must not be empty",
            ),
            (
                "empty profile command",
                "version = 1\n[profiles.review]\ncommand = []\n",
                "profile \"review\" command must not be empty",
            ),
            (
                "empty profile executable",
                "version = 1\n[profiles.review]\nexecutable = \"\"\n",
                "profile \"review\" executable must not be empty",
            ),
            (
                "ignored profile executable",
                "version = 1\n[profiles.review]\ncommand = [\"true\"]\nexecutable = \"false\"\n",
                "cannot set both command and executable",
            ),
            (
                "ignored profile args",
                "version = 1\n[profiles.review]\ncommand = [\"true\"]\nargs = [\"--ignored\"]\n",
                "cannot set both command and args",
            ),
        ] {
            let message = format!("{:#}", load_config_text(text).unwrap_err());
            assert!(message.contains(expected), "{label}: {message}");
        }
    }

    #[test]
    fn config_semantics_reject_invalid_numeric_limits() {
        for (label, field, expected) in [
            ("memory", "memory_bytes = 0", "memory_bytes must be greater"),
            ("pids", "pids = 0", "pids must be greater"),
            ("quota", "cpu_quota_us = 0", "cpu_quota_us must be greater"),
            (
                "period",
                "cpu_quota_us = 1\ncpu_period_us = 0",
                "cpu_period_us must be greater",
            ),
            (
                "orphan period",
                "cpu_period_us = 100000",
                "cpu_period_us requires cpu_quota_us",
            ),
        ] {
            let text = format!("version = 1\n[profiles.review.limits]\n{field}\n");
            let message = format!("{:#}", load_config_text(&text).unwrap_err());
            assert!(message.contains(expected), "{label}: {message}");
        }

        let config = load_config_text("version = 1\n").unwrap();
        let error = config
            .resolve(
                vec!["/bin/true".into()],
                Some("shell"),
                None,
                Path::new("/tmp"),
                None,
                &BTreeMap::new(),
                &Limits {
                    pids: Some(0),
                    ..Limits::default()
                },
                None,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("resolved launch limits pids"),
            "{error:#}"
        );
    }

    #[test]
    fn valid_config_references_commands_and_limits_still_load() {
        let config = load_config_text(
            "version = 1\n\
             default_engine = \"custom\"\n\
             default_profile = \"review\"\n\
             [engines.custom]\n\
             command = [\"/bin/sh\", \"-l\"]\n\
             env_unset = [\"CUSTOM_SECRET\"]\n\
             [profiles.review]\n\
             engine = \"custom\"\n\
             args = [\"--review\"]\n\
             history_bytes = 0\n\
             [profiles.review.limits]\n\
             memory_bytes = 1048576\n\
             pids = 4\n\
             cpu_quota_us = 50000\n\
             cpu_period_us = 100000\n\
             [shortcuts.rev]\n\
             engine = \"custom\"\n\
             profile = \"review\"\n",
        )
        .unwrap();

        assert_eq!(config.default_engine.as_deref(), Some("custom"));
        assert_eq!(config.default_profile.as_deref(), Some("review"));
        for (name, shortcut) in &config.shortcuts {
            assert!(config.engines.contains_key(&shortcut.engine), "{name}");
            if let Some(profile) = &shortcut.profile {
                assert!(config.profiles.contains_key(profile), "{name}");
            }
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_byte_size("2MiB").unwrap(), 2 * 1024 * 1024);
    }

    #[test]
    fn kill_grace_is_bounded_before_duration_or_deadline_math() {
        assert_eq!(
            kill_grace_duration(MAX_KILL_GRACE_MS).unwrap(),
            Duration::from_millis(MAX_KILL_GRACE_MS)
        );
        assert!(kill_grace_duration(MAX_KILL_GRACE_MS + 1).is_err());
        assert!(kill_grace_duration(u64::MAX).is_err());
    }

    #[test]
    fn cgroup_counter_read_errors_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let events = dir.path().join("cgroup.events");

        assert!(read_counter(&events, "populated").is_err());
        fs::write(&events, "frozen 0\n").unwrap();
        assert!(read_counter(&events, "populated").is_err());
        fs::write(&events, "populated nope\n").unwrap();
        assert!(read_counter(&events, "populated").is_err());
        fs::write(&events, "populated 1\n").unwrap();
        assert_eq!(read_counter(&events, "populated").unwrap(), 1);
    }

    #[test]
    fn recorded_cgroup_cleanup_checks_deadline_before_locator_io() {
        let id = Uuid::new_v4();
        let locator = PathBuf::from(format!("/sys/fs/cgroup/aplexer-workload-{id}.scope"));
        let error = cleanup_recorded_cgroup_until(
            id,
            &locator,
            None,
            libc::SIGKILL,
            Duration::ZERO,
            Instant::now(),
        )
        .expect_err("expired cleanup must stop before locator inspection");
        assert!(error.to_string().contains("timed out validating"));
    }

    #[test]
    fn recorded_cgroup_cleanup_rejects_untrusted_locator() {
        let id = Uuid::new_v4();
        let identity = current_cgroup_identity().unwrap();
        let error = cleanup_recorded_cgroup_until(
            id,
            Path::new("/tmp/not-a-cgroup"),
            Some(&identity),
            libc::SIGKILL,
            Duration::ZERO,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("untrusted locator must fail closed");
        assert!(error
            .to_string()
            .contains("untrusted recorded cgroup locator"));
    }

    #[test]
    fn cgroup_identity_captures_current_v2_kernel_domain() {
        let identity = current_cgroup_identity().unwrap();
        assert_eq!(identity.boot_id, linux_boot_id().unwrap());
        assert_ne!(identity.cgroup_namespace_inode, 0);
        assert_ne!(identity.mount_namespace_inode, 0);
        assert_ne!(identity.cgroup_mount_id, 0);
        assert_ne!(identity.cgroup_root_inode, 0);
        ensure_cgroup2_filesystem(Path::new(CGROUP_V2_ROOT)).unwrap();
    }

    #[test]
    fn live_cgroup_disappearance_is_empty_only_in_matching_domain() {
        let identity = current_cgroup_identity().unwrap();
        let missing_path =
            Path::new(CGROUP_V2_ROOT).join(format!("aplexer-workload-{}.scope", Uuid::new_v4()));
        assert!(!missing_path.exists());
        let collected = Cgroup {
            path: missing_path,
            identity: identity.clone(),
            anchor: Arc::new(Mutex::new(None)),
            initial_oom_kill: 0,
        };
        assert!(!collected.populated().unwrap());

        assert!(!live_cgroup_populated_with(&identity, || {
            Err(io::Error::from(io::ErrorKind::NotFound).into())
        })
        .unwrap());

        let mut wrong_mount = identity.clone();
        wrong_mount.cgroup_mount_id ^= 1;
        let mismatch = live_cgroup_populated_with(&wrong_mount, || {
            panic!("membership must not be read in a mismatched kernel domain")
        })
        .expect_err("mismatched identity must fail closed");
        assert!(mismatch.to_string().contains("before reading membership"));

        let malformed =
            live_cgroup_populated_with(&identity, || Err(anyhow!("malformed cgroup.events")))
                .expect_err("non-ENOENT membership errors must fail closed");
        assert!(malformed
            .to_string()
            .contains("read live cgroup membership"));
    }

    #[test]
    fn control_group_locator_is_uuid_bound_and_cannot_escape_root() {
        let id = Uuid::new_v4();
        let valid = format!("/user.slice/user-1000.slice/aplexer-workload-{id}.scope");
        assert_eq!(
            control_group_locator(id, &valid).unwrap(),
            Path::new(CGROUP_V2_ROOT).join(valid.trim_start_matches('/'))
        );
        assert!(
            control_group_locator(id, &format!("/user.slice/../aplexer-workload-{id}.scope"))
                .is_err()
        );
        assert!(control_group_locator(id, "relative.scope").is_err());
        assert!(control_group_locator(
            id,
            &format!("/user.slice/aplexer-workload-{}.scope", Uuid::new_v4())
        )
        .is_err());
    }

    #[test]
    fn scope_wait_retries_empty_control_group_then_accepts_valid_path() {
        let dir = tempfile::tempdir().unwrap();
        let systemctl = dir.path().join("systemctl");
        let id = Uuid::new_v4();
        let unit = format!("aplexer-workload-{id}");
        let reported = format!("/user.slice/{unit}.scope");
        fs::write(
            &systemctl,
            format!(
                "#!/bin/sh\nif [ ! -e \"$0.seen\" ]; then : > \"$0.seen\"; printf '\\n'; else printf '%s\\n' '{}'; fi\n",
                reported
            ),
        )
        .unwrap();
        fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o755)).unwrap();

        let mut validations = 0;
        let path = wait_for_scope_cgroup_with(
            id,
            &unit,
            &systemctl,
            "--user",
            Duration::from_secs(1),
            |path| {
                validations += 1;
                Ok(Some(path.to_path_buf()))
            },
        )
        .unwrap();

        assert_eq!(
            path,
            Path::new(CGROUP_V2_ROOT).join(reported.trim_start_matches('/'))
        );
        assert_eq!(validations, 1, "empty value must not reach validation");
        assert!(systemctl.with_extension("seen").exists());
    }

    #[test]
    fn system_helpers_resolve_without_ambient_path() {
        for helper in ["systemd-run", "systemctl", "sleep"] {
            let path = trusted_system_helper(helper).unwrap();
            assert!(path.is_absolute());
            assert_eq!(fs::metadata(path).unwrap().uid(), 0);
        }

        let dir = tempfile::tempdir().unwrap();
        let shadow = dir.path().join("systemctl");
        fs::write(&shadow, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&shadow, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_trusted_helper(&shadow).is_err());
    }

    #[test]
    fn missing_cgroup_leaf_requires_matching_persisted_identity() {
        let id = Uuid::new_v4();
        let locator = PathBuf::from(format!("{CGROUP_V2_ROOT}/aplexer-workload-{id}.scope"));
        let missing = validate_recorded_cgroup(id, &locator, None)
            .expect_err("legacy locator must not prove emptiness");
        assert!(missing
            .to_string()
            .contains("no boot/namespace/mount identity"));

        let mut wrong_boot = current_cgroup_identity().unwrap();
        wrong_boot.boot_id = Uuid::new_v4().to_string();
        let mismatch = validate_recorded_cgroup(id, &locator, Some(&wrong_boot))
            .expect_err("cross-boot locator must not prove emptiness");
        assert!(mismatch.to_string().contains("does not match"));

        let mut wrong_mount = current_cgroup_identity().unwrap();
        wrong_mount.cgroup_mount_id = wrong_mount.cgroup_mount_id.saturating_add(1);
        let mismatch = validate_recorded_cgroup(id, &locator, Some(&wrong_mount))
            .expect_err("replacement mount must not prove emptiness");
        assert!(mismatch.to_string().contains("does not match"));

        let identity = current_cgroup_identity().unwrap();
        assert_eq!(
            validate_recorded_cgroup(id, &locator, Some(&identity)).unwrap(),
            None,
            "same-domain missing cgroup is empty"
        );
    }

    #[test]
    fn cgroup_recovery_pidfds_preserve_descriptor_reserve() {
        assert_eq!(
            cgroup_recovery_pidfd_capacity_from_counts(CGROUP_RECOVERY_FD_RESERVE, 0),
            0
        );
        assert_eq!(
            cgroup_recovery_pidfd_capacity_from_counts(CGROUP_RECOVERY_FD_RESERVE + 7, 3),
            4
        );
        assert_eq!(
            cgroup_recovery_pidfd_capacity_from_counts(u64::MAX, 0),
            MAX_CGROUP_RECOVERY_MEMBERS
        );
    }

    #[test]
    fn cgroup_member_fallback_uses_identity_pinned_signal() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("cgroup.procs"),
            format!("{}\n", std::process::id()),
        )
        .unwrap();
        signal_cgroup_path_until(dir.path(), 0, Instant::now() + Duration::from_secs(1))
            .expect("pidfd signal-zero probe");
    }

    #[test]
    fn cgroup_setup_helper_obeys_wall_clock_deadline() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let started = Instant::now();
        let error = command_output_until(
            &mut command,
            Instant::now() + Duration::from_millis(50),
            "exercise setup timeout",
        )
        .expect_err("wedged setup helper must time out");
        assert!(error.to_string().contains("timed out waiting"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cgroup_setup_helper_collects_bounded_output() {
        let mut command = Command::new("/bin/printf");
        command.arg("/user.slice/example.scope\n");
        let output = command_output_until(
            &mut command,
            Instant::now() + Duration::from_secs(1),
            "exercise setup output",
        )
        .expect("short-lived setup helper");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"/user.slice/example.scope\n");
    }

    #[test]
    fn cgroup_setup_helper_pipe_cannot_outlive_deadline() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 0.2 & exit 0"]);
        let started = Instant::now();
        let error = command_output_until(
            &mut command,
            Instant::now() + Duration::from_millis(50),
            "exercise inherited output pipe",
        )
        .expect_err("inherited helper pipe must not defeat deadline");
        assert!(error.to_string().contains("timed out waiting"));
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    /// `kill(pid, 0)` succeeds for a zombie, so the raw signalability test
    /// reported exited-but-unreaped processes as alive. Under a worker's
    /// child subreaper that is not a corner case: a session started inside
    /// another session reparents onto the outer worker, and until it is
    /// reaped every liveness answer about it -- `worker_alive`,
    /// `workload_leader_alive`, and therefore `reap_verdict` and `a prune` --
    /// was wrong in the direction of "still running, keep it".
    #[test]
    fn process_alive_reports_an_unreaped_zombie_as_dead() {
        let mut child = Command::new("/bin/true").spawn().unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process_is_zombie(pid) {
            assert!(
                Instant::now() < deadline,
                "child {pid} never became a zombie"
            );
            thread::sleep(Duration::from_millis(5));
        }

        assert_eq!(
            process_state(pid).unwrap(),
            'Z',
            "the test needs a real unreaped zombie"
        );
        assert_eq!(
            unsafe { libc::kill(pid as libc::pid_t, 0) },
            0,
            "a zombie is still signalable, which is exactly the trap"
        );
        assert!(
            !process_alive(pid),
            "zombie {pid} must not be reported alive"
        );

        child.wait().unwrap();
        assert!(!process_alive(pid));
        assert!(
            !process_is_zombie(pid),
            "a reaped pid has no state to read, so it is not a zombie either"
        );
    }

    /// A `Z` in `/proc/<pid>/stat` is not by itself proof that a process is
    /// finished: a thread group leader that exited while its siblings kept
    /// running reads exactly the same (verified against a real process --
    /// `state=Z` with two entries under `/proc/<pid>/task`). Treating that
    /// as dead would let a multi-threaded workload be declared contained
    /// while it was still executing, so the thread group must be down to the
    /// leader's corpse alone.
    #[test]
    fn zombie_detection_requires_an_empty_thread_group() {
        let root = tempfile::tempdir().unwrap();
        let write_process = |pid: u32, state: char, threads: &[u32]| {
            let dir = root.path().join(pid.to_string());
            fs::create_dir_all(&dir).unwrap();
            // A comm containing spaces and a ')' is legal and must not shift
            // the state field.
            fs::write(
                dir.join("stat"),
                format!("{pid} (od d) ba) {state} 1 {pid} 0 -1 4194304 0 0\n"),
            )
            .unwrap();
            for tid in threads {
                fs::create_dir_all(dir.join("task").join(tid.to_string())).unwrap();
            }
        };

        write_process(11, 'Z', &[11]);
        write_process(12, 'Z', &[12, 13]);
        write_process(14, 'S', &[14]);
        write_process(15, 'R', &[15, 16]);

        assert_eq!(process_state_in(root.path(), 11).unwrap(), 'Z');
        assert_eq!(process_state_in(root.path(), 12).unwrap(), 'Z');

        assert!(
            process_is_zombie_in(root.path(), 11),
            "a Z leader alone in its thread group is a reapable zombie"
        );
        assert!(
            !process_is_zombie_in(root.path(), 12),
            "a Z leader with a live sibling thread is still running code"
        );
        assert!(!process_is_zombie_in(root.path(), 14));
        assert!(!process_is_zombie_in(root.path(), 15));
        assert!(
            !process_is_zombie_in(root.path(), 99),
            "an unreadable process must not be subtracted from liveness"
        );
    }

    /// A live process must never be mistaken for a zombie by the state read.
    #[test]
    fn process_alive_still_reports_a_running_child_as_alive() {
        let mut child = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        assert!(process_alive(pid));
        assert!(!process_is_zombie(pid));
        assert!(matches!(process_state(pid).unwrap(), 'R' | 'S' | 'D'));
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn cgroup_anchor_release_owns_child_through_kill_and_reap() {
        let anchor = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let anchor_pid = anchor.id();
        let cgroup = Cgroup {
            path: PathBuf::from("/does/not/exist"),
            identity: current_cgroup_identity().unwrap(),
            anchor: Arc::new(Mutex::new(Some(anchor))),
            initial_oom_kill: 0,
        };
        let clone = cgroup.clone();

        cgroup.release_anchor().unwrap();
        assert!(cgroup.anchor.lock().unwrap().is_none());
        clone.release_anchor().unwrap();

        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(anchor_pid as libc::pid_t, &mut status, libc::WNOHANG) },
            -1,
            "anchor must already be reaped exactly once"
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[test]
    fn cgroup_anchor_release_retains_handle_when_reaping_fails() {
        let mut slot = Some(7_u8);
        let error = release_anchor_slot(&mut slot, |_| bail!("injected release failure"))
            .expect_err("release must fail");
        assert!(error.to_string().contains("injected release failure"));
        assert_eq!(slot, Some(7), "failed release must preserve ownership");
    }

    #[test]
    fn legacy_exit_info_remains_a_containment_proof() {
        let state = tempfile::tempdir().unwrap();
        let mut value = serde_json::to_value(liveness_record(state.path())).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("containment_cgroup");
        object.remove("containment_cgroup_identity");
        object.remove("containment_empty");
        object.insert("phase".into(), serde_json::json!("exited"));
        object.insert(
            "exit".into(),
            serde_json::json!({
                "code": 0,
                "signal": null,
                "oom_killed": false,
                "exited_at_ms": 2
            }),
        );
        let terminal: SessionRecord = serde_json::from_value(value.clone()).unwrap();
        assert!(terminal.containment_proven_empty());

        value
            .as_object_mut()
            .unwrap()
            .insert("containment_empty".into(), serde_json::json!(false));
        let explicit_failure: SessionRecord = serde_json::from_value(value.clone()).unwrap();
        assert!(!explicit_failure.containment_proven_empty());

        value.as_object_mut().unwrap().remove("exit");
        value
            .as_object_mut()
            .unwrap()
            .insert("phase".into(), serde_json::json!("failed"));
        let ambiguous: SessionRecord = serde_json::from_value(value).unwrap();
        assert!(!ambiguous.containment_proven_empty());
    }

    #[test]
    fn session_metadata_keeps_only_transcript_roots() {
        let env = BTreeMap::from([
            ("CODEX_HOME".to_string(), "/profiles/codex".to_string()),
            ("API_TOKEN".to_string(), "secret".to_string()),
        ]);
        assert_eq!(
            session_metadata_env(&env),
            BTreeMap::from([("CODEX_HOME".to_string(), "/profiles/codex".to_string())])
        );
    }
    /// The load-bearing property from pocketshell-integration-plan.md 0.2: a
    /// custom engine's own (smaller/different) `env_unset` can only ADD to
    /// the forced provider-key union, never replace or shrink it.
    #[test]
    fn env_unset_union_is_forced() {
        let mut config = Config {
            default_engine: Some("custom".into()),
            ..Config::default()
        };
        config.engines.insert(
            "custom".into(),
            EngineConfig {
                command: vec!["true".into()],
                env: BTreeMap::new(),
                // deliberately includes a name already in the forced list
                // (to exercise dedup) plus one new name.
                env_unset: vec!["ANTHROPIC_API_KEY".into(), "MY_CUSTOM_VAR".into()],
                skip_permissions_argv: Vec::new(),
            },
        );
        let launch = config
            .resolve(
                Vec::new(),
                None,
                None,
                Path::new("/tmp"),
                None,
                &BTreeMap::new(),
                &Limits::default(),
                None,
            )
            .unwrap();
        for name in PROVIDER_ENV_UNSET_VARS {
            assert!(
                launch.env_unset.iter().any(|v| v == name),
                "forced provider var {name} missing from env_unset"
            );
        }
        assert!(launch.env_unset.iter().any(|v| v == "MY_CUSTOM_VAR"));
        let count = launch
            .env_unset
            .iter()
            .filter(|v| v.as_str() == "ANTHROPIC_API_KEY")
            .count();
        assert_eq!(count, 1, "ANTHROPIC_API_KEY must not be duplicated");
        assert_eq!(
            launch.env_unset.len(),
            PROVIDER_ENV_UNSET_VARS.len() + 1,
            "union must be exactly the forced list plus the one new custom name"
        );
    }

    #[test]
    fn shell_env_unset_preserves_provider_overrides_and_configured_removals() {
        let config = load_config_text(
            "version = 1\n\
             [engines.shell]\n\
             command = [\"/bin/sh\", \"-l\"]\n\
             env_unset = [\"SHELL_SECRET\", \"SHELL_SECRET\"]\n",
        )
        .unwrap();
        let overrides = BTreeMap::from([
            ("OPENAI_API_KEY".into(), "literal-shell-value".into()),
            ("SHELL_SECRET".into(), "remove-me".into()),
        ]);
        let launch = config
            .resolve(
                vec!["/bin/true".into()],
                Some("shell"),
                None,
                Path::new("/tmp"),
                None,
                &overrides,
                &Limits::default(),
                None,
            )
            .unwrap();

        assert_eq!(
            launch.env.get("OPENAI_API_KEY").map(String::as_str),
            Some("literal-shell-value")
        );
        assert_eq!(launch.env_unset, vec!["SHELL_SECRET"]);
        assert!(!launch.env_unset.iter().any(|name| name == "OPENAI_API_KEY"));

        let agent = config
            .resolve(
                vec!["/bin/true".into()],
                Some("codex"),
                None,
                Path::new("/tmp"),
                None,
                &overrides,
                &Limits::default(),
                None,
            )
            .unwrap();
        assert!(agent.env_unset.iter().any(|name| name == "OPENAI_API_KEY"));
    }

    #[test]
    fn skip_permissions_argv_ported_values() {
        let config = Config {
            engines: BTreeMap::from([(
                "claude".to_string(),
                EngineConfig {
                    command: vec!["claude".into()],
                    env: BTreeMap::new(),
                    env_unset: Vec::new(),
                    skip_permissions_argv: vec!["--dangerously-skip-permissions".into()],
                },
            )]),
            ..Config::default()
        };
        let launch = config
            .resolve(
                Vec::new(),
                Some("claude"),
                None,
                Path::new("/tmp"),
                None,
                &BTreeMap::new(),
                &Limits::default(),
                None,
            )
            .unwrap();
        assert_eq!(
            launch.skip_permissions_argv,
            vec!["--dangerously-skip-permissions".to_string()]
        );
    }

    #[test]
    fn executable_available_requires_execute_permission() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("tool");
        fs::write(&program, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o600)).unwrap();

        assert!(!executable_available(program.to_str().unwrap()));

        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(executable_available(program.to_str().unwrap()));
    }

    #[test]
    fn explicit_relative_path_overrides_are_resolved_once() {
        let resolved = absolute_override_path(PathBuf::from("state"), "APLEXER_STATE_DIR").unwrap();
        assert!(resolved.is_absolute());
        assert_eq!(resolved, env::current_dir().unwrap().join("state"));
    }

    #[test]
    fn xdg_paths_must_be_absolute() {
        let error = absolute_xdg_path(PathBuf::from("runtime"), "XDG_RUNTIME_DIR").unwrap_err();
        assert!(error.to_string().contains("must be an absolute path"));
        assert_eq!(
            absolute_xdg_path(PathBuf::from("/run/user/1000"), "XDG_RUNTIME_DIR").unwrap(),
            PathBuf::from("/run/user/1000")
        );
    }
}
