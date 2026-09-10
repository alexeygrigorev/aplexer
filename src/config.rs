//! Configuration: engine/profile/shortcut configuration loading and
//! validation, profile discovery rules, provider environment handling, and
//! launch resolution into an executable argv.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::env;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::paths::home_dir;
use crate::{validate_history_bytes, validate_limits, Limits, Paths, DEFAULT_HISTORY_BYTES};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Additional vars to unset at spawn time. Agent engines add these to the
    /// forced `PROVIDER_ENV_UNSET_VARS` union; the literal `shell` engine uses
    /// exactly this configured list so ordinary shell commands retain their
    /// ambient/provider environment unless the user explicitly opts out.
    #[serde(default)]
    pub env_unset: Vec<String>,
    /// Argv appended after `command` when skip-permissions is requested
    /// (ported from PocketShell's `LaunchSpec.skip_permissions_argv` /
    /// `engines.py::builtin_manifests`). Empty means the engine has no such
    /// flag (e.g. `opencode`, `shell`) -- permissions are config-driven or
    /// not applicable.
    #[serde(default)]
    pub skip_permissions_argv: Vec<String>,
}
impl EngineConfig {
    /// Effective unset policy for one named engine. `shell` is the deliberate
    /// non-agent exception; all other built-in and custom engine ids retain
    /// the subscription-auth provider stripping policy.
    pub fn resolved_env_unset(&self, engine_name: &str) -> Vec<String> {
        if engine_name == "shell" {
            ordered_unique_env_names(std::iter::empty(), &self.env_unset)
        } else {
            ordered_unique_env_names(PROVIDER_ENV_UNSET_VARS.iter().copied(), &self.env_unset)
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ProfileConfig {
    #[serde(default)]
    pub engine: Option<String>,
    /// Override just the engine's executable (argv[0]); engine default
    /// arguments and skip-permissions argv still apply.
    #[serde(default)]
    pub executable: Option<String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub history_bytes: Option<usize>,
    #[serde(default)]
    pub limits: Limits,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ShortcutConfig {
    pub engine: String,
    #[serde(default)]
    pub profile: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_config_version")]
    pub version: u32,
    #[serde(default)]
    pub default_engine: Option<String>,
    #[serde(default)]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub engines: BTreeMap<String, EngineConfig>,
    #[serde(default)]
    pub profiles: BTreeMap<String, ProfileConfig>,
    #[serde(default)]
    pub shortcuts: BTreeMap<String, ShortcutConfig>,
    /// Keep a durable record for a session whose workload finished.
    ///
    /// Default `false`: a session that ends -- `exit`, Ctrl-D at a shell,
    /// a workload that returned non-zero, a signalled workload, or `a kill`
    /// -- removes its own record and history, so it leaves `a list` the
    /// moment it is over instead of parking an `exited` row there until
    /// somebody runs `a prune`. Set `keep_exited = true` to retain those
    /// post-mortem records (`a status`, `a capture --screen`, the durable
    /// terminal transition a polling `a watch` can observe) and go back to
    /// pruning them explicitly.
    ///
    /// Never suppresses evidence the operator did not choose to lose:
    /// worker-side finalization failures, a containment domain that was
    /// not proven empty, and OOM kills keep their record regardless (see
    /// `run_lifecycle` in src/worker.rs).
    #[serde(default)]
    pub keep_exited: bool,
}
pub(crate) fn default_config_version() -> u32 {
    1
}

/// The version-gated prefix of a config file, parsed leniently: unknown
/// fields are ignored here so a future-version file reports "unsupported
/// config version" rather than tripping over the first field the strict
/// [`Config`] schema (`deny_unknown_fields`) does not know.
#[derive(Deserialize)]
struct ConfigHeader {
    #[serde(default = "default_config_version")]
    version: u32,
    #[serde(default)]
    keep_exited: bool,
}

impl ConfigHeader {
    fn parse(text: &str) -> Result<Self> {
        let header: Self = toml::from_str(text)?;
        if header.version != 1 {
            bail!("unsupported config version {}", header.version);
        }
        Ok(header)
    }
}

/// Read the `keep_exited` policy without building a full [`Config`].
///
/// The worker consults this on its exit path, where [`Config::load`] would
/// be the wrong tool twice over: it walks the filesystem for engine profile
/// discovery that finalization has no use for, and it fails the whole load
/// on an unrelated invalid engine/profile/shortcut entry -- which would flip
/// the retention policy as a side effect of a typo elsewhere in the file.
/// Only the [`ConfigHeader`] is parsed here, so an unreadable or unparsable
/// config file, one of an unsupported version (whose field may not mean
/// the same thing), and a missing one (the common case) all mean the
/// documented default: do not keep exited records.
///
/// The `Config` field above stays the source of truth for the name and the
/// default; `config_keep_exited_matches_full_config_load` pins the two
/// readers together.
pub fn config_keep_exited(paths: &Paths) -> bool {
    fs::read_to_string(&paths.config_file)
        .ok()
        .and_then(|text| ConfigHeader::parse(&text).ok())
        .map(|header| header.keep_exited)
        .unwrap_or(false)
}

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

/// Provider API-key-style env vars unset for every agent-engine launch, so
/// the agent falls back to its subscription auth instead of a per-token env
/// key. Ported verbatim (same order) from PocketShell's
/// `tools/pocketshell/src/pocketshell/engines.py::PROVIDER_ENV_UNSET_VARS`
/// (maintainer decision, pocketshell issue #703 -- subscription billing
/// across the board for codex/claude/opencode). `Config::resolve` unions
/// this with each agent engine's own `EngineConfig.env_unset`; the union is
/// forced for every engine id except the literal `shell` engine (see that
/// function's doc comment) -- an agent config can only add to this list,
/// never remove from it.
pub(crate) const PROVIDER_ENV_UNSET_VARS: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_PROFILE",
    "AWS_REGION",
    "AWS_BEARER_TOKEN_BEDROCK",
    "AWS_WEB_IDENTITY_TOKEN_FILE",
    "AWS_ROLE_ARN",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_ORG_ID",
    "OPENAI_PROJECT_ID",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "GROQ_API_KEY",
    "GOOGLE_APPLICATION_CREDENTIALS",
    "GOOGLE_CLOUD_PROJECT",
    "GOOGLE_API_KEY",
    "VERTEX_LOCATION",
    "VERTEX_AI_PROJECT",
    "DEEPSEEK_API_KEY",
    "XAI_API_KEY",
    "FIREWORKS_API_KEY",
    "CEREBRAS_API_KEY",
    "OPENROUTER_API_KEY",
    "TOGETHER_API_KEY",
    "TOGETHER_AI_API_KEY",
    "AZURE_API_KEY",
    "AZURE_RESOURCE_NAME",
    "AZURE_COGNITIVE_SERVICES_RESOURCE_NAME",
    "AZURE_OPENAI_API_KEY",
    "AZURE_OPENAI_ENDPOINT",
    "CLOUDFLARE_API_TOKEN",
    "CLOUDFLARE_ACCOUNT_ID",
    "CLOUDFLARE_GATEWAY_ID",
    "CLOUDFLARE_API_KEY",
    "HUGGING_FACE_API_KEY",
    "HF_TOKEN",
    "HF_API_TOKEN",
    "MOONSHOT_API_KEY",
    "MOONSHOTAI_API_KEY",
    "MINIMAX_API_KEY",
    "NEBIUS_API_KEY",
    "DEEPINFRA_API_KEY",
    "BASETEN_API_KEY",
    "VENICE_API_KEY",
    "SCALEWAY_API_KEY",
    "OVH_API_KEY",
    "CORTECS_API_KEY",
    "IONET_API_KEY",
    "VERCEL_API_KEY",
    "ZENMUX_API_KEY",
    "ZAI_API_KEY",
    "HELICONE_API_KEY",
    "OPENCODE_API_KEY",
    "OPENCODE_ZEN_API_KEY",
    "GITLAB_TOKEN",
    "GITLAB_INSTANCE_URL",
    "GITLAB_AI_GATEWAY_URL",
    "GITLAB_OAUTH_CLIENT_ID",
    "AICORE_SERVICE_KEY",
    "AICORE_DEPLOYMENT_ID",
    "AICORE_RESOURCE_GROUP",
    "OPENAI_COMPATIBLE_API_KEY",
    "LMSTUDIO_API_KEY",
    "OLLAMA_API_KEY",
    "302AI_API_KEY",
    "FIRMWARE_API_KEY",
    "2AI_API_KEY",
    "GEMINI_API_KEY",
];

/// Preserve first-seen order and drop blanks/duplicates across the policy
/// prefix and configured additions.
pub(crate) fn ordered_unique_env_names<'a>(
    prefix: impl IntoIterator<Item = &'a str>,
    extra: &[String],
) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |name: &str| {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return;
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    };
    for name in prefix {
        push(name);
    }
    for name in extra {
        push(name);
    }
    out
}

impl Config {
    fn validate(&self) -> Result<()> {
        if let Some(name) = &self.default_engine {
            if !self.engines.contains_key(name) {
                bail!("default_engine {name:?} does not reference a configured engine");
            }
        }
        if let Some(name) = &self.default_profile {
            if !self.profiles.contains_key(name) {
                bail!("default_profile {name:?} does not reference a configured profile");
            }
        }

        for (name, engine) in &self.engines {
            if name.is_empty() {
                bail!("engine name must not be empty");
            }
            if engine.command.is_empty() {
                bail!("engine {name:?} command must not be empty");
            }
            if engine.command[0].is_empty() {
                bail!("engine {name:?} command executable must not be empty");
            }
        }

        for (name, profile) in &self.profiles {
            if name.is_empty() {
                bail!("profile name must not be empty");
            }
            if let Some(engine) = &profile.engine {
                if !self.engines.contains_key(engine) {
                    bail!(
                        "profile {name:?} engine {engine:?} does not reference a configured engine"
                    );
                }
            }
            if profile.executable.as_deref() == Some("") {
                bail!("profile {name:?} executable must not be empty");
            }
            if let Some(command) = &profile.command {
                if command.is_empty() {
                    bail!("profile {name:?} command must not be empty");
                }
                if command[0].is_empty() {
                    bail!("profile {name:?} command executable must not be empty");
                }
                if profile.executable.is_some() {
                    bail!("profile {name:?} cannot set both command and executable");
                }
                if !profile.args.is_empty() {
                    bail!("profile {name:?} cannot set both command and args");
                }
            }
            if let Some(history_bytes) = profile.history_bytes {
                validate_history_bytes(history_bytes)
                    .with_context(|| format!("profile {name:?} history_bytes"))?;
            }
            validate_limits(&profile.limits, &format!("profile {name:?} limits"))?;
        }

        for (name, shortcut) in &self.shortcuts {
            if name.is_empty() {
                bail!("shortcut name must not be empty");
            }
            if !self.engines.contains_key(&shortcut.engine) {
                bail!(
                    "shortcut {name:?} engine {:?} does not reference a configured engine",
                    shortcut.engine
                );
            }
            if let Some(profile_name) = &shortcut.profile {
                let profile = self.profiles.get(profile_name).ok_or_else(|| {
                    anyhow!(
                        "shortcut {name:?} profile {profile_name:?} does not reference a configured profile"
                    )
                })?;
                if let Some(profile_engine) = &profile.engine {
                    if profile_engine != &shortcut.engine {
                        bail!(
                            "shortcut {name:?} selects engine {:?}, but profile {profile_name:?} selects engine {profile_engine:?}",
                            shortcut.engine
                        );
                    }
                }
            }
        }
        Ok(())
    }

