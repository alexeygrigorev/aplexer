//! PyO3 bindings: `import aplexer` calls into this module, not the `a` binary.

use anyhow::Context;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::api::{self, StartRequest};
use crate::Paths;

fn py_err(err: anyhow::Error) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

fn paths(
    state_dir: Option<&str>,
    runtime_dir: Option<&str>,
    config: Option<&str>,
) -> anyhow::Result<Paths> {
    let uid = unsafe { libc::geteuid() };
    let runtime_root = runtime_dir
        .map(PathBuf::from)
        .or_else(|| env::var_os("APLEXER_RUNTIME_DIR").map(PathBuf::from))
        .or_else(|| env::var_os("XDG_RUNTIME_DIR").map(|path| PathBuf::from(path).join("aplexer")))
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/aplexer-{uid}")));
    let state_root = state_dir
        .map(PathBuf::from)
        .or_else(|| env::var_os("APLEXER_STATE_DIR").map(PathBuf::from))
        .or_else(|| env::var_os("XDG_STATE_HOME").map(|path| PathBuf::from(path).join("aplexer")))
        .or_else(|| {
            env::var_os("HOME").map(|path| PathBuf::from(path).join(".local/state/aplexer"))
        })
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let config_file = config
        .map(PathBuf::from)
        .or_else(|| env::var_os("APLEXER_CONFIG").map(PathBuf::from))
        .or_else(|| {
            env::var_os("XDG_CONFIG_HOME")
                .map(|path| PathBuf::from(path).join("aplexer/config.toml"))
        })
        .or_else(|| {
            env::var_os("HOME").map(|path| PathBuf::from(path).join(".config/aplexer/config.toml"))
        })
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    let paths = Paths {
        runtime_root,
        state_root,
        config_file,
    };
    paths.ensure()?;
    Ok(paths)
}

#[pyfunction]
#[pyo3(signature = (state_dir=None, runtime_dir=None, config=None))]
fn engines(
    state_dir: Option<&str>,
    runtime_dir: Option<&str>,
    config: Option<&str>,
) -> PyResult<String> {
    let value = api::engines_json(&paths(state_dir, runtime_dir, config).map_err(py_err)?)
        .map_err(py_err)?;
    Ok(value.to_string())
}

#[pyfunction]
#[pyo3(signature = (state_dir=None, runtime_dir=None, config=None))]
fn profiles(
    state_dir: Option<&str>,
    runtime_dir: Option<&str>,
    config: Option<&str>,
) -> PyResult<String> {
    let value = api::profiles_json(&paths(state_dir, runtime_dir, config).map_err(py_err)?)
        .map_err(py_err)?;
    Ok(value.to_string())
}

#[pyfunction]
#[pyo3(signature = (
    engine=None,
    profile=None,
    cwd=None,
    no_skip_permissions=false,
    state_dir=None,
    runtime_dir=None,
    config=None,
))]
fn launch_spec(
    engine: Option<&str>,
    profile: Option<&str>,
    cwd: Option<&str>,
    no_skip_permissions: bool,
    state_dir: Option<&str>,
    runtime_dir: Option<&str>,
    config: Option<&str>,
) -> PyResult<String> {
    let cwd = cwd.map(Path::new);
    let value = api::launch_spec_json(
        &paths(state_dir, runtime_dir, config).map_err(py_err)?,
        engine,
        profile,
        cwd,
        no_skip_permissions,
    )
    .map_err(py_err)?;
    Ok(value.to_string())
}

#[pyfunction]
#[pyo3(signature = (running=false, state_dir=None, runtime_dir=None, config=None))]
fn snapshot(
    running: bool,
    state_dir: Option<&str>,
    runtime_dir: Option<&str>,
    config: Option<&str>,
) -> PyResult<String> {
    let value = api::snapshot_json(
        &paths(state_dir, runtime_dir, config).map_err(py_err)?,
        running,
    )
    .map_err(py_err)?;
    Ok(value.to_string())
}

#[pyfunction]
#[allow(clippy::too_many_arguments)] // Mirrors the keyword-oriented Python API.
#[pyo3(signature = (
    workspace=".",
    tag="default",
    engine=None,
    profile=None,
    cwd=None,
    env=None,
    command=None,
    memory=None,
    pids=None,
    no_skip_permissions=false,
    python=None,
    startup_timeout_ms=10_000,
    state_dir=None,
    runtime_dir=None,
    config=None,
))]
fn start(
    workspace: &str,
    tag: &str,
    engine: Option<&str>,
    profile: Option<&str>,
    cwd: Option<&str>,
    env: Option<BTreeMap<String, String>>,
    command: Option<Vec<String>>,
    memory: Option<&str>,
    pids: Option<u64>,
    no_skip_permissions: bool,
    python: Option<&str>,
    startup_timeout_ms: u64,
    state_dir: Option<&str>,
    runtime_dir: Option<&str>,
    config: Option<&str>,
) -> PyResult<String> {
    let req = StartRequest {
        workspace: PathBuf::from(workspace),
        tag: tag.to_string(),
        engine: engine.map(str::to_string),
        profile: profile.map(str::to_string),
        cwd: cwd.map(PathBuf::from),
        env: env.unwrap_or_default(),
        command: command.unwrap_or_default(),
        memory: memory.map(str::to_string),
        pids,
        cpu_quota_us: None,
        cpu_period_us: 100_000,
        history_bytes: None,
        no_skip_permissions,
        startup_timeout_ms,
        worker_rows: None,
        worker_cols: None,
        python: python.map(PathBuf::from),
    };
    let record = api::start_session(
        &paths(state_dir, runtime_dir, config).map_err(py_err)?,
        &req,
    )
    .map_err(py_err)?;
    serde_json::to_string(&record)
        .context("serialize session")
        .map_err(py_err)
}

#[pyfunction]
fn run_worker(id: &str) -> PyResult<()> {
    let id: Uuid = id
        .parse()
        .map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
    crate::worker::run_worker(id, None).map_err(py_err)
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(engines, m)?)?;
    m.add_function(wrap_pyfunction!(profiles, m)?)?;
    m.add_function(wrap_pyfunction!(launch_spec, m)?)?;
    m.add_function(wrap_pyfunction!(snapshot, m)?)?;
    m.add_function(wrap_pyfunction!(start, m)?)?;
    m.add_function(wrap_pyfunction!(run_worker, m)?)?;
    Ok(())
}
