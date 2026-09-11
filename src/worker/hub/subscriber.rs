use super::*;

pub(in crate::worker) struct SubscriberState {
    pub(in crate::worker) queue: VecDeque<OutputEvent>,
    pub(in crate::worker) queued_bytes: usize,
    pub(in crate::worker) terminal: Option<OutputEvent>,
    pub(in crate::worker) terminal_taken: bool,
    pub(in crate::worker) sender_alive: bool,
    pub(in crate::worker) receiver_alive: bool,
}

pub(in crate::worker) struct SubscriberShared {
    pub(in crate::worker) state: Mutex<SubscriberState>,
    pub(in crate::worker) cvar: Condvar,
}

impl SubscriberShared {
    /// Per-subscriber state ignores poisoning on purpose; see the worker's
    /// `lock` for the policy and why this queue is the one exception.
    pub(in crate::worker) fn poisoned_lock(&self) -> std::sync::MutexGuard<'_, SubscriberState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(in crate::worker) struct SubscriberSender {
    pub(in crate::worker) shared: Arc<SubscriberShared>,
    /// Whether this subscriber asked for the live screen (`AttachPayload::Screen`)
    /// rather than a raw tail. Only screen subscribers are eligible for
    /// backlog coalescing in `OutputHub::append`; tail subscribers keep the
    /// evict-and-reattach contract so `--history-bytes` stays byte-exact.
    pub(in crate::worker) want_screen: bool,
}

/// What one subscriber did with an offered PTY chunk.
pub(in crate::worker) enum Offer {
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
    pub(in crate::worker) fn offer(
        &self,
        data: &Arc<[u8]>,
        snapshot: &mut dyn FnMut() -> Arc<[u8]>,
    ) -> Offer {
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

    pub(in crate::worker) fn try_event(&self, event: OutputEvent) -> bool {
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

    pub(in crate::worker) fn terminate(self, event: OutputEvent) {
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

pub(in crate::worker) struct OutputReceiver {
    pub(in crate::worker) shared: Arc<SubscriberShared>,
}

impl OutputReceiver {
    /// Drain already-queued output before reporting the terminal outcome.
    /// Queued data always wins over a terminal event; once the queue is
    /// empty the terminal outcome (if any) is returned exactly once, and
    /// afterwards the receiver reports disconnection -- mirroring the old
    /// two-channel `mpsc` behavior this replaced.
    pub(in crate::worker) fn recv(&self) -> Result<OutputEvent, mpsc::RecvError> {
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
