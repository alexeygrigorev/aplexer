#![cfg(target_os = "linux")]

pub mod agent_events;
pub mod agent_kind;
pub mod api;
pub mod hooks;
pub mod messaging;
pub mod placement;
pub mod screen;
pub mod watch;
pub mod worker;

mod cgroup;
pub use cgroup::*;

mod history;
pub use history::*;

mod protocol;
pub use protocol::*;

mod config;
pub use config::*;

mod util;
pub use util::*;

mod process;
pub use process::*;

mod record;
pub use record::*;

mod registry;
pub use registry::{list_records, read_record, read_session_record, resolve_record};

mod paths;
pub use paths::{canonical_workspace, ensure_private_dir, Paths};

mod persist;
pub use persist::{atomic_write_bytes, atomic_write_json, FileLock};

#[cfg(feature = "python")]
mod python;

#[cfg(test)]
mod tests;
