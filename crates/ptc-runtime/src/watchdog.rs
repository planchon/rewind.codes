//! The CPU-time limit (`archi/03-execution.md`, "Resource limits"): the budget of a call into
//! the script is CPU time of the isolate's thread, not wall-clock time, so a loaded host does
//! not make a script exceed it.
//!
//! A thread's CPU clock can be read portably only from the thread itself
//! (`CLOCK_THREAD_CPUTIME_ID`). So the watchdog thread asks V8 to interrupt the script; the
//! interrupt runs on the isolate's thread between two JavaScript instructions, reads the clock,
//! and terminates the execution once the budget is spent. On one thread, CPU time never runs
//! faster than wall-clock time: the watchdog first waits for the whole budget, then for what is
//! left of it at the last reading. A script that is never descheduled is interrupted about once,
//! at the end of its budget.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use deno_core::v8::{IsolateHandle, UnsafeRawIsolatePtr};

/// The CPU time this thread has used.
pub(crate) fn thread_cpu_time() -> Duration {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    Duration::new(
        u64::try_from(time.tv_sec).unwrap_or(0),
        u32::try_from(time.tv_nsec).unwrap_or(0),
    )
}

/// The watchdog never waits less than this between two readings.
const MIN_WAIT: Duration = Duration::from_millis(1);

thread_local! {
    /// The watchdog of the isolate living on this thread: the interrupt reaches it here.
    static WATCHED: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    isolate: IsolateHandle,
}

#[derive(Default)]
struct State {
    armed: Option<Armed>,
    /// An interrupt was requested and has not run yet.
    requested: bool,
    fired: bool,
    shutdown: bool,
}

#[derive(Clone, Copy)]
struct Armed {
    /// The thread's CPU time when the call started.
    start: Duration,
    budget: Duration,
    /// The CPU time the call had used at the last reading.
    used: Duration,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // The state stays consistent even if a holder panicked.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// On the isolate's thread: reads its CPU clock, and terminates the execution once the
    /// budget is spent.
    fn measure(&self) {
        let now = thread_cpu_time();
        let mut state = self.lock();
        state.requested = false;
        if let Some(armed) = state.armed.as_mut() {
            armed.used = now.saturating_sub(armed.start);
            if armed.used >= armed.budget {
                state.armed = None;
                state.fired = true;
                self.isolate.terminate_execution();
            }
        }
        self.wake.notify_one();
    }
}

/// Runs on the isolate's thread, called by V8 between two JavaScript instructions.
extern "C" fn on_interrupt(_isolate: UnsafeRawIsolatePtr, _data: *mut c_void) {
    WATCHED.with(|watched| {
        if let Some(shared) = watched.borrow().as_ref() {
            shared.measure();
        }
    });
}

/// One watchdog thread per isolate, created on the isolate's thread. [`Watchdog::arm`] before
/// calling into the script, [`Watchdog::disarm`] after: it tells whether the budget ran out
/// meanwhile.
pub(crate) struct Watchdog {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    /// Must be called on the isolate's thread.
    pub(crate) fn spawn(isolate: IsolateHandle) -> std::io::Result<Self> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            isolate,
        });
        let watched = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("ptc-watchdog".into())
            .spawn(move || watch(&watched))?;
        WATCHED.with(|watched| *watched.borrow_mut() = Some(Arc::clone(&shared)));
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Starts the budget of a call, from the CPU time this thread has used so far. Called on
    /// the isolate's thread.
    pub(crate) fn arm(&self, budget: Duration) {
        let start = thread_cpu_time();
        let mut state = self.shared.lock();
        state.armed = Some(Armed {
            start,
            budget,
            used: Duration::ZERO,
        });
        state.fired = false;
        self.shared.wake.notify_one();
    }

    /// Returns whether the watchdog terminated the execution since it was armed.
    pub(crate) fn disarm(&self) -> bool {
        let mut state = self.shared.lock();
        state.armed = None;
        self.shared.wake.notify_one();
        state.fired
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.shared.lock().shutdown = true;
        self.shared.wake.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        WATCHED.with(|watched| {
            let mut watched = watched.borrow_mut();
            if watched
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.shared))
            {
                *watched = None;
            }
        });
    }
}

fn watch(shared: &Shared) {
    let mut state = shared.lock();
    loop {
        if state.shutdown {
            return;
        }
        let Some(armed) = state.armed else {
            state = shared
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
            continue;
        };
        let left = armed.budget.saturating_sub(armed.used).max(MIN_WAIT);
        let (guard, waited) = shared
            .wake
            .wait_timeout(state, left)
            .unwrap_or_else(PoisonError::into_inner);
        state = guard;
        // One interrupt at a time: a thread busy outside JavaScript (in an op) does not pile
        // them up.
        if waited.timed_out() && state.armed.is_some() && !state.requested && !state.shutdown {
            state.requested = shared
                .isolate
                .request_interrupt(on_interrupt, std::ptr::null_mut());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spin(cpu: Duration) {
        let start = thread_cpu_time();
        let mut x = 0u64;
        while thread_cpu_time() - start < cpu {
            x = std::hint::black_box(x.wrapping_add(1));
        }
    }

    /// A call is stopped once it has used its budget of CPU time, not once its budget of
    /// wall-clock time has passed: time the thread spends off the CPU does not count.
    #[test]
    fn the_budget_is_cpu_time_of_the_isolate_thread() {
        crate::executor::init_v8();
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let _context = tokio.enter();
        let mut runtime = deno_core::JsRuntime::new(deno_core::RuntimeOptions::default());
        let watchdog = Watchdog::spawn(runtime.v8_isolate().thread_safe_handle()).unwrap();

        // Three budgets of wall-clock time off the CPU, then a little work: within budget.
        watchdog.arm(Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(300));
        runtime
            .execute_script("work", "let x = 0; for (let i = 0; i < 1e5; i++) x += i; x")
            .unwrap();
        assert!(!watchdog.disarm());

        // An endless loop is stopped once it has used its budget.
        watchdog.arm(Duration::from_millis(100));
        let start = thread_cpu_time();
        assert!(runtime.execute_script("loop", "for (;;) {}").is_err());
        let used = thread_cpu_time() - start;
        assert!(watchdog.disarm());
        assert!(used >= Duration::from_millis(100), "{used:?}");
        assert!(used < Duration::from_secs(2), "{used:?}");
    }

    /// The clock is the thread's CPU time: sleeping does not move it, working does.
    #[test]
    fn the_clock_counts_cpu_time_not_wall_clock_time() {
        let start = thread_cpu_time();
        std::thread::sleep(Duration::from_millis(200));
        let slept = thread_cpu_time() - start;
        assert!(slept < Duration::from_millis(50), "{slept:?}");

        let start = thread_cpu_time();
        spin(Duration::from_millis(50));
        let worked = thread_cpu_time() - start;
        assert!(worked >= Duration::from_millis(50), "{worked:?}");
    }

    /// Another thread's work does not count against this thread.
    #[test]
    fn the_clock_belongs_to_the_thread() {
        let start = thread_cpu_time();
        std::thread::spawn(|| spin(Duration::from_millis(100)))
            .join()
            .unwrap();
        assert!(thread_cpu_time() - start < Duration::from_millis(50));
    }
}
