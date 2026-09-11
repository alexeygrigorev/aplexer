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
//!   `session.status` (`busy`/`retry` → `working`, `idle` → `idle`) plus
//!   legacy `session.idle` → `idle`, `permission.asked` / `session.error` →
//!   `waiting`, `session.created` → `working`, and `tool.execute.before` →
//!   `working` (a starting tool means back to work). `session.status` is
//!   the authoritative signal — `session.idle` is deprecated — and it is
//!   what covers new prompts in an existing session, which `session.created`
//!   (once per session) does not.
//!
//! State vocabulary in every mapping is `a state-report`'s own
//! (`idle`/`waiting`/`working`, matching PocketShell's
//! Idle/WaitingForInput/Working one for one): a finished turn rests
//! (`idle`), a permission prompt or error needs the user (`waiting`), a
//! submitted prompt or (re)started session is back to work (`working`).
//! OpenCode is the one exception with a per-tool hook:
//! `tool.execute.before` → `working`. Its `session.status idle` can fire
//! mid-turn between think→tool steps, and `session.created` fires only once
//! per session, so without a tool-start signal a mid-turn `idle` blip (or a
//! second turn's work after the first turn's `idle`) had no follow-up push
//! to correct it. The other engines need no such hook: after an `idle` push
//! goes stale the PTY-activity heuristic takes over again and reports
//! `active` while the agent produces output, so the resume boundary needs
//! no hook of its own.
//!
//! Layout: this file holds the event tables, the hook command, and target
//! resolution; `nested` merges into the shared nested-hooks JSON shape,
//! `codex_notify` handles codex's legacy `notify` line, `files` is the
//! non-clobbering file layer, and `drivers` is the per-engine
//! install/check/uninstall table.

mod codex_notify;
mod drivers;
mod files;
mod nested;
#[cfg(test)]
mod tests;

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

pub use codex_notify::*;
pub use drivers::*;
pub(crate) use files::*;
pub use nested::*;
