use super::*;

#[test]
fn ensure_workspace_migrates_valid_legacy_mailbox() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-legacy-workspace");
    let legacy = message_paths_for_key(&paths, &legacy_workspace_key(workspace));
    ensure_private_dir(&legacy.workspace_dir).unwrap();
    ensure_private_dir(&legacy.msgs_dir).unwrap();
    ensure_private_dir(&legacy.cursors_dir).unwrap();
    atomic_write_json(
        &legacy.workspace_file,
        &serde_json::json!({"workspace": workspace}),
    )
    .unwrap();
    fs::write(legacy.msgs_dir.join("migration-marker"), b"present").unwrap();

    let migrated = ensure_workspace(&paths, workspace).unwrap();
    assert_eq!(
        migrated.workspace_dir,
        message_paths(&paths, workspace).workspace_dir
    );
    assert!(migrated.msgs_dir.join("migration-marker").exists());
    assert!(!legacy.workspace_dir.exists());
}

#[test]
fn ensure_workspace_losslessly_merges_coexisting_mailboxes() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-coexisting-mailboxes");
    let stable = ensure_workspace(&paths, workspace).unwrap();
    let legacy = create_legacy_mailbox(&paths, workspace);
    let duplicate_id = Uuid::from_u128(1);
    let stable_only_id = Uuid::from_u128(2);
    let legacy_only_id = Uuid::from_u128(3);
    let duplicate = test_message(workspace, duplicate_id);
    write_message_file(&stable, &duplicate);
    write_message_file(&legacy, &duplicate);
    write_message_file(&stable, &test_message(workspace, stable_only_id));
    write_message_file(&legacy, &test_message(workspace, legacy_only_id));

    let consumer_id = Uuid::from_u128(100);
    atomic_write_json(
        &stable.cursors_dir.join(format!("{consumer_id}.json")),
        &Cursor {
            acked_through: None,
            exceptions: BTreeSet::from([stable_only_id]),
        },
    )
    .unwrap();
    atomic_write_json(
        &legacy.cursors_dir.join(format!("{consumer_id}.json")),
        &Cursor {
            acked_through: None,
            exceptions: BTreeSet::from([legacy_only_id]),
        },
    )
    .unwrap();
    fs::write(cursor_lock_path(&legacy.cursors_dir, consumer_id), b"").unwrap();

    let merged = ensure_workspace(&paths, workspace).unwrap();
    assert_eq!(merged.workspace_dir, stable.workspace_dir);
    let messages = list_messages(&paths, workspace).unwrap();
    assert_eq!(
        messages
            .iter()
            .map(|message| message.id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([duplicate_id, stable_only_id, legacy_only_id])
    );
    let cursor = read_cursor(&paths, workspace, consumer_id).unwrap();
    assert_eq!(
        cursor.exceptions,
        BTreeSet::from([stable_only_id, legacy_only_id])
    );
    assert!(json_files(&legacy.msgs_dir).unwrap().is_empty());
    assert!(json_files(&legacy.cursors_dir).unwrap().is_empty());
    assert!(!cursor_lock_path(&legacy.cursors_dir, consumer_id).exists());

    // The intentionally-retained compatibility skeleton makes future
    // calls idempotent and lets current clients drain a later old-client
    // append instead of ignoring it.
    ensure_workspace(&paths, workspace).unwrap();
    write_message_file(&legacy, &test_message(workspace, Uuid::from_u128(4)));
    ensure_workspace(&paths, workspace).unwrap();
    assert_eq!(list_messages(&paths, workspace).unwrap().len(), 4);
}

#[test]
fn legacy_merge_rejects_special_file_cursor_collisions_without_blocking() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-mailbox-special-cursor-collision");
    let stable = ensure_workspace(&paths, workspace).unwrap();
    let legacy = create_legacy_mailbox(&paths, workspace);
    let consumer_id = Uuid::from_u128(101);
    let cursor_name = format!("{consumer_id}.json");
    atomic_write_json(&legacy.cursors_dir.join(&cursor_name), &Cursor::default()).unwrap();

    let fifo_path = root.path().join("cursor-fifo");
    let fifo = std::ffi::CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    symlink(&fifo_path, stable.cursors_dir.join(&cursor_name)).unwrap();

    let error = ensure_workspace(&paths, workspace)
        .expect_err("legacy merge must reject a special-file cursor collision");
    assert!(
        format!("{error:#}").contains("open mailbox cursor"),
        "unexpected error: {error:#}"
    );
    assert!(legacy.cursors_dir.join(cursor_name).exists());
}

#[test]
fn legacy_merge_validates_unique_sources_before_moving_any_file() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-mailbox-invalid-unique-legacy-source");
    let stable = ensure_workspace(&paths, workspace).unwrap();
    let legacy = create_legacy_mailbox(&paths, workspace);
    let valid_id = Uuid::from_u128(102);
    let oversized_id = Uuid::from_u128(103);
    let valid_path = legacy.cursors_dir.join(format!("{valid_id}.json"));
    let oversized_path = legacy.cursors_dir.join(format!("{oversized_id}.json"));
    atomic_write_json(&valid_path, &Cursor::default()).unwrap();
    let oversized = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&oversized_path)
        .unwrap();
    oversized
        .set_len(MAX_MAILBOX_STATE_BYTES as u64 + 1)
        .unwrap();
    drop(oversized);

    let error = ensure_workspace(&paths, workspace)
        .expect_err("unique oversized legacy cursor must fail preflight");
    assert!(
        format!("{error:#}").contains("exceeds the"),
        "unexpected error: {error:#}"
    );
    assert!(
        valid_path.exists(),
        "preflight must not move an earlier file"
    );
    assert!(
        oversized_path.exists(),
        "invalid source must remain recoverable"
    );
    assert!(!stable.cursors_dir.join(format!("{valid_id}.json")).exists());
    assert!(!stable
        .cursors_dir
        .join(format!("{oversized_id}.json"))
        .exists());
}

#[test]
fn ensure_workspace_rejects_divergent_message_collision_without_mutation() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-mailbox-collision");
    let stable = ensure_workspace(&paths, workspace).unwrap();
    let legacy = create_legacy_mailbox(&paths, workspace);
    let id = Uuid::from_u128(1);
    let legacy_unique_id = Uuid::from_u128(2);
    let stable_message = test_message(workspace, id);
    let mut legacy_message = stable_message.clone();
    legacy_message.body = "different".into();
    write_message_file(&stable, &stable_message);
    write_message_file(&legacy, &legacy_message);
    write_message_file(&legacy, &test_message(workspace, legacy_unique_id));

    let error = ensure_workspace(&paths, workspace).unwrap_err();
    assert!(error.to_string().contains("mailbox message collision"));
    assert_eq!(
        fs::read(stable.msgs_dir.join(format!("{id}.json"))).unwrap(),
        serialized_envelope(&stable_message).unwrap()
    );
    assert_eq!(
        fs::read(legacy.msgs_dir.join(format!("{id}.json"))).unwrap(),
        serialized_envelope(&legacy_message).unwrap()
    );
    assert!(legacy
        .msgs_dir
        .join(format!("{legacy_unique_id}.json"))
        .exists());
    assert!(!stable
        .msgs_dir
        .join(format!("{legacy_unique_id}.json"))
        .exists());
}
