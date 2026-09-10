use super::*;

#[test]
fn cursor_tracks_acks() {
    let mut cursor = Cursor::default();
    let id1 = Uuid::now_v7();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let id2 = Uuid::now_v7();
    assert!(!cursor.is_acked(id1));
    cursor.exceptions.insert(id2);
    assert!(cursor.is_acked(id2));
    assert!(!cursor.is_acked(id1));
    cursor.acked_through = Some(id2);
    assert!(cursor.is_acked(id1));
    assert!(cursor.is_acked(id2));
}

#[test]
fn corrupt_cursor_fails_without_reset_or_ack_overwrite() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-corrupt-cursor-workspace");
    let consumer_id = Uuid::from_u128(100);
    let message_id = Uuid::from_u128(1);
    write_test_message(&paths, workspace, message_id);
    let mp = ensure_workspace(&paths, workspace).unwrap();
    let cursor_path = mp.cursors_dir.join(format!("{consumer_id}.json"));
    let corrupt = b"{\"exceptions\":";
    fs::write(&cursor_path, corrupt).unwrap();

    let read_error = read_cursor(&paths, workspace, consumer_id)
        .expect_err("a corrupt cursor must not be interpreted as empty");
    assert!(
        format!("{read_error:#}").contains("parse mailbox cursor"),
        "unexpected error: {read_error:#}"
    );
    assert_eq!(fs::read(&cursor_path).unwrap(), corrupt);

    let ack_error = ack_messages(&paths, workspace, consumer_id, &[message_id])
        .expect_err("ack must not overwrite a corrupt cursor from an empty default");
    assert!(
        format!("{ack_error:#}").contains("parse mailbox cursor"),
        "unexpected error: {ack_error:#}"
    );
    assert_eq!(fs::read(&cursor_path).unwrap(), corrupt);
}

#[test]
fn cursor_compaction_migrates_legacy_watermark_to_exact_ids() {
    let id1 = Uuid::from_u128(1);
    let id2 = Uuid::from_u128(2);
    let id3 = Uuid::from_u128(3);
    let retained = BTreeSet::from([id1, id2, id3]);
    let mut cursor = Cursor {
        acked_through: Some(id2),
        exceptions: BTreeSet::from([id3]),
    };

    compact_cursor(&mut cursor, &retained);
    assert_eq!(cursor.acked_through, None);
    assert_eq!(cursor.exceptions, retained);
}

#[test]
fn exact_acks_do_not_hide_a_lower_id_committed_later() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-delayed-message-workspace");
    let consumer_id = Uuid::from_u128(100);
    let lower = Uuid::from_u128(1);
    let higher = Uuid::from_u128(2);

    write_test_message(&paths, workspace, higher);
    ack_messages(&paths, workspace, consumer_id, &[higher]).unwrap();
    let before = read_cursor(&paths, workspace, consumer_id).unwrap();
    assert_eq!(before.acked_through, None);
    assert!(before.is_acked(higher));

    // This is the problematic interleaving: an id generated earlier is
    // committed only after the later id was acknowledged.
    write_test_message(&paths, workspace, lower);
    let after = read_cursor(&paths, workspace, consumer_id).unwrap();
    assert!(after.is_acked(higher));
    assert!(!after.is_acked(lower));
}

#[test]
fn ack_waits_for_the_per_consumer_lock() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = PathBuf::from("/tmp/aplexer-ack-lock-workspace");
    let consumer_id = Uuid::new_v4();
    let message_id = Uuid::now_v7();
    write_test_message(&paths, &workspace, message_id);
    let mp = ensure_workspace(&paths, &workspace).unwrap();
    let lock = FileLock::exclusive(&cursor_lock_path(&mp.cursors_dir, consumer_id), false).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread_paths = paths.clone();
    let thread_workspace = workspace.clone();
    let worker = std::thread::spawn(move || {
        tx.send(ack_messages(
            &thread_paths,
            &thread_workspace,
            consumer_id,
            &[message_id],
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
    assert!(read_cursor(&paths, &workspace, consumer_id)
        .unwrap()
        .is_acked(message_id));
}

#[test]
fn ack_returns_only_the_ids_the_mailbox_holds() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-ack-report-workspace");
    let consumer_id = Uuid::from_u128(100);
    let known = Uuid::from_u128(1);
    let unknown = Uuid::from_u128(2);
    write_test_message(&paths, workspace, known);

    let acked = ack_messages(&paths, workspace, consumer_id, &[unknown, known, known]).unwrap();
    assert_eq!(acked, vec![known]);
    let cursor = read_cursor(&paths, workspace, consumer_id).unwrap();
    assert!(cursor.is_acked(known));
    assert!(!cursor.is_acked(unknown));
    // Acknowledging again is idempotent and still reports the id.
    let again = ack_messages(&paths, workspace, consumer_id, &[known]).unwrap();
    assert_eq!(again, vec![known]);
}
