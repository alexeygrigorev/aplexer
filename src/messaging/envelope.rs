//! The message wire format: sender, recipient, envelope, and the
//! addressing rule.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageFrom {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Design doc section 2.1: a sender with no resolvable session identity
    /// (no `--from`, no `APLEXER_SESSION_ID`) is still allowed to send, "a
    /// human poking at the mailbox is a legitimate participant". On the
    /// wire that is `{"external": true}` alone: absent fields are omitted,
    /// so the design doc's `"tag": null` is never written (and reads back
    /// as `None` either way).
    #[serde(default, skip_serializing_if = "is_false")]
    pub external: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl MessageFrom {
    pub fn anonymous() -> Self {
        Self {
            session_id: None,
            tag: None,
            engine: None,
            profile: None,
            external: true,
        }
    }
}

/// One of exactly three shapes (design doc section 5): `{"tag":...}` (with
/// optional `session_id` when resolvable at send time), `{"broadcast":true}`,
/// or `{"engine":...}`. `#[serde(untagged)]` tries each variant in
/// declaration order and matches on field presence, which reproduces this
/// exact wire shape (each variant has a disjoint field name) without a
/// separate discriminant tag -- unlike `Phase`/`FrameKind` elsewhere in this
/// crate, which use an explicit `tag = "..."` because their variants are
/// plain enums, not field-carrying shapes keyed by different field names.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Recipient {
    Tag {
        tag: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<Uuid>,
    },
    Broadcast {
        broadcast: bool,
    },
    Engine {
        engine: String,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    #[default]
    Inbox,
    Pane,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageEnvelope {
    pub schema_version: u32,
    pub id: Uuid,
    pub workspace: PathBuf,
    pub created_at: u64,
    pub from: MessageFrom,
    pub to: Recipient,
    /// Open enum (design doc section 5): unknown/future kinds must be
    /// preserved and displayed, never dropped -- hence a plain `String`
    /// rather than a closed Rust enum.
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<Uuid>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default)]
    pub delivery: Delivery,
}

fn default_kind() -> String {
    "note".to_string()
}

pub fn check_body_size(body: &str) -> Result<()> {
    let len = body.len();
    if len > MAX_BODY_BYTES {
        bail!("message body exceeds the {MAX_BODY_BYTES}-byte cap (got {len} bytes); point at a file in the workspace instead of pasting large content");
    }
    Ok(())
}

pub(crate) fn serialized_envelope(envelope: &MessageEnvelope) -> Result<Vec<u8>> {
    check_body_size(&envelope.body)?;
    // Match `atomic_write_json`'s durable representation exactly: it first
    // converts to a Value, pretty-prints it, and appends a newline.
    let value = serde_json::to_value(envelope)?;
    let mut bytes = serde_json::to_vec_pretty(&value)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_ENVELOPE_BYTES {
        bail!(
            "serialized message envelope exceeds the {MAX_ENVELOPE_BYTES}-byte cap (got {} bytes)",
            bytes.len()
        );
    }
    Ok(bytes)
}

/// Design doc section 2.2/2.3: the inbox filter matches on the recipient
/// session's *current* tag plus its session id recorded at send time (when
/// resolvable) -- so a renamed session keeps messages resolved to its id
/// and stops matching its old tag string, and a reused tag is inherited by
/// whatever session holds it now. Broadcast/engine-filtered forms exclude
/// the sender itself ("every session in the workspace except the sender").
pub fn addressed_to(
    envelope: &MessageEnvelope,
    consumer_id: Uuid,
    consumer_tag: &str,
    consumer_engine: &str,
) -> bool {
    let is_sender = envelope.from.session_id == Some(consumer_id);
    match &envelope.to {
        Recipient::Tag { tag, session_id } => {
            session_id.map(|sid| sid == consumer_id).unwrap_or(false) || tag == consumer_tag
        }
        Recipient::Broadcast { broadcast } => *broadcast && !is_sender,
        Recipient::Engine { engine } => engine == consumer_engine && !is_sender,
    }
}
