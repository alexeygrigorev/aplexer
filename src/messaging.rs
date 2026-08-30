//! Inter-agent messaging channel (docs/inter-agent-messaging-design.md).
//!
//! Storage model (design doc section 3.2): one file per message under
//! `${state_root}/messages/<workspace-key>/{workspace.json, msgs/<uuid>.json,
//! cursors/<consumer-id>.json}`, written with the same atomic-write
//! discipline (temp file + fsync + rename) the rest of aplexer already uses
//! for session metadata (spec.md 14.1). No process owns this state; any
//! process may read, append, or prune it.

use crate::{
    atomic_write_bytes, atomic_write_json, ensure_private_dir, list_records, now_ms, FileLock,
    Paths,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use uuid::Uuid;

pub const MESSAGE_SCHEMA_VERSION: u32 = 1;
/// Design doc section 5: "body ... size-capped (e.g. 64 KB)".
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// Cap the complete serialized envelope too: `data`, sender metadata, and
/// JSON escaping must not provide a route around the body limit. Eight times
/// the body cap leaves room for the worst-case JSON escaping of a 64 KiB body
/// while still keeping every individual mailbox file comfortably bounded.
pub const MAX_ENVELOPE_BYTES: usize = 8 * MAX_BODY_BYTES;
/// Design doc section 4: "default TTL 7 days".
pub const DEFAULT_TTL_SECS: u64 = 7 * 24 * 3600;
/// Design doc section 4: "a per-workspace cap (e.g. 1000 messages / 10 MB)
/// as backstop".
pub const MAX_MESSAGES_PER_WORKSPACE: usize = 1000;
pub const MAX_WORKSPACE_BYTES: u64 = 10 * 1024 * 1024;
/// Minimum interval between opportunistic sweeps triggered from `send`/
/// `inbox` (see `maybe_gc`) so a large mailbox is not rescanned on every
/// call; `a message gc` itself always runs unconditionally.
const OPPORTUNISTIC_GC_INTERVAL_SECS: u64 = 300;
const MAX_MAILBOX_STATE_BYTES: usize = 64 * 1024;
/// Cursor state belongs to one session UUID and has no value once that
/// session is gone. Keep inactive cursor state for a full month before GC so
/// short-lived cleanup/recovery gaps cannot make an acknowledgement reappear.
/// Live/starting session ids are retained regardless of age.
pub const STALE_CURSOR_RETENTION_SECS: u64 = 30 * 24 * 3600;
const MAILBOX_LOCK_FILE: &str = ".mailbox.lock";

pub fn now_secs() -> u64 {
    now_ms() / 1000
}

/// Directory-naming key for a workspace's mailbox (design doc section 3.2
/// and open question 8): the first 128 bits of SHA-256 over the canonical
/// Unix path's exact bytes. Hashing the raw bytes avoids aliasing distinct
/// non-UTF-8 paths through lossy string conversion, and specifying SHA-256
/// keeps keys stable across Rust/toolchain releases. Every caller MUST pass
/// a path already run through `canonical_workspace` (open question 8) so
/// two sessions in the same workspace can never straddle two mailboxes.
pub fn workspace_key(canonical_workspace: &Path) -> String {
    let digest = Sha256::digest(canonical_workspace.as_os_str().as_bytes());
    digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Key emitted before mailbox keys were specified as truncated SHA-256.
/// This intentionally preserves the old implementation exactly so
/// `ensure_workspace` can locate and migrate existing mailboxes. It must not
/// be used for new directories: both `DefaultHasher`'s algorithm and the
/// lossy path conversion are unsuitable as a persistent storage format.
fn legacy_workspace_key(canonical_workspace: &Path) -> String {
    let text = canonical_workspace.to_string_lossy();
    let mut h1 = DefaultHasher::new();
    text.hash(&mut h1);
    let a = h1.finish();
    let mut h2 = DefaultHasher::new();
    (text.as_ref(), "aplexer-messaging-key-v1").hash(&mut h2);
    let b = h2.finish();
    format!("{a:016x}{b:016x}")
}

#[derive(Debug, Clone)]
pub struct MessagePaths {
    pub workspace_dir: PathBuf,
    pub msgs_dir: PathBuf,
    pub cursors_dir: PathBuf,
    pub workspace_file: PathBuf,
}

fn message_paths_for_key(paths: &Paths, key: &str) -> MessagePaths {
    let workspace_dir = paths.state_root.join("messages").join(key);
    MessagePaths {
        msgs_dir: workspace_dir.join("msgs"),
        cursors_dir: workspace_dir.join("cursors"),
        workspace_file: workspace_dir.join("workspace.json"),
        workspace_dir,
    }
}

pub fn message_paths(paths: &Paths, canonical_workspace: &Path) -> MessagePaths {
    message_paths_for_key(paths, &workspace_key(canonical_workspace))
}

#[derive(Deserialize)]
struct WorkspaceMetadata {
    workspace: PathBuf,
}

/// Verifies the reverse mapping before adopting a mailbox directory. The old
/// key was based on lossy UTF-8 and therefore could alias two distinct Unix
/// paths; the stable key is truncated and likewise must never be trusted
/// without its reverse mapping.
fn verify_workspace_metadata(workspace_dir: &Path, canonical_workspace: &Path) -> Result<()> {
    let metadata_path = workspace_dir.join("workspace.json");
    let bytes =
        read_bounded_regular_file(&metadata_path, "mailbox metadata", MAX_MAILBOX_STATE_BYTES)?
            .ok_or_else(|| anyhow!("mailbox metadata is missing: {}", metadata_path.display()))?;
    let metadata: WorkspaceMetadata = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse mailbox metadata {}", metadata_path.display()))?;
    if metadata.workspace != canonical_workspace {
        bail!(
            "refusing to migrate mailbox {}: workspace metadata names {}, expected {}",
            workspace_dir.display(),
            metadata.workspace.display(),
            canonical_workspace.display()
        );
    }
    Ok(())
}

/// Read small mailbox state without following symlinks or blocking on an
/// accidental FIFO/device. Returning `None` for absence lets cursor callers
/// retain their documented empty-state behavior while every other file type
/// and oversize value fails closed.
fn read_bounded_regular_file(path: &Path, label: &str, cap: usize) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("open {label} {}", path.display()))
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("{label} is not a regular file: {}", path.display());
    }
    if metadata.len() > cap as u64 {
        bail!(
            "{label} {} exceeds the {cap}-byte cap (got {} bytes)",
            path.display(),
            metadata.len()
        );
    }
    let mut bytes = Vec::new();
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {label} {}", path.display()))?;
    if bytes.len() > cap {
        bail!("{label} {} exceeds the {cap}-byte cap", path.display());
    }
    Ok(Some(bytes))
}

fn initialize_workspace_dir(mp: &MessagePaths, canonical_workspace: &Path) -> Result<()> {
    ensure_private_dir(&mp.workspace_dir)?;
    ensure_private_dir(&mp.msgs_dir)?;
    ensure_private_dir(&mp.cursors_dir)?;
    if mp.workspace_file.exists() {
        verify_workspace_metadata(&mp.workspace_dir, canonical_workspace)?;
    } else {
        atomic_write_json(
            &mp.workspace_file,
            &serde_json::json!({"workspace": canonical_workspace}),
        )?;
    }
    Ok(())
}

fn json_files_equal(left: &Path, right: &Path, label: &str, cap: usize) -> Result<bool> {
    let left_bytes = read_bounded_regular_file(left, label, cap)?
        .ok_or_else(|| anyhow!("{label} disappeared during migration: {}", left.display()))?;
    let right_bytes = read_bounded_regular_file(right, label, cap)?
        .ok_or_else(|| anyhow!("{label} disappeared during migration: {}", right.display()))?;
    if left_bytes == right_bytes {
        return Ok(true);
    }
    let left_json = serde_json::from_slice::<Value>(&left_bytes);
    let right_json = serde_json::from_slice::<Value>(&right_bytes);
    Ok(matches!((left_json, right_json), (Ok(left), Ok(right)) if left == right))
}

