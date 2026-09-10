use super::*;

#[test]
fn json_assembler_single_line() {
    let mut a = JsonAssembler::default();
    let v = a.feed(r#"{"type":"assistant"}"#).payload.unwrap();
    assert_eq!(v.get("type").unwrap(), "assistant");
}

#[test]
fn json_assembler_multi_line() {
    let mut a = JsonAssembler::default();
    assert!(a.feed("{\"type\":\"assistant\",").payload.is_none());
    let v = a.feed("\"x\":1}").payload.unwrap();
    assert_eq!(v.get("x").unwrap(), 1);
}

#[test]
fn json_assembler_recovers_from_an_unterminated_line() {
    let mut a = JsonAssembler::default();
    let stuck = a.feed(r#"{"type":"user","text":"never closed"#);
    assert!(stuck.discarded.is_none() && stuck.payload.is_none());
    // A continuation line is still buffered: it does not open a record.
    let more = a.feed(r#"  "x": 1,"#);
    assert!(more.discarded.is_none() && more.payload.is_none());
    let fresh = a.feed(r#"{"type":"assistant"}"#);
    assert!(fresh.discarded.is_some_and(|bytes| bytes > 0));
    assert_eq!(fresh.payload.unwrap().get("type").unwrap(), "assistant");
    // Back in sync: the next record needs no recovery.
    let next = a.feed(r#"{"type":"user"}"#);
    assert!(next.discarded.is_none());
    assert!(next.payload.is_some());

    // A genuinely multi-line value whose inner lines are indented is
    // still assembled, not mistaken for corruption.
    let mut a = JsonAssembler::default();
    assert!(a.feed("{").payload.is_none());
    assert!(a.feed(r#"  "inner": {"k": 1},"#).payload.is_none());
    assert!(a.feed(r#"  "type": "x""#).payload.is_none());
    assert_eq!(a.feed("}").payload.unwrap().get("type").unwrap(), "x");
}
