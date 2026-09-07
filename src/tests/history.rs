//! Unit tests for durable history persistence.

use super::*;

#[test]
fn bounded_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut h = History::open(dir.path().join("h"), 4).unwrap();
    h.append(b"abcdef").unwrap();
    assert_eq!(h.snapshot(None), b"cdef");
}

#[test]
fn history_incremental_flush_writes_only_delta_and_recovers_exact_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.bin");
    let mut history = History::open(path.clone(), 8).unwrap();

    history.append(b"abcdef").unwrap();
    history.flush().unwrap();
    assert_eq!(history.data_bytes_written, 6);
    history.append(b"\0g").unwrap();
    history.flush().unwrap();
    assert_eq!(history.data_bytes_written, 8);
    history.append(b"hi").unwrap();
    history.flush().unwrap();
    assert_eq!(history.data_bytes_written, 10);
    assert_eq!(history.snapshot(None), b"cdef\0ghi");

    let reopened = History::open(path.clone(), 8).unwrap();
    assert_eq!(reopened.snapshot(None), b"cdef\0ghi");
    assert_eq!(
        read_persisted_history_tail(&path, None).unwrap(),
        b"cdef\0ghi"
    );
}

#[test]
fn history_uncommitted_suffix_is_ignored_and_truncated_on_retry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.bin");
    let mut history = History::open(path.clone(), 16).unwrap();
    history.append(b"safe").unwrap();
    history.flush().unwrap();

    let data_path = history_data_path(&path, 0);
    OpenOptions::new()
        .append(true)
        .open(&data_path)
        .unwrap()
        .write_all(b"torn")
        .unwrap();
    let mut reopened = History::open(path.clone(), 16).unwrap();
    assert_eq!(reopened.snapshot(None), b"safe");
    reopened.append(b"-next").unwrap();
    reopened.flush().unwrap();
    assert_eq!(
        History::open(path, 16).unwrap().snapshot(None),
        b"safe-next"
    );
}

#[test]
fn history_corrupt_newest_commit_recovers_previous_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.bin");
    let mut history = History::open(path.clone(), 16).unwrap();
    history.append(b"prior").unwrap();
    history.flush().unwrap();
    history.append(b"-newest").unwrap();
    history.flush().unwrap();

    fs::write(history_commit_path(&path, 0), b"{torn").unwrap();
    let reopened = History::open(path.clone(), 16).unwrap();
    assert_eq!(reopened.snapshot(None), b"prior");
    assert_eq!(read_persisted_history_tail(&path, None).unwrap(), b"prior");
}

#[test]
fn history_corrupt_v2_pair_never_falls_back_to_stale_raw_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.bin");
    let mut history = History::open(path.clone(), 16).unwrap();
    history.append(b"prior").unwrap();
    history.flush().unwrap();
    history.append(b"-newest").unwrap();
    history.flush().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"prior-newest");

    fs::write(history_commit_path(&path, 0), b"{torn-newest").unwrap();
    fs::write(history_commit_path(&path, 1), b"{torn-prior").unwrap();
    let read_error = read_persisted_history_tail(&path, None).unwrap_err();
    assert!(
        format!("{read_error:#}").contains("no valid committed history generation"),
        "{read_error:#}"
    );
    assert!(History::open(path.clone(), 16).is_err());
    assert_eq!(
        fs::read(path).unwrap(),
        b"prior-newest",
        "fail-closed v2 recovery mutated the raw compatibility evidence"
    );
}

