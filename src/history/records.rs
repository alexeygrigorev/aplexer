use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryMarker {
    pub(crate) format_version: u32,
    pub(crate) store_id: Uuid,
    pub(crate) session_id: Uuid,
    pub(crate) metadata_sha256: String,
}

impl HistoryMarker {
    pub(crate) fn seal(mut self) -> Result<Self> {
        self.metadata_sha256.clear();
        self.metadata_sha256 = sha256_hex(&serde_json::to_vec(&self)?);
        Ok(self)
    }

    pub(crate) fn validate(&self, path: &Path) -> Result<()> {
        if self.format_version != HISTORY_FORMAT_VERSION {
            bail!("unsupported history marker format {}", self.format_version);
        }
        if self.store_id.is_nil() {
            bail!("history marker has a nil store id");
        }
        if self.session_id != history_session_id(path) {
            bail!("history marker belongs to a different session");
        }
        let mut unsigned = self.clone();
        let supplied = std::mem::take(&mut unsigned.metadata_sha256);
        let expected = sha256_hex(&serde_json::to_vec(&unsigned)?);
        if supplied != expected {
            bail!("history marker metadata checksum mismatch");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct HistoryCommit {
    pub(crate) format_version: u32,
    pub(crate) store_id: Uuid,
    pub(crate) session_id: Uuid,
    pub(crate) commit_generation: u64,
    pub(crate) bank_generation: u64,
    pub(crate) data_slot: u8,
    pub(crate) capacity: u64,
    pub(crate) committed_len: u64,
    pub(crate) stream_end: u64,
    pub(crate) data_sha256: String,
    pub(crate) metadata_sha256: String,
}

impl HistoryCommit {
    pub(crate) fn seal(mut self) -> Result<Self> {
        self.metadata_sha256.clear();
        self.metadata_sha256 = sha256_hex(&serde_json::to_vec(&self)?);
        Ok(self)
    }

    pub(crate) fn validate(&self, path: &Path, metadata_slot: u8) -> Result<()> {
        if self.format_version != HISTORY_FORMAT_VERSION {
            bail!("unsupported history format {}", self.format_version);
        }
        if self.data_slot >= HISTORY_BANK_COUNT {
            bail!("history commit names invalid data slot {}", self.data_slot);
        }
        if self.commit_generation % HISTORY_COMMIT_COUNT as u64 != metadata_slot as u64 {
            bail!("history commit is stored in the wrong metadata slot");
        }
        if self.session_id != history_session_id(path) {
            bail!("history commit belongs to a different session");
        }
        if self.commit_generation == 0 || self.bank_generation == 0 {
            bail!("history generation counters must be positive");
        }
        let capacity = usize::try_from(self.capacity).context("history capacity does not fit")?;
        validate_history_bytes(capacity)?;
        let max_payload = self
            .capacity
            .checked_mul(2)
            .ok_or_else(|| anyhow!("history bank size overflow"))?;
        if self.committed_len > max_payload {
            bail!("history committed length exceeds its bounded bank size");
        }
        if self.committed_len > self.stream_end {
            bail!("history committed length exceeds its logical stream position");
        }
        if self.data_sha256.len() != 64
            || !self
                .data_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            bail!("history commit has an invalid data checksum");
        }
        let mut unsigned = self.clone();
        let supplied = std::mem::take(&mut unsigned.metadata_sha256);
        let expected = sha256_hex(&serde_json::to_vec(&unsigned)?);
        if supplied != expected {
            bail!("history commit metadata checksum mismatch");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct HistoryBankHeader {
    pub(crate) store_id: Uuid,
    pub(crate) session_id: Uuid,
    pub(crate) bank_generation: u64,
    pub(crate) data_slot: u8,
    pub(crate) capacity: u64,
}

impl HistoryBankHeader {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HISTORY_BANK_HEADER_BYTES);
        bytes.extend_from_slice(HISTORY_BANK_MAGIC);
        bytes.extend_from_slice(&HISTORY_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(HISTORY_BANK_HEADER_BYTES as u32).to_le_bytes());
        bytes.extend_from_slice(self.store_id.as_bytes());
        bytes.extend_from_slice(self.session_id.as_bytes());
        bytes.extend_from_slice(&self.bank_generation.to_le_bytes());
        bytes.push(self.data_slot);
        bytes.extend_from_slice(&[0; 7]);
        bytes.extend_from_slice(&self.capacity.to_le_bytes());
        debug_assert_eq!(bytes.len(), HISTORY_BANK_HEADER_PREFIX_BYTES);
        let checksum = Sha256::digest(&bytes);
        bytes.extend_from_slice(&checksum);
        bytes
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != HISTORY_BANK_HEADER_BYTES {
            bail!("history bank header has the wrong length");
        }
        if &bytes[..8] != HISTORY_BANK_MAGIC {
            bail!("history bank magic mismatch");
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if version != HISTORY_FORMAT_VERSION {
            bail!("unsupported history bank format {version}");
        }
        let header_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        if header_len != HISTORY_BANK_HEADER_BYTES {
            bail!("history bank header length is invalid");
        }
        let expected = Sha256::digest(&bytes[..HISTORY_BANK_HEADER_PREFIX_BYTES]);
        if expected.as_slice() != &bytes[HISTORY_BANK_HEADER_PREFIX_BYTES..] {
            bail!("history bank header checksum mismatch");
        }
        let store_id = Uuid::from_slice(&bytes[16..32]).context("parse history store id")?;
        let session_id = Uuid::from_slice(&bytes[32..48]).context("parse history session id")?;
        let bank_generation = u64::from_le_bytes(bytes[48..56].try_into().unwrap());
        let data_slot = bytes[56];
        if bytes[57..64].iter().any(|byte| *byte != 0) {
            bail!("history bank reserved header bytes are nonzero");
        }
        let capacity = u64::from_le_bytes(bytes[64..72].try_into().unwrap());
        Ok(Self {
            store_id,
            session_id,
            bank_generation,
            data_slot,
            capacity,
        })
    }
}
