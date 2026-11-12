//! The ops: the only way the script's glue talks to Rust.
//!
//! Ops are synchronous: they record a command, assign its `seq` and return it. Nothing is
//! asynchronous from V8's point of view; the glue turns a `seq` into a promise that a later
//! delivery settles.

use std::collections::BTreeMap;
use std::time::Duration;

use chacha20::ChaCha20;
use chacha20::cipher::{KeyIvInit, StreamCipher};
use deno_core::{OpState, extension, op2, v8};
use deno_error::JsErrorBox;
use ptc_core::{
    Annotation, Builtin, Callsite, Command, LogLevel, Payload, RunId, ScriptError, Seed, Seq,
    Timestamp, ToolRoute, Toolset, parse_duration,
};
use ptc_engine::ports::Emitted;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::isolate::SandboxLimits;

/// The state of one isolate, kept in deno_core's `OpState`.
pub(crate) struct ExecState {
    /// The logical time: `recorded_at` of the last delivered trigger.
    pub(crate) now: Timestamp,
    started_at: Timestamp,
    run_id: RunId,
    toolset: Toolset,
    next_seq: Option<Seq>,
    emitted: Vec<Emitted>,
    /// The bytes of text `emitted` holds.
    emitted_bytes: usize,
    /// Bounds on `emitted`, which lives outside V8's heap until the call ends.
    max_emitted: usize,
    max_emitted_bytes: usize,
    /// The heap limit, which bounds the elements one built-in call may create.
    heap_bytes: usize,
    /// Why the execution was terminated, once the script emitted more than the bounds allow.
    overflow: Option<String>,
    /// Terminates the execution from an op.
    isolate: Option<v8::IsolateHandle>,
    /// State of `Math.random` (sfc32), derived from the seed.
    math_random_state: [u32; 4],
    /// The keystream behind `crypto` (ChaCha20, RFC 8439), keyed by the seed.
    crypto: ChaCha20,
    /// The validators of the tools called so far, compiled from their pinned input schemas.
    validators: BTreeMap<String, Validator>,
}

impl ExecState {
    pub(crate) fn new(run_id: RunId, seed: &Seed, toolset: Toolset, started_at: Timestamp) -> Self {
        // Each generator gets its own key, derived from the seed: what `Math.random` reveals
        // tells nothing about `crypto`.
        let math = derive_key(b"ai-ptc/epoch-1/math-random", seed);
        let math_random_state = std::array::from_fn(|i| {
            u32::from_le_bytes([
                math[4 * i],
                math[4 * i + 1],
                math[4 * i + 2],
                math[4 * i + 3],
            ])
        });
        let key = derive_key(b"ai-ptc/epoch-1/crypto", seed);
        let limits = SandboxLimits::default();
        Self {
            now: started_at,
            started_at,
            run_id,
            toolset,
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
        }
    }

    /// What the glue's `init` hook receives: what belongs to this instance.
    pub(crate) fn init_config(&self) -> Value {
        json!({
            "runId": self.run_id.to_string(),
            "startedAt": self.started_at.as_millis(),
            "mathRandom": self.math_random_state,
            "http": self.toolset.tools().iter().any(|tool| {
                matches!(tool.route, ToolRoute::Builtin(Builtin::Http { .. }))
            }),
            "maxElements": max_elements(self.heap_bytes),
        })
    }

    /// Bounds what the script may emit reacting to one trigger, in the isolate `isolate`.
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

