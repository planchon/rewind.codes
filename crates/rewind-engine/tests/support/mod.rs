//! Test support: a fake executor whose "scripts" are Rust programs, a recording dispatcher, and
//! the stored format of tests; [`plane`] wires them as nodes stepped by the driver in virtual
//! time.

#![allow(dead_code, unreachable_pub)]

pub mod interleaved;
pub mod plane;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rewind_core::{
    Annotation, Callsite, Command, Epoch, JsonValue, Payload, RunId, Script, ScriptError, Seq,
    Timestamp, ToolFailure, Toolset,
};
use rewind_engine::StartRun;
use rewind_engine::ports::{
    CallAuth, CompiledScript, Delivery, Dispatched, Emitted, ExecutorError, InvalidScript,
    ScriptExecutor, ScriptInstance, StartScript, ToolCall, ToolDispatcher,
};

/// 2026-01-01T00:00:00Z
pub const T0: Timestamp = Timestamp(1_767_225_600_000);
pub const HOUR: u64 = 3_600_000;
pub const DAY: u64 = 24 * HOUR;

/// The CPU time a call into a fake script reports, unless its program says otherwise
/// ([`FakeRun::spend`]).
pub const CALL_CPU: Duration = Duration::from_millis(2);

/// A run of the program named `name`.
pub fn request(name: &str, input: JsonValue) -> StartRun {
    StartRun::new(
        Script::new(name),
        Toolset::from_host_tools(["a", "b", "cleanup"]).unwrap(),
        input,
    )
}

// -------------------------------------------------------------------------------------------
// Fake executor

/// A script written in Rust. It reacts to deliveries, as the JS glue would, by emitting
/// commands through [`FakeRun`].
pub trait Program: Send + Sync + 'static {
    fn start(&self, run: &mut FakeRun);
    fn deliver(&self, run: &mut FakeRun, delivery: &Delivery);
}

struct FnProgram<S, D> {
    start: S,
    deliver: D,
}

impl<S, D> Program for FnProgram<S, D>
where
    S: Fn(&mut FakeRun) + Send + Sync + 'static,
    D: Fn(&mut FakeRun, &Delivery) + Send + Sync + 'static,
{
    fn start(&self, run: &mut FakeRun) {
        (self.start)(run);
    }

    fn deliver(&self, run: &mut FakeRun, delivery: &Delivery) {
        (self.deliver)(run, delivery);
    }
}

/// The state of one fake instance.
#[derive(Debug)]
pub struct FakeRun {
    pub run_id: RunId,
    pub input: JsonValue,
    pub now: Timestamp,
    next_seq: u32,
    emitted: Vec<Emitted>,
    refused: Option<ExecutorError>,
    hung: bool,
    /// The CPU time this call reports.
    cpu: Duration,
}

impl FakeRun {
    /// Makes this call report `cpu` of CPU time, instead of [`CALL_CPU`].
    pub fn spend(&mut self, cpu: Duration) {
        self.cpu = cpu;
    }

    /// Makes the start of this instance fail with `error`, as an executor would.
    pub fn refuse(&mut self, error: ExecutorError) {
        self.refused = Some(error);
    }

    /// Makes this call into the script never return, as a script that brings its worker down
    /// does: the activation is left without a commit and without a recorded failure.
    pub fn hang(&mut self) {
        self.hung = true;
    }

    fn take_seq(&mut self) -> Seq {
        let seq = Seq(self.next_seq);
        self.next_seq += 1;
        seq
    }

    pub fn call(&mut self, tool: &str, args: JsonValue) -> Seq {
        let seq = self.take_seq();
        self.emitted
            .push(Emitted::Command(Command::ScheduleToolCall {
                seq,
                tool: tool.into(),
                args: args.into(),
                callsite: Some(Callsite {
                    line: seq.0,
                    column: 1,
                }),
            }));
        seq
    }

    pub fn timer(&mut self, duration_ms: u64) -> Seq {
        let seq = self.take_seq();
        self.emitted.push(Emitted::Command(Command::StartTimer {
            seq,
            duration_ms,
            fire_at: Timestamp(self.now.0 + duration_ms),
            callsite: None,
        }));
        seq
    }

    pub fn log(&mut self, message: &str) {
        self.emitted.push(Emitted::Annotation(Annotation::Log {
            level: rewind_core::LogLevel::Info,
            message: message.into(),
            callsite: None,
        }));
    }

