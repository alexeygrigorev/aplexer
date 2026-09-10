use super::*;

#[test]
fn gc_discards_exceptions_for_messages_no_longer_retained() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-cursor-gc-workspace");
    let consumer_id = Uuid::new_v4();
    let first = Uuid::now_v7();
    std::thread::sleep(std::time::Duration::from_millis(2));
    let second = Uuid::now_v7();
    write_test_message(&paths, workspace, first);
    write_test_message(&paths, workspace, second);
    ack_messages(&paths, workspace, consumer_id, &[second]).unwrap();
    let before = read_cursor(&paths, workspace, consumer_id).unwrap();
    assert_eq!(before.exceptions, BTreeSet::from([second]));

    fs::remove_file(
        message_paths(&paths, workspace)
            .msgs_dir
            .join(format!("{second}.json")),
    )
    .unwrap();
    gc_workspace(&paths, workspace).unwrap();

    let after = read_cursor(&paths, workspace, consumer_id).unwrap();
    assert!(after.exceptions.is_empty());
    assert!(!after.is_acked(first));
}

#[test]
fn cursor_gc_respects_retention_active_sessions_and_held_locks() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-stale-cursor-workspace");
    let mp = ensure_workspace(&paths, workspace).unwrap();
    let stale = Uuid::from_u128(10);
    let active = Uuid::from_u128(11);
    let fresh = Uuid::from_u128(12);
    let busy = Uuid::from_u128(13);
    let orphan_stale = Uuid::from_u128(14);
    let orphan_fresh = Uuid::from_u128(15);
    for consumer_id in [stale, active, fresh, busy] {
        atomic_write_json(
            &mp.cursors_dir.join(format!("{consumer_id}.json")),
            &Cursor::default(),
        )
        .unwrap();
    }
    for consumer_id in [stale, active, fresh, busy, orphan_stale, orphan_fresh] {
        fs::write(cursor_lock_path(&mp.cursors_dir, consumer_id), b"").unwrap();
    }

    let now = STALE_CURSOR_RETENTION_SECS + 10_000;
    let old = 1;
    let fresh_at = now - STALE_CURSOR_RETENTION_SECS;
    for consumer_id in [stale, active, busy] {
        set_modified_secs(&mp.cursors_dir.join(format!("{consumer_id}.json")), old);
        set_modified_secs(&cursor_lock_path(&mp.cursors_dir, consumer_id), old);
    }
    set_modified_secs(&mp.cursors_dir.join(format!("{fresh}.json")), fresh_at);
    set_modified_secs(&cursor_lock_path(&mp.cursors_dir, fresh), fresh_at);
    set_modified_secs(&cursor_lock_path(&mp.cursors_dir, orphan_stale), old);
    set_modified_secs(&cursor_lock_path(&mp.cursors_dir, orphan_fresh), fresh_at);

    let busy_lock = FileLock::exclusive(&cursor_lock_path(&mp.cursors_dir, busy), false).unwrap();
    let _mailbox = FileLock::exclusive(&mailbox_lock_path(&mp), false).unwrap();
    maintain_workspace_cursors_locked(
        &mp,
        &BTreeSet::from([active]),
        now,
        STALE_CURSOR_RETENTION_SECS,
    )
    .unwrap();

    assert!(!mp.cursors_dir.join(format!("{stale}.json")).exists());
    assert!(!cursor_lock_path(&mp.cursors_dir, stale).exists());
    assert!(mp.cursors_dir.join(format!("{active}.json")).exists());
    assert!(mp.cursors_dir.join(format!("{fresh}.json")).exists());
    assert!(mp.cursors_dir.join(format!("{busy}.json")).exists());
    assert!(!cursor_lock_path(&mp.cursors_dir, orphan_stale).exists());
    assert!(cursor_lock_path(&mp.cursors_dir, orphan_fresh).exists());

    drop(busy_lock);
    maintain_workspace_cursors_locked(
        &mp,
        &BTreeSet::from([active]),
        now,
        STALE_CURSOR_RETENTION_SECS,
    )
    .unwrap();
    assert!(!mp.cursors_dir.join(format!("{busy}.json")).exists());
    assert!(!cursor_lock_path(&mp.cursors_dir, busy).exists());
}

#[test]
fn opportunistic_gc_waits_out_a_future_marker_mtime() {
    let root = TempDir::new().unwrap();
    let paths = test_paths(root.path());
    let workspace = Path::new("/tmp/aplexer-gc-marker-workspace");
    let mp = ensure_workspace(&paths, workspace).unwrap();
    let mut expired = test_message(workspace, Uuid::from_u128(1));
    expired.created_at = now_secs() - DEFAULT_TTL_SECS - 60;
    write_message_file(&mp, &expired);
    let marker = mp.workspace_dir.join(".gc_marker");
    fs::write(&marker, b"").unwrap();
    set_modified_secs(&marker, now_secs() + 3600);

    maybe_gc(&paths, workspace).unwrap();
    assert_eq!(
        list_messages(&paths, workspace).unwrap().len(),
        1,
        "a recent (if future-dated) sweep must not be repeated on every call"
    );

    set_modified_secs(&marker, now_secs() - OPPORTUNISTIC_GC_INTERVAL_SECS - 1);
    maybe_gc(&paths, workspace).unwrap();
    assert!(list_messages(&paths, workspace).unwrap().is_empty());
}
