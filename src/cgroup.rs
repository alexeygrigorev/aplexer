//! cgroup-v2 containment: identity of the kernel domain a session was
//! created in, trusted-helper system-scope escape, scope creation with
//! anchor process, live probing, and bounded cleanup/kill/reap recovery of
//! recorded cgroups after a worker death.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::{
    ensure_sigchld_compatible_for_child_management, linux_boot_id, CgroupIdentity, Limits,
};

pub(crate) const MAX_CGROUP_RECOVERY_MEMBERS: usize = 4096;
pub(crate) const MAX_CGROUP_PROCS_BYTES: u64 = 128 * 1024;
pub(crate) const CGROUP_RECOVERY_FD_RESERVE: u64 = 16;

pub(crate) const CGROUP_V2_ROOT: &str = "/sys/fs/cgroup";
pub(crate) const CGROUP2_SUPER_MAGIC: libc::c_long = 0x6367_7270;
pub(crate) const TRUSTED_HELPER_DIRS: &[&str] = &[
    "/usr/bin",
    "/bin",
    "/usr/local/bin",
    "/run/current-system/sw/bin",
];

pub(crate) fn namespace_coordinates(path: &Path, label: &str) -> Result<(u64, u64)> {
    let metadata = fs::metadata(path).with_context(|| format!("inspect {label} namespace"))?;
    Ok((metadata.dev(), metadata.ino()))
}

pub(crate) fn ensure_cgroup2_filesystem(path: &Path) -> Result<()> {
    let encoded = CString::new(path.as_os_str().as_bytes())
        .with_context(|| format!("encode cgroup path {}", path.display()))?;
    let mut stats: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(encoded.as_ptr(), &mut stats) } != 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("inspect filesystem for {}", path.display()));
    }
    if stats.f_type != CGROUP2_SUPER_MAGIC {
        bail!("{} is not on a cgroup-v2 filesystem", path.display());
    }
    Ok(())
}

pub(crate) fn mount_id_for_file(file: &File) -> Result<u64> {
    let path = format!("/proc/self/fdinfo/{}", file.as_raw_fd());
    let info = fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
    info.lines()
        .find_map(|line| line.strip_prefix("mnt_id:"))
        .map(str::trim)
        .ok_or_else(|| anyhow!("{path} has no mount identity"))?
        .parse()
        .with_context(|| format!("parse mount identity from {path}"))
}

/// Capture the kernel domain that gives a persisted cgroup locator meaning.
/// The root checks also keep resource-limit setup from accepting a lookalike
/// directory mounted at `/sys/fs/cgroup`.
pub fn current_cgroup_identity() -> Result<CgroupIdentity> {
    let root = Path::new(CGROUP_V2_ROOT);
    let root_handle = File::open(root).context("open cgroup-v2 root")?;
    let root_metadata = root_handle.metadata().context("inspect cgroup-v2 root")?;
    if !root_metadata.is_dir() {
        bail!("{CGROUP_V2_ROOT} is not a directory");
    }
    ensure_cgroup2_filesystem(root)?;
    let controllers = root.join("cgroup.controllers");
    if !fs::metadata(&controllers)
        .with_context(|| format!("inspect {}", controllers.display()))?
        .is_file()
    {
        bail!(
            "{} is not a cgroup-v2 controllers file",
            controllers.display()
        );
    }
    let (cgroup_namespace_device, cgroup_namespace_inode) =
        namespace_coordinates(Path::new("/proc/self/ns/cgroup"), "cgroup")?;
    let (mount_namespace_device, mount_namespace_inode) =
        namespace_coordinates(Path::new("/proc/self/ns/mnt"), "mount")?;
    Ok(CgroupIdentity {
        boot_id: linux_boot_id()?,
        cgroup_namespace_device,
        cgroup_namespace_inode,
        mount_namespace_device,
        mount_namespace_inode,
        cgroup_mount_id: mount_id_for_file(&root_handle)?,
        cgroup_root_device: root_metadata.dev(),
        cgroup_root_inode: root_metadata.ino(),
    })
}

