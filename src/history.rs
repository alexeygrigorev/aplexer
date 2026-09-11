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

use crate::persist::read_bounded_json;
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

mod layout;
mod records;
mod recovery;

pub(crate) use layout::*;
pub(crate) use records::*;
pub use recovery::read_persisted_history_tail;
pub(crate) use recovery::*;

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
    /// The v2 presence marker is written once per store and never changes,
    /// so after this worker has seen or published it there is nothing to
    /// re-read and re-validate on every 500 ms flush.
    pub(crate) marker_published: bool,
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
        // The v2 store is authoritative when present; only without it does
        // the raw legacy file supply the tail and the stream position.
        let legacy = match &recovered {
            Some(_) => LegacyHistory::absent(),
            None => read_legacy_history_tail(&path, cap)?,
        };
        let (tail, observed_end) = match &recovered {
            Some(recovered) => (&recovered.tail, recovered.commit.stream_end),
            None => (&legacy.tail, legacy.total_len),
        };
        let bytes = tail.iter().copied().collect();
        let mut history = Self {
            path,
            cap,
            bytes,
            pending: VecDeque::new(),
            dirty: false,
            observed_end,
            durable_end: observed_end,
            compatibility_end: if legacy.present { observed_end } else { 0 },
            compatibility_len: legacy.total_len,
            compatibility_known: !had_v2,
            persisted: recovered,
            marker_published: marker_present,
            #[cfg(test)]
            data_bytes_written: 0,
            #[cfg(test)]
            append_failure: None,
        };
        if let Some(persisted) = &history.persisted {
            if !marker_present {
                history.marker_published =
                    publish_history_marker(&history.path, &persisted.commit)?;
            }
        }
        let needs_capacity_migration = history
            .persisted
            .as_ref()
            .is_some_and(|persisted| persisted.commit.capacity != cap as u64);
        if legacy.present && legacy.total_len > cap as u64 {
            history.repair_legacy_compatibility()?;
        }
        if legacy.present || needs_capacity_migration {
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

    fn ensure_marker_published(&mut self, commit: &HistoryCommit) -> Result<()> {
        if !self.marker_published {
            self.marker_published = publish_history_marker(&self.path, commit)?;
        }
        Ok(())
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
            data_sha256: hex_encode(&hasher.clone().finalize()),
            metadata_sha256: String::new(),
        };
        let commit = self.publish_commit(commit)?;
        self.ensure_marker_published(&commit)?;
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
            data_sha256: hex_encode(&hasher.clone().finalize()),
            metadata_sha256: String::new(),
        };
        let commit = self.publish_commit(commit)?;
        self.ensure_marker_published(&commit)?;
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
