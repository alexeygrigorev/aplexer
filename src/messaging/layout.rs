//! Mailbox naming and creation: the workspace key, the per-workspace
//! directory set, and `ensure_workspace`.

use super::*;

pub fn now_secs() -> u64 {
    now_ms() / 1000
}

/// Directory-naming key for a workspace's mailbox (design doc section 3.2
/// and open question 8): the first 128 bits of SHA-256 over the canonical
/// Unix path's exact bytes. Hashing the raw bytes avoids aliasing distinct
/// non-UTF-8 paths through lossy string conversion, and specifying SHA-256
/// keeps keys stable across Rust/toolchain releases. Every caller MUST pass
/// a path already run through `canonical_workspace` (open question 8) so
/// two sessions in the same workspace can never straddle two mailboxes.
pub fn workspace_key(canonical_workspace: &Path) -> String {
    let digest = Sha256::digest(canonical_workspace.as_os_str().as_bytes());
    hex_encode(&digest[..16])
}

/// Key emitted before mailbox keys were specified as truncated SHA-256.
/// This intentionally preserves the old implementation exactly so
/// `ensure_workspace` can locate and migrate existing mailboxes. It must not
/// be used for new directories: both `DefaultHasher`'s algorithm and the
/// lossy path conversion are unsuitable as a persistent storage format.
pub(crate) fn legacy_workspace_key(canonical_workspace: &Path) -> String {
    let text = canonical_workspace.to_string_lossy();
    let mut h1 = DefaultHasher::new();
    text.hash(&mut h1);
    let a = h1.finish();
    let mut h2 = DefaultHasher::new();
    (text.as_ref(), "aplexer-messaging-key-v1").hash(&mut h2);
    let b = h2.finish();
    format!("{a:016x}{b:016x}")
}

#[derive(Debug, Clone)]
pub struct MessagePaths {
    pub workspace_dir: PathBuf,
    pub msgs_dir: PathBuf,
    pub cursors_dir: PathBuf,
    pub workspace_file: PathBuf,
}

pub(crate) fn message_paths_for_key(paths: &Paths, key: &str) -> MessagePaths {
    let workspace_dir = paths.state_root.join("messages").join(key);
    MessagePaths {
        msgs_dir: workspace_dir.join("msgs"),
        cursors_dir: workspace_dir.join("cursors"),
        workspace_file: workspace_dir.join("workspace.json"),
        workspace_dir,
    }
}

pub fn message_paths(paths: &Paths, canonical_workspace: &Path) -> MessagePaths {
    message_paths_for_key(paths, &workspace_key(canonical_workspace))
}

#[derive(Deserialize)]
struct WorkspaceMetadata {
    workspace: PathBuf,
}

/// Verifies the reverse mapping before adopting a mailbox directory. The old
/// key was based on lossy UTF-8 and therefore could alias two distinct Unix
/// paths; the stable key is truncated and likewise must never be trusted
/// without its reverse mapping.
pub(crate) fn verify_workspace_metadata(
    workspace_dir: &Path,
    canonical_workspace: &Path,
) -> Result<()> {
    let metadata_path = workspace_dir.join("workspace.json");
    let bytes =
        read_bounded_regular_file(&metadata_path, "mailbox metadata", MAX_MAILBOX_STATE_BYTES)?
            .ok_or_else(|| anyhow!("mailbox metadata is missing: {}", metadata_path.display()))?;
    let metadata: WorkspaceMetadata = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse mailbox metadata {}", metadata_path.display()))?;
    if metadata.workspace != canonical_workspace {
        bail!(
            "refusing to migrate mailbox {}: workspace metadata names {}, expected {}",
            workspace_dir.display(),
            metadata.workspace.display(),
            canonical_workspace.display()
        );
    }
    Ok(())
}

fn initialize_workspace_dir(mp: &MessagePaths, canonical_workspace: &Path) -> Result<()> {
    ensure_private_dir(&mp.workspace_dir)?;
    ensure_private_dir(&mp.msgs_dir)?;
    ensure_private_dir(&mp.cursors_dir)?;
    if mp.workspace_file.exists() {
        verify_workspace_metadata(&mp.workspace_dir, canonical_workspace)?;
    } else {
        atomic_write_json(
            &mp.workspace_file,
            &serde_json::json!({"workspace": canonical_workspace}),
        )?;
    }
    Ok(())
}

