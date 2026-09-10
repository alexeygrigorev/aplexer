//! Parse an aplexer session's native engine conversation log into heru
//! `UnifiedEvent` JSONL (`a transcript`). PocketShell's conversation pane
//! is the intended consumer -- last-N for the initial view, `--before` for
//! older pages, `--after` plus `--follow` for live tail.
//!
//! This is a different layer from `a watch`: `a watch` derives coarse
//! host-level session-lifecycle events (created/exited/oom / a
//! running-vs-waiting heuristic) by polling `session.json`. It never looks
//! at what an agent is saying. This module parses the conversation the
//! agent CLI already writes to disk during an ordinary interactive
//! `a start`/`a attach` session.
//!
//! Capture and keep (deliberate, not a copy of the PTY history):
//!
//! - **Source of truth** is the engine's own append-only JSONL, not aplexer
//!   state. We do not duplicate conversation bytes under the session dir
//!   (that would go stale, double disk, and fight the engine's own log).
//!   PTY `history.bin` stays the raw terminal capture; this is the
//!   structured conversation.
//! - **Location** is a heuristic the first time: aplexer has the session's
//!   cwd + created_at, not the engine-native session id. Rules are ported
//!   from pocketshell's `agent_log.py` (`~/.claude/projects/<encoded-cwd>/`,
//!   `~/.codex/sessions/<Y>/<M>/<D>/`, `$GROK_HOME/sessions/<urlencoded-cwd>/`).
//! - **Bind**: once located, the path is written to
//!   `<state>/sessions/<id>/transcript.json` so later reads and `--follow`
//!   hit the same file even if a second session shares the cwd. The bind
//!   is a sidecar, not a `SessionRecord` field, so the worker's periodic
//!   `last_activity_ms` writes cannot race it away.
//! - **Re-locate** if the bound path disappears (agent rotated the log).
//!
//! Supported engines: claude, codex, grok. Variant engines identified with a
//! family by `engine_family` (e.g. `zcodex`, a codex-rs fork) parse and
//! locate through their family's machinery while keeping their own engine id
//! on emitted events. `shell`/`gemini`/`opencode` have
//! no reader here yet. Claude's native `.jsonl` is the Anthropic Messages
//! API event shape (same functions as heru's claude adapter). Codex's
//! native rollout (`response_item`) is a different shape from `codex exec
//! --json` -- only the native shape is parsed, because this is not a
//! headless launcher. Grok Build writes ACP `updates.jsonl`.
//!
//! Known, deliberate departures from heru's Python source:
//!
//! - heru's claude `live_events()` has an unreachable top-level
//!   `tool_result` branch; real logs nest tool results inside
//!   `{"type":"user","message":{"content":[{"type":"tool_result",...}]}}`.
//!   This keeps the dead branch and unwraps the real `"user"` shape, and
//!   also emits user **text** turns from that same `"user"` event (needed
//!   for PocketShell display; heru's live adapter was assistant-centric).
//! - Claude `content_block_delta` chunks AND the later complete
//!   `"assistant"` message both become `message` events -- heru's behavior,
//!   ported faithfully. A consumer that wants only the final text should
//!   skip the small deltas.
//!
//! Layout: `assembler` reassembles one JSON value per row, `claude`/`codex`/
//! `grok` translate each native shape, `wire` dispatches on the engine
//! family, `locate` finds and binds the log file, and `reader` streams it
//! into pages and a live tail.

mod assembler;
mod claude;
mod codex;
mod grok;
mod locate;
mod reader;
#[cfg(test)]
mod tests;
mod wire;

use crate::watch::{iso8601_utc, UnifiedEvent};
use crate::{atomic_write_json, engine_family, now_ms, SessionRecord};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

pub(crate) use assembler::*;
pub(crate) use claude::*;
pub(crate) use codex::*;
pub(crate) use grok::*;
pub use locate::*;
pub use reader::*;
pub use wire::*;
