//! One script instance: a V8 isolate, its glue and its limits. It lives on its own thread
//! (V8 objects cannot change threads): see `executor.rs`.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use deno_core::{JsRuntime, ModuleSpecifier, PollEventLoopOptions, RuntimeOptions, v8};
use ptc_engine::ports::{Delivery, Emitted, ExecutorError, StartScript};
use serde_json::{Value, json};

use crate::epochs::Glue;
use crate::loader::{ENTRY_SPECIFIER, Loader};
use crate::ops::{ExecState, ptc_sandbox};
use crate::watchdog::Watchdog;

/// Resource limits of one instance (`archi/03-execution.md`, "Resource limits").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxLimits {
    /// CPU time the script may use to react to one trigger (`main(input)`, or one delivery).
    /// The watchdog measures it as the wall-clock time of the call: the isolate runs on its
    /// own thread and does no I/O.
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
    // Dropped first: the watchdog's handle must not outlive the isolate.
    watchdog: Watchdog,
    runtime: JsRuntime,
    hooks: Hooks,
    limits: SandboxLimits,
    heap_exceeded: Rc<Cell<bool>>,
    /// The isolate was terminated: it cannot run JavaScript any more.
    dead: bool,
}

struct Hooks {
    start: v8::Global<v8::Function>,
    deliver: v8::Global<v8::Function>,
    idle: v8::Global<v8::Function>,
}

impl Isolate {
    /// Creates the isolate, runs the bootstrap and evaluates the modules. Load failures
    /// (forbidden import, no default export, top-level error) are `InvalidScript`.
    pub(crate) async fn load(
        start: &StartScript,
        glue: &'static Glue,
        limits: SandboxLimits,
    ) -> Result<Self, ExecutorError> {
        let loader = Rc::new(Loader {
            glue,
            script: start.compiled.javascript.clone(),
            source_map: start.compiled.source_map.clone().into_bytes(),
            tools: (glue.tools)(&start.toolset),
        });
        let mut runtime = JsRuntime::try_new(RuntimeOptions {
            module_loader: Some(loader),
            extensions: vec![ptc_sandbox::init()],
            create_params: Some(v8::CreateParams::default().heap_limits(0, limits.heap_bytes)),
            ..Default::default()
        })
        .map_err(internal)?;
        runtime.op_state().borrow_mut().put(ExecState::new(
            start.run_id,
            &start.seed,
            start.toolset.clone(),
            start.started_at,
        ));

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
            .execute_script("ptc:bootstrap", glue.bootstrap)
            .map_err(internal)?;
        let hooks = {
            let runtime = &mut runtime;
            deno_core::scope!(scope, runtime);
            let hooks = v8::Local::new(scope, hooks);
            let hooks = v8::Local::<v8::Object>::try_from(hooks).map_err(internal)?;
            Hooks {
                start: function(scope, hooks, "start")?,
                deliver: function(scope, hooks, "deliver")?,
                idle: function(scope, hooks, "idle")?,
            }
        };

        let mut isolate = Self {
            watchdog,
            runtime,
            hooks,
            limits,
            heap_exceeded,
            dead: false,
        };
        isolate.evaluate_modules().await?;
        Ok(isolate)
    }

    async fn evaluate_modules(&mut self) -> Result<(), ExecutorError> {
        let invalid = |err: &dyn std::fmt::Display| ExecutorError::InvalidScript(err.to_string());
        let entry = ModuleSpecifier::parse(ENTRY_SPECIFIER).map_err(internal)?;
        self.watchdog.arm(self.limits.cpu_per_trigger);
        let result = async {
            let module = self
                .runtime
                .load_main_es_module(&entry)
                .await
                .map_err(|err| invalid(&err))?;
            let evaluation = self.runtime.mod_evaluate(module);
            self.runtime
                .run_event_loop(PollEventLoopOptions::default())
                .await
                .map_err(|err| invalid(&err))?;
            evaluation.await.map_err(|err| invalid(&err))
        }
        .await;
        let timed_out = self.watchdog.disarm();
        self.check_limits(timed_out)?;
        result
    }

    /// Runs `main(input)` until the script is idle.
    pub(crate) fn start(&mut self, input: &Value) -> Result<Vec<Emitted>, ExecutorError> {
        let start = self.hooks.start.clone();
        self.run(&start, &input.to_string())
    }