    pub fn load(paths: &Paths) -> Result<Self> {
        let mut config = Config {
            version: 1,
            default_engine: Some("shell".into()),
            engines: Self::builtin_engines(),
            // Auto-discovered profiles (spec.md 9.2/23) go in as defaults
            // before the user's file is merged, exactly like the built-in
            // engines above -- an explicit `[profiles.<id>]` entry in the
            // user's config still wins on the extend in merge_user_file.
            profiles: discover_profiles(),
            shortcuts: Self::builtin_shortcuts(),
            ..Config::default()
        };
        config.merge_user_file(paths)?;
        config.add_profile_shortcuts();
        config.validate()?;
        Ok(config)
    }

    /// The engines every installation gets, before user config extends or
    /// overrides them. `zcodex` is a codex variant (see `engine_family`): a
    /// codex-rs fork with the same CLI surface and the same rollout log, so
    /// its launch spec mirrors codex's exactly, with the fork's own binary
    /// name. `opencode` is the PocketShell built-in
    /// (tools/pocketshell/src/pocketshell/engines.py ::builtin_manifests)
    /// that aplexer's engine set was missing -- required for aplexer to
    /// become authoritative for pocketshell's engine registry
    /// (pocketshell-integration-plan.md 0.1).
    fn builtin_engines() -> BTreeMap<String, EngineConfig> {
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
    fn builtin_shortcuts() -> BTreeMap<String, ShortcutConfig> {
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

    /// Merges the user's config file over the built-in defaults. Options
    /// with an "unset" state (default engine/profile) only override when
    /// set; maps extend so a user entry wins on key collision.
    fn merge_user_file(&mut self, paths: &Paths) -> Result<()> {
        let text = match fs::read_to_string(&paths.config_file) {
            Ok(text) => text,
            // No file is the common case and means "defaults". Any other
            // failure (EACCES, ENOTDIR, ...) is a file the user wrote and
            // we could not honour; it must not silently become defaults.
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", paths.config_file.display()))
            }
        };
        ConfigHeader::parse(&text)
            .with_context(|| format!("parse {}", paths.config_file.display()))?;
        let user: Config = toml::from_str(&text)
            .with_context(|| format!("parse {}", paths.config_file.display()))?;
        if user.default_engine.is_some() {
            self.default_engine = user.default_engine;
        }
        if user.default_profile.is_some() {
            self.default_profile = user.default_profile;
        }
        self.engines.extend(user.engines);
        self.profiles.extend(user.profiles);
        self.shortcuts.extend(user.shortcuts);
        // A bool has no "unset" value to test the way the options above
        // do, and the built-in default is `false`, so the user's parsed
        // value simply is the answer.
        self.keep_exited = user.keep_exited;
        Ok(())
    }

    /// Profile-specific built-ins are useful only when discovery or user
    /// config supplied their target profile. Insert them after merging so
    /// they never create dangling references, while an explicit user
    /// shortcut with the same id still wins.
    fn add_profile_shortcuts(&mut self) {
        for (shortcut, engine, profile) in [
            ("clz", "claude", "zlaude"),
            ("coz", "codex", "zodex"),
            ("cog", "codex", "godex"),
        ] {
            if self.profiles.contains_key(profile) {
                self.shortcuts
                    .entry(shortcut.into())
                    .or_insert_with(|| ShortcutConfig {
                        engine: engine.into(),
                        profile: Some(profile.into()),
                    });
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn resolve(
        &self,
        direct: Vec<String>,
        engine_name: Option<&str>,
        profile_name: Option<&str>,
        workspace: &Path,
        cwd: Option<&Path>,
        env_overrides: &BTreeMap<String, String>,
        limits: &Limits,
        history_bytes: Option<usize>,
    ) -> Result<ResolvedLaunch> {
        let selected_profile = profile_name
            .map(str::to_owned)
            .or_else(|| self.default_profile.clone());
        let profile = match selected_profile.as_deref() {
            Some(name) => Some(
                self.profiles
                    .get(name)
                    .ok_or_else(|| anyhow!("unknown profile {name}"))?,
            ),
            None => None,
        };
        let selected_engine = engine_name
            .map(str::to_owned)
            .or_else(|| profile.and_then(|p| p.engine.clone()))
            .or_else(|| self.default_engine.clone())
            .unwrap_or_else(|| "shell".into());
        let engine = self
            .engines
            .get(&selected_engine)
            .ok_or_else(|| anyhow!("unknown engine {selected_engine}"))?;
        let command = build_argv(direct, engine, profile);
        if command.is_empty() {
            bail!("engine {selected_engine} has no command");
        }
        let mut merged_env = engine.env.clone();
        if let Some(p) = profile {
            merged_env.extend(p.env.clone());
        }
        merged_env.extend(env_overrides.clone());
        let merged_limits = merge_limits(
            profile.map(|p| p.limits.clone()).unwrap_or_default(),
            limits,
        );
        validate_limits(&merged_limits, "resolved launch limits")?;
        let launch_cwd = cwd
            .map(Path::to_path_buf)
            .or_else(|| profile.and_then(|p| p.cwd.clone()))
            .unwrap_or_else(|| workspace.to_path_buf());
        // Forced provider-key union (pocketshell-integration-plan.md 1.4/0.2)
        // for agent engines: `PROVIDER_ENV_UNSET_VARS` comes first regardless
        // of a custom engine's additions. The exact `shell` engine id is the
        // deliberate exception: literal shells use only their configured
        // env_unset list, so explicit shell --env values are not silently
        // removed. Callers apply this after env_set at workload spawn.
        let env_unset = engine.resolved_env_unset(&selected_engine);
        let history_bytes = history_bytes
            .or_else(|| profile.and_then(|p| p.history_bytes))
            .unwrap_or(DEFAULT_HISTORY_BYTES);
        validate_history_bytes(history_bytes)?;
        Ok(ResolvedLaunch {
            engine: selected_engine,
            profile: selected_profile,
            command,
            cwd: launch_cwd,
            env: merged_env,
            env_unset,
            skip_permissions_argv: engine.skip_permissions_argv.clone(),
            limits: merged_limits,
            history_bytes,
        })
    }
}

/// The launch argv before skip-permissions handling: an explicit `-- argv`
/// verbatim; else the profile's whole `command`; else the engine's command
/// with the profile's `executable` swapped into argv[0] and its `args`
/// appended. (`validate` forbids an engine command without an argv[0].)
fn build_argv(
    direct: Vec<String>,
    engine: &EngineConfig,
    profile: Option<&ProfileConfig>,
) -> Vec<String> {
    if !direct.is_empty() {
        return direct;
    }
    if let Some(command) = profile.and_then(|p| p.command.clone()) {
        return command;
    }
    let mut argv = engine.command.clone();
    if let Some(profile) = profile {
        if let (Some(first), Some(executable)) = (argv.first_mut(), &profile.executable) {
            *first = executable.clone();
        }
        argv.extend(profile.args.iter().cloned());
    }
    argv
}

/// `base` (the profile's limits) with every explicitly requested launch
/// limit laid over it.
fn merge_limits(base: Limits, overrides: &Limits) -> Limits {
    Limits {
        memory_bytes: overrides.memory_bytes.or(base.memory_bytes),
        pids: overrides.pids.or(base.pids),
        cpu_quota_us: overrides.cpu_quota_us.or(base.cpu_quota_us),
        cpu_period_us: overrides.cpu_period_us.or(base.cpu_period_us),
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedLaunch {
    pub engine: String,
    pub profile: Option<String>,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    /// Effective vars that must be absent from the spawned workload, applied
    /// AFTER `env` at spawn time. Agent engines receive the forced provider
    /// union; the `shell` engine receives only its explicitly configured
    /// `env_unset` entries.
    pub env_unset: Vec<String>,
    /// Argv to append to `command` when skip-permissions is requested (see
    /// `EngineConfig::skip_permissions_argv`). `a start` appends this by
    /// default (unless `--no-skip-permissions` or an explicit `-- argv`);
    /// `a launch-spec`/`a launch-exec` do the same.
    pub skip_permissions_argv: Vec<String>,
    pub limits: Limits,
    pub history_bytes: usize,
}

pub fn executable_available(program: &str) -> bool {
    fn is_executable_file(path: &Path) -> bool {
        let Ok(metadata) = fs::metadata(path) else {
            return false;
        };
        if !metadata.is_file() {
            return false;
        }
        let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
            return false;
        };
        unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
    }

    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return is_executable_file(candidate);
    }
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| is_executable_file(&dir.join(program))))
        .unwrap_or(false)
}
