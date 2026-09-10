//! Location + bind sidecar.

use super::*;

// ---------------------------------------------------------------------
// Location + bind sidecar.
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TranscriptBind {
    pub(crate) path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) engine_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LocatedTranscript {
    pub path: PathBuf,
    pub engine_session_id: Option<String>,
}

/// Locate (or reuse the bound path of) the native log for `record`.
/// Writes `<state>/sessions/<id>/transcript.json` on a successful first
/// find so `--follow` and later pages do not re-run the cwd/mtime heuristic.
pub fn resolve_transcript(record: &SessionRecord, bind_path: &Path) -> Result<LocatedTranscript> {
    if let Ok(bytes) = fs::read(bind_path) {
        if let Ok(bind) = serde_json::from_slice::<TranscriptBind>(&bytes) {
            if bind.path.is_file() {
                return Ok(LocatedTranscript {
                    path: bind.path,
                    engine_session_id: bind.engine_session_id,
                });
            }
        }
    }
    let path = locate_transcript(
        &record.engine,
        &record.cwd,
        record.created_at_ms,
        &record.env,
    )
    .ok_or_else(|| {
        anyhow!(
            "no {} transcript found for session {} (cwd {}); the agent may not have written anything yet",
            record.engine,
            record.id,
            record.cwd.display()
        )
    })?;
    let engine_session_id = peek_continuation(&record.engine, &path);
    let bind = TranscriptBind {
        path: path.clone(),
        engine_session_id: engine_session_id.clone(),
    };
    // Best-effort: a bind write failing must not hide a successful locate.
    let _ = atomic_write_json(bind_path, &bind);
    Ok(LocatedTranscript {
        path,
        engine_session_id,
    })
}

pub fn locate_transcript(
    engine: &str,
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    match engine_family(engine) {
        "claude" => locate_claude_transcript(cwd, created_at_ms, env),
        "codex" => locate_codex_transcript(cwd, created_at_ms, env),
        "grok" => locate_grok_transcript(cwd, created_at_ms, env),
        _ => None,
    }
}

fn peek_continuation(engine: &str, path: &Path) -> Option<String> {
    let format = wire_format_for(engine).ok()?;
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut assembler = JsonAssembler::default();
    for line in reader.lines().map_while(std::io::Result::ok).take(64) {
        let Some(payload) = assembler.feed(&line).payload else {
            continue;
        };
        let (_events, continuation) = translate(format, &payload);
        if continuation.is_some() {
            return continuation;
        }
    }
    None
}

/// Claude Code: `~/.claude/projects/<encoded-cwd>/<session>.jsonl`
/// (or `$CLAUDE_CONFIG_DIR/projects/...` for profiles). Encoding matches
/// PocketShell `AgentDetector.encodeClaudeCwd`: `/` and `.` both become `-`. aplexer has no direct handle on the underlying
/// claude session id, only the aplexer session's own `cwd` and
/// `created_at_ms` -- so this picks the most-recently-modified `*.jsonl`
/// directly under that cwd's project directory whose mtime is not earlier
/// than the aplexer session's creation (with a few seconds of slack for
/// startup ordering). This is a heuristic, not an exact session-id match:
/// if two aplexer claude sessions share the exact same cwd and are both
/// live, the bind sidecar is what keeps later reads on the first-found
/// file. Documented, not silently assumed.
pub fn locate_claude_transcript(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    let config_dir = env
        .get("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")))?;
    let encoded = encode_claude_cwd(&cwd.display().to_string());
    let dir = config_dir.join("projects").join(encoded);
    if !dir.is_dir() {
        return None;
    }
    let since = created_at_ms.saturating_sub(5_000);
    // Direct children only: claude's project dirs also hold a `subagents/`
    // subdirectory, which is deliberately NOT walked -- those are sub-agent
    // transcripts, not the top-level session.
    let children = fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("jsonl")
        });
    newest_since(children, since)
}

