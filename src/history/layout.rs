use super::*;

pub(crate) const HISTORY_FORMAT_VERSION: u32 = 2;
pub(crate) const HISTORY_BANK_MAGIC: &[u8; 8] = b"APLXH2D\0";
pub(crate) const HISTORY_BANK_HEADER_PREFIX_BYTES: usize = 72;
pub(crate) const HISTORY_BANK_HEADER_BYTES: usize = HISTORY_BANK_HEADER_PREFIX_BYTES + 32;
pub(crate) const HISTORY_COMMIT_MAX_BYTES: usize = 4096;
pub(crate) const HISTORY_MARKER_MAX_BYTES: usize = 4096;
pub(crate) const HISTORY_BANK_COUNT: u8 = 2;
pub(crate) const HISTORY_COMMIT_COUNT: u8 = 2;
pub(crate) const HISTORY_HASH_CHUNK_BYTES: usize = 64 * 1024;

/// Lowercase hex, the one encoding every on-disk checksum and key uses.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

pub(crate) fn history_sidecar_path(path: &Path, kind: &str, slot: u8) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("history.bin"))
        .to_os_string();
    name.push(format!(".v2.{kind}.{slot}"));
    path.with_file_name(name)
}

pub(crate) fn history_data_path(path: &Path, slot: u8) -> PathBuf {
    history_sidecar_path(path, "data", slot)
}

pub(crate) fn history_commit_path(path: &Path, slot: u8) -> PathBuf {
    history_sidecar_path(path, "commit", slot)
}

pub(crate) fn history_marker_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| OsStr::new("history.bin"))
        .to_os_string();
    name.push(".v2.marker");
    path.with_file_name(name)
}

pub(crate) fn history_session_id(path: &Path) -> Uuid {
    path.parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .and_then(|name| name.parse().ok())
        .unwrap_or_else(Uuid::nil)
}

pub(crate) fn validate_optional_history_node(path: &Path, label: &str) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => bail!("{label} {} is not a regular file", path.display()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("inspect {label} {}", path.display())),
    }
}

pub(crate) fn open_optional_history_file(
    path: &Path,
    label: &str,
    write: bool,
) -> Result<Option<File>> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("open {label} {}", path.display()))
        }
    };
    let metadata = file
        .metadata()
        .with_context(|| format!("inspect {label} {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        bail!("{label} {} is not a trusted regular file", path.display());
    }
    Ok(Some(file))
}

pub(crate) fn validate_history_artifacts(path: &Path) -> Result<()> {
    validate_optional_history_node(path, "legacy history")?;
    validate_optional_history_node(&history_marker_path(path), "history marker")?;
    for slot in 0..HISTORY_BANK_COUNT {
        let data_path = history_data_path(path, slot);
        if validate_optional_history_node(&data_path, "history data bank")? {
            let length = fs::symlink_metadata(&data_path)?.len();
            let hard_cap = HISTORY_BANK_HEADER_BYTES as u64 + 2 * MAX_HISTORY_BYTES as u64;
            if length > hard_cap {
                bail!(
                    "history data bank {} exceeds the {}-byte hard cap",
                    data_path.display(),
                    hard_cap
                );
            }
        }
    }
    for slot in 0..HISTORY_COMMIT_COUNT {
        validate_optional_history_node(&history_commit_path(path, slot), "history commit")?;
    }
    Ok(())
}
