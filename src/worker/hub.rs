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

pub(super) struct SecretBytes(pub(super) Vec<u8>);

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub(super) struct LaunchEnvironment(pub(super) BTreeMap<String, String>);

impl Drop for LaunchEnvironment {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            // Overwrite the initialized allocation before String drops it.
            // The temporary non-UTF-8 contents are never observed as text.
            unsafe {
                value.as_bytes_mut().fill(0);
            }
            value.clear();
        }
    }
}

pub(super) struct ConnectionPermit {
    pub(super) active: Arc<AtomicUsize>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) fn try_acquire_connection(active: &Arc<AtomicUsize>) -> Option<ConnectionPermit> {
    active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
            (count < MAX_CLIENT_CONNECTIONS).then_some(count + 1)
        })
        .ok()?;
    Some(ConnectionPermit {
        active: Arc::clone(active),
    })
}

/// Resource pressure must not tear down the worker: doing so also closes the
/// PTY master and can SIGHUP an otherwise healthy workload. Existing client
/// threads may release descriptors while the listener backs off, after which
/// accepting can resume normally.
pub(super) fn transient_accept_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOBUFS) | Some(libc::ENOMEM)
    )
}

pub(super) fn bounded_history_limit(requested: Option<usize>) -> usize {
    requested.unwrap_or(MAX_FRAME_BYTES).min(MAX_FRAME_BYTES)
}

pub(super) fn ensure_frame_payload_size(kind: &str, len: usize) -> Result<()> {
    if len > MAX_FRAME_BYTES {
        bail!("{kind} exceeds the maximum frame size of {MAX_FRAME_BYTES} bytes");
    }
    Ok(())
}

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

pub(super) fn output_event_queued_bytes(event: &OutputEvent) -> usize {
    match event {
        OutputEvent::Data(data) => data.len(),
        // Layout changes are a few bools; Exit/Error are terminal outcomes,
        // never queued behind data.
        _ => 0,
    }
}

pub(super) struct SubscriberState {
    pub(super) queue: VecDeque<OutputEvent>,
    pub(super) queued_bytes: usize,
    pub(super) terminal: Option<OutputEvent>,
    pub(super) terminal_taken: bool,
    pub(super) sender_alive: bool,
    pub(super) receiver_alive: bool,
}

pub(super) struct SubscriberShared {
    pub(super) state: Mutex<SubscriberState>,
    pub(super) cvar: Condvar,
}

impl SubscriberShared {
    pub(super) fn poisoned_lock(&self) -> std::sync::MutexGuard<'_, SubscriberState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(super) struct SubscriberSender {
    pub(super) shared: Arc<SubscriberShared>,
    /// Whether this subscriber asked for the live screen (`AttachPayload::Screen`)
    /// rather than a raw tail. Only screen subscribers are eligible for
    /// backlog coalescing in `OutputHub::append`; tail subscribers keep the
    /// evict-and-reattach contract so `--history-bytes` stays byte-exact.
    pub(super) want_screen: bool,
}

/// What one subscriber did with an offered PTY chunk.
pub(super) enum Offer {
    /// Queued behind its backlog (or silently accepted because a terminal
    /// outcome is already pending and will win once the queue drains).
    Queued,
    /// A live-screen subscriber that fell behind: its backlog was replaced
    /// by the current screen snapshot plus a layout nudge so the client
    /// redraws its status bar (the snapshot's own ED2 blanked the bar row).
    Coalesced,
    /// The receiver is gone, or a raw-tail subscriber exceeded the cap and
    /// was evicted; the hub drops the subscriber either way.
    Dropped,
}

