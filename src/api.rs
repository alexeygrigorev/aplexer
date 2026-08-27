//! Library API used by the Python bindings and the `a` CLI.
//!
//! These functions are the source of truth. The CLI prints them; the Python
//! package calls them in-process (no subprocess of `a`).

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::{
    atomic_write_json, canonical_workspace, command_exists, ensure_private_dir, list_records,
    parse_byte_size, process_alive, read_record, validate_tag, worker_executable, Config,
    FileLock, Limits, Paths, Phase, SCHEMA_VERSION, SessionRecord,
};

pub fn engines_json(paths: &Paths) -> Result<Value> {
    let config = Config::load(paths)?;
    let values = config
        .engines
        .iter()
        .map(|(name, e)| {
            let env_unset = e.resolved_env_unset();
            json!({
                "name": name,
                "command": e.command,
                "available": command_exists(&e.command),
                "env_unset_count": env_unset.len(),
                "env_unset": env_unset,
            })
        })
        .collect::<Vec<_>>();
    Ok(Value::Array(values))
}

pub fn profiles_json(paths: &Paths) -> Result<Value> {
    let config = Config::load(paths)?;
    Ok(serde_json::to_value(&config.profiles)?)
}

pub fn launch_spec_json(
    paths: &Paths,
    engine: Option<&str>,
    profile: Option<&str>,
    cwd: Option<&Path>,
    no_skip_permissions: bool,
) -> Result<Value> {
    let config = Config::load(paths)?;
    let workspace = canonical_workspace(Path::new("."))?;
    let launch = config.resolve(
        Vec::new(),
        engine,
        profile,
        &workspace,
        cwd,
        &BTreeMap::new(),
        &Limits::default(),
        None,
    )?;
    let mut argv = launch.command.clone();
    if !no_skip_permissions {
        argv.extend(launch.skip_permissions_argv.clone());
    }
    let cwd = canonical_workspace(&launch.cwd).unwrap_or(launch.cwd);
    Ok(json!({
        "engine": launch.engine,
        "profile": launch.profile,
        "argv": argv,
        "env_set": launch.env,
        "env_unset": launch.env_unset,
        "cwd": cwd,
    }))
}

pub fn snapshot_json(paths: &Paths, running: bool) -> Result<Value> {
    let mut records = list_records(paths)?;
    if running {
        records.retain(|r| {
            matches!(r.phase, Phase::Starting | Phase::Running | Phase::Exiting)
                && r.worker_pid.map(process_alive).unwrap_or(false)
        });
    }
    let enriched: Vec<Value> = records
        .iter()
        .map(|r| {
            let mut value = serde_json::to_value(r).unwrap_or(Value::Null);
            value["worker_alive"] = json!(r.worker_pid.map(process_alive).unwrap_or(false));
            value
        })
        .collect();
    Ok(Value::Array(enriched))
}

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub workspace: PathBuf,
    pub tag: String,
    pub engine: Option<String>,
    pub profile: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub command: Vec<String>,
    pub memory: Option<String>,
    pub pids: Option<u64>,
    pub cpu_quota_us: Option<u64>,
    pub cpu_period_us: u64,
    pub history_bytes: Option<usize>,
    pub no_skip_permissions: bool,
    pub startup_timeout_ms: u64,
    pub worker_rows: Option<u16>,
    pub worker_cols: Option<u16>,
    /// When set, spawn the worker as `python -m aplexer worker --id …`
    /// (Python bindings). Otherwise spawn the `aplexer` worker binary.
    pub python: Option<PathBuf>,
}

