use super::*;

#[test]
fn workspace_key_stable_and_distinct() {
    let a = workspace_key(Path::new("/home/alexey/git/pocketshell"));
    let b = workspace_key(Path::new("/home/alexey/git/pocketshell"));
    let c = workspace_key(Path::new("/home/alexey/git/other"));
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert_eq!(a.len(), 32);
    assert_eq!(a, "9c3c95a47c6557b18956e6903a57497f");
}

#[test]
fn workspace_key_hashes_raw_unix_path_bytes() {
    let a = PathBuf::from(OsString::from_vec(b"/tmp/aplexer-\x80".to_vec()));
    let b = PathBuf::from(OsString::from_vec(b"/tmp/aplexer-\x81".to_vec()));
    assert_eq!(a.to_string_lossy(), b.to_string_lossy());
    assert_ne!(workspace_key(&a), workspace_key(&b));
}

#[test]
fn workspace_and_cursor_state_reject_special_and_oversized_files() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-mailbox-state-types-workspace");
    let mp = ensure_workspace(&paths, workspace).unwrap();

    fs::remove_file(&mp.workspace_file).unwrap();
    let fifo = std::ffi::CString::new(mp.workspace_file.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let metadata_error = verify_workspace_metadata(&mp.workspace_dir, workspace)
        .expect_err("workspace metadata FIFO must fail without blocking");
    assert!(
        format!("{metadata_error:#}").contains("not a regular file"),
        "unexpected error: {metadata_error:#}"
    );
    fs::remove_file(&mp.workspace_file).unwrap();
    atomic_write_json(
        &mp.workspace_file,
        &serde_json::json!({"workspace": workspace}),
    )
    .unwrap();

    let cursor_path = mp.cursors_dir.join(format!("{}.json", Uuid::from_u128(7)));
    let outside = root.path().join("outside-cursor.json");
    fs::write(&outside, b"{}").unwrap();
    symlink(&outside, &cursor_path).unwrap();
    let symlink_error =
        read_cursor_file(&cursor_path).expect_err("cursor symlink must not be followed");
    assert!(
        format!("{symlink_error:#}").contains("open mailbox cursor"),
        "unexpected error: {symlink_error:#}"
    );
    fs::remove_file(&cursor_path).unwrap();

    let oversized = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&cursor_path)
        .unwrap();
    oversized
        .set_len(MAX_MAILBOX_STATE_BYTES as u64 + 1)
        .unwrap();
    drop(oversized);
    let size_error = read_cursor_file(&cursor_path)
        .expect_err("oversized cursor must be rejected before reading");
    assert!(
        format!("{size_error:#}").contains("exceeds the"),
        "unexpected error: {size_error:#}"
    );
}