fn mailbox_json_files_equal(left: &Path, right: &Path) -> Result<bool> {
    let is_cursor = left
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "cursors");
    if is_cursor {
        json_files_equal(left, right, "mailbox cursor", MAX_MAILBOX_STATE_BYTES)
    } else {
        json_files_equal(left, right, "mailbox message", MAX_ENVELOPE_BYTES)
    }
}

fn json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(paths),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        if !entry.file_type()?.is_file() {
            bail!(
                "refusing to migrate non-file mailbox entry {}",
                path.display()
            );
        }
        paths.push(path);
    }
    paths.sort();
    Ok(paths)
}

enum MailboxMigration {
    Move {
        source: PathBuf,
        destination: PathBuf,
    },
    RemoveDuplicate {
        source: PathBuf,
        destination: PathBuf,
    },
    MergeCursor {
        source: PathBuf,
        destination: PathBuf,
        cursor: Cursor,
    },
}

fn merged_cursor(left: &Cursor, right: &Cursor, retained_ids: &BTreeSet<Uuid>) -> Cursor {
    let mut left = left.clone();
    let mut right = right.clone();
    compact_cursor(&mut left, retained_ids);
    compact_cursor(&mut right, retained_ids);
    left.exceptions.extend(right.exceptions);
    left
}

/// Preflights every collision before moving anything. A crash during the
/// subsequent application can leave a partially drained legacy directory,
/// but rerunning is idempotent: unique files use no-replace hard links,
/// identical files are deduplicated, and cursors are unioned.
fn plan_mailbox_merge(
    stable: &MessagePaths,
    legacy: &MessagePaths,
    canonical_workspace: &Path,
) -> Result<Vec<MailboxMigration>> {
    let mut actions = Vec::new();
    for source in json_files(&legacy.msgs_dir)? {
        // Validate every source before scheduling any move. Unique files must
        // not bypass the same schema, identity, workspace, type, and size
        // checks applied to collisions and normal inbox reads.
        load_message_file(&source, canonical_workspace)
            .with_context(|| format!("validate legacy mailbox message {}", source.display()))?;
        let destination = stable.msgs_dir.join(
            source
                .file_name()
                .ok_or_else(|| anyhow!("{} has no file name", source.display()))?,
        );
        if destination.exists() {
            if !mailbox_json_files_equal(&source, &destination)? {
                bail!(
                    "mailbox message collision: {} and {} have different content",
                    source.display(),
                    destination.display()
                );
            }
            actions.push(MailboxMigration::RemoveDuplicate {
                source,
                destination,
            });
        } else {
            actions.push(MailboxMigration::Move {
                source,
                destination,
            });
        }
    }

    let mut retained_ids = retained_message_ids(&stable.msgs_dir)?;
    retained_ids.extend(retained_message_ids(&legacy.msgs_dir)?);
    for source in json_files(&legacy.cursors_dir)? {
        let source_cursor = read_cursor_file(&source)
            .with_context(|| format!("validate legacy mailbox cursor {}", source.display()))?;
        let destination = stable.cursors_dir.join(
            source
                .file_name()
                .ok_or_else(|| anyhow!("{} has no file name", source.display()))?,
        );
        if !destination.exists() {
            actions.push(MailboxMigration::Move {
                source,
                destination,
            });
            continue;
        }
        if mailbox_json_files_equal(&source, &destination)? {
            actions.push(MailboxMigration::RemoveDuplicate {
                source,
                destination,
            });
            continue;
        }
        let destination_cursor = read_cursor_file(&destination)
            .with_context(|| format!("parse colliding cursor {}", destination.display()))?;
        actions.push(MailboxMigration::MergeCursor {
            source,
            destination,
            cursor: merged_cursor(&source_cursor, &destination_cursor, &retained_ids),
        });
    }
    Ok(actions)
}

fn apply_mailbox_merge(actions: Vec<MailboxMigration>) -> Result<()> {
    let mut changed_dirs = BTreeSet::new();
    for action in actions {
        match action {
            MailboxMigration::Move {
                source,
                destination,
            } => {
                match fs::hard_link(&source, &destination) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        if !mailbox_json_files_equal(&source, &destination)? {
                            bail!(
                                "mailbox migration destination appeared with different content: {}",
                                destination.display()
                            );
                        }
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("migrate {} to {}", source.display(), destination.display())
                        });
                    }
                }
                fs::remove_file(&source)
                    .with_context(|| format!("remove migrated {}", source.display()))?;
                changed_dirs.insert(source.parent().unwrap().to_path_buf());
                changed_dirs.insert(destination.parent().unwrap().to_path_buf());
            }
            MailboxMigration::RemoveDuplicate {
                source,
                destination,
            } => {
                if !mailbox_json_files_equal(&source, &destination)? {
                    bail!(
                        "mailbox duplicate changed during migration: {}",
                        source.display()
                    );
                }
                fs::remove_file(&source)
                    .with_context(|| format!("remove duplicate {}", source.display()))?;
                changed_dirs.insert(source.parent().unwrap().to_path_buf());
            }
            MailboxMigration::MergeCursor {
                source,
                destination,
                cursor,
            } => {
                atomic_write_json(&destination, &cursor)?;
                fs::remove_file(&source)
                    .with_context(|| format!("remove merged cursor {}", source.display()))?;
                changed_dirs.insert(source.parent().unwrap().to_path_buf());
                changed_dirs.insert(destination.parent().unwrap().to_path_buf());
            }
        }
    }
    for dir in changed_dirs {
        fs::File::open(&dir)?.sync_all()?;
    }
    Ok(())
}

fn remove_drained_legacy_cursor_locks(legacy: &MessagePaths) -> Result<()> {
    let mut directory_changed = false;
    for consumer_id in cursor_entry_ids(&legacy.cursors_dir)? {
        let cursor_path = legacy.cursors_dir.join(format!("{consumer_id}.json"));
        if cursor_path.exists() {
            continue;
        }
        let lock_path = cursor_lock_path(&legacy.cursors_dir, consumer_id);
        let Some(_lock) = try_cursor_lock(&lock_path)? else {
            continue;
        };
        match fs::remove_file(&lock_path) {
            Ok(()) => directory_changed = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("remove drained legacy cursor lock {}", lock_path.display())
                });
            }
        }
    }
    if directory_changed {
        fs::File::open(&legacy.cursors_dir)?.sync_all()?;
    }
    Ok(())
}

fn merge_legacy_mailbox(
    stable: &MessagePaths,
    legacy: &MessagePaths,
    canonical_workspace: &Path,
) -> Result<()> {
    let actions = plan_mailbox_merge(stable, legacy, canonical_workspace)?;
    apply_mailbox_merge(actions)?;
    remove_drained_legacy_cursor_locks(legacy)
}

