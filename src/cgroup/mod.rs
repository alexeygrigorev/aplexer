//! cgroup-v2 containment: identity of the kernel domain a session was
//! created in, trusted-helper system-scope escape, scope creation with
//! anchor process, live probing, and bounded cleanup/kill/reap recovery of
//! recorded cgroups after a worker death.

mod identity;
mod recovery;
mod scope;
mod systemd;

pub use identity::*;
pub use recovery::*;
pub use scope::*;
pub use systemd::*;

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
