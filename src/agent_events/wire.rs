//! Wire format dispatch (native logs only -- there is no headless exec)
//! and the event-construction helpers every translator shares.

use super::*;

pub(crate) fn ev(kind: &'static str) -> UnifiedEvent {
    UnifiedEvent {
        kind,
        // heru's `UnifiedEvent.raw` defaults to `{}` (`Field(default_factory
        // =dict)`), not null -- `Value`'s own `Default` is `Null`.
        raw: json!({}),
        ..Default::default()
    }
}

pub(crate) fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

pub(crate) fn int_meta(map: &mut BTreeMap<String, Value>, key: &str, v: &Value) {
    if let Some(n) = v.get(key).and_then(|x| x.as_i64()) {
        map.insert(key.to_string(), json!(n));
    }
}

// ---------------------------------------------------------------------
// Wire format dispatch (native logs only -- there is no headless exec).
// ---------------------------------------------------------------------

#[derive(Clone, Copy)]
pub(crate) enum WireFormat {
    Claude,
    CodexNative,
    Grok,
}

pub(crate) fn wire_format_for(engine: &str) -> Result<WireFormat> {
    // Variant engines (see `engine_family`) parse as their family -- the
    // session's own engine id is what lands on emitted events.
    match engine_family(engine) {
        "claude" => Ok(WireFormat::Claude),
        "codex" => Ok(WireFormat::CodexNative),
        "grok" => Ok(WireFormat::Grok),
        other => bail!("a transcript supports claude, codex, and grok only (got engine {other})"),
    }
}

/// Events and any continuation id in one payload. Events come back
/// without an engine: the reader stamps the session's own engine id, which
/// for a variant engine differs from the family that parsed it.
pub(crate) fn translate(
    format: WireFormat,
    payload: &Value,
) -> (Vec<UnifiedEvent>, Option<String>) {
    match format {
        WireFormat::Claude => (
            claude_wire_events(payload),
            claude_wire_continuation(payload),
        ),
        WireFormat::CodexNative => (
            codex_native_events(payload),
            codex_native_continuation(payload),
        ),
        WireFormat::Grok => (
            grok_native_events(payload),
            grok_native_continuation(payload),
        ),
    }
}

pub(crate) fn payload_to_raw(payload: &Value) -> Value {
    if payload.is_object() {
        payload.clone()
    } else {
        json!({"value": payload})
    }
}

pub(crate) fn row_timestamp(format: WireFormat, payload: &Value) -> String {
    match format {
        WireFormat::Grok => grok_row_timestamp(payload),
        WireFormat::Claude | WireFormat::CodexNative => payload
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

pub(crate) fn stamp_session(event: &mut UnifiedEvent, record: &SessionRecord) {
    event
        .metadata
        .extend(UnifiedEvent::session_metadata(record));
    if let Some(profile) = &record.profile {
        event.metadata.insert("profile".into(), json!(profile));
    }
}

/// Compact one-line human rendering (`UnifiedEvent::render_human`).
pub fn render_human(event: &UnifiedEvent) -> String {
    event.render_human()
}
