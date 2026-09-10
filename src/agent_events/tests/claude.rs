use super::*;

#[test]
fn claude_content_block_delta_maps_to_message() {
    let payload: Value = serde_json::from_str(
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}}"#,
    )
    .unwrap();
    let events = claude_wire_events(&payload);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "message");
    assert_eq!(events[0].content, "hi");
    assert_eq!(events[0].role.as_deref(), Some("assistant"));
}

#[test]
fn claude_tool_use_maps_to_tool_call() {
    let payload: Value = serde_json::from_str(
        r#"{"type":"stream_event","event":{"type":"content_block_start","content_block":{"type":"tool_use","name":"Bash","input":{"command":"ls"}}}}"#,
    )
    .unwrap();
    let events = claude_wire_events(&payload);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "tool_call");
    assert_eq!(events[0].tool_name.as_deref(), Some("Bash"));
    assert!(events[0].tool_input.as_deref().unwrap().contains("ls"));
}

#[test]
fn claude_user_tool_result_unwrap() {
    let payload: Value = serde_json::from_str(
        r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"t1","type":"tool_result","content":"output text"}]}}"#,
    )
    .unwrap();
    let events = claude_wire_events(&payload);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "tool_result");
    assert_eq!(events[0].tool_output.as_deref(), Some("output text"));
}

#[test]
fn claude_user_text_message() {
    let payload: Value = serde_json::from_str(
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"please review"}]}}"#,
    )
    .unwrap();
    let events = claude_wire_events(&payload);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].kind, "message");
    assert_eq!(events[0].role.as_deref(), Some("user"));
    assert_eq!(events[0].content, "please review");
}

#[test]
fn claude_continuation_from_system_init() {
    let payload: Value =
        serde_json::from_str(r#"{"type":"system","subtype":"init","session_id":"abc-123"}"#)
            .unwrap();
    assert_eq!(
        claude_wire_continuation(&payload).as_deref(),
        Some("abc-123")
    );
}
