//! Auto-discovery of sibling engine profile directories under `$HOME`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::ProfileConfig;
use crate::paths::home_dir;

/// A single engine's profile-discovery rule (spec.md 9.2 / 23: "Aplexer
/// should absorb PocketShell's existing profile discovery concepts"), ported
/// from PocketShell's `tools/pocketshell/src/pocketshell/profiles.py`.
pub(crate) struct ProfileDiscoveryRule {
    engine: &'static str,
    env_var: &'static str,
    default_dirname: &'static str,
    markers: &'static [&'static str],
    hints: &'static [&'static str],
}

/// Only claude and codex currently support a profile config dir (matches
/// PocketShell's `PROFILE_ENGINES`; opencode has no profile env var and grok
/// is not yet known to have one either, so neither is listed here).
pub(crate) const PROFILE_DISCOVERY_RULES: &[ProfileDiscoveryRule] = &[
    ProfileDiscoveryRule {
        engine: "claude",
        env_var: "CLAUDE_CONFIG_DIR",
        default_dirname: ".claude",
        markers: &[".claude.json", "settings.json"],
        hints: &["claude", "laude"],
    },
    ProfileDiscoveryRule {
        engine: "codex",
        env_var: "CODEX_HOME",
        default_dirname: ".codex",
        markers: &["config.toml", "auth.json"],
        hints: &["codex", "odex"],
    },
];

pub(crate) fn has_marker(dir: &Path, markers: &[&str]) -> bool {
    if !dir.is_dir() {
        return false;
    }
    markers.iter().any(|m| dir.join(m).is_file())
}

/// Auto-discovers non-default sibling profile dirs for claude/codex.
///
/// Conservative by construction, matching PocketShell's own discovery:
/// top-level `~/.<name>` dirs only, never recursive, a real marker file
/// required, and only directory-existence/marker-*name* checks -- this
/// never reads inside a config dir (that's where secrets such as
/// `auth.json` live).
///
/// Only the non-default sibling-dir case produces a `ProfileConfig`. An
/// engine's own default dir (e.g. `~/.claude`) deliberately gets no profile
/// entry: the engine's built-in command already resolves to that dir with
/// no `CLAUDE_CONFIG_DIR`/`CODEX_HOME` override needed, so a profile entry
/// for it would be a redundant no-op.
///
/// The returned map is keyed by the discovered directory's own stem minus
/// its leading dot (e.g. `~/.zlaude` -> `"zlaude"`), never by a humanized
/// display name -- `Config.profiles` is a single flat namespace shared by
/// every engine (unlike PocketShell's per-engine `Profile.name`), so two
/// engines' same-sounding profiles (e.g. both named "zai") would otherwise
/// silently clobber each other. A directory stem is collision-free by
/// construction: two different top-level dirs can never share a name.
pub(crate) fn discover_profiles() -> BTreeMap<String, ProfileConfig> {
    let mut out = BTreeMap::new();
    let home = match home_dir() {
        Ok(h) => h,
        Err(_) => return out,
    };
    let entries = match fs::read_dir(&home) {
        Ok(e) => e,
        Err(_) => return out,
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    names.sort();
    for rule in PROFILE_DISCOVERY_RULES {
        for stem in &names {
            if !stem.starts_with('.') || stem == rule.default_dirname {
                continue;
            }
            let lower = stem.to_ascii_lowercase();
            if !rule.hints.iter().any(|hint| lower.contains(hint)) {
                continue;
            }
            let dir = home.join(stem);
            if !has_marker(&dir, rule.markers) {
                continue;
            }
            let id = stem.trim_start_matches('.').to_string();
            if id.is_empty() {
                continue;
            }
            // A directory whose name and markers satisfy more than one
            // rule belongs to the first rule that claims it; a later rule
            // must not silently re-attribute it to another engine.
            out.entry(id).or_insert_with(|| {
                let mut env = BTreeMap::new();
                env.insert(rule.env_var.to_string(), dir.display().to_string());
                ProfileConfig {
                    engine: Some(rule.engine.to_string()),
                    env,
                    ..ProfileConfig::default()
                }
            });
        }
    }
    out
}
