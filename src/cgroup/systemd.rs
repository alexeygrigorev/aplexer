//! The systemd side of containment: trusted helper resolution, the opt-in
//! system-manager scope escape, transient scope commands, and bounded
//! `systemctl` queries for a scope's cgroup path.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::{validate_recorded_cgroup, CGROUP_V2_ROOT, TRUSTED_HELPER_DIRS};
use crate::CgroupIdentity;

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

/// `systemd-run <bus> --scope --collect [--quiet] --unit=<unit>` from the
/// trusted helper, ready for unit properties and the `-- argv` tail.
pub(super) fn systemd_run_scope(
    systemd_run: PathBuf,
    bus_flag: &str,
    unit: &str,
    quiet: bool,
) -> Command {
    let mut command = Command::new(systemd_run);
    command.arg(bus_flag).arg("--scope").arg("--collect");
    if quiet {
        command.arg("--quiet");
    }
    command.arg(format!("--unit={unit}"));
    command
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
    let mut command = systemd_run_scope(systemd_run, "--system", &unit, true);
    command
        .arg("--")
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
    probe_system_scope_backend().map(|()| true)
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
    let mut command = systemd_run_scope(
        systemd_run,
        "--system",
        &format!("aplexer-worker-{id}"),
        true,
    );
    command.arg("--").arg(&program).args(&worker_args);
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

/// Give up on a helper: kill it, reap it off the critical path (see
/// `reap_helper_child_async`), and hand back `error`.
fn abandon_helper<T>(mut child: std::process::Child, error: anyhow::Error) -> Result<T> {
    let _ = child.kill();
    reap_helper_child_async(child);
    Err(error)
}

/// Move everything currently readable from a nonblocking `reader` into
/// `buffer`: `Ok(true)` at EOF, `Ok(false)` once a read would block, and an
/// error if the total ever exceeds `limit` bytes.
fn drain_nonblocking(reader: &mut impl Read, buffer: &mut Vec<u8>, limit: usize) -> Result<bool> {
    let mut chunk = [0_u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                buffer.extend_from_slice(&chunk[..count]);
                if buffer.len() > limit {
                    bail!("output exceeds {} KiB", limit / 1024);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
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
    let Some(mut child_stdout) = child.stdout.take() else {
        return abandon_helper(child, anyhow!("{operation} helper has no stdout"));
    };
    let fd = child_stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        let error = anyhow::Error::from(io::Error::last_os_error());
        return abandon_helper(
            child,
            error.context(format!("make {operation} output nonblocking")),
        );
    }
    let mut stdout = Vec::new();
    let mut stdout_eof = false;
    let mut status = None;
    loop {
        match drain_nonblocking(&mut child_stdout, &mut stdout, 64 * 1024) {
            Ok(eof) => stdout_eof |= eof,
            Err(error) => {
                return abandon_helper(
                    child,
                    error.context(format!("read output from {operation}")),
                )
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(result)) => status = Some(result),
                Ok(None) => {}
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let error = anyhow::Error::from(error);
                    return abandon_helper(child, error.context(format!("wait for {operation}")));
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
                // A helper stuck in uninterruptible sleep must not extend the
                // startup deadline; abandoning reaps it once the kernel permits.
                return abandon_helper(child, anyhow!("timed out waiting to {operation}"));
            }
            crate::worker::disown_child_pid(helper_pid);
            bail!("timed out waiting to {operation}");
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(Duration::from_millis(20).min(remaining));
    }
}
