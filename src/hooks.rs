//! `a init` — machine-wide agent-state hook installation.
//!
//! `a state-report <idle|waiting|working>` (see `cmd_state_report` in
//! `src/bin/a.rs`) is the ingestion primitive: a hook running inside a
//! session pushes semantic state, and `a watch` / `a list` / `a status` /
//! the attach status bar treat a fresh push as authoritative over the
//! PTY-recency heuristic. But nothing in this repo used to *call* it from
//! inside a live agent session, so every session fell back to the heuristic
//! — an idle opencode/grok sitting in a `shell`-engine session showed
//! `RUNNING`/`active` forever (docs/pocketshell-integration-plan.md Open
//! question #2 called this wiring "explicitly undesigned").
//!
//! This module is that wiring. `a init` merges a `state-report` hook into
//! every agent engine aplexer knows how to launch — `claude`, `codex` (which
//! also covers the `zcodex` variant: it reads the same `CODEX_HOME`
//! mechanism), `grok`, `gemini`, and `opencode` — including each discovered
//! / configured profile config dir (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`), so a
//! `codex/zodex` session reports state just like a plain `codex` one.
//! `a init --check [--json]` reports whether that wiring is present, for
//! humans and for automation (the PocketShell host CLI runs the `--json`
//! form on startup and runs `a init` when it says `initialized: false`).
//!
//! Design rules, ported from PocketShell's `hooks.py` (merge, never
//! clobber):
//!
//! - Install is non-destructive and idempotent. Existing hook groups,
//!   unrelated config keys, and other plugins are preserved; running
//!   install twice adds nothing new, and files are only rewritten when
//!   their content would actually change (no mtime churn).
//! - A hook command never blocks the agent. Every generated command ends
//!   in `|| true`, so outside an aplexer session (no `APLEXER_SESSION_ID`)
//!   — or if the `a` binary is ever missing — the hook exits 0 and the
//!   agent's stop/submit flow proceeds. This matters most for blocking
//!   events (`Stop`, `UserPromptSubmit`): a failing hook there would hold
//!   the agent open.
//! - An existing `notify` (codex) pointing elsewhere is left alone. The
//!   modern `hooks.json` channel carries our signal independently, so both
//!   can coexist; we only write `notify` when it is absent (old binaries,
//!   `codex exec` where hooks do not fire).
//! - `uninstall` (`a init --uninstall`) removes only `state-report` hook
//!   entries and our own generated files. Anything it cannot recognise as
//!   ours-by-content (`state-report` in the command) is left untouched.
//!
//! Per-engine mechanisms (verified against each CLI's docs, see the
//! `*_EVENTS` tables below):
//!
//! - Claude: nested `{"hooks": {<Event>: [{"hooks": [{"type":
//!   "command", "command": ...}]}]}}` in `settings.json`.
//! - Codex: the same nested shape in `hooks.json` (newer binaries also
//!   accept inline `[hooks.*]` in `config.toml`; a separate file avoids
//!   TOML round-tripping entirely), plus legacy top-level
//!   `notify = [...]` in `config.toml` when absent.
//! - Grok: same nested shape as Claude, in one owned file
//!   `<grok-home>/hooks/aplexer.json` (the dir merges every `*.json`).
//! - Gemini: same nested shape in `settings.json`, with Claude's `Stop`
//!   spelled `AfterAgent` and `UserPromptSubmit` spelled `BeforeAgent`.
//! - OpenCode: an owned plugin file in the global plugin dir, mapping
//!   `session.idle` → `idle`, `permission.asked` / `session.error` →
//!   `waiting`, `session.created` → `working`.
//!
//! State vocabulary in every mapping is `a state-report`'s own
//! (`idle`/`waiting`/`working`, matching PocketShell's
//! Idle/WaitingForInput/Working one for one): a finished turn rests
//! (`idle`), a permission prompt or error needs the user (`waiting`), a
//! submitted prompt or (re)started session is back to work (`working`).
//! There is deliberately no per-tool hook (e.g. `PreToolUse`): after an
//! `idle` push goes stale the PTY-activity heuristic takes over again and
//! reports `active` while the agent produces output, so the resume boundary
//! needs no hook of its own.

use crate::persist::atomic_write_bytes_with_mode;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Engines `a init` manages. `zcodex` is intentionally absent: it is a
/// codex-family variant sharing the `CODEX_HOME` mechanism, so every codex
/// config dir installed here already covers it.
pub const HOOK_ENGINES: [&str; 5] = ["claude", "codex", "grok", "gemini", "opencode"];

/// (hook event, reported state) wirings per engine. The state words are `a
/// state-report`'s vocabulary; the event names are each engine's own.
///
/// `SubagentStop` is deliberately absent even though Claude and Grok fire
/// it: a subagent finishing does not leave *the agent* idle -- the main
/// turn keeps producing output after it -- so mapping it to `idle` pushed
/// a fresh lie mid-turn, and `idle` is the one push with no follow-up hook
/// to correct it (see `watch::fresh_reported_state`). The main turn's
/// `Stop` alone marks the rest.
pub const CLAUDE_EVENTS: [(&str, &str); 4] = [
    ("Stop", "idle"),
    ("Notification", "waiting"),
    ("UserPromptSubmit", "working"),
    ("SessionStart", "working"),
];
/// Codex's `hooks.json` uses Claude-style event names for these three.
pub const CODEX_EVENTS: [(&str, &str); 3] = [
    ("Stop", "idle"),
    ("UserPromptSubmit", "working"),
    ("SessionStart", "working"),
];
/// Grok's personal-hooks dir speaks the Claude-compatible nested format.
pub const GROK_EVENTS: [(&str, &str); 4] = [
    ("Stop", "idle"),
    ("Notification", "waiting"),
    ("UserPromptSubmit", "working"),
    ("SessionStart", "working"),
];
/// Gemini renames the turn boundaries; the shape is otherwise identical.
pub const GEMINI_EVENTS: [(&str, &str); 4] = [
    ("AfterAgent", "idle"),
    ("BeforeAgent", "working"),
    ("Notification", "waiting"),
    ("SessionStart", "working"),
];

/// Owned filename for our hooks inside Grok's merged `hooks/` dir.
pub const GROK_HOOKS_FILENAME: &str = "aplexer.json";
/// Codex's modern per-home hooks file (alongside `config.toml`).
pub const CODEX_HOOKS_FILENAME: &str = "hooks.json";
/// Owned filename for our plugin inside OpenCode's global plugin dir.
pub const OPENCODE_PLUGIN_FILENAME: &str = "aplexer-state-report.js";

