//! Incremental native-log reader, pagination, follow.

use super::*;

/// Byte-identical to PocketShell's `LINE_TRUNCATION_SENTINEL` so a client
/// that already recognises the marker can render a truncation chip instead
/// of feeding an oversized line to a parser.
pub(crate) const LINE_TRUNCATION_SENTINEL: &str = "@@PS_LINE_TRUNCATED@@";

const FOLLOW_POLL: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------
// Incremental native-log reader, pagination, follow.
// ---------------------------------------------------------------------

/// PocketShell-facing query: last-N initial view, `--before` older page,
/// `--after` catch-up cursor, `--follow` live tail.
#[derive(Debug, Clone, Default)]
pub struct TranscriptQuery {
    pub last: Option<usize>,
    pub kind: Option<String>,
    pub after: Option<u64>,
    pub before: Option<u64>,
    pub follow: bool,
    pub max_line_bytes: Option<usize>,
}

/// Read buffer for streaming a native log; lines are handed out one at a
/// time so a multi-hundred-megabyte transcript never sits in memory whole.
const READ_BUFFER_BYTES: usize = 64 * 1024;

pub(crate) struct NativeLogReader {
    path: PathBuf,
    engine: String,
    format: WireFormat,
    assembler: JsonAssembler,
    sequence: u64,
    /// File offset of the first byte not yet consumed as a complete line.
    /// An unterminated trailing line is left behind this offset while
    /// following, and simply re-read once the writer finishes it.
    byte_offset: u64,
    max_line_bytes: Option<usize>,
}

impl NativeLogReader {
    pub(crate) fn open(engine: &str, path: &Path, max_line_bytes: Option<usize>) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            engine: engine.to_string(),
            format: wire_format_for(engine)?,
            assembler: JsonAssembler::default(),
            sequence: 0,
            byte_offset: 0,
            max_line_bytes,
        })
    }

    fn reset(&mut self) {
        self.assembler = JsonAssembler::default();
        self.sequence = 0;
        self.byte_offset = 0;
    }

    /// Read newly appended complete lines into a vector; see `read_into`.
    pub(crate) fn read_available(
        &mut self,
        record: &SessionRecord,
        consume_tail: bool,
    ) -> Result<Vec<UnifiedEvent>> {
        let mut events = Vec::new();
        self.read_into(record, consume_tail, &mut |event| events.push(event))?;
        Ok(events)
    }

    /// Stream newly appended lines through `sink`, one line in memory at a
    /// time. `consume_tail` is true for a one-shot snapshot (the last line
    /// may lack a trailing newline) and false for `--follow` (wait for the
    /// newline so a mid-write row is not parsed as truncated JSON).
    fn read_into(
        &mut self,
        record: &SessionRecord,
        consume_tail: bool,
        sink: &mut dyn FnMut(UnifiedEvent),
    ) -> Result<()> {
        let mut file =
            File::open(&self.path).with_context(|| format!("read {}", self.path.display()))?;
        if file.metadata()?.len() < self.byte_offset {
            self.reset();
        }
        file.seek(SeekFrom::Start(self.byte_offset))?;
        let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, file);
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader
                .read_until(b'\n', &mut line)
                .with_context(|| format!("read {}", self.path.display()))?;
            if read == 0 {
                return Ok(());
            }
            let terminated = line.last() == Some(&b'\n');
            if !terminated && !consume_tail {
                return Ok(());
            }
            self.byte_offset += read as u64;
            if terminated {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.is_empty() {
                continue;
            }
            self.process_line(&line, record, sink);
        }
    }

    fn process_line(
        &mut self,
        line: &[u8],
        record: &SessionRecord,
        sink: &mut dyn FnMut(UnifiedEvent),
    ) {
        if let Some(max) = self.max_line_bytes {
            if line.len() > max {
                let mut e = ev("error");
                e.error = Some(format!("{LINE_TRUNCATION_SENTINEL}{}", line.len()));
                e.timestamp = iso8601_utc(now_ms());
                sink(self.finish(e, record));
                return;
            }
        }
        let fed = self.assembler.feed(&String::from_utf8_lossy(line));
        if let Some(discarded) = fed.discarded {
            let mut e = ev("error");
            e.error = Some(format!(
                "discarded {discarded} bytes of unbalanced JSON before a new record"
            ));
            e.timestamp = iso8601_utc(now_ms());
            sink(self.finish(e, record));
        }
        let Some(payload) = fed.payload else {
            return;
        };
        let ts = row_timestamp(self.format, &payload);
        let (drafted, _continuation) = translate(self.format, &payload);
        for mut event in drafted {
            event.timestamp = ts.clone();
            event.raw = payload_to_raw(&payload);
            sink(self.finish(event, record));
        }
    }

    /// Stamps the reader-owned fields every emitted event carries: engine,
    /// sequence number, and the session's identity.
    fn finish(&mut self, mut event: UnifiedEvent, record: &SessionRecord) -> UnifiedEvent {
        event.engine = self.engine.clone();
        event.sequence = self.sequence;
        self.sequence += 1;
        stamp_session(&mut event, record);
        event
    }
}

