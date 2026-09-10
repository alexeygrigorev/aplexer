use super::*;

#[test]
fn codex_native_message_response_item() {
    let payload: Value = serde_json::from_str(
        r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}"#,
    )
    .unwrap();
    let events = codex_native_events(&payload);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "message");
    assert_eq!(events[0].role.as_deref(), Some("assistant"));
    assert_eq!(events[0].content, "hello");
}

#[test]
fn codex_native_tool_call_and_result() {
    let call: Value = serde_json::from_str(
        r#"{"type":"response_item","payload":{"type":"custom_tool_call","name":"exec","input":"ls"}}"#,
    )
    .unwrap();
    let events = codex_native_events(&call);
    assert_eq!(events[0].kind, "tool_call");
    assert_eq!(events[0].tool_name.as_deref(), Some("exec"));

    let output: Value = serde_json::from_str(
        r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":[{"type":"input_text","text":"ok"}]}}"#,
    )
    .unwrap();
    let events = codex_native_events(&output);
    assert_eq!(events[0].kind, "tool_result");
    assert_eq!(events[0].tool_output.as_deref(), Some("ok"));
}

#[test]
fn codex_native_continuation_from_session_meta() {
    let payload: Value = serde_json::from_str(
        r#"{"type":"session_meta","payload":{"id":"thread-abc","cwd":"/tmp/x"}}"#,
    )
    .unwrap();
    assert_eq!(
        codex_native_continuation(&payload).as_deref(),
        Some("thread-abc")
    );
    assert_eq!(codex_native_cwd(&payload).as_deref(), Some("/tmp/x"));
}

#[test]
fn zcodex_rides_the_codex_machinery() {
    // Family identification: the variant parses with codex's wire format...
    assert!(matches!(
        wire_format_for("zcodex").unwrap(),
        WireFormat::CodexNative
    ));
    // ...while events emitted from its rollout carry the variant's own
    // engine id, not the family's.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout.jsonl");
    std::fs::write(
        &path,
        concat!(
            r#"{"timestamp":"2026-09-06T12:00:00.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}"#,
            "\n",
        ),
    )
    .unwrap();
    let events = read_transcript_events("zcodex", &path).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].engine, "zcodex");
    assert_eq!(events[0].role.as_deref(), Some("assistant"));
    assert_eq!(events[0].content, "hello");

    // Location rides the codex heuristic too: the CODEX_HOME sessions
    // tree, disambiguated by the rollout's own session_meta cwd.
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join("sessions/2026/09/06");
    std::fs::create_dir_all(&sessions).unwrap();
    let rollout = sessions.join("thread-z.jsonl");
    std::fs::write(
        &rollout,
        concat!(
            r#"{"type":"session_meta","payload":{"id":"thread-z","cwd":"/tmp/zcodex-work"}}"#,
            "\n",
        ),
    )
    .unwrap();
    let mut env = BTreeMap::new();
    env.insert("CODEX_HOME".to_string(), home.path().display().to_string());
    let found = locate_transcript("zcodex", Path::new("/tmp/zcodex-work"), now_ms(), &env).unwrap();
    assert_eq!(found, rollout);
}
