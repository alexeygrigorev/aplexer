use super::*;

#[test]
fn grok_user_and_agent_chunks() {
    let user: Value = serde_json::from_str(
        r#"{"params":{"sessionId":"s1","update":{"sessionUpdate":"user_message_chunk","content":{"text":"hi"}}}}"#,
    )
    .unwrap();
    let events = grok_native_events(&user);
    assert_eq!(events[0].kind, "message");
    assert_eq!(events[0].role.as_deref(), Some("user"));
    assert_eq!(events[0].content, "hi");

    let agent: Value = serde_json::from_str(
        r#"{"params":{"update":{"sessionUpdate":"agent_message_chunk","content":"hello back"}}}"#,
    )
    .unwrap();
    let events = grok_native_events(&agent);
    assert_eq!(events[0].role.as_deref(), Some("assistant"));
    assert_eq!(events[0].content, "hello back");
}

#[test]
fn grok_tool_call_and_completed_result() {
    let call: Value = serde_json::from_str(
        r#"{"params":{"update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Read","rawInput":{"path":"a.rs"}}}}"#,
    )
    .unwrap();
    let events = grok_native_events(&call);
    assert_eq!(events[0].kind, "tool_call");
    assert_eq!(events[0].tool_name.as_deref(), Some("Read"));

    let result: Value = serde_json::from_str(
        r#"{"params":{"update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","status":"completed","content":[{"content":{"text":"ok"}}]}}}"#,
    )
    .unwrap();
    let events = grok_native_events(&result);
    assert_eq!(events[0].kind, "tool_result");
    assert_eq!(events[0].tool_output.as_deref(), Some("ok"));
}

#[test]
fn grok_row_timestamp_rejects_negative_values() {
    let seconds: Value = serde_json::from_str(r#"{"timestamp":1700000000}"#).unwrap();
    assert_eq!(grok_row_timestamp(&seconds), iso8601_utc(1_700_000_000_000));
    let millis: Value = serde_json::from_str(r#"{"timestamp":1700000000000}"#).unwrap();
    assert_eq!(grok_row_timestamp(&millis), iso8601_utc(1_700_000_000_000));
    let negative: Value = serde_json::from_str(
        r#"{"timestamp":-5,"params":{"_meta":{"agentTimestampMs":1700000000000}}}"#,
    )
    .unwrap();
    assert_eq!(
        grok_row_timestamp(&negative),
        iso8601_utc(1_700_000_000_000)
    );
    let garbage: Value = serde_json::from_str(r#"{"timestamp":-9223372036854775808}"#).unwrap();
    assert_eq!(grok_row_timestamp(&garbage), "");
}