/// Incremental `paginate`: admits `event` to `page` when the query's
/// kind/after/before filters accept it, keeping at most the last `last`
/// admitted events -- so a snapshot of a huge transcript retains only the
/// page it will print.
fn admit(page: &mut VecDeque<UnifiedEvent>, query: &TranscriptQuery, event: UnifiedEvent) {
    if query.kind.as_deref().is_some_and(|kind| event.kind != kind)
        || query.after.is_some_and(|after| event.sequence <= after)
        || query.before.is_some_and(|before| event.sequence >= before)
    {
        return;
    }
    page.push_back(event);
    if query.last.is_some_and(|last| page.len() > last) {
        page.pop_front();
    }
}

pub fn paginate(events: Vec<UnifiedEvent>, query: &TranscriptQuery) -> Vec<UnifiedEvent> {
    let mut page = VecDeque::new();
    for event in events {
        admit(&mut page, query, event);
    }
    page.into()
}

/// The initial page: everything currently in the file, reduced to what the
/// query asks for as it streams past. A one-shot page takes an
/// unterminated last line as-is; a page that will be followed leaves it for
/// the tail loop, so a row split mid-string is parsed whole once the
/// writer finishes it rather than being joined across two polls.
pub(crate) fn snapshot_page(
    reader: &mut NativeLogReader,
    record: &SessionRecord,
    query: &TranscriptQuery,
) -> Result<Vec<UnifiedEvent>> {
    let mut page = VecDeque::new();
    reader.read_into(record, !query.follow, &mut |event| {
        admit(&mut page, query, event)
    })?;
    Ok(page.into())
}

/// One-shot page, or a page followed by a live tail of the same file.
pub fn run_transcript(
    record: &SessionRecord,
    path: &Path,
    query: TranscriptQuery,
    json_output: bool,
) -> Result<()> {
    let mut reader = NativeLogReader::open(&record.engine, path, query.max_line_bytes)?;
    let mut stdout = std::io::stdout();
    let page = snapshot_page(&mut reader, record, &query)?;
    for event in &page {
        if event.emit(&mut stdout, json_output).is_err() {
            return Ok(());
        }
    }
    if !query.follow {
        return Ok(());
    }
    let mut after = page.last().map(|e| e.sequence).or(query.after);
    loop {
        thread::sleep(FOLLOW_POLL);
        let more = reader.read_available(record, false)?;
        let follow_query = TranscriptQuery {
            last: None,
            before: None,
            after,
            kind: query.kind.clone(),
            follow: true,
            max_line_bytes: query.max_line_bytes,
        };
        let page = paginate(more, &follow_query);
        for event in &page {
            if event.emit(&mut stdout, json_output).is_err() {
                return Ok(());
            }
            after = Some(event.sequence);
        }
    }
}

/// Reads and parses one transcript file into `UnifiedEvent`s, in file
/// order, sequence-numbered from 0. Kept for unit tests and any in-process
/// caller that does not need `--follow`.
pub fn read_transcript_events(engine: &str, path: &Path) -> Result<Vec<UnifiedEvent>> {
    let dummy = dummy_record(engine);
    let mut reader = NativeLogReader::open(engine, path, None)?;
    reader.read_available(&dummy, true)
}

pub(crate) fn dummy_record(engine: &str) -> SessionRecord {
    SessionRecord {
        parent_session: None,
        schema_version: crate::SCHEMA_VERSION,
        id: uuid::Uuid::nil(),
        workspace: PathBuf::from("/"),
        tag: "t".into(),
        engine: engine.to_string(),
        profile: None,
        command: vec![engine.to_string()],
        cwd: PathBuf::from("/"),
        env: Default::default(),
        env_unset: Default::default(),
        limits: Default::default(),
        history_bytes: 0,
        created_at_ms: 0,
        updated_at_ms: 0,
        last_activity_ms: None,
        last_accessed_ms: None,
        reported_state: None,
        reported_state_at_ms: None,
        phase: crate::Phase::Running,
        worker_pid: None,
        workload_pid: None,
        worker_cgroup: None,
        workload_cgroup: None,
        containment_cgroup: None,
        containment_cgroup_identity: None,
        containment_empty: Some(false),
        socket_path: PathBuf::from("/"),
        history_path: PathBuf::from("/"),
        exit: None,
        error: None,
    }
}
