//! Worker-start identity persistence and the recorded-worker signal path:
//! pinning "the same Linux process" across pid reuse, and refusing to act
//! on anything less.

use super::SessionRecord;
use crate::persist::TEMP_COUNTER;
use crate::{io_kind, linux_boot_id, pidfd_open, process_start_time_ticks};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::Ordering;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ProcessIdentity {
    pub(crate) pid: u32,
    pub(crate) start_time_ticks: u64,
    pub(crate) boot_id: String,
}

pub(crate) const WORKER_IDENTITY_FILE: &str = "worker.identity.json";

/// How the process at a recorded identity's pid compares with that identity
/// right now.
pub(crate) enum WorkerIdentity {
    /// Same boot, same start time: the recorded worker itself.
    Verified,
    /// No process holds that pid any more.
    Gone,
    /// Recorded during a different boot; whatever holds the pid is unrelated.
    DifferentBoot,
    /// Same boot, but the pid was recycled by a later process.
    PidReused { recorded: u64, current: u64 },
}

/// The one boot-id and start-time comparison behind `worker_alive` and
/// `signal_recorded_worker`, so the two cannot drift on what "the same
/// worker" means. `Err` is "could not tell" (an unreadable boot id or a
/// `/proc` read failure other than the process being gone); callers decide
/// which way that fails.
pub(crate) fn verify_worker_identity(identity: &ProcessIdentity) -> Result<WorkerIdentity> {
    if linux_boot_id()? != identity.boot_id {
        return Ok(WorkerIdentity::DifferentBoot);
    }
    match process_start_time_ticks(identity.pid) {
        Ok(current) if current == identity.start_time_ticks => Ok(WorkerIdentity::Verified),
        Ok(current) => Ok(WorkerIdentity::PidReused {
            recorded: identity.start_time_ticks,
            current,
        }),
        Err(error) if io_kind(&error) == Some(io::ErrorKind::NotFound) => Ok(WorkerIdentity::Gone),
        Err(error) => Err(error),
    }
}

pub(crate) fn read_worker_identity(record: &SessionRecord) -> Result<Option<ProcessIdentity>> {
    let parent = record
        .history_path
        .parent()
        .ok_or_else(|| anyhow!("session {} has no state directory", record.id))?;
    let path = parent.join(WORKER_IDENTITY_FILE);
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("open {}", path.display())),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("untrusted worker identity file {}", path.display());
    }
    serde_json::from_reader(file).with_context(|| format!("parse {}", path.display()))
}

/// Capture the worker identity on the first record write that contains a
/// worker pid. `run_worker` writes `worker_pid` and immediately persists the
/// record, so doing this in the shared record writer keeps the identity
/// update coupled to that registration without giving later record writes a
/// chance to replace it after a pid has been recycled.
pub(crate) fn persist_worker_identity_once(path: &Path, value: &Value) -> Result<()> {
    if path.file_name() != Some(OsStr::new("session.json")) {
        return Ok(());
    }
    let Some(pid) = value
        .as_object()
        .and_then(|object| object.get("worker_pid"))
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
    else {
        return Ok(());
    };
    // Only the process registering itself may create this immutable file.
    // A different process rewriting a legacy/stale record must never bless
    // whichever unrelated process may now occupy its old numeric pid.
    if pid != std::process::id() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    let identity_path = parent.join(WORKER_IDENTITY_FILE);
    if identity_path.try_exists()? {
        return Ok(());
    }

    let identity = ProcessIdentity {
        pid,
        start_time_ticks: process_start_time_ticks(pid)
            .with_context(|| format!("inspect worker pid {pid} before recording its identity"))?,
        boot_id: linux_boot_id()?,
    };
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{WORKER_IDENTITY_FILE}.{}.{}.tmp",
        std::process::id(),
        seq
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .with_context(|| format!("create {}", temp.display()))?;
        serde_json::to_writer(&mut file, &identity)?;
        file.write_all(b"\n")?;
        file.sync_all()?;

        // A hard link is an atomic no-replace publication. If another writer
        // won the race, retain its earlier identity rather than refreshing it
        // from what may now be a recycled pid.
        match fs::hard_link(&temp, &identity_path) {
            Ok(()) => File::open(parent)?.sync_all()?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error).with_context(|| format!("publish {}", identity_path.display()));
            }
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temp);
    result
}

/// Signal the worker recorded for a session only if it is still the exact
/// Linux process registered at startup. The pidfd pins the verified process
/// across the final check/signal boundary, so an exit and pid reuse cannot
/// redirect the signal to an unrelated process.
///
/// Records created before worker identities were introduced remain readable
/// and otherwise usable, but direct stale-worker signalling fails closed.
pub fn signal_recorded_worker(record: &SessionRecord, signal: i32) -> Result<()> {
    let Some(pid) = record.worker_pid else {
        return Ok(());
    };
    let untrusted = || {
        format!(
            "session {} has no trustworthy recorded worker identity; refusing to signal pid {}",
            record.id, pid
        )
    };
    let identity = read_worker_identity(record)
        .with_context(untrusted)?
        .ok_or_else(|| anyhow!(untrusted()))?;
    if identity.pid != pid {
        bail!(
            "session {} recorded worker pid {}, but its identity belongs to pid {}; refusing to signal",
            record.id,
            pid,
            identity.pid
        );
    }

    let pidfd = match pidfd_open(pid) {
        Ok(pidfd) => pidfd,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("open pidfd for worker pid {pid}"));
        }
    };
    // Verified only after the pidfd pins the process, so an exit and pid
    // reuse between the check and the signal cannot redirect it.
    match verify_worker_identity(&identity)? {
        WorkerIdentity::Verified => {}
        WorkerIdentity::Gone => return Ok(()),
        WorkerIdentity::DifferentBoot => bail!(
            "worker pid {} for session {} was recorded during a different boot; refusing to signal",
            pid,
            record.id
        ),
        WorkerIdentity::PidReused { recorded, current } => bail!(
            "worker pid {} for session {} has been reused (recorded start {}, current start {}); refusing to signal",
            pid,
            record.id,
            recorded,
            current
        ),
    }

    crate::pidfd::send_signal(&pidfd, signal)
        .with_context(|| format!("signal worker pid {pid} through pidfd"))?;
    Ok(())
}