/// Paths of `dir`'s entries whose extension is one of `extensions`, in
/// byte order. A missing directory lists as empty: every mailbox
/// subdirectory has a documented empty state, and creating it is
/// `ensure_workspace`'s job, not a reader's.
pub(crate) fn entries_with_extension(dir: &Path, extensions: &[&str]) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", dir.display())),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("enumerate {}", dir.display()))?
            .path();
        let extension = path.extension().and_then(|extension| extension.to_str());
        if extension.is_some_and(|extension| extensions.contains(&extension)) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

/// Creates the workspace mailbox's directories (`0700`, spec.md 26) and its
/// reverse-lookup `workspace.json` if missing, idempotently.
///
/// `ensure_private_dir` only chmods the exact path it's given, not any
/// parent directories `fs::create_dir_all` had to create along the way --
/// `${state_root}/messages/` itself would otherwise be created world/group-
/// readable (whatever the process umask gives `create_dir_all`) the first
/// time any workspace's mailbox is touched, since `Paths::ensure()` never
/// creates it up front. So the shared `messages/` root is chmod'd
/// explicitly here, before the per-workspace subdirectories.
pub fn ensure_workspace(paths: &Paths, canonical_workspace: &Path) -> Result<MessagePaths> {
    let messages_root = paths.state_root.join("messages");
    ensure_private_dir(&messages_root)?;

    let stable_key = workspace_key(canonical_workspace);
    let mp = message_paths_for_key(paths, &stable_key);
    let legacy_mp = message_paths_for_key(paths, &legacy_workspace_key(canonical_workspace));

    // Serialize discovery/migration for this destination. If only the legacy
    // directory exists it can still move atomically as a unit. If both exist,
    // take both mailbox locks and losslessly drain legacy JSON files into the
    // stable directory. The empty legacy skeleton is deliberately retained:
    // an older concurrently-installed CLI may write there again, and every
    // current operation will notice and drain it instead of silently ignoring
    // those messages.
    let migration_lock = messages_root.join(format!(".{stable_key}.migration.lock"));
    let _migration = FileLock::exclusive(&migration_lock, false)?;
    let legacy_present =
        legacy_mp.workspace_dir != mp.workspace_dir && legacy_mp.workspace_dir.exists();
    if legacy_present && !mp.workspace_dir.exists() {
        let _legacy_mailbox = FileLock::exclusive(&mailbox_lock_path(&legacy_mp), false)?;
        verify_workspace_metadata(&legacy_mp.workspace_dir, canonical_workspace)?;
        fs::rename(&legacy_mp.workspace_dir, &mp.workspace_dir).with_context(|| {
            format!(
                "migrate legacy mailbox {} to {}",
                legacy_mp.workspace_dir.display(),
                mp.workspace_dir.display()
            )
        })?;
    } else if legacy_present {
        initialize_workspace_dir(&mp, canonical_workspace)?;
        let _stable_mailbox = FileLock::exclusive(&mailbox_lock_path(&mp), false)?;
        let _legacy_mailbox = FileLock::exclusive(&mailbox_lock_path(&legacy_mp), false)?;
        verify_workspace_metadata(&mp.workspace_dir, canonical_workspace)?;
        verify_workspace_metadata(&legacy_mp.workspace_dir, canonical_workspace)?;
        ensure_private_dir(&legacy_mp.msgs_dir)?;
        ensure_private_dir(&legacy_mp.cursors_dir)?;
        merge_legacy_mailbox(&mp, &legacy_mp, canonical_workspace)?;
    }
    initialize_workspace_dir(&mp, canonical_workspace)?;
    Ok(mp)
}

pub(crate) fn mailbox_lock_path(mp: &MessagePaths) -> PathBuf {
    mp.workspace_dir.join(MAILBOX_LOCK_FILE)
}

pub(crate) fn uuid_stem(path: &Path) -> Option<Uuid> {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| Uuid::parse_str(stem).ok())
}
