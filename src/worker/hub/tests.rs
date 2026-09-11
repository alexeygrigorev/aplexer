use super::*;

pub(crate) fn test_hub(dir: &tempfile::TempDir) -> OutputHub {
    OutputHub::new(
        History::open(dir.path().join("history.bin"), 1024 * 1024).unwrap(),
        24,
        80,
        dir.path().join("screen.txt"),
    )
    .unwrap()
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
