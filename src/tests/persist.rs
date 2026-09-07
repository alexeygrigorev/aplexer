//! Unit tests for crash-safe persistence primitives.

use super::*;

#[test]
fn atomic_write_json_removes_temp_after_rename_failure() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("record.json");
    fs::create_dir(&destination).unwrap();

    assert!(atomic_write_json(&destination, &serde_json::json!({"secret": "value"})).is_err());
    assert_no_atomic_temps(root.path());
}

#[test]
fn atomic_write_bytes_removes_temp_after_rename_failure() {
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("history.bin");
    fs::create_dir(&destination).unwrap();

    assert!(atomic_write_bytes(&destination, b"secret bytes").is_err());
    assert_no_atomic_temps(root.path());
}

fn assert_no_atomic_temps(directory: &Path) {
    let leftovers = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().ends_with(".tmp"))
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
}
