//! One code instance: an isolate, its glue's hooks, and the run's modules.

use std::rc::Rc;

use deno_core::{ModuleSpecifier, PollEventLoopOptions, v8};
use rewind_engine::ports::{Emitted, ExecutorError, Handoff, StartCode};
use serde_json::{Value, json};

use crate::epochs::Glue;
use crate::isolate::{Isolate, SandboxLimits, function, internal};
use crate::loader::{ENTRY_SPECIFIER, Loader};
use crate::memory;
use crate::ops::ExecState;

pub(crate) struct Instance {
    isolate: Isolate,
    start: v8::Global<v8::Function>,
    deliver: v8::Global<v8::Function>,
    idle: v8::Global<v8::Function>,
}

impl Instance {
    /// Starts an isolate from the epoch's snapshot and evaluates the run's modules. Load
    /// failures (forbidden import, no default export, top-level error) are `InvalidCode`.
    pub(crate) async fn load(
        start: &StartCode,
        glue: &'static Glue,
        limits: SandboxLimits,
    ) -> Result<Self, ExecutorError> {
        let loader = Rc::new(Loader::code(
            glue,
            start.compiled.javascript.clone(),
            start.compiled.source_map.clone().into_bytes(),
            (glue.actions)(&start.action_set),
            (glue.workflows)(&start.action_set),
        ));
        let mut state = ExecState::new(
            start.run_id,
            &start.seed,
            start.action_set.clone(),
            start.started_at,
        );
        state.old_texts = glue.old_texts;
        let (mut isolate, hooks) = Isolate::new(glue, limits, loader, state)?;
        let (start_hook, deliver, idle) = {
            let runtime = isolate.runtime();
            deno_core::scope!(scope, runtime);
            let hooks = v8::Local::new(scope, hooks);
            (
                function(scope, hooks, "start")?,
                function(scope, hooks, "deliver")?,
                function(scope, hooks, "idle")?,
            )
        };
        let mut instance = Self {
            isolate,
            start: start_hook,
            deliver,
            idle,
        };
        instance.evaluate_modules().await?;
        Ok(instance)
    }

    async fn evaluate_modules(&mut self) -> Result<(), ExecutorError> {
        let invalid = |err: &dyn std::fmt::Display| ExecutorError::InvalidCode(err.to_string());
        let entry = ModuleSpecifier::parse(ENTRY_SPECIFIER).map_err(internal)?;
        self.isolate.begin()?;
        let result = async {
            let runtime = self.isolate.runtime();
            let module = runtime
                .load_main_es_module(&entry)
                .await
                .map_err(|err| invalid(&err))?;
            // The code's top-level code runs here, as deep as V8 lets it below this frame.
            memory::pin_stack(runtime.v8_isolate());
            let evaluation = runtime.mod_evaluate(module);
            runtime
                .run_event_loop(PollEventLoopOptions::default())
                .await
                .map_err(|err| invalid(&err))?;
            evaluation.await.map_err(|err| invalid(&err))
        }
        .await;
        self.isolate.end()?;
        result
    }

    /// Runs `main(input)` until the code is idle.
    pub(crate) fn start(&mut self, input: &Value) -> Result<Vec<Emitted>, ExecutorError> {
        let start = self.start.clone();
        self.run(&start, &input.to_string())
    }

    /// Delivers one completion at logical time `now`, and runs the code until it is idle.
    pub(crate) fn deliver(
        &mut self,
        handoff: &Handoff,
        now: rewind_core::Timestamp,
    ) -> Result<Vec<Emitted>, ExecutorError> {
        self.isolate.state(|state| state.now = now);
        let deliver = self.deliver.clone();
        self.run(&deliver, &handoff_json(handoff))
    }

    /// The memory the isolate holds, in bytes.
    pub(crate) fn memory_usage(&mut self) -> u64 {
        self.isolate.memory_usage()
    }

    /// The CPU time of the isolate's thread in the calls into the code since the last take:
    /// loading the modules and `main(input)` after [`Instance::load`] and [`Instance::start`],
    /// one handoff after [`Instance::deliver`]; whether they succeeded or not.
    pub(crate) fn take_cpu_time(&mut self) -> std::time::Duration {
        self.isolate.take_cpu_time()
    }

    /// Calls a hook with one string argument, drains microtasks, then evaluates the code's
    /// outcome (`idle`), all under the CPU budget.
    fn run(
        &mut self,
        hook: &v8::Global<v8::Function>,
        arg: &str,
    ) -> Result<Vec<Emitted>, ExecutorError> {
        let idle = self.idle.clone();
        self.isolate.begin()?;
        let result = self
            .isolate
            .call(hook, &[arg])
            .and_then(|()| self.isolate.call(&idle, &[]));
        self.isolate.end()?;
        result?;
        Ok(self.isolate.state(ExecState::take_emitted))
    }
}

/// The JS ↔ Rust encoding of a handoff: internal to the sandbox, shaped for the glue.
fn handoff_json(handoff: &Handoff) -> String {
    let value = match handoff {
        Handoff::CallCompleted { seq, result } => json!({
            "type": "call_completed",
            "seq": seq.0,
            "result": result,
        }),
        Handoff::CallFailed { seq, error } => json!({
            "type": "call_failed",
            "seq": seq.0,
            "message": error.message,
            "outcomeUnknown": error.kind == rewind_core::ActionFailureKind::OutcomeUnknown,
            "data": error.data,
        }),
        Handoff::TimerFired { seq } => json!({ "type": "timer_fired", "seq": seq.0 }),
        Handoff::SignalReceived { name, payload } => json!({
            "type": "signal_received",
            "name": name,
            "payload": payload,
        }),
        Handoff::CancelRequested { reason } => json!({
            "type": "cancel_requested",
            "message": match reason {
                rewind_core::CancelReason::Requested(Some(message)) => format!("run cancelled: {message}"),
                rewind_core::CancelReason::Requested(None) => "run cancelled".to_owned(),
                rewind_core::CancelReason::Ttl => "run cancelled: its deadline has passed".to_owned(),
            },
        }),
    };
    value.to_string()
}