/// Shell-quote one argv word (the resolved `a` binary path) for embedding
/// in a hook `command` string. Paths are almost always boring; quote only
/// when needed so the common case stays readable in the user's config.
fn shell_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%_-+=:,./".contains(&b))
    {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The exact hook command installed for one state: `<a> state-report
/// <state> || true`. The `|| true` is the non-blocking guarantee (see the
/// module docs): `a state-report` exits 1 outside an aplexer session, and a
/// hook must never hold the agent open because of that.
pub fn state_report_command(a_bin: &str, state: &str) -> String {
    format!("{} state-report {state} || true", shell_quote(a_bin))
}

/// Whether a hook command string is a state-report hook (ours, or one the
/// user hand-wrote — either way it feeds `a state-report`, which is what
/// install-status checks). Matching is deliberately path-independent: the
/// `a` binary may legitimately move between installs.
pub fn is_state_report_command(command: &str) -> bool {
    command.contains("state-report")
}

/// Whether a command reports the expected state (used for precise
/// per-event status: a `Stop` hooked to `working` would be a wiring bug,
/// not an installed stop hook). The state must be the argument right
/// after `state-report`: a binary path that happens to contain "idle"
/// does not make every hook an idle hook.
fn reports_state(command: &str, state: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    words
        .windows(2)
        .any(|pair| pair[0] == "state-report" && pair[1] == state)
}

/// Resolve the `a` binary path to embed in generated hooks: the running
/// executable's absolute path when known (hook environments may have a
/// minimal `PATH`), falling back to a bare `a` looked up on `PATH`.
pub fn resolve_a_bin() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "a".to_string())
}

// ---------------------------------------------------------------------------
// Install targets
// ---------------------------------------------------------------------------

/// Every filesystem location `a init` manages, resolved from `$HOME`,
/// `$XDG_CONFIG_HOME`, `$GROK_HOME`, and the loaded aplexer config's
/// profile env (so profile config dirs are covered too). `home` is a
/// parameter rather than read here so tests can point at a throwaway dir.
#[derive(Debug, Clone)]
pub struct HookTargets {
    /// `settings.json` files gaining Claude hooks: the default
    /// `~/.claude/settings.json` plus one per distinct `CLAUDE_CONFIG_DIR`
    /// found in profiles.
    pub claude_settings: Vec<PathBuf>,
    /// Codex home dirs gaining `hooks.json` (+ legacy `notify` when
    /// absent): default `~/.codex` plus one per distinct `CODEX_HOME`.
    pub codex_dirs: Vec<PathBuf>,
    /// Grok home dir holding `hooks/aplexer.json`.
    pub grok_dir: PathBuf,
    /// Gemini user settings gaining hooks.
    pub gemini_settings: PathBuf,
    /// OpenCode global plugin dir holding our plugin file.
    pub opencode_plugin_dir: PathBuf,
}

/// Resolve install targets. `profile_envs` is every profile's `env` map
/// (plus any caller-supplied extras); only the recognised config-dir vars
/// (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) are read out of them — anything else
/// is ignored, never executed or merged.
pub fn resolve_targets(
    home: &Path,
    config_home: Option<&Path>,
    grok_home: Option<&Path>,
    profile_envs: &[BTreeMap<String, String>],
) -> HookTargets {
    let mut claude_settings = vec![home.join(".claude").join("settings.json")];
    let mut codex_dirs = vec![home.join(".codex")];
    for env in profile_envs {
        if let Some(dir) = env.get("CLAUDE_CONFIG_DIR") {
            let path = PathBuf::from(dir).join("settings.json");
            if !claude_settings.contains(&path) {
                claude_settings.push(path);
            }
        }
        if let Some(dir) = env.get("CODEX_HOME") {
            let path = PathBuf::from(dir);
            if !codex_dirs.contains(&path) {
                codex_dirs.push(path);
            }
        }
    }
    claude_settings.sort();
    codex_dirs.sort();
    let config_base: PathBuf = match config_home {
        Some(dir) => dir.to_path_buf(),
        None => home.join(".config"),
    };
    HookTargets {
        claude_settings,
        codex_dirs,
        grok_dir: match grok_home {
            Some(dir) => dir.to_path_buf(),
            None => home.join(".grok"),
        },
        gemini_settings: home.join(".gemini").join("settings.json"),
        opencode_plugin_dir: config_base.join("opencode").join("plugin"),
    }
}

/// Production resolution: `$HOME` + `$XDG_CONFIG_HOME` / `$GROK_HOME` from
/// the environment. Relative XDG/GROK overrides are ignored (same policy
/// as `Paths`: those must be absolute to mean anything).
pub fn resolve_targets_from_env(profile_envs: &[BTreeMap<String, String>]) -> Result<HookTargets> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    let grok_home = std::env::var_os("GROK_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    Ok(resolve_targets(
        &home,
        config_home.as_deref(),
        grok_home.as_deref(),
        profile_envs,
    ))
}

// ---------------------------------------------------------------------------
// Nested-hook JSON merge (Claude settings.json, Codex hooks.json,
// Grok aplexer.json, Gemini settings.json share one shape)
// ---------------------------------------------------------------------------

/// One hook group in the nested format.
fn our_group(command: String) -> Value {
    serde_json::json!({"hooks": [{"type": "command", "command": command}]})
}

/// Does a hook group already contain a state-report entry for `state`?
fn group_reports(group: &Value, state: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|hooks| {
            hooks.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| reports_state(c, state))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Merge our `(event, state)` wirings into a nested-hooks document.
/// Returns the number of events changed. Idempotent: a second merge with
/// the same commands changes nothing.
///
/// Shape errors (non-object root, non-object `hooks`) are refused rather
/// than clobbered — the file may hold something newer than this tool
/// understands. A non-array event slot is schema-invalid in every engine,
/// so it is replaced (there is nothing meaningful to preserve).
pub fn merge_nested_hooks(doc: &mut Value, events: &[(&str, &str)], a_bin: &str) -> Result<usize> {
    let root = doc.as_object_mut().ok_or_else(|| {
        anyhow::anyhow!("hooks document root is not a JSON object; left untouched")
    })?;
    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Default::default()));
    let hooks = hooks
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("\"hooks\" key is not an object; left untouched"))?;
    let mut changed = 0;
    for (event, state) in events {
        let slot = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let groups = match slot.as_array_mut() {
            Some(groups) => groups,
            None => {
                *slot = Value::Array(Vec::new());
                slot.as_array_mut().expect("just set to array")
            }
        };
        if !groups.iter().any(|g| group_reports(g, state)) {
            groups.push(our_group(state_report_command(a_bin, state)));
            changed += 1;
        }
    }
    Ok(changed)
}

