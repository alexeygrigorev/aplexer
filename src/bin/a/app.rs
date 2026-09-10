// The binary is organized as small, responsibility-focused modules. The
// re-exports keep the command modules' internal APIs crate-private while
// preserving a narrow entry point for the binary root.
pub(crate) use anyhow::{anyhow, bail, Context, Result};
pub(crate) use aplexer::messaging::*;
pub(crate) use aplexer::*;
pub(crate) use clap::{CommandFactory, Parser};
pub(crate) use clap_complete::generate;
pub(crate) use serde_json::{json, Value};
pub(crate) use std::collections::BTreeMap;
pub(crate) use std::env;
pub(crate) use std::ffi::{CString, OsString};
pub(crate) use std::fs;
pub(crate) use std::io::{self, IsTerminal, Read, Write};
pub(crate) use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
pub(crate) use std::os::unix::ffi::OsStrExt;
pub(crate) use std::os::unix::net::UnixStream;
pub(crate) use std::os::unix::process::CommandExt;
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::process::Command;
pub(crate) use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
pub(crate) use std::sync::{Arc, Mutex, PoisonError};
pub(crate) use std::thread;
pub(crate) use std::time::{Duration, Instant};
pub(crate) use unicode_segmentation::UnicodeSegmentation;
pub(crate) use unicode_width::UnicodeWidthStr;
pub(crate) use uuid::Uuid;

#[path = "attach.rs"]
mod attach;
#[path = "attach_input.rs"]
mod attach_input;
#[path = "attach_session.rs"]
mod attach_session;
#[path = "attach_threads.rs"]
mod attach_threads;
#[path = "cli.rs"]
mod cli;
#[path = "commands.rs"]
mod commands;
#[path = "diagnostics.rs"]
mod diagnostics;
#[path = "list_helpers.rs"]
mod list_helpers;
#[path = "list_plain.rs"]
mod list_plain;
#[path = "list_tty.rs"]
mod list_tty;
#[path = "rpc.rs"]
mod rpc;
#[path = "scroll.rs"]
mod scroll;
#[path = "session_commands.rs"]
mod session_commands;
#[path = "status_bar.rs"]
mod status_bar;
#[path = "switching.rs"]
mod switching;
#[path = "system.rs"]
mod system;
#[path = "terminal.rs"]
mod terminal;

pub(crate) use attach::*;
pub(crate) use attach_input::*;
pub(crate) use attach_session::*;
pub(crate) use attach_threads::*;
pub(crate) use cli::*;
pub(crate) use commands::*;
pub(crate) use diagnostics::*;
pub(crate) use list_helpers::*;
pub(crate) use list_plain::*;
pub(crate) use list_tty::*;
pub(crate) use rpc::*;
pub(crate) use scroll::*;
pub(crate) use session_commands::*;
pub(crate) use status_bar::*;
pub(crate) use switching::*;
pub(crate) use system::*;
pub(crate) use terminal::*;

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod switching_tests {
    include!("../a_tests.rs");
}

pub(crate) fn entrypoint() {
    cli::main();
}
