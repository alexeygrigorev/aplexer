//! The bounded output fan-out: the OutputHub every worker owns, and the
//! subscriber ends attached clients read from.
//!
//! One reason to exist: deciding what a lagging reader deserves. Queues are
//! bounded by bytes first and events second; a raw-tail subscriber that
//! exceeds the bound is evicted (it can reattach for a fresh tail), while a
//! live-screen subscriber's backlog is replaced by a fresh screen snapshot
//! so it jumps to live instead of replaying. The constants and the reason
//! for each bound live next to the machinery that enforces them.

use super::*;

mod guards;
mod subscriber;
#[cfg(test)]
pub(super) mod tests;

pub(in crate::worker) use guards::*;
pub(in crate::worker) use subscriber::*;

/// What a newly-attaching client should be sent as its initial payload
/// (design doc section 6.1/checklist item 4): the live screen snapshot, or
/// the historical raw-tail replay old clients (and `--history-bytes`) still
/// get.
pub(super) enum AttachPayload {
    Screen,
    Tail(Option<usize>),
}

pub(super) struct HubInner {
    pub(super) history: History,
    pub(super) history_persistence_error: Option<String>,
    pub(super) history_retry_at: Instant,
    pub(super) history_retry_delay: Duration,
    pub(super) screen: screen::ScreenTracker,
    pub(super) subscribers: HashMap<u64, SubscriberSender>,
    pub(super) next_id: u64,
    pub(super) terminal: Option<OutputEvent>,
}

/// Note a failed history write and schedule the next attempt with capped
/// exponential backoff; returns the message Status reports meanwhile.
pub(super) fn record_history_persistence_error(
    inner: &mut HubInner,
    error: impl std::fmt::Display,
) -> String {
    let message = format!("{error:#}");
    inner.history_persistence_error = Some(message.clone());
    inner.history_retry_at = Instant::now() + inner.history_retry_delay;
    inner.history_retry_delay = inner
        .history_retry_delay
        .saturating_mul(2)
        .min(HISTORY_RETRY_MAX);
    message
}

pub(super) fn output_event_queued_bytes(event: &OutputEvent) -> usize {
    match event {
        OutputEvent::Data(data) => data.len(),
        // Layout changes are a few bools; Exit/Error are terminal outcomes,
        // never queued behind data.
        _ => 0,
    }
}