pub(crate) fn verify_recorded_cgroup_identity(
    recorded: Option<&CgroupIdentity>,
) -> Result<CgroupIdentity> {
    let recorded = recorded.ok_or_else(|| {
        anyhow!(
            "recorded cgroup has no boot/namespace/mount identity; refusing legacy destructive recovery"
        )
    })?;
    let current = current_cgroup_identity()?;
    if recorded != &current {
        bail!(
            "recorded cgroup identity does not match the current boot, cgroup namespace, mount namespace, or cgroup-v2 root"
        );
    }
    Ok(current)
}

pub(crate) fn validate_trusted_helper(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("helper path is not absolute: {}", path.display());
    }
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("resolve helper executable {}", path.display()))?;
    let metadata = fs::metadata(&canonical)
        .with_context(|| format!("inspect helper executable {}", canonical.display()))?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o022 != 0
    {
        bail!(
            "untrusted helper executable {} (must be root-owned, executable, and not group/world writable)",
            canonical.display()
        );
    }
    let mut parent = canonical.parent();
    while let Some(directory) = parent {
        let metadata = fs::metadata(directory)
            .with_context(|| format!("inspect helper directory {}", directory.display()))?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!(
                "untrusted helper directory {} (must be root-owned and not group/world writable)",
                directory.display()
            );
        }
        parent = directory.parent();
    }
    Ok(canonical)
}

pub(crate) fn trusted_system_helper(name: &str) -> Result<PathBuf> {
    if name.is_empty() || name.contains('/') {
        bail!("invalid system helper name {name:?}");
    }
    let mut failures = Vec::new();
    for directory in TRUSTED_HELPER_DIRS {
        let candidate = Path::new(directory).join(name);
        match validate_trusted_helper(&candidate) {
            Ok(path) => return Ok(path),
            Err(error) => failures.push(format!("{}: {error:#}", candidate.display())),
        }
    }
    bail!(
        "no trusted absolute {name} helper was found; {}",
        failures.join("; ")
    )
}

/// End-to-end probe of the system-manager scope backend behind the
/// `APLEXER_LAUNCH_SYSTEM_SCOPE=system` escape (issue #1): resolves the same
/// trusted helpers the real launch path resolves, then creates and collects
/// one trivial transient scope (`-- true`) on the system manager. This is
/// the only way to know the backend actually works -- as a regular user it
/// usually does not (`org.freedesktop.systemd1.manage-units` needs root or
/// a polkit authorization), and guessing would turn the opt-in escape into
/// a broken start. Probing creates no lasting state: the scope runs `true`,
/// exits, and `--collect` garbage-collects it. Never called unless the env
/// opt-in is set.
pub fn probe_system_scope_backend() -> Result<()> {
    let systemd_run = trusted_system_helper("systemd-run")?;
    let true_binary = trusted_system_helper("true")?;
    let unit = format!("aplexer-escape-probe-{}", Uuid::new_v4().simple());
    let mut command = Command::new(systemd_run);
    command
        .args([
            "--system",
            "--scope",
            "--collect",
            "--quiet",
            &format!("--unit={unit}"),
            "--",
        ])
        .arg(&true_binary)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut probe = command
        .spawn()
        .with_context(|| format!("spawn system-scope probe from {}", true_binary.display()))?;
    // Same discipline as every other helper child: register the pid so the
    // worker's descendant reaper cannot consume its status, and always reap
    // it here before dropping.
    let probe_pid = probe.id();
    crate::worker::own_child_pid(probe_pid);
    let probe_result = (|| -> Result<std::process::ExitStatus> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match probe.try_wait()? {
                Some(status) => return Ok(status),
                None if Instant::now() >= deadline => {
                    let _ = probe.kill();
                    let _ = probe.wait();
                    bail!("systemd-run system-scope probe timed out after 10s");
                }
                None => thread::sleep(Duration::from_millis(20)),
            }
        }
    })();
    crate::worker::disown_child_pid(probe_pid);
    let status = probe_result?;
    if !status.success() {
        bail!(
            "systemd-run --system --scope probe exited with {status}; creating system \
             manager scopes needs root or polkit authorization \
             (org.freedesktop.systemd1.manage-units)"
        );
    }
    Ok(())
}

