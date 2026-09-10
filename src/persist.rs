//! Crash-safe persistence primitives shared by every on-disk artifact:
//! temp-file-and-rename writes for JSON records and raw bytes, and advisory
//! whole-file locks.

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{ensure_private_dir, persist_worker_identity_once};

pub(crate) static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) struct AtomicTempGuard(PathBuf);

impl Drop for AtomicTempGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    ensure_private_dir(parent)?;
    let value = serde_json::to_value(value)?;
    persist_worker_identity_once(path, &value)?;
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
    let _temp_guard = AtomicTempGuard(temp.clone());
    serde_json::to_writer_pretty(&mut file, &value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temp, path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = parent_dir(path)?;
    ensure_private_dir(parent)?;
    write_atomically(parent, path, bytes, 0o600)
}

/// `atomic_write_bytes` with an explicit file mode, for files outside
/// aplexer's private state tree (engine configs in the user's home). The
/// parent directory must already exist and is left exactly as found: this
/// never forces it private.
pub fn atomic_write_bytes_with_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    write_atomically(parent_dir(path)?, path, bytes, mode)
}

fn parent_dir(path: &Path) -> Result<&Path> {
    path.parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))
}

fn write_atomically(parent: &Path, path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .unwrap_or(OsStr::new("bytes"))
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
    let _temp_guard = AtomicTempGuard(temp.clone());
    // Created private, then widened while still empty: fchmod is not
    // subject to the umask, so the requested mode lands exactly, and no
    // content is ever visible at a wider mode than it will end up with.
    if mode != 0o600 {
        file.set_permissions(fs::Permissions::from_mode(mode))
            .with_context(|| format!("chmod {}", temp.display()))?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, path)
        .with_context(|| format!("rename {} to {}", temp.display(), path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

/// Reads a small file the caller has already opened and vetted, refusing
/// more than `cap` bytes both by size up front and by a recount after the
/// read, so a regular file that grows after fstat is rejected rather than
/// parsed from a truncated prefix.
pub(crate) fn read_bounded(file: File, path: &Path, label: &str, cap: usize) -> Result<Vec<u8>> {
    let length = file
        .metadata()
        .with_context(|| format!("inspect {label} {}", path.display()))?
        .len();
    if length > cap as u64 {
        bail!(
            "{label} {} exceeds the {cap}-byte cap (got {length} bytes)",
            path.display()
        );
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {label} {}", path.display()))?;
    if bytes.len() > cap {
        bail!("{label} {} exceeds the {cap}-byte cap", path.display());
    }
    Ok(bytes)
}

/// `read_bounded`, parsed as JSON.
pub(crate) fn read_bounded_json<T: DeserializeOwned>(
    file: File,
    path: &Path,
    label: &str,
    cap: usize,
) -> Result<T> {
    let bytes = read_bounded(file, path, label, cap)?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {label} {}", path.display()))
}

/// Opens `path` without following a final-component symlink or blocking
/// on an accidental FIFO/device, then reads it whole under `cap`. `None`
/// when absent, so callers with a documented empty state keep it; every
/// other file type and any oversize fails closed.
pub(crate) fn read_bounded_regular_file(
    path: &Path,
    label: &str,
    cap: usize,
) -> Result<Option<Vec<u8>>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("open {label} {}", path.display()))
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if !metadata.file_type().is_file() {
        bail!("{label} is not a regular file: {}", path.display());
    }
    Ok(Some(read_bounded(file, path, label, cap)?))
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
            .truncate(false)
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
