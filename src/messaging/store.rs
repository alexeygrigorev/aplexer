//! Appending and reading message files.

use super::*;

/// Writes one serialized JSON message via the crate's standard atomic-write
/// discipline (temp file + fsync + rename, spec.md 14.1), named by the
/// message's own UUIDv7 id so lexical directory order is chronological
/// order (design doc section 3.2/4).
pub fn write_message(paths: &Paths, envelope: &MessageEnvelope) -> Result<()> {
    write_message_in(&ensure_workspace(paths, &envelope.workspace)?, envelope)
}

/// `write_message` into an already-ensured mailbox.
pub fn write_message_in(mp: &MessagePaths, envelope: &MessageEnvelope) -> Result<()> {
    write_message_limited(
        mp,
        envelope,
        MAX_MESSAGES_PER_WORKSPACE,
        MAX_WORKSPACE_BYTES,
    )
}

pub(crate) fn write_message_limited(
    mp: &MessagePaths,
    envelope: &MessageEnvelope,
    max_messages: usize,
    max_bytes: u64,
) -> Result<()> {
    let bytes = serialized_envelope(envelope)?;
    if bytes.len() as u64 > max_bytes {
        bail!(
            "serialized message envelope is {} bytes, larger than the {max_bytes}-byte workspace quota",
            bytes.len()
        );
    }
    let _mailbox = FileLock::exclusive(&mailbox_lock_path(mp), false)?;
    let path = mp.msgs_dir.join(format!("{}.json", envelope.id));
    if path.try_exists()? {
        bail!("message {} already exists", envelope.id);
    }
    atomic_write_bytes(&path, &bytes)?;

    // The new message is protected from eviction: a successful send must
    // mean that exact id is still durable when this function returns. If an
    // old file cannot be removed, roll this append back and report failure
    // instead of returning success with the mailbox above its hard limits.
    if let Err(error) = prune_workspace_locked(
        mp,
        &envelope.workspace,
        Some(&path),
        max_messages,
        max_bytes,
        false,
    ) {
        let _ = fs::remove_file(&path);
        let _ = fs::File::open(&mp.msgs_dir).and_then(|dir| dir.sync_all());
        return Err(error).context("enforce mailbox quota after append");
    }
    Ok(())
}

pub fn read_message(
    paths: &Paths,
    canonical_workspace: &Path,
    id: Uuid,
) -> Result<MessageEnvelope> {
    read_message_in(
        &ensure_workspace(paths, canonical_workspace)?,
        canonical_workspace,
        id,
    )
}

/// `read_message` from an already-ensured mailbox.
pub fn read_message_in(
    mp: &MessagePaths,
    canonical_workspace: &Path,
    id: Uuid,
) -> Result<MessageEnvelope> {
    let path = mp.msgs_dir.join(format!("{id}.json"));
    load_message_file(&path, canonical_workspace)
        .with_context(|| format!("read mailbox message {id}"))
}

fn message_id_from_path(path: &Path) -> Result<Uuid> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| {
            anyhow!(
                "mailbox message has no UTF-8 filename UUID: {}",
                path.display()
            )
        })?;
    Uuid::parse_str(stem).with_context(|| {
        format!(
            "parse mailbox message filename UUID from {}",
            path.display()
        )
    })
}

/// Open an existing mailbox entry without following a final-component
/// symlink. O_NONBLOCK keeps an accidental FIFO from hanging the caller before
/// its descriptor can be inspected; it has no effect on regular-file reads.
pub(crate) fn open_message_file(path: &Path) -> Result<(File, u64)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open mailbox message {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect mailbox message {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("mailbox message is not a regular file: {}", path.display());
    }
    if metadata.len() > MAX_ENVELOPE_BYTES as u64 {
        bail!(
            "mailbox message {} exceeds the {MAX_ENVELOPE_BYTES}-byte envelope cap (got {} bytes)",
            path.display(),
            metadata.len()
        );
    }
    Ok((file, metadata.len()))
}

pub(crate) fn load_open_message_file(
    file: File,
    path: &Path,
    expected_workspace: &Path,
) -> Result<MessageEnvelope> {
    let expected_id = message_id_from_path(path)?;
    let envelope: MessageEnvelope =
        read_bounded_json(file, path, "mailbox message", MAX_ENVELOPE_BYTES)?;
    if envelope.schema_version != MESSAGE_SCHEMA_VERSION {
        bail!(
            "unsupported mailbox message schema {} in {}",
            envelope.schema_version,
            path.display()
        );
    }
    if envelope.id != expected_id {
        bail!(
            "mailbox message id {} does not match filename id {} in {}",
            envelope.id,
            expected_id,
            path.display()
        );
    }
    if envelope.workspace != expected_workspace {
        bail!(
            "mailbox message {} belongs to workspace {}, expected {}",
            envelope.id,
            envelope.workspace.display(),
            expected_workspace.display()
        );
    }
    if matches!(envelope.to, Recipient::Broadcast { broadcast: false }) {
        bail!(
            "mailbox message {} is addressed to {{\"broadcast\": false}}, which is nobody",
            envelope.id
        );
    }
    Ok(envelope)
}

pub(crate) fn load_message_file(path: &Path, expected_workspace: &Path) -> Result<MessageEnvelope> {
    let (file, _) = open_message_file(path)?;
    load_open_message_file(file, path, expected_workspace)
}

/// Lists every message currently on disk for a workspace, in id (= time)
/// order. Invalid `.json` entries fail the read rather than being silently
/// reinterpreted or hidden: cursor acknowledgement identity depends on the
/// filename, envelope id, and mailbox workspace agreeing exactly.
pub fn list_messages(paths: &Paths, canonical_workspace: &Path) -> Result<Vec<MessageEnvelope>> {
    list_messages_in(
        &ensure_workspace(paths, canonical_workspace)?,
        canonical_workspace,
    )
}

/// `list_messages` from an already-ensured mailbox.
pub fn list_messages_in(
    mp: &MessagePaths,
    canonical_workspace: &Path,
) -> Result<Vec<MessageEnvelope>> {
    let mut out = Vec::new();
    for path in entries_with_extension(&mp.msgs_dir, &["json"])? {
        out.push(load_message_file(&path, canonical_workspace)?);
    }
    // Uuid's Ord is a byte-wise compare of the 128-bit value; for UUIDv7 the
    // 48-bit millisecond timestamp occupies the top bits, so this is also
    // chronological order (design doc section 4).
    out.sort_by_key(|m| m.id);
    Ok(out)
}