/// Creates the workspace mailbox's directories (`0700`, spec.md 26) and its
/// reverse-lookup `workspace.json` if missing, idempotently.
///
/// `ensure_private_dir` only chmods the exact path it's given, not any
/// parent directories `fs::create_dir_all` had to create along the way --
/// `${state_root}/messages/` itself would otherwise be created world/group-
/// readable (whatever the process umask gives `create_dir_all`) the first
/// time any workspace's mailbox is touched, since `Paths::ensure()` never
/// creates it up front. So the shared `messages/` root is chmod'd
/// explicitly here, before the per-workspace subdirectories.
pub fn ensure_workspace(paths: &Paths, canonical_workspace: &Path) -> Result<MessagePaths> {
    let messages_root = paths.state_root.join("messages");
    ensure_private_dir(&messages_root)?;

    let stable_key = workspace_key(canonical_workspace);
    let mp = message_paths_for_key(paths, &stable_key);
    let legacy_mp = message_paths_for_key(paths, &legacy_workspace_key(canonical_workspace));

    // Serialize discovery/migration for this destination. If only the legacy
    // directory exists it can still move atomically as a unit. If both exist,
    // take both mailbox locks and losslessly drain legacy JSON files into the
    // stable directory. The empty legacy skeleton is deliberately retained:
    // an older concurrently-installed CLI may write there again, and every
    // current operation will notice and drain it instead of silently ignoring
    // those messages.
    let migration_lock = messages_root.join(format!(".{stable_key}.migration.lock"));
    let _migration = FileLock::exclusive(&migration_lock, false)?;
    if !mp.workspace_dir.exists()
        && legacy_mp.workspace_dir != mp.workspace_dir
        && legacy_mp.workspace_dir.exists()
    {
        let _legacy_mailbox = FileLock::exclusive(&mailbox_lock_path(&legacy_mp), false)?;
        verify_workspace_metadata(&legacy_mp.workspace_dir, canonical_workspace)?;
        fs::rename(&legacy_mp.workspace_dir, &mp.workspace_dir).with_context(|| {
            format!(
                "migrate legacy mailbox {} to {}",
                legacy_mp.workspace_dir.display(),
                mp.workspace_dir.display()
            )
        })?;
    } else {
        initialize_workspace_dir(&mp, canonical_workspace)?;
        if legacy_mp.workspace_dir != mp.workspace_dir && legacy_mp.workspace_dir.exists() {
            let _stable_mailbox = FileLock::exclusive(&mailbox_lock_path(&mp), false)?;
            let _legacy_mailbox = FileLock::exclusive(&mailbox_lock_path(&legacy_mp), false)?;
            verify_workspace_metadata(&mp.workspace_dir, canonical_workspace)?;
            verify_workspace_metadata(&legacy_mp.workspace_dir, canonical_workspace)?;
            ensure_private_dir(&legacy_mp.msgs_dir)?;
            ensure_private_dir(&legacy_mp.cursors_dir)?;
            merge_legacy_mailbox(&mp, &legacy_mp, canonical_workspace)?;
        }
    }
    initialize_workspace_dir(&mp, canonical_workspace)?;
    Ok(mp)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageFrom {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Design doc section 2.1: a sender with no resolvable session identity
    /// (no `--from`, no `APLEXER_SESSION_ID`) is still allowed to send, "a
    /// human poking at the mailbox is a legitimate participant" -- recorded
    /// as `{"tag": null, "external": true}`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub external: bool,
}
fn is_false(b: &bool) -> bool {
    !*b
}
impl MessageFrom {
    pub fn anonymous() -> Self {
        Self {
            session_id: None,
            tag: None,
            engine: None,
            profile: None,
            external: true,
        }
    }
}

/// One of exactly three shapes (design doc section 5): `{"tag":...}` (with
/// optional `session_id` when resolvable at send time), `{"broadcast":true}`,
/// or `{"engine":...}`. `#[serde(untagged)]` tries each variant in
/// declaration order and matches on field presence, which reproduces this
/// exact wire shape (each variant has a disjoint field name) without a
/// separate discriminant tag -- unlike `Phase`/`FrameKind` elsewhere in this
/// crate, which use an explicit `tag = "..."` because their variants are
/// plain enums, not field-carrying shapes keyed by different field names.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Recipient {
    Tag {
        tag: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<Uuid>,
    },
    Broadcast {
        broadcast: bool,
    },
    Engine {
        engine: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    Inbox,
    Pane,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEnvelope {
    pub schema_version: u32,
    pub id: Uuid,
    pub workspace: PathBuf,
    pub created_at: u64,
    pub from: MessageFrom,
    pub to: Recipient,
    /// Open enum (design doc section 5): unknown/future kinds must be
    /// preserved and displayed, never dropped -- hence a plain `String`
    /// rather than a closed Rust enum.
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<Uuid>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default)]
    pub delivery: Delivery,
}
fn default_kind() -> String {
    "note".to_string()
}

pub fn check_body_size(body: &str) -> Result<()> {
    let len = body.len();
    if len > MAX_BODY_BYTES {
        bail!("message body exceeds the {MAX_BODY_BYTES}-byte cap (got {len} bytes); point at a file in the workspace instead of pasting large content");
    }
    Ok(())
}

fn serialized_envelope(envelope: &MessageEnvelope) -> Result<Vec<u8>> {
    check_body_size(&envelope.body)?;
    // Match `atomic_write_json`'s durable representation exactly: it first
    // converts to a Value, pretty-prints it, and appends a newline.
    let value = serde_json::to_value(envelope)?;
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_ENVELOPE_BYTES {
        bail!(
            "serialized message envelope exceeds the {MAX_ENVELOPE_BYTES}-byte cap (got {} bytes)",
            bytes.len()
        );
    }
    Ok(bytes)
}

fn mailbox_lock_path(mp: &MessagePaths) -> PathBuf {
    mp.workspace_dir.join(MAILBOX_LOCK_FILE)
}

/// Writes one serialized JSON message via the crate's standard atomic-write
/// discipline (temp file + fsync + rename, spec.md 14.1), named by the
/// message's own UUIDv7 id so lexical directory order is chronological
/// order (design doc section 3.2/4).
pub fn write_message(paths: &Paths, envelope: &MessageEnvelope) -> Result<()> {
    write_message_with_limits(
        paths,
        envelope,
        MAX_MESSAGES_PER_WORKSPACE,
        MAX_WORKSPACE_BYTES,
    )
}

