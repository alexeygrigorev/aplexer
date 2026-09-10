//! Unit tests for registry IO.

use super::*;

fn registry_record(paths: &Paths, id: Uuid) -> SessionRecord {
    let mut record = SessionRecord::fixture(paths.state_root.clone(), "registry-test");
    record.id = id;
    record.history_bytes = DEFAULT_HISTORY_BYTES;
    record.phase = Phase::Exited;
    record.containment_empty = Some(true);
    record.socket_path = paths.socket(id);
    record.history_path = paths.history(id);
    record
}

#[test]
fn registry_enumeration_reports_corrupt_records() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: root.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let id = Uuid::new_v4();
    fs::create_dir(paths.state_session(id)).unwrap();
    fs::write(paths.record(id), b"{truncated").unwrap();

    let error = list_records(&paths).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains(&id.to_string()), "{message}");
    assert!(message.contains("parse"), "{message}");
}

/// The window `start_session` opens between creating a session directory
/// and writing that session's first record. Any reader that does not hold
/// the registry lock can land in it, and treating it as corruption killed
/// `a watch` outright (see `list_records`). The same fixture must still be
/// reported once the record appears, so the entry is skipped, not
/// blacklisted.
#[test]
fn registry_enumeration_skips_a_session_whose_record_is_not_written_yet() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: root.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let pending = Uuid::new_v4();
    fs::create_dir(paths.state_session(pending)).unwrap();
    let written = Uuid::new_v4();
    fs::create_dir(paths.state_session(written)).unwrap();
    atomic_write_json(&paths.record(written), &registry_record(&paths, written)).unwrap();

    let records = list_records(&paths).unwrap();
    assert_eq!(
        records.iter().map(|record| record.id).collect::<Vec<_>>(),
        vec![written],
        "a session mid-creation must be skipped, not reported and not fatal"
    );

    // ... and picked up as soon as its record lands.
    atomic_write_json(&paths.record(pending), &registry_record(&paths, pending)).unwrap();
    let mut ids = list_records(&paths)
        .unwrap()
        .iter()
        .map(|record| record.id)
        .collect::<Vec<_>>();
    ids.sort();
    let mut expected = vec![pending, written];
    expected.sort();
    assert_eq!(ids, expected);
}

/// The complement of the test above: skipping a missing record must not
/// weaken the fail-closed contract for a record that is present and wrong.
#[test]
fn registry_enumeration_still_fails_closed_on_an_empty_record_file() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: root.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let id = Uuid::new_v4();
    fs::create_dir(paths.state_session(id)).unwrap();
    fs::write(paths.record(id), b"").unwrap();

    let error = list_records(&paths).unwrap_err();
    assert!(format!("{error:#}").contains("parse"), "{error:#}");
}

#[test]
fn registry_enumeration_reports_unsupported_schema() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: root.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let id = Uuid::new_v4();
    fs::create_dir(paths.state_session(id)).unwrap();
    let mut record = registry_record(&paths, id);
    record.schema_version = SCHEMA_VERSION + 1;
    atomic_write_json(&paths.record(id), &record).unwrap();

    let error = list_records(&paths).unwrap_err();
    assert!(format!("{error:#}").contains("unsupported session schema"));
}

#[test]
fn registry_enumeration_validates_directory_id_and_paths() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: root.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let id = Uuid::new_v4();
    fs::create_dir(paths.state_session(id)).unwrap();

    let mut record = registry_record(&paths, id);
    record.id = Uuid::new_v4();
    atomic_write_json(&paths.record(id), &record).unwrap();
    assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("directory id"));

    record = registry_record(&paths, id);
    record.socket_path = paths.socket(Uuid::new_v4());
    atomic_write_json(&paths.record(id), &record).unwrap();
    assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("socket path"));

    record = registry_record(&paths, id);
    record.history_path = paths.history(Uuid::new_v4());
    atomic_write_json(&paths.record(id), &record).unwrap();
    assert!(format!("{:#}", list_records(&paths).unwrap_err()).contains("history path"));
}

#[test]
fn registry_enumeration_grandfathers_legacy_history_capacity() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: root.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let id = Uuid::new_v4();
    fs::create_dir(paths.state_session(id)).unwrap();

    let mut record = registry_record(&paths, id);
    record.history_bytes = MAX_HISTORY_BYTES + 1;
    atomic_write_json(&paths.record(id), &record).unwrap();

    let records = list_records(&paths).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].history_bytes, MAX_HISTORY_BYTES + 1);
}

#[test]
fn registry_enumeration_rejects_unexpected_entries() {
    let root = tempfile::tempdir().unwrap();
    let paths = Paths {
        runtime_root: root.path().join("runtime"),
        state_root: root.path().join("state"),
        config_file: root.path().join("config.toml"),
    };
    paths.ensure().unwrap();
    let unexpected = paths.state_root.join("sessions").join("leftover");
    fs::write(&unexpected, b"not a session directory").unwrap();

    let error = list_records(&paths).unwrap_err();
    assert!(format!("{error:#}").contains("is not a directory"));
}