/// Decide, once per launch, whether the opt-in escape backend is usable.
/// `Ok(true)`/`Ok(false)` mean "system scope"/"user manager fallback" for
/// the caller's placement decision; the error is the probe failure, for the
/// caller to report honestly instead of silently pretending the escape
/// happened (issue #1: warn or fail clearly).
pub fn system_scope_escape_decision() -> Result<bool> {
    if !crate::placement::system_scope_requested() {
        return Ok(false);
    }
    match probe_system_scope_backend() {
        Ok(()) => Ok(true),
        Err(error) => Err(error),
    }
}

/// Rewrite `worker` (already carrying the worker program and its initial
/// argv from `worker_command`) so spawning it creates the worker inside a
/// system-manager scope (`systemd-run --system --scope --collect
/// --unit=aplexer-worker-<id>`) instead of bare `setsid()` in the ambient
/// cgroup (issue #1). Everything configured on the command afterwards --
/// per-session env, `--rows/--cols` -- lands on the systemd-run wrapper and
/// is passed through to the worker child: systemd-run shares its own
/// environment with the scope's process, and argv after `--` is the
/// worker's argv verbatim.
///
/// The `pre_exec` closure the caller installs afterwards (setsid + signal
/// blocking) then applies to systemd-run itself; the worker inherits the
/// blocked-signal baseline it unblocks at startup and needs no session of
/// its own (the workload's spawn does its own `setsid` + `TIOCSCTTY`).
/// The wrapper stays the worker's parent for the worker's whole life --
/// one extra small process per escaped session -- and `--collect` removes
/// the scope as soon as the worker exits.
///
/// `Ok(())` means the command now spawns into the escape scope; the error
/// is the reason the escape was not applied, for the caller to surface.
/// The caller falls back to the plain (setsid-only, ambient-cgroup) spawn
/// in that case: the issue asks for honest degradation with a warning,
/// never for a broken start.
pub(crate) fn wrap_worker_in_system_scope(id: Uuid, worker: &mut Command) -> Result<()> {
    let systemd_run = trusted_system_helper("systemd-run")?;
    let program = worker.get_program().to_os_string();
    let worker_args: Vec<OsString> = worker.get_args().map(|arg| arg.to_os_string()).collect();
    let mut command = Command::new(systemd_run);
    command.args([
        "--system",
        "--scope",
        "--collect",
        "--quiet",
        &format!("--unit=aplexer-worker-{id}"),
        "--",
    ]);
    command.arg(&program);
    command.args(&worker_args);
    *worker = command;
    Ok(())
}