    /// Why the execution was terminated, if the script emitted more than the bounds allow or
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
    /// exceeded, and whatever the script runs before the termination takes effect is dropped.
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
            "the script emitted more than {exceeded} reacting to one trigger"
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
            .ok_or_else(|| JsErrorBox::range_error("the script emitted too many commands"))?;
        self.next_seq = seq.next();
        Ok(seq)
    }

    fn command(&mut self, command: Command) {
        self.emitted.push(Emitted::Command(command));
    }

    fn annotation(&mut self, annotation: Annotation) {
        self.emitted.push(Emitted::Annotation(annotation));
    }

    /// Checks a call against the run's toolset and the tool's pinned input schema
    /// (`archi/04-tools.md`, "Toolsets and code generation"). The message is the `TypeError`
    /// the script sees at its call.
    fn check_call(&mut self, tool: &str, args: &Value) -> Result<(), String> {
        let Some(pinned) = self.toolset.tool(tool) else {
            return Err(match tool {
                "webhook" => "webhook() needs the built-in webhook tool, which this run's toolset \
                              does not have"
                    .to_owned(),
                "http" => "fetch() needs the built-in http tool, which this run's toolset does \
                           not have"
                    .to_owned(),
                _ => format!("{tool} is not a tool of this run's toolset"),
            });
        };
        let validator = self
            .validators
            .entry(tool.to_owned())
            .or_insert_with(|| Validator::compile(&pinned.input_schema));
        validator.check(tool, args)
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
        "the script used more than {} MiB of heap",
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

/// A tool's pinned input schema, compiled once per instance.
enum Validator {
    /// `{}` or `true`: any arguments.
    Any,
    Schema(Box<jsonschema::Validator>),
    /// The schema cannot be used (an invalid schema, a `$ref` outside it): every call of the
    /// tool is refused with this message, rather than sent unchecked.
    Unusable(String),
}

/// At most this many problems are listed in one message.
const MAX_PROBLEMS: usize = 5;

impl Validator {
    fn compile(schema: &Value) -> Self {
        match schema {
            Value::Bool(true) => return Self::Any,
            Value::Object(object) if object.is_empty() => return Self::Any,
            _ => {}
        }
        match jsonschema::validator_for(schema) {
            Ok(validator) => Self::Schema(Box::new(validator)),
            Err(error) => Self::Unusable(error.to_string()),
        }
    }

    /// Every problem, each with its place in the arguments, sorted so that the message does not
    /// depend on the order the validator walks the schema in.
    fn check(&self, tool: &str, args: &Value) -> Result<(), String> {
        let validator = match self {
            Self::Any => return Ok(()),
            Self::Unusable(error) => {
                return Err(format!(
                    "the pinned input schema of {tool} cannot be used, so the call cannot be \
                     checked: {error}"
                ));
            }
            Self::Schema(validator) => validator,
        };
        let mut problems: Vec<String> = validator
            .iter_errors(args)
            .map(|error| format!("{}: {error}", path(args, error.instance_path())))
            .collect();
        if problems.is_empty() {
            return Ok(());
        }
        problems.sort();
        problems.dedup();
        let more = problems.len().saturating_sub(MAX_PROBLEMS);
        problems.truncate(MAX_PROBLEMS);
        let mut message = format!("invalid arguments for {tool}: {}", problems.join("; "));
        if more > 0 {
            message.push_str(&format!(" (and {more} more)"));
        }
        Err(message)
    }
}

/// A place in the arguments as a script writes it: `args`, `args.repo`, `args.labels[0]`,
/// `args["a-b"]`.
fn path(args: &Value, location: &jsonschema::paths::Location) -> String {
    let mut path = String::from("args");
    let mut value = Some(args);
    for segment in location.iter() {
        let key = segment.to_string();
        match value {
            Some(Value::Array(items)) => {
                path.push_str(&format!("[{key}]"));
                value = key.parse::<usize>().ok().and_then(|index| items.get(index));
            }
            _ => {
                if is_identifier(&key) {
                    path.push('.');
                    path.push_str(&key);
                } else {
                    path.push_str(&format!("[{}]", Value::String(key.clone())));
                }
                value = value.and_then(|value| value.get(&key));
            }
        }
    }
    path
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// A position from the glue: 0 when the command was not issued from the script's source.
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
fn op_ptc_heap_exceeded(state: &mut OpState) {
    state.borrow_mut::<ExecState>().exceed_heap();
}

#[op2(fast)]
fn op_ptc_now(state: &mut OpState) -> f64 {
    state.borrow::<ExecState>().now.as_millis() as f64
}

/// Fills `bytes` from the run's ChaCha20 keystream.
#[op2(fast)]
fn op_ptc_random_fill(state: &mut OpState, #[buffer] bytes: &mut [u8]) {
    bytes.fill(0);
    state
        .borrow_mut::<ExecState>()
        .crypto
        .apply_keystream(bytes);
}

/// Records a tool call, once its arguments match the tool's pinned input schema. A call that
/// does not is refused with a `TypeError`, and nothing is recorded.
#[op2(fast)]
fn op_ptc_call_tool(
    state: &mut OpState,
    #[string] tool: &str,
    #[string] args: &str,
    line: u32,
    column: u32,
) -> Result<u32, JsErrorBox> {
    let state = state.borrow_mut::<ExecState>();
    // Before parsing: the arguments as values take more memory than as text.
    if !state.admit(tool.len().saturating_add(args.len())) {
        return Err(over_bound());
    }
    let args = parse_json(args, "the arguments")?;
    state
        .check_call(tool, &args)
        .map_err(JsErrorBox::type_error)?;
    let seq = state.take_seq()?;
    state.command(Command::ScheduleToolCall {
        seq,
        tool: tool.to_owned(),
        args: Payload::Value(args),
        callsite: callsite(line, column),
    });
    Ok(seq.0)
}

#[op2(fast)]
fn op_ptc_start_timer(
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
/// script never sees it.
fn over_bound() -> JsErrorBox {
    JsErrorBox::range_error("the script emitted more than one trigger's response may hold")
}

#[op2(fast)]
fn op_ptc_cancel_timer(state: &mut OpState, seq: u32) {
    let state = state.borrow_mut::<ExecState>();
    if state.admit(0) {
        state.command(Command::CancelTimer { seq: Seq(seq) });
    }
}

// Not a fast call: copying a string built by concatenation flattens it on the V8 heap.
#[op2(nofast)]
fn op_ptc_log(
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
fn op_ptc_signal_awaited(
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
fn op_ptc_complete(state: &mut OpState, #[string] output: &str) -> Result<(), JsErrorBox> {
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
fn op_ptc_fail(
    scope: &mut v8::PinScope,
    state: &mut OpState,
    name: v8::Local<v8::String>,
    message: v8::Local<v8::String>,
    stack: v8::Local<v8::String>,
) {
    let state = state.borrow_mut::<ExecState>();
    if let Some([name, message, stack]) = state.admit_strings(scope, [name, message, stack]) {
        state.command(Command::FailRun {
            error: ScriptError {
                name,
                message,
                stack: (!stack.is_empty()).then_some(stack),
            },
        });
    }
}

extension!(
    ptc_sandbox,
    deps = [deno_web],
    ops = [
        op_ptc_heap_exceeded,
        op_ptc_now,
        op_ptc_random_fill,
        op_ptc_call_tool,
        op_ptc_start_timer,
        op_ptc_cancel_timer,
        op_ptc_log,
        op_ptc_signal_awaited,
        op_ptc_complete,
        op_ptc_fail,
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
        ptc_sandbox::init(),
    ]
}

#[cfg(test)]
mod tests {
    use ptc_core::{Tool, ToolRoute};

    use super::*;

    #[test]
    fn generators_are_keyed_by_the_seed_and_independent() {
        let toolset = Toolset::default();
        let run = RunId::from_u128(1);
        let a = ExecState::new(
            run,
            &Seed::from_bytes([1; 32]),
            toolset.clone(),
            Timestamp(0),
        );
        let b = ExecState::new(
            run,
            &Seed::from_bytes([1; 32]),
            toolset.clone(),
            Timestamp(0),
        );
        let c = ExecState::new(run, &Seed::from_bytes([2; 32]), toolset, Timestamp(0));
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
        let tool = Tool::new(
            "github.list_issues",
            ToolRoute::Host {
                group: Some("github".into()),
            },
            schema,
        );
        let toolset = Toolset::new([], [tool]).unwrap();
        ExecState::new(
            RunId::from_u128(1),
            &Seed::from_bytes([0; 32]),
            toolset,
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
        let tool = "github.list_issues";
        assert_eq!(
            state.check_call(tool, &json!({ "repo": "acme/api" })),
            Ok(())
        );
        assert_eq!(
            state.check_call(
                tool,
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
            state.check_call(tool, &json!({})),
            Err(
                "invalid arguments for github.list_issues: args: \"repo\" is a required property"
                    .into()
            )
        );
        assert_eq!(
            state.check_call("slack.post", &json!({})),
            Err("slack.post is not a tool of this run's toolset".into())
        );
        assert!(
            state
                .check_call("webhook", &json!({ "name": "x" }))
                .unwrap_err()
                .starts_with("webhook() needs the built-in webhook tool")
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
        assert_eq!(message.matches("is not of type").count(), MAX_PROBLEMS);
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
