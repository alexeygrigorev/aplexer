use super::*;

#[test]
fn encode_claude_cwd_replaces_slash_and_dot() {
    assert_eq!(
        encode_claude_cwd("/data/tmp/.tmpBU2mbw"),
        "-data-tmp--tmpBU2mbw"
    );
    assert_eq!(
        encode_claude_cwd("/tmp/aplexer-follow"),
        "-tmp-aplexer-follow"
    );
}

#[test]
fn encode_grok_cwd_matches_python_quote_safe_empty() {
    assert_eq!(
        encode_grok_cwd("/home/alexey/git/aplexer"),
        "%2Fhome%2Falexey%2Fgit%2Faplexer"
    );
    assert_eq!(
        encode_grok_cwd("/tmp/aplexer-tx-test"),
        "%2Ftmp%2Faplexer-tx-test"
    );
}

#[test]
fn bind_sidecar_reuses_path() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.jsonl");
    std::fs::write(&log, "{}\n").unwrap();
    let bind = dir.path().join("transcript.json");
    let record = dummy_record("claude");
    // First locate would fail (HOME is not this dir); write a bind first.
    atomic_write_json(
        &bind,
        &TranscriptBind {
            path: log.clone(),
            engine_session_id: Some("x".into()),
        },
    )
    .unwrap();
    let located = resolve_transcript(&record, &bind).unwrap();
    assert_eq!(located.path, log);
    assert_eq!(located.engine_session_id.as_deref(), Some("x"));
}