    /// Delivers one completion at logical time `now`, and runs the script until it is idle.
    pub(crate) fn deliver(
        &mut self,
        delivery: &Delivery,
        now: ptc_core::Timestamp,
    ) -> Result<Vec<Emitted>, ExecutorError> {
        self.state(|state| state.now = now);
        let deliver = self.hooks.deliver.clone();
        self.run(&deliver, &delivery_json(delivery))
    }

    /// Calls a hook with one string argument, drains microtasks, then evaluates the script's
    /// outcome (`idle`), all under the CPU budget.
    fn run(
        &mut self,
        hook: &v8::Global<v8::Function>,
        arg: &str,
    ) -> Result<Vec<Emitted>, ExecutorError> {
        if self.dead {
            return Err(ExecutorError::Internal(
                "the isolate was terminated and cannot run any more".into(),
            ));
        }
        let idle = self.hooks.idle.clone();
        self.watchdog.arm(self.limits.cpu_per_trigger);
        let result = self
            .call(hook, Some(arg))
            .and_then(|()| self.call(&idle, None));
        let timed_out = self.watchdog.disarm();
        self.check_limits(timed_out)?;
        result?;
        Ok(self.state(ExecState::take_emitted))
    }

    fn call(
        &mut self,
        function: &v8::Global<v8::Function>,
        arg: Option<&str>,
    ) -> Result<(), ExecutorError> {
        let runtime = &mut self.runtime;
        deno_core::scope!(scope, runtime);
        v8::tc_scope!(let scope, scope);
        let function = v8::Local::new(scope, function);
        let receiver = v8::undefined(scope).into();
        let args: Vec<v8::Local<v8::Value>> = match arg {
            Some(arg) => vec![
                v8::String::new(scope, arg)
                    .ok_or_else(|| ExecutorError::Internal("payload too large for V8".into()))?
                    .into(),
            ],
            None => Vec::new(),
        };
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

    /// A terminated execution is a limit exceeded; the isolate is then unusable.
    fn check_limits(&mut self, timed_out: bool) -> Result<(), ExecutorError> {
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

    fn state<R>(&mut self, f: impl FnOnce(&mut ExecState) -> R) -> R {
        f(self
            .runtime
            .op_state()
            .borrow_mut()
            .borrow_mut::<ExecState>())
    }
}

fn function(
    scope: &mut v8::PinScope,
    object: v8::Local<v8::Object>,
    name: &str,
) -> Result<v8::Global<v8::Function>, ExecutorError> {
    let key = v8::String::new(scope, name)
        .ok_or_else(|| ExecutorError::Internal(format!("cannot create the string {name:?}")))?;
    let value = object
        .get(scope, key.into())
        .ok_or_else(|| ExecutorError::Internal(format!("the bootstrap hook {name} is missing")))?;
    let function = v8::Local::<v8::Function>::try_from(value).map_err(internal)?;
    Ok(v8::Global::new(scope, function))
}

/// The JS ↔ Rust encoding of a delivery: internal to the sandbox, shaped for the glue.
fn delivery_json(delivery: &Delivery) -> String {
    let value = match delivery {
        Delivery::ToolCallCompleted { seq, result } => json!({
            "type": "tool_call_completed",
            "seq": seq.0,
            "result": result,
        }),
        Delivery::ToolCallFailed { seq, error } => json!({
            "type": "tool_call_failed",
            "seq": seq.0,
            "message": error.message,
            "outcomeUnknown": error.kind == ptc_core::ToolFailureKind::OutcomeUnknown,
            "data": error.data,
        }),
        Delivery::TimerFired { seq } => json!({ "type": "timer_fired", "seq": seq.0 }),
        Delivery::SignalReceived { name, payload } => json!({
            "type": "signal_received",
            "name": name,
            "payload": payload,
        }),
        Delivery::CancelRequested { reason } => json!({
            "type": "cancel_requested",
            "message": match reason {
                ptc_core::CancelReason::Requested(Some(message)) => format!("run cancelled: {message}"),
                ptc_core::CancelReason::Requested(None) => "run cancelled".to_owned(),
                ptc_core::CancelReason::Ttl => "run cancelled: its deadline has passed".to_owned(),
            },
        }),
    };
    value.to_string()
}

fn internal(err: impl std::fmt::Display) -> ExecutorError {
    ExecutorError::Internal(err.to_string())
}
