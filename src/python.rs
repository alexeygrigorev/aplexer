//! PyO3 bindings: `import aplexer` calls into this module, not the `a` binary.

use anyhow::Context;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::api::{self, StartRequest};
use crate::Paths;

fn py_err(err: anyhow::Error) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

fn paths() -> anyhow::Result<Paths> {
    Paths::discover()
}

#[pyfunction]
fn engines() -> PyResult<String> {
    let value = api::engines_json(&paths().map_err(py_err)?).map_err(py_err)?;
    Ok(value.to_string())
}

#[pyfunction]
fn profiles() -> PyResult<String> {
    let value = api::profiles_json(&paths().map_err(py_err)?).map_err(py_err)?;
    Ok(value.to_string())
}

#[pyfunction]
#[pyo3(signature = (engine=None, profile=None, cwd=None, no_skip_permissions=false))]
fn launch_spec(
    engine: Option<&str>,
    profile: Option<&str>,
    cwd: Option<&str>,
    no_skip_permissions: bool,
) -> PyResult<String> {
    let cwd = cwd.map(Path::new);
    let value = api::launch_spec_json(
        &paths().map_err(py_err)?,
        engine,
        profile,
        cwd,
        no_skip_permissions,
    )
    .map_err(py_err)?;
    Ok(value.to_string())
}

#[pyfunction]
#[pyo3(signature = (running=false))]
fn snapshot(running: bool) -> PyResult<String> {
    let value = api::snapshot_json(&paths().map_err(py_err)?, running).map_err(py_err)?;
    Ok(value.to_string())
}

#[pyfunction]
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
        isolated: false,
    };
    let record = api::start_session(&paths().map_err(py_err)?, &req).map_err(py_err)?;
    serde_json::to_string(&record)
        .context("serialize session")
        .map_err(py_err)
}

#[pyfunction]
fn run_worker(id: &str) -> PyResult<()> {
    let id: Uuid = id.parse().map_err(|e| PyRuntimeError::new_err(format!("{e}")))?;
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
