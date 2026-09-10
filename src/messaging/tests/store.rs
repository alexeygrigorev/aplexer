use super::*;

#[test]
fn serialized_envelope_cap_rejects_oversized_structured_data() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-envelope-cap-workspace");
    let mut message = test_message(workspace, Uuid::from_u128(1));
    message.body = "tiny".into();
    message.data = Some(serde_json::json!({
        "blob": "x".repeat(MAX_ENVELOPE_BYTES)
    }));

    let error = write_message(&paths, &message).unwrap_err();
    assert!(error.to_string().contains("serialized message envelope"));
    assert!(!message_paths(&paths, workspace)
        .msgs_dir
        .join(format!("{}.json", message.id))
        .exists());
}

#[test]
fn message_loader_validates_schema_filename_id_and_workspace() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-message-invariants-workspace");
    let mp = ensure_workspace(&paths, workspace).unwrap();
    let filename_id = Uuid::from_u128(1);
    let path = mp.msgs_dir.join(format!("{filename_id}.json"));

    let mut message = test_message(workspace, filename_id);
    message.schema_version = MESSAGE_SCHEMA_VERSION + 1;
    atomic_write_bytes(&path, &serialized_envelope(&message).unwrap()).unwrap();
    let schema_error = read_message(&paths, workspace, filename_id)
        .expect_err("an unsupported message schema must fail closed");
    assert!(
        format!("{schema_error:#}").contains("unsupported mailbox message schema"),
        "unexpected error: {schema_error:#}"
    );

    message.schema_version = MESSAGE_SCHEMA_VERSION;
    message.id = Uuid::from_u128(2);
    atomic_write_bytes(&path, &serialized_envelope(&message).unwrap()).unwrap();
    let id_error = read_message(&paths, workspace, filename_id)
        .expect_err("the envelope id must match its filename");
    assert!(
        format!("{id_error:#}").contains("does not match filename id"),
        "unexpected error: {id_error:#}"
    );

    message.id = filename_id;
    message.workspace = PathBuf::from("/tmp/a-different-mailbox-workspace");
    atomic_write_bytes(&path, &serialized_envelope(&message).unwrap()).unwrap();
    let workspace_error = list_messages(&paths, workspace)
        .expect_err("an envelope from another workspace must fail closed");
    assert!(
        format!("{workspace_error:#}").contains("belongs to workspace"),
        "unexpected error: {workspace_error:#}"
    );
}

#[test]
fn message_loader_rejects_symlink_non_regular_and_oversized_entries() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-message-file-type-workspace");
    let mp = ensure_workspace(&paths, workspace).unwrap();
    let id = Uuid::from_u128(1);
    let path = mp.msgs_dir.join(format!("{id}.json"));
    let outside = root.path().join("outside-message.json");
    fs::write(
        &outside,
        serialized_envelope(&test_message(workspace, id)).unwrap(),
    )
    .unwrap();
    symlink(&outside, &path).unwrap();

    let symlink_error =
        list_messages(&paths, workspace).expect_err("a mailbox symlink must not be followed");
    assert!(
        format!("{symlink_error:#}").contains("open mailbox message"),
        "unexpected error: {symlink_error:#}"
    );
    let cursor_error = read_cursor(&paths, workspace, Uuid::from_u128(100))
        .expect_err("cursor maintenance must not retain a symlink by filename alone");
    assert!(
        format!("{cursor_error:#}").contains("open mailbox message"),
        "unexpected error: {cursor_error:#}"
    );
    fs::remove_file(&path).unwrap();

    fs::create_dir(&path).unwrap();
    let type_error =
        list_messages(&paths, workspace).expect_err("a non-regular mailbox entry must be rejected");
    assert!(
        format!("{type_error:#}").contains("is not a regular file"),
        "unexpected error: {type_error:#}"
    );
    fs::remove_dir(&path).unwrap();

    let oversized = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    oversized.set_len(MAX_ENVELOPE_BYTES as u64 + 1).unwrap();
    drop(oversized);
    let size_error = list_messages(&paths, workspace)
        .expect_err("an oversized pre-existing envelope must be rejected before reading");
    assert!(
        format!("{size_error:#}").contains("envelope cap"),
        "unexpected error: {size_error:#}"
    );
}

