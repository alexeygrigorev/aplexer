//! The on-disk config schema and its lenient version header, plus the
//! resolved launch a config produces.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use super::{ordered_unique_env_names, PROVIDER_ENV_UNSET_VARS};
use crate::{Limits, Paths};

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
pub(super) struct ConfigHeader {
    #[serde(default = "default_config_version")]
    version: u32,
    #[serde(default)]
    keep_exited: bool,
}

impl ConfigHeader {
    pub(super) fn parse(text: &str) -> Result<Self> {
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