    pub fn await_signal(&mut self, name: &str) {
        self.emitted
            .push(Emitted::Annotation(Annotation::SignalAwaited {
                name: name.into(),
                timeout: None,
                callsite: None,
            }));
    }

    pub fn complete(&mut self, output: JsonValue) {
        self.emitted.push(Emitted::Command(Command::CompleteRun {
            output: Payload::Value(output),
        }));
    }

    pub fn fail(&mut self, name: &str, message: &str) {
        self.emitted.push(Emitted::Command(Command::FailRun {
            error: ScriptError {
                name: name.into(),
                message: message.into(),
                stack: None,
            },
        }));
    }
}

/// Programs by name: a script's source is the name of its program. A source starting with
/// `syntax error` does not compile; the program `unloadable` fails to load. A program imports
/// every name of `rewind:tools` unless [`FakeExecutor::importing`] says which.
#[derive(Clone, Default)]
pub struct FakeExecutor {
    programs: HashMap<String, Arc<dyn Program>>,
    imports: HashMap<String, BTreeSet<String>>,
    starts: Arc<Mutex<usize>>,
}

impl std::fmt::Debug for FakeExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeExecutor").finish_non_exhaustive()
    }
}

impl FakeExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(
        mut self,
        name: &str,
        start: impl Fn(&mut FakeRun) + Send + Sync + 'static,
        deliver: impl Fn(&mut FakeRun, &Delivery) + Send + Sync + 'static,
    ) -> Self {
        self.programs
            .insert(name.into(), Arc::new(FnProgram { start, deliver }));
        self
    }

    /// The program `name` imports only these names from `rewind:tools`.
    pub fn importing(mut self, name: &str, imports: &[&str]) -> Self {
        self.imports.insert(
            name.into(),
            imports.iter().map(|name| (*name).to_owned()).collect(),
        );
        self
    }

    /// How many instances were started, replays included.
    pub fn starts(&self) -> usize {
        *self.starts.lock().unwrap()
    }
}

pub struct FakeInstance {
    program: Arc<dyn Program>,
    run: FakeRun,
    /// The CPU time of the last call.
    cpu: Duration,
}

impl ScriptExecutor for FakeExecutor {
    type Instance = FakeInstance;

    fn epochs(&self) -> &[Epoch] {
        &[Epoch(1)]
    }

    fn compile(
        &self,
        script: &Script,
        _toolset: &Toolset,
    ) -> Result<CompiledScript, InvalidScript> {
        if script.source().starts_with("syntax error") {
            return Err(InvalidScript {
                message: format!("{}: unexpected token", script.source()),
            });
        }
        Ok(CompiledScript {
            javascript: script.source().into(),
            source_map: String::new(),
        })
    }

    fn tool_imports(&self, script: &Script) -> Option<BTreeSet<String>> {
        self.imports.get(script.source()).cloned()
    }

    async fn start(
        &self,
        start: StartScript,
    ) -> Result<(FakeInstance, Vec<Emitted>), ExecutorError> {
        *self.starts.lock().unwrap() += 1;
        let name = start.compiled.javascript.as_str();
        if name == "unloadable" {
            return Err(ExecutorError::InvalidScript(
                "Error: top-level failure\n    at script.ts:1:7".into(),
            ));
        }
        let program = self
            .programs
            .get(name)
            .cloned()
            .ok_or_else(|| ExecutorError::Internal(format!("no program named {name}")))?;
        let mut run = FakeRun {
            run_id: start.run_id,
            input: start.input,
            now: start.started_at,
            next_seq: 1,
            emitted: Vec::new(),
            refused: None,
            hung: false,
            cpu: CALL_CPU,
        };
        program.start(&mut run);
        if run.hung {
            std::future::pending::<()>().await;
        }
        if let Some(error) = run.refused.take() {
            return Err(error);
        }
        let emitted = std::mem::take(&mut run.emitted);
        let cpu = run.cpu;
        Ok((FakeInstance { program, run, cpu }, emitted))
    }
}

