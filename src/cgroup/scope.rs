//! A live, delegated workload scope: creation through `systemd-run` with an
//! anchor process, membership and telemetry probes, and anchor release.

use anyhow::{anyhow, bail, Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::{
    check_cgroup_cleanup_deadline, current_cgroup_identity, kill_cgroup_path_until,
    live_cgroup_populated_with, read_counter, signal_cgroup_path_until,
    system_scope_escape_decision, systemd_run_scope, trusted_system_helper,
    verify_recorded_cgroup_identity, wait_for_scope_cgroup, CGROUP_V2_ROOT,
};
use crate::{ensure_sigchld_compatible_for_child_management, CgroupIdentity, Limits};

#[derive(Debug, Clone)]
pub struct Cgroup {
    pub(crate) path: PathBuf,
    pub(crate) identity: CgroupIdentity,
    /// Keep exclusive ownership of the unreaped child until release. An
    /// unreaped child reserves its pid, so Child::kill cannot be redirected
    /// to a recycled process; clones serialize the single kill+wait through
    /// this shared slot.
    pub(crate) anchor: Arc<Mutex<Option<std::process::Child>>>,
    pub(crate) initial_oom_kill: u64,
}

pub(crate) fn release_anchor_child(anchor: &mut std::process::Child) -> Result<()> {
    let pid = anchor.id();
    match anchor.kill() {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => {}
        Err(error) => return Err(error).context("kill systemd-run anchor"),
    }
    let waited = anchor.wait().context("reap systemd-run anchor");
    // Released only after the wait has returned, so the worker's descendant
    // reaper can never consume this status first.
    crate::worker::disown_child_pid(pid);
    waited?;
    Ok(())
}

pub(crate) fn release_anchor_slot<T>(
    slot: &mut Option<T>,
    release: impl FnOnce(&mut T) -> Result<()>,
) -> Result<()> {
    if let Some(anchor) = slot.as_mut() {
        release(anchor)?;
        *slot = None;
    }
    Ok(())
}

pub(crate) fn cleanup_anchor_after_failure(
    anchor: &mut std::process::Child,
    error: anyhow::Error,
) -> anyhow::Error {
    match release_anchor_child(anchor) {
        Ok(()) => error,
        Err(cleanup_error) => {
            anyhow!("{error:#}; systemd-run anchor cleanup failed: {cleanup_error:#}")
        }
    }
}

/// `systemd-run` for the delegated workload scope: `Delegate=yes` plus one
/// unit property per requested limit, with a placeholder `sleep infinity`
/// holding the scope open until the real workload moves in.
fn workload_scope_command(
    systemd_run: PathBuf,
    bus_flag: &str,
    unit: &str,
    limits: &Limits,
    sleep: &Path,
) -> Command {
    let mut command = systemd_run_scope(systemd_run, bus_flag, unit, false);
    command.arg("-p").arg("Delegate=yes");
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
        .arg(sleep)
        .arg("infinity")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// A scope without the controller a requested limit needs cannot enforce
/// it; limits fail closed rather than silently not applying.
fn verify_delegated_controllers(path: &Path, limits: &Limits) -> Result<()> {
    for (requested, file, controller) in [
        (limits.memory_bytes.is_some(), "memory.max", "memory"),
        (limits.pids.is_some(), "pids.max", "pids"),
    ] {
        if requested && !path.join(file).exists() {
            bail!("systemd did not delegate the {controller} controller; limits fail closed");
        }
    }
    Ok(())
}

/// The kernel's `oom_kill` count for a cgroup; a scope without the memory
/// controller (or one already collected) has no such counter and reads 0.
fn oom_kill_count(path: &Path) -> u64 {
    read_counter(&path.join("memory.events"), "oom_kill").unwrap_or(0)
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
    // scope (a sibling, not a nested child, of the ambient cgroup) and hold
    // it open with a placeholder process until the real workload can be
    // moved in.
    //
    // Which manager owns the new scope is a placement decision with a real
    // failure-domain consequence (issue #1): the default `--user` scope
    // lives beneath user@UID.service and dies with the per-user manager's
    // exit.target; the opt-in `--system` scope (APLEXER_LAUNCH_SYSTEM_SCOPE
    // = system, probed first via `system_scope_escape_decision`) lives under
    // the system manager and survives it. Probe failure downgrades to the
    // user manager with a printed warning -- limits still apply either way;
    // only the survival domain differs. A failure *after* a successful probe
    // (spawn, scope wait, controller delegation) fails closed exactly as the
    // `--user` path always has: a validated backend that then breaks is a
    // real error, not a placement preference to silently swap.
    pub fn create<F>(id: Uuid, limits: &Limits, setup_started: F) -> Result<Option<Self>>
    where
        F: FnOnce(),
    {
        ensure_sigchld_compatible_for_child_management()?;
        if !limits.requested() {
            return Ok(None);
        }
        let system_scope = match system_scope_escape_decision() {
            Ok(system_scope) => system_scope,
            Err(error) => {
                eprintln!(
                    "warning: APLEXER_LAUNCH_SYSTEM_SCOPE=system requested, but the \
                     system-scope backend is unavailable ({error:#}); the workload scope \
                     falls back to the per-user manager and inherits its exit.target \
                     failure domain"
                );
                false
            }
        };
        let bus_flag = if system_scope { "--system" } else { "--user" };
        let identity = current_cgroup_identity()?;
        // Resolve every executable before starting the scope. Ambient PATH is
        // intentionally irrelevant: a user-controlled shadow helper must not
        // choose or fabricate the containment domain we later trust.
        let systemd_run = trusted_system_helper("systemd-run")?;
        let systemctl = trusted_system_helper("systemctl")?;
        let sleep = trusted_system_helper("sleep")?;
        let unit = format!("aplexer-workload-{id}");
        let mut anchor = workload_scope_command(systemd_run, bus_flag, &unit, limits, &sleep)
            .spawn()
            .context("spawn systemd-run anchor; limits fail closed")?;
        // The worker waits on this pid itself (`release_anchor_child`), so
        // register it before anything else in the process can observe it as
        // a child. See `worker::OWNED_CHILD_PIDS`.
        crate::worker::own_child_pid(anchor.id());
        // From this point, systemd may own a scope member outside the worker's
        // procfs descendant tree. Let the caller preserve recovery evidence
        // until an authoritative cgroup path has been recorded.
        setup_started();
        let path = match wait_for_scope_cgroup(
            id,
            &unit,
            &identity,
            &systemctl,
            bus_flag,
            Duration::from_secs(5),
        )
        .context("limits fail closed")
        .and_then(|path| verify_delegated_controllers(&path, limits).map(|()| path))
        {
            Ok(path) => path,
            Err(error) => return Err(cleanup_anchor_after_failure(&mut anchor, error)),
        };
        let initial_oom_kill = oom_kill_count(&path);
        Ok(Some(Self {
            path,
            identity,
            anchor: Arc::new(Mutex::new(Some(anchor))),
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
    pub fn locator(&self) -> &Path {
        &self.path
    }
    /// The same cgroup in `/proc/<pid>/cgroup` form (`/<relative>` under the
    /// cgroup-v2 root), so launch-time validation can compare what systemd
    /// was asked to create against what the workload actually reports being
    /// in (issue #1).
    pub fn proc_path(&self) -> String {
        let relative = self
            .path
            .strip_prefix(CGROUP_V2_ROOT)
            .unwrap_or(&self.path)
            .to_string_lossy()
            .to_string();
        format!("/{}", relative.trim_start_matches('/'))
    }
    pub fn identity(&self) -> &CgroupIdentity {
        &self.identity
    }
    /// Kills the placeholder process that was keeping the delegated scope
    /// alive. Call this only after the real workload pid has been added to
    /// the cgroup, so the cgroup never goes empty (and gets garbage
    /// collected by systemd) before the real workload takes residence.
    pub fn release_anchor(&self) -> Result<()> {
        let mut slot = self
            .anchor
            .lock()
            .map_err(|_| anyhow!("systemd-run anchor lock poisoned"))?;
        release_anchor_slot(&mut slot, release_anchor_child)
    }
    /// The path, once the live kernel domain has been re-pinned to the one
    /// this cgroup was created in: the precondition for every destructive
    /// pass over its members.
    fn recovered_path(&self, deadline: Instant) -> Result<&Path> {
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        verify_recorded_cgroup_identity(Some(&self.identity))?;
        Ok(&self.path)
    }
    pub fn signal_all_until(&self, signal: i32, deadline: Instant) -> Result<()> {
        signal_cgroup_path_until(self.recovered_path(deadline)?, signal, deadline)
    }
    pub fn kill_all_until(&self, deadline: Instant) -> Result<()> {
        kill_cgroup_path_until(self.recovered_path(deadline)?, deadline)
    }
    pub fn populated(&self) -> Result<bool> {
        live_cgroup_populated_with(&self.identity, || {
            read_counter(&self.path.join("cgroup.events"), "populated")
        })
    }
    pub fn oom_killed(&self) -> bool {
        oom_kill_count(&self.path) > self.initial_oom_kill
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
        let oom_kill_total = oom_kill_count(&self.path);
        serde_json::json!({
            "memory_current": read_value("memory.current"),
            "memory_peak": read_value("memory.peak"),
            "memory_swap_current": read_value("memory.swap.current"),
            "oom_kill_count": oom_kill_total,
            "oom_kill_count_since_start": oom_kill_total.saturating_sub(self.initial_oom_kill),
            // Status telemetry is explicitly best-effort; lifecycle and kill
            // paths call `populated` directly and propagate every error.
            "populated": self.populated().ok(),
        })
    }
    pub fn cleanup(&self) {
        let _ = self.release_anchor();
        let _ = fs::remove_dir(&self.path);
    }
}
