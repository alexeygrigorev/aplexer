//! Unit tests for the wire protocol.

use super::*;

#[test]
fn frame_round_trip() {
    let mut bytes = Vec::new();
    write_frame(&mut bytes, FrameKind::Data, b"a\0b").unwrap();
    let mut cursor = io::Cursor::new(bytes);
    let frame = read_frame(&mut cursor).unwrap().unwrap();
    assert_eq!(frame.kind, FrameKind::Data);
    assert_eq!(frame.payload, b"a\0b");
}

#[test]
fn bound_request_remains_readable_by_legacy_workers() {
    #[derive(Deserialize)]
    struct LegacyRequest {
        version: u16,
        request_id: String,
        #[serde(flatten)]
        operation: Operation,
    }

    let request = Request::new(Uuid::new_v4(), Operation::Ping);
    let legacy: LegacyRequest =
        serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
    assert_eq!(legacy.version, PROTOCOL_VERSION);
    assert_eq!(legacy.request_id, request.request_id);
    assert!(matches!(legacy.operation, Operation::Ping));
}
