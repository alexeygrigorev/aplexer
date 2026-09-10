//! Who is sending or reading: session lookup by tag and identity
//! resolution from `--from` or the environment.

use super::*;

/// Known tags for a workspace (design doc section 2.3), from session
/// metadata -- live or historical, not just currently-running sessions.
pub fn known_tags(records: &[SessionRecord], canonical_workspace: &Path) -> Vec<String> {
    let mut tags: Vec<String> = records
        .iter()
        .filter(|r| r.workspace == canonical_workspace)
        .map(|r| r.tag.clone())
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

/// The session record for `tag` in `canonical_workspace`, live or
/// historical -- the one lookup `--from`, `--to`, `reply`, and pane
/// delivery all share.
pub fn session_by_tag<'a>(
    records: &'a [SessionRecord],
    canonical_workspace: &Path,
    tag: &str,
) -> Option<&'a SessionRecord> {
    records
        .iter()
        .find(|r| r.workspace == canonical_workspace && r.tag == tag)
}

/// A resolved sender or consumer identity: the session id plus whatever
/// of tag/engine/profile its record -- or, failing that, the environment
/// -- could supply. `send`/`log` degrade to anonymous without one
/// (`MessageFrom::from_identity`); `inbox`/`ack` require one
/// (`SessionIdentity::required`) -- design doc section 2.1 and section 7's
/// closing note.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    pub id: Uuid,
    pub tag: Option<String>,
    pub engine: Option<String>,
    pub profile: Option<String>,
}

impl SessionIdentity {
    fn from_record(record: &SessionRecord) -> Self {
        Self {
            id: record.id,
            tag: Some(record.tag.clone()),
            engine: Some(record.engine.clone()),
            profile: record.profile.clone(),
        }
    }

    /// The consumer identity `inbox`/`ack` need, or a clear error when
    /// neither `--from` nor `APLEXER_SESSION_ID` was available.
    pub fn required(identity: Option<Self>) -> Result<Self> {
        identity.ok_or_else(|| {
            anyhow!(
                "no session identity: APLEXER_SESSION_ID is not set (you're not inside an aplexer \
                 session) and no --from TAG was given"
            )
        })
    }

    /// Whether `envelope` is addressed to this session (`addressed_to`).
    pub fn receives(&self, envelope: &MessageEnvelope) -> bool {
        addressed_to(
            envelope,
            self.id,
            self.tag.as_deref().unwrap_or(""),
            self.engine.as_deref().unwrap_or(""),
        )
    }
}

impl MessageFrom {
    /// The sender recorded on an envelope: the identity when one resolved,
    /// else anonymous.
    pub fn from_identity(identity: Option<SessionIdentity>) -> Self {
        match identity {
            Some(identity) => Self {
                session_id: Some(identity.id),
                tag: identity.tag,
                engine: identity.engine,
                profile: identity.profile,
                external: false,
            },
            None => Self::anonymous(),
        }
    }
}

/// Resolution order (design doc section 7): `--from <tag>` matched against
/// session metadata for this workspace (an unknown tag is an error), else
/// `APLEXER_SESSION_ID` with a best-effort record lookup for tag/engine/
/// profile (falling back to `APLEXER_TAG`), else `None`.
pub fn resolve_identity(
    records: &[SessionRecord],
    canonical_workspace: &Path,
    from_tag: Option<&str>,
) -> Result<Option<SessionIdentity>> {
    if let Some(tag) = from_tag {
        let record = session_by_tag(records, canonical_workspace, tag).ok_or_else(|| {
            anyhow!(
                "no session tagged {tag:?} has ever existed in workspace {}",
                canonical_workspace.display()
            )
        })?;
        return Ok(Some(SessionIdentity::from_record(record)));
    }
    let Some(session_id) = crate::discover_session_id() else {
        return Ok(None);
    };
    Ok(Some(match records.iter().find(|r| r.id == session_id) {
        Some(record) => SessionIdentity::from_record(record),
        None => SessionIdentity {
            id: session_id,
            tag: std::env::var("APLEXER_TAG").ok(),
            engine: None,
            profile: None,
        },
    }))
}
