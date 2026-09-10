//! Library API used by the Python bindings and the `a` CLI.
//!
//! These functions are the source of truth. The CLI prints them; the Python
//! package calls them in-process (no subprocess of `a`).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

mod start;
mod startup_cleanup;

pub use start::*;
use startup_cleanup::*;

use crate::agent_kind::{detect_agent, AgentKind, DEFAULT_PROC_ROOT};
use crate::{
    atomic_write_json, canonical_workspace, cleanup_recorded_cgroup_until, command_exists,
    ensure_private_dir, ensure_sigchld_compatible_for_child_management, frame_json, io_kind,
    kill_grace_duration, list_records, parse_byte_size, process_start_time_ticks,
    public_session_record, read_frame, read_persisted_history_tail, read_record,
    read_session_record, reap_verdict, resolve_record, session_metadata_env, validate_tag,
    worker_executable, write_frame, write_json, Config, ContainmentReap, FileLock, FrameKind,
    Limits, Operation, Paths, Phase, Request, Response, SessionRecord, MAX_FRAME_BYTES,
    PROTOCOL_VERSION, SCHEMA_VERSION,
};

struct LaunchEnvironmentGuard(PathBuf);

impl Drop for LaunchEnvironmentGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// `start_session` makes the worker a session leader in `pre_exec`. TERM asks
/// the worker's cancellation handler to unwind startup and clean its separate
/// workload containment domain; signalling the leader's group is only the
/// last-resort way to stop the worker itself after that grace period.
fn signal_worker_group(pid: u32, signal: i32) -> io::Result<()> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "worker pid exceeds pid_t"))?;
    if unsafe { libc::kill(-pid, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

pub fn engines_json(paths: &Paths) -> Result<Value> {
    let config = Config::load(paths)?;
    let values = config
        .engines
        .iter()
        .map(|(name, e)| {
            let env_unset = e.resolved_env_unset(name);
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
    let mut profiles = Config::load(paths)?.profiles;
    for profile in profiles.values_mut() {
        profile.env = session_metadata_env(&profile.env);
    }
    Ok(serde_json::to_value(profiles)?)
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

/// Which agent is running inside `record`'s workload process tree right now
/// (see `crate::agent_kind`), or `None` when nothing recognisable is there.
///
/// Detection is query-time only and never persisted: the record on disk has
/// no `agent` field, so it can never be stale. Two guards keep the answer
/// honest rather than merely present:
///
/// * A record whose phase is already terminal (`exited`/`failed`) is not
///   probed at all. Its `workload_pid` names a process that is gone, and a
///   recycled numeric pid could otherwise make an unrelated `claude` on the
///   box look like this dead session's agent.
/// * A record with no recorded `workload_pid` has no handle to walk.
pub fn record_agent(record: &SessionRecord) -> Option<AgentKind> {
    if !record.worker_phase_active() {
        return None;
    }
    detect_agent(Path::new(DEFAULT_PROC_ROOT), record.workload_pid?)
}

pub fn snapshot_json(paths: &Paths, running: bool) -> Result<Value> {
    let records = list_records(paths)?;
    let mut enriched = Vec::with_capacity(records.len());
    // One clock for the whole snapshot, so two rows created in the same
    // instant cannot land on opposite sides of the startup window.
    let now = crate::now_ms();
    for record in &records {
        // One liveness probe per row: it reads the identity sidecar and
        // `/proc`, and both the `running` filter and the row need it.
        let worker_alive = record.worker_alive();
        if running && !(record.worker_phase_active() && worker_alive) {
            continue;
        }
        let mut value = serde_json::to_value(public_session_record(record))?;
        value["worker_alive"] = json!(worker_alive);
        // The derived liveness fact, identical to `a status`'s `state:`
        // line (see `observed_state`). Machine consumers were previously
        // handed only the persisted `phase`, which a killed worker leaves
        // at "running" forever, so a zombie record was indistinguishable
        // from a live session on the wire.
        value["state"] = json!(crate::observed_state(
            &record.phase,
            worker_alive,
            record.created_at_ms,
            now
        ));
        // Which agent is live inside the session right now, detected from the
        // workload's process tree at query time (`record_agent`). Always
        // present, `null` when no agent is detectable -- every pocketshell
        // session is `engine: "shell"` with the agent started by hand inside
        // it, so `engine` cannot answer this and a consumer needs one key it
        // can read unconditionally.
        value["agent"] = json!(record_agent(record));
        // Placement facts derived from the recorded cgroup paths
        // (`placement::classify_cgroup_path`), the same classification
        // `a doctor`'s launch_placement check uses -- so a consumer of this
        // row and a consumer of doctor can never disagree about whether a
        // session dies with the per-user manager (issue #1).
        value["worker_placement"] =
            crate::placement::placement_summary(record.worker_cgroup.as_deref());
        value["workload_placement"] =
            crate::placement::placement_summary(record.workload_cgroup.as_deref());
        enriched.push(value);
    }
    Ok(Value::Array(enriched))
}

#[cfg(not(test))]
const CONTROL_RPC_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(test)]
const CONTROL_RPC_TIMEOUT: Duration = Duration::from_millis(100);

fn selected_record(paths: &Paths, selector: &str) -> Result<SessionRecord> {
    resolve_record(paths, Some(selector), None, None)
}

fn connect_control(record: &SessionRecord) -> Result<UnixStream> {
    let deadline = Instant::now() + CONTROL_RPC_TIMEOUT;
    let stream = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("connect {} timed out", record.socket_path.display());
        }
        match connect_startup_control(&record.socket_path, remaining) {
            Ok(stream) => break stream,
            Err(error)
                if error.raw_os_error() == Some(libc::EAGAIN) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("connect {}", record.socket_path.display()))
            }
        }
    };
    stream
        .set_read_timeout(Some(CONTROL_RPC_TIMEOUT))
        .context("set worker response deadline")?;
    stream
        .set_write_timeout(Some(CONTROL_RPC_TIMEOUT))
        .context("set worker request deadline")?;
    Ok(stream)
}

