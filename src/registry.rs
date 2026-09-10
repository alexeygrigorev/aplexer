//! The session registry: reading a record file (with corrupt-record
//! diagnosis), enumerating the registry directory, and resolving a CLI
//! tag/id reference to its record.

use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::Path;
use uuid::Uuid;

use crate::{canonical_workspace, io_kind, reap_verdict, Paths, SessionRecord, SCHEMA_VERSION};

pub fn read_record(path: &Path) -> Result<SessionRecord> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let record: SessionRecord =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    if record.schema_version != SCHEMA_VERSION {
        bail!("unsupported session schema {}", record.schema_version);
    }
    Ok(record)
}

/// Load one record through its registry identity and reject cross-session or
/// displaced path metadata. `read_record` remains the low-level parser for
/// startup rollback code that already owns an exact path; registry consumers
/// should use this function (directly or through `list_records`).
pub fn read_session_record(paths: &Paths, id: Uuid) -> Result<SessionRecord> {
    let path = paths.record(id);
    let record = read_record(&path)?;
    if record.id != id {
        bail!(
            "session record {} contains id {}, expected directory id {id}",
            path.display(),
            record.id
        );
    }
    let expected_socket = paths.socket(id);
    if record.socket_path != expected_socket {
        bail!(
            "session record {} contains socket path {}, expected {}",
            path.display(),
            record.socket_path.display(),
            expected_socket.display()
        );
    }
    let expected_history = paths.history(id);
    if record.history_path != expected_history {
        bail!(
            "session record {} contains history path {}, expected {}",
            path.display(),
            record.history_path.display(),
            expected_history.display()
        );
    }
    // Do not apply the current worker-allocation ceiling while enumerating
    // durable records. Older releases could persist larger rings; those
    // sessions must remain addressable for status, capture, and forget after
    // an upgrade. Config resolution and `History::open` enforce the ceiling
    // before every new worker allocation.
    Ok(record)
}

pub fn list_records(paths: &Paths) -> Result<Vec<SessionRecord>> {
    let mut out = Vec::new();
    let root = paths.state_root.join("sessions");
    let entries =
        fs::read_dir(&root).with_context(|| format!("read session registry {}", root.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("read entry in {}", root.display()))?;
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("inspect session registry entry {}", entry_path.display()))?;
        if !file_type.is_dir() {
            bail!(
                "session registry entry {} is not a directory",
                entry_path.display()
            );
        }
        let id_text = entry.file_name().into_string().map_err(|_| {
            anyhow!(
                "session registry entry {} is not UTF-8",
                entry_path.display()
            )
        })?;
        let id = id_text.parse::<Uuid>().with_context(|| {
            format!(
                "session registry directory {} is not named by a UUID",
                entry_path.display()
            )
        })?;
        match read_session_record(paths, id) {
            Ok(record) => out.push(record),
            // A session directory with no record in it yet is a session being
            // created, not a corrupt registry: `start_session` creates the
            // directory a moment before it writes the session's first record.
            // Both happen under the registry lock, so no other `a start` can
            // observe the gap -- but every reader that does NOT take that lock
            // can, and `a watch` polls the registry forever, so it hits the gap
            // eventually and used to exit on it. Measured on an idle box, an
            // `a watch` starting alongside an `a start` died this way on 2 runs
            // out of 20:
            //
            //     a: load session registry entry <dir>: read <dir>/session.json:
            //     No such file or directory (os error 2)
            //
            // Skipping the entry reports the session on the next scan instead,
            // which is what the watcher's own new-session handling already
            // does. Every other defect -- unparseable record, unsupported
            // schema, mismatched identity or paths, a stray non-directory entry
            // -- still fails closed exactly as before.
            Err(error) if record_is_not_written_yet(&error) => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("load session registry entry {}", entry_path.display())
                })
            }
        }
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.created_at_ms));
    Ok(out)
}

/// Whether `error` reports a session record that is absent, as opposed to one
/// that exists and is wrong. `read_record`'s `fs::read` is the only filesystem
/// access in the chain, so a `NotFound` anywhere in it can only mean the record
/// file itself is missing.
pub(crate) fn record_is_not_written_yet(error: &anyhow::Error) -> bool {
    io_kind(error) == Some(io::ErrorKind::NotFound)
}

pub fn resolve_record(
    paths: &Paths,
    selector: Option<&str>,
    workspace: Option<&Path>,
    tag: Option<&str>,
) -> Result<SessionRecord> {
    let records = list_records(paths)?;
    let mut matches: Vec<&SessionRecord> = Vec::new();
    if let Some(raw) = selector {
        let needle = raw.to_ascii_lowercase();
        // `workspace:tag`, matched against the record's fields directly so
        // no selector string is built per record compared.
        let pair = raw.rsplit_once(':');
        matches.extend(records.iter().filter(|record| {
            let id = record.id.to_string();
            id == needle
                || (needle.len() >= 8 && id.starts_with(&needle))
                || pair.is_some_and(|(workspace, tag)| {
                    record.tag == tag && record.workspace.as_os_str() == OsStr::new(workspace)
                })
        }));
        if matches.is_empty() {
            if let Some((workspace_text, tag_text)) = pair {
                if let Ok(ws) = canonical_workspace(Path::new(workspace_text)) {
                    matches.extend(
                        records
                            .iter()
                            .filter(|record| record.workspace == ws && record.tag == tag_text),
                    );
                }
            }
        }
    } else {
        let ws = canonical_workspace(workspace.unwrap_or(Path::new(".")))?;
        let tag = tag.unwrap_or("default");
        matches.extend(
            records
                .iter()
                .filter(|record| record.workspace == ws && record.tag == tag),
        );
    }
    match matches.len() {
        0 => bail!("no matching session"),
        1 => Ok(matches[0].clone()),
        // A pair can transiently be held by two records: `a rename` takes a
        // dead holder's name but leaves the corpse for `a prune` (issue
        // #13), and the pair is only clean again once prune runs. In that
        // window the selector still means the live session -- a corpse must
        // not shadow it, or the rename that fixed the invisible-corpse
        // error would make the name unusable instead. Only when no live
        // holder disambiguates the matches does the ambiguity error apply.
        _ => {
            let mut live = matches.iter().filter(|r| reap_verdict(r).is_none());
            match (live.next(), live.next()) {
                (Some(only), None) => Ok((*only).clone()),
                _ => bail!("selector is ambiguous; use a longer UUID"),
            }
        }
    }
}
