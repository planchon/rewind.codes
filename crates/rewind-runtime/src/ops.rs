//! The ops: the only way the code's glue talks to Rust.
//!
//! Ops are synchronous: they record a command, assign its `seq` and return it. Nothing is
//! asynchronous from V8's point of view; the glue turns a `seq` into a promise that a later
//! handoff settles.

use std::collections::BTreeMap;
use std::time::Duration;

use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use deno_core::{OpState, extension, op2, v8};
use deno_error::JsErrorBox;
use rewind_core::{
    ActionRoute, ActionSet, Annotation, Builtin, Callsite, Command, ErrorInfo, LogLevel, Payload,
    RunId, Seed, Seq, Timestamp, WorkflowOp, parse_duration,
};
use rewind_engine::ports::Emitted;
use rewind_engine::schema::{Refused, Validator};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::isolate::SandboxLimits;

/// The state of one isolate, kept in deno_core's `OpState`.
pub(crate) struct ExecState {
    /// The logical time: `recorded_at` of the last delivered trigger.
    pub(crate) now: Timestamp,
    started_at: Timestamp,
    run_id: RunId,
    action_set: ActionSet,
    next_seq: Option<Seq>,
    emitted: Vec<Emitted>,
    /// The bytes of text `emitted` holds.
    emitted_bytes: usize,
    /// Bounds on `emitted`, which lives outside V8's heap until the call ends.
    max_emitted: usize,
    max_emitted_bytes: usize,
    /// The heap limit, which bounds the elements one built-in call may create.
    heap_bytes: usize,
    /// Why the execution was terminated, once the code emitted more than the bounds allow.
    overflow: Option<String>,
    /// Terminates the execution from an op.
    isolate: Option<v8::IsolateHandle>,
    /// State of `Math.random` (sfc32), derived from the seed.
    math_random_state: [u32; 4],
    /// The keystream behind `crypto` (ChaCha20, RFC 8439), keyed by the seed.
    crypto: ChaCha20,
    /// The validators of the actions called so far, compiled from their pinned input schemas.
    validators: BTreeMap<String, Validator>,
    /// The validators of the pinned workflows' schemas used so far: by workflow, then
    /// `input`, `output` or `signal:<name>`.
    workflow_validators: BTreeMap<(String, String), Validator>,
}

impl ExecState {
    pub(crate) fn new(
        run_id: RunId,
        seed: &Seed,
        action_set: ActionSet,
        started_at: Timestamp,
    ) -> Self {
        // Each generator gets its own key, derived from the seed: what `Math.random` reveals
        // tells nothing about `crypto`.
        let math = derive_key(b"rewind/epoch-1/math-random", seed);
        let math_random_state = std::array::from_fn(|i| {
            u32::from_le_bytes([
                math[4 * i],
                math[4 * i + 1],
                math[4 * i + 2],
                math[4 * i + 3],
            ])
        });
        let key = derive_key(b"rewind/epoch-1/crypto", seed);
        let limits = SandboxLimits::default();
        Self {
            now: started_at,
            started_at,
            run_id,
            action_set,
            next_seq: Some(Seq::FIRST),
            emitted: Vec::new(),
            emitted_bytes: 0,
            max_emitted: limits.emitted_per_trigger,
            max_emitted_bytes: limits.emitted_bytes_per_trigger,
            heap_bytes: limits.heap_bytes,
            overflow: None,
            isolate: None,
            math_random_state,
            crypto: ChaCha20::new(&key.into(), &[0u8; 12].into()),
            validators: BTreeMap::new(),
            workflow_validators: BTreeMap::new(),
        }
    }

    /// What the glue's `init` hook receives: what belongs to this instance.
    pub(crate) fn init_config(&self) -> Value {
        json!({
            "runId": self.run_id.to_string(),
            "startedAt": self.started_at.as_millis(),
            "mathRandom": self.math_random_state,
            "http": self.action_set.actions().iter().any(|action| {
                matches!(action.route, ActionRoute::Builtin(Builtin::Http { .. }))
            }),
            "maxElements": max_elements(self.heap_bytes),
        })
    }

