use super::*;

pub(in crate::worker) struct SecretBytes(pub(in crate::worker) Vec<u8>);

impl Drop for SecretBytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub(in crate::worker) struct LaunchEnvironment(pub(in crate::worker) BTreeMap<String, String>);

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

pub(in crate::worker) struct ConnectionPermit {
    pub(in crate::worker) active: Arc<AtomicUsize>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(in crate::worker) fn try_acquire_connection(
    active: &Arc<AtomicUsize>,
) -> Option<ConnectionPermit> {
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
pub(in crate::worker) fn transient_accept_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EMFILE) | Some(libc::ENFILE) | Some(libc::ENOBUFS) | Some(libc::ENOMEM)
    )
}

pub(in crate::worker) fn bounded_history_limit(requested: Option<usize>) -> usize {
    requested.unwrap_or(MAX_FRAME_BYTES).min(MAX_FRAME_BYTES)
}

pub(in crate::worker) fn ensure_frame_payload_size(kind: &str, len: usize) -> Result<()> {
    if len > MAX_FRAME_BYTES {
        bail!("{kind} exceeds the maximum frame size of {MAX_FRAME_BYTES} bytes");
    }
    Ok(())
}
