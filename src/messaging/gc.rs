//! TTL/quota pruning of messages and cursor state, explicit and
//! opportunistic.

use super::*;

#[derive(Debug, Serialize)]
pub struct GcReport {
    pub removed: usize,
    pub remaining: usize,
}

struct MailboxEntry {
    path: PathBuf,
    created_at: Option<u64>,
    size: u64,
}

fn mailbox_entries(
    mp: &MessagePaths,
    expected_workspace: &Path,
    read_created_at: bool,
) -> Result<Vec<MailboxEntry>> {
    let mut entries = Vec::new();
    for path in entries_with_extension(&mp.msgs_dir, &["json"])? {
        // Even when append quota enforcement needs only the size, use the
        // same no-follow/non-regular/oversize preflight as envelope reads.
        // Explicit TTL GC additionally parses and validates the envelope from
        // this exact open descriptor, avoiding a second path lookup.
        let (file, size) = open_message_file(&path)?;
        let created_at = if read_created_at {
            Some(load_open_message_file(file, &path, expected_workspace)?.created_at)
        } else {
            None
        };
        entries.push(MailboxEntry {
            path,
            created_at,
            size,
        });
    }
    // Message paths are UUIDv7 filenames, so the listing's bytewise path
    // order is chronological mailbox order. Malformed names still get a
    // stable eviction order rather than escaping the cap.
    Ok(entries)
}

/// Unlinks one mailbox message; false when it was already gone.
fn remove_mailbox_entry(entry: &MailboxEntry) -> Result<bool> {
    match fs::remove_file(&entry.path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("remove mailbox message {}", entry.path.display()))
        }
    }
}

/// Applies TTL and hard caps while the caller holds `MAILBOX_LOCK_FILE`.
/// `protected` is the just-appended file: a successful send must not evict
/// itself merely because its UUID sorts before an older committed writer.
pub(crate) fn prune_workspace_locked(
    mp: &MessagePaths,
    expected_workspace: &Path,
    protected: Option<&Path>,
    max_messages: usize,
    max_bytes: u64,
    expire: bool,
) -> Result<GcReport> {
    let is_protected = |entry: &MailboxEntry| protected.is_some_and(|path| entry.path == path);
    let mut entries: VecDeque<MailboxEntry> =
        mailbox_entries(mp, expected_workspace, expire)?.into();
    let mut total: u64 = entries.iter().map(|entry| entry.size).sum();
    let mut removed = 0usize;
    let mut directory_changed = false;
    let mut evict = |entry: &MailboxEntry, total: &mut u64| -> Result<()> {
        let deleted = remove_mailbox_entry(entry)?;
        *total = total.saturating_sub(entry.size);
        removed += usize::from(deleted);
        directory_changed |= deleted;
        Ok(())
    };

    if expire {
        let now = now_secs();
        let mut kept = VecDeque::with_capacity(entries.len());
        for entry in entries {
            let expired = entry
                .created_at
                .is_some_and(|created_at| now.saturating_sub(created_at) > DEFAULT_TTL_SECS);
            if expired && !is_protected(&entry) {
                evict(&entry, &mut total)?;
            } else {
                kept.push_back(entry);
            }
        }
        entries = kept;
    }

    // Oldest first: entries are in id order, so the victim is the front
    // unless that is the protected message, in which case the one behind it.
    while entries.len() > max_messages || total > max_bytes {
        let Some(index) = entries.iter().position(|entry| !is_protected(entry)) else {
            bail!(
                "mailbox quota cannot retain the protected message ({} messages, {total} bytes)",
                entries.len()
            );
        };
        let entry = entries.remove(index).expect("index came from this deque");
        evict(&entry, &mut total)?;
    }

    if directory_changed {
        fs::File::open(&mp.msgs_dir)?.sync_all()?;
    }
    // Exact acknowledgements for removed messages are harmless, and are
    // compacted on the consumer's next read/ack. Keep append quota
    // enforcement independent from cursor-file health: otherwise a corrupt
    // unrelated cursor could make a send fail after old messages were
    // already pruned. Explicit GC performs the eager cursor sweep after this
    // message pass succeeds.
    Ok(GcReport {
        removed,
        remaining: entries.len(),
    })
}

pub(crate) fn cursor_entry_ids(cursors_dir: &Path) -> Result<BTreeSet<Uuid>> {
    Ok(entries_with_extension(cursors_dir, &["json", "lock"])?
        .iter()
        .filter_map(|path| uuid_stem(path))
        .collect())
}

fn modified_secs(path: &Path) -> Result<Option<u64>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    let modified = metadata
        .modified()
        .with_context(|| format!("read mtime for {}", path.display()))?;
    Ok(Some(
        modified
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    ))
}

fn stale_at(path: &Path, now: u64, retention_secs: u64) -> Result<bool> {
    Ok(modified_secs(path)?.is_some_and(|modified| now.saturating_sub(modified) > retention_secs))
}

fn is_lock_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|io_error| io_error.kind() == io::ErrorKind::WouldBlock)
    })
}