impl SubscriberSender {
    /// Offer one PTY chunk under a single lock hold. The backlog check, the
    /// coalesce-or-evict decision, and the queue update used to take three
    /// separate locks per subscriber per PTY read; here they are one.
    ///
    /// `snapshot` renders the current screen lazily, so the hub pays for it
    /// only when some subscriber actually coalesces, and once per read.
    /// A pending terminal outcome is never disturbed -- it still wins once
    /// the queue drains.
    pub(super) fn offer(&self, data: &Arc<[u8]>, snapshot: &mut dyn FnMut() -> Arc<[u8]>) -> Offer {
        let mut state = self.shared.poisoned_lock();
        if !state.receiver_alive {
            return Offer::Dropped;
        }
        if state.terminal.is_some() {
            return Offer::Queued;
        }
        let queued_bytes = state.queued_bytes.saturating_add(data.len());
        if self.want_screen
            && (queued_bytes > COALESCE_SUBSCRIBER_QUEUED_BYTES
                || state.queue.len() >= COALESCE_SUBSCRIBER_QUEUED_EVENTS)
        {
            let snapshot = snapshot();
            state.queue.clear();
            state.queued_bytes = snapshot.len();
            state.queue.push_back(OutputEvent::Data(snapshot));
            state
                .queue
                .push_back(OutputEvent::Layout(screen::LayoutChange {
                    alt_screen: false,
                    margins_reset: false,
                    erase_reset: true,
                }));
            self.shared.cvar.notify_all();
            return Offer::Coalesced;
        }
        if state.queue.len() >= MAX_SUBSCRIBER_QUEUED_EVENTS
            || queued_bytes > MAX_SUBSCRIBER_QUEUED_BYTES
        {
            state.terminal = Some(OutputEvent::Error(
                "attached client fell behind live output; reattach for a fresh snapshot".into(),
            ));
            self.shared.cvar.notify_all();
            return Offer::Dropped;
        }
        state.queued_bytes = queued_bytes;
        state.queue.push_back(OutputEvent::Data(Arc::clone(data)));
        self.shared.cvar.notify_one();
        Offer::Queued
    }

    pub(super) fn try_event(&self, event: OutputEvent) -> bool {
        let mut state = self.shared.poisoned_lock();
        if !state.receiver_alive {
            return false;
        }
        if state.terminal.is_some() {
            return true;
        }
        let bytes = output_event_queued_bytes(&event);
        if state.queue.len() >= MAX_SUBSCRIBER_QUEUED_EVENTS
            || state.queued_bytes.saturating_add(bytes) > MAX_SUBSCRIBER_QUEUED_BYTES
        {
            state.terminal = Some(OutputEvent::Error(
                "attached client fell behind live output; reattach for a fresh snapshot".into(),
            ));
            self.shared.cvar.notify_all();
            return false;
        }
        state.queued_bytes = state.queued_bytes.saturating_add(bytes);
        state.queue.push_back(event);
        self.shared.cvar.notify_one();
        true
    }

    pub(super) fn terminate(self, event: OutputEvent) {
        {
            let mut state = self.shared.poisoned_lock();
            if state.terminal.is_none() {
                state.terminal = Some(event);
            }
            self.shared.cvar.notify_all();
        }
        // `self` drops here, marking the sender gone (see Drop impl).
    }
}

impl Drop for SubscriberSender {
    fn drop(&mut self) {
        let mut state = self.shared.poisoned_lock();
        state.sender_alive = false;
        self.shared.cvar.notify_all();
    }
}

pub(super) struct OutputReceiver {
    pub(super) shared: Arc<SubscriberShared>,
}

