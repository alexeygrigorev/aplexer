//! The kernel domain a cgroup locator is only meaningful inside: boot id,
//! cgroup and mount namespaces, and the cgroup-v2 root's mount identity.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use super::{CGROUP2_SUPER_MAGIC, CGROUP_V2_ROOT};
use crate::{linux_boot_id, CgroupIdentity};

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