pub(crate) fn control_group_locator(id: Uuid, value: &str) -> Result<PathBuf> {
    let value = value.trim();
    let reported = Path::new(value);
    if value.is_empty() || value == "/" || !reported.is_absolute() {
        bail!("systemd returned invalid ControlGroup value {value:?}");
    }
    let mut relative = PathBuf::new();
    for component in reported.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => relative.push(value),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                bail!("systemd ControlGroup escapes cgroup root: {value:?}")
            }
        }
    }
    let expected = format!("aplexer-workload-{id}.scope");
    if relative.file_name() != Some(OsStr::new(&expected)) {
        bail!("systemd ControlGroup does not belong to session {id}: {value:?}");
    }
    Ok(Path::new(CGROUP_V2_ROOT).join(relative))
}

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
        let mut command = Command::new(systemd_run);
        command
            .arg(bus_flag)
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
            .arg(sleep)
            .arg("infinity")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut anchor = command
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
        ) {
            Ok(path) => path,
            Err(error) => {
                return Err(cleanup_anchor_after_failure(
                    &mut anchor,
                    error.context("limits fail closed"),
                ));
            }
        };
        if limits.memory_bytes.is_some() && !path.join("memory.max").exists() {
            return Err(cleanup_anchor_after_failure(
                &mut anchor,
                anyhow!("systemd did not delegate the memory controller; limits fail closed"),
            ));
        }
        if limits.pids.is_some() && !path.join("pids.max").exists() {
            return Err(cleanup_anchor_after_failure(
                &mut anchor,
                anyhow!("systemd did not delegate the pids controller; limits fail closed"),
            ));
        }
        let initial_oom_kill = read_counter(&path.join("memory.events"), "oom_kill").unwrap_or(0);
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
    pub fn signal_all_until(&self, signal: i32, deadline: Instant) -> Result<()> {
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        verify_recorded_cgroup_identity(Some(&self.identity))?;
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        signal_cgroup_path_until(&self.path, signal, deadline)
    }
    pub fn kill_all_until(&self, deadline: Instant) -> Result<()> {
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        verify_recorded_cgroup_identity(Some(&self.identity))?;
        check_cgroup_cleanup_deadline(deadline, "validating live cgroup identity")?;
        kill_cgroup_path_until(&self.path, deadline)
    }
    pub fn populated(&self) -> Result<bool> {
        live_cgroup_populated_with(&self.identity, || {
            read_counter(&self.path.join("cgroup.events"), "populated")
        })
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
        let oom_kill_total =
            read_counter(&self.path.join("memory.events"), "oom_kill").unwrap_or(0);
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

/// Validate and recover a resource-limited session through its recorded
/// kernel containment domain. A path that has disappeared after it was
/// durably recorded is empty by construction: cgroup v2 cannot remove a
/// populated cgroup. Every other inspection error fails closed.
pub fn cleanup_recorded_cgroup(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
    signal: i32,
    grace: Duration,
) -> Result<()> {
    let deadline = Instant::now()
        .checked_add(grace)
        .and_then(|deadline| deadline.checked_add(Duration::from_secs(2)))
        .ok_or_else(|| anyhow!("cgroup cleanup deadline overflow"))?;
    cleanup_recorded_cgroup_until(id, locator, identity, signal, grace, deadline)
}

/// Preflight a durable locator before destroying a broken session's worker
/// subreaper. This performs no signalling; it only establishes that later
/// cgroup recovery will operate inside the expected kernel domain.
pub fn validate_recorded_cgroup_locator(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
) -> Result<()> {
    validate_recorded_cgroup(id, locator, identity).map(|_| ())
}

/// Deadline-sharing variant for startup rollback, where cgroup recovery must
/// consume the same wall-clock budget as procfs discovery and pidfd cleanup.
pub fn cleanup_recorded_cgroup_until(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
    signal: i32,
    grace: Duration,
    deadline: Instant,
) -> Result<()> {
    check_cgroup_cleanup_deadline(deadline, "validating recorded cgroup")?;
    let Some(path) = validate_recorded_cgroup(id, locator, identity)? else {
        check_cgroup_cleanup_deadline(deadline, "validating recorded cgroup")?;
        return Ok(());
    };
    check_cgroup_cleanup_deadline(deadline, "validating recorded cgroup")?;

    if signal == libc::SIGKILL {
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        kill_cgroup_path_until(&path, deadline)?;
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
    } else {
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup")?;
        signal_cgroup_path_until(&path, signal, deadline)?;
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup")?;
        let grace_deadline = Instant::now()
            .checked_add(grace)
            .ok_or_else(|| anyhow!("cgroup cleanup grace deadline overflow"))?
            .min(deadline);
        // Grace expiry is the cue to escalate, never an error: only the
        // overall deadline (checked by the populated probe) can fail here.
        while cgroup_path_populated_until(&path, deadline)? {
            let now = Instant::now();
            if now >= grace_deadline {
                break;
            }
            thread::sleep(Duration::from_millis(25).min(grace_deadline - now));
        }
        if cgroup_path_populated_until(&path, deadline)? {
            check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
            kill_cgroup_path_until(&path, deadline)?;
            check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        }
    }

    while cgroup_path_populated_until(&path, deadline)? {
        // Older cgroup-v2 mounts may not expose cgroup.kill. Repeat the
        // identity-pinned cgroup.procs fallback so a member that forked
        // between the first read and signal cannot escape cleanup.
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        kill_cgroup_path_until(&path, deadline)?;
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        sleep_until_cgroup_deadline(deadline, "proving recorded cgroup empty")?;
    }
    Ok(())
}

pub(crate) fn check_cgroup_cleanup_deadline(deadline: Instant, operation: &str) -> Result<()> {
    if Instant::now() >= deadline {
        bail!("timed out {operation}");
    }
    Ok(())
}

pub(crate) fn sleep_until_cgroup_deadline(deadline: Instant, operation: &str) -> Result<()> {
    check_cgroup_cleanup_deadline(deadline, operation)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    thread::sleep(Duration::from_millis(25).min(remaining));
    check_cgroup_cleanup_deadline(deadline, operation)
}

pub(crate) fn validate_recorded_cgroup(
    id: Uuid,
    locator: &Path,
    identity: Option<&CgroupIdentity>,
) -> Result<Option<PathBuf>> {
    // This comparison intentionally precedes canonicalizing the leaf. A
    // missing leaf proves emptiness only inside the exact kernel domain in
    // which it was durably recorded.
    let current_identity = verify_recorded_cgroup_identity(identity)?;
    let root = Path::new(CGROUP_V2_ROOT);
    let expected = format!("aplexer-workload-{id}.scope");
    if !locator.is_absolute()
        || !locator.starts_with(root)
        || locator.file_name() != Some(OsStr::new(&expected))
        || locator
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!(
            "untrusted recorded cgroup locator for session {id}: {}",
            locator.display()
        );
    }
    let canonical = match fs::canonicalize(locator) {
        Ok(path) => path,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("resolve recorded cgroup {}", locator.display()))
        }
    };
    let canonical_root = fs::canonicalize(root).context("resolve cgroup v2 root")?;
    if !canonical.starts_with(&canonical_root)
        || canonical.file_name() != Some(OsStr::new(&expected))
    {
        bail!(
            "recorded cgroup for session {id} escaped the cgroup root: {}",
            canonical.display()
        );
    }
    if !fs::metadata(&canonical)?.is_dir() {
        bail!(
            "recorded cgroup is not a directory: {}",
            canonical.display()
        );
    }
    ensure_cgroup2_filesystem(&canonical)?;
    let metadata = fs::metadata(&canonical)?;
    if metadata.dev() != current_identity.cgroup_root_device {
        bail!(
            "recorded cgroup {} is on a different cgroup-v2 mount",
            canonical.display()
        );
    }
    let procs = canonical.join("cgroup.procs");
    if !fs::metadata(&procs)
        .with_context(|| format!("inspect {}", procs.display()))?
        .is_file()
    {
        bail!("{} is not a cgroup member file", procs.display());
    }
    Ok(Some(canonical))
}