/// Remove every state-report hook entry from the given events. Drops
/// emptied groups, events, and the top-level `hooks` object when we
/// emptied them. Returns true when anything changed.
/// Removes every `state-report` hook from a nested hooks document, keyed by
/// the ours-by-content command match alone, across ALL event groups -- not
/// just the events the current install tables name. The event tables change
/// between releases (SubagentStop was unmapped from `idle` in 2026-09); an
/// uninstall that iterated only the current names would leave a retired
/// event's entry behind forever, pushing state from an event this version no
/// longer believes in. Foreign commands (no `state-report` in them) are
/// never touched, whatever the event.
pub fn unmerge_nested_hooks(doc: &mut Value) -> bool {
    let Some(hooks) = doc.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    let events: Vec<String> = hooks.keys().cloned().collect();
    for event in events {
        let Some(groups) = hooks.get_mut(&event).and_then(Value::as_array_mut) else {
            continue;
        };
        let before = groups.len();
        let mut kept = Vec::with_capacity(groups.len());
        for group in groups.drain(..) {
            match group {
                Value::Object(mut map) => {
                    let inner = map.get_mut("hooks").and_then(Value::as_array_mut);
                    match inner {
                        Some(inner) => {
                            let inner_before = inner.len();
                            inner.retain(|h| {
                                h.get("command")
                                    .and_then(Value::as_str)
                                    .map(|c| !is_state_report_command(c))
                                    .unwrap_or(true)
                            });
                            if inner.len() != inner_before {
                                changed = true;
                            }
                            if !inner.is_empty() {
                                kept.push(Value::Object(map));
                            } else {
                                changed = true;
                            }
                        }
                        None => kept.push(Value::Object(map)),
                    }
                }
                other => kept.push(other),
            }
        }
        let _ = before;
        if kept.is_empty() {
            hooks.remove(&event);
        } else {
            hooks.insert(event, Value::Array(kept));
        }
    }
    if hooks.is_empty() {
        if let Some(root) = doc.as_object_mut() {
            root.remove("hooks");
        }
    }
    changed
}

/// Which required `(event, state)` wirings are missing from a document.
/// Empty means installed. A missing/unparseable file counts as all
/// missing (callers treat absent files as "not installed", not as errors).
pub fn missing_nested_hooks(doc: &Value, events: &[(&str, &str)]) -> Vec<String> {
    let mut missing = Vec::new();
    let hooks = doc.get("hooks").and_then(Value::as_object);
    for (event, state) in events {
        let present = hooks
            .and_then(|h| h.get(*event))
            .and_then(Value::as_array)
            .map(|groups| groups.iter().any(|g| group_reports(g, state)))
            .unwrap_or(false);
        if !present {
            missing.push((*event).to_string());
        }
    }
    missing
}

// ---------------------------------------------------------------------------
// Codex legacy notify (top-level `notify = [...]` in config.toml)
// ---------------------------------------------------------------------------

/// Render our notify argv for `config.toml`: a `sh -c` wrapper so the hook
/// always exits 0 (same non-blocking guarantee as the `|| true` commands).
pub fn codex_notify_line(a_bin: &str) -> String {
    let inner = state_report_command(a_bin, "idle");
    let mut rendered = String::from("notify = [\"sh\", \"-c\", ");
    rendered.push_str(&serde_json::to_string(&inner).unwrap_or_else(|_| format!("{inner:?}")));
    rendered.push(']');
    rendered
}

/// Classify the top-level `notify` in a `config.toml` text: the byte span
/// of its line plus whether it is ours (`Absend`/`Ours`/`Foreign`).
fn find_top_level_notify(text: &str) -> Option<(usize, usize)> {
    // A top-level `notify` lives before the first `[table]` header; anything
    // at or after that belongs to a table and is not Codex's `notify`.
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let line_start = pos;
        pos += line.len();
        let body = line.strip_suffix('\n').unwrap_or(line);
        let trimmed = body.trim_start();
        if trimmed.starts_with('[') {
            break;
        }
        if trimmed.starts_with("notify") && trimmed["notify".len()..].trim_start().starts_with('=')
        {
            return Some((line_start, line_start + body.len()));
        }
    }
    None
}

fn parse_notify_line(line: &str) -> Option<toml::Value> {
    line.parse::<toml::Value>()
        .ok()
        .and_then(|v| v.get("notify").cloned())
}

fn notify_value_is_ours(value: &toml::Value) -> bool {
    let args: Vec<&str> = match value.as_array() {
        Some(items) => items.iter().filter_map(toml::Value::as_str).collect(),
        None => return false,
    };
    args.iter().any(|a| is_state_report_command(a))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyState {
    Absent,
    Ours,
    Foreign,
}

/// Status of the legacy `notify` line (informational only — `hooks.json`
/// is the primary codex channel; see the module docs).
pub fn codex_notify_state(text: &str) -> NotifyState {
    match find_top_level_notify(text) {
        None => NotifyState::Absent,
        Some((start, end)) => match parse_notify_line(&text[start..end]) {
            Some(value) if notify_value_is_ours(&value) => NotifyState::Ours,
            _ => NotifyState::Foreign,
        },
    }
}

/// Ensure our `notify` line exists. Returns the new text and whether it
/// changed. A foreign `notify` is never clobbered (returns unchanged).
pub fn ensure_codex_notify(text: &str, a_bin: &str) -> (String, bool) {
    match find_top_level_notify(text) {
        None => {
            let mut out = codex_notify_line(a_bin);
            out.push('\n');
            out.push_str(text);
            (out, true)
        }
        Some((start, end)) => match parse_notify_line(&text[start..end]) {
            Some(value) if notify_value_is_ours(&value) => (text.to_string(), false),
            _ => (text.to_string(), false),
        },
    }
}

/// Remove our `notify` line. Returns the new text and whether it changed.
/// A foreign `notify` is left alone.
pub fn remove_codex_notify(text: &str) -> (String, bool) {
    let Some((start, end)) = find_top_level_notify(text) else {
        return (text.to_string(), false);
    };
    let is_ours = parse_notify_line(&text[start..end])
        .map(|v| notify_value_is_ours(&v))
        .unwrap_or(false);
    if !is_ours {
        return (text.to_string(), false);
    }
    let drop_end = if text.as_bytes().get(end) == Some(&b'\n') {
        end + 1
    } else {
        end
    };
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&text[drop_end..]);
    (out, true)
}

// ---------------------------------------------------------------------------
// OpenCode plugin
// ---------------------------------------------------------------------------

