use std::future::Future;

use ptc_core::{
    Annotation, CancelReason, Command, Epoch, Event, JsonValue, RunId, Script, Seed, Seq,
    Timestamp, ToolFailure, Toolset,
};

/// What an executor compiled from a script's TypeScript source. It is stored with the
/// script, so an upgrade of the transpiler never changes the code a run replays
/// (`archi/03-execution.md`, "Determinism epoch"). The engine never interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledScript {
    pub javascript: String,
    pub source_map: String,
}

/// A script that cannot run: a syntax error, a forbidden import… The message is written to
/// be read by a model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct InvalidScript {
    pub message: String,
}

/// Everything a script instance needs to run `main(input)`.
#[derive(Debug, Clone)]
pub struct StartScript {
    pub run_id: RunId,
    pub compiled: CompiledScript,
    pub toolset: Toolset,
    pub input: JsonValue,
    pub seed: Seed,
    pub epoch: Epoch,
    /// `recorded_at` of `run_started`: the script's `run.startedAt` and its first logical time.
    pub started_at: Timestamp,
}

/// A completion, with its payloads loaded: what reaches the script.
#[derive(Debug, Clone, PartialEq)]
pub enum Delivery {
    ToolCallCompleted { seq: Seq, result: JsonValue },
    ToolCallFailed { seq: Seq, error: ToolFailure },
    TimerFired { seq: Seq },
    SignalReceived { name: String, payload: JsonValue },
    CancelRequested { reason: CancelReason },
}

/// What a script emits in response to a trigger, in emission order.
#[derive(Debug, Clone, PartialEq)]
pub enum Emitted {
    Command(Command),
    Annotation(Annotation),
}

impl From<Emitted> for Event {
    fn from(emitted: Emitted) -> Self {
        match emitted {
            Emitted::Command(command) => Event::Command(command),
            Emitted::Annotation(annotation) => Event::Annotation(annotation),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecutorError {
    /// The script could not be loaded although it passed [`ScriptExecutor::compile`]: an
    /// error thrown by its top-level code, a default export that is not a function. It ends
    /// the run with `run_aborted { invalid_script }`.
    #[error("the script could not be loaded: {0}")]
    InvalidScript(String),
    /// A resource limit was exceeded (CPU time, heap). It ends the run with
    /// `run_aborted { limit_exceeded }`.
    #[error("{0}")]
    LimitExceeded(String),
    #[error("{0} is not supported by this executor")]
    UnsupportedEpoch(Epoch),
    /// A fault of the executor itself.
    #[error("internal executor error: {0}")]
    Internal(String),
}

/// Runs scripts in a sandbox (`archi/03-execution.md`, "Inside the isolate").
///
/// The executor only executes: it knows nothing about history, replay, storage or dispatch.
/// An instance is started with `main(input)`, receives deliveries one at a time, and after
/// each one returns what the script emitted once it is idle. Commands carry the `seq` the
/// script assigned, in emission order. The script's terminal command (`complete_run` or
/// `fail_run`) is emitted following the rules of "How a script ends".
pub trait ScriptExecutor: Send + Sync {
    type Instance: ScriptInstance;

    /// The determinism epochs this executor can run, oldest first. New runs start under the
    /// last one; a run of another epoch is never offered to this executor.
    fn epochs(&self) -> &[Epoch];

    /// Checks a script against the toolset it will run with, and compiles it. Everything that
    /// can be refused without running the script is refused here: syntax errors, imports of
    /// modules or names that do not exist, a missing default export. Imports are checked
    /// against the last epoch, the one new runs start under; the compiled form does not depend
    /// on the epoch nor on the toolset.
    fn compile(&self, script: &Script, toolset: &Toolset) -> Result<CompiledScript, InvalidScript>;

    /// Starts an instance under `epoch` and runs `main(input)` until the script is idle.
    fn start(
        &self,
        start: StartScript,
    ) -> impl Future<Output = Result<(Self::Instance, Vec<Emitted>), ExecutorError>> + Send;
}

/// A running script.
pub trait ScriptInstance: Send {
    /// Delivers one completion, recorded at `now` (the run's new logical time), and runs the
    /// script until it is idle.
    fn deliver(
        &mut self,
        delivery: Delivery,
        now: Timestamp,
    ) -> impl Future<Output = Result<Vec<Emitted>, ExecutorError>> + Send;
}
