//! Filesystem layer: read-or-default, mode-preserving symlink-aware
//! atomic writes, and the per-file nested-hooks operations.

use super::*;

/// Read a JSON file, defaulting to an empty object when absent/blank.
/// A malformed file is an error (refuse to clobber what we cannot parse).
fn read_json_or_default(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Object(Default::default()));
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    serde_json::from_str(&text)
        .with_context(|| format!("parse {} (left untouched)", path.display()))
}

/// Where a write to `path` must land: the file itself, or -- when `path`
/// is a symlink, as a dotfiles-managed `settings.json` is -- its target,
/// so the atomic rename replaces the real file and leaves the link intact.
/// A dangling link resolves to the file it points at, which the write then
/// creates.
fn write_target(path: &Path) -> Result<PathBuf> {
    let is_symlink = fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
    if !is_symlink {
        return Ok(path.to_path_buf());
    }
    if let Ok(real) = fs::canonicalize(path) {
        return Ok(real);
    }
    let link = fs::read_link(path).with_context(|| format!("read link {}", path.display()))?;
    Ok(if link.is_absolute() {
        link
    } else {
        path.parent().unwrap_or(Path::new("")).join(link)
    })
}

/// Atomically write text, preserving the existing file's mode and using
/// 0600 for new files. Unlike the session-record writer this must NOT
/// force private dirs: engine configs live in the user's normal (often
/// 0755) home tree.
fn atomic_write_text_preserving_mode(path: &Path, text: &str) -> Result<()> {
    let target = write_target(path)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    let mode = fs::metadata(&target)
        .map(|meta| meta.permissions().mode() & 0o777)
        .unwrap_or(0o600);
    atomic_write_bytes_with_mode(&target, text.as_bytes(), mode)
        .with_context(|| format!("write {}", target.display()))
}

/// Write only when the content differs (idempotence without mtime churn).
/// Returns true when the file was written.
pub(crate) fn write_if_changed(path: &Path, text: &str) -> Result<bool> {
    if path.exists() {
        let current =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        if current == text {
            return Ok(false);
        }
    }
    atomic_write_text_preserving_mode(path, text)?;
    Ok(true)
}

pub(crate) fn render_json(doc: &Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(doc)? + "\n")
}

/// Install/merge nested hooks into one JSON settings file. Returns
/// (changed, message).
pub(crate) fn install_nested_file(
    path: &Path,
    events: &[(&str, &str)],
    a_bin: &str,
) -> Result<(bool, String)> {
    let mut doc = read_json_or_default(path)?;
    let changed = merge_nested_hooks(&mut doc, events, a_bin)?;
    if changed == 0 {
        return Ok((
            false,
            format!("hook already installed in {}", path.display()),
        ));
    }
    write_if_changed(path, &render_json(&doc)?)?;
    Ok((true, format!("merged hook into {}", path.display())))
}

pub(crate) fn check_nested_file(path: &Path, events: &[(&str, &str)]) -> (bool, String) {
    if !path.exists() {
        return (false, format!("{} not present", path.display()));
    }
    match read_json_or_default(path) {
        Err(e) => (false, format!("{}: {e:#}", path.display())),
        Ok(doc) => {
            let missing = missing_nested_hooks(&doc, events);
            if missing.is_empty() {
                (true, format!("hook installed in {}", path.display()))
            } else {
                (
                    false,
                    format!("{} missing events: {}", path.display(), missing.join(", ")),
                )
            }
        }
    }
}

pub(crate) fn uninstall_nested_file(path: &Path) -> Result<(bool, String)> {
    if !path.exists() {
        return Ok((false, format!("{} not present", path.display())));
    }
    let mut doc = read_json_or_default(path)?;
    if !unmerge_nested_hooks(&mut doc) {
        return Ok((false, format!("no hook in {}", path.display())));
    }
    write_if_changed(path, &render_json(&doc)?)?;
    Ok((true, format!("removed hook from {}", path.display())))
}
