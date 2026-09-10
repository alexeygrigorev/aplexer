use super::*;

pub(crate) fn base64_standard(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(data.len().div_ceil(3).saturating_mul(4));
    for chunk in data.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

pub(crate) fn capture_json_value(record: &SessionRecord, data: &[u8]) -> Value {
    let mut value = json!({
        "id": record.id,
        "bytes": data.len(),
        "encoding": "base64",
        "data": base64_standard(data),
    });
    // Preserve the old ergonomic field for text consumers, but only when it
    // is exact. `from_utf8_lossy` corrupted arbitrary PTY bytes while still
    // presenting the replacement-filled string as if it were authoritative.
    if let Ok(text) = std::str::from_utf8(data) {
        value["utf8"] = json!(text);
    }
    value
}

pub(crate) fn cmd_capture(paths: &Paths, args: CaptureArgs, json_output: bool) -> Result<()> {
    let record = resolve(paths, &args.target)?;
    let data = if args.screen {
        match rpc_capture_screen(&record, args.plain) {
            Ok(data) => data,
            // Dead-session fallback (design doc section 5.5/8): screen.txt
            // is the plain-text screen as it looked the moment the worker
            // exited, written once by OutputHub::finish. Unlike the raw
            // history fallback below, there is no paintable-form fallback
            // for a dead session -- the live grid died with the worker, and
            // only the plain text was preserved -- so --screen without
            // --plain against a dead session still surfaces the "worker
            // unavailable" error rather than silently downgrading to text.
            Err(_) if args.plain => match fs::read(paths.screen_txt(record.id)) {
                Ok(bytes) => bytes,
                Err(read_error) => {
                    check_attachable(&record)?;
                    return Err(read_error)
                        .context("worker unavailable and persisted screen.txt cannot be read");
                }
            },
            Err(error) => {
                check_attachable(&record)?;
                return Err(error).context("worker unavailable");
            }
        }
    } else {
        match rpc_capture(&record, args.bytes) {
            Ok(data) => data,
            // Persisted history is authoritative post-mortem data only once
            // the record is terminal or the worker process is known gone. A
            // live process returning an RPC error may merely be wedged or
            // temporarily unreachable; silently returning an older file in
            // that case makes stale output look current and hides the actual
            // operational failure.
            Err(_)
                if matches!(record.phase, Phase::Exited | Phase::Failed)
                    || !record.worker_alive() =>
            {
                match read_persisted_history_tail(&record.history_path, args.bytes) {
                    Ok(bytes) => bytes,
                    Err(read_error) => {
                        check_attachable(&record)?;
                        return Err(read_error)
                            .context("worker unavailable and persisted history cannot be read");
                    }
                }
            }
            Err(error) => {
                return Err(error).context(
                    "capture RPC failed while the worker process is still alive; refusing to return potentially stale persisted history",
                );
            }
        }
    };
    if let Some(path) = args.output {
        fs::write(&path, &data).with_context(|| format!("write {}", path.display()))?;
    } else if json_output {
        println!("{}", capture_json_value(&record, &data));
    } else {
        io::stdout().write_all(&data)?;
    }
    Ok(())
}
