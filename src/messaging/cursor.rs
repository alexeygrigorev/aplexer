//! Per-consumer acknowledgement state.

use super::*;

/// Per-consumer read/ack state (design doc section 3.2). New acknowledgements
/// are exact ids in `exceptions`; despite its legacy name, this set is now the
/// source of truth. `acked_through` remains only to read cursor files emitted
/// by older versions and is expanded into exact ids on the next cursor read.
/// Exact ids matter because UUID generation precedes the atomic mailbox
/// append: a delayed writer may commit a lower id after a later message was
/// acknowledged, which makes a high-water comparison unsafe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_through: Option<Uuid>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub exceptions: BTreeSet<Uuid>,
}

impl Cursor {
    pub fn is_acked(&self, id: Uuid) -> bool {
        self.acked_through.map(|t| id <= t).unwrap_or(false) || self.exceptions.contains(&id)
    }
}

pub(crate) fn cursor_lock_path(cursors_dir: &Path, consumer_id: Uuid) -> PathBuf {
    cursors_dir.join(format!("{consumer_id}.lock"))
}

pub(crate) fn read_cursor_file(path: &Path) -> Result<Cursor> {
    let Some(bytes) = read_bounded_regular_file(path, "mailbox cursor", MAX_MAILBOX_STATE_BYTES)?
    else {
        return Ok(Cursor::default());
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse mailbox cursor {}", path.display()))
}

pub(crate) fn retained_message_ids(msgs_dir: &Path) -> Result<BTreeSet<Uuid>> {
    let mut ids = BTreeSet::new();
    for path in entries_with_extension(msgs_dir, &["json"])? {
        // Cursor maintenance must not bless an unexpected mailbox entry as a
        // retained message id merely because its filename looks like a UUID.
        // Use the same bounded, no-follow regular-file check as actual loads;
        // parsing the full envelope remains the list/show operation's job.
        let _ = open_message_file(&path)?;
        if let Some(id) = uuid_stem(&path) {
            ids.insert(id);
        }
    }
    Ok(ids)
}

pub fn read_cursor(paths: &Paths, canonical_workspace: &Path, consumer_id: Uuid) -> Result<Cursor> {
    read_cursor_in(&ensure_workspace(paths, canonical_workspace)?, consumer_id)
}

/// `read_cursor` from an already-ensured mailbox.
pub fn read_cursor_in(mp: &MessagePaths, consumer_id: Uuid) -> Result<Cursor> {
    let _mailbox = FileLock::exclusive(&mailbox_lock_path(mp), false)?;
    let path = mp.cursors_dir.join(format!("{consumer_id}.json"));
    let _cursor = FileLock::exclusive(&cursor_lock_path(&mp.cursors_dir, consumer_id), false)?;
    let mut value = read_cursor_file(&path)?;
    let original = value.clone();
    let retained_ids = retained_message_ids(&mp.msgs_dir)?;
    compact_cursor(&mut value, &retained_ids);
    if value != original {
        atomic_write_json(&path, &value)?;
    }
    Ok(value)
}

/// Migrates a legacy high-water mark into exact ids for the messages that are
/// currently retained, then discards exact ids whose messages were pruned.
/// Once migrated, a message that commits later is unread regardless of how
/// its UUID compares with messages acknowledged earlier.
pub(crate) fn compact_cursor(cursor: &mut Cursor, retained_ids: &BTreeSet<Uuid>) {
    if let Some(through) = cursor.acked_through.take() {
        cursor
            .exceptions
            .extend(retained_ids.range(..=through).copied());
    }
    cursor.exceptions.retain(|id| retained_ids.contains(id));
}

/// Records `ids` exactly as acknowledged for `consumer_id` and returns the
/// ones the mailbox still holds, in request order. An id whose message was
/// pruned (or never existed here) is not recorded -- there is nothing to
/// hide -- and is left out so the caller can say so. The mailbox lock
/// prevents append/GC from changing the retained set during the update; the
/// per-consumer lock prevents two acknowledgements from overwriting each
/// other. Lock order is always mailbox then cursor.
pub fn ack_messages(
    paths: &Paths,
    canonical_workspace: &Path,
    consumer_id: Uuid,
    ids: &[Uuid],
) -> Result<Vec<Uuid>> {
    ack_messages_in(
        &ensure_workspace(paths, canonical_workspace)?,
        consumer_id,
        ids,
    )
}

/// `ack_messages` in an already-ensured mailbox.
pub fn ack_messages_in(mp: &MessagePaths, consumer_id: Uuid, ids: &[Uuid]) -> Result<Vec<Uuid>> {
    let path = mp.cursors_dir.join(format!("{consumer_id}.json"));
    let _mailbox = FileLock::exclusive(&mailbox_lock_path(mp), false)?;
    let _cursor = FileLock::exclusive(&cursor_lock_path(&mp.cursors_dir, consumer_id), false)?;
    let mut cursor = read_cursor_file(&path)?;
    let retained_ids = retained_message_ids(&mp.msgs_dir)?;
    compact_cursor(&mut cursor, &retained_ids);
    let mut acked = Vec::new();
    for id in ids {
        if retained_ids.contains(id) && !acked.contains(id) {
            cursor.exceptions.insert(*id);
            acked.push(*id);
        }
    }
    atomic_write_json(&path, &cursor)?;
    Ok(acked)
}
