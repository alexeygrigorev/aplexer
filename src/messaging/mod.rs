//! Inter-agent messaging channel (docs/inter-agent-messaging-design.md).
//!
//! Storage model (design doc section 3.2): one file per message under
//! `${state_root}/messages/<workspace-key>/{workspace.json, msgs/<uuid>.json,
//! cursors/<consumer-id>.json}`, written with the same atomic-write
//! discipline (temp file + fsync + rename) the rest of aplexer already uses
//! for session metadata (spec.md 14.1). No process owns this state; any
//! process may read, append, or prune it.
//!
//! Layout: `layout` names and creates a workspace mailbox, `migrate` drains
//! a pre-SHA-256 mailbox into it, `envelope` is the wire format, `store`
//! appends and reads messages, `cursor` tracks per-consumer acknowledgements,
//! `gc` prunes, and `identity` resolves who is sending or reading.

mod cursor;
mod envelope;
mod gc;
mod identity;
mod layout;
mod migrate;
mod store;
#[cfg(test)]
mod tests;

use crate::history::hex_encode;
use crate::persist::{read_bounded_json, read_bounded_regular_file};
use crate::{
    atomic_write_bytes, atomic_write_json, ensure_private_dir, list_records, now_ms, FileLock,
    Paths, SessionRecord,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io;
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

pub use cursor::*;
pub use envelope::*;
pub use gc::*;
pub use identity::*;
pub use layout::*;
pub(crate) use migrate::*;
pub use store::*;