pub(crate) fn cgroup_path_populated(path: &Path) -> Result<bool> {
    match read_counter(&path.join("cgroup.events"), "populated") {
        Ok(value) => Ok(value != 0),
        Err(error) if error_is_not_found(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) fn error_is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
    })
}

/// Read live membership only while the cgroup pathname still belongs to the
/// exact kernel domain captured at creation. A collected scope loses both its
/// directory and `cgroup.events`; ENOENT therefore means empty, but only when
/// the domain matches both before and after that observation.
pub(crate) fn live_cgroup_populated_with(
    identity: &CgroupIdentity,
    read_populated: impl FnOnce() -> Result<u64>,
) -> Result<bool> {
    verify_recorded_cgroup_identity(Some(identity))
        .context("validate live cgroup identity before reading membership")?;
    let populated = match read_populated() {
        Ok(value) => Some(value != 0),
        Err(error) if error_is_not_found(&error) => None,
        Err(error) => return Err(error).context("read live cgroup membership"),
    };
    verify_recorded_cgroup_identity(Some(identity))
        .context("validate live cgroup identity after reading membership")?;
    Ok(populated.unwrap_or(false))
}

pub(crate) fn cgroup_path_populated_until(path: &Path, deadline: Instant) -> Result<bool> {
    check_cgroup_cleanup_deadline(deadline, "inspecting recorded cgroup")?;
    let populated = cgroup_path_populated(path)?;
    check_cgroup_cleanup_deadline(deadline, "inspecting recorded cgroup")?;
    Ok(populated)
}

