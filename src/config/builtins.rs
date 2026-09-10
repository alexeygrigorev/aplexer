//! The engines and shortcuts every installation starts with.

use std::collections::BTreeMap;
use std::env;

use super::{Config, EngineConfig, ShortcutConfig};

/// Transcript-family normalization: a variant engine -- a fork of a built-in
/// engine CLI with the same wire format and the same native conversation-log
/// location -- is identified with that engine's family for parsing, while
/// sessions and emitted events keep the variant's own id. `zcodex` is a
/// codex-rs fork (same `-c` overrides, same rollout JSONL under
/// `CODEX_HOME`/`~/.codex`), so it rides the codex machinery; everything
/// else is its own family.
pub fn engine_family(engine: &str) -> &str {
    match engine {
        "zcodex" => "codex",
        other => other,
    }
}

impl Config {
    /// The engines every installation gets, before user config extends or
    /// overrides them. `zcodex` is a codex variant (see `engine_family`): a
    /// codex-rs fork with the same CLI surface and the same rollout log, so
    /// its launch spec mirrors codex's exactly, with the fork's own binary
    /// name. `opencode` is the PocketShell built-in
    /// (tools/pocketshell/src/pocketshell/engines.py ::builtin_manifests)
    /// that aplexer's engine set was missing -- required for aplexer to
    /// become authoritative for pocketshell's engine registry
    /// (pocketshell-integration-plan.md 0.1).
    pub(super) fn builtin_engines() -> BTreeMap<String, EngineConfig> {
        fn engine(command: &[&str], skip_permissions_argv: &[&str]) -> EngineConfig {
            let strings = |values: &[&str]| values.iter().map(|value| value.to_string()).collect();
            EngineConfig {
                command: strings(command),
                env: BTreeMap::new(),
                env_unset: Vec::new(),
                skip_permissions_argv: strings(skip_permissions_argv),
            }
        }
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        // Skip-permissions argv is ported from pocketshell engines.py's
        // LaunchSpecs. opencode has none there (permissions are config-driven
        // via opencode.json) and gemini is an aplexer-only extra with no
        // pocketshell source, so both stay empty.
        let engines: [(&str, &[&str], &[&str]); 7] = [
            ("shell", &[shell.as_str(), "-l"], &[]),
            (
                "codex",
                &["codex", "-c", "check_for_update_on_startup=false"],
                &["--dangerously-bypass-approvals-and-sandbox"],
            ),
            ("claude", &["claude"], &["--dangerously-skip-permissions"]),
            (
                "zcodex",
                &["zcodex", "-c", "check_for_update_on_startup=false"],
                &["--dangerously-bypass-approvals-and-sandbox"],
            ),
            ("gemini", &["gemini"], &[]),
            ("grok", &["grok"], &["--always-approve"]),
            ("opencode", &["opencode"], &[]),
        ];
        engines
            .into_iter()
            .map(|(name, command, skip)| (name.to_string(), engine(command, skip)))
            .collect()
    }

    /// Built-in quick-launch shortcuts (`a - <id>`, see cmd_quick_launch in
    /// src/bin/a.rs): short mnemonics onto an (engine, profile) pair. Same
    /// defaults-then-user-file-extends layering as engines/profiles, so
    /// `[shortcuts.<id>]` in the user's config can add new ones or override
    /// these. "cl"/"co"/"g" are the plain engines; "clz"/"coz"/"cog"
    /// additionally select the Z.AI/Go sibling profiles discovered above
    /// (ids match those profiles' own dir-stem ids).
    pub(super) fn builtin_shortcuts() -> BTreeMap<String, ShortcutConfig> {
        let mut shortcuts = BTreeMap::new();
        shortcuts.insert(
            "cl".into(),
            ShortcutConfig {
                engine: "claude".into(),
                profile: None,
            },
        );
        shortcuts.insert(
            "co".into(),
            ShortcutConfig {
                engine: "codex".into(),
                profile: None,
            },
        );
        shortcuts.insert(
            "g".into(),
            ShortcutConfig {
                engine: "grok".into(),
                profile: None,
            },
        );
        shortcuts
    }
}
