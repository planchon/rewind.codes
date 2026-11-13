//! Wake-ups and activity.
//!
//! Everything a waiter may wait for (a commit, a new task, a signal) bumps one counter: the
//! loops, the long polls and the event streams look again when it moves. Another process (an
//! api role apart from its workers) cannot bump it, so every waiter also looks again after a
//! short poll interval: no `LISTEN/NOTIFY` (08).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// How long a waiter waits for a wake-up before it looks again by itself.
pub(crate) const POLL: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub(crate) struct Wake {
    changes: watch::Sender<u64>,
}

impl Wake {
    pub(crate) fn new() -> Self {
        Self {
            changes: watch::Sender::new(0),
        }
    }

    /// Something changed: every waiter looks again.
    pub(crate) fn bump(&self) {
        self.changes
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

/// Waits for a change, at most `poll`, never past `deadline`.
pub(crate) async fn wait(
    changes: &mut watch::Receiver<u64>,
    poll: Duration,
    deadline: Option<tokio::time::Instant>,
) {
    let mut until = tokio::time::Instant::now() + poll;
    if let Some(deadline) = deadline {
        until = until.min(deadline);
    }
    let _ = tokio::time::timeout_at(until, changes.changed()).await;
}

/// What keeps an embedded server from stopping on idle: requests and work in progress, and
/// how long ago the last one ended.
#[derive(Debug, Clone)]
pub(crate) struct Activity {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    last: Mutex<Instant>,
    busy: AtomicUsize,
}

/// Held while a request, a stream or a piece of work is in progress.
#[derive(Debug)]
pub(crate) struct Busy(Arc<Inner>);

impl Activity {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                last: Mutex::new(Instant::now()),
                busy: AtomicUsize::new(0),
            }),
        }
    }

    pub(crate) fn busy(&self) -> Busy {
        self.inner.busy.fetch_add(1, Ordering::SeqCst);
        Busy(self.inner.clone())
    }

    /// Something happened now.
    pub(crate) fn touch(&self) {
        if let Ok(mut last) = self.inner.last.lock() {
            *last = Instant::now();
        }
    }

    /// How long nothing has happened; `None` while something is in progress.
    pub(crate) fn idle_for(&self) -> Option<Duration> {
        if self.inner.busy.load(Ordering::SeqCst) > 0 {
            return None;
        }
        self.inner.last.lock().ok().map(|last| last.elapsed())
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Ok(mut last) = self.0.last.lock() {
            *last = Instant::now();
        }
        self.0.busy.fetch_sub(1, Ordering::SeqCst);
    }
}
