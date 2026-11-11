//! The CPU-time limit: a watchdog thread terminates the isolate's execution when a call into
//! the script runs past its budget.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use deno_core::v8::IsolateHandle;

#[derive(Debug, Default)]
struct State {
    deadline: Option<Instant>,
    fired: bool,
    shutdown: bool,
}

/// One watchdog thread per isolate. [`Watchdog::arm`] before calling into the script,
/// [`Watchdog::disarm`] after: it tells whether the budget ran out meanwhile.
pub(crate) struct Watchdog {
    shared: Arc<(Mutex<State>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    pub(crate) fn spawn(isolate: IsolateHandle) -> std::io::Result<Self> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let watched = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("ptc-watchdog".into())
            .spawn(move || watch(&watched, &isolate))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    pub(crate) fn arm(&self, budget: Duration) {
        let mut state = self.lock();
        state.deadline = Some(Instant::now() + budget);
        state.fired = false;
        self.shared.1.notify_one();
    }

    /// Returns whether the watchdog terminated the execution since it was armed.
    pub(crate) fn disarm(&self) -> bool {
        let mut state = self.lock();
        state.deadline = None;
        self.shared.1.notify_one();
        state.fired
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The state stays consistent even if a holder panicked.
        self.shared.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.lock().shutdown = true;
        self.shared.1.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn watch(shared: &(Mutex<State>, Condvar), isolate: &IsolateHandle) {
    let (lock, wake) = shared;
    let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if state.shutdown {
            return;
        }
        state = match state.deadline {
            None => wake.wait(state).unwrap_or_else(PoisonError::into_inner),
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    isolate.terminate_execution();
                    state.fired = true;
                    state.deadline = None;
                    continue;
                }
                wake.wait_timeout(state, deadline - now)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
        };
    }
}