/// Codex: `~/.codex/sessions/<YYYY>/<MM>/<DD>/<session>.jsonl`, date-
/// partitioned so the tree is walked rather than computed directly
/// (`agent_log.py::_resolve_codex_path`). Each rollout file's first line is
/// a `session_meta` row carrying its own `cwd`, which lets this disambiguate
/// candidates precisely rather than relying on mtime alone.
pub fn locate_codex_transcript(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    let root = env
        .get("CODEX_HOME")
        .map(|h| PathBuf::from(h).join("sessions"))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".codex/sessions")))?;
    if !root.is_dir() {
        return None;
    }
    let since = created_at_ms.saturating_sub(5_000);
    let cwd_str = cwd.display().to_string();
    let mut rollouts = Vec::new();
    walk_jsonl(&root, &mut |path| rollouts.push(path.to_path_buf()));
    // Only rollouts recent enough to matter are opened: one whose
    // session_meta names a different cwd is not ours, whatever its mtime,
    // while one without a readable session_meta stays a candidate on
    // mtime alone.
    let ours = rollouts
        .into_iter()
        .filter(|path| file_mtime_ms(path).is_some_and(|mtime| mtime >= since))
        .filter(|path| rollout_cwd(path).is_none_or(|rollout_cwd| rollout_cwd == cwd_str));
    newest_since(ours, since)
}

/// The `cwd` recorded in a codex rollout's first (`session_meta`) row.
fn rollout_cwd(path: &Path) -> Option<String> {
    let mut first_line = String::new();
    BufReader::new(File::open(path).ok()?)
        .read_line(&mut first_line)
        .ok()?;
    codex_native_cwd(&serde_json::from_str::<Value>(first_line.trim()).ok()?)
}

/// Grok Build: `$GROK_HOME/sessions/<urlencoded-cwd>/<session-id>/updates.jsonl`
/// (default `GROK_HOME` is `~/.grok`). Percent-encoding matches
/// `urllib.parse.quote(cwd, safe="")` in pocketshell's `agent_log.py`.
pub fn locate_grok_transcript(
    cwd: &Path,
    created_at_ms: u64,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    let root = grok_sessions_root(env)?;
    if !root.is_dir() {
        return None;
    }
    let since = created_at_ms.saturating_sub(5_000);
    let encoded = encode_grok_cwd(&cwd.display().to_string());
    let project = root.join(&encoded);
    // Stay inside this cwd's encoded directory. Walking every grok session
    // tree would bind an unrelated live session (this agent's own
    // updates.jsonl is the usual false match).
    if project.is_dir() {
        return best_grok_updates(&project, since);
    }
    None
}

fn grok_sessions_root(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(home) = env
        .get("GROK_HOME")
        .cloned()
        .or_else(|| std::env::var("GROK_HOME").ok())
    {
        return Some(PathBuf::from(home).join("sessions"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".grok/sessions"))
}

pub(crate) fn encode_claude_cwd(cwd: &str) -> String {
    let trimmed = cwd.trim();
    if trimmed.is_empty() {
        "-".into()
    } else {
        trimmed.replace(['/', '.'], "-")
    }
}

pub(crate) fn encode_grok_cwd(cwd: &str) -> String {
    // urllib.parse.quote(cwd, safe="") -- RFC 3986 unreserved
    // (ALPHA / DIGIT / "-" / "." / "_" / "~") stay literal; everything
    // else, including `/`, is percent-encoded. `safe=""` only *adds*
    // extra unencoded bytes; it does not encode `-_.~`.
    let trimmed = cwd.trim();
    let trimmed = if trimmed.is_empty() { "/" } else { trimmed };
    let mut out = String::new();
    for b in trimmed.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn best_grok_updates(project_dir: &Path, since_ms: u64) -> Option<PathBuf> {
    let updates = fs::read_dir(project_dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join("updates.jsonl"))
        .filter(|candidate| candidate.is_file());
    newest_since(updates, since_ms)
}

fn file_mtime_ms(path: &Path) -> Option<u64> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as u64)
}

/// Every `*.jsonl` under `dir`, recursively.
fn walk_jsonl(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_jsonl(&path, visit);
        } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            visit(&path);
        }
    }
}

/// The most-recently-modified candidate whose mtime is `>= since_ms` --
/// the "which log is this session's" heuristic every engine shares.
fn newest_since(candidates: impl IntoIterator<Item = PathBuf>, since_ms: u64) -> Option<PathBuf> {
    candidates
        .into_iter()
        .filter_map(|path| file_mtime_ms(&path).map(|mtime| (mtime, path)))
        .filter(|(mtime, _)| *mtime >= since_ms)
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, path)| path)
}
