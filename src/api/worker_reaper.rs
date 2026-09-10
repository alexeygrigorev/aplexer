//! The launcher-side reaper for workers it started: a detached waiter so an
//! embedder that outlives many sessions (notably Python) does not keep every
//! finished worker as a zombie.

use super::*;

pub(super) const WORKER_REAPER_POLL: Duration = Duration::from_millis(100);
pub(super) static WORKER_REAPER: Mutex<Option<mpsc::Sender<Child>>> = Mutex::new(None);

pub(super) fn worker_reaper_loop(receiver: mpsc::Receiver<Child>) {
    let mut children: Vec<Child> = Vec::new();
    loop {
        let received = if children.is_empty() {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        } else {
            receiver.recv_timeout(WORKER_REAPER_POLL)
        };
        match received {
            Ok(child) => children.push(child),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(child) = receiver.try_recv() {
            children.push(child);
        }
        children.retain_mut(|child| match child.try_wait() {
            Ok(Some(_)) => false,
            Ok(None) => true,
            Err(error) => {
                eprintln!("aplexer: wait for worker {} failed: {error}", child.id());
                false
            }
        });
    }
}