pub(crate) fn read_cgroup_pids_until(path: &Path, deadline: Instant) -> Result<BTreeSet<i32>> {
    check_cgroup_cleanup_deadline(deadline, "reading recorded cgroup members")?;
    let procs = path.join("cgroup.procs");
    let file = match File::open(&procs) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", procs.display())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_CGROUP_PROCS_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", procs.display()))?;
    check_cgroup_cleanup_deadline(deadline, "reading recorded cgroup members")?;
    if bytes.len() as u64 > MAX_CGROUP_PROCS_BYTES {
        bail!("recorded cgroup member list exceeds safe byte limit of {MAX_CGROUP_PROCS_BYTES}");
    }
    let text =
        std::str::from_utf8(&bytes).with_context(|| format!("decode {}", procs.display()))?;
    let mut pids = BTreeSet::new();
    for value in text.lines() {
        check_cgroup_cleanup_deadline(deadline, "parsing recorded cgroup members")?;
        if pids.len() >= MAX_CGROUP_RECOVERY_MEMBERS {
            bail!("recorded cgroup exceeds safe member limit of {MAX_CGROUP_RECOVERY_MEMBERS}");
        }
        let pid = value
            .parse::<i32>()
            .with_context(|| format!("parse pid in {}/cgroup.procs", path.display()))?;
        if pid <= 0 {
            bail!("invalid pid {pid} in {}/cgroup.procs", path.display());
        }
        pids.insert(pid);
    }
    Ok(pids)
}

pub(crate) struct CgroupMemberHandle {
    pid: i32,
    pidfd: File,
}

pub(crate) fn signal_cgroup_path_until(path: &Path, signal: i32, deadline: Instant) -> Result<()> {
    let candidates = read_cgroup_pids_until(path, deadline)?;
    let capacity = cgroup_recovery_pidfd_capacity(deadline)?;
    if candidates.len() > capacity {
        bail!(
            "recorded cgroup has {} members but only {capacity} pidfds can be opened safely",
            candidates.len()
        );
    }
    let mut members = Vec::with_capacity(candidates.len());
    for pid in candidates {
        check_cgroup_cleanup_deadline(deadline, "pinning recorded cgroup members")?;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
        if fd < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            return Err(error).with_context(|| format!("open pidfd for cgroup member {pid}"));
        }
        members.push(CgroupMemberHandle {
            pid,
            pidfd: unsafe { File::from_raw_fd(fd) },
        });
    }

    // A pidfd pins process identity; this second membership snapshot ensures
    // each pinned identity still belongs to the recorded domain before it is
    // signalled. New forks are handled by the repeated populated/kill loop.
    let current = read_cgroup_pids_until(path, deadline)?;
    for member in members {
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup members")?;
        if !current.contains(&member.pid) {
            continue;
        }
        let result = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                member.pidfd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).with_context(|| format!("signal cgroup member {}", member.pid));
            }
        }
        check_cgroup_cleanup_deadline(deadline, "signalling recorded cgroup members")?;
    }
    Ok(())
}

