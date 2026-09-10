//! Durable history persistence: capacity limits, the in-memory ring with
//! amortized compaction, crash-safe two-bank commits with a published
//! marker guarding the raw tail, legacy-format recovery, and read-side
//! tail reconstruction.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

use crate::{atomic_write_bytes, atomic_write_json, MAX_FRAME_BYTES};

pub const DEFAULT_HISTORY_BYTES: usize = 4 * 1024 * 1024;
/// Global per-session raw-history ceiling. The ring is resident in every
/// worker and periodically copied for atomic persistence, while protocol and
/// post-mortem capture can expose at most one frame, so retaining more than a
/// maximum-sized frame adds memory/write amplification without a usable read
/// path.
pub const MAX_HISTORY_BYTES: usize = MAX_FRAME_BYTES;
pub fn validate_history_bytes(value: usize) -> Result<usize> {
    if value > MAX_HISTORY_BYTES {
        bail!("history_bytes {value} exceeds the maximum of {MAX_HISTORY_BYTES} bytes (16 MiB)");
    }
    Ok(value)
}

/// How stale the persisted history file may get behind the in-memory ring.
/// Live reads (capture/attach) are always served from memory; the file only
/// matters after the worker is gone, so a worker crash loses at most this
/// much of the tail.
pub const HISTORY_FLUSH_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) const HISTORY_FORMAT_VERSION: u32 = 2;
pub(crate) const HISTORY_BANK_MAGIC: &[u8; 8] = b"APLXH2D\0";
pub(crate) const HISTORY_BANK_HEADER_PREFIX_BYTES: usize = 72;
pub(crate) const HISTORY_BANK_HEADER_BYTES: usize = HISTORY_BANK_HEADER_PREFIX_BYTES + 32;
pub(crate) const HISTORY_COMMIT_MAX_BYTES: usize = 4096;
pub(crate) const HISTORY_MARKER_MAX_BYTES: usize = 4096;
pub(crate) const HISTORY_BANK_COUNT: u8 = 2;
pub(crate) const HISTORY_COMMIT_COUNT: u8 = 2;
const HISTORY_HASH_CHUNK_BYTES: usize = 64 * 1024;

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

pub(crate) fn history_sidecar_path(path: &Path, kind: &str, slot: u8) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("history.bin"))
        .to_os_string();
    name.push(format!(".v2.{kind}.{slot}"));
    path.with_file_name(name)
}

pub(crate) fn history_data_path(path: &Path, slot: u8) -> PathBuf {
    history_sidecar_path(path, "data", slot)
}

pub(crate) fn history_commit_path(path: &Path, slot: u8) -> PathBuf {
    history_sidecar_path(path, "commit", slot)
}

pub(crate) fn history_marker_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("history.bin"))
        .to_os_string();
    name.push(".v2.marker");
    path.with_file_name(name)
}

pub(crate) fn history_session_id(path: &Path) -> Uuid {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse().ok())
        .unwrap_or_else(Uuid::nil)
}

pub(crate) fn validate_optional_history_node(path: &Path, label: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => bail!("{label} {} is not a regular file", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {label} {}", path.display())),
    }
}

pub(crate) fn open_optional_history_file(
    path: &Path,
    label: &str,
    write: bool,
) -> Result<Option<File>> {
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

pub(crate) fn validate_history_artifacts(path: &Path) -> Result<()> {
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
pub(crate) struct HistoryMarker {
    pub(crate) format_version: u32,
    pub(crate) store_id: Uuid,
    pub(crate) session_id: Uuid,
    pub(crate) metadata_sha256: String,
}

impl HistoryMarker {
    pub(crate) fn seal(mut self) -> Result<Self> {
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
pub(crate) struct HistoryCommit {
    pub(crate) format_version: u32,
    pub(crate) store_id: Uuid,
    pub(crate) session_id: Uuid,
    pub(crate) commit_generation: u64,
    pub(crate) bank_generation: u64,
    pub(crate) data_slot: u8,
    pub(crate) capacity: u64,
    pub(crate) committed_len: u64,
    pub(crate) stream_end: u64,
    pub(crate) data_sha256: String,
    pub(crate) metadata_sha256: String,
}

impl HistoryCommit {
    pub(crate) fn seal(mut self) -> Result<Self> {
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
pub(crate) struct HistoryBankHeader {
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

pub(crate) struct RecoveredHistory {
    commit: HistoryCommit,
    tail: Vec<u8>,
    data_hasher: Sha256,
}

pub(crate) fn read_history_commit(path: &Path, slot: u8) -> Result<Option<HistoryCommit>> {
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

pub(crate) fn read_history_marker(path: &Path) -> Result<Option<HistoryMarker>> {
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

pub(crate) fn publish_history_marker(path: &Path, commit: &HistoryCommit) -> Result<()> {
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

pub(crate) fn recover_history_candidate(
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
    // Hash the committed payload in chunks and keep only the tail the
    // caller can use: this runs for both commit slots on every open and
    // every read of a persisted tail, and a bank can be 32 MiB.
    let count = tail_limit.min(commit.capacity as usize).min(committed_len);
    let mut hasher = Sha256::new();
    let mut tail: VecDeque<u8> = VecDeque::with_capacity(count);
    let mut chunk = vec![0; HISTORY_HASH_CHUNK_BYTES.min(committed_len.max(1))];
    let mut remaining = committed_len;
    while remaining > 0 {
        let want = chunk.len().min(remaining);
        let read = file
            .read(&mut chunk[..want])
            .context("read committed history payload")?;
        if read == 0 {
            bail!("history data bank ended inside its committed prefix");
        }
        let bytes = &chunk[..read];
        hasher.update(bytes);
        if bytes.len() >= count {
            tail.clear();
            tail.extend(&bytes[bytes.len() - count..]);
        } else {
            tail.extend(bytes);
            let excess = tail.len().saturating_sub(count);
            tail.drain(..excess);
        }
        remaining -= read;
    }
    if format!("{:x}", hasher.clone().finalize()) != commit.data_sha256 {
        bail!("history data checksum mismatch");
    }
    Ok(RecoveredHistory {
        commit,
        tail: tail.into(),
        data_hasher: hasher,
    })
}

pub(crate) fn recover_v2_history(
    path: &Path,
    tail_limit: usize,
) -> Result<(Option<RecoveredHistory>, bool)> {
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

pub(crate) struct LegacyHistory {
    tail: Vec<u8>,
    total_len: u64,
    present: bool,
}

pub(crate) fn read_legacy_history_tail(path: &Path, limit: usize) -> Result<LegacyHistory> {
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
    pub(crate) path: PathBuf,
    pub(crate) cap: usize,
    pub(crate) bytes: VecDeque<u8>,
    pub(crate) pending: VecDeque<u8>,
    pub(crate) dirty: bool,
    pub(crate) observed_end: u64,
    pub(crate) durable_end: u64,
    pub(crate) compatibility_end: u64,
    pub(crate) compatibility_len: u64,
    pub(crate) compatibility_known: bool,
    pub(crate) persisted: Option<RecoveredHistory>,
    #[cfg(test)]
    pub(crate) data_bytes_written: u64,
    #[cfg(test)]
    pub(crate) append_failure: Option<i32>,
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