    /// Bounds what the code may emit reacting to one trigger, in the isolate `isolate`.
    pub(crate) fn bound(&mut self, limits: &SandboxLimits, isolate: v8::IsolateHandle) {
        self.max_emitted = limits.emitted_per_trigger;
        self.max_emitted_bytes = limits.emitted_bytes_per_trigger;
        self.heap_bytes = limits.heap_bytes;
        self.isolate = Some(isolate);
    }

    pub(crate) fn take_emitted(&mut self) -> Vec<Emitted> {
        self.emitted_bytes = 0;
        std::mem::take(&mut self.emitted)
    }

    /// Why the execution was terminated, if the code emitted more than the bounds allow or
    /// asked a built-in for more than the heap holds.
    pub(crate) fn overflow(&mut self) -> Option<String> {
        self.overflow.clone()
    }

    /// Terminates the execution as the heap limit does.
    fn exceed_heap(&mut self) {
        if self.overflow.is_none() {
            self.overflow = Some(heap_exceeded(self.heap_bytes));
        }
        if let Some(isolate) = &self.isolate {
            isolate.terminate_execution();
        }
    }

    /// Makes room for one more command or annotation carrying `bytes` of text. Past the bounds,
    /// nothing more is kept and the execution is terminated: the call ends with a limit
    /// exceeded, and whatever the code runs before the termination takes effect is dropped.
    fn admit(&mut self, bytes: usize) -> bool {
        if self.overflow.is_some() {
            return false;
        }
        let total = self.emitted_bytes.saturating_add(bytes);
        let exceeded = if self.emitted.len() >= self.max_emitted {
            format!("{} commands and logs", self.max_emitted)
        } else if total > self.max_emitted_bytes {
            format!("{} of commands and logs", size(self.max_emitted_bytes))
        } else {
            self.emitted_bytes = total;
            return true;
        };
        self.overflow = Some(format!(
            "the code emitted more than {exceeded} reacting to one trigger"
        ));
        if let Some(isolate) = &self.isolate {
            isolate.terminate_execution();
        }
        false
    }

    /// Copies strings out of V8 for one command or annotation, and admits it. Strings whose
    /// UTF-16 length alone is above what is left are never copied: their UTF-8 form has at least
    /// as many bytes, and a string built by concatenation can be far larger than the heap.
    fn admit_strings<const N: usize>(
        &mut self,
        scope: &mut v8::PinScope,
        strings: [v8::Local<v8::String>; N],
    ) -> Option<[String; N]> {
        let length = strings
            .iter()
            .fold(0usize, |sum, string| sum.saturating_add(string.length()));
        let left = self.max_emitted_bytes.saturating_sub(self.emitted_bytes);
        if length > left {
            self.admit(length);
            return None;
        }
        let strings = strings.map(|string| string.to_rust_string_lossy(scope));
        let bytes = strings.iter().map(String::len).sum();
        self.admit(bytes).then_some(strings)
    }

    fn take_seq(&mut self) -> Result<Seq, JsErrorBox> {
        let seq = self
            .next_seq
            .ok_or_else(|| JsErrorBox::range_error("the code emitted too many commands"))?;
        self.next_seq = seq.next();
        Ok(seq)
    }

    fn command(&mut self, command: Command) {
        self.emitted.push(Emitted::Command(command));
    }

    fn annotation(&mut self, annotation: Annotation) {
        self.emitted.push(Emitted::Annotation(annotation));
    }

