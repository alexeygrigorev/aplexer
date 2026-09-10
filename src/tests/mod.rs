//! Crate-root unit tests, grouped into one file per module under test.

mod cgroup;
mod config;
mod history;
mod paths;
mod persist;
mod placement;
mod process;
mod protocol;
mod record;
mod registry;
mod util;

use super::*;
use crate::paths::{absolute_override_path, absolute_xdg_path};
use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// A cgroup created inside the caller's own delegated subtree, named
/// exactly the way a real session's containment scope is named, so
/// `validate_recorded_cgroup`'s full chain (locator shape, cgroup-v2
/// filesystem, mount device, identity triple) runs for real. Returns
/// None when the environment has no writable cgroup-v2 parent.
struct DelegatedCgroup {
    path: PathBuf,
    members: Vec<std::process::Child>,
}

impl DelegatedCgroup {
    fn create(id: Uuid) -> Option<Self> {
        let own = fs::read_to_string("/proc/self/cgroup").ok()?;
        let relative = own
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?
            .trim()
            .trim_start_matches('/')
            .to_string();
        let mut candidate = Path::new(CGROUP_V2_ROOT).join(&relative);
        let leaf = format!("aplexer-workload-{id}.scope");
        // Walk up until a parent accepts a new child cgroup: the leaf a
        // test process sits in is usually not delegated, its user@.service
        // ancestor is.
        loop {
            let path = candidate.join(&leaf);
            if fs::create_dir(&path).is_ok() {
                return Some(Self {
                    path,
                    members: Vec::new(),
                });
            }
            candidate = candidate.parent()?.to_path_buf();
            if !candidate.starts_with(CGROUP_V2_ROOT) || candidate == Path::new(CGROUP_V2_ROOT) {
                return None;
            }
        }
    }

    fn populate(&mut self) -> u32 {
        self.populate_ignoring(None)
    }

    /// Add a `sleep` member; with `ignored` set, that signal is already
    /// SIG_IGN before `exec`, so the member shrugs it off from its very
    /// first instruction (no window in which a trap is not yet installed).
    fn populate_ignoring(&mut self, ignored: Option<i32>) -> u32 {
        use std::os::unix::process::CommandExt;
        let mut command = std::process::Command::new("sleep");
        command.arg("30");
        if let Some(signal) = ignored {
            unsafe {
                command.pre_exec(move || {
                    libc::signal(signal, libc::SIG_IGN);
                    Ok(())
                });
            }
        }
        let child = command.spawn().expect("spawn cgroup member");
        let pid = child.id();
        self.members.push(child);
        fs::write(self.path.join("cgroup.procs"), format!("{pid}\n"))
            .expect("move member into the delegated cgroup");
        pid
    }

    /// Stop every member and reap it, so the cgroup can be collected and
    /// no `sleep` outlives the test.
    fn drain_members(&mut self) {
        for mut member in self.members.drain(..) {
            let _ = member.kill();
            let _ = member.wait();
        }
    }
}

impl Drop for DelegatedCgroup {
    fn drop(&mut self) {
        self.drain_members();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.path.exists() && Instant::now() < deadline {
            if fs::remove_dir(&self.path).is_ok() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
