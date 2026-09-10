use super::*;

#[test]
fn paginate_last_after_before() {
    let mk = |seq: u64, kind: &'static str| UnifiedEvent {
        kind,
        sequence: seq,
        engine: "claude".into(),
        timestamp: String::new(),
        raw: json!({}),
        ..Default::default()
    };
    let events = vec![
        mk(0, "message"),
        mk(1, "tool_call"),
        mk(2, "message"),
        mk(3, "message"),
        mk(4, "usage"),
    ];
    let last = paginate(
        events.clone(),
        &TranscriptQuery {
            last: Some(2),
            ..Default::default()
        },
    );
    assert_eq!(
        last.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![3, 4]
    );

    let after = paginate(
        events.clone(),
        &TranscriptQuery {
            after: Some(1),
            kind: Some("message".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        after.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![2, 3]
    );

    let older = paginate(
        events,
        &TranscriptQuery {
            before: Some(4),
            last: Some(2),
            kind: Some("message".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        older.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![2, 3]
    );
}

#[test]
fn read_transcript_events_claude_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sess.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"one"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"two"}]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"three"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    let events = read_transcript_events("claude", &path).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].role.as_deref(), Some("user"));
    assert_eq!(events[0].content, "one");
    assert_eq!(events[2].content, "three");
    assert_eq!(events[2].sequence, 2);
}

#[test]
fn max_line_bytes_emits_truncation_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sess.jsonl");
    let huge = format!(
        r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
        "x".repeat(200)
    );
    std::fs::write(&path, format!("{huge}\n")).unwrap();
    let record = dummy_record("claude");
    let mut reader = NativeLogReader::open("claude", &path, Some(50)).unwrap();
    let events = reader.read_available(&record, true).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "error");
    assert!(events[0]
        .error
        .as_deref()
        .unwrap()
        .starts_with(LINE_TRUNCATION_SENTINEL));
}

#[test]
fn follow_reader_picks_up_appended_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sess.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            "\n",
        ),
    )
    .unwrap();
    let record = dummy_record("claude");
    let mut reader = NativeLogReader::open("claude", &path, None).unwrap();
    let first = reader.read_available(&record, true).unwrap();
    assert_eq!(first.len(), 1);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(
            concat!(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"yo"}]}}"#,
                "\n",
            )
            .as_bytes(),
        )
        .unwrap();
    let second = reader.read_available(&record, false).unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].content, "yo");
    assert_eq!(second[0].sequence, 1);
}

#[test]
fn follow_reader_holds_a_partial_line_until_terminated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sess.jsonl");
    let (head, tail) = (
        r#"{"type":"user","message":{"role":"user","content":"hi "#,
        "there\"}}\n",
    );
    std::fs::write(&path, head).unwrap();
    let record = dummy_record("claude");
    let mut reader = NativeLogReader::open("claude", &path, None).unwrap();
    assert!(reader.read_available(&record, false).unwrap().is_empty());
    assert!(reader.read_available(&record, false).unwrap().is_empty());
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(tail.as_bytes())
        .unwrap();
    let events = reader.read_available(&record, false).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].content, "hi there");
    assert_eq!(events[0].sequence, 0);
}

#[test]
fn reader_offset_counts_file_bytes_not_lossy_chars() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sess.jsonl");
    let mut first = br#"{"type":"user","message":{"role":"user","content":"a"#.to_vec();
    first.extend_from_slice(b"\xff\xfe");
    first.extend_from_slice(b"b\"}}\n");
    std::fs::write(&path, &first).unwrap();
    let record = dummy_record("claude");
    let mut reader = NativeLogReader::open("claude", &path, None).unwrap();
    let events = reader.read_available(&record, true).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].content, "a\u{fffd}\u{fffd}b");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(
            concat!(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"yo"}]}}"#,
                "\n",
            )
            .as_bytes(),
        )
        .unwrap();
    let events = reader.read_available(&record, false).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].content, "yo");
    assert_eq!(events[0].sequence, 1);
}

#[test]
fn followed_snapshot_leaves_a_partial_row_for_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sess.jsonl");
    let (head, tail) = (
        r#"{"type":"user","message":{"role":"user","content":"hi "#,
        "there\"}}\n",
    );
    std::fs::write(&path, head).unwrap();
    let record = dummy_record("claude");
    let mut reader = NativeLogReader::open("claude", &path, None).unwrap();
    let query = TranscriptQuery {
        follow: true,
        ..Default::default()
    };
    assert!(snapshot_page(&mut reader, &record, &query)
        .unwrap()
        .is_empty());
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(tail.as_bytes())
        .unwrap();
    let events = reader.read_available(&record, false).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].content, "hi there");

    // A one-shot page still takes the unterminated tail as it stands.
    std::fs::write(&path, head).unwrap();
    let mut reader = NativeLogReader::open("claude", &path, None).unwrap();
    let once = snapshot_page(&mut reader, &record, &TranscriptQuery::default()).unwrap();
    assert!(once.is_empty(), "an unterminated string is not a row");
    std::fs::write(&path, format!("{head}{tail}")).unwrap();
    let mut reader = NativeLogReader::open("claude", &path, None).unwrap();
    let once = snapshot_page(&mut reader, &record, &TranscriptQuery::default()).unwrap();
    assert_eq!(once.len(), 1);
}

#[test]
fn reader_reports_a_discarded_corrupt_row_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sess.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"user","message":{"role":"user","content":"torn"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    let events = read_transcript_events("claude", &path).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, "error");
    assert!(events[0]
        .error
        .as_deref()
        .unwrap()
        .starts_with("discarded "));
    assert_eq!(events[1].content, "ok");
    assert_eq!(events[1].sequence, 1);
}