pub(crate) fn cgroup_recovery_pidfd_capacity(deadline: Instant) -> Result<usize> {
    check_cgroup_cleanup_deadline(deadline, "preflighting cgroup recovery descriptors")?;
    let mut limit: libc::rlimit = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return Err(io::Error::last_os_error()).context("read RLIMIT_NOFILE for cgroup recovery");
    }
    check_cgroup_cleanup_deadline(deadline, "preflighting cgroup recovery descriptors")?;
    let descriptors = fs::read_dir("/proc/self/fd").context("count open recovery descriptors")?;
    let mut open = 0_u64;
    for descriptor in descriptors {
        check_cgroup_cleanup_deadline(deadline, "counting open recovery descriptors")?;
        descriptor.context("enumerate open recovery descriptors")?;
        open = open
            .checked_add(1)
            .ok_or_else(|| anyhow!("open recovery descriptor count overflow"))?;
    }
    let soft_limit = if limit.rlim_cur == libc::RLIM_INFINITY {
        u64::MAX
    } else {
        limit.rlim_cur
    };
    Ok(cgroup_recovery_pidfd_capacity_from_counts(soft_limit, open))
}

pub(crate) fn cgroup_recovery_pidfd_capacity_from_counts(soft_limit: u64, open: u64) -> usize {
    let available = soft_limit
        .saturating_sub(open)
        .saturating_sub(CGROUP_RECOVERY_FD_RESERVE);
    usize::try_from(available)
        .unwrap_or(usize::MAX)
        .min(MAX_CGROUP_RECOVERY_MEMBERS)
}

pub(crate) fn kill_cgroup_path_until(path: &Path, deadline: Instant) -> Result<()> {
    check_cgroup_cleanup_deadline(deadline, "checking recorded cgroup kill support")?;
    let kill = path.join("cgroup.kill");
    if kill.exists() {
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        let result = match fs::write(&kill, "1") {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("write {}", kill.display())),
        };
        check_cgroup_cleanup_deadline(deadline, "killing recorded cgroup")?;
        result
    } else {
        signal_cgroup_path_until(path, libc::SIGKILL, deadline)
    }
}
pub(crate) fn wait_for_scope_cgroup(
    id: Uuid,
    unit: &str,
    identity: &CgroupIdentity,
    systemctl: &Path,
    bus_flag: &str,
    timeout: Duration,
) -> Result<PathBuf> {
    wait_for_scope_cgroup_with(id, unit, systemctl, bus_flag, timeout, |path| {
        validate_recorded_cgroup(id, path, Some(identity))
    })
}

