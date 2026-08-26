#![cfg(target_os = "linux")]

pub mod watch;
pub mod worker;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::ffi::{CString, OsStr};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const SCHEMA_VERSION: u32 = 1;
pub const PROTOCOL_VERSION: u16 = 1;
pub const DEFAULT_HISTORY_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Debug, Clone)]
pub struct Paths {
    pub runtime_root: PathBuf,
    pub state_root: PathBuf,
    pub config_file: PathBuf,
}

impl Paths {
    pub fn discover() -> Result<Self> {
        let uid = unsafe { libc::geteuid() };
        let runtime_root = if let Some(value) = env::var_os("APLEXER_RUNTIME_DIR") {
            PathBuf::from(value)
        } else if let Some(value) = env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(value).join("aplexer")
        } else {
            PathBuf::from(format!("/tmp/aplexer-{uid}"))
        };
        let state_root = if let Some(value) = env::var_os("APLEXER_STATE_DIR") {
            PathBuf::from(value)
        } else if let Some(value) = env::var_os("XDG_STATE_HOME") {
            PathBuf::from(value).join("aplexer")
        } else {
            home_dir()?.join(".local/state/aplexer")
        };
        let config_file = if let Some(value) = env::var_os("APLEXER_CONFIG") {
            PathBuf::from(value)
        } else if let Some(value) = env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(value).join("aplexer/config.toml")
        } else {
            home_dir()?.join(".config/aplexer/config.toml")
        };
        let paths = Self {
            runtime_root,
            state_root,
            config_file,
        };
        paths.ensure()?;
        Ok(paths)
    }

    pub fn ensure(&self) -> Result<()> {
        ensure_private_dir(&self.runtime_root)?;
        ensure_private_dir(&self.runtime_root.join("sessions"))?;
        ensure_private_dir(&self.state_root)?;
        ensure_private_dir(&self.state_root.join("sessions"))?;
        Ok(())
    }

    pub fn runtime_session(&self, id: Uuid) -> PathBuf {
        self.runtime_root.join("sessions").join(id.to_string())
    }
    pub fn state_session(&self, id: Uuid) -> PathBuf {
        self.state_root.join("sessions").join(id.to_string())
    }
    pub fn socket(&self, id: Uuid) -> PathBuf {
        self.runtime_session(id).join("control.sock")
    }
    pub fn record(&self, id: Uuid) -> PathBuf {
        self.state_session(id).join("session.json")
    }
    pub fn history(&self, id: Uuid) -> PathBuf {
        self.state_session(id).join("history.bin")
    }
    pub fn worker_lock(&self, id: Uuid) -> PathBuf {
        self.runtime_session(id).join("worker.lock")
    }
    pub fn registry_lock(&self) -> PathBuf {
        self.state_root.join("registry.lock")
    }
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

pub fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", path.display()))?;
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_dir() || meta.file_type().is_symlink() {
        bail!("{} is not a real directory", path.display());
    }
    let uid = unsafe { libc::geteuid() };
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != uid {
        bail!(
            "{} is owned by uid {}, expected {}",
            path.display(),
            meta.uid(),
            uid
        );
    }
    Ok(())
}

pub fn canonical_workspace(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    fs::canonicalize(&absolute).or_else(|_| {
        let parent = absolute
            .parent()
            .ok_or_else(|| anyhow!("invalid workspace"))?;
        let leaf = absolute
            .file_name()
            .ok_or_else(|| anyhow!("invalid workspace"))?;
        Ok::<PathBuf, anyhow::Error>(fs::canonicalize(parent)?.join(leaf))
    })
}

pub fn validate_tag(tag: &str) -> Result<()> {
    if tag.is_empty() || tag.len() > 64 {
        bail!("tag must contain 1..64 bytes");
    }
    if !tag
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("tag may contain only ASCII letters, digits, '.', '_' and '-'");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota_us: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_period_us: Option<u64>,
}