#[test]
fn history_marker_prevents_raw_fallback_when_all_commits_disappear() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.bin");
    let mut history = History::open(path.clone(), 16).unwrap();
    history.append(b"v2-authoritative").unwrap();
    history.flush().unwrap();
    assert!(history_marker_path(&path).is_file());
    assert!(history_data_path(&path, 0).is_file());

    fs::write(&path, b"stale-raw").unwrap();
    for slot in 0..HISTORY_COMMIT_COUNT {
        let commit = history_commit_path(&path, slot);
        if commit.exists() {
            fs::remove_file(commit).unwrap();
        }
    }

    let read_error = read_persisted_history_tail(&path, None).unwrap_err();
    assert!(
        format!("{read_error:#}").contains("no valid committed history generation"),
        "{read_error:#}"
    );
    assert!(History::open(path.clone(), 16).is_err());
    assert_eq!(fs::read(path).unwrap(), b"stale-raw");
}

#[test]
fn history_unpublished_first_bank_without_marker_still_recovers_legacy_raw() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.bin");
    let mut history = History::open(path.clone(), 16).unwrap();
    history.append(b"raw-precommit").unwrap();
    let blocked_commit = history_commit_path(&path, 1);
    fs::create_dir(&blocked_commit).unwrap();

    assert!(history.flush().is_err());
    assert!(history_data_path(&path, 0).is_file());
    assert!(!history_marker_path(&path).exists());
    assert_eq!(fs::read(&path).unwrap(), b"raw-precommit");
    drop(history);
    fs::remove_dir(blocked_commit).unwrap();

    assert_eq!(
        read_persisted_history_tail(&path, None).unwrap(),
        b"raw-precommit"
    );
    let reopened = History::open(path.clone(), 16).unwrap();
    assert_eq!(reopened.snapshot(None), b"raw-precommit");
    assert!(history_marker_path(&path).is_file());
}

#[test]
fn history_markerless_v2_is_readable_and_next_writable_open_publishes_marker() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.bin");
    let mut history = History::open(path.clone(), 16).unwrap();
    history.append(b"pre-marker-v2").unwrap();
    history.flush().unwrap();
    fs::remove_file(history_marker_path(&path)).unwrap();

    assert_eq!(
        read_persisted_history_tail(&path, None).unwrap(),
        b"pre-marker-v2"
    );
    assert!(!history_marker_path(&path).exists());
    let reopened = History::open(path.clone(), 16).unwrap();
    assert_eq!(reopened.snapshot(None), b"pre-marker-v2");
    assert!(history_marker_path(&path).is_file());
}

#[test]
fn history_marker_is_bounded_checksummed_and_a_safe_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.bin");
    let marker_path = history_marker_path(&path);
    let mut history = History::open(path.clone(), 16).unwrap();
    history.append(b"committed").unwrap();
    history.flush().unwrap();
    let valid_marker = fs::read(&marker_path).unwrap();

    let mut bad_checksum: HistoryMarker = serde_json::from_slice(&valid_marker).unwrap();
    bad_checksum.store_id = Uuid::new_v4();
    fs::write(&marker_path, serde_json::to_vec(&bad_checksum).unwrap()).unwrap();
    let error = read_persisted_history_tail(&path, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("checksum mismatch"),
        "{error:#}"
    );

    let wrong_store = bad_checksum.seal().unwrap();
    fs::write(&marker_path, serde_json::to_vec(&wrong_store).unwrap()).unwrap();
    let error = read_persisted_history_tail(&path, None).unwrap_err();
    assert!(
        format!("{error:#}").contains("no valid committed history generation"),
        "{error:#}"
    );

    fs::write(&marker_path, vec![b'x'; HISTORY_MARKER_MAX_BYTES + 1]).unwrap();
    let error = read_persisted_history_tail(&path, None).unwrap_err();
    assert!(format!("{error:#}").contains("exceeds the"), "{error:#}");

    fs::remove_file(&marker_path).unwrap();
    let target = dir.path().join("marker-target");
    fs::write(&target, b"unrelated").unwrap();
    symlink(&target, &marker_path).unwrap();
    assert!(read_persisted_history_tail(&path, None).is_err());
    fs::remove_file(&marker_path).unwrap();

    let marker_c = CString::new(marker_path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(marker_c.as_ptr(), 0o600) }, 0);
    assert!(read_persisted_history_tail(&path, None).is_err());
    assert!(History::open(path.clone(), 16).is_err());
    assert_eq!(fs::read(target).unwrap(), b"unrelated");
}