fn rpc_simple(record: &SessionRecord, operation: Operation, data: Option<&[u8]>) -> Result<Value> {
    rpc_simple_within(record, operation, data, CONTROL_RPC_TIMEOUT)
}

/// `rpc_simple` for a request whose response is legitimately slower than
/// the ordinary control deadline: `response_timeout` replaces the read
/// deadline once the request is on the wire.
fn rpc_simple_within(
    record: &SessionRecord,
    operation: Operation,
    data: Option<&[u8]>,
    response_timeout: Duration,
) -> Result<Value> {
    let mut stream = connect_control(record)?;
    stream
        .set_read_timeout(Some(response_timeout))
        .context("set worker response deadline")?;
    let request = Request::new(record.id, operation);
    let request_id = request.request_id.clone();
    write_json(&mut stream, &request)?;
    if let Some(data) = data {
        write_frame(&mut stream, FrameKind::Data, data)?;
    }
    let frame = read_frame(&mut stream)?.ok_or_else(|| anyhow!("worker closed connection"))?;
    let response: Response = frame_json(frame).context("parse worker response")?;
    if response.version != PROTOCOL_VERSION {
        bail!("worker response used unsupported protocol version");
    }
    if response.request_id != request_id {
        bail!("worker response request id mismatch");
    }
    response.into_result()
}

/// Return the live session record when reachable, or the persisted record plus
/// explicit reachability evidence when the worker cannot answer.
pub fn status_json(paths: &Paths, selector: &str) -> Result<Value> {
    let persisted = selected_record(paths, selector)?;
    let (mut value, current, worker_reachable, rpc_error) =
        match rpc_simple(&persisted, Operation::Status, None) {
            Ok(value) => {
                let current: SessionRecord = serde_json::from_value(value.clone())
                    .context("worker returned an invalid status record")?;
                (value, current, true, None)
            }
            Err(error) => {
                let value = serde_json::to_value(public_session_record(&persisted))?;
                (value, persisted.clone(), false, Some(format!("{error:#}")))
            }
        };
    value["worker_alive"] = json!(current.worker_alive());
    value["worker_reachable"] = json!(worker_reachable);
    // Same query-time agent detection every `a list --json`/`a snapshot` row
    // carries, so the two commands cannot disagree about which agent is in a
    // session.
    value["agent"] = json!(record_agent(&current));
    // Same derived placement facts every `a list --json`/`a snapshot` row
    // carries, from the same helper (issue #1).
    value["worker_placement"] =
        crate::placement::placement_summary(current.worker_cgroup.as_deref());
    value["workload_placement"] =
        crate::placement::placement_summary(current.workload_cgroup.as_deref());
    if let Some(error) = rpc_error {
        value["rpc_error"] = json!(error);
    }
    Ok(value)
}

/// Send bytes without transcoding, splitting only at the framing limit.
pub fn send_bytes(paths: &Paths, selector: &str, data: &[u8]) -> Result<usize> {
    let record = selected_record(paths, selector)?;
    for chunk in data.chunks(MAX_FRAME_BYTES) {
        let result = rpc_simple(&record, Operation::Send { bytes: chunk.len() }, Some(chunk))?;
        let reported = result
            .get("bytes")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| anyhow!("worker send response omitted byte count"))?;
        if reported != chunk.len() {
            bail!(
                "worker send byte count mismatch: sent {}, acknowledged {reported}",
                chunk.len()
            );
        }
    }
    Ok(data.len())
}

