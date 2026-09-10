//! Draining a legacy (pre-SHA-256-key) mailbox into the stable one.

use super::*;

fn json_files_equal(left: &Path, right: &Path, label: &str, cap: usize) -> Result<bool> {
    let left_bytes = read_bounded_regular_file(left, label, cap)?
        .ok_or_else(|| anyhow!("{label} disappeared during migration: {}", left.display()))?;
    let right_bytes = read_bounded_regular_file(right, label, cap)?
        .ok_or_else(|| anyhow!("{label} disappeared during migration: {}", right.display()))?;
    if left_bytes == right_bytes {
        return Ok(true);
    }
    let left_json = serde_json::from_slice::<Value>(&left_bytes);
    let right_json = serde_json::from_slice::<Value>(&right_bytes);
    Ok(matches!((left_json, right_json), (Ok(left), Ok(right)) if left == right))
}

fn mailbox_json_files_equal(left: &Path, right: &Path) -> Result<bool> {
    let is_cursor = left
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == "cursors");
    if is_cursor {
        json_files_equal(left, right, "mailbox cursor", MAX_MAILBOX_STATE_BYTES)
    } else {
        json_files_equal(left, right, "mailbox message", MAX_ENVELOPE_BYTES)
    }
}

pub(crate) fn json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let paths = entries_with_extension(dir, &["json"])?;
    for path in &paths {
        if !fs::symlink_metadata(path)?.file_type().is_file() {
            bail!(
                "refusing to migrate non-file mailbox entry {}",
                path.display()
            );
        }
    }
    Ok(paths)
}

enum MailboxMigration {
    Move {
        source: PathBuf,
        destination: PathBuf,
    },
    RemoveDuplicate {
        source: PathBuf,
        destination: PathBuf,
    },
    MergeCursor {
        source: PathBuf,
        destination: PathBuf,
        cursor: Cursor,
    },
}

fn merged_cursor(left: &Cursor, right: &Cursor, retained_ids: &BTreeSet<Uuid>) -> Cursor {
    let mut left = left.clone();
    let mut right = right.clone();
    compact_cursor(&mut left, retained_ids);
    compact_cursor(&mut right, retained_ids);
    left.exceptions.extend(right.exceptions);
    left
}

/// Preflights every collision before moving anything. A crash during the
/// subsequent application can leave a partially drained legacy directory,
/// but rerunning is idempotent: unique files use no-replace hard links,
/// identical files are deduplicated, and cursors are unioned.
fn plan_mailbox_merge(
    stable: &MessagePaths,
    legacy: &MessagePaths,
    canonical_workspace: &Path,
) -> Result<Vec<MailboxMigration>> {
    let mut actions = Vec::new();
    for source in json_files(&legacy.msgs_dir)? {
        // Validate every source before scheduling any move. Unique files must
        // not bypass the same schema, identity, workspace, type, and size
        // checks applied to collisions and normal inbox reads.
        load_message_file(&source, canonical_workspace)
            .with_context(|| format!("validate legacy mailbox message {}", source.display()))?;
        let destination = stable.msgs_dir.join(
            source
                .file_name()
                .ok_or_else(|| anyhow!("{} has no file name", source.display()))?,
        );
        if destination.exists() {
            if !mailbox_json_files_equal(&source, &destination)? {
                bail!(
                    "mailbox message collision: {} and {} have different content",
                    source.display(),
                    destination.display()
                );
            }
            actions.push(MailboxMigration::RemoveDuplicate {
                source,
                destination,
            });
        } else {
            actions.push(MailboxMigration::Move {
                source,
                destination,
            });
        }
    }

    let mut retained_ids = retained_message_ids(&stable.msgs_dir)?;
    retained_ids.extend(retained_message_ids(&legacy.msgs_dir)?);
    for source in json_files(&legacy.cursors_dir)? {
        let source_cursor = read_cursor_file(&source)
            .with_context(|| format!("validate legacy mailbox cursor {}", source.display()))?;
        let destination = stable.cursors_dir.join(
            source
                .file_name()
                .ok_or_else(|| anyhow!("{} has no file name", source.display()))?,
        );
        if !destination.exists() {
            actions.push(MailboxMigration::Move {
                source,
                destination,
            });
            continue;
        }
        if mailbox_json_files_equal(&source, &destination)? {
            actions.push(MailboxMigration::RemoveDuplicate {
                source,
                destination,
            });
            continue;
        }
        let destination_cursor = read_cursor_file(&destination)
            .with_context(|| format!("parse colliding cursor {}", destination.display()))?;
        actions.push(MailboxMigration::MergeCursor {
            source,
            destination,
            cursor: merged_cursor(&source_cursor, &destination_cursor, &retained_ids),
        });
    }
    Ok(actions)
}