#[test]
fn append_enforces_message_count_and_keeps_the_new_message() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-count-cap-workspace");
    let first = test_message(workspace, Uuid::from_u128(1));
    let second = test_message(workspace, Uuid::from_u128(2));
    let delayed_lower = test_message(workspace, Uuid::from_u128(0));

    write_message_limited(
        &ensure_workspace(&paths, workspace).unwrap(),
        &first,
        2,
        MAX_WORKSPACE_BYTES,
    )
    .unwrap();
    write_message_limited(
        &ensure_workspace(&paths, workspace).unwrap(),
        &second,
        2,
        MAX_WORKSPACE_BYTES,
    )
    .unwrap();
    write_message_limited(
        &ensure_workspace(&paths, workspace).unwrap(),
        &delayed_lower,
        2,
        MAX_WORKSPACE_BYTES,
    )
    .unwrap();

    let retained = list_messages(&paths, workspace).unwrap();
    assert_eq!(retained.len(), 2);
    assert!(retained
        .iter()
        .any(|message| message.id == delayed_lower.id));
    assert!(!retained.iter().any(|message| message.id == first.id));
}

#[test]
fn append_enforces_workspace_byte_cap() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-byte-cap-workspace");
    let first = test_message(workspace, Uuid::from_u128(1));
    let mut second = test_message(workspace, Uuid::from_u128(2));
    second.body = "second".into();
    let second_size = serialized_envelope(&second).unwrap().len() as u64;

    write_message_limited(
        &ensure_workspace(&paths, workspace).unwrap(),
        &first,
        10,
        second_size,
    )
    .unwrap();
    write_message_limited(
        &ensure_workspace(&paths, workspace).unwrap(),
        &second,
        10,
        second_size,
    )
    .unwrap();

    let retained = list_messages(&paths, workspace).unwrap();
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].id, second.id);
}

#[test]
fn append_rolls_back_when_quota_cannot_retain_the_new_message() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-impossible-cap-workspace");
    let message = test_message(workspace, Uuid::from_u128(1));

    let error = write_message_limited(
        &ensure_workspace(&paths, workspace).unwrap(),
        &message,
        0,
        MAX_WORKSPACE_BYTES,
    )
    .expect_err("a zero-message quota must reject the append");

    assert!(error.to_string().contains("enforce mailbox quota"));
    assert!(list_messages(&paths, workspace).unwrap().is_empty());
}

#[test]
fn append_waits_for_the_workspace_mailbox_lock() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = PathBuf::from("/tmp/aplexer-append-lock-workspace");
    let mp = ensure_workspace(&paths, &workspace).unwrap();
    let lock = FileLock::exclusive(&mailbox_lock_path(&mp), false).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread_paths = paths.clone();
    let thread_workspace = workspace.clone();
    let worker = std::thread::spawn(move || {
        tx.send(write_message(
            &thread_paths,
            &test_message(&thread_workspace, Uuid::from_u128(1)),
        ))
        .unwrap();
    });

    assert!(rx
        .recv_timeout(std::time::Duration::from_millis(50))
        .is_err());
    drop(lock);
    rx.recv_timeout(std::time::Duration::from_secs(2))
        .unwrap()
        .unwrap();
    worker.join().unwrap();
}

#[test]
fn message_loader_rejects_a_false_broadcast() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-false-broadcast-workspace");
    let mp = ensure_workspace(&paths, workspace).unwrap();
    let mut message = test_message(workspace, Uuid::from_u128(1));
    message.to = Recipient::Broadcast { broadcast: false };
    write_message_file(&mp, &message);
    let error = list_messages(&paths, workspace)
        .expect_err("a recipient that matches nobody must not load");
    assert!(
        format!("{error:#}").contains("which is nobody"),
        "unexpected error: {error:#}"
    );
}
