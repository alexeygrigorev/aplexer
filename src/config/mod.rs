//! Configuration: engine/profile/shortcut configuration loading and
//! validation, profile discovery rules, provider environment handling, and
//! launch resolution into an executable argv.

mod builtins;
mod discovery;
mod provider_env;
mod schema;

pub use builtins::*;
pub(crate) use discovery::*;
pub(crate) use provider_env::*;
pub use schema::*;

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use crate::{validate_history_bytes, validate_limits, Limits, Paths, DEFAULT_HISTORY_BYTES};

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