pub(crate) fn wait_for_scope_cgroup_with(
    id: Uuid,
    unit: &str,
    systemctl: &Path,
    bus_flag: &str,
    timeout: Duration,
    mut validate: impl FnMut(&Path) -> Result<Option<PathBuf>>,
) -> Result<PathBuf> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut command = Command::new(systemctl);
        command.args([
            bus_flag,
            "show",
            &format!("{unit}.scope"),
            "-p",
            "ControlGroup",
            "--value",
        ]);
        let output = command_output_until(&mut command, deadline, "query systemd scope")?;
        if output.status.success() {
            let value = std::str::from_utf8(&output.stdout)
                .context("decode systemd ControlGroup output")?;
            // systemd may publish the unit before assigning its ControlGroup.
            // Empty and root are transient "not assigned yet" values; every
            // other malformed, escaping, or wrong-session value is hostile
            // evidence and must fail closed rather than being retried.
            if !matches!(value.trim(), "" | "/") {
                let path = control_group_locator(id, value)?;
                if let Some(path) = validate(&path)? {
                    return Ok(path);
                }
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for systemd scope {unit}.scope to appear");
        }
        thread::sleep(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

/// Reap a startup helper off the caller's critical path. The pid stays
/// registered as worker-owned (see `worker::OWNED_CHILD_PIDS`) until this
/// thread's `wait` returns, so the worker's descendant reaper cannot take
/// the status out from under it and cannot be handed a recycled pid early.
pub(crate) fn reap_helper_child_async(mut child: std::process::Child) {
    let pid = child.id();
    thread::spawn(move || {
        let _ = child.wait();
        crate::worker::disown_child_pid(pid);
    });
}

/// Run a small setup query without allowing a wedged helper to defeat the
/// caller's wall-clock timeout. Stdout is intentionally bounded: systemctl's
/// ControlGroup value is one short path, and anything larger is malformed.
pub(crate) fn command_output_until(
    command: &mut Command,
    deadline: Instant,
    operation: &str,
) -> Result<std::process::Output> {
    if Instant::now() >= deadline {
        bail!("timed out before {operation}");
    }
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn helper to {operation}"))?;
    // This helper's status belongs to this function (or to the detached
    // waiter `reap_helper_child_async` starts), never to the worker's
    // descendant reaper.
    let helper_pid = child.id();
    crate::worker::own_child_pid(helper_pid);
    let mut child_stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("{operation} helper has no stdout"))?;
    let flags = unsafe { libc::fcntl(child_stdout.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe {
            libc::fcntl(
                child_stdout.as_raw_fd(),
                libc::F_SETFL,
                flags | libc::O_NONBLOCK,
            )
        } < 0
    {
        let error = io::Error::last_os_error();
        let _ = child.kill();
        reap_helper_child_async(child);
        return Err(error).with_context(|| format!("make {operation} output nonblocking"));
    }
    let mut stdout = Vec::new();
    let mut stdout_eof = false;
    let mut status = None;
    loop {
        loop {
            let mut buffer = [0_u8; 4096];
            match child_stdout.read(&mut buffer) {
                Ok(0) => {
                    stdout_eof = true;
                    break;
                }
                Ok(count) => {
                    stdout.extend_from_slice(&buffer[..count]);
                    if stdout.len() > 64 * 1024 {
                        let _ = child.kill();
                        reap_helper_child_async(child);
                        bail!("output from {operation} exceeds 64 KiB");
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = child.kill();
                    reap_helper_child_async(child);
                    return Err(error).with_context(|| format!("read output from {operation}"));
                }
            }
        }

        if status.is_none() {
            match child.try_wait() {
                Ok(Some(result)) => status = Some(result),
                Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = child.kill();
                    reap_helper_child_async(child);
                    return Err(error).with_context(|| format!("wait for {operation}"));
                }
            }
        }
        if let (Some(status), true) = (status, stdout_eof) {
            crate::worker::disown_child_pid(helper_pid);
            return Ok(std::process::Output {
                status,
                stdout,
                stderr: Vec::new(),
            });
        }

        if Instant::now() >= deadline {
            if status.is_none() {
                let _ = child.kill();
                // A helper stuck in uninterruptible sleep must not extend the
                // startup deadline. Reap asynchronously once the kernel permits.
                reap_helper_child_async(child);
            } else {
                crate::worker::disown_child_pid(helper_pid);
            }
            bail!("timed out waiting to {operation}");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(Duration::from_millis(20).min(remaining));
    }
}
pub(crate) fn read_counter(path: &Path, key: &str) -> Result<u64> {
    let text = fs::read_to_string(path)?;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some(key) {
            let value = parts
                .next()
                .ok_or_else(|| anyhow!("counter {key} in {} has no value", path.display()))?;
            return value
                .parse()
                .with_context(|| format!("parse counter {key} in {}", path.display()));
        }
    }
    bail!("counter {key} not found in {}", path.display())
}
