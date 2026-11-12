//! One isolate, started from its epoch's snapshot, with its limits. It lives on its own thread
//! (V8 objects cannot change threads): see `executor.rs` for scripts and `mocks.rs` for mock
//! handlers.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use deno_core::{JsRuntime, ModuleLoader, RuntimeOptions, v8};
use ptc_engine::ports::ExecutorError;

use crate::epochs::Glue;
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
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            cpu_per_trigger: Duration::from_secs(5),
            heap_bytes: 128 * 1024 * 1024,
        }
    }
}

pub(crate) struct Isolate {
    // Dropped first: the watchdog stops before the isolate goes.
    watchdog: Watchdog,
    runtime: JsRuntime,
    limits: SandboxLimits,
    heap_exceeded: Rc<Cell<bool>>,
    /// The isolate was terminated: it cannot run JavaScript any more.
    dead: bool,
}

impl Isolate {
    /// Starts an isolate from the epoch's snapshot and runs the glue's `init` hook with what
    /// belongs to this instance. Returns the glue's hooks. Must be called on the isolate's
    /// thread, inside a tokio runtime (V8 schedules its delayed tasks on it).
    pub(crate) fn new(
        glue: &'static Glue,
        limits: SandboxLimits,
        loader: Rc<dyn ModuleLoader>,
        state: ExecState,
    ) -> Result<(Self, v8::Global<v8::Object>), ExecutorError> {
        let mut runtime = JsRuntime::try_new(RuntimeOptions {
            module_loader: Some(loader),
            extensions: ops::extensions(),
            startup_snapshot: Some(snapshot::of(glue)?),
            create_params: Some(v8::CreateParams::default().heap_limits(0, limits.heap_bytes)),
            ..Default::default()
        })
        .map_err(internal)?;
        let config = state.init_config().to_string();
        runtime.op_state().borrow_mut().put(state);

        let heap_exceeded = Rc::new(Cell::new(false));
        let handle = runtime.v8_isolate().thread_safe_handle();
        {
            let exceeded = Rc::clone(&heap_exceeded);
            let handle = handle.clone();
            runtime.add_near_heap_limit_callback(move |current, _initial| {
                exceeded.set(true);
                handle.terminate_execution();
                // Room for V8 to unwind the terminated execution.
                current.saturating_mul(2)
            });
        }
        let watchdog = Watchdog::spawn(handle).map_err(internal)?;

        let hooks = runtime
            .execute_script("ptc:hooks", "globalThis.__ptcHooks")
            .map_err(internal)?;
        let mut isolate = Self {
            watchdog,
            runtime,
            limits,
            heap_exceeded,
            dead: false,
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
        let timed_out = self.watchdog.disarm();
        if self.heap_exceeded.get() {
            self.dead = true;
            return Err(ExecutorError::LimitExceeded(format!(
                "the script used more than {} MiB of heap",
                self.limits.heap_bytes / (1024 * 1024)
            )));
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

    /// Calls a function of the glue with string arguments, then drains microtasks.
    pub(crate) fn call(
        &mut self,
        function: &v8::Global<v8::Function>,
        args: &[&str],
    ) -> Result<(), ExecutorError> {
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
