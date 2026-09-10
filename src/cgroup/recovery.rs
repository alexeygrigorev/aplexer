//! Bounded recovery of a recorded cgroup after its worker died: locator
//! validation inside the recorded kernel domain, populated probes, and the
//! identity-pinned signal/kill passes with their descriptor preflight.

use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::{
    ensure_cgroup2_filesystem, verify_recorded_cgroup_identity, CGROUP_RECOVERY_FD_RESERVE,
    CGROUP_V2_ROOT, MAX_CGROUP_PROCS_BYTES, MAX_CGROUP_RECOVERY_MEMBERS,
};
use crate::{pidfd_open, CgroupIdentity};

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
    // Validation does locator and identity I/O before anything below checks
    // the clock; every later step checks it on entry itself.
    check_cgroup_cleanup_deadline(deadline, "validating recorded cgroup")?;
    let Some(path) = validate_recorded_cgroup(id, locator, identity)? else {
        return Ok(());
    };

    if signal == libc::SIGKILL {
        kill_cgroup_path_until(&path, deadline)?;
    } else {
        signal_cgroup_path_until(&path, signal, deadline)?;
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
            kill_cgroup_path_until(&path, deadline)?;
        }
    }

    while cgroup_path_populated_until(&path, deadline)? {
        // Older cgroup-v2 mounts may not expose cgroup.kill. Repeat the
        // identity-pinned cgroup.procs fallback so a member that forked
        // between the first read and signal cannot escape cleanup.
        kill_cgroup_path_until(&path, deadline)?;
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
    let metadata = fs::metadata(&canonical)
        .with_context(|| format!("inspect recorded cgroup {}", canonical.display()))?;
    if !metadata.is_dir() {
        bail!(
            "recorded cgroup is not a directory: {}",
            canonical.display()
        );
    }
    ensure_cgroup2_filesystem(&canonical)?;
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
    crate::io_kind(error) == Some(io::ErrorKind::NotFound)
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
        // `read_cgroup_pids_until` admits only positive pids.
        let pidfd = match pidfd_open(pid as u32) {
            Ok(pidfd) => pidfd,
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("open pidfd for cgroup member {pid}"))
            }
        };
        members.push(CgroupMemberHandle { pid, pidfd });
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
        crate::pidfd::send_signal(&member.pidfd, signal)
            .with_context(|| format!("signal cgroup member {}", member.pid))?;
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