fn rpc_capture(record: &SessionRecord, max_bytes: Option<usize>) -> Result<Vec<u8>> {
    let mut stream = connect_control(record)?;
    let request = Request::new(record.id, Operation::Capture { max_bytes });
    let request_id = request.request_id.clone();
    write_json(&mut stream, &request)?;
    let response: Response = frame_json(
        read_frame(&mut stream)?.ok_or_else(|| anyhow!("worker closed before capture response"))?,
    )
    .context("parse worker capture response")?;
    if response.version != PROTOCOL_VERSION {
        bail!("worker response used unsupported protocol version");
    }
    if response.request_id != request_id {
        bail!("worker response request id mismatch");
    }
    let result = response.into_result()?;
    let reported = result
        .get("bytes")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| anyhow!("worker capture response omitted byte count"))?;
    let frame =
        read_frame(&mut stream)?.ok_or_else(|| anyhow!("worker closed before capture data"))?;
    if frame.kind != FrameKind::Data {
        bail!("worker returned a non-data capture frame");
    }
    if frame.payload.len() != reported {
        bail!(
            "worker capture byte count mismatch: reported {reported}, returned {}",
            frame.payload.len()
        );
    }
    Ok(frame.payload)
}

/// Capture live history bytes, falling back to the bounded persisted tail only
/// when the worker is terminal or known gone.
pub fn capture_bytes(paths: &Paths, selector: &str, max_bytes: Option<usize>) -> Result<Vec<u8>> {
    let record = selected_record(paths, selector)?;
    match rpc_capture(&record, max_bytes) {
        Ok(data) => Ok(data),
        Err(_) if record.worker_finished() || !record.worker_alive() => {
            read_persisted_history_tail(&record.history_path, max_bytes)
                .context("worker unavailable and persisted history cannot be read")
        }
        Err(error) => Err(error).context(
            "capture RPC failed while the worker process is still alive; refusing to return potentially stale persisted history",
        ),
    }
}

/// How long a client should wait for the worker's answer to a `Kill` RPC.
///
/// The worker holds the response until the kill has run to completion:
/// the graceful signal, the whole grace window while the workload ignores
/// it, and then the bounded SIGKILL sweep (`DESCENDANT_KILL_TIMEOUT`). With
/// the ordinary 3 s control deadline, `kill --grace-ms 5000` against a
/// TERM-trapping workload timed out client-side and reported an error
/// while the worker went on to escalate and remove the record anyway.
/// The ordinary deadline is kept on top as the margin for signal delivery
/// and the record write.
pub fn kill_response_timeout(grace: Duration) -> Duration {
    grace
        .saturating_add(crate::worker::DESCENDANT_KILL_TIMEOUT)
        .saturating_add(CONTROL_RPC_TIMEOUT)
}

/// Ask the live worker to stop its complete workload containment domain.
pub fn kill_session(paths: &Paths, selector: &str, signal: i32, grace_ms: u64) -> Result<()> {
    if !(1..=64).contains(&signal) {
        bail!("signal out of range");
    }
    let grace = kill_grace_duration(grace_ms)?;
    let record = selected_record(paths, selector)?;
    let result = rpc_simple_within(
        &record,
        Operation::Kill { signal, grace_ms },
        None,
        kill_response_timeout(grace),
    )?;
    if result.get("signalled").and_then(Value::as_bool) != Some(true) {
        bail!("worker kill response omitted confirmation");
    }
    Ok(())
}

/// Result of fencing the spawn-to-worker-lock gap for one record.
pub(crate) enum PrePidFence {
    /// Nothing can come up under this record while the guard (if any) lives.
    /// `None` means no fence was needed: the record is past the gap, so its
    /// `worker_pid` is the authority and `worker_alive()` already answered.
    Fenced(Option<FileLock>),
    /// A worker exists for this record even though it has not registered a
    /// pid yet, so `worker_alive()` reads false for a session that is very
    /// much coming up. Carries the lock path for the caller's diagnostic.
    WorkerHoldsLock(PathBuf),
}