pub fn start_session(paths: &Paths, req: &StartRequest) -> Result<SessionRecord> {
    validate_tag(&req.tag)?;
    let workspace = canonical_workspace(&req.workspace)?;
    let limits = Limits {
        memory_bytes: req.memory.as_deref().map(parse_byte_size).transpose()?,
        pids: req.pids,
        cpu_quota_us: req.cpu_quota_us,
        cpu_period_us: req.cpu_quota_us.map(|_| req.cpu_period_us),
    };
    let config = Config::load(paths)?;
    let mut launch = config.resolve(
        req.command.clone(),
        req.engine.as_deref(),
        req.profile.as_deref(),
        &workspace,
        req.cwd.as_deref(),
        &req.env,
        &limits,
        req.history_bytes,
    )?;
    if req.command.is_empty() && !req.no_skip_permissions {
        launch
            .command
            .extend(launch.skip_permissions_argv.iter().cloned());
    }
    if !command_exists(&launch.command) {
        bail!(
            "command is not executable or was not found in PATH: {}",
            launch
                .command
                .first()
                .map(String::as_str)
                .unwrap_or("<empty>")
        );
    }
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    if let Some(existing) = list_records(paths)?
        .into_iter()
        .find(|r| r.workspace == workspace && r.tag == req.tag)
    {
        let worker_alive = existing.worker_pid.map(process_alive).unwrap_or(false);
        let finished = matches!(existing.phase, Phase::Exited | Phase::Failed) && !worker_alive;
        if !finished {
            bail!(
                "workspace+tag already belongs to session {}; rename it or choose a different tag",
                existing.id
            );
        }
        fs::remove_dir_all(paths.state_session(existing.id))
            .with_context(|| format!("remove superseded session {}", existing.id))?;
        let _ = fs::remove_dir_all(paths.runtime_session(existing.id));
    }
    let id = Uuid::new_v4();
    ensure_private_dir(&paths.state_session(id))?;
    ensure_private_dir(&paths.runtime_session(id))?;
    let now = crate::now_ms();
    let record = SessionRecord {
        schema_version: SCHEMA_VERSION,
        id,
        workspace: workspace.clone(),
        tag: req.tag.clone(),
        engine: launch.engine,
        profile: launch.profile,
        command: launch.command,
        cwd: canonical_workspace(&launch.cwd).unwrap_or(launch.cwd),
        env: launch.env,
        env_unset: launch.env_unset,
        limits: launch.limits,
        history_bytes: launch.history_bytes,
        created_at_ms: now,
        updated_at_ms: now,
        last_activity_ms: None,
        phase: Phase::Starting,
        worker_pid: None,
        workload_pid: None,
        socket_path: paths.socket(id),
        history_path: paths.history(id),
        exit: None,
        error: None,
    };
    atomic_write_json(&paths.record(id), &record)?;
    let worker_log = File::create(paths.state_session(id).join("worker.log"))
        .context("create worker log")?;
    let mut command = worker_command(id, req.python.as_deref())?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(worker_log));
    if let (Some(rows), Some(cols)) = (req.worker_rows, req.worker_cols) {
        command
            .arg("--rows")
            .arg(rows.to_string())
            .arg("--cols")
            .arg(cols.to_string());
    }
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("spawn worker")?;
    let deadline = Instant::now() + Duration::from_millis(req.startup_timeout_ms);
    loop {
        if let Ok(current) = read_record(&paths.record(id)) {
            match current.phase {
                Phase::Running | Phase::Exiting | Phase::Exited if current.socket_path.exists() => {
                    return Ok(current);
                }
                Phase::Failed => bail!(
                    "worker startup failed: {}",
                    current.error.unwrap_or_else(|| "unknown error".into())
                ),
                _ => {}
            }
        }
        if let Some(status) = child.try_wait()? {
            bail!("worker exited during startup: {status}");
        }
        if Instant::now() >= deadline {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGTERM);
            }
            bail!(
                "worker did not become ready within {} ms",
                req.startup_timeout_ms
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn worker_command(id: Uuid, python: Option<&Path>) -> Result<Command> {
    if let Some(python) = python {
        let mut command = Command::new(python);
        command.args([
            "-m",
            "aplexer",
            "worker",
            "--id",
            &id.to_string(),
        ]);
        return Ok(command);
    }
    let mut command = Command::new(worker_executable()?);
    command.arg("worker").arg("--id").arg(id.to_string());
    Ok(command)
}
