use super::*;

pub(crate) struct RecoveredHistory {
    pub(crate) commit: HistoryCommit,
    pub(crate) tail: Vec<u8>,
    pub(crate) data_hasher: Sha256,
}

pub(crate) fn read_history_commit(path: &Path, slot: u8) -> Result<Option<HistoryCommit>> {
    let commit_path = history_commit_path(path, slot);
    let Some(file) = open_optional_history_file(&commit_path, "history commit", false)? else {
        return Ok(None);
    };
    let commit: HistoryCommit = read_bounded_json(
        file,
        &commit_path,
        "history commit",
        HISTORY_COMMIT_MAX_BYTES,
    )?;
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
    let marker: HistoryMarker = read_bounded_json(
        file,
        &marker_path,
        "history marker",
        HISTORY_MARKER_MAX_BYTES,
    )?;
    marker
        .validate(path)
        .with_context(|| format!("validate history marker {}", marker_path.display()))?;
    Ok(Some(marker))
}

/// Publishes the v2 presence marker for `commit`'s store unless one is
/// already there. Returns true once the marker is known to be in place.
pub(crate) fn publish_history_marker(path: &Path, commit: &HistoryCommit) -> Result<bool> {
    if let Some(marker) = read_history_marker(path)? {
        if marker.store_id != commit.store_id || marker.session_id != commit.session_id {
            bail!("history marker does not match the committed history store");
        }
        return Ok(true);
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
        .with_context(|| format!("publish history marker {}", marker_path.display()))?;
    Ok(true)
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
    if hex_encode(&hasher.clone().finalize()) != commit.data_sha256 {
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
    pub(crate) tail: Vec<u8>,
    pub(crate) total_len: u64,
    pub(crate) present: bool,
}

impl LegacyHistory {
    pub(crate) fn absent() -> Self {
        Self {
            tail: Vec::new(),
            total_len: 0,
            present: false,
        }
    }
}

pub(crate) fn read_legacy_history_tail(path: &Path, limit: usize) -> Result<LegacyHistory> {
    let Some(mut file) = open_optional_history_file(path, "legacy history", false)? else {
        return Ok(LegacyHistory::absent());
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