pub(crate) fn try_cursor_lock(path: &Path) -> Result<Option<FileLock>> {
    match FileLock::exclusive(path, true) {
        Ok(lock) => Ok(Some(lock)),
        Err(error) if is_lock_busy(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Compacts live cursor contents and removes inactive cursor state older than
/// `retention_secs`. The caller holds the workspace mailbox lock, which is
/// first in every current read/ack lock order. A nonblocking per-consumer lock
/// plus a post-lock mtime check also protects against older/external clients
/// that may hold only the cursor lock.
pub(crate) fn maintain_workspace_cursors_locked(
    mp: &MessagePaths,
    active_consumers: &BTreeSet<Uuid>,
    now: u64,
    retention_secs: u64,
) -> Result<()> {
    let retained_ids = retained_message_ids(&mp.msgs_dir)?;
    let mut directory_changed = false;
    for consumer_id in cursor_entry_ids(&mp.cursors_dir)? {
        let cursor_path = mp.cursors_dir.join(format!("{consumer_id}.json"));
        let lock_path = cursor_lock_path(&mp.cursors_dir, consumer_id);
        let Some(_lock) = try_cursor_lock(&lock_path)? else {
            continue;
        };

        // Re-read mtimes only after acquiring the consumer lock. An active
        // writer that refreshed the cursor immediately before the lock handoff
        // must not be judged using the earlier directory scan.
        let cursor_exists = cursor_path.try_exists()?;
        let remove_cursor = !active_consumers.contains(&consumer_id)
            && cursor_exists
            && stale_at(&cursor_path, now, retention_secs)?;
        if remove_cursor {
            match fs::remove_file(&cursor_path) {
                Ok(()) => directory_changed = true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("remove stale cursor {}", cursor_path.display()));
                }
            }
            match fs::remove_file(&lock_path) {
                Ok(()) => directory_changed = true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("remove stale cursor lock {}", lock_path.display())
                    });
                }
            }
            continue;
        }

        if cursor_exists {
            let mut cursor = read_cursor_file(&cursor_path)?;
            let original = cursor.clone();
            compact_cursor(&mut cursor, &retained_ids);
            if cursor != original {
                atomic_write_json(&cursor_path, &cursor)?;
            }
        } else if !active_consumers.contains(&consumer_id)
            && stale_at(&lock_path, now, retention_secs)?
        {
            // A read of an empty inbox may create only the advisory lock, not
            // a JSON cursor. Once that orphan lock is both unlocked and old,
            // it carries no state and is safe to unlink.
            match fs::remove_file(&lock_path) {
                Ok(()) => directory_changed = true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("remove orphan cursor lock {}", lock_path.display())
                    });
                }
            }
        }
    }
    if directory_changed {
        fs::File::open(&mp.cursors_dir)?.sync_all()?;
    }
    Ok(())
}

/// Prunes a workspace mailbox per design doc section 4: default 7-day TTL,
/// then a per-workspace cap (1000 messages / 10 MiB), oldest first. The same
/// mailbox lock used by append makes the scan/delete/cursor-compaction pass a
/// transaction with respect to sends and acknowledgements.
pub fn gc_workspace(paths: &Paths, canonical_workspace: &Path) -> Result<GcReport> {
    gc_workspace_in(
        &ensure_workspace(paths, canonical_workspace)?,
        canonical_workspace,
        &list_records(paths)?,
    )
}

/// `gc_workspace` on an already-ensured mailbox; `records` (every session
/// record on this host) decides which consumers' cursor state is still
/// live.
pub fn gc_workspace_in(
    mp: &MessagePaths,
    canonical_workspace: &Path,
    records: &[SessionRecord],
) -> Result<GcReport> {
    let _mailbox = FileLock::exclusive(&mailbox_lock_path(mp), false)?;
    let report = prune_workspace_locked(
        mp,
        canonical_workspace,
        None,
        MAX_MESSAGES_PER_WORKSPACE,
        MAX_WORKSPACE_BYTES,
        true,
    )?;
    let active_consumers = records
        .iter()
        .filter(|record| record.workspace == canonical_workspace && record.worker_phase_active())
        .map(|record| record.id)
        .collect();
    maintain_workspace_cursors_locked(
        mp,
        &active_consumers,
        now_secs(),
        STALE_CURSOR_RETENTION_SECS,
    )?;
    Ok(report)
}

/// Cheap opportunistic sweep, gated by a marker file's mtime so a busy
/// mailbox is not rescanned on every `send`/`inbox` call (design doc
/// section 4: "pruning is opportunistic ... any `a message` invocation may
/// unlink expired files"). `a message gc` itself bypasses this gate.
pub fn maybe_gc(paths: &Paths, canonical_workspace: &Path) -> Result<()> {
    maybe_gc_in(
        &ensure_workspace(paths, canonical_workspace)?,
        canonical_workspace,
        &list_records(paths)?,
    )
}

/// `maybe_gc` on an already-ensured mailbox.
pub fn maybe_gc_in(
    mp: &MessagePaths,
    canonical_workspace: &Path,
    records: &[SessionRecord],
) -> Result<()> {
    let marker = mp.workspace_dir.join(".gc_marker");
    // A marker mtime in the future (clock step, restored backup) must not
    // make every call sweep: saturate the age at zero instead of erroring.
    let due = modified_secs(&marker)
        .ok()
        .flatten()
        .is_none_or(|modified| {
            now_secs().saturating_sub(modified) > OPPORTUNISTIC_GC_INTERVAL_SECS
        });
    if due {
        gc_workspace_in(mp, canonical_workspace, records)?;
        let _ = fs::write(&marker, now_secs().to_string());
    }
    Ok(())
}
