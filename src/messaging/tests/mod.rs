//! Unit tests for the messaging channel, one file per submodule; shared
//! fixtures live here.

mod cursor;
mod envelope;
mod gc;
mod layout;
mod migrate;
mod store;

use super::*;

use std::ffi::OsString;

use std::fs::{FileTimes, OpenOptions};

use std::os::unix::ffi::OsStringExt;

use std::os::unix::fs::symlink;

use std::time::Duration;

use tempfile::TempDir;

fn test_paths(root: &Path) -> Paths {
    let paths = Paths {
        runtime_root: root.join("runtime"),
        state_root: root.join("state"),
        config_file: root.join("config.toml"),
    };
    paths.ensure().unwrap();
    paths
}

fn test_message(workspace: &Path, id: Uuid) -> MessageEnvelope {
    MessageEnvelope {
        schema_version: MESSAGE_SCHEMA_VERSION,
        id,
        workspace: workspace.to_path_buf(),
        created_at: now_secs(),
        from: MessageFrom::anonymous(),
        to: Recipient::Broadcast { broadcast: true },
        kind: "note".into(),
        reply_to: None,
        body: "test".into(),
        data: None,
        delivery: Delivery::Inbox,
    }
}

fn write_test_message(paths: &Paths, workspace: &Path, id: Uuid) {
    write_message(paths, &test_message(workspace, id)).unwrap();
}

fn create_legacy_mailbox(paths: &Paths, workspace: &Path) -> MessagePaths {
    let legacy = message_paths_for_key(paths, &legacy_workspace_key(workspace));
    ensure_private_dir(&legacy.workspace_dir).unwrap();
    ensure_private_dir(&legacy.msgs_dir).unwrap();
    ensure_private_dir(&legacy.cursors_dir).unwrap();
    atomic_write_json(
        &legacy.workspace_file,
        &serde_json::json!({"workspace": workspace}),
    )
    .unwrap();
    legacy
}

fn write_message_file(mp: &MessagePaths, message: &MessageEnvelope) {
    atomic_write_bytes(
        &mp.msgs_dir.join(format!("{}.json", message.id)),
        &serialized_envelope(message).unwrap(),
    )
    .unwrap();
}

fn set_modified_secs(path: &Path, seconds: u64) {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
        .unwrap();
}
