//! One isolate, started from its epoch's snapshot, with its limits. It lives on its own thread
//! (V8 objects cannot change threads): see `executor.rs` for scripts and `mocks.rs` for mock
//! handlers.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use deno_core::{JsRuntime, ModuleLoader, RuntimeOptions, v8};
use rewind_engine::ports::ExecutorError;

use crate::epochs::Glue;
use crate::memory;
use crate::ops::{self, ExecState};
use crate::snapshot;
use crate::watchdog::Watchdog;

/// Resource limits of one instance (`archi/03-execution.md`, "Resource limits").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxLimits {
    /// CPU time the script may use to react to one trigger (`main(input)`, or one delivery),
    /// measured on the CPU clock of the isolate's thread: time the thread spends waiting for a
    /// core does not count.
    pub cpu_per_trigger: Duration,
    /// V8 heap size.
    pub heap_bytes: usize,
    /// Bytes the instance's array buffers may hold at once (an `ArrayBuffer`, the memory of a
    /// typed array), which V8 keeps outside its heap. An allocation past it fails with V8's
    /// `RangeError: Array buffer allocation failed`, which the script can catch. A resizable
    /// buffer takes its maximum length from it when it is created.
    pub array_buffer_bytes: usize,
    /// Commands and annotations (logs, signal waits) the script may emit reacting to one
    /// trigger. What it emits is kept outside V8's heap until the call ends.
    pub emitted_per_trigger: usize,
    /// Bytes of text those may carry, reacting to one trigger: log messages, signal names, action
    /// arguments and the output as JSON, the error of a failure.
    pub emitted_bytes_per_trigger: usize,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            cpu_per_trigger: Duration::from_secs(5),
            heap_bytes: 128 * 1024 * 1024,
            array_buffer_bytes: 128 * 1024 * 1024,
            // The default cap on a run's history: more could never be committed.
            emitted_per_trigger: 50_000,
            emitted_bytes_per_trigger: 64 * 1024 * 1024,
        }
    }
}

/// Room for V8's largest object, with its page: what V8 may still need to allocate once the heap
/// limit is reached.
const LARGEST_OBJECT: usize = (1024 + 64) * 1024 * 1024;

pub(crate) struct Isolate {
    // Dropped first: the watchdog stops before the isolate goes.
    watchdog: Watchdog,
    runtime: JsRuntime,
    /// The budget of the isolate's array buffers, dropped after it.
    _array_buffers: Arc<memory::ArrayBufferBudget>,
    limits: SandboxLimits,
    heap_exceeded: Rc<Cell<bool>>,
    /// The isolate was terminated: it cannot run JavaScript any more.
    dead: bool,
    /// The CPU time of the calls ended since [`Isolate::take_cpu_time`] was last called.
    cpu: Duration,
}

impl Isolate {
    /// Starts an isolate from the epoch's snapshot and runs the glue's `init` hook with what
    /// belongs to this instance. Returns the glue's hooks. Must be called on the isolate's
    /// thread, inside a tokio runtime (V8 schedules its delayed tasks on it).
    pub(crate) fn new(
        glue: &'static Glue,
        limits: SandboxLimits,
        loader: Rc<dyn ModuleLoader>,
        mut state: ExecState,
    ) -> Result<(Self, v8::Global<v8::Object>), ExecutorError> {
        let (array_buffers, budget) = memory::capped_allocator(limits.array_buffer_bytes);
        let mut runtime = JsRuntime::try_new(RuntimeOptions {
            module_loader: Some(loader),
            extensions: ops::extensions(),
            startup_snapshot: Some(snapshot::of(glue)?),
            create_params: Some(
                v8::CreateParams::default()
                    .heap_limits(0, limits.heap_bytes)
                    .array_buffer_allocator(array_buffers),
            ),
            ..Default::default()
        })
        .map_err(internal)?;
        let handle = runtime.v8_isolate().thread_safe_handle();
        state.bound(&limits, handle.clone());
        let config = state.init_config().to_string();
        runtime.op_state().borrow_mut().put(state);

        memory::track_refusals(runtime.v8_isolate(), &budget);
        let heap_exceeded = Rc::new(Cell::new(false));
        {
            let exceeded = Rc::clone(&heap_exceeded);
            let handle = handle.clone();
            let budget = Arc::clone(&budget);
            runtime.add_near_heap_limit_callback(move |current, _initial| {
                // V8 also calls at the start of the collection it runs before it gives up on an
                // array buffer, whatever the heap's size: the script then gets a RangeError.
                if budget.took_refusal() && !exceeded.get() {
                    return current;
                }
                exceeded.set(true);
                handle.terminate_execution();
                // The termination takes effect at the next interrupt check. Until then V8 must
                // finish what it is doing: the allocation that does not fit, and the rest of a
                // built-in that never checks for interrupts. If the new limit leaves no room
                // for it, V8 aborts the process. One object is at most 1 GiB (2^27 elements, or
                // a string of 2^29 two-byte characters), so that much room is always given.
                current.saturating_add(LARGEST_OBJECT)
            });
        }
        let watchdog = Watchdog::spawn(handle).map_err(internal)?;

        let hooks = runtime
            .execute_script("rewind:hooks", "globalThis.__rewindHooks")
            .map_err(internal)?;
        let mut isolate = Self {
            watchdog,
            runtime,
            _array_buffers: budget,
            limits,
            heap_exceeded,
            dead: false,
            cpu: Duration::ZERO,
        };
        let (hooks, init) = {
            let runtime = &mut isolate.runtime;
            deno_core::scope!(scope, runtime);
            let hooks = v8::Local::new(scope, hooks);
            let hooks = v8::Local::<v8::Object>::try_from(hooks).map_err(internal)?;
            let init = function(scope, hooks, "init")?;
            (v8::Global::new(scope, hooks), init)
        };
        isolate.call(&init, &[&config])?;
        Ok((isolate, hooks))
    }