/// Fence a record whose worker may have been spawned but has not reached its
/// first required lock yet.
///
/// `start_session` writes a `Starting` record with `worker_pid: None` before
/// it spawns anything, so for a few tens of milliseconds a perfectly healthy
/// session is on disk as `phase: starting, worker_pid: null` -- and
/// `worker_alive()` is false for a `None` pid. Any predicate that reads "no
/// live worker" as "free to destroy" will happily take that session's state
/// out from under a live spawn. The worker's own first action is to acquire
/// `paths.worker_lock(id)` exclusively, so holding that lock is both the
/// detector (it is already held => a worker exists) and the fence (we hold
/// it => a worker that has not got there yet will fail its acquisition and
/// cannot proceed after we have destroyed the record).
///
/// Callers must keep the returned guard alive across every removal, exactly
/// as `a forget` does. `rename`'s claim check (issue #13) uses the same
/// fence for the same reason, holding it across its record update: its
/// verdict must not read a coming-up session as free either, and while
/// rename destroys nothing, holding the lock keeps a worker that has not
/// reached its acquisition yet from coming up on top of the pair the
/// rename just handed out.
pub(crate) fn fence_pre_pid_worker(paths: &Paths, record: &SessionRecord) -> Result<PrePidFence> {
    if !record.worker_phase_active() || record.worker_pid.is_some() {
        return Ok(PrePidFence::Fenced(None));
    }
    let lock_path = paths.worker_lock(record.id);
    match FileLock::exclusive(&lock_path, true) {
        Ok(lock) => Ok(PrePidFence::Fenced(Some(lock))),
        Err(error) if io_kind(&error) == Some(io::ErrorKind::WouldBlock) => {
            Ok(PrePidFence::WorkerHoldsLock(lock_path))
        }
        Err(error) => Err(error),
    }
}

/// Forget a session record without signalling any process.
///
/// The one implementation of `a forget`: the force gate, the live-worker
/// refusal, the pre-PID worker fence, both directory removals, and the
/// "workload processes may survive" warning live only here. `a forget`'s
/// `cmd_forget` resolves the CLI's target spellings and then calls this, so
/// the Python binding cannot diverge from the CLI on the operation with the
/// least recoverable outcome (issue #11).
pub fn forget_session(paths: &Paths, selector: &str, force: bool) -> Result<Value> {
    if !force {
        // Names both spellings because both callers land here: `--force` on
        // the CLI, `force=True` from the Python binding.
        bail!("forget requires --force (force=True from the Python binding)");
    }
    let selected = selected_record(paths, selector)?;
    let _registry = FileLock::exclusive(&paths.registry_lock(), false)?;
    // Resolve happened before taking the registry lock. Re-read under the
    // lock so a concurrent rename or lifecycle update cannot make a stale
    // liveness decision destructive.
    let current = read_record(&paths.record(selected.id))
        .with_context(|| format!("re-read session {} before forgetting", selected.id))?;
    if current.worker_alive() {
        bail!(
            "session {} still has a live worker; refusing to forget it",
            current.id
        );
    }
    let _startup_absence_lock = match fence_pre_pid_worker(paths, &current).with_context(|| {
        format!(
            "cannot fence session {}'s pre-PID worker; refusing to forget it",
            current.id
        )
    })? {
        PrePidFence::Fenced(lock) => lock,
        PrePidFence::WorkerHoldsLock(lock_path) => bail!(
            "session {} still has a worker holding {}; refusing to forget it",
            current.id,
            lock_path.display()
        ),
    };

    let containment_proven_empty = current.containment_proven_empty();
    // Durable state first, runtime dir second -- the order `a prune` uses
    // and the order the worker's own startup relies on: it acquires the
    // worker lock (in the runtime dir) and only then reads the record, so
    // a worker that recreates the runtime dir after this removal finds the
    // record already gone and refuses to come up.
    fs::remove_dir_all(paths.state_session(current.id))
        .with_context(|| format!("remove forgotten session {} durable state", current.id))?;
    match fs::remove_dir_all(paths.runtime_session(current.id)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove forgotten session runtime state"),
    }

    // The record is gone, so this warning is the only remaining trace that
    // uncontained workload processes may still be running. It goes to stderr
    // rather than into the JSON alone so a CLI user cannot miss it; the
    // `workload_may_survive` key carries the same fact for programmatic
    // callers.
    let workload_may_survive = !containment_proven_empty;
    if workload_may_survive {
        eprintln!(
            "a: forgot session {} without signalling any process; containment was not proven empty, so workload processes may survive",
            current.id
        );
    } else {
        eprintln!(
            "a: forgot session {} without signalling any process (containment was proven empty)",
            current.id
        );
    }
    Ok(json!({
        "id": current.id,
        "forgotten": true,
        "signalled": false,
        "containment_proven_empty": containment_proven_empty,
        "workload_may_survive": workload_may_survive,
    }))
}

fn worker_command(id: Uuid, python: Option<&Path>) -> Result<Command> {
    if let Some(python) = python {
        let mut command = Command::new(python);
        command.args(["-m", "aplexer", "worker", "--id", &id.to_string()]);
        return Ok(command);
    }
    let mut command = Command::new(worker_executable()?);
    command.arg("worker").arg("--id").arg(id.to_string());
    Ok(command)
}