/// Render our OpenCode plugin. The `a` path is embedded as a JSON string
/// literal (valid JS), and the plugin shells out via argv — no shell
/// quoting issues — wrapped so hook failures never surface to the agent.
pub fn opencode_plugin_source(a_bin: &str) -> String {
    let a_json = serde_json::to_string(a_bin).unwrap_or_else(|_| "\"a\"".to_string());
    format!(
        r#"// aplexer state-report plugin (generated by `a init`).
//
// Reports agent lifecycle to `a state-report` so `a list`, `a status` and
// the attach status bar show semantic state (idle/waiting/working) instead
// of guessing from PTY output. Best-effort: failures never surface to the
// agent. Re-running `a init` refreshes this file; `a init --uninstall`
// removes it. Other plugins in this dir are untouched.
import child_process from "node:child_process";

const A = {a_json};

function report(state) {{
  try {{
    child_process.spawnSync(A, ["state-report", state], {{ stdio: "ignore" }});
  }} catch (e) {{
    // best-effort; never throw out of a plugin hook
  }}
}}

export const AplexerStateReport = async () => {{
  return {{
    event: async ({{ event }}) => {{
      if (event.type === "session.idle") {{
        report("idle");
      }} else if (event.type === "permission.asked") {{
        report("waiting");
      }} else if (event.type === "session.error") {{
        report("waiting");
      }} else if (event.type === "session.created") {{
        report("working");
      }}
    }},
  }};
}};
"#
    )
}

// ---------------------------------------------------------------------------
// Filesystem drivers
// ---------------------------------------------------------------------------

/// One engine's install/check/uninstall outcome. Serializable so `a init
/// --json` / `a init --check --json` are the machine contract PocketShell
/// automates against.
#[derive(Debug, Clone, Serialize)]
pub struct EngineInitStatus {
    pub engine: String,
    pub installed: bool,
    pub action: String,
    pub message: String,
    pub paths: Vec<String>,
}

/// Read a JSON file, defaulting to an empty object when absent/blank.
/// A malformed file is an error (refuse to clobber what we cannot parse).
fn read_json_or_default(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Default::default()));
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(&text)
        .with_context(|| format!("parse {} (left untouched)", path.display()))
}

/// Where a write to `path` must land: the file itself, or -- when `path`
/// is a symlink, as a dotfiles-managed `settings.json` is -- its target,
/// so the atomic rename replaces the real file and leaves the link intact.
/// A dangling link resolves to the file it points at, which the write then
/// creates.
fn write_target(path: &Path) -> Result<PathBuf> {
    let is_symlink = fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
    if !is_symlink {
        return Ok(path.to_path_buf());
    }
    if let Ok(real) = fs::canonicalize(path) {
        return Ok(real);
    }
    let link = fs::read_link(path).with_context(|| format!("read link {}", path.display()))?;
    Ok(if link.is_absolute() {
        link
    } else {
        path.parent().unwrap_or(Path::new("")).join(link)
    })
}

/// Atomically write text, preserving the existing file's mode and using
/// 0600 for new files. Unlike the session-record writer this must NOT
/// force private dirs: engine configs live in the user's normal (often
/// 0755) home tree.
fn atomic_write_text_preserving_mode(path: &Path, text: &str) -> Result<()> {
    let target = write_target(path)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    let mode = fs::metadata(&target)
        .map(|meta| meta.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    atomic_write_bytes_with_mode(&target, text.as_bytes(), mode)
        .with_context(|| format!("write {}", target.display()))
}

/// Write only when the content differs (idempotence without mtime churn).
/// Returns true when the file was written.
fn write_if_changed(path: &Path, text: &str) -> Result<bool> {
    if path.exists() {
        let current =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if current == text {
            return Ok(false);
        }
    }
    atomic_write_text_preserving_mode(path, text)?;
    Ok(true)
}

fn render_json(doc: &Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(doc)? + "\n")
}

/// Install/merge nested hooks into one JSON settings file. Returns
/// (changed, message).
fn install_nested_file(
    path: &Path,
    events: &[(&str, &str)],
    a_bin: &str,
) -> Result<(bool, String)> {
    let mut doc = read_json_or_default(path)?;
    let changed = merge_nested_hooks(&mut doc, events, a_bin)?;
    if changed == 0 {
        return Ok((
            false,
            format!("hook already installed in {}", path.display()),
        ));
    }
    write_if_changed(path, &render_json(&doc)?)?;
    Ok((true, format!("merged hook into {}", path.display())))
}

fn check_nested_file(path: &Path, events: &[(&str, &str)]) -> (bool, String) {
    if !path.exists() {
        return (false, format!("{} not present", path.display()));
    }
    match read_json_or_default(path) {
        Err(e) => (false, format!("{}: {e:#}", path.display())),
        Ok(doc) => {
            let missing = missing_nested_hooks(&doc, events);
            if missing.is_empty() {
                (true, format!("hook installed in {}", path.display()))
            } else {
                (
                    false,
                    format!("{} missing events: {}", path.display(), missing.join(", ")),
                )
            }
        }
    }
}

fn uninstall_nested_file(path: &Path) -> Result<(bool, String)> {
    if !path.exists() {
        return Ok((false, format!("{} not present", path.display())));
    }
    let mut doc = read_json_or_default(path)?;
    if !unmerge_nested_hooks(&mut doc) {
        return Ok((false, format!("no hook in {}", path.display())));
    }
    write_if_changed(path, &render_json(&doc)?)?;
    Ok((true, format!("removed hook from {}", path.display())))
}

// ---------------------------------------------------------------------------
// Per-engine drivers
// ---------------------------------------------------------------------------

fn install_claude(targets: &HookTargets, a_bin: &str) -> EngineInitStatus {
    let mut changed_any = false;
    let mut messages = Vec::new();
    let mut failed = false;
    for path in &targets.claude_settings {
        match install_nested_file(path, &CLAUDE_EVENTS, a_bin) {
            Ok((changed, message)) => {
                changed_any |= changed;
                messages.push(message);
            }
            Err(e) => {
                failed = true;
                messages.push(format!("{}: {e:#}", path.display()));
            }
        }
    }
    EngineInitStatus {
        engine: "claude".to_string(),
        installed: !failed && messages.len() == targets.claude_settings.len(),
        action: if failed {
            "error"
        } else if changed_any {
            "installed"
        } else {
            "present"
        }
        .to_string(),
        message: messages.join("; "),
        paths: targets
            .claude_settings
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
    }
}

fn install_codex(targets: &HookTargets, a_bin: &str) -> EngineInitStatus {
    let mut changed_any = false;
    let mut messages = Vec::new();
    let mut failed = false;
    let mut paths = Vec::new();
    for dir in &targets.codex_dirs {
        let hooks_path = dir.join(CODEX_HOOKS_FILENAME);
        paths.push(hooks_path.display().to_string());
        match install_nested_file(&hooks_path, &CODEX_EVENTS, a_bin) {
            Ok((changed, message)) => {
                changed_any |= changed;
                messages.push(message);
            }
            Err(e) => {
                failed = true;
                messages.push(format!("{}: {e:#}", hooks_path.display()));
            }
        }
        // Legacy notify: only when absent, never clobbering a foreign
        // program (which may be PocketShell's handler or the user's own).
        let config_path = dir.join("config.toml");
        paths.push(config_path.display().to_string());
        if config_path.exists() {
            match fs::read_to_string(&config_path) {
                Err(e) => {
                    failed = true;
                    messages.push(format!("{}: {e:#}", config_path.display()));
                }
                Ok(text) => match codex_notify_state(&text) {
                    NotifyState::Ours | NotifyState::Foreign => {}
                    NotifyState::Absent => {
                        let (new_text, _) = ensure_codex_notify(&text, a_bin);
                        match write_if_changed(&config_path, &new_text) {
                            Ok(true) => {
                                messages.push(format!("set notify in {}", config_path.display()))
                            }
                            Ok(false) => {}
                            Err(e) => {
                                failed = true;
                                messages.push(format!("{}: {e:#}", config_path.display()));
                            }
                        }
                    }
                },
            }
        }
    }
    EngineInitStatus {
        engine: "codex".to_string(),
        installed: !failed,
        action: if failed {
            "error"
        } else if changed_any {
            "installed"
        } else {
            "present"
        }
        .to_string(),
        // `zcodex` shares CODEX_HOME, so it is covered by the same dirs.
        message: messages.join("; ") + " (covers zcodex via shared CODEX_HOME)",
        paths,
    }
}

