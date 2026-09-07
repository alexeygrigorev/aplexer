//! Small shared helpers: wall-clock milliseconds, byte-size parsing, and
//! environment/OsString conversions.

use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}


pub fn parse_byte_size(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty byte size");
    }
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let value: u64 = raw[..split].parse()?;
    let suffix = raw[split..].trim().to_ascii_lowercase();
    let multiplier = match suffix.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024_u64.pow(2),
        "g" | "gb" | "gib" => 1024_u64.pow(3),
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        _ => bail!("unknown byte-size suffix {suffix}"),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("byte size overflow"))
}

pub fn parse_env(values: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for value in values {
        let (key, val) = value
            .split_once('=')
            .ok_or_else(|| anyhow!("environment override must be KEY=VALUE"))?;
        if key.is_empty() || key.as_bytes().contains(&0) || val.as_bytes().contains(&0) {
            bail!("invalid environment override");
        }
        out.insert(key.to_owned(), val.to_owned());
    }
    Ok(out)
}

pub fn os_to_utf8(value: &OsStr, what: &str) -> Result<String> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("{what} must be valid UTF-8"))
}