    /// Checks a call against the run's action set and the action's pinned input schema
    /// (`archi/04-tools.md`, "Action sets and code generation"). The message is the `TypeError`
    /// the code sees at its call.
    fn check_call(&mut self, action: &str, args: &Value) -> Result<(), String> {
        let Some(pinned) = self.action_set.action(action) else {
            return Err(match action {
                "signalUrl" => {
                    "signalUrl() needs the built-in signalUrl action, which this run's action set \
                              does not have"
                        .to_owned()
                }
                "http" => {
                    "fetch() needs the built-in http action, which this run's action set does \
                           not have"
                        .to_owned()
                }
                _ => format!("{action} is not an action of this run's action set"),
            });
        };
        if let ActionRoute::Builtin(Builtin::Workflow { workflow, op }) = &pinned.route {
            let (workflow, op) = (workflow.clone(), *op);
            return match op {
                WorkflowOp::Start => self.check_start(&workflow, args),
                WorkflowOp::Signal => self.check_signal(&workflow, args),
            };
        }
        let validator = self
            .validators
            .entry(action.to_owned())
            .or_insert_with(|| Validator::compile(&pinned.input_schema));
        check_args(validator, action, args)
    }

    /// Checks a value against one schema of a pinned workflow: `input`, `output` or
    /// `signal:<name>`. `None` when the workflow does not declare that schema.
    fn check_workflow_value(
        &mut self,
        workflow: &str,
        schema: &str,
        root: &str,
        value: &Value,
    ) -> Option<Result<(), Refused>> {
        let key = (workflow.to_owned(), schema.to_owned());
        if !self.workflow_validators.contains_key(&key) {
            let pinned = self.action_set.workflow(workflow)?;
            let json = match schema.strip_prefix("signal:") {
                Some(signal) => pinned.schemas.signals.get(signal)?,
                None if schema == "input" => &pinned.schemas.input,
                None => &pinned.schemas.output,
            };
            self.workflow_validators
                .insert(key.clone(), Validator::compile(json));
        }
        Some(self.workflow_validators[&key].check(root, value))
    }

    /// `workflows.<name>.start`: `{ input, detached?, labels?, ttl? }`, the input matching the
    /// workflow's input schema (`archi/03-execution.md`, "Workflows across runs").
    fn check_start(&mut self, workflow: &str, args: &Value) -> Result<(), String> {
        let Value::Object(fields) = args else {
            return Err(format!("{workflow}.start() takes the run's input"));
        };
        if let Some(unknown) = fields
            .keys()
            .find(|key| !matches!(key.as_str(), "input" | "detached" | "labels" | "ttl"))
        {
            return Err(format!(
                "{workflow}.start() takes no option {unknown:?}: its options are detached, \
                 labels and ttl"
            ));
        }
        let input = fields.get("input").unwrap_or(&Value::Null);
        match self.check_workflow_value(workflow, "input", "input", input) {
            Some(Ok(())) => {}
            Some(Err(refused)) => {
                return Err(refused_value(
                    &format!("invalid input for workflow {workflow}"),
                    &format!("the input schema of workflow {workflow}"),
                    refused,
                ));
            }
            None => {
                return Err(format!(
                    "{workflow} is not a workflow of this run's action set"
                ));
            }
        }
        if fields
            .get("detached")
            .is_some_and(|detached| !detached.is_boolean())
        {
            return Err("the option detached is a boolean".into());
        }
        if let Some(labels) = fields.get("labels") {
            let strings = labels
                .as_object()
                .is_some_and(|labels| labels.values().all(Value::is_string));
            if !strings {
                return Err("the option labels maps names to strings".into());
            }
        }
        if let Some(ttl) = fields.get("ttl") {
            let valid = match ttl {
                Value::Number(ms) => ms.as_f64().is_some_and(|ms| ms.is_finite() && ms >= 0.0),
                Value::String(text) => parse_duration(text).is_ok(),
                _ => false,
            };
            if !valid {
                return Err(format!(
                    "invalid ttl {ttl}: a number of milliseconds, or a duration such as \"2d\""
                ));
            }
        }
        Ok(())
    }