fn install_grok(targets: &HookTargets, a_bin: &str) -> EngineInitStatus {
    let path = targets.grok_dir.join("hooks").join(GROK_HOOKS_FILENAME);
    // Owned file in a merged dir: no need to read anything else.
    let mut doc = Value::Object(Default::default());
    match merge_nested_hooks(&mut doc, &GROK_EVENTS, a_bin) {
        Err(e) => EngineInitStatus {
            engine: "grok".to_string(),
            installed: false,
            action: "error".to_string(),
            message: format!("{}: {e:#}", path.display()),
            paths: vec![path.display().to_string()],
        },
        Ok(_) => match render_json(&doc).and_then(|text| write_if_changed(&path, &text)) {
            Err(e) => EngineInitStatus {
                engine: "grok".to_string(),
                installed: false,
                action: "error".to_string(),
                message: format!("{}: {e:#}", path.display()),
                paths: vec![path.display().to_string()],
            },
            Ok(wrote) => EngineInitStatus {
                engine: "grok".to_string(),
                installed: true,
                action: if wrote { "installed" } else { "present" }.to_string(),
                message: format!(
                    "{} {}",
                    if wrote {
                        "wrote"
                    } else {
                        "already installed in"
                    },
                    path.display()
                ),
                paths: vec![path.display().to_string()],
            },
        },
    }
}

fn install_gemini(targets: &HookTargets, a_bin: &str) -> EngineInitStatus {
    match install_nested_file(&targets.gemini_settings, &GEMINI_EVENTS, a_bin) {
        Ok((changed, message)) => EngineInitStatus {
            engine: "gemini".to_string(),
            installed: true,
            action: if changed { "installed" } else { "present" }.to_string(),
            message,
            paths: vec![targets.gemini_settings.display().to_string()],
        },
        Err(e) => EngineInitStatus {
            engine: "gemini".to_string(),
            installed: false,
            action: "error".to_string(),
            message: format!("{}: {e:#}", targets.gemini_settings.display()),
            paths: vec![targets.gemini_settings.display().to_string()],
        },
    }
}

fn install_opencode(targets: &HookTargets, a_bin: &str) -> EngineInitStatus {
    let path = targets.opencode_plugin_dir.join(OPENCODE_PLUGIN_FILENAME);
    let source = opencode_plugin_source(a_bin);
    match write_if_changed(&path, &source) {
        Ok(wrote) => EngineInitStatus {
            engine: "opencode".to_string(),
            installed: true,
            action: if wrote { "installed" } else { "present" }.to_string(),
            message: format!(
                "{} {}",
                if wrote {
                    "wrote plugin"
                } else {
                    "plugin already installed"
                },
                path.display()
            ),
            paths: vec![path.display().to_string()],
        },
        Err(e) => EngineInitStatus {
            engine: "opencode".to_string(),
            installed: false,
            action: "error".to_string(),
            message: format!("{}: {e:#}", path.display()),
            paths: vec![path.display().to_string()],
        },
    }
}

fn check_claude(targets: &HookTargets) -> EngineInitStatus {
    let mut ok = true;
    let mut messages = Vec::new();
    for path in &targets.claude_settings {
        let (present, message) = check_nested_file(path, &CLAUDE_EVENTS);
        ok &= present;
        messages.push(message);
    }
    EngineInitStatus {
        engine: "claude".to_string(),
        installed: ok,
        action: if ok { "present" } else { "absent" }.to_string(),
        message: messages.join("; "),
        paths: targets
            .claude_settings
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
    }
}

fn check_codex(targets: &HookTargets) -> EngineInitStatus {
    let mut ok = true;
    let mut messages = Vec::new();
    let mut paths = Vec::new();
    for dir in &targets.codex_dirs {
        let hooks_path = dir.join(CODEX_HOOKS_FILENAME);
        paths.push(hooks_path.display().to_string());
        let (present, message) = check_nested_file(&hooks_path, &CODEX_EVENTS);
        ok &= present;
        messages.push(message);
    }
    EngineInitStatus {
        engine: "codex".to_string(),
        installed: ok,
        action: if ok { "present" } else { "absent" }.to_string(),
        message: messages.join("; ") + " (covers zcodex via shared CODEX_HOME)",
        paths,
    }
}

fn check_grok(targets: &HookTargets) -> EngineInitStatus {
    let path = targets.grok_dir.join("hooks").join(GROK_HOOKS_FILENAME);
    let (installed, message) = check_nested_file(&path, &GROK_EVENTS);
    EngineInitStatus {
        engine: "grok".to_string(),
        installed,
        action: if installed { "present" } else { "absent" }.to_string(),
        message,
        paths: vec![path.display().to_string()],
    }
}

fn check_gemini(targets: &HookTargets) -> EngineInitStatus {
    let (installed, message) = check_nested_file(&targets.gemini_settings, &GEMINI_EVENTS);
    EngineInitStatus {
        engine: "gemini".to_string(),
        installed,
        action: if installed { "present" } else { "absent" }.to_string(),
        message,
        paths: vec![targets.gemini_settings.display().to_string()],
    }
}

fn check_opencode(targets: &HookTargets) -> EngineInitStatus {
    let path = targets.opencode_plugin_dir.join(OPENCODE_PLUGIN_FILENAME);
    let installed = if !path.exists() {
        false
    } else {
        fs::read_to_string(&path)
            .map(|text| text.contains("state-report"))
            .unwrap_or(false)
    };
    EngineInitStatus {
        engine: "opencode".to_string(),
        installed,
        action: if installed { "present" } else { "absent" }.to_string(),
        message: if installed {
            format!("plugin installed at {}", path.display())
        } else {
            format!("{} not present", path.display())
        },
        paths: vec![path.display().to_string()],
    }
}

fn uninstall_claude(targets: &HookTargets) -> EngineInitStatus {
    uninstall_nested_driver("claude", &targets.claude_settings)
}