fn write_message_with_limits(
    paths: &Paths,
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
    let mp = ensure_workspace(paths, &envelope.workspace)?;
    let _mailbox = FileLock::exclusive(&mailbox_lock_path(&mp), false)?;
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
        &mp,
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
    let mp = ensure_workspace(paths, canonical_workspace)?;
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
fn open_message_file(path: &Path) -> Result<(File, u64)> {
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

fn load_open_message_file(
    file: File,
    path: &Path,
    expected_workspace: &Path,
) -> Result<MessageEnvelope> {
    let expected_id = message_id_from_path(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_ENVELOPE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read mailbox message {}", path.display()))?;
    // Recheck after reading so a regular file that grows after fstat remains
    // bounded and is rejected instead of being parsed from a truncated prefix.
    if bytes.len() > MAX_ENVELOPE_BYTES {
        bail!(
            "mailbox message {} exceeds the {MAX_ENVELOPE_BYTES}-byte envelope cap",
            path.display()
        );
    }
    let envelope: MessageEnvelope = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse mailbox message {}", path.display()))?;
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
    Ok(envelope)
}

fn load_message_file(path: &Path, expected_workspace: &Path) -> Result<MessageEnvelope> {
    let (file, _) = open_message_file(path)?;
    load_open_message_file(file, path, expected_workspace)
}

/// Lists every message currently on disk for a workspace, in id (= time)
/// order. Invalid `.json` entries fail the read rather than being silently
/// reinterpreted or hidden: cursor acknowledgement identity depends on the
/// filename, envelope id, and mailbox workspace agreeing exactly.
pub fn list_messages(paths: &Paths, canonical_workspace: &Path) -> Result<Vec<MessageEnvelope>> {
    let mp = ensure_workspace(paths, canonical_workspace)?;
    let mut out = Vec::new();
    let entries = match fs::read_dir(&mp.msgs_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("enumerate {}", mp.msgs_dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        out.push(load_message_file(&path, canonical_workspace)?);
    }
    // Uuid's Ord is a byte-wise compare of the 128-bit value; for UUIDv7 the
    // 48-bit millisecond timestamp occupies the top bits, so this is also
    // chronological order (design doc section 4).
    out.sort_by_key(|m| m.id);
    Ok(out)
}

/// Design doc section 2.2/2.3: the inbox filter matches on the recipient
/// session's *current* tag plus its session id recorded at send time (when
/// resolvable) -- so a renamed session keeps messages resolved to its id
/// and stops matching its old tag string, and a reused tag is inherited by
/// whatever session holds it now. Broadcast/engine-filtered forms exclude
/// the sender itself ("every session in the workspace except the sender").
pub fn addressed_to(
    envelope: &MessageEnvelope,
    consumer_id: Uuid,
    consumer_tag: &str,
    consumer_engine: &str,
) -> bool {
    let is_sender = envelope.from.session_id == Some(consumer_id);
    match &envelope.to {
        Recipient::Tag { tag, session_id } => {
            session_id.map(|sid| sid == consumer_id).unwrap_or(false) || tag == consumer_tag
        }
        Recipient::Broadcast { broadcast } => *broadcast && !is_sender,
        Recipient::Engine { engine } => engine == consumer_engine && !is_sender,
    }
}

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

fn cursor_lock_path(cursors_dir: &Path, consumer_id: Uuid) -> PathBuf {
    cursors_dir.join(format!("{consumer_id}.lock"))
}

fn read_cursor_file(path: &Path) -> Result<Cursor> {
    let Some(bytes) = read_bounded_regular_file(path, "mailbox cursor", MAX_MAILBOX_STATE_BYTES)?
    else {
        return Ok(Cursor::default());
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse mailbox cursor {}", path.display()))
}

fn retained_message_ids(msgs_dir: &Path) -> Result<BTreeSet<Uuid>> {
    let entries = match fs::read_dir(msgs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", msgs_dir.display())),
    };
    let mut ids = BTreeSet::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("enumerate {}", msgs_dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        // Cursor maintenance must not bless an unexpected mailbox entry as a
        // retained message id merely because its filename looks like a UUID.
        // Use the same bounded, no-follow regular-file check as actual loads;
        // parsing the full envelope remains the list/show operation's job.
        let _ = open_message_file(&path)?;
        if let Some(id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| Uuid::parse_str(stem).ok())
        {
            ids.insert(id);
        }
    }
    Ok(ids)
}

pub fn read_cursor(paths: &Paths, canonical_workspace: &Path, consumer_id: Uuid) -> Result<Cursor> {
    let mp = ensure_workspace(paths, canonical_workspace)?;
    let _mailbox = FileLock::exclusive(&mailbox_lock_path(&mp), false)?;
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
fn compact_cursor(cursor: &mut Cursor, retained_ids: &BTreeSet<Uuid>) {
    if let Some(through) = cursor.acked_through.take() {
        cursor
            .exceptions
            .extend(retained_ids.range(..=through).copied());
    }
    cursor.exceptions.retain(|id| retained_ids.contains(id));
}

/// Records `ids` exactly as acknowledged for `consumer_id`. The mailbox lock
/// prevents append/GC from changing the retained set during the update; the
/// per-consumer lock prevents two acknowledgements from overwriting each
/// other. Lock order is always mailbox then cursor.
pub fn ack_messages(
    paths: &Paths,
    canonical_workspace: &Path,
    consumer_id: Uuid,
    ids: &[Uuid],
) -> Result<()> {
    let mp = ensure_workspace(paths, canonical_workspace)?;
    let path = mp.cursors_dir.join(format!("{consumer_id}.json"));
    let _mailbox = FileLock::exclusive(&mailbox_lock_path(&mp), false)?;
    let _cursor = FileLock::exclusive(&cursor_lock_path(&mp.cursors_dir, consumer_id), false)?;
    let mut cursor = read_cursor_file(&path)?;
    let retained_ids = retained_message_ids(&mp.msgs_dir)?;
    compact_cursor(&mut cursor, &retained_ids);
    for id in ids {
        if retained_ids.contains(id) && !cursor.is_acked(*id) {
            cursor.exceptions.insert(*id);
        }
    }
    atomic_write_json(&path, &cursor)
}

/// Known tags for a workspace (design doc section 2.3), from session
/// metadata -- live or historical, not just currently-running sessions.
pub fn known_tags(paths: &Paths, canonical_workspace: &Path) -> Vec<String> {
    let mut tags: Vec<String> = list_records(paths)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.workspace == canonical_workspace)
        .map(|r| r.tag)
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

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
    let dir = match fs::read_dir(&mp.msgs_dir) {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", mp.msgs_dir.display()));
        }
    };
    for entry in dir {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
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
    // Message paths are UUIDv7 filenames, so bytewise path order is the
    // existing deterministic mailbox order. Malformed names still get a
    // stable eviction order rather than escaping the cap.
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn remove_mailbox_entry(
    entries: &mut Vec<MailboxEntry>,
    index: usize,
    total: &mut u64,
) -> Result<bool> {
    match fs::remove_file(&entries[index].path) {
        Ok(()) => {
            let entry = entries.remove(index);
            *total = total.saturating_sub(entry.size);
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let entry = entries.remove(index);
            *total = total.saturating_sub(entry.size);
            Ok(false)
        }
        Err(error) => Err(error)
            .with_context(|| format!("remove mailbox message {}", entries[index].path.display())),
    }
}

/// Applies TTL and hard caps while the caller holds `MAILBOX_LOCK_FILE`.
/// `protected` is the just-appended file: a successful send must not evict
/// itself merely because its UUID sorts before an older committed writer.
fn prune_workspace_locked(
    mp: &MessagePaths,
    expected_workspace: &Path,
    protected: Option<&Path>,
    max_messages: usize,
    max_bytes: u64,
    expire: bool,
) -> Result<GcReport> {
    let mut entries = mailbox_entries(mp, expected_workspace, expire)?;
    let mut total: u64 = entries.iter().map(|entry| entry.size).sum();
    let mut removed = 0usize;
    let mut directory_changed = false;

    if expire {
        let now = now_secs();
        let mut index = 0;
        while index < entries.len() {
            let is_protected = protected.is_some_and(|path| entries[index].path == path);
            let expired = entries[index]
                .created_at
                .is_some_and(|created_at| now.saturating_sub(created_at) > DEFAULT_TTL_SECS);
            if !is_protected && expired {
                let deleted = remove_mailbox_entry(&mut entries, index, &mut total)?;
                removed += usize::from(deleted);
                directory_changed |= deleted;
            } else {
                index += 1;
            }
        }
    }

    while entries.len() > max_messages || total > max_bytes {
        let Some(index) = entries
            .iter()
            .position(|entry| protected.is_none_or(|protected| entry.path != protected))
        else {
            bail!(
                "mailbox quota cannot retain the protected message ({} messages, {total} bytes)",
                entries.len()
            );
        };
        let deleted = remove_mailbox_entry(&mut entries, index, &mut total)?;
        removed += usize::from(deleted);
        directory_changed |= deleted;
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

fn cursor_entry_ids(cursors_dir: &Path) -> Result<BTreeSet<Uuid>> {
    let entries = match fs::read_dir(cursors_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", cursors_dir.display())),
    };
    let mut ids = BTreeSet::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("json" | "lock")
        ) {
            continue;
        }
        let Some(consumer_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| Uuid::parse_str(stem).ok())
        else {
            continue;
        };
        ids.insert(consumer_id);
    }
    Ok(ids)
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

fn try_cursor_lock(path: &Path) -> Result<Option<FileLock>> {
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
fn maintain_workspace_cursors_locked(
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
    let mp = ensure_workspace(paths, canonical_workspace)?;
    let _mailbox = FileLock::exclusive(&mailbox_lock_path(&mp), false)?;
    let report = prune_workspace_locked(
        &mp,
        canonical_workspace,
        None,
        MAX_MESSAGES_PER_WORKSPACE,
        MAX_WORKSPACE_BYTES,
        true,
    )?;
    let active_consumers = list_records(paths)?
        .into_iter()
        .filter(|record| record.workspace == canonical_workspace && record.worker_phase_active())
        .map(|record| record.id)
        .collect();
    maintain_workspace_cursors_locked(
        &mp,
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
    let mp = ensure_workspace(paths, canonical_workspace)?;
    let marker = mp.workspace_dir.join(".gc_marker");
    let due = fs::metadata(&marker)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .map(|elapsed| elapsed.as_secs() > OPPORTUNISTIC_GC_INTERVAL_SECS)
        .unwrap_or(true);
    if due {
        gc_workspace(paths, canonical_workspace)?;
        let _ = fs::write(&marker, now_secs().to_string());
    }
    Ok(())
}

/// Resolves the sender identity for `send`/`log` (design doc section 2.1):
/// `--from <tag>` (matched against session metadata for this workspace)
/// first, else `APLEXER_SESSION_ID`/`APLEXER_TAG` from the environment
/// (with a best-effort lookup of the full session record for engine/
/// profile), else anonymous.
pub fn resolve_sender(
    paths: &Paths,
    canonical_workspace: &Path,
    from_tag: Option<&str>,
) -> Result<MessageFrom> {
    if let Some(tag) = from_tag {
        let record = list_records(paths)?
            .into_iter()
            .find(|r| r.workspace == canonical_workspace && r.tag == tag)
            .ok_or_else(|| {
                anyhow!(
                    "no session tagged {tag:?} has ever existed in workspace {}",
                    canonical_workspace.display()
                )
            })?;
        return Ok(MessageFrom {
            session_id: Some(record.id),
            tag: Some(record.tag),
            engine: Some(record.engine),
            profile: record.profile,
            external: false,
        });
    }
    if let Some(session_id) = crate::discover_session_id() {
        if let Some(record) = list_records(paths)?
            .into_iter()
            .find(|r| r.id == session_id)
        {
            return Ok(MessageFrom {
                session_id: Some(record.id),
                tag: Some(record.tag),
                engine: Some(record.engine),
                profile: record.profile,
                external: false,
            });
        }
        return Ok(MessageFrom {
            session_id: Some(session_id),
            tag: std::env::var("APLEXER_TAG").ok(),
            engine: None,
            profile: None,
            external: false,
        });
    }
    Ok(MessageFrom::anonymous())
}

/// Resolves the *consumer* identity for `inbox`/`ack` (design doc section
/// 7's closing note: these need a consumer identity, unlike `send`/`log`
/// which degrade gracefully to anonymous). Errors clearly when neither
/// `--from` nor `APLEXER_SESSION_ID` is available.
pub fn resolve_consumer(
    paths: &Paths,
    canonical_workspace: &Path,
    from_tag: Option<&str>,
) -> Result<(Uuid, String, String)> {
    if let Some(tag) = from_tag {
        let record = list_records(paths)?
            .into_iter()
            .find(|r| r.workspace == canonical_workspace && r.tag == tag)
            .ok_or_else(|| {
                anyhow!(
                    "no session tagged {tag:?} has ever existed in workspace {}",
                    canonical_workspace.display()
                )
            })?;
        return Ok((record.id, record.tag, record.engine));
    }
    if let Some(session_id) = crate::discover_session_id() {
        let (tag, engine) = list_records(paths)?
            .into_iter()
            .find(|r| r.id == session_id)
            .map(|r| (r.tag, r.engine))
            .unwrap_or_else(|| {
                (
                    std::env::var("APLEXER_TAG").unwrap_or_default(),
                    String::new(),
                )
            });
        return Ok((session_id, tag, engine));
    }
    bail!(
        "no session identity: APLEXER_SESSION_ID is not set (you're not inside an aplexer \
         session) and no --from TAG was given"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::fs::{FileTimes, OpenOptions};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;
    use std::time::Duration;
    use tempfile::TempDir;

    fn test_paths(root: &Path) -> Paths {
        let paths = Paths {
            runtime_root: root.join("runtime"),
            state_root: root.join("state"),
            config_file: root.join("config.toml"),
        };
        paths.ensure().unwrap();
        paths
    }

    fn test_message(workspace: &Path, id: Uuid) -> MessageEnvelope {
        MessageEnvelope {
            schema_version: MESSAGE_SCHEMA_VERSION,
            id,
            workspace: workspace.to_path_buf(),
            created_at: now_secs(),
            from: MessageFrom::anonymous(),
            to: Recipient::Broadcast { broadcast: true },
            kind: "note".into(),
            reply_to: None,
            body: "test".into(),
            data: None,
            delivery: Delivery::Inbox,
        }
    }

    fn write_test_message(paths: &Paths, workspace: &Path, id: Uuid) {
        write_message(paths, &test_message(workspace, id)).unwrap();
    }

    fn create_legacy_mailbox(paths: &Paths, workspace: &Path) -> MessagePaths {
        let legacy = message_paths_for_key(paths, &legacy_workspace_key(workspace));
        ensure_private_dir(&legacy.workspace_dir).unwrap();
        ensure_private_dir(&legacy.msgs_dir).unwrap();
        ensure_private_dir(&legacy.cursors_dir).unwrap();
        atomic_write_json(
            &legacy.workspace_file,
            &serde_json::json!({"workspace": workspace}),
        )
        .unwrap();
        legacy
    }

    fn write_message_file(mp: &MessagePaths, message: &MessageEnvelope) {
        atomic_write_bytes(
            &mp.msgs_dir.join(format!("{}.json", message.id)),
            &serialized_envelope(message).unwrap(),
        )
        .unwrap();
    }

    fn set_modified_secs(path: &Path, seconds: u64) {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .unwrap();
        file.set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
            .unwrap();
    }

    #[test]
    fn workspace_key_stable_and_distinct() {
        let a = workspace_key(Path::new("/home/alexey/git/pocketshell"));
        let b = workspace_key(Path::new("/home/alexey/git/pocketshell"));
        let c = workspace_key(Path::new("/home/alexey/git/other"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 32);
        assert_eq!(a, "9c3c95a47c6557b18956e6903a57497f");
    }

    #[test]
    fn workspace_key_hashes_raw_unix_path_bytes() {
        let a = PathBuf::from(OsString::from_vec(b"/tmp/aplexer-\x80".to_vec()));
        let b = PathBuf::from(OsString::from_vec(b"/tmp/aplexer-\x81".to_vec()));
        assert_eq!(a.to_string_lossy(), b.to_string_lossy());
        assert_ne!(workspace_key(&a), workspace_key(&b));
    }

    #[test]
    fn ensure_workspace_migrates_valid_legacy_mailbox() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-legacy-workspace");
        let legacy = message_paths_for_key(&paths, &legacy_workspace_key(workspace));
        ensure_private_dir(&legacy.workspace_dir).unwrap();
        ensure_private_dir(&legacy.msgs_dir).unwrap();
        ensure_private_dir(&legacy.cursors_dir).unwrap();
        atomic_write_json(
            &legacy.workspace_file,
            &serde_json::json!({"workspace": workspace}),
        )
        .unwrap();
        fs::write(legacy.msgs_dir.join("migration-marker"), b"present").unwrap();

        let migrated = ensure_workspace(&paths, workspace).unwrap();
        assert_eq!(
            migrated.workspace_dir,
            message_paths(&paths, workspace).workspace_dir
        );
        assert!(migrated.msgs_dir.join("migration-marker").exists());
        assert!(!legacy.workspace_dir.exists());
    }

    #[test]
    fn ensure_workspace_losslessly_merges_coexisting_mailboxes() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-coexisting-mailboxes");
        let stable = ensure_workspace(&paths, workspace).unwrap();
        let legacy = create_legacy_mailbox(&paths, workspace);
        let duplicate_id = Uuid::from_u128(1);
        let stable_only_id = Uuid::from_u128(2);
        let legacy_only_id = Uuid::from_u128(3);
        let duplicate = test_message(workspace, duplicate_id);
        write_message_file(&stable, &duplicate);
        write_message_file(&legacy, &duplicate);
        write_message_file(&stable, &test_message(workspace, stable_only_id));
        write_message_file(&legacy, &test_message(workspace, legacy_only_id));

        let consumer_id = Uuid::from_u128(100);
        atomic_write_json(
            &stable.cursors_dir.join(format!("{consumer_id}.json")),
            &Cursor {
                acked_through: None,
                exceptions: BTreeSet::from([stable_only_id]),
            },
        )
        .unwrap();
        atomic_write_json(
            &legacy.cursors_dir.join(format!("{consumer_id}.json")),
            &Cursor {
                acked_through: None,
                exceptions: BTreeSet::from([legacy_only_id]),
            },
        )
        .unwrap();
        fs::write(cursor_lock_path(&legacy.cursors_dir, consumer_id), b"").unwrap();

        let merged = ensure_workspace(&paths, workspace).unwrap();
        assert_eq!(merged.workspace_dir, stable.workspace_dir);
        let messages = list_messages(&paths, workspace).unwrap();
        assert_eq!(
            messages
                .iter()
                .map(|message| message.id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([duplicate_id, stable_only_id, legacy_only_id])
        );
        let cursor = read_cursor(&paths, workspace, consumer_id).unwrap();
        assert_eq!(
            cursor.exceptions,
            BTreeSet::from([stable_only_id, legacy_only_id])
        );
        assert!(json_files(&legacy.msgs_dir).unwrap().is_empty());
        assert!(json_files(&legacy.cursors_dir).unwrap().is_empty());
        assert!(!cursor_lock_path(&legacy.cursors_dir, consumer_id).exists());

        // The intentionally-retained compatibility skeleton makes future
        // calls idempotent and lets current clients drain a later old-client
        // append instead of ignoring it.
        ensure_workspace(&paths, workspace).unwrap();
        write_message_file(&legacy, &test_message(workspace, Uuid::from_u128(4)));
        ensure_workspace(&paths, workspace).unwrap();
        assert_eq!(list_messages(&paths, workspace).unwrap().len(), 4);
    }

    #[test]
    fn legacy_merge_rejects_special_file_cursor_collisions_without_blocking() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-mailbox-special-cursor-collision");
        let stable = ensure_workspace(&paths, workspace).unwrap();
        let legacy = create_legacy_mailbox(&paths, workspace);
        let consumer_id = Uuid::from_u128(101);
        let cursor_name = format!("{consumer_id}.json");
        atomic_write_json(&legacy.cursors_dir.join(&cursor_name), &Cursor::default()).unwrap();

        let fifo_path = root.path().join("cursor-fifo");
        let fifo = std::ffi::CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        symlink(&fifo_path, stable.cursors_dir.join(&cursor_name)).unwrap();

        let error = ensure_workspace(&paths, workspace)
            .expect_err("legacy merge must reject a special-file cursor collision");
        assert!(
            format!("{error:#}").contains("open mailbox cursor"),
            "unexpected error: {error:#}"
        );
        assert!(legacy.cursors_dir.join(cursor_name).exists());
    }

    #[test]
    fn legacy_merge_validates_unique_sources_before_moving_any_file() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-mailbox-invalid-unique-legacy-source");
        let stable = ensure_workspace(&paths, workspace).unwrap();
        let legacy = create_legacy_mailbox(&paths, workspace);
        let valid_id = Uuid::from_u128(102);
        let oversized_id = Uuid::from_u128(103);
        let valid_path = legacy.cursors_dir.join(format!("{valid_id}.json"));
        let oversized_path = legacy.cursors_dir.join(format!("{oversized_id}.json"));
        atomic_write_json(&valid_path, &Cursor::default()).unwrap();
        let oversized = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&oversized_path)
            .unwrap();
        oversized
            .set_len(MAX_MAILBOX_STATE_BYTES as u64 + 1)
            .unwrap();
        drop(oversized);

        let error = ensure_workspace(&paths, workspace)
            .expect_err("unique oversized legacy cursor must fail preflight");
        assert!(
            format!("{error:#}").contains("exceeds the"),
            "unexpected error: {error:#}"
        );
        assert!(
            valid_path.exists(),
            "preflight must not move an earlier file"
        );
        assert!(
            oversized_path.exists(),
            "invalid source must remain recoverable"
        );
        assert!(!stable.cursors_dir.join(format!("{valid_id}.json")).exists());
        assert!(!stable
            .cursors_dir
            .join(format!("{oversized_id}.json"))
            .exists());
    }

    #[test]
    fn ensure_workspace_rejects_divergent_message_collision_without_mutation() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-mailbox-collision");
        let stable = ensure_workspace(&paths, workspace).unwrap();
        let legacy = create_legacy_mailbox(&paths, workspace);
        let id = Uuid::from_u128(1);
        let legacy_unique_id = Uuid::from_u128(2);
        let stable_message = test_message(workspace, id);
        let mut legacy_message = stable_message.clone();
        legacy_message.body = "different".into();
        write_message_file(&stable, &stable_message);
        write_message_file(&legacy, &legacy_message);
        write_message_file(&legacy, &test_message(workspace, legacy_unique_id));

        let error = ensure_workspace(&paths, workspace).unwrap_err();
        assert!(error.to_string().contains("mailbox message collision"));
        assert_eq!(
            fs::read(stable.msgs_dir.join(format!("{id}.json"))).unwrap(),
            serialized_envelope(&stable_message).unwrap()
        );
        assert_eq!(
            fs::read(legacy.msgs_dir.join(format!("{id}.json"))).unwrap(),
            serialized_envelope(&legacy_message).unwrap()
        );
        assert!(legacy
            .msgs_dir
            .join(format!("{legacy_unique_id}.json"))
            .exists());
        assert!(!stable
            .msgs_dir
            .join(format!("{legacy_unique_id}.json"))
            .exists());
    }

    #[test]
    fn recipient_shapes_round_trip() {
        let tag = Recipient::Tag {
            tag: "review".into(),
            session_id: None,
        };
        let json = serde_json::to_value(&tag).unwrap();
        assert_eq!(json, serde_json::json!({"tag": "review"}));
        let broadcast = Recipient::Broadcast { broadcast: true };
        assert_eq!(
            serde_json::to_value(&broadcast).unwrap(),
            serde_json::json!({"broadcast": true})
        );
        let engine = Recipient::Engine {
            engine: "codex".into(),
        };
        assert_eq!(
            serde_json::to_value(&engine).unwrap(),
            serde_json::json!({"engine": "codex"})
        );
        let back: Recipient = serde_json::from_value(json).unwrap();
        matches!(back, Recipient::Tag { .. });
    }

    #[test]
    fn cursor_tracks_acks() {
        let mut cursor = Cursor::default();
        let id1 = Uuid::now_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id2 = Uuid::now_v7();
        assert!(!cursor.is_acked(id1));
        cursor.exceptions.insert(id2);
        assert!(cursor.is_acked(id2));
        assert!(!cursor.is_acked(id1));
        cursor.acked_through = Some(id2);
        assert!(cursor.is_acked(id1));
        assert!(cursor.is_acked(id2));
    }

    #[test]
    fn corrupt_cursor_fails_without_reset_or_ack_overwrite() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-corrupt-cursor-workspace");
        let consumer_id = Uuid::from_u128(100);
        let message_id = Uuid::from_u128(1);
        write_test_message(&paths, workspace, message_id);
        let mp = ensure_workspace(&paths, workspace).unwrap();
        let cursor_path = mp.cursors_dir.join(format!("{consumer_id}.json"));
        let corrupt = b"{\"exceptions\":";
        fs::write(&cursor_path, corrupt).unwrap();

        let read_error = read_cursor(&paths, workspace, consumer_id)
            .expect_err("a corrupt cursor must not be interpreted as empty");
        assert!(
            format!("{read_error:#}").contains("parse mailbox cursor"),
            "unexpected error: {read_error:#}"
        );
        assert_eq!(fs::read(&cursor_path).unwrap(), corrupt);

        let ack_error = ack_messages(&paths, workspace, consumer_id, &[message_id])
            .expect_err("ack must not overwrite a corrupt cursor from an empty default");
        assert!(
            format!("{ack_error:#}").contains("parse mailbox cursor"),
            "unexpected error: {ack_error:#}"
        );
        assert_eq!(fs::read(&cursor_path).unwrap(), corrupt);
    }

    #[test]
    fn workspace_and_cursor_state_reject_special_and_oversized_files() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-mailbox-state-types-workspace");
        let mp = ensure_workspace(&paths, workspace).unwrap();

        fs::remove_file(&mp.workspace_file).unwrap();
        let fifo = std::ffi::CString::new(mp.workspace_file.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let metadata_error = verify_workspace_metadata(&mp.workspace_dir, workspace)
            .expect_err("workspace metadata FIFO must fail without blocking");
        assert!(
            format!("{metadata_error:#}").contains("not a regular file"),
            "unexpected error: {metadata_error:#}"
        );
        fs::remove_file(&mp.workspace_file).unwrap();
        atomic_write_json(
            &mp.workspace_file,
            &serde_json::json!({"workspace": workspace}),
        )
        .unwrap();

        let cursor_path = mp.cursors_dir.join(format!("{}.json", Uuid::from_u128(7)));
        let outside = root.path().join("outside-cursor.json");
        fs::write(&outside, b"{}").unwrap();
        symlink(&outside, &cursor_path).unwrap();
        let symlink_error =
            read_cursor_file(&cursor_path).expect_err("cursor symlink must not be followed");
        assert!(
            format!("{symlink_error:#}").contains("open mailbox cursor"),
            "unexpected error: {symlink_error:#}"
        );
        fs::remove_file(&cursor_path).unwrap();

        let oversized = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&cursor_path)
            .unwrap();
        oversized
            .set_len(MAX_MAILBOX_STATE_BYTES as u64 + 1)
            .unwrap();
        drop(oversized);
        let size_error = read_cursor_file(&cursor_path)
            .expect_err("oversized cursor must be rejected before reading");
        assert!(
            format!("{size_error:#}").contains("exceeds the"),
            "unexpected error: {size_error:#}"
        );
    }

    #[test]
    fn cursor_compaction_migrates_legacy_watermark_to_exact_ids() {
        let id1 = Uuid::from_u128(1);
        let id2 = Uuid::from_u128(2);
        let id3 = Uuid::from_u128(3);
        let retained = BTreeSet::from([id1, id2, id3]);
        let mut cursor = Cursor {
            acked_through: Some(id2),
            exceptions: BTreeSet::from([id3]),
        };

        compact_cursor(&mut cursor, &retained);
        assert_eq!(cursor.acked_through, None);
        assert_eq!(cursor.exceptions, retained);
    }

    #[test]
    fn exact_acks_do_not_hide_a_lower_id_committed_later() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-delayed-message-workspace");
        let consumer_id = Uuid::from_u128(100);
        let lower = Uuid::from_u128(1);
        let higher = Uuid::from_u128(2);

        write_test_message(&paths, workspace, higher);
        ack_messages(&paths, workspace, consumer_id, &[higher]).unwrap();
        let before = read_cursor(&paths, workspace, consumer_id).unwrap();
        assert_eq!(before.acked_through, None);
        assert!(before.is_acked(higher));

        // This is the problematic interleaving: an id generated earlier is
        // committed only after the later id was acknowledged.
        write_test_message(&paths, workspace, lower);
        let after = read_cursor(&paths, workspace, consumer_id).unwrap();
        assert!(after.is_acked(higher));
        assert!(!after.is_acked(lower));
    }

    #[test]
    fn ack_waits_for_the_per_consumer_lock() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = PathBuf::from("/tmp/aplexer-ack-lock-workspace");
        let consumer_id = Uuid::new_v4();
        let message_id = Uuid::now_v7();
        write_test_message(&paths, &workspace, message_id);
        let mp = ensure_workspace(&paths, &workspace).unwrap();
        let lock =
            FileLock::exclusive(&cursor_lock_path(&mp.cursors_dir, consumer_id), false).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_paths = paths.clone();
        let thread_workspace = workspace.clone();
        let worker = std::thread::spawn(move || {
            tx.send(ack_messages(
                &thread_paths,
                &thread_workspace,
                consumer_id,
                &[message_id],
            ))
            .unwrap();
        });

        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
        drop(lock);
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
        assert!(read_cursor(&paths, &workspace, consumer_id)
            .unwrap()
            .is_acked(message_id));
    }

    #[test]
    fn gc_discards_exceptions_for_messages_no_longer_retained() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-cursor-gc-workspace");
        let consumer_id = Uuid::new_v4();
        let first = Uuid::now_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = Uuid::now_v7();
        write_test_message(&paths, workspace, first);
        write_test_message(&paths, workspace, second);
        ack_messages(&paths, workspace, consumer_id, &[second]).unwrap();
        let before = read_cursor(&paths, workspace, consumer_id).unwrap();
        assert_eq!(before.exceptions, BTreeSet::from([second]));

        fs::remove_file(
            message_paths(&paths, workspace)
                .msgs_dir
                .join(format!("{second}.json")),
        )
        .unwrap();
        gc_workspace(&paths, workspace).unwrap();

        let after = read_cursor(&paths, workspace, consumer_id).unwrap();
        assert!(after.exceptions.is_empty());
        assert!(!after.is_acked(first));
    }

    #[test]
    fn cursor_gc_respects_retention_active_sessions_and_held_locks() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-stale-cursor-workspace");
        let mp = ensure_workspace(&paths, workspace).unwrap();
        let stale = Uuid::from_u128(10);
        let active = Uuid::from_u128(11);
        let fresh = Uuid::from_u128(12);
        let busy = Uuid::from_u128(13);
        let orphan_stale = Uuid::from_u128(14);
        let orphan_fresh = Uuid::from_u128(15);
        for consumer_id in [stale, active, fresh, busy] {
            atomic_write_json(
                &mp.cursors_dir.join(format!("{consumer_id}.json")),
                &Cursor::default(),
            )
            .unwrap();
        }
        for consumer_id in [stale, active, fresh, busy, orphan_stale, orphan_fresh] {
            fs::write(cursor_lock_path(&mp.cursors_dir, consumer_id), b"").unwrap();
        }

        let now = STALE_CURSOR_RETENTION_SECS + 10_000;
        let old = 1;
        let fresh_at = now - STALE_CURSOR_RETENTION_SECS;
        for consumer_id in [stale, active, busy] {
            set_modified_secs(&mp.cursors_dir.join(format!("{consumer_id}.json")), old);
            set_modified_secs(&cursor_lock_path(&mp.cursors_dir, consumer_id), old);
        }
        set_modified_secs(&mp.cursors_dir.join(format!("{fresh}.json")), fresh_at);
        set_modified_secs(&cursor_lock_path(&mp.cursors_dir, fresh), fresh_at);
        set_modified_secs(&cursor_lock_path(&mp.cursors_dir, orphan_stale), old);
        set_modified_secs(&cursor_lock_path(&mp.cursors_dir, orphan_fresh), fresh_at);

        let busy_lock =
            FileLock::exclusive(&cursor_lock_path(&mp.cursors_dir, busy), false).unwrap();
        let _mailbox = FileLock::exclusive(&mailbox_lock_path(&mp), false).unwrap();
        maintain_workspace_cursors_locked(
            &mp,
            &BTreeSet::from([active]),
            now,
            STALE_CURSOR_RETENTION_SECS,
        )
        .unwrap();

        assert!(!mp.cursors_dir.join(format!("{stale}.json")).exists());
        assert!(!cursor_lock_path(&mp.cursors_dir, stale).exists());
        assert!(mp.cursors_dir.join(format!("{active}.json")).exists());
        assert!(mp.cursors_dir.join(format!("{fresh}.json")).exists());
        assert!(mp.cursors_dir.join(format!("{busy}.json")).exists());
        assert!(!cursor_lock_path(&mp.cursors_dir, orphan_stale).exists());
        assert!(cursor_lock_path(&mp.cursors_dir, orphan_fresh).exists());

        drop(busy_lock);
        maintain_workspace_cursors_locked(
            &mp,
            &BTreeSet::from([active]),
            now,
            STALE_CURSOR_RETENTION_SECS,
        )
        .unwrap();
        assert!(!mp.cursors_dir.join(format!("{busy}.json")).exists());
        assert!(!cursor_lock_path(&mp.cursors_dir, busy).exists());
    }

    #[test]
    fn body_size_cap_rejects_oversized() {
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(check_body_size(&big).is_err());
        let ok = "x".repeat(MAX_BODY_BYTES);
        assert!(check_body_size(&ok).is_ok());
    }

    #[test]
    fn serialized_envelope_cap_rejects_oversized_structured_data() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-envelope-cap-workspace");
        let mut message = test_message(workspace, Uuid::from_u128(1));
        message.body = "tiny".into();
        message.data = Some(serde_json::json!({
            "blob": "x".repeat(MAX_ENVELOPE_BYTES)
        }));

        let error = write_message(&paths, &message).unwrap_err();
        assert!(error.to_string().contains("serialized message envelope"));
        assert!(!message_paths(&paths, workspace)
            .msgs_dir
            .join(format!("{}.json", message.id))
            .exists());
    }

    #[test]
    fn message_loader_validates_schema_filename_id_and_workspace() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-message-invariants-workspace");
        let mp = ensure_workspace(&paths, workspace).unwrap();
        let filename_id = Uuid::from_u128(1);
        let path = mp.msgs_dir.join(format!("{filename_id}.json"));

        let mut message = test_message(workspace, filename_id);
        message.schema_version = MESSAGE_SCHEMA_VERSION + 1;
        atomic_write_bytes(&path, &serialized_envelope(&message).unwrap()).unwrap();
        let schema_error = read_message(&paths, workspace, filename_id)
            .expect_err("an unsupported message schema must fail closed");
        assert!(
            format!("{schema_error:#}").contains("unsupported mailbox message schema"),
            "unexpected error: {schema_error:#}"
        );

        message.schema_version = MESSAGE_SCHEMA_VERSION;
        message.id = Uuid::from_u128(2);
        atomic_write_bytes(&path, &serialized_envelope(&message).unwrap()).unwrap();
        let id_error = read_message(&paths, workspace, filename_id)
            .expect_err("the envelope id must match its filename");
        assert!(
            format!("{id_error:#}").contains("does not match filename id"),
            "unexpected error: {id_error:#}"
        );

        message.id = filename_id;
        message.workspace = PathBuf::from("/tmp/a-different-mailbox-workspace");
        atomic_write_bytes(&path, &serialized_envelope(&message).unwrap()).unwrap();
        let workspace_error = list_messages(&paths, workspace)
            .expect_err("an envelope from another workspace must fail closed");
        assert!(
            format!("{workspace_error:#}").contains("belongs to workspace"),
            "unexpected error: {workspace_error:#}"
        );
    }

    #[test]
    fn message_loader_rejects_symlink_non_regular_and_oversized_entries() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-message-file-type-workspace");
        let mp = ensure_workspace(&paths, workspace).unwrap();
        let id = Uuid::from_u128(1);
        let path = mp.msgs_dir.join(format!("{id}.json"));
        let outside = root.path().join("outside-message.json");
        fs::write(
            &outside,
            serialized_envelope(&test_message(workspace, id)).unwrap(),
        )
        .unwrap();
        symlink(&outside, &path).unwrap();

        let symlink_error =
            list_messages(&paths, workspace).expect_err("a mailbox symlink must not be followed");
        assert!(
            format!("{symlink_error:#}").contains("open mailbox message"),
            "unexpected error: {symlink_error:#}"
        );
        let cursor_error = read_cursor(&paths, workspace, Uuid::from_u128(100))
            .expect_err("cursor maintenance must not retain a symlink by filename alone");
        assert!(
            format!("{cursor_error:#}").contains("open mailbox message"),
            "unexpected error: {cursor_error:#}"
        );
        fs::remove_file(&path).unwrap();

        fs::create_dir(&path).unwrap();
        let type_error = list_messages(&paths, workspace)
            .expect_err("a non-regular mailbox entry must be rejected");
        assert!(
            format!("{type_error:#}").contains("is not a regular file"),
            "unexpected error: {type_error:#}"
        );
        fs::remove_dir(&path).unwrap();

        let oversized = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        oversized.set_len(MAX_ENVELOPE_BYTES as u64 + 1).unwrap();
        drop(oversized);
        let size_error = list_messages(&paths, workspace)
            .expect_err("an oversized pre-existing envelope must be rejected before reading");
        assert!(
            format!("{size_error:#}").contains("envelope cap"),
            "unexpected error: {size_error:#}"
        );
    }

    #[test]
    fn append_enforces_message_count_and_keeps_the_new_message() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-count-cap-workspace");
        let first = test_message(workspace, Uuid::from_u128(1));
        let second = test_message(workspace, Uuid::from_u128(2));
        let delayed_lower = test_message(workspace, Uuid::from_u128(0));

        write_message_with_limits(&paths, &first, 2, MAX_WORKSPACE_BYTES).unwrap();
        write_message_with_limits(&paths, &second, 2, MAX_WORKSPACE_BYTES).unwrap();
        write_message_with_limits(&paths, &delayed_lower, 2, MAX_WORKSPACE_BYTES).unwrap();

        let retained = list_messages(&paths, workspace).unwrap();
        assert_eq!(retained.len(), 2);
        assert!(retained
            .iter()
            .any(|message| message.id == delayed_lower.id));
        assert!(!retained.iter().any(|message| message.id == first.id));
    }

    #[test]
    fn append_enforces_workspace_byte_cap() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-byte-cap-workspace");
        let first = test_message(workspace, Uuid::from_u128(1));
        let mut second = test_message(workspace, Uuid::from_u128(2));
        second.body = "second".into();
        let second_size = serialized_envelope(&second).unwrap().len() as u64;

        write_message_with_limits(&paths, &first, 10, second_size).unwrap();
        write_message_with_limits(&paths, &second, 10, second_size).unwrap();

        let retained = list_messages(&paths, workspace).unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].id, second.id);
    }

    #[test]
    fn append_rolls_back_when_quota_cannot_retain_the_new_message() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = Path::new("/tmp/aplexer-impossible-cap-workspace");
        let message = test_message(workspace, Uuid::from_u128(1));

        let error = write_message_with_limits(&paths, &message, 0, MAX_WORKSPACE_BYTES)
            .expect_err("a zero-message quota must reject the append");

        assert!(error.to_string().contains("enforce mailbox quota"));
        assert!(list_messages(&paths, workspace).unwrap().is_empty());
    }

    #[test]
    fn append_waits_for_the_workspace_mailbox_lock() {
        let root = TempDir::new().unwrap();
        let paths = test_paths(root.path());
        let workspace = PathBuf::from("/tmp/aplexer-append-lock-workspace");
        let mp = ensure_workspace(&paths, &workspace).unwrap();
        let lock = FileLock::exclusive(&mailbox_lock_path(&mp), false).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_paths = paths.clone();
        let thread_workspace = workspace.clone();
        let worker = std::thread::spawn(move || {
            tx.send(write_message(
                &thread_paths,
                &test_message(&thread_workspace, Uuid::from_u128(1)),
            ))
            .unwrap();
        });

        assert!(rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err());
        drop(lock);
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
    }
}
