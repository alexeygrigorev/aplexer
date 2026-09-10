use super::*;

#[test]
fn human_truncation_respects_utf8_boundaries() {
    let mut event = ev("tool_result");
    event.tool_output = Some(format!("{}é", "x".repeat(299)));
    assert_eq!(
        render_human(&event),
        format!("[tool_result]  {}...", "x".repeat(299))
    );
}