pub(super) struct OutputHub {
    pub(super) inner: Mutex<HubInner>,
    /// Where `finish` writes the final plain-text screen on exit (design
    /// doc section 5.5) -- immutable, so no need to route it through the
    /// lock.
    pub(super) screen_txt_path: std::path::PathBuf,
    /// Set by `WorkerRuntime::mark_finalized` the moment the lifecycle
    /// decides to remove the session's durable state, under both the hub
    /// lock and the record lock. Every durable writer -- history flushes
    /// here, record updates in `WorkerRuntime::update_record` -- checks it
    /// under the same lock it writes under, so nothing that starts after
    /// the flag is set can recreate the removed directory. Without it the
    /// periodic flush thread kept running for the connection-drain window
    /// after `remove_dir_all` (`atomic_write_*` recreates parents), leaving
    /// a `session.json` with `phase: exiting` and a dead worker pid behind
    /// for every clean exit `a list` then showed as broken until `a prune`.
    pub(super) finalized: AtomicBool,
}
impl OutputHub {
    pub(super) fn new(
        history: History,
        rows: u16,
        cols: u16,
        screen_txt_path: std::path::PathBuf,
    ) -> Result<Self> {
        Ok(Self {
            inner: Mutex::new(HubInner {
                history,
                history_persistence_error: None,
                history_retry_at: Instant::now(),
                history_retry_delay: HISTORY_RETRY_INITIAL,
                screen: screen::ScreenTracker::try_new(rows, cols)?,
                subscribers: HashMap::new(),
                next_id: 1,
                terminal: None,
            }),
            screen_txt_path,
            finalized: AtomicBool::new(false),
        })
    }
    /// Whether the session's durable state is being (or has been) removed.
    pub(super) fn finalized(&self) -> bool {
        self.finalized.load(Ordering::SeqCst)
    }
    pub(super) fn append(&self, data: &[u8]) -> Result<()> {
        let mut inner = lock(&self.inner)?;
        if let Err(error) = inner.history.append(data) {
            // History is best-effort: a full disk must not look like a
            // broken PTY. Live fan-out and the screen model still see this
            // chunk; Status keeps using history_persistence_error.
            record_history_persistence_error(&mut inner, error);
        }
        let layout = inner.screen.process(data);
        // Ordering matters and is automatic: both sends go through the same
        // per-subscriber queue under the same lock hold, so a Layout event
        // always arrives after the Data frame that caused it (design doc
        // section 5.1).
        //
        // A screen subscriber that fell behind does not get a fast-forward
        // replay of everything it missed: its backlog is replaced by the
        // current screen snapshot, which already includes this chunk (the
        // model above processed it before the snapshot is rendered). Skipped
        // bytes are not lost -- the history ring keeps them for `a capture`.
        //
        // One allocation for the chunk, shared by every queue it lands in;
        // one snapshot render at most, on the first subscriber that needs
        // it. Destructured so the lazy render borrows `screen` while the
        // retain borrows `subscribers`.
        let HubInner {
            screen,
            subscribers,
            ..
        } = &mut *inner;
        let data: Arc<[u8]> = Arc::from(data);
        let mut snapshot: Option<Arc<[u8]>> = None;
        let mut render =
            || Arc::clone(snapshot.get_or_insert_with(|| Arc::from(screen.snapshot())));
        // Ids coalesced on the Data phase skip the Layout phase below: the
        // snapshot carries an erase nudge of its own, and the per-chunk
        // Layout would just be a redundant extra event behind it.
        let mut coalesced: Vec<u64> = Vec::new();
        subscribers.retain(
            |id, subscriber| match subscriber.offer(&data, &mut render) {
                Offer::Queued => true,
                Offer::Coalesced => {
                    coalesced.push(*id);
                    true
                }
                Offer::Dropped => false,
            },
        );
        if let Some(change) = layout {
            subscribers.retain(|id, subscriber| {
                if coalesced.contains(id) {
                    return true;
                }
                subscriber.try_event(OutputEvent::Layout(change))
            });
        }
        Ok(())
    }
    pub(super) fn snapshot(&self, max: Option<usize>) -> Result<Vec<u8>> {
        Ok(lock(&self.inner)?
            .history
            .snapshot(Some(bounded_history_limit(max))))
    }
    /// The rendered current-screen snapshot (design doc section 6.2),
    /// shared by attach's `AttachPayload::Screen` and
    /// `Operation::CaptureScreen { plain: false }`.
    pub(super) fn screen_snapshot(&self) -> Result<Vec<u8>> {
        let data = lock(&self.inner)?.screen.snapshot();
        ensure_frame_payload_size("screen snapshot", data.len())?;
        Ok(data)
    }
    /// Plain text of the current screen (design doc section 8), for
    /// `Operation::CaptureScreen { plain: true }`.
    pub(super) fn screen_contents(&self) -> Result<String> {
        let data = lock(&self.inner)?.screen.contents();
        ensure_frame_payload_size("plain screen capture", data.len())?;
        Ok(data)
    }
    /// Resizes the live screen model; called by `WorkerRuntime::resize`
    /// before the PTY ioctl (design doc section 5.3).
    pub(super) fn set_size(&self, rows: u16, cols: u16) -> Result<()> {
        lock(&self.inner)?.screen.try_set_size(rows, cols)
    }
    /// Persists dirty history without ever coupling failure back into PTY
    /// delivery. Repeated failures use capped backoff; `force` is reserved
    /// for the final lifecycle attempt.
    pub(super) fn flush(&self) -> Result<()> {
        self.flush_history(false)
    }
    pub(super) fn flush_history(&self, force: bool) -> Result<()> {
        let mut inner = lock(&self.inner)?;
        // Checked under the lock the flush writes under (see `finalized`).
        if self.finalized() {
            return Ok(());
        }
        if !force && Instant::now() < inner.history_retry_at {
            return Ok(());
        }
        match inner.history.flush() {
            Ok(()) => {
                inner.history_persistence_error = None;
                inner.history_retry_at = Instant::now();
                inner.history_retry_delay = HISTORY_RETRY_INITIAL;
                Ok(())
            }
            Err(error) => {
                let message = record_history_persistence_error(&mut inner, error);
                Err(anyhow!(message))
            }
        }
    }
    pub(super) fn history_persistence_error(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| inner.history_persistence_error.clone())
    }
    pub(super) fn subscribe(
        &self,
        payload: AttachPayload,
    ) -> Result<(u64, Vec<u8>, OutputReceiver)> {
        let mut inner = lock(&self.inner)?;
        let want_screen = matches!(payload, AttachPayload::Screen);
        let initial = match payload {
            AttachPayload::Screen => {
                let data = inner.screen.snapshot();
                ensure_frame_payload_size("screen snapshot", data.len())?;
                data
            }
            AttachPayload::Tail(max) => inner.history.snapshot(Some(bounded_history_limit(max))),
        };
        if inner.terminal.is_none() && inner.subscribers.len() >= MAX_SUBSCRIBERS {
            bail!("too many attached clients");
        }
        let id = inner.next_id;
        inner.next_id += 1;
        let shared = Arc::new(SubscriberShared {
            state: Mutex::new(SubscriberState {
                queue: VecDeque::new(),
                queued_bytes: 0,
                terminal: None,
                terminal_taken: false,
                sender_alive: true,
                receiver_alive: true,
            }),
            cvar: Condvar::new(),
        });
        let subscriber = SubscriberSender {
            shared: Arc::clone(&shared),
            want_screen,
        };
        let receiver = OutputReceiver { shared };
        if let Some(terminal) = inner.terminal.clone() {
            subscriber.terminate(terminal);
        } else {
            inner.subscribers.insert(id, subscriber);
        }
        Ok((id, initial, receiver))
    }
    pub(super) fn unsubscribe(&self, id: u64) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.subscribers.remove(&id);
        }
    }
    /// Record the terminal outcome (an outcome already recorded wins) and
    /// hand it to every subscriber, running `persist` in between under the
    /// same lock so post-mortem files are written before any client learns
    /// the session is over.
    fn terminate_all(&self, event: OutputEvent, persist: impl FnOnce(&mut HubInner)) {
        if let Ok(mut inner) = self.inner.lock() {
            let terminal = inner.terminal.get_or_insert(event).clone();
            persist(&mut inner);
            for (_, subscriber) in inner.subscribers.drain() {
                subscriber.terminate(terminal.clone());
            }
        }
    }
    pub(super) fn finish(&self, exit: ExitInfo) {
        self.terminate_all(OutputEvent::Exit(exit), |inner| {
            if let Err(error) = inner.history.flush_final() {
                eprintln!("aplexer worker: flush history at exit: {error:#}");
                inner.history_persistence_error = Some(format!("{error:#}"));
            }
            // Cheap post-mortem "what was on screen when it died" fallback
            // for `a capture --screen` on a dead session (design doc
            // section 5.5) -- the live grid itself dies with the worker,
            // this is the durable trace of it. Best-effort: a failure here
            // must not stop the exit event from reaching subscribers.
            if let Err(error) = fs::write(&self.screen_txt_path, inner.screen.contents()) {
                eprintln!("aplexer worker: write screen.txt at exit: {error:#}");
            }
        });
    }
    /// Terminate subscribers without any durable writes, for sessions the
    /// worker is about to delete (benchmark PLAN P0.2): `run_lifecycle`
    /// removes the whole state dir moments later, so `finish`'s history
    /// `flush_final` (fsync) plus `screen.txt` write are fsync-and-delete
    /// waste. The Exit event still reaches attached clients; only the
    /// post-mortem files are skipped. Called only on the clean-and-proven
    /// removal path (natural exit, Ctrl-D / shell EOF, or `a kill`); any
    /// failure or OOM keeps the evidence via the regular `finish` path.
    pub(super) fn finish_killed(&self, exit: ExitInfo) {
        self.terminate_all(OutputEvent::Exit(exit), |_| {});
    }
    pub(super) fn fail_subscribers(&self, message: String) {
        self.terminate_all(OutputEvent::Error(message), |_| {});
    }
    #[cfg(test)]
    pub(super) fn inject_history_append_failure(&self, errno: i32) {
        lock(&self.inner)
            .expect("output hub lock")
            .history
            .inject_append_failure(errno);
    }
}