impl ScriptInstance for FakeInstance {
    async fn deliver(
        &mut self,
        delivery: Delivery,
        now: Timestamp,
    ) -> Result<Vec<Emitted>, ExecutorError> {
        self.run.now = now;
        self.run.cpu = CALL_CPU;
        self.program.deliver(&mut self.run, &delivery);
        if self.run.hung {
            std::future::pending::<()>().await;
        }
        self.cpu = self.run.cpu;
        Ok(std::mem::take(&mut self.run.emitted))
    }

    fn cpu_time(&self) -> Duration {
        self.cpu
    }
}

// -------------------------------------------------------------------------------------------
// Dispatcher

type Handler = dyn Fn(&ToolCall, &CallAuth) -> Dispatched + Send + Sync;
type Calls = Arc<Mutex<Vec<(ToolCall, CallAuth)>>>;

/// Answers with a handler and records every call, with the authentication it was given.
#[derive(Clone)]
pub struct RecordingDispatcher {
    handler: Arc<Handler>,
    calls: Calls,
}

impl std::fmt::Debug for RecordingDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingDispatcher")
            .finish_non_exhaustive()
    }
}

impl RecordingDispatcher {
    pub fn new(
        handler: impl Fn(&ToolCall) -> Result<JsonValue, ToolFailure> + Send + Sync + 'static,
    ) -> Self {
        Self::dispatching(move |call, _| match handler(call) {
            Ok(result) => Dispatched::Result(result),
            Err(failure) => Dispatched::Failed(failure),
        })
    }

    /// Answers with whatever the handler says, given the call and its authentication: transient
    /// failures, deferrals, parked calls.
    pub fn dispatching(
        handler: impl Fn(&ToolCall, &CallAuth) -> Dispatched + Send + Sync + 'static,
    ) -> Self {
        Self {
            handler: Arc::new(handler),
            calls: Arc::default(),
        }
    }

    /// Answers every call with its args.
    pub fn echo() -> Self {
        Self::new(|call| Ok(call.args.clone()))
    }

    pub fn calls(&self) -> Vec<ToolCall> {
        let calls = self.calls.lock().unwrap();
        calls.iter().map(|(call, _)| call.clone()).collect()
    }

    /// The authentication each call was dispatched with, in order.
    pub fn auth(&self) -> Vec<CallAuth> {
        let calls = self.calls.lock().unwrap();
        calls.iter().map(|(_, auth)| auth.clone()).collect()
    }
}

impl ToolDispatcher for RecordingDispatcher {
    async fn dispatch(&self, call: ToolCall, auth: CallAuth) -> Dispatched {
        let outcome = (self.handler)(&call, &auth);
        self.calls.lock().unwrap().push((call, auth));
        outcome
    }
}

/// The delivered value of a tool call, or panics.
pub fn result_of(delivery: &Delivery) -> &JsonValue {
    match delivery {
        Delivery::ToolCallCompleted { result, .. } => result,
        other => panic!("expected a tool result, got {other:?}"),
    }
}

// Tests and suites

/// The documents of tests, for the engine's tests: the engine does not know the stored format,
/// so these tests give it one that keeps each document's test in memory. A document is the
/// test's `Debug` form with its seed, which `Debug` redacts.
pub fn test_format() -> rewind_engine::suites::TestFormat {
    rewind_engine::suites::TestFormat {
        encode: encode_test,
        decode: decode_test,
    }
}

fn documents() -> &'static Mutex<HashMap<String, rewind_core::Test>> {
    static DOCUMENTS: std::sync::OnceLock<Mutex<HashMap<String, rewind_core::Test>>> =
        std::sync::OnceLock::new();
    DOCUMENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn encode_test(test: &rewind_core::Test) -> String {
    let document = format!("{test:?} seed={:?}", test.seed.map(|seed| *seed.as_bytes()));
    documents()
        .lock()
        .unwrap()
        .insert(document.clone(), test.clone());
    document
}

fn decode_test(document: &str) -> Result<rewind_core::Test, String> {
    documents()
        .lock()
        .unwrap()
        .get(document)
        .cloned()
        .ok_or_else(|| "not a document of this test format".to_owned())
}

/// What a save needs to run a suite, without TypeScript mock handlers.
pub fn kit() -> rewind_engine::suites::TestKit<'static, rewind_engine::testing::NoMockLoader> {
    rewind_engine::suites::TestKit {
        loader: &rewind_engine::testing::NoMockLoader,
        format: test_format(),
        options: rewind_engine::testing::TestOptions::default(),
    }
}
