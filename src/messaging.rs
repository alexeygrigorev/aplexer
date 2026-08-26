//! Inter-agent messaging channel (docs/inter-agent-messaging-design.md).
//!
//! Storage model (design doc section 3.2): one file per message under
//! `${state_root}/messages/<workspace-key>/{workspace.json, msgs/<uuid>.json,
//! cursors/<consumer-id>.json}`, written with the same atomic-write
//! discipline (temp file + fsync + rename) the rest of aplexer already uses
//! for session metadata (spec.md 14.1). No process owns this state; any
//! process may read, append, or prune it.

use crate::{atomic_write_json, ensure_private_dir, list_records, now_ms, Paths};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::collections::BTreeSet;
use std::fs;
use std::hash::{Hash, Hasher};
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
/// and open question 8). The design doc suggests a truncated SHA-256 of the
/// canonical workspace path; `sha2` is not otherwise a dependency of this
/// crate, and this key is not a security boundary (the containing directory
/// is already `0700`, owned by this uid, per spec.md 26) -- it only needs to
/// be a stable, collision-resistant *filename*. Rather than add a crypto
/// hash dependency for that, this combines two independently-seeded
/// `DefaultHasher` (SipHash-1-3) digests of the canonical path into a
/// 32-hex-digit (128-bit) key -- the same key space a truncated SHA-256
/// would give, at zero new dependency cost. Every caller MUST pass a path
/// already run through `canonical_workspace` (open question 8) so two
/// sessions in the same workspace can never straddle two mailboxes.
pub fn workspace_key(canonical_workspace: &Path) -> String {
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

pub fn message_paths(paths: &Paths, canonical_workspace: &Path) -> MessagePaths {
    let workspace_dir = paths
        .state_root
        .join("messages")
        .join(workspace_key(canonical_workspace));
    MessagePaths {
        msgs_dir: workspace_dir.join("msgs"),
        cursors_dir: workspace_dir.join("cursors"),
        workspace_file: workspace_dir.join("workspace.json"),
        workspace_dir,
    }
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
    let mp = message_paths(paths, canonical_workspace);
    ensure_private_dir(&paths.state_root.join("messages"))?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Inbox,
    Pane,
}
impl Default for Delivery {
    fn default() -> Self {
        Delivery::Inbox
    }
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

pub fn read_message(paths: &Paths, canonical_workspace: &Path, id: Uuid) -> Result<MessageEnvelope> {
    let mp = message_paths(paths, canonical_workspace);
    let path = mp.msgs_dir.join(format!("{id}.json"));
    let bytes = fs::read(&path).with_context(|| format!("no such message {id}"))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

/// Lists every message currently on disk for a workspace, in id (= time)
/// order. Files that fail to parse (should not happen given atomic writes,
/// but a partial/corrupt file must never wedge every other read) are
/// silently skipped rather than failing the whole listing.
pub fn list_messages(paths: &Paths, canonical_workspace: &Path) -> Result<Vec<MessageEnvelope>> {
    let mp = message_paths(paths, canonical_workspace);
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
/// that are not yet subsumed by `acked_through`. Each consumer writes only
/// its own cursor file (single-writer, atomic-rename, no contention).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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

fn cursor_path(paths: &Paths, canonical_workspace: &Path, consumer_id: Uuid) -> PathBuf {
    message_paths(paths, canonical_workspace)
        .cursors_dir
        .join(format!("{consumer_id}.json"))
}

pub fn read_cursor(paths: &Paths, canonical_workspace: &Path, consumer_id: Uuid) -> Result<Cursor> {
    let path = cursor_path(paths, canonical_workspace, consumer_id);
    match fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Cursor::default()),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

/// Records `ids` as acked for `consumer_id`. Not required to be called with
/// a contiguous prefix -- out-of-order ids simply accumulate in
/// `exceptions`; `a message gc` drops exceptions whose message no longer
/// exists once pruned, so this cannot grow without bound.
pub fn ack_messages(
    paths: &Paths,
    canonical_workspace: &Path,
    consumer_id: Uuid,
    ids: &[Uuid],
) -> Result<()> {
    ensure_workspace(paths, canonical_workspace)?;
    let mut cursor = read_cursor(paths, canonical_workspace, consumer_id)?;
    for id in ids {
        if !cursor.is_acked(*id) {
            cursor.exceptions.insert(*id);
        }
    }
    atomic_write_json(&cursor_path(paths, canonical_workspace, consumer_id), &cursor)
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
            let _ = fs::remove_file(&e.path);
            removed += 1;
        }
        !expired
    });
    while entries.len() > MAX_MESSAGES_PER_WORKSPACE {
        let (_, e) = entries.remove(0);
        let _ = fs::remove_file(&e.path);
        removed += 1;
    }
    let mut total: u64 = entries.iter().map(|(_, e)| e.size).sum();
    while total > MAX_WORKSPACE_BYTES && !entries.is_empty() {
        let (_, e) = entries.remove(0);
        total = total.saturating_sub(e.size);
        let _ = fs::remove_file(&e.path);
        removed += 1;
    }
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
pub fn resolve_sender(paths: &Paths, canonical_workspace: &Path, from_tag: Option<&str>) -> Result<MessageFrom> {
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
    if let Ok(raw_id) = std::env::var("APLEXER_SESSION_ID") {
        if let Ok(session_id) = raw_id.parse::<Uuid>() {
            if let Some(record) = list_records(paths)?.into_iter().find(|r| r.id == session_id) {
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
    if let Ok(raw_id) = std::env::var("APLEXER_SESSION_ID") {
        if let Ok(session_id) = raw_id.parse::<Uuid>() {
            let (tag, engine) = list_records(paths)?
                .into_iter()
                .find(|r| r.id == session_id)
                .map(|r| (r.tag, r.engine))
                .unwrap_or_else(|| (std::env::var("APLEXER_TAG").unwrap_or_default(), String::new()));
            return Ok((session_id, tag, engine));
        }
    }
    bail!(
        "no session identity: APLEXER_SESSION_ID is not set (you're not inside an aplexer \
         session) and no --from TAG was given"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_key_stable_and_distinct() {
        let a = workspace_key(Path::new("/home/alexey/git/pocketshell"));
        let b = workspace_key(Path::new("/home/alexey/git/pocketshell"));
        let c = workspace_key(Path::new("/home/alexey/git/other"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 32);
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
    fn body_size_cap_rejects_oversized() {
        let big = "x".repeat(MAX_BODY_BYTES + 1);
        assert!(check_body_size(&big).is_err());
        let ok = "x".repeat(MAX_BODY_BYTES);
        assert!(check_body_size(&ok).is_ok());
    }
}
