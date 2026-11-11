//! Test support: a fake executor whose "scripts" are Rust programs, a recording dispatcher,
//! and an engine wired with in-memory adapters and a virtual clock.

#![allow(dead_code, unreachable_pub)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ptc_core::{
    Annotation, Callsite, Command, Epoch, JsonValue, Payload, RunId, Script, ScriptError, Seq,
    Timestamp, ToolFailure, Toolset,
};
use ptc_engine::adapters::{MemoryBlobs, MemoryJournal, SeededEntropy, VirtualClock};
use ptc_engine::ports::{
    CompiledScript, Delivery, Emitted, ExecutorError, InvalidScript, ScriptExecutor,
    ScriptInstance, StartScript, ToolCall, ToolDispatcher,
};
use ptc_engine::{Adapters, Engine, EngineConfig, StartRun};

/// 2026-01-01T00:00:00Z
pub const T0: Timestamp = Timestamp(1_767_225_600_000);
pub const HOUR: u64 = 3_600_000;
pub const DAY: u64 = 24 * HOUR;

pub type TestEngine = Engine<
    MemoryJournal,
    MemoryBlobs,
    FakeExecutor,
    RecordingDispatcher,
    VirtualClock,
    SeededEntropy,
>;

pub fn engine(programs: FakeExecutor, dispatcher: RecordingDispatcher) -> TestEngine {
    engine_with(programs, dispatcher, EngineConfig::default())
}

pub fn engine_with(
    programs: FakeExecutor,
    dispatcher: RecordingDispatcher,
    config: EngineConfig,
) -> TestEngine {
    Engine::new(
        Adapters {
            journal: MemoryJournal::new(),
            blobs: MemoryBlobs::new(),
            executor: programs,
            dispatcher,
            clock: VirtualClock::new(T0),
            entropy: SeededEntropy::new(42),
        },
        config,
    )
}

/// A run of the program named `name`.
pub fn request(name: &str, input: JsonValue) -> StartRun {
    StartRun::new(
        Script::new(name),
        Toolset::from_names(["a", "b", "cleanup"]),
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
}

impl FakeRun {
    /// Makes the start of this instance fail with `error`, as an executor would.
    pub fn refuse(&mut self, error: ExecutorError) {
        self.refused = Some(error);
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
            level: ptc_core::LogLevel::Info,
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
/// `syntax error` does not compile; the program `unloadable` fails to load.
#[derive(Clone, Default)]
pub struct FakeExecutor {
    programs: HashMap<String, Arc<dyn Program>>,
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

    /// How many instances were started, replays included.
    pub fn starts(&self) -> usize {
        *self.starts.lock().unwrap()
    }
}

pub struct FakeInstance {
    program: Arc<dyn Program>,
    run: FakeRun,
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
        };
        program.start(&mut run);
        if let Some(error) = run.refused.take() {
            return Err(error);
        }
        let emitted = std::mem::take(&mut run.emitted);
        Ok((FakeInstance { program, run }, emitted))
    }
}

impl ScriptInstance for FakeInstance {
    async fn deliver(
        &mut self,
        delivery: Delivery,
        now: Timestamp,
    ) -> Result<Vec<Emitted>, ExecutorError> {
        self.run.now = now;
        self.program.deliver(&mut self.run, &delivery);
        Ok(std::mem::take(&mut self.run.emitted))
    }
}

// -------------------------------------------------------------------------------------------
// Dispatcher

type Handler = dyn Fn(&ToolCall) -> Result<JsonValue, ToolFailure> + Send + Sync;

/// Answers with a handler and records every call.
#[derive(Clone)]
pub struct RecordingDispatcher {
    handler: Arc<Handler>,
    calls: Arc<Mutex<Vec<ToolCall>>>,
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
        self.calls.lock().unwrap().clone()
    }
}

impl ToolDispatcher for RecordingDispatcher {
    async fn dispatch(&self, call: ToolCall) -> Result<JsonValue, ToolFailure> {
        let outcome = (self.handler)(&call);
        self.calls.lock().unwrap().push(call);
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
