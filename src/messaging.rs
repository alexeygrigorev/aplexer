//! Inter-agent messaging channel (docs/inter-agent-messaging-design.md).
//!
//! Storage model (design doc section 3.2): one file per message under
//! `${state_root}/messages/<workspace-key>/{workspace.json, msgs/<uuid>.json,
//! cursors/<consumer-id>.json}`, written with the same atomic-write
//! discipline (temp file + fsync + rename) the rest of aplexer already uses
//! for session metadata (spec.md 14.1). No process owns this state; any
//! process may read, append, or prune it.

use crate::{atomic_write_json, ensure_private_dir, list_records, now_ms, FileLock, Paths};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::fs;
use std::hash::{Hash, Hasher};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const MESSAGE_SCHEMA_VERSION: u32 = 1;
/// Design doc section 5: "body ... size-capped (e.g. 64 KB)".
pub const MAX_BODY_BYTES: usize = 64 * 1024;
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

/// Verifies the reverse mapping before adopting a directory found under a
/// legacy key. The old key was based on lossy UTF-8 and therefore could
/// alias two distinct Unix paths; silently renaming without this check could
/// expose another workspace's messages.
fn verify_workspace_metadata(workspace_dir: &Path, canonical_workspace: &Path) -> Result<()> {
    let metadata_path = workspace_dir.join("workspace.json");
    let bytes = fs::read(&metadata_path)
        .with_context(|| format!("read legacy mailbox metadata {}", metadata_path.display()))?;
    let metadata: WorkspaceMetadata = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse legacy mailbox metadata {}", metadata_path.display()))?;
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

    // Serialize discovery and rename for this destination. Atomic rename
    // makes the mailbox contents move as a unit; the lock also prevents two
    // current processes from racing and treating a successful peer migration
    // as a missing source.
    let migration_lock = messages_root.join(format!(".{stable_key}.migration.lock"));
    let _migration = FileLock::exclusive(&migration_lock, false)?;
    if !mp.workspace_dir.exists()
        && legacy_mp.workspace_dir != mp.workspace_dir
        && legacy_mp.workspace_dir.exists()
    {
        verify_workspace_metadata(&legacy_mp.workspace_dir, canonical_workspace)?;
        fs::rename(&legacy_mp.workspace_dir, &mp.workspace_dir).with_context(|| {
            format!(
                "migrate legacy mailbox {} to {}",
                legacy_mp.workspace_dir.display(),
                mp.workspace_dir.display()
            )
        })?;
    }

    ensure_private_dir(&mp.workspace_dir)?;
    ensure_private_dir(&mp.msgs_dir)?;
    ensure_private_dir(&mp.cursors_dir)?;
    if !mp.workspace_file.exists() {
        atomic_write_json(
            &mp.workspace_file,
            &serde_json::json!({"workspace": canonical_workspace}),
        )?;
    }
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

/// Writes one message file via the crate's standard atomic-write-json
/// discipline (temp file + fsync + rename, spec.md 14.1), named by the
/// message's own UUIDv7 id so lexical directory order is chronological
/// order (design doc section 3.2/4).
pub fn write_message(paths: &Paths, envelope: &MessageEnvelope) -> Result<()> {
    check_body_size(&envelope.body)?;
    let mp = ensure_workspace(paths, &envelope.workspace)?;
    let path = mp.msgs_dir.join(format!("{}.json", envelope.id));
    atomic_write_json(&path, envelope)
}

pub fn read_message(
    paths: &Paths,
    canonical_workspace: &Path,
    id: Uuid,
) -> Result<MessageEnvelope> {
    let mp = ensure_workspace(paths, canonical_workspace)?;
    let path = mp.msgs_dir.join(format!("{id}.json"));
    let bytes = fs::read(&path).with_context(|| format!("no such message {id}"))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

/// Lists every message currently on disk for a workspace, in id (= time)
/// order. Files that fail to parse (should not happen given atomic writes,
/// but a partial/corrupt file must never wedge every other read) are
/// silently skipped rather than failing the whole listing.
pub fn list_messages(paths: &Paths, canonical_workspace: &Path) -> Result<Vec<MessageEnvelope>> {
    let mp = ensure_workspace(paths, canonical_workspace)?;
    let mut out = Vec::new();
    let entries = match fs::read_dir(&mp.msgs_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(bytes) = fs::read(&path) {
            if let Ok(envelope) = serde_json::from_slice::<MessageEnvelope>(&bytes) {
                out.push(envelope);
            }
        }
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

/// Per-consumer read/ack state (design doc section 3.2): "a cursor file
/// records the consumer's last-acked message id plus optional per-id ack
/// exceptions" -- `acked_through` is the common in-order case (ack advances
/// a single high-water mark), `exceptions` holds ids acked out of order
/// that are not yet subsumed by `acked_through`. Updates use a per-consumer
/// advisory lock because multiple CLI processes can act for one session at
/// the same time; the cursor itself is still replaced atomically.
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
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cursor::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn retained_message_ids(msgs_dir: &Path) -> Result<BTreeSet<Uuid>> {
    let entries = match fs::read_dir(msgs_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", msgs_dir.display())),
    };
    let mut ids = BTreeSet::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
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
    read_cursor_file(&mp.cursors_dir.join(format!("{consumer_id}.json")))
}

/// Promotes an acknowledged prefix into the high-water mark and discards
/// exceptions for messages no longer retained. Promotion only crosses ids
/// that are explicitly acknowledged, so an out-of-order ack never hides an
/// earlier unread message. As GC removes old messages, stale exceptions are
/// discarded and the set stays bounded by the mailbox itself.
fn compact_cursor(cursor: &mut Cursor, retained_ids: &BTreeSet<Uuid>) {
    cursor.exceptions.retain(|id| {
        retained_ids.contains(id)
            && cursor
                .acked_through
                .map(|through| *id > through)
                .unwrap_or(true)
    });

    let mut through = cursor.acked_through;
    for id in retained_ids {
        if through.map(|value| *id <= value).unwrap_or(false) {
            continue;
        }
        if cursor.exceptions.remove(id) {
            through = Some(*id);
        } else {
            break;
        }
    }
    cursor.acked_through = through;
}

/// Records `ids` as acked for `consumer_id`. Not required to be called with
/// a contiguous prefix -- out-of-order ids remain in `exceptions` until the
/// preceding retained messages are acked or pruned. The read-modify-write is
/// serialized per consumer so concurrent acknowledgements cannot overwrite
/// each other.
pub fn ack_messages(
    paths: &Paths,
    canonical_workspace: &Path,
    consumer_id: Uuid,
    ids: &[Uuid],
) -> Result<()> {
    let mp = ensure_workspace(paths, canonical_workspace)?;
    let path = mp.cursors_dir.join(format!("{consumer_id}.json"));
    let _lock = FileLock::exclusive(&cursor_lock_path(&mp.cursors_dir, consumer_id), false)?;
    let mut cursor = read_cursor_file(&path)?;
    for id in ids {
        if !cursor.is_acked(*id) {
            cursor.exceptions.insert(*id);
        }
    }
    let retained_ids = retained_message_ids(&mp.msgs_dir)?;
    compact_cursor(&mut cursor, &retained_ids);
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

fn compact_workspace_cursors(mp: &MessagePaths) -> Result<()> {
    let entries = match fs::read_dir(&mp.cursors_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", mp.cursors_dir.display())),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Some(consumer_id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| Uuid::parse_str(stem).ok())
        else {
            continue;
        };
        let _lock = FileLock::exclusive(&cursor_lock_path(&mp.cursors_dir, consumer_id), false)?;
        let mut cursor = read_cursor_file(&path)?;
        let original = cursor.clone();
        // Take the mailbox snapshot after acquiring this cursor's lock. An
        // acknowledgement racing GC must either be visible in this read or
        // run after the compacted cursor is committed; it cannot be silently
        // discarded as an "absent" exception.
        let retained_ids = retained_message_ids(&mp.msgs_dir)?;
        compact_cursor(&mut cursor, &retained_ids);
        if cursor != original {
            atomic_write_json(&path, &cursor)?;
        }
    }
    Ok(())
}

/// Prunes a workspace mailbox per design doc section 4: default 7-day TTL,
/// then a per-workspace cap (~1000 messages / 10 MB) as backstop, oldest
/// first. Unlinking a message file is always safe from any process --
/// idempotent, no lock, no owner (section 3.2).
pub fn gc_workspace(paths: &Paths, canonical_workspace: &Path) -> Result<GcReport> {
    let mp = ensure_workspace(paths, canonical_workspace)?;
    struct Entry {
        path: PathBuf,
        created_at: u64,
        size: u64,
    }
    let mut entries = Vec::new();
    if let Ok(dir) = fs::read_dir(&mp.msgs_dir) {
        for e in dir.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Ok(envelope) = serde_json::from_slice::<MessageEnvelope>(&bytes) else {
                continue;
            };
            entries.push((
                envelope.id,
                Entry {
                    path,
                    created_at: envelope.created_at,
                    size: bytes.len() as u64,
                },
            ));
        }
    }
    entries.sort_by_key(|(id, _)| *id);
    let now = now_secs();
    let mut removed = 0usize;
    entries.retain(|(_, e)| {
        let expired = now.saturating_sub(e.created_at) > DEFAULT_TTL_SECS;
        if expired {
            match fs::remove_file(&e.path) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return true,
            }
        }
        !expired
    });
    let mut index = 0;
    while entries.len() > MAX_MESSAGES_PER_WORKSPACE && index < entries.len() {
        match fs::remove_file(&entries[index].1.path) {
            Ok(()) => {
                entries.remove(index);
                removed += 1;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                entries.remove(index);
            }
            Err(_) => index += 1,
        }
    }
    let mut total: u64 = entries.iter().map(|(_, e)| e.size).sum();
    index = 0;
    while total > MAX_WORKSPACE_BYTES && index < entries.len() {
        match fs::remove_file(&entries[index].1.path) {
            Ok(()) => {
                let (_, entry) = entries.remove(index);
                total = total.saturating_sub(entry.size);
                removed += 1;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let (_, entry) = entries.remove(index);
                total = total.saturating_sub(entry.size);
            }
            Err(_) => index += 1,
        }
    }
    compact_workspace_cursors(&mp)?;
    Ok(GcReport {
        removed,
        remaining: entries.len(),
    })
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
    use std::os::unix::ffi::OsStringExt;
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

    fn write_test_message(paths: &Paths, workspace: &Path, id: Uuid) {
        write_message(
            paths,
            &MessageEnvelope {
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
            },
        )
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
    fn cursor_compaction_promotes_only_an_acked_prefix() {
        let id1 = Uuid::now_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id2 = Uuid::now_v7();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id3 = Uuid::now_v7();
        let retained = BTreeSet::from([id1, id2, id3]);
        let mut cursor = Cursor {
            acked_through: None,
            exceptions: BTreeSet::from([id1, id3]),
        };

        compact_cursor(&mut cursor, &retained);
        assert_eq!(cursor.acked_through, Some(id1));
        assert_eq!(cursor.exceptions, BTreeSet::from([id3]));

        cursor.exceptions.insert(id2);
        compact_cursor(&mut cursor, &retained);
        assert_eq!(cursor.acked_through, Some(id3));
        assert!(cursor.exceptions.is_empty());
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
    fn body_size_cap_rejects_oversized() {
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(check_body_size(&big).is_err());
        let ok = "x".repeat(MAX_BODY_BYTES);
        assert!(check_body_size(&ok).is_ok());
    }
}