fn uninstall_codex(targets: &HookTargets) -> EngineInitStatus {
    let mut changed_any = false;
    let mut messages = Vec::new();
    let mut paths = Vec::new();
    for dir in &targets.codex_dirs {
        let hooks_path = dir.join(CODEX_HOOKS_FILENAME);
        paths.push(hooks_path.display().to_string());
        match uninstall_nested_file(&hooks_path) {
            Ok((changed, message)) => {
                changed_any |= changed;
                messages.push(message);
            }
            Err(e) => messages.push(format!("{}: {e:#}", hooks_path.display())),
        }
        let config_path = dir.join("config.toml");
        paths.push(config_path.display().to_string());
        if config_path.exists() {
            match fs::read_to_string(&config_path) {
                Err(e) => messages.push(format!("{}: {e:#}", config_path.display())),
                Ok(text) => {
                    let (new_text, changed) = remove_codex_notify(&text);
                    if changed {
                        match write_if_changed(&config_path, &new_text) {
                            Ok(_) => {
                                changed_any = true;
                                messages
                                    .push(format!("removed notify from {}", config_path.display()));
                            }
                            Err(e) => messages.push(format!("{}: {e:#}", config_path.display())),
                        }
                    }
                }
            }
        }
    }
    EngineInitStatus {
        engine: "codex".to_string(),
        installed: false,
        action: if changed_any { "removed" } else { "absent" }.to_string(),
        message: messages.join("; "),
        paths,
    }
}

fn uninstall_grok(targets: &HookTargets) -> EngineInitStatus {
    let path = targets.grok_dir.join("hooks").join(GROK_HOOKS_FILENAME);
    if path.exists() {
        match fs::remove_file(&path) {
            Ok(()) => EngineInitStatus {
                engine: "grok".to_string(),
                installed: false,
                action: "removed".to_string(),
                message: format!("removed {}", path.display()),
                paths: vec![path.display().to_string()],
            },
            Err(e) => EngineInitStatus {
                engine: "grok".to_string(),
                installed: false,
                action: "error".to_string(),
                message: format!("{}: {e:#}", path.display()),
                paths: vec![path.display().to_string()],
            },
        }
    } else {
        EngineInitStatus {
            engine: "grok".to_string(),
            installed: false,
            action: "absent".to_string(),
            message: format!("{} not present", path.display()),
            paths: vec![path.display().to_string()],
        }
    }
}

fn uninstall_gemini(targets: &HookTargets) -> EngineInitStatus {
    uninstall_nested_driver("gemini", std::slice::from_ref(&targets.gemini_settings))
}

fn uninstall_opencode(targets: &HookTargets) -> EngineInitStatus {
    let path = targets.opencode_plugin_dir.join(OPENCODE_PLUGIN_FILENAME);
    if path.exists() {
        match fs::remove_file(&path) {
            Ok(()) => EngineInitStatus {
                engine: "opencode".to_string(),
                installed: false,
                action: "removed".to_string(),
                message: format!("removed plugin {}", path.display()),
                paths: vec![path.display().to_string()],
            },
            Err(e) => EngineInitStatus {
                engine: "opencode".to_string(),
                installed: false,
                action: "error".to_string(),
                message: format!("{}: {e:#}", path.display()),
                paths: vec![path.display().to_string()],
            },
        }
    } else {
        EngineInitStatus {
            engine: "opencode".to_string(),
            installed: false,
            action: "absent".to_string(),
            message: format!("{} not present", path.display()),
            paths: vec![path.display().to_string()],
        }
    }
}

fn uninstall_nested_driver(engine: &str, files: &[PathBuf]) -> EngineInitStatus {
    let mut changed_any = false;
    let mut messages = Vec::new();
    for path in files {
        match uninstall_nested_file(path) {
            Ok((changed, message)) => {
                changed_any |= changed;
                messages.push(message);
            }
            Err(e) => messages.push(format!("{}: {e:#}", path.display())),
        }
    }
    EngineInitStatus {
        engine: engine.to_string(),
        installed: false,
        action: if changed_any { "removed" } else { "absent" }.to_string(),
        message: messages.join("; "),
        paths: files.iter().map(|p| p.display().to_string()).collect(),
    }
}

// ---------------------------------------------------------------------------
// Engine selection + top-level drivers
// ---------------------------------------------------------------------------

/// Normalise `--engine` for `a init`: `zcodex` rides the codex dirs, every
/// other id must be one of `HOOK_ENGINES`.
pub fn normalize_engine_filter(engine: &str) -> Result<&'static str> {
    match engine {
        "claude" => Ok("claude"),
        "codex" | "zcodex" => Ok("codex"),
        "grok" => Ok("grok"),
        "gemini" => Ok("gemini"),
        "opencode" => Ok("opencode"),
        other => anyhow::bail!(
            "unknown engine {other:?} for hook installation; expected one of claude, codex, zcodex, grok, gemini, opencode"
        ),
    }
}

fn selected(engine: Option<&str>, candidate: &str) -> bool {
    match engine {
        None => true,
        Some(want) => want == candidate,
    }
}

/// Install hooks for the selected engines. Idempotent; merge, never
/// clobber (see the module docs).
pub fn install(targets: &HookTargets, a_bin: &str, engine: Option<&str>) -> Vec<EngineInitStatus> {
    let mut out = Vec::new();
    if selected(engine, "claude") {
        out.push(install_claude(targets, a_bin));
    }
    if selected(engine, "codex") {
        out.push(install_codex(targets, a_bin));
    }
    if selected(engine, "grok") {
        out.push(install_grok(targets, a_bin));
    }
    if selected(engine, "gemini") {
        out.push(install_gemini(targets, a_bin));
    }
    if selected(engine, "opencode") {
        out.push(install_opencode(targets, a_bin));
    }
    out
}

/// Check hook presence for the selected engines. No files are touched.
pub fn check(targets: &HookTargets, engine: Option<&str>) -> Vec<EngineInitStatus> {
    let mut out = Vec::new();
    if selected(engine, "claude") {
        out.push(check_claude(targets));
    }
    if selected(engine, "codex") {
        out.push(check_codex(targets));
    }
    if selected(engine, "grok") {
        out.push(check_grok(targets));
    }
    if selected(engine, "gemini") {
        out.push(check_gemini(targets));
    }
    if selected(engine, "opencode") {
        out.push(check_opencode(targets));
    }
    out
}

