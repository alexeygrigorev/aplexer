//! Crash-safe persistence primitives shared by every on-disk artifact:
//! temp-file-and-rename writes for JSON records and raw bytes, and advisory
//! whole-file locks.

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
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