impl Limits {
    pub fn requested(&self) -> bool {
        self.memory_bytes.is_some() || self.pids.is_some() || self.cpu_quota_us.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Starting,
    Running,
    Exiting,
    Exited,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub oom_killed: bool,
    pub exited_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub schema_version: u32,
    pub id: Uuid,
    pub workspace: PathBuf,
    pub tag: String,
    pub engine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub limits: Limits,
    pub history_bytes: usize,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// Last time the workload's PTY produced output, as observed by the
    /// worker (see worker.rs's periodic debounce thread) -- deliberately a
    /// separate field from `updated_at_ms`, which already fires on unrelated
    /// record writes (phase transitions, rename, ...) and would be a noisy,
    /// misleading proxy for "is this session's PTY currently active" if
    /// reused for that purpose. `a watch`'s `agent.state` heuristic
    /// (running/waiting) is built on this field; see its doc comments for
    /// the important caveat that PTY-output recency is a coarse proxy for
    /// activity, not true agent-semantic state (spec.md section 20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<u64>,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_pid: Option<u32>,
    pub socket_path: PathBuf,
    pub history_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<ExitInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl SessionRecord {
    pub fn selector(&self) -> String {
        format!("{}:{}", self.workspace.display(), self.tag)
    }
}

pub fn read_record(path: &Path) -> Result<SessionRecord> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let record: SessionRecord =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    if record.schema_version != SCHEMA_VERSION {
        bail!("unsupported session schema {}", record.schema_version);
    }
    Ok(record)
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    ensure_private_dir(parent)?;
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .unwrap_or(OsStr::new("record"))
            .to_string_lossy(),
        std::process::id(),
        seq
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .with_context(|| format!("create {}", temp.display()))?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temp, path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    ensure_private_dir(parent)?;
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".history.{}.{}.tmp", std::process::id(), seq));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub struct FileLock {
    file: File,
}
impl FileLock {
    pub fn exclusive(path: &Path, nonblocking: bool) -> Result<Self> {
        let parent = path.parent().ok_or_else(|| anyhow!("lock has no parent"))?;
        ensure_private_dir(parent)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(path)?;
        let mut op = libc::LOCK_EX;
        if nonblocking {
            op |= libc::LOCK_NB;
        }
        if unsafe { libc::flock(file.as_raw_fd(), op) } != 0 {
            return Err(io::Error::last_os_error())
                .with_context(|| format!("lock {}", path.display()));
        }
        Ok(Self { file })
    }
}
impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn list_records(paths: &Paths) -> Result<Vec<SessionRecord>> {
    let mut out = Vec::new();
    let root = paths.state_root.join("sessions");
    for entry in fs::read_dir(root)? {
        let entry = match entry {
            Ok(v) => v,
            Err(_) => continue,
        };
        let path = entry.path().join("session.json");
        if let Ok(record) = read_record(&path) {
            out.push(record);
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
    Ok(out)
}

pub fn process_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn resolve_record(
    paths: &Paths,
    selector: Option<&str>,
    workspace: Option<&Path>,
    tag: Option<&str>,
) -> Result<SessionRecord> {
    let records = list_records(paths)?;
    let mut matches = Vec::new();
    if let Some(raw) = selector {
        let needle = raw.to_ascii_lowercase();
        for record in records {
            let id = record.id.to_string();
            if id == needle
                || (needle.len() >= 8 && id.starts_with(&needle))
                || record.selector() == raw
            {
                matches.push(record);
            }
        }
        if matches.is_empty() {
            if let Some((workspace_text, tag_text)) = raw.rsplit_once(':') {
                if let Ok(ws) = canonical_workspace(Path::new(workspace_text)) {
                    for record in list_records(paths)? {
                        if record.workspace == ws && record.tag == tag_text {
                            matches.push(record);
                        }
                    }
                }
            }
        }
    } else {
        let ws = canonical_workspace(workspace.unwrap_or(Path::new(".")))?;
        let tag = tag.unwrap_or("default");
        for record in records {
            if record.workspace == ws && record.tag == tag {
                matches.push(record);
            }
        }
    }
    match matches.len() {
        0 => bail!("no matching session"),
        1 => Ok(matches.remove(0)),
        _ => bail!("selector is ambiguous; use a longer UUID"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EngineConfig {
    pub command: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProfileConfig {
    #[serde(default)]
    pub engine: Option<String>,
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
pub struct ShortcutConfig {
    pub engine: String,
    #[serde(default)]
    pub profile: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
}
fn default_config_version() -> u32 {
    1
}

/// A single engine's profile-discovery rule (spec.md 9.2 / 23: "Aplexer
/// should absorb PocketShell's existing profile discovery concepts"), ported
/// from PocketShell's `tools/pocketshell/src/pocketshell/profiles.py`.
struct ProfileDiscoveryRule {
    engine: &'static str,
    env_var: &'static str,
    default_dirname: &'static str,
    markers: &'static [&'static str],
    hints: &'static [&'static str],
}

/// Only claude and codex currently support a profile config dir (matches
/// PocketShell's `PROFILE_ENGINES`; opencode has no profile env var and grok
/// is not yet known to have one either, so neither is listed here).
const PROFILE_DISCOVERY_RULES: &[ProfileDiscoveryRule] = &[
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

fn has_marker(dir: &Path, markers: &[&str]) -> bool {
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
fn discover_profiles() -> BTreeMap<String, ProfileConfig> {
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
            let mut env = BTreeMap::new();
            env.insert(rule.env_var.to_string(), dir.display().to_string());
            out.insert(
                id,
                ProfileConfig {
                    engine: Some(rule.engine.to_string()),
                    env,
                    ..ProfileConfig::default()
                },
            );
        }
    }
    out
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let shell = env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut config = Config::default();
        config.version = 1;
        config.default_engine = Some("shell".into());
        config.engines.insert(
            "shell".into(),
            EngineConfig {
                command: vec![shell, "-l".into()],
                env: BTreeMap::new(),
            },
        );
        config.engines.insert(
            "codex".into(),
            EngineConfig {
                command: vec!["codex".into()],
                env: BTreeMap::new(),
            },
        );
        config.engines.insert(
            "claude".into(),
            EngineConfig {
                command: vec!["claude".into()],
                env: BTreeMap::new(),
            },
        );
        config.engines.insert(
            "gemini".into(),
            EngineConfig {
                command: vec!["gemini".into()],
                env: BTreeMap::new(),
            },
        );
        config.engines.insert(
            "grok".into(),
            EngineConfig {
                command: vec!["grok".into()],
                env: BTreeMap::new(),
            },
        );
        // Auto-discovered profiles (spec.md 9.2/23) go in as defaults before
        // the user's file is merged, exactly like the built-in engines above
        // -- an explicit `[profiles.<id>]` entry in the user's config still
        // wins on a key collision via the `extend()` below.
        config.profiles.extend(discover_profiles());
        // Built-in quick-launch shortcuts (`a - <id>`, see cmd_quick_launch
        // in src/bin/a.rs): short mnemonics onto an (engine, profile) pair.
        // Same defaults-then-user-file-extends layering as engines/profiles
        // above, so `[shortcuts.<id>]` in the user's config can add new ones
        // or override these. "cl"/"co"/"g" are the plain engines; "clz"/
        // "coz"/"cog" additionally select the Z.AI/Go sibling profiles
        // discovered above (ids match those profiles' own dir-stem ids).
        config.shortcuts.insert(
            "cl".into(),
            ShortcutConfig {
                engine: "claude".into(),
                profile: None,
            },
        );
        config.shortcuts.insert(
            "co".into(),
            ShortcutConfig {
                engine: "codex".into(),
                profile: None,
            },
        );
        config.shortcuts.insert(
            "g".into(),
            ShortcutConfig {
                engine: "grok".into(),
                profile: None,
            },
        );
        config.shortcuts.insert(
            "clz".into(),
            ShortcutConfig {
                engine: "claude".into(),
                profile: Some("zlaude".into()),
            },
        );
        config.shortcuts.insert(
            "coz".into(),
            ShortcutConfig {
                engine: "codex".into(),
                profile: Some("zodex".into()),
            },
        );
        config.shortcuts.insert(
            "cog".into(),
            ShortcutConfig {
                engine: "codex".into(),
                profile: Some("godex".into()),
            },
        );
        if paths.config_file.exists() {
            let text = fs::read_to_string(&paths.config_file)?;
            let user: Config = toml::from_str(&text)
                .with_context(|| format!("parse {}", paths.config_file.display()))?;
            if user.version != 1 {
                bail!("unsupported config version {}", user.version);
            }
            if user.default_engine.is_some() {
                config.default_engine = user.default_engine;
            }
            if user.default_profile.is_some() {
                config.default_profile = user.default_profile;
            }
            config.engines.extend(user.engines);
            config.profiles.extend(user.profiles);
            config.shortcuts.extend(user.shortcuts);
        }
        Ok(config)
    }

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
        let profile = selected_profile
            .as_ref()
            .and_then(|name| self.profiles.get(name));
        if selected_profile.is_some() && profile.is_none() {
            bail!("unknown profile {}", selected_profile.as_deref().unwrap());
        }
        let selected_engine = engine_name
            .map(str::to_owned)
            .or_else(|| profile.and_then(|p| p.engine.clone()))
            .or_else(|| self.default_engine.clone())
            .unwrap_or_else(|| "shell".into());
        let engine = self
            .engines
            .get(&selected_engine)
            .ok_or_else(|| anyhow!("unknown engine {selected_engine}"))?;
        let direct_supplied = !direct.is_empty();
        let mut command = if direct_supplied {
            direct
        } else if let Some(cmd) = profile.and_then(|p| p.command.clone()) {
            cmd
        } else {
            engine.command.clone()
        };
        if command.is_empty() {
            bail!("engine {selected_engine} has no command");
        }
        if !direct_supplied {
            if let Some(p) = profile {
                if p.command.is_none() {
                    command.extend(p.args.clone());
                }
            }
        }
        let mut merged_env = engine.env.clone();
        if let Some(p) = profile {
            merged_env.extend(p.env.clone());
        }
        merged_env.extend(env_overrides.clone());
        let mut merged_limits = profile.map(|p| p.limits.clone()).unwrap_or_default();
        if limits.memory_bytes.is_some() {
            merged_limits.memory_bytes = limits.memory_bytes;
        }
        if limits.pids.is_some() {
            merged_limits.pids = limits.pids;
        }
        if limits.cpu_quota_us.is_some() {
            merged_limits.cpu_quota_us = limits.cpu_quota_us;
        }
        if limits.cpu_period_us.is_some() {
            merged_limits.cpu_period_us = limits.cpu_period_us;
        }
        let launch_cwd = cwd
            .map(Path::to_path_buf)
            .or_else(|| profile.and_then(|p| p.cwd.clone()))
            .unwrap_or_else(|| workspace.to_path_buf());
        Ok(ResolvedLaunch {
            engine: selected_engine,
            profile: selected_profile,
            command,
            cwd: launch_cwd,
            env: merged_env,
            limits: merged_limits,
            history_bytes: history_bytes
                .or_else(|| profile.and_then(|p| p.history_bytes))
                .unwrap_or(DEFAULT_HISTORY_BYTES),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedLaunch {
    pub engine: String,
    pub profile: Option<String>,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub limits: Limits,
    pub history_bytes: usize,
}

pub fn executable_available(program: &str) -> bool {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return candidate.is_file();
    }
    env::var_os("PATH")
        .map(|path| env::split_paths(&path).any(|dir| dir.join(program).is_file()))
        .unwrap_or(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    Json = 1,
    Data = 2,
    End = 3,
}
#[derive(Debug)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

pub fn write_frame<W: Write>(writer: &mut W, kind: FrameKind, payload: &[u8]) -> Result<()> {
    if payload.len() > MAX_FRAME_BYTES {
        bail!("frame too large: {}", payload.len());
    }
    let mut header = [0u8; 12];
    header[..4].copy_from_slice(b"APX1");
    header[4] = kind as u8;
    header[8..12].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Frame>> {
    let mut header = [0u8; 12];
    match reader.read(&mut header[..1]) {
        Ok(0) => return Ok(None),
        Ok(1) => {}
        Ok(_) => unreachable!(),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => return read_frame(reader),
        Err(e) => return Err(e.into()),
    }
    reader.read_exact(&mut header[1..])?;
    if &header[..4] != b"APX1" {
        bail!("invalid protocol magic");
    }
    if header[5..8] != [0, 0, 0] {
        bail!("unsupported frame flags");
    }
    let kind = match header[4] {
        1 => FrameKind::Json,
        2 => FrameKind::Data,
        3 => FrameKind::End,
        n => bail!("unknown frame type {n}"),
    };
    let length = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
    if length > MAX_FRAME_BYTES {
        bail!("frame exceeds maximum");
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    Ok(Some(Frame { kind, payload }))
}

pub fn write_json<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<()> {
    write_frame(writer, FrameKind::Json, &serde_json::to_vec(value)?)
}
pub fn frame_json<T: for<'de> Deserialize<'de>>(frame: Frame) -> Result<T> {
    if frame.kind != FrameKind::Json {
        bail!("expected JSON frame");
    }
    Ok(serde_json::from_slice(&frame.payload)?)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub version: u16,
    pub request_id: String,
    #[serde(flatten)]
    pub operation: Operation,
}
impl Request {
    pub fn new(operation: Operation) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: Uuid::new_v4().to_string(),
            operation,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Ping,
    Status,
    Send { bytes: usize },
    Capture { max_bytes: Option<usize> },
    Attach { history_bytes: Option<usize> },
    Resize { rows: u16, cols: u16 },
    Kill { signal: i32, grace_ms: u64 },
    Rename { workspace: PathBuf, tag: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub version: u16,
    pub request_id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
impl Response {
    pub fn ok(id: impl Into<String>, value: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: id.into(),
            ok: true,
            result: Some(value),
            error: None,
        }
    }
    pub fn error(id: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: id.into(),
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
    pub fn into_result(self) -> Result<Value> {
        if self.ok {
            Ok(self.result.unwrap_or(Value::Null))
        } else {
            bail!("{}", self.error.unwrap_or_else(|| "request failed".into()))
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ServerEvent {
    Exit { exit: ExitInfo },
    Error { message: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AttachControl {
    Resize { rows: u16, cols: u16 },
    Signal { signal: i32 },
    Detach,
}

/// How stale the persisted history file may get behind the in-memory ring.
/// Live reads (capture/attach) are always served from memory; the file only
/// matters after the worker is gone, so a worker crash loses at most this
/// much of the tail.
pub const HISTORY_FLUSH_INTERVAL: Duration = Duration::from_millis(500);

pub struct History {
    path: PathBuf,
    cap: usize,
    bytes: VecDeque<u8>,
    dirty: bool,
    last_flush: Instant,
}
impl History {
    pub fn open(path: PathBuf, cap: usize) -> Result<Self> {
        let existing = fs::read(&path).unwrap_or_default();
        let start = existing.len().saturating_sub(cap);
        let bytes = existing[start..].iter().copied().collect();
        Ok(Self {
            path,
            cap,
            bytes,
            dirty: false,
            last_flush: Instant::now(),
        })
    }
    /// Appends to the in-memory ring and persists at most once per
    /// HISTORY_FLUSH_INTERVAL. Persisting on every append rewrote the whole
    /// file (up to `cap` bytes) with two fsyncs for every PTY read of at
    /// most 32KB -- measured >100x write amplification whose backpressure
    /// throttled the workload itself to ~150KB/s of terminal output (11x
    /// slower than with persistence disabled). Callers must arrange a final
    /// flush() when output ends.
    pub fn append(&mut self, data: &[u8]) -> Result<()> {
        if self.cap == 0 {
            return Ok(());
        }
        if data.len() >= self.cap {
            self.bytes.clear();
            self.bytes
                .extend(data[data.len() - self.cap..].iter().copied());
        } else {
            self.bytes.extend(data.iter().copied());
            while self.bytes.len() > self.cap {
                self.bytes.pop_front();
            }
        }
        self.dirty = true;
        if self.last_flush.elapsed() >= HISTORY_FLUSH_INTERVAL {
            self.flush()?;
        }
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        let contiguous: Vec<u8> = self.bytes.iter().copied().collect();
        atomic_write_bytes(&self.path, &contiguous)?;
        self.dirty = false;
        self.last_flush = Instant::now();
        Ok(())
    }
    pub fn snapshot(&self, max: Option<usize>) -> Vec<u8> {
        let count = max.unwrap_or(self.bytes.len()).min(self.bytes.len());
        self.bytes
            .iter()
            .skip(self.bytes.len() - count)
            .copied()
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct Cgroup {
    path: PathBuf,
    anchor_pid: u32,
    /// Shared across clones: the anchor pid must be killed at most once.
    /// After the first kill the pid is reaped (a background thread waits on
    /// it) and may be recycled by the kernel for an unrelated process of
    /// this user, so a second kill(anchor_pid, SIGKILL) -- e.g. from
    /// Cgroup::cleanup at session end, hours after spawn_workload already
    /// released the anchor -- could kill an innocent process.
    anchor_released: std::sync::Arc<AtomicBool>,
    initial_oom_kill: u64,
}
impl Cgroup {
    // A worker's own ambient cgroup (inherited from whatever spawned `a start`,
    // e.g. a tmux pane or SSH session) is never a safe place to nest a
    // resource-limited child: cgroup v2 refuses to enable controllers in
    // cgroup.subtree_control while the parent still has processes attached
    // directly ("no internal process" constraint) -- and the worker, plus
    // everything else in that ambient session, is exactly such a process.
    // Writing to memory.max there fails closed with EACCES rather than
    // applying a limit; forcing it through would risk taking down unrelated
    // sessions sharing that ambient cgroup, which is the one failure mode
    // this project exists to prevent.
    //
    // Instead we ask systemd-run to create a fresh, independently delegated
    // scope directly under the user's own slice (a sibling, not a nested
    // child, of the ambient cgroup) and hold it open with a placeholder
    // process until the real workload can be moved in.
    pub fn create(id: Uuid, limits: &Limits) -> Result<Option<Self>> {
        if !limits.requested() {
            return Ok(None);
        }
        let controllers = Path::new("/sys/fs/cgroup/cgroup.controllers");
        if !controllers.exists() {
            bail!("resource limits require cgroup v2");
        }
        let unit = format!("aplexer-workload-{id}");
        let mut command = Command::new("systemd-run");
        command
            .arg("--user")
            .arg("--scope")
            .arg("--collect")
            .arg(format!("--unit={unit}"))
            .arg("-p")
            .arg("Delegate=yes");
        if let Some(value) = limits.memory_bytes {
            command.arg("-p").arg(format!("MemoryMax={value}"));
            // Without a swap cap, hitting MemoryMax doesn't OOM-kill the
            // workload -- it swaps unboundedly instead, which both defeats
            // the purpose of a memory limit and risks host-wide I/O
            // pressure that *would* leak into unrelated sessions. A
            // memory-limited session gets no swap; a configurable swap
            // allowance is not yet exposed by the CLI.
            command.arg("-p").arg("MemorySwapMax=0");
        }
        if let Some(value) = limits.pids {
            command.arg("-p").arg(format!("TasksMax={value}"));
        }
        if let Some(quota) = limits.cpu_quota_us {
            let period = limits.cpu_period_us.unwrap_or(100_000);
            let percent = ((quota as f64 / period as f64) * 100.0).ceil().max(1.0) as u64;
            command.arg("-p").arg(format!("CPUQuota={percent}%"));
        }
        command
            .arg("--")
            .arg("sleep")
            .arg("infinity")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut anchor = command
            .spawn()
            .context("spawn systemd-run anchor; limits fail closed")?;
        let anchor_pid = anchor.id();
        thread::spawn(move || {
            let _ = anchor.wait();
        });
        let path = match wait_for_scope_cgroup(&unit, Duration::from_secs(5)) {
            Ok(path) => path,
            Err(error) => {
                unsafe {
                    libc::kill(anchor_pid as i32, libc::SIGKILL);
                }
                return Err(error.context("limits fail closed"));
            }
        };
        if limits.memory_bytes.is_some() && !path.join("memory.max").exists() {
            unsafe {
                libc::kill(anchor_pid as i32, libc::SIGKILL);
            }
            bail!("systemd did not delegate the memory controller; limits fail closed");
        }
        if limits.pids.is_some() && !path.join("pids.max").exists() {
            unsafe {
                libc::kill(anchor_pid as i32, libc::SIGKILL);
            }
            bail!("systemd did not delegate the pids controller; limits fail closed");
        }
        let initial_oom_kill = read_counter(&path.join("memory.events"), "oom_kill").unwrap_or(0);
        Ok(Some(Self {
            path,
            anchor_pid,
            anchor_released: std::sync::Arc::new(AtomicBool::new(false)),
            initial_oom_kill,
        }))
    }
    /// Opens `cgroup.procs` for writing so the not-yet-exec'd workload child
    /// can move itself into the cgroup from inside a `pre_exec` closure
    /// (any process may write its own pid into a cgroup it has access to;
    /// this needs no cooperation from the parent after fork).
    ///
    /// We deliberately do not have the parent write the child's pid into
    /// `cgroup.procs` after `Command::spawn()` returns: `spawn()` itself
    /// blocks in the parent until the child either execs or reports a
    /// pre_exec failure, so any post-spawn, pre-exec rendezvous between
    /// parent and child (e.g. a gate the child waits on) deadlocks --
    /// the parent can never reach the code that would release it.
    pub fn open_procs(&self) -> Result<File> {
        OpenOptions::new()
            .write(true)
            .open(self.path.join("cgroup.procs"))
            .with_context(|| format!("open {}/cgroup.procs", self.path.display()))
    }
    /// Kills the placeholder process that was keeping the delegated scope
    /// alive. Call this only after the real workload pid has been added to
    /// the cgroup, so the cgroup never goes empty (and gets garbage
    /// collected by systemd) before the real workload takes residence.
    pub fn release_anchor(&self) {
        if !self.anchor_released.swap(true, Ordering::SeqCst) {
            unsafe {
                libc::kill(self.anchor_pid as i32, libc::SIGKILL);
            }
        }
    }
    pub fn signal_all(&self, signal: i32) -> Result<()> {
        let pids = fs::read_to_string(self.path.join("cgroup.procs"))?;
        for text in pids.lines() {
            if let Ok(pid) = text.parse::<i32>() {
                unsafe {
                    libc::kill(pid, signal);
                }
            }
        }
        Ok(())
    }
    pub fn kill_all(&self) -> Result<()> {
        let kill = self.path.join("cgroup.kill");
        if kill.exists() {
            fs::write(kill, "1")?;
            Ok(())
        } else {
            self.signal_all(libc::SIGKILL)
        }
    }
    pub fn populated(&self) -> bool {
        read_counter(&self.path.join("cgroup.events"), "populated").unwrap_or(0) != 0
    }
    pub fn oom_killed(&self) -> bool {
        read_counter(&self.path.join("memory.events"), "oom_kill").unwrap_or(0)
            > self.initial_oom_kill
    }
    /// Live telemetry for a still-running cgroup. A workload's own OOM kill
    /// only shows up in the session record's `exit` field once the tracked
    /// PTY-owning process itself exits -- a subprocess it launched can be
    /// OOM-killed by the kernel while the shell survives, which is common
    /// and otherwise invisible. `a status` surfaces this live instead of
    /// only at session exit.
    pub fn stats(&self) -> serde_json::Value {
        let read_value = |name: &str| -> Option<u64> {
            fs::read_to_string(self.path.join(name))
                .ok()
                .and_then(|text| text.trim().parse().ok())
        };
        let oom_kill_total = read_counter(&self.path.join("memory.events"), "oom_kill").unwrap_or(0);
        serde_json::json!({
            "memory_current": read_value("memory.current"),
            "memory_peak": read_value("memory.peak"),
            "memory_swap_current": read_value("memory.swap.current"),
            "oom_kill_count": oom_kill_total,
            "oom_kill_count_since_start": oom_kill_total.saturating_sub(self.initial_oom_kill),
            "populated": self.populated(),
        })
    }
    pub fn cleanup(&self) {
        self.release_anchor();
        let _ = fs::remove_dir(&self.path);
    }
}
fn wait_for_scope_cgroup(unit: &str, timeout: Duration) -> Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        let output = Command::new("systemctl")
            .args([
                "--user",
                "show",
                &format!("{unit}.scope"),
                "-p",
                "ControlGroup",
                "--value",
            ])
            .output();
        if let Ok(output) = output {
            if output.status.success() {
                let relative = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !relative.is_empty() && relative != "/" {
                    let path = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
                    if path.join("cgroup.procs").exists() {
                        return Ok(path);
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for systemd scope {unit}.scope to appear");
        }
        thread::sleep(Duration::from_millis(20));
    }
}
fn read_counter(path: &Path, key: &str) -> Result<u64> {
    let text = fs::read_to_string(path)?;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some(key) {
            return Ok(parts.next().unwrap_or("0").parse()?);
        }
    }
    Ok(0)
}

pub fn parse_byte_size(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty byte size");
    }
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let value: u64 = raw[..split].parse()?;
    let suffix = raw[split..].trim().to_ascii_lowercase();
    let multiplier = match suffix.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024_u64.pow(2),
        "g" | "gb" | "gib" => 1024_u64.pow(3),
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        _ => bail!("unknown byte-size suffix {suffix}"),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("byte size overflow"))
}

pub fn parse_env(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for value in values {
        let (key, val) = value
            .split_once('=')
            .ok_or_else(|| anyhow!("environment override must be KEY=VALUE"))?;
        if key.is_empty() || key.as_bytes().contains(&0) || val.as_bytes().contains(&0) {
            bail!("invalid environment override");
        }
        out.insert(key.to_owned(), val.to_owned());
    }
    Ok(out)
}

pub fn os_to_utf8(value: &OsStr, what: &str) -> Result<String> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{what} must be valid UTF-8"))
}

pub fn command_exists(command: &[String]) -> bool {
    command
        .first()
        .map(|p| executable_available(p))
        .unwrap_or(false)
}

pub fn worker_executable() -> Result<PathBuf> {
    if let Some(path) = env::var_os("APLEXER_WORKER") {
        return Ok(PathBuf::from(path));
    }
    let current = env::current_exe()?;
    if let Some(parent) = current.parent() {
        let sibling = parent.join("aplexer");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    Ok(PathBuf::from("aplexer"))
}

pub fn set_cloexec(fd: RawFd, enabled: bool) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let next = if enabled {
        flags | libc::FD_CLOEXEC
    } else {
        flags & !libc::FD_CLOEXEC
    };
    if unsafe { libc::fcntl(fd, libc::F_SETFD, next) } < 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

pub fn open_pty(rows: u16, cols: u16) -> Result<(File, File)> {
    let master = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
    if master < 0 {
        return Err(io::Error::last_os_error()).context("posix_openpt");
    }
    let cleanup_master = || unsafe {
        libc::close(master);
    };
    if unsafe { libc::grantpt(master) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("grantpt");
    }
    if unsafe { libc::unlockpt(master) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("unlockpt");
    }
    let mut name = vec![0i8; 256];
    if unsafe { libc::ptsname_r(master, name.as_mut_ptr(), name.len()) } != 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("ptsname_r");
    }
    let slave = unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    };
    if slave < 0 {
        let e = io::Error::last_os_error();
        cleanup_master();
        return Err(e).context("open PTY slave");
    }
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(master, libc::TIOCSWINSZ, &ws);
    }
    Ok(unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) })
}

pub fn set_winsize(fd: RawFd, rows: u16, cols: u16) -> Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) } < 0 {
        return Err(io::Error::last_os_error()).context("TIOCSWINSZ");
    }
    Ok(())
}

pub fn peer_uid(fd: RawFd) -> Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut _ as *mut _,
            &mut len,
        )
    } != 0
    {
        return Err(io::Error::last_os_error()).context("SO_PEERCRED");
    }
    Ok(cred.uid)
}

pub fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

pub fn c_string(path: &Path) -> Result<CString> {
    CString::new(path.as_os_str().as_bytes()).context("path contains NUL")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_round_trip() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, FrameKind::Data, b"a\0b").unwrap();
        let mut cursor = io::Cursor::new(bytes);
        let frame = read_frame(&mut cursor).unwrap().unwrap();
        assert_eq!(frame.kind, FrameKind::Data);
        assert_eq!(frame.payload, b"a\0b");
    }
    #[test]
    fn bounded_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = History::open(dir.path().join("h"), 4).unwrap();
        h.append(b"abcdef").unwrap();
        assert_eq!(h.snapshot(None), b"cdef");
    }
    #[test]
    fn sizes() {
        assert_eq!(parse_byte_size("2MiB").unwrap(), 2 * 1024 * 1024);
    }
}