    /// `workflows.<name>.signal`: `{ run, name, payload }`, the name one the workflow declares
    /// and the payload matching that signal's schema.
    fn check_signal(&mut self, workflow: &str, args: &Value) -> Result<(), String> {
        let run = args.get("run").and_then(Value::as_str).unwrap_or_default();
        if run.parse::<RunId>().is_err() {
            return Err(format!("{run:?} is not a run id"));
        }
        let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
        let payload = args.get("payload").unwrap_or(&Value::Null);
        match self.check_workflow_value(workflow, &format!("signal:{name}"), "payload", payload) {
            Some(Ok(())) => Ok(()),
            Some(Err(refused)) => Err(refused_value(
                &format!("invalid payload for the signal {name:?} of workflow {workflow}"),
                &format!("the schema of the signal {name:?} of workflow {workflow}"),
                refused,
            )),
            None => {
                let declared: Vec<String> = self
                    .action_set
                    .workflow(workflow)
                    .map(|pinned| {
                        pinned
                            .schemas
                            .signals
                            .keys()
                            .map(|name| format!("{name:?}"))
                            .collect()
                    })
                    .unwrap_or_default();
                Err(format!(
                    "workflow {workflow} declares no signal {name:?} (it declares {})",
                    if declared.is_empty() {
                        "none".to_owned()
                    } else {
                        declared.join(", ")
                    }
                ))
            }
        }
    }

    /// What `result()` checks of a run's output: `""` when the pinned output schema of
    /// `workflow` takes it, else why not.
    fn check_output(&mut self, workflow: &str, output: &Value) -> String {
        match self.check_workflow_value(workflow, "output", "output", output) {
            Some(Ok(())) | None => String::new(),
            Some(Err(Refused::Invalid(problems))) => problems,
            Some(Err(Refused::Unusable(error))) => {
                format!("the output schema of workflow {workflow} cannot be used: {error}")
            }
        }
    }
}

/// A value a workflow's schema refused, as the `TypeError` the code sees.
fn refused_value(invalid: &str, schema: &str, refused: Refused) -> String {
    match refused {
        Refused::Invalid(problems) => format!("{invalid}: {problems}"),
        Refused::Unusable(error) => {
            format!("{schema} cannot be used, so the value cannot be checked: {error}")
        }
    }
}

fn derive_key(domain: &[u8], seed: &Seed) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(seed.as_bytes());
    hasher.finalize().into()
}

/// A number of bytes as a limit is written: in MiB when it is a whole number of them.
/// How the heap limit ends a call.
pub(crate) fn heap_exceeded(heap_bytes: usize) -> String {
    format!(
        "the code used more than {} MiB of heap",
        heap_bytes / (1024 * 1024)
    )
}

/// The most elements one call of a built-in may create: each takes at least 8 bytes of heap
/// (pointer compression is off), and past 2^27 V8 aborts the process ("invalid size").
fn max_elements(heap_bytes: usize) -> usize {
    (heap_bytes / 8).min(1 << 27)
}