/// Remove our hooks for the selected engines. Only `state-report` entries
/// and our own generated files are removed.
pub fn uninstall(targets: &HookTargets, engine: Option<&str>) -> Vec<EngineInitStatus> {
    let mut out = Vec::new();
    if selected(engine, "claude") {
        out.push(uninstall_claude(targets));
    }
    if selected(engine, "codex") {
        out.push(uninstall_codex(targets));
    }
    if selected(engine, "grok") {
        out.push(uninstall_grok(targets));
    }
    if selected(engine, "gemini") {
        out.push(uninstall_gemini(targets));
    }
    if selected(engine, "opencode") {
        out.push(uninstall_opencode(targets));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const A_BIN: &str = "/home/test/.local/bin/a";

    fn merged(events: &[(&str, &str)], start: Value) -> Value {
        let mut doc = start;
        merge_nested_hooks(&mut doc, events, A_BIN).unwrap();
        doc
    }

    #[test]
    fn merge_creates_hooks_object_from_empty() {
        let doc = merged(&CLAUDE_EVENTS, json!({}));
        for (event, state) in CLAUDE_EVENTS {
            let groups = doc["hooks"][event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "event {event}");
            assert!(group_reports(&groups[0], state));
        }
        assert!(missing_nested_hooks(&doc, &CLAUDE_EVENTS).is_empty());
    }

    #[test]
    fn merge_is_idempotent() {
        let once = merged(&CLAUDE_EVENTS, json!({}));
        let mut twice = once.clone();
        let changed = merge_nested_hooks(&mut twice, &CLAUDE_EVENTS, A_BIN).unwrap();
        assert_eq!(changed, 0);
        assert_eq!(once, twice);
    }

    #[test]
    fn merge_preserves_existing_hooks_and_keys() {
        let start = json!({
            "permissions": {"deny": ["AskUserQuestion"]},
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "my-linter"}]}],
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "check"}]}]
            }
        });
        let doc = merged(&CLAUDE_EVENTS, start);
        // Unrelated top-level keys survive.
        assert_eq!(doc["permissions"]["deny"], json!(["AskUserQuestion"]));
        // Pre-existing Stop group survives alongside ours.
        let stop = doc["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 2);
        assert!(stop
            .iter()
            .any(|g| g["hooks"][0]["command"] == json!("my-linter")));
        // Untouched events survive byte-for-byte in value.
        assert_eq!(
            doc["hooks"]["PreToolUse"],
            json!([{"matcher": "Bash", "hooks": [{"type": "command", "command": "check"}]}])
        );
    }

    #[test]
    fn merge_accepts_a_foreign_state_report_hook_as_installed() {
        // A hand-written `a state-report` hook (different binary path)
        // already feeds ingestion; do not add a duplicate group.
        let start = json!({
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "a state-report idle || true"}]}]
            }
        });
        let mut doc = start;
        let changed = merge_nested_hooks(&mut doc, &[("Stop", "idle")], A_BIN).unwrap();
        assert_eq!(changed, 0);
        assert!(missing_nested_hooks(&doc, &[("Stop", "idle")]).is_empty());
    }

    #[test]
    fn merge_replaces_a_schema_invalid_event_slot() {
        let doc = merged(&CODEX_EVENTS, json!({"hooks": {"Stop": "bogus"}}));
        assert!(missing_nested_hooks(&doc, &CODEX_EVENTS).is_empty());
    }

    #[test]
    fn merge_refuses_a_non_object_root() {
        let mut doc = json!([1, 2, 3]);
        assert!(merge_nested_hooks(&mut doc, &CODEX_EVENTS, A_BIN).is_err());
        assert_eq!(doc, json!([1, 2, 3]));
    }

    #[test]
    fn unmerge_removes_only_ours_and_drops_emptied_keys() {
        let mut doc = merged(&CLAUDE_EVENTS, json!({}));
        assert!(unmerge_nested_hooks(&mut doc));
        // Whole `hooks` object is gone: we created every key in it.
        assert_eq!(doc, json!({}));
    }

    #[test]
    fn unmerge_keeps_user_hooks_in_shared_groups() {
        let start = json!({
            "hooks": {
                "Stop": [{
                    "matcher": "x",
                    "hooks": [
                        {"type": "command", "command": "my-linter"},
                        {"type": "command", "command": "a state-report idle || true"}
                    ]
                }]
            }
        });
        let mut doc = start;
        assert!(unmerge_nested_hooks(&mut doc));
        assert_eq!(
            doc["hooks"]["Stop"],
            json!([{
                "matcher": "x",
                "hooks": [{"type": "command", "command": "my-linter"}]
            }])
        );
    }

    #[test]
    fn unmerge_sweeps_retired_events_but_leaves_foreign_hooks_there() {
        // SubagentStop was unmapped from `idle` in 2026-09. An uninstall
        // keyed on the current install tables would never visit its group,
        // leaving our entry pushing idle from an event this version no
        // longer believes in -- while a foreign program's SubagentStop hook
        // in the same document must survive untouched.
        let start = json!({
            "hooks": {
                "Stop": [{
                    "hooks": [{"type": "command", "command": "a state-report idle || true"}]
                }],
                "SubagentStop": [{
                    "hooks": [
                        {"type": "command", "command": "python3 /opt/pocketshell/hooks/claude_hook.py"},
                        {"type": "command", "command": "/usr/local/bin/a state-report idle || true"}
                    ]
                }]
            }
        });
        let mut doc = start;
        assert!(unmerge_nested_hooks(&mut doc));
        assert!(doc["hooks"].get("Stop").is_none());
        assert_eq!(
            doc["hooks"]["SubagentStop"],
            json!([{
                "hooks": [
                    {"type": "command", "command": "python3 /opt/pocketshell/hooks/claude_hook.py"}
                ]
            }])
        );
    }

    #[test]
    fn missing_reports_every_absent_event() {
        let missing = missing_nested_hooks(&json!({}), &GEMINI_EVENTS);
        assert_eq!(missing.len(), GEMINI_EVENTS.len());
        let doc = merged(&GEMINI_EVENTS, json!({}));
        assert!(missing_nested_hooks(&doc, &GEMINI_EVENTS).is_empty());
    }

    #[test]
    fn state_report_command_never_blocks() {
        let cmd = state_report_command("/home/u/.local/bin/a", "idle");
        assert!(cmd.ends_with("|| true"), "{cmd}");
        // Boring paths stay unquoted and readable.
        assert_eq!(cmd, "/home/u/.local/bin/a state-report idle || true");
        // Weird paths are quoted, never interpolated raw.
        let quoted = state_report_command("/home/u/my dir/a", "waiting");
        assert!(quoted.starts_with("'/home/u/my dir/a'"), "{quoted}");
    }

    #[test]
    fn codex_notify_round_trip() {
        let (text, changed) = ensure_codex_notify("", A_BIN);
        assert!(changed);
        assert_eq!(codex_notify_state(&text), NotifyState::Ours);
        let (same, changed) = ensure_codex_notify(&text, A_BIN);
        assert!(!changed);
        assert_eq!(same, text);
        let (removed, changed) = remove_codex_notify(&text);
        assert!(changed);
        assert_eq!(removed, "");
        assert_eq!(codex_notify_state(&removed), NotifyState::Absent);
    }

    #[test]
    fn codex_notify_never_clobbers_a_foreign_program() {
        let foreign = "notify = [\"notify-send\", \"Codex\"]\n[model]\nname = \"x\"\n";
        assert_eq!(codex_notify_state(foreign), NotifyState::Foreign);
        let (same, changed) = ensure_codex_notify(foreign, A_BIN);
        assert!(!changed);
        assert_eq!(same, foreign);
        let (same, changed) = remove_codex_notify(foreign);
        assert!(!changed);
        assert_eq!(same, foreign);
    }

    #[test]
    fn codex_notify_ignores_table_scoped_keys() {
        // A `notify` under a [table] is not the top-level notify.
        let text = "[tui]\nnotify = [\"x\"]\n";
        assert_eq!(codex_notify_state(text), NotifyState::Absent);
        let (out, changed) = ensure_codex_notify(text, A_BIN);
        assert!(changed);
        assert!(out.starts_with("notify = "));
        assert!(out.contains("[tui]\nnotify = [\"x\"]\n"));
    }

    #[test]
    fn opencode_plugin_embeds_the_a_binary_and_maps_events() {
        let source = opencode_plugin_source(A_BIN);
        assert!(source.contains(A_BIN));
        for event in [
            "session.idle",
            "permission.asked",
            "session.error",
            "session.created",
        ] {
            assert!(source.contains(event), "missing {event}");
        }
        for state in ["idle", "waiting", "working"] {
            assert!(source.contains(state), "missing {state}");
        }
    }

    #[test]
    fn resolve_targets_covers_profile_config_dirs() {
        let home = Path::new("/home/u");
        let mut codex_profile = BTreeMap::new();
        codex_profile.insert("CODEX_HOME".to_string(), "/home/u/.zodex".to_string());
        let mut claude_profile = BTreeMap::new();
        claude_profile.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/home/u/.zlaude".to_string(),
        );
        let targets = resolve_targets(home, None, None, &[codex_profile, claude_profile]);
        assert!(targets
            .claude_settings
            .contains(&PathBuf::from("/home/u/.claude/settings.json")));
        assert!(targets
            .claude_settings
            .contains(&PathBuf::from("/home/u/.zlaude/settings.json")));
        assert!(targets
            .codex_dirs
            .contains(&PathBuf::from("/home/u/.codex")));
        assert!(targets
            .codex_dirs
            .contains(&PathBuf::from("/home/u/.zodex")));
        assert_eq!(targets.grok_dir, PathBuf::from("/home/u/.grok"));
        assert_eq!(
            targets.gemini_settings,
            PathBuf::from("/home/u/.gemini/settings.json")
        );
        assert_eq!(
            targets.opencode_plugin_dir,
            PathBuf::from("/home/u/.config/opencode/plugin")
        );
    }

    #[test]
    fn normalize_engine_filter_maps_zcodex_onto_codex() {
        assert_eq!(normalize_engine_filter("zcodex").unwrap(), "codex");
        assert_eq!(normalize_engine_filter("codex").unwrap(), "codex");
        assert!(normalize_engine_filter("shell").is_err());
    }

    #[test]
    fn install_check_uninstall_round_trip_in_a_throwaway_home() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path();
        let targets = resolve_targets(home, None, None, &[]);
        // check before install: nothing installed.
        let statuses = check(&targets, None);
        assert_eq!(statuses.len(), HOOK_ENGINES.len());
        assert!(statuses.iter().all(|s| !s.installed));
        // install: everything installs (fresh home, no foreign config).
        let installed = install(&targets, A_BIN, None);
        assert!(installed.iter().all(|s| s.installed), "{installed:?}");
        // check after install: fully initialized.
        let statuses = check(&targets, None);
        assert!(statuses.iter().all(|s| s.installed), "{statuses:?}");
        // install again: idempotent, nothing new.
        let again = install(&targets, A_BIN, None);
        assert!(again.iter().all(|s| s.action == "present"), "{again:?}");
        // uninstall: removes what install added.
        let removed = uninstall(&targets, None);
        assert!(removed.iter().all(|s| s.action != "error"), "{removed:?}");
        let statuses = check(&targets, None);
        assert!(statuses.iter().all(|s| !s.installed));
    }

    #[test]
    fn engine_filter_limits_the_drivers() {
        let dir = tempfile::TempDir::new().unwrap();
        let targets = resolve_targets(dir.path(), None, None, &[]);
        let only = install(&targets, A_BIN, Some("opencode"));
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].engine, "opencode");
        let statuses = check(&targets, Some("opencode"));
        assert!(statuses.iter().all(|s| s.installed));
        let others = check(&targets, Some("claude"));
        assert!(others.iter().all(|s| !s.installed));
    }

    #[test]
    fn install_writes_through_a_symlinked_settings_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().join("home");
        let dotfiles = dir.path().join("dotfiles");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir_all(home.join(".gemini")).unwrap();
        fs::create_dir_all(&dotfiles).unwrap();
        let real = dotfiles.join("claude-settings.json");
        fs::write(&real, "{\"permissions\": {}}\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
        let link = home.join(".claude").join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // A dotfiles link whose target does not exist yet.
        let dangling_target = dotfiles.join("gemini").join("settings.json");
        let dangling = home.join(".gemini").join("settings.json");
        std::os::unix::fs::symlink(&dangling_target, &dangling).unwrap();

        let targets = resolve_targets(&home, None, None, &[]);
        let statuses = install(&targets, A_BIN, None);
        assert!(statuses.iter().all(|s| s.installed), "{statuses:?}");

        for path in [&link, &dangling] {
            assert!(
                fs::symlink_metadata(path).unwrap().file_type().is_symlink(),
                "{} is no longer a symlink",
                path.display()
            );
        }
        let doc: Value = serde_json::from_str(&fs::read_to_string(&real).unwrap()).unwrap();
        assert!(missing_nested_hooks(&doc, &CLAUDE_EVENTS).is_empty());
        assert_eq!(doc["permissions"], json!({}));
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o640,
            "mode of the real file was not preserved"
        );
        let doc: Value =
            serde_json::from_str(&fs::read_to_string(&dangling_target).unwrap()).unwrap();
        assert!(missing_nested_hooks(&doc, &GEMINI_EVENTS).is_empty());
        assert_eq!(
            fs::metadata(&dangling_target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn reports_state_matches_the_state_argument_not_the_path() {
        let cmd = state_report_command("/home/u/idle-tools/a", "working");
        assert!(reports_state(&cmd, "working"));
        assert!(!reports_state(&cmd, "idle"), "{cmd}");
        assert!(!reports_state(
            "/home/u/idle-tools/a status || true",
            "idle"
        ));
        assert!(reports_state("a state-report idle", "idle"));
        assert!(!reports_state("a state-report", "idle"));
    }
}