impl OutputReceiver {
    /// Drain already-queued output before reporting the terminal outcome.
    /// Queued data always wins over a terminal event; once the queue is
    /// empty the terminal outcome (if any) is returned exactly once, and
    /// afterwards the receiver reports disconnection -- mirroring the old
    /// two-channel `mpsc` behavior this replaced.
    pub(super) fn recv(&self) -> Result<OutputEvent, mpsc::RecvError> {
        let mut state = self.shared.poisoned_lock();
        loop {
            if let Some(event) = state.queue.pop_front() {
                state.queued_bytes = state
                    .queued_bytes
                    .saturating_sub(output_event_queued_bytes(&event));
                return Ok(event);
            }
            if let Some(terminal) = state.terminal.clone() {
                if !state.terminal_taken {
                    state.terminal_taken = true;
                    return Ok(terminal);
                }
                return Err(mpsc::RecvError);
            }
            if !state.sender_alive {
                return Err(mpsc::RecvError);
            }
            state = self
                .shared
                .cvar
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl Drop for OutputReceiver {
    fn drop(&mut self) {
        let mut state = self.shared.poisoned_lock();
        state.receiver_alive = false;
        self.shared.cvar.notify_all();
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(super) fn test_hub(dir: &tempfile::TempDir) -> OutputHub {
        OutputHub::new(
            History::open(dir.path().join("history.bin"), 1024 * 1024).unwrap(),
            24,
            80,
            dir.path().join("screen.txt"),
        )
        .unwrap()
    }

    #[test]
    pub(super) fn termination_event_blocks_without_timer_and_wakes_on_notification() {
        let fd = create_worker_event_fd("termination").unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            started_tx.send(()).unwrap();
            done_tx.send(wait_for_event_fd(fd, "termination")).unwrap();
        });
        started_rx.recv().unwrap();

        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(75)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        notify_event_fd(fd);
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("eventfd notification did not wake waiter")
            .unwrap();
        waiter.join().unwrap();
        assert_eq!(unsafe { libc::close(fd) }, 0);
    }

    #[test]
    pub(super) fn lifecycle_wait_blocks_until_an_event_before_cleanup_is_needed() {
        let (life_tx, life_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            started_tx.send(()).unwrap();
            let woke_for_event = matches!(
                wait_for_lifecycle_wake(&life_rx, false),
                LifecycleWake::Event(LifeEvent::PtyEof)
            );
            done_tx.send(woke_for_event).unwrap();
        });
        started_rx.recv().unwrap();

        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(75)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        life_tx.send(LifeEvent::PtyEof).unwrap();
        assert!(done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("lifecycle event did not wake waiter"));
        waiter.join().unwrap();
    }

    #[test]
    pub(super) fn lagging_subscriber_is_evicted_when_queue_fills() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();

        // Fill past the ~1 MiB byte cap with max-size PTY reads: 32 x 32 KiB
        // fits exactly, the 33rd exceeds it and evicts.
        let chunk = vec![b'x'; 32 * 1024];
        for _ in 0..33 {
            hub.append(&chunk).unwrap();
        }

        assert!(hub.inner.lock().unwrap().subscribers.is_empty());
        for _ in 0..32 {
            assert!(matches!(rx.recv().unwrap(), OutputEvent::Data(_)));
        }
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Error(message) if message.contains("fell behind")
        ));
    }

    #[test]
    pub(super) fn screen_subscriber_coalesces_to_snapshot_instead_of_replaying() {
        // The reported UX bug: a client whose screen went quiet (background
        // tab, slow link, slept laptop) came back to a 10x fast-forward
        // replay of everything it missed. A live-screen subscriber must jump
        // straight to the current screen instead.
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Screen).unwrap();

        // Two max-size PTY reads fit under the coalescing threshold and
        // stream; the third pushes the backlog past 64 KiB and coalesces.
        let chunk = vec![b'x'; 32 * 1024];
        for _ in 0..3 {
            hub.append(&chunk).unwrap();
        }

        assert!(
            !hub.inner.lock().unwrap().subscribers.is_empty(),
            "a lagging screen subscriber must coalesce, not be evicted"
        );
        // The backlog was replaced: the first thing out is the live snapshot
        // (which already includes all three chunks), followed by the layout
        // nudge that redraws the status bar the snapshot's ED2 blanked --
        // not two 32 KiB replays and not a "fell behind" error.
        let first = rx.recv().unwrap();
        let OutputEvent::Data(snapshot) = first else {
            panic!("expected coalesced snapshot Data, got something else");
        };
        assert_eq!(&snapshot[..], &hub.screen_snapshot().unwrap()[..]);
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Layout(change) if change.erase_reset
        ));
    }

    #[test]
    pub(super) fn screen_subscriber_coalesces_on_event_count_not_just_bytes() {
        // A pathological stream of tiny writes would previously evict even a
        // screen subscriber at 1024 queued events. It must coalesce instead
        // and stay attached; the tail path keeps the old eviction contract
        // (see event_cap_still_bounds_a_pathological_stream_of_tiny_writes).
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Screen).unwrap();

        for _ in 0..=MAX_SUBSCRIBER_QUEUED_EVENTS {
            hub.append(b"y").unwrap();
        }

        assert!(
            !hub.inner.lock().unwrap().subscribers.is_empty(),
            "screen subscriber must survive a tiny-write flood via coalescing"
        );
        let first = rx.recv().unwrap();
        let OutputEvent::Data(snapshot) = first else {
            panic!("expected coalesced snapshot Data");
        };
        // A raw 1-byte write never starts with ESC; a vt100 snapshot always
        // does (attribute reset / clear). This distinguishes "jumped to live"
        // from "replaying the 1-byte writes one by one".
        assert!(
            snapshot.first() == Some(&0x1b),
            "first frame should be a snapshot repaint, got {} bytes starting with {:?}",
            snapshot.len(),
            snapshot.first()
        );
    }

    #[test]
    pub(super) fn tail_subscriber_does_not_coalesce_small_backlog() {
        // `--history-bytes` stays byte-exact: the same backlog that coalesces
        // a screen subscriber must stream verbatim for a tail subscriber.
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();

        let chunk = vec![b'x'; 32 * 1024];
        for _ in 0..3 {
            hub.append(&chunk).unwrap();
        }

        assert!(!hub.inner.lock().unwrap().subscribers.is_empty());
        for _ in 0..3 {
            let event = rx.recv().unwrap();
            let OutputEvent::Data(data) = event else {
                panic!("tail subscriber must replay raw bytes, not a snapshot");
            };
            assert_eq!(&data[..], &chunk[..]);
        }
    }

    #[test]
    pub(super) fn event_cap_still_bounds_a_pathological_stream_of_tiny_writes() {
        // The byte cap alone would let a stream of 1-byte writes queue
        // megabytes of per-event overhead; the event cap bounds that.
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();

        for _ in 0..=MAX_SUBSCRIBER_QUEUED_EVENTS {
            hub.append(b"x").unwrap();
        }

        assert!(hub.inner.lock().unwrap().subscribers.is_empty());
        for _ in 0..MAX_SUBSCRIBER_QUEUED_EVENTS {
            assert!(matches!(rx.recv().unwrap(), OutputEvent::Data(_)));
        }
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Error(message) if message.contains("fell behind")
        ));
    }

    #[test]
    pub(super) fn small_burst_of_many_tiny_writes_does_not_evict_subscriber() {
        // Regression test for attach spontaneously detaching on busy
        // sessions (e.g. data-engineering-zoomcamp / machine-learning-zoomcamp
        // codex TUIs): a resize-triggered repaint arrives as dozens of small
        // PTY reads (a few hundred bytes each, ~30KB total). The old
        // event-count-only queue (32 events) evicted the just-attached client
        // even though the backlog was tiny, and `a attach` printed
        // "attached client fell behind live output" and detached. A burst
        // that is small in bytes must not evict, no matter how many events
        // it takes.
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();

        let chunk = vec![b'x'; 200];
        for _ in 0..200 {
            hub.append(&chunk).unwrap();
        }

        assert!(
            !hub.inner.lock().unwrap().subscribers.is_empty(),
            "a 40KB burst of small writes evicted the subscriber"
        );
        for _ in 0..200 {
            assert!(matches!(rx.recv().unwrap(), OutputEvent::Data(_)));
        }
    }

    #[test]
    pub(super) fn full_subscriber_queue_drains_before_explicit_exit() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();
        // A full (but not over-full) queue: 32 x 32 KiB == the 1 MiB byte
        // cap exactly, so nothing is evicted and `finish` must still deliver
        // every queued byte before the Exit.
        let chunk = vec![b'x'; 32 * 1024];
        for _ in 0..32 {
            hub.append(&chunk).unwrap();
        }
        assert!(!hub.inner.lock().unwrap().subscribers.is_empty());
        let exit = ExitInfo {
            code: Some(0),
            signal: None,
            oom_killed: false,
            exited_at_ms: 1,
        };
        hub.finish(exit.clone());

        for _ in 0..32 {
            assert!(matches!(rx.recv().unwrap(), OutputEvent::Data(_)));
        }
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Exit(received) if received.code == exit.code
        ));
    }

    #[test]
    pub(super) fn full_subscriber_queue_drains_before_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();
        let chunk = vec![b'x'; 32 * 1024];
        for _ in 0..32 {
            hub.append(&chunk).unwrap();
        }
        hub.fail_subscribers("PTY failed".into());

        for _ in 0..32 {
            assert!(matches!(rx.recv().unwrap(), OutputEvent::Data(_)));
        }
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Error(message) if message == "PTY failed"
        ));
    }

    #[test]
    pub(super) fn subscriber_count_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let mut receivers = Vec::new();
        for _ in 0..MAX_SUBSCRIBERS {
            let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();
            receivers.push(rx);
        }

        assert!(hub.subscribe(AttachPayload::Tail(None)).is_err());
        assert_eq!(receivers.len(), MAX_SUBSCRIBERS);
    }

    #[test]
    pub(super) fn history_capture_limits_cannot_exceed_one_frame() {
        assert_eq!(bounded_history_limit(None), MAX_FRAME_BYTES);
        assert_eq!(bounded_history_limit(Some(123)), 123);
        assert_eq!(bounded_history_limit(Some(usize::MAX)), MAX_FRAME_BYTES);
        assert!(ensure_frame_payload_size("test", MAX_FRAME_BYTES).is_ok());
        assert!(ensure_frame_payload_size("test", MAX_FRAME_BYTES + 1).is_err());
    }

    #[test]
    pub(super) fn history_failure_does_not_interrupt_live_output_and_can_recover() {
        let dir = tempfile::tempdir().unwrap();
        let history_path = dir.path().join("history.bin");
        let hub = OutputHub::new(
            History::open(history_path.clone(), 1024).unwrap(),
            24,
            80,
            dir.path().join("screen.txt"),
        )
        .unwrap();
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();

        hub.append(b"still-live").unwrap();
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Data(data) if &data[..] == b"still-live"
        ));
        assert_eq!(hub.snapshot(None).unwrap(), b"still-live");
        let blocked_bank = dir.path().join("history.bin.v2.data.0");
        fs::create_dir(&blocked_bank).unwrap();
        assert!(hub.flush_history(true).is_err());
        assert!(hub.history_persistence_error().is_some());
        assert_eq!(fs::read(&history_path).unwrap(), b"still-live");

        fs::remove_dir(&blocked_bank).unwrap();
        hub.flush_history(true).unwrap();
        assert!(hub.history_persistence_error().is_none());
        assert_eq!(fs::read(&history_path).unwrap(), b"still-live");
        assert_eq!(
            read_persisted_history_tail(&history_path, None).unwrap(),
            b"still-live"
        );
    }

    #[test]
    pub(super) fn history_append_failure_does_not_drop_subscribers_or_stop_live_output() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let (_, _, rx) = hub.subscribe(AttachPayload::Tail(None)).unwrap();

        hub.inject_history_append_failure(libc::ENOSPC);
        hub.append(b"still-live").unwrap();

        let error = hub
            .history_persistence_error()
            .expect("append failure must set history_persistence_error");
        assert!(
            error.contains("No space left on device") || error.contains("os error 28"),
            "unexpected history persistence error: {error}"
        );
        assert!(
            !hub.inner.lock().unwrap().subscribers.is_empty(),
            "history append failure must not drop attached clients"
        );
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Data(data) if &data[..] == b"still-live"
        ));
        assert_eq!(
            hub.inner.lock().unwrap().history_retry_delay,
            HISTORY_RETRY_INITIAL
                .saturating_mul(2)
                .min(HISTORY_RETRY_MAX),
            "append failure must use the same capped backoff as flush"
        );

        hub.append(b"and-more").unwrap();
        assert!(matches!(
            rx.recv().unwrap(),
            OutputEvent::Data(data) if &data[..] == b"and-more"
        ));
        assert!(
            !hub.inner.lock().unwrap().subscribers.is_empty(),
            "a later chunk after append failure must still fan out"
        );
        assert!(hub.history_persistence_error().is_some());
    }

    #[test]
    pub(super) fn failed_pty_resize_restores_the_previous_screen_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let hub = test_hub(&dir);
        let before = hub.screen_snapshot().unwrap();

        let error = resize_screen_and_pty(&hub, (24, 80), (10, 20), || {
            bail!("injected PTY ioctl failure")
        })
        .unwrap_err();

        assert_eq!(error.to_string(), "injected PTY ioctl failure");
        assert_eq!(
            hub.screen_snapshot().unwrap(),
            before,
            "failed PTY resize left the screen model at the rejected size"
        );
    }

    #[test]
    pub(super) fn connection_permits_are_bounded_and_release_on_drop() {
        let active = Arc::new(AtomicUsize::new(0));
        let permits: Vec<_> = (0..MAX_CLIENT_CONNECTIONS)
            .map(|_| try_acquire_connection(&active).expect("permit below limit"))
            .collect();
        assert!(try_acquire_connection(&active).is_none());
        assert_eq!(active.load(Ordering::Acquire), MAX_CLIENT_CONNECTIONS);

        drop(permits);
        assert_eq!(active.load(Ordering::Acquire), 0);
        assert!(try_acquire_connection(&active).is_some());

        let _ = std::panic::catch_unwind({
            let active = Arc::clone(&active);
            move || {
                let _permit = try_acquire_connection(&active).unwrap();
                panic!("exercise unwind cleanup");
            }
        });
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    pub(super) fn descriptor_pressure_accept_errors_are_retriable() {
        for errno in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
            assert!(transient_accept_error(&io::Error::from_raw_os_error(errno)));
        }
        assert!(!transient_accept_error(&io::Error::from_raw_os_error(
            libc::EBADF
        )));
    }

    /// A `WorkerRuntime` over a throwaway hub, with its durable record at
    /// `record_path` and every other path under `dir`.
    pub(super) fn test_runtime(
        dir: &tempfile::TempDir,
        record_path: std::path::PathBuf,
    ) -> WorkerRuntime {
        let mut record = SessionRecord::fixture(dir.path(), "before");
        record.socket_path = dir.path().join("control.sock");
        record.history_path = dir.path().join("history.bin");
        WorkerRuntime {
            id: record.id,
            paths: Paths {
                runtime_root: dir.path().join("runtime"),
                state_root: dir.path().join("state"),
                config_file: dir.path().join("config.toml"),
            },
            record_path,
            runtime_session_dir: dir.path().join("runtime-session"),
            socket_path: dir.path().join("control.sock"),
            record: Mutex::new(record),
            pty_write: Mutex::new(Some(Arc::new(File::open("/dev/null").unwrap()))),
            workload: Mutex::new(WorkloadState {
                running: true,
                pgid: 1,
            }),
            terminal: Mutex::new(TerminalState {
                rows: 24,
                cols: 80,
                clients: HashMap::new(),
                next_client_id: 1,
                activity_clock: 0,
            }),
            cgroup: Mutex::new(None),
            kill_gate: Mutex::new(()),
            output: test_hub(dir),
            record_persistence_error: Mutex::new(None),
            active_connections: Arc::new(AtomicUsize::new(0)),
            last_activity_ms: AtomicU64::new(0),
        }
    }

    #[test]
    pub(super) fn failed_record_persistence_does_not_publish_and_idle_activity_retries() {
        let dir = tempfile::tempdir().unwrap();
        let record_path = dir.path().join("session.json");
        // Atomic rename onto a directory deterministically fails after the
        // candidate was serialized, exercising the publish boundary.
        fs::create_dir(&record_path).unwrap();
        let runtime = test_runtime(&dir, record_path);

        assert!(runtime
            .update_record(|candidate| candidate.tag = "after".into())
            .is_err());
        assert_eq!(runtime.record().unwrap().tag, "before");
        assert!(runtime.record_persistence_error.lock().unwrap().is_some());

        runtime.last_activity_ms.store(123, Ordering::Relaxed);
        let mut persisted_activity_ms = 0;
        assert!(persist_activity_checkpoint(&runtime, &mut persisted_activity_ms).is_err());
        assert_eq!(persisted_activity_ms, 0, "failed write advanced checkpoint");
        assert_eq!(runtime.record().unwrap().last_activity_ms, None);

        // No new activity occurs between attempts. Once the transient
        // destination failure is removed, the unchanged timestamp must still
        // be retried and published by the next tick.
        fs::remove_dir(&runtime.record_path).unwrap();
        persist_activity_checkpoint(&runtime, &mut persisted_activity_ms).unwrap();
        assert_eq!(persisted_activity_ms, 123);
        assert_eq!(runtime.record().unwrap().last_activity_ms, Some(123));
        assert!(runtime.record_persistence_error.lock().unwrap().is_none());
    }

    /// The clean-exit resurrection: `run_lifecycle` removes the state dir,
    /// then drains connections for up to 3 s before exiting, and in that
    /// window the periodic flush thread and a late attach both wrote into
    /// the removed directory (`atomic_write_*` recreates parents), leaving
    /// a `phase: exiting` record with a dead worker pid for `a list` to
    /// show as broken until `a prune`. Pins that once the lifecycle marks
    /// the session finalized, neither the record writer nor the history
    /// flusher recreates anything -- whether the write is an activity
    /// checkpoint, an attach stamp, or a forced flush.
    #[test]
    pub(super) fn finalized_session_refuses_every_later_durable_write() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state-session");
        let runtime = test_runtime(&dir, state_dir.join("session.json"));
        // Sanity: before finalization the same writers land on disk.
        runtime
            .update_record(|record| record.tag = "live".into())
            .expect("record write before finalization");
        runtime.output.append(b"output").unwrap();
        runtime.output.flush_history(true).unwrap();
        assert!(runtime.record_path.exists());
        assert!(dir.path().join("history.bin").exists());

        runtime.mark_finalized().unwrap();
        fs::remove_dir_all(&state_dir).unwrap();
        fs::remove_file(dir.path().join("history.bin")).unwrap();

        let error = runtime
            .update_record(|record| record.last_accessed_ms = Some(now_ms()))
            .expect_err("a finalized session must refuse record writes");
        assert!(format!("{error:#}").contains("finalized"), "{error:#}");
        assert!(
            runtime.record_persistence_error.lock().unwrap().is_none(),
            "a refused post-finalization write is not a persistence failure"
        );
        runtime.last_activity_ms.store(now_ms(), Ordering::Relaxed);
        let mut persisted_activity_ms = 0;
        persist_activity_checkpoint(&runtime, &mut persisted_activity_ms)
            .expect("the activity checkpoint quietly skips a finalized session");
        runtime.output.append(b"late output").unwrap();
        runtime.output.flush_history(false).unwrap();
        runtime.output.flush_history(true).unwrap();

        assert!(
            !state_dir.exists(),
            "a durable write after finalization resurrected the state dir"
        );
        assert!(
            !dir.path().join("history.bin").exists(),
            "a history flush after finalization resurrected the history file"
        );
    }

    /// `a start` holds the registry lock for its whole spawn-and-poll; a
    /// rename that blocked on it outlived the client's control deadline
    /// and then applied unobserved. It must refuse instead, in time, with
    /// an error that says the registry is busy and nothing changed.
    #[test]
    pub(super) fn rename_refuses_in_time_while_the_registry_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = test_runtime(&dir, dir.path().join("session.json"));
        runtime.paths.ensure().unwrap();
        let _held = FileLock::exclusive(&runtime.paths.registry_lock(), true).unwrap();

        let started = Instant::now();
        let error = runtime
            .rename(dir.path().to_path_buf(), "renamed".into())
            .expect_err("rename must not wait out a held registry lock");
        let elapsed = started.elapsed();
        assert!(
            format!("{error:#}").contains("registry is busy"),
            "{error:#}"
        );
        assert!(
            elapsed >= RENAME_REGISTRY_WAIT
                && elapsed < RENAME_REGISTRY_WAIT + Duration::from_secs(1),
            "rename gave up after {elapsed:?}, expected about {RENAME_REGISTRY_WAIT:?}"
        );
        assert_eq!(runtime.record().unwrap().tag, "before");
        assert!(
            !runtime.record_path.exists(),
            "a refused rename wrote the record"
        );
    }

    #[test]
    pub(super) fn startup_history_node_must_be_regular_or_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.bin");
        assert!(validate_existing_history_node(&missing).is_ok());

        let regular = dir.path().join("regular.bin");
        fs::write(&regular, b"history").unwrap();
        assert!(validate_existing_history_node(&regular).is_ok());

        let directory = dir.path().join("directory.bin");
        fs::create_dir(&directory).unwrap();
        assert!(validate_existing_history_node(&directory).is_err());

        let symlink = dir.path().join("symlink.bin");
        std::os::unix::fs::symlink(&regular, &symlink).unwrap();
        assert!(validate_existing_history_node(&symlink).is_err());

        let fifo = dir.path().join("fifo.bin");
        let fifo_c = c_string(&fifo).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(validate_existing_history_node(&fifo).is_err());
    }
}