fn size(bytes: usize) -> String {
    const MIB: usize = 1024 * 1024;
    if bytes > 0 && bytes.is_multiple_of(MIB) {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// Checks an action's args, as the `TypeError` the code sees at its call.
fn check_args(validator: &Validator, action: &str, args: &Value) -> Result<(), String> {
    validator
        .check("args", args)
        .map_err(|refused| match refused {
            Refused::Unusable(error) => format!(
                "the pinned input schema of {action} cannot be used, so the call cannot be checked: \
             {error}"
            ),
            Refused::Invalid(problems) => format!("invalid arguments for {action}: {problems}"),
        })
}

/// A position from the glue: 0 when the command was not issued from the code's source.
fn callsite(line: u32, column: u32) -> Option<Callsite> {
    (line > 0).then_some(Callsite { line, column })
}

fn parse_json(text: &str, what: &str) -> Result<Value, JsErrorBox> {
    serde_json::from_str(text)
        .map_err(|err| JsErrorBox::type_error(format!("{what} is not valid JSON: {err}")))
}

/// A number of milliseconds (written by the glue as a string), or `"1h30m"`-style text.
fn parse_script_duration(text: &str) -> Result<Duration, JsErrorBox> {
    if let Ok(ms) = text.parse::<f64>() {
        if ms.is_finite() && ms >= 0.0 && ms < u64::MAX as f64 {
            return Ok(Duration::from_millis(ms.round() as u64));
        }
        return Err(JsErrorBox::type_error(format!("invalid duration: {text}")));
    }
    parse_duration(text)
        .map_err(|err| JsErrorBox::type_error(format!("invalid duration {text:?}: {err}")))
}

/// Ends the call as the heap limit does. The glue calls it before a built-in V8 would run to
/// its end without an interrupt check, when that built-in must create more elements than the
/// heap holds.
#[op2(fast)]
fn op_rewind_heap_exceeded(state: &mut OpState) {
    state.borrow_mut::<ExecState>().exceed_heap();
}

#[op2(fast)]
fn op_rewind_now(state: &mut OpState) -> f64 {
    state.borrow::<ExecState>().now.as_millis() as f64
}

/// Fills `bytes` from the run's ChaCha20 keystream.
#[op2(fast)]
fn op_rewind_random_fill(state: &mut OpState, #[buffer] bytes: &mut [u8]) {
    bytes.fill(0);
    state
        .borrow_mut::<ExecState>()
        .crypto
        .apply_keystream(bytes);
}

/// Records a call, once its arguments match the action's pinned input schema. A call that
/// does not is refused with a `TypeError`, and nothing is recorded.
#[op2(fast)]
fn op_rewind_call_action(
    state: &mut OpState,
    #[string] action: &str,
    #[string] args: &str,
    line: u32,
    column: u32,
) -> Result<u32, JsErrorBox> {
    let state = state.borrow_mut::<ExecState>();
    // Before parsing: the arguments as values take more memory than as text.
    if !state.admit(action.len().saturating_add(args.len())) {
        return Err(over_bound());
    }
    let args = parse_json(args, "the arguments")?;
    state
        .check_call(action, &args)
        .map_err(JsErrorBox::type_error)?;
    let seq = state.take_seq()?;
    state.command(Command::ScheduleCall {
        seq,
        action: action.to_owned(),
        args: Payload::Value(args),
        callsite: callsite(line, column),
    });
    Ok(seq.0)
}

/// What `result()` checks of the output of a run of `workflow`: `""` when its pinned output
/// schema takes it, else the problems. Deterministic: the schema is pinned.
#[op2]
#[string]
fn op_rewind_check_workflow_output(
    state: &mut OpState,
    #[string] workflow: &str,
    #[string] output: &str,
) -> Result<String, JsErrorBox> {
    let state = state.borrow_mut::<ExecState>();
    let output = parse_json(output, "the output")?;
    Ok(state.check_output(workflow, &output))
}

#[op2(fast)]
fn op_rewind_start_timer(
    state: &mut OpState,
    #[string] duration: &str,
    line: u32,
    column: u32,
) -> Result<u32, JsErrorBox> {
    let duration = parse_script_duration(duration)?;
    let state = state.borrow_mut::<ExecState>();
    if !state.admit(0) {
        return Err(over_bound());
    }
    let seq = state.take_seq()?;
    state.command(Command::StartTimer {
        seq,
        duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        fire_at: state.now.saturating_add(duration),
        callsite: callsite(line, column),
    });
    Ok(seq.0)
}

/// The error of an op whose command was dropped: the execution is already terminating, so the
/// code never sees it.
fn over_bound() -> JsErrorBox {
    JsErrorBox::range_error("the code emitted more than one trigger's response may hold")
}

#[op2(fast)]
fn op_rewind_cancel_timer(state: &mut OpState, seq: u32) {
    let state = state.borrow_mut::<ExecState>();
    if state.admit(0) {
        state.command(Command::CancelTimer { seq: Seq(seq) });
    }
}

// Not a fast call: copying a string built by concatenation flattens it on the V8 heap.
#[op2(nofast)]
fn op_rewind_log(
    scope: &mut v8::PinScope,
    state: &mut OpState,
    #[string] level: &str,
    message: v8::Local<v8::String>,
    line: u32,
    column: u32,
) {
    let level = match level {
        "debug" => LogLevel::Debug,
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        _ => LogLevel::Info,
    };
    let state = state.borrow_mut::<ExecState>();
    if let Some([message]) = state.admit_strings(scope, [message]) {
        state.annotation(Annotation::Log {
            level,
            message,
            callsite: callsite(line, column),
        });
    }
}

// Not a fast call: copying a string built by concatenation flattens it on the V8 heap.
/// `timeout` is the `seq` of the timeout's timer, 0 for none.
#[op2(nofast)]
fn op_rewind_signal_awaited(
    scope: &mut v8::PinScope,
    state: &mut OpState,
    name: v8::Local<v8::String>,
    timeout: u32,
    line: u32,
    column: u32,
) {
    let state = state.borrow_mut::<ExecState>();
    if let Some([name]) = state.admit_strings(scope, [name]) {
        state.annotation(Annotation::SignalAwaited {
            name,
            timeout: (timeout > 0).then_some(Seq(timeout)),
            callsite: callsite(line, column),
        });
    }
}

#[op2(fast)]
fn op_rewind_complete(state: &mut OpState, #[string] output: &str) -> Result<(), JsErrorBox> {
    let state = state.borrow_mut::<ExecState>();
    // Before parsing: the output as a value takes more memory than as text.
    if !state.admit(output.len()) {
        return Err(over_bound());
    }
    let output = parse_json(output, "the output of main")?;
    state.command(Command::CompleteRun {
        output: Payload::Value(output),
    });
    Ok(())
}

// Not a fast call: copying a string built by concatenation flattens it on the V8 heap.
#[op2(nofast)]
fn op_rewind_fail(
    scope: &mut v8::PinScope,
    state: &mut OpState,
    name: v8::Local<v8::String>,
    message: v8::Local<v8::String>,
    stack: v8::Local<v8::String>,
) {
    let state = state.borrow_mut::<ExecState>();
    if let Some([name, message, stack]) = state.admit_strings(scope, [name, message, stack]) {
        state.command(Command::FailRun {
            error: ErrorInfo {
                name,
                message,
                stack: (!stack.is_empty()).then_some(stack),
            },
        });
    }
}

extension!(
    rewind_sandbox,
    deps = [deno_web],
    ops = [
        op_rewind_heap_exceeded,
        op_rewind_now,
        op_rewind_random_fill,
        op_rewind_call_action,
        op_rewind_check_workflow_output,
        op_rewind_start_timer,
        op_rewind_cancel_timer,
        op_rewind_log,
        op_rewind_signal_awaited,
        op_rewind_complete,
        op_rewind_fail,
    ],
);

/// The extensions of every isolate, in order: the snapshot and the isolates started from it
/// must have the same.
pub(crate) fn extensions() -> Vec<deno_core::Extension> {
    vec![
        deno_webidl::deno_webidl::init(),
        deno_web::deno_web::init(
            std::sync::Arc::new(deno_web::BlobStore::default()),
            None,
            false,
            deno_web::InMemoryBroadcastChannel::default(),
        ),
        rewind_sandbox::init(),
    ]
}

#[cfg(test)]
mod tests {
    use rewind_core::{Action, ActionRoute};

    use super::*;

    #[test]
    fn generators_are_keyed_by_the_seed_and_independent() {
        let action_set = ActionSet::default();
        let run = RunId::from_u128(1);
        let a = ExecState::new(
            run,
            &Seed::from_bytes([1; 32]),
            action_set.clone(),
            Timestamp(0),
        );
        let b = ExecState::new(
            run,
            &Seed::from_bytes([1; 32]),
            action_set.clone(),
            Timestamp(0),
        );
        let c = ExecState::new(run, &Seed::from_bytes([2; 32]), action_set, Timestamp(0));
        assert_eq!(a.math_random_state, b.math_random_state);
        assert_ne!(a.math_random_state, c.math_random_state);

        let keystream = |mut state: ExecState| {
            let mut bytes = [0u8; 16];
            state.crypto.apply_keystream(&mut bytes);
            bytes
        };
        assert_eq!(keystream(a), keystream(b));
    }

    #[test]
    fn script_durations_are_milliseconds_or_text() {
        assert_eq!(
            parse_script_duration("1500").unwrap(),
            Duration::from_millis(1500)
        );
        assert_eq!(parse_script_duration("0.4").unwrap(), Duration::ZERO);
        assert_eq!(
            parse_script_duration("1h").unwrap(),
            Duration::from_secs(3600)
        );
        assert!(parse_script_duration("-1").is_err());
        assert!(parse_script_duration("NaN").is_err());
        assert!(parse_script_duration("soon").is_err());
    }

    fn state_with(schema: Value) -> ExecState {
        let action = Action::new(
            "github.list_issues",
            ActionRoute::Harness {
                group: Some("github".into()),
            },
            schema,
        );
        let action_set = ActionSet::new([], [action]).unwrap();
        ExecState::new(
            RunId::from_u128(1),
            &Seed::from_bytes([0; 32]),
            action_set,
            Timestamp(0),
        )
    }

    #[test]
    fn args_are_checked_against_the_pinned_schema() {
        let mut state = state_with(json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string" },
                "state": { "enum": ["open", "closed"] },
                "labels": { "type": "array", "items": { "type": "string" } },
                "a-b": { "type": "integer" },
            },
            "required": ["repo"],
            "additionalProperties": false,
        }));
        let action = "github.list_issues";
        assert_eq!(
            state.check_call(action, &json!({ "repo": "acme/api" })),
            Ok(())
        );
        assert_eq!(
            state.check_call(
                action,
                &json!({ "repo": 42, "state": "all", "labels": ["x", 1], "a-b": 1.5 })
            ),
            Err(
                "invalid arguments for github.list_issues: args.labels[1]: 1 is not of type \
                 \"string\"; args.repo: 42 is not of type \"string\"; args.state: \"all\" is not \
                 one of \"open\" or \"closed\"; args[\"a-b\"]: 1.5 is not of type \"integer\""
                    .into()
            )
        );
        assert_eq!(
            state.check_call(action, &json!({})),
            Err(
                "invalid arguments for github.list_issues: args: \"repo\" is a required property"
                    .into()
            )
        );
        assert_eq!(
            state.check_call("slack.post", &json!({})),
            Err("slack.post is not an action of this run's action set".into())
        );
        assert!(
            state
                .check_call("signalUrl", &json!({ "name": "x" }))
                .unwrap_err()
                .starts_with("signalUrl() needs the built-in signalUrl action")
        );
    }

    #[test]
    fn a_long_list_of_problems_is_cut() {
        let mut state = state_with(json!({
            "type": "object",
            "additionalProperties": { "type": "string" },
        }));
        let args: serde_json::Map<String, Value> =
            (0..8).map(|i| (format!("k{i}"), json!(i))).collect();
        let message = state
            .check_call("github.list_issues", &Value::Object(args))
            .unwrap_err();
        assert!(message.ends_with("(and 3 more)"), "{message}");
        assert_eq!(message.matches("is not of type").count(), 5);
    }

    #[test]
    fn an_unusable_schema_refuses_every_call() {
        let mut state = state_with(json!({ "$ref": "https://schemas.example.com/issue.json" }));
        let message = state
            .check_call("github.list_issues", &json!({}))
            .unwrap_err();
        assert!(
            message.starts_with("the pinned input schema of github.list_issues cannot be used"),
            "{message}"
        );
    }

    #[test]
    fn an_empty_schema_accepts_anything() {
        let mut state = state_with(json!({}));
        assert_eq!(
            state.check_call("github.list_issues", &json!([1, "x"])),
            Ok(())
        );
    }
}