    pub(crate) fn runtime(&mut self) -> &mut JsRuntime {
        &mut self.runtime
    }

    /// The CPU time of the calls into JavaScript that ended since the last take, failed calls
    /// included, as the watchdog measured it.
    pub(crate) fn take_cpu_time(&mut self) -> Duration {
        std::mem::take(&mut self.cpu)
    }

    /// Starts the CPU budget of one call into JavaScript.
    pub(crate) fn begin(&mut self) -> Result<(), ExecutorError> {
        if self.dead {
            return Err(ExecutorError::Internal(
                "the isolate was terminated and cannot run any more".into(),
            ));
        }
        self.watchdog.arm(self.limits.cpu_per_trigger);
        Ok(())
    }

    /// Ends the CPU budget of a call: a terminated execution is a limit exceeded, and the
    /// isolate is then unusable.
    pub(crate) fn end(&mut self) -> Result<(), ExecutorError> {
        let disarmed = self.watchdog.disarm();
        self.cpu = self.cpu.saturating_add(disarmed.cpu);
        let timed_out = disarmed.fired;
        if self.heap_exceeded.get() {
            self.dead = true;
            return Err(ExecutorError::LimitExceeded(ops::heap_exceeded(
                self.limits.heap_bytes,
            )));
        }
        if let Some(overflow) = self.state(ExecState::overflow) {
            self.dead = true;
            return Err(ExecutorError::LimitExceeded(overflow));
        }
        if timed_out {
            self.dead = true;
            return Err(ExecutorError::LimitExceeded(format!(
                "the script used more than {}ms of CPU time reacting to one trigger",
                self.limits.cpu_per_trigger.as_millis()
            )));
        }
        Ok(())
    }

    /// Calls a function of the glue with string arguments, then drains microtasks. Both run
    /// with the stack limit pinned below this frame.
    pub(crate) fn call(
        &mut self,
        function: &v8::Global<v8::Function>,
        args: &[&str],
    ) -> Result<(), ExecutorError> {
        memory::pin_stack(self.runtime.v8_isolate());
        let runtime = &mut self.runtime;
        deno_core::scope!(scope, runtime);
        v8::tc_scope!(let scope, scope);
        let function = v8::Local::new(scope, function);
        let receiver = v8::undefined(scope).into();
        let args = args
            .iter()
            .map(|arg| {
                v8::String::new(scope, arg)
                    .map(Into::into)
                    .ok_or_else(|| ExecutorError::Internal("payload too large for V8".into()))
            })
            .collect::<Result<Vec<v8::Local<v8::Value>>, _>>()?;
        let returned = function.call(scope, receiver, &args);
        if returned.is_some() {
            scope.perform_microtask_checkpoint();
        }
        if scope.is_execution_terminating() || scope.has_terminated() {
            return Err(ExecutorError::Internal("execution terminated".into()));
        }
        if returned.is_none() {
            let message = scope
                .exception()
                .map_or_else(|| "unknown error".into(), |e| e.to_rust_string_lossy(scope));
            return Err(ExecutorError::Internal(format!(
                "the runtime's glue threw: {message}"
            )));
        }
        Ok(())
    }

    /// The memory the isolate holds, in bytes, from V8's heap statistics: the heap's total
    /// size (what V8 reserved for it, used or not), plus what V8 allocated with `malloc` and
    /// the external memory its objects keep alive (array buffers).
    pub(crate) fn memory_usage(&mut self) -> u64 {
        let statistics = self.runtime.v8_isolate().get_heap_statistics();
        let bytes = statistics
            .total_heap_size()
            .saturating_add(statistics.malloced_memory())
            .saturating_add(statistics.external_memory());
        u64::try_from(bytes).unwrap_or(u64::MAX)
    }

    pub(crate) fn state<R>(&mut self, f: impl FnOnce(&mut ExecState) -> R) -> R {
        f(self
            .runtime
            .op_state()
            .borrow_mut()
            .borrow_mut::<ExecState>())
    }
}

/// The function `name` of an object.
pub(crate) fn function(
    scope: &mut v8::PinScope,
    object: v8::Local<v8::Object>,
    name: &str,
) -> Result<v8::Global<v8::Function>, ExecutorError> {
    let key = v8::String::new(scope, name)
        .ok_or_else(|| ExecutorError::Internal(format!("cannot create the string {name:?}")))?;
    let value = object
        .get(scope, key.into())
        .ok_or_else(|| ExecutorError::Internal(format!("the hook {name} is missing")))?;
    let function = v8::Local::<v8::Function>::try_from(value)
        .map_err(|_| ExecutorError::Internal(format!("{name} is not a function")))?;
    Ok(v8::Global::new(scope, function))
}

pub(crate) fn internal(err: impl std::fmt::Display) -> ExecutorError {
    ExecutorError::Internal(err.to_string())
}