fn apply_mailbox_merge(actions: Vec<MailboxMigration>) -> Result<()> {
    let mut changed_dirs = BTreeSet::new();
    for action in actions {
        match action {
            MailboxMigration::Move {
                source,
                destination,
            } => {
                match fs::hard_link(&source, &destination) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        if !mailbox_json_files_equal(&source, &destination)? {
                            bail!(
                                "mailbox migration destination appeared with different content: {}",
                                destination.display()
                            );
                        }
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("migrate {} to {}", source.display(), destination.display())
                        });
                    }
                }
                fs::remove_file(&source)
                    .with_context(|| format!("remove migrated {}", source.display()))?;
                changed_dirs.insert(source.parent().unwrap().to_path_buf());
                changed_dirs.insert(destination.parent().unwrap().to_path_buf());
            }
            MailboxMigration::RemoveDuplicate {
                source,
                destination,
            } => {
                if !mailbox_json_files_equal(&source, &destination)? {
                    bail!(
                        "mailbox duplicate changed during migration: {}",
                        source.display()
                    );
                }
                fs::remove_file(&source)
                    .with_context(|| format!("remove duplicate {}", source.display()))?;
                changed_dirs.insert(source.parent().unwrap().to_path_buf());
            }
            MailboxMigration::MergeCursor {
                source,
                destination,
                cursor,
            } => {
                atomic_write_json(&destination, &cursor)?;
                fs::remove_file(&source)
                    .with_context(|| format!("remove merged cursor {}", source.display()))?;
                changed_dirs.insert(source.parent().unwrap().to_path_buf());
                changed_dirs.insert(destination.parent().unwrap().to_path_buf());
            }
        }
    }
    for dir in changed_dirs {
        fs::File::open(&dir)?.sync_all()?;
    }
    Ok(())
}

fn remove_drained_legacy_cursor_locks(legacy: &MessagePaths) -> Result<()> {
    let mut directory_changed = false;
    for consumer_id in cursor_entry_ids(&legacy.cursors_dir)? {
        let cursor_path = legacy.cursors_dir.join(format!("{consumer_id}.json"));
        if cursor_path.exists() {
            continue;
        }
        let lock_path = cursor_lock_path(&legacy.cursors_dir, consumer_id);
        let Some(_lock) = try_cursor_lock(&lock_path)? else {
            continue;
        };
        match fs::remove_file(&lock_path) {
            Ok(()) => directory_changed = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("remove drained legacy cursor lock {}", lock_path.display())
                });
            }
        }
    }
    if directory_changed {
        fs::File::open(&legacy.cursors_dir)?.sync_all()?;
    }
    Ok(())
}

pub(crate) fn merge_legacy_mailbox(
    stable: &MessagePaths,
    legacy: &MessagePaths,
    canonical_workspace: &Path,
) -> Result<()> {
    let actions = plan_mailbox_merge(stable, legacy, canonical_workspace)?;
    apply_mailbox_merge(actions)?;
    remove_drained_legacy_cursor_locks(legacy)
}