#[test]
fn history_compaction_is_bounded_and_amortized_by_new_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.bin");
    let mut history = History::open(path.clone(), 4).unwrap();
    history.append(b"abcd").unwrap();
    history.flush().unwrap();
    history.append(b"efgh").unwrap();
    history.flush().unwrap();
    history.append(b"i").unwrap();
    history.flush().unwrap();

    assert_eq!(history.snapshot(None), b"fghi");
    assert_eq!(history.data_bytes_written, 12);
    assert_eq!(fs::read(&path).unwrap(), b"fghi");
    for slot in 0..HISTORY_BANK_COUNT {
        let data_path = history_data_path(&path, slot);
        if let Ok(metadata) = fs::metadata(data_path) {
            assert!(metadata.len() <= HISTORY_BANK_HEADER_BYTES as u64 + 2 * history.cap as u64);
        }
    }
    assert_eq!(History::open(path, 4).unwrap().snapshot(None), b"fghi");
}

#[test]
fn history_legacy_migration_and_capacity_changes_keep_only_exact_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.bin");
    fs::write(&path, b"0123456789").unwrap();

    let mut migrated = History::open(path.clone(), 4).unwrap();
    assert_eq!(migrated.snapshot(None), b"6789");
    assert_eq!(fs::read(&path).unwrap(), b"6789");
    migrated.append(b"AB").unwrap();
    migrated.flush().unwrap();
    assert_eq!(read_persisted_history_tail(&path, None).unwrap(), b"89AB");
    assert_eq!(fs::read(&path).unwrap(), b"6789AB");
    migrated.flush_final().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"89AB");

    let shrunk = History::open(path.clone(), 3).unwrap();
    assert_eq!(shrunk.snapshot(None), b"9AB");
    let mut grown = History::open(path.clone(), 6).unwrap();
    assert_eq!(grown.snapshot(None), b"9AB");
    grown.append(b"CD").unwrap();
    grown.flush().unwrap();
    assert_eq!(History::open(path, 6).unwrap().snapshot(None), b"9ABCD");
}

#[test]
fn history_special_files_fail_without_becoming_persistence_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    fs::write(&target, b"unrelated").unwrap();
    let legacy = dir.path().join("history.bin");
    symlink(&target, &legacy).unwrap();
    assert!(History::open(legacy.clone(), 8).is_err());
    assert!(read_persisted_history_tail(&legacy, None).is_err());
    fs::remove_file(&legacy).unwrap();

    let commit = history_commit_path(&legacy, 0);
    let commit_c = CString::new(commit.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(commit_c.as_ptr(), 0o600) }, 0);
    assert!(History::open(legacy.clone(), 8).is_err());
    assert!(read_persisted_history_tail(&legacy, None).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"unrelated");
}

#[test]
fn history_capacity_has_one_global_limit_and_zero_stays_disabled() {
    for value in [0, 1, DEFAULT_HISTORY_BYTES, MAX_HISTORY_BYTES] {
        assert_eq!(validate_history_bytes(value).unwrap(), value);
    }
    for value in [MAX_HISTORY_BYTES + 1, usize::MAX] {
        let error = validate_history_bytes(value).unwrap_err().to_string();
        assert!(error.contains("history_bytes"), "{error}");
        assert!(error.contains(&MAX_HISTORY_BYTES.to_string()), "{error}");
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("disabled-history.bin");
    let mut history = History::open(path.clone(), 0).unwrap();
    history.append(b"not retained").unwrap();
    history.flush().unwrap();
    assert!(history.snapshot(None).is_empty());
    assert!(!path.exists());
    assert!(read_persisted_history_tail(&path, None).unwrap().is_empty());
    assert!(History::open(dir.path().join("too-large"), MAX_HISTORY_BYTES + 1).is_err());
}
