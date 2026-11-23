//! The embedded server's idle stop (`archi/07-code-architecture.md`, "Embedded server and
//! CLI"): one timer, pushed back by each request, each stream held open and each activation. It
//! is one of the clocks that read no store (principle 11).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::Instant;

/// What keeps a server awake.
#[derive(Debug, Clone)]
pub(crate) struct Idle(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    /// Requests and streams in progress.
    busy: AtomicUsize,
    /// The last activity: a request that ended, an activation.
    last: Mutex<Instant>,
    /// Told when the last request in progress ends.
    quiet: Notify,
}

/// A request or a stream in progress: the server does not stop while it lives.
#[derive(Debug)]
pub(crate) struct Busy(Arc<Inner>);

impl Idle {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Inner {
            busy: AtomicUsize::new(0),
            last: Mutex::new(Instant::now()),
            quiet: Notify::new(),
        }))
    }

    /// Something happened: the idle timer starts again from now.
    pub(crate) fn touch(&self) {
        *self.0.last.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
    }

    pub(crate) fn busy(&self) -> Busy {
        self.0.busy.fetch_add(1, Ordering::SeqCst);
        self.touch();
        Busy(Arc::clone(&self.0))
    }

    /// Returns once nothing has happened for `timeout` and nothing is in progress.
    pub(crate) async fn until_idle(&self, timeout: Duration) {
        loop {
            let quiet = self.0.quiet.notified();
            if self.0.busy.load(Ordering::SeqCst) > 0 {
                quiet.await;
                continue;
            }
            let last = *self.0.last.lock().unwrap_or_else(PoisonError::into_inner);
            let deadline = last + timeout;
            if Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep_until(deadline).await;
        }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        *self.0.last.lock().unwrap_or_else(PoisonError::into_inner) = Instant::now();
        if self.0.busy.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.0.quiet.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn the_timer_is_pushed_back_and_held_by_work_in_progress() {
        let idle = Idle::new();
        let timeout = Duration::from_secs(60);
        let started = Instant::now();
        let busy = idle.busy();
        let waiting = tokio::spawn({
            let idle = idle.clone();
            async move { idle.until_idle(timeout).await }
        });
        tokio::time::sleep(Duration::from_secs(600)).await;
        assert!(!waiting.is_finished(), "a request in progress holds it");
        drop(busy);
        tokio::time::sleep(Duration::from_secs(30)).await;
        idle.touch();
        waiting.await.unwrap();
        assert!(Instant::now() - started >= Duration::from_secs(690));
    }
}
