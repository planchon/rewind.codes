use std::collections::BTreeSet;
use std::future::Future;
use std::time::Duration;

use rewind_core::{
    ActionFailure, ActionSet, Annotation, CancelReason, Code, Command, Epoch, Event, JsonValue,
    RunId, Seed, Seq, Timestamp,
};

/// What an executor compiled from a code's TypeScript source. It is stored with the
/// code, so an upgrade of the transpiler never changes the code a run replays
/// (`archi/03-execution.md`, "Determinism epoch"). The engine never interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledCode {
    pub javascript: String,
    pub source_map: String,
}

/// A code that cannot run: a syntax error, a forbidden import… The message is written to
/// be read by a model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct InvalidCode {
    pub message: String,
}

/// Everything a code instance needs to run `main(input)`.
#[derive(Debug, Clone)]
pub struct StartCode {
    pub run_id: RunId,
    pub compiled: CompiledCode,
    pub action_set: ActionSet,
    pub input: JsonValue,
    pub seed: Seed,
    pub epoch: Epoch,
    /// `recorded_at` of `run_started`: the code's `run.startedAt` and its first logical time.
    pub started_at: Timestamp,
}

/// A completion, with its payloads loaded: what reaches the code.
#[derive(Debug, Clone, PartialEq)]
pub enum Delivery {
    CallCompleted { seq: Seq, result: JsonValue },
    CallFailed { seq: Seq, error: ActionFailure },
    TimerFired { seq: Seq },
    SignalReceived { name: String, payload: JsonValue },
    CancelRequested { reason: CancelReason },
}

/// What a code emits in response to a trigger, in emission order.
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
    /// The code could not be loaded although it passed [`CodeExecutor::compile`]: an
    /// error thrown by its top-level code, a default export that is not a function. It ends
    /// the run with `run_aborted { invalid_code }`.
    #[error("the code could not be loaded: {0}")]
    InvalidCode(String),
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

/// Runs workflows in a sandbox (`archi/03-execution.md`, "Inside the isolate").
///
/// The executor only executes: it knows nothing about history, replay, storage or dispatch.
/// An instance is started with `main(input)`, receives deliveries one at a time, and after
/// each one returns what the code emitted once it is idle. Commands carry the `seq` the
/// code assigned, in emission order. The code's terminal command (`complete_run` or
/// `fail_run`) is emitted following the rules of "How a code ends".
pub trait CodeExecutor: Send + Sync {
    type Instance: CodeInstance;

    /// The determinism epochs this executor can run, oldest first. New runs start under the
    /// last one; a run of another epoch is never offered to this executor.
    fn epochs(&self) -> &[Epoch];

    /// Checks a code against the action set it runs with, and compiles it. Everything that
    /// can be refused without running the code is refused here: syntax errors, imports of
    /// modules or names that do not exist, a missing default export. Imports are checked
    /// against the last epoch, the one new runs start under; the compiled form does not depend
    /// on the epoch nor on the action set.
    fn compile(&self, code: &Code, action_set: &ActionSet) -> Result<CompiledCode, InvalidCode>;

    /// The names a code imports from `rewind:actions`: its MCP servers, APIs and groups of
    /// harness actions by their namespace, and its harness actions without a group by name. A start
    /// checks that each server or API it imports whose `auth` takes a secret has one attached
    /// (`archi/04-tools.md`, "Secrets"). `None` when the code may reach every one: it imports
    /// the whole module (`import * as actions`), or the executor cannot tell, which is the
    /// default. Called on a code [`compile`](Self::compile) accepted.
    fn action_imports(&self, code: &Code) -> Option<BTreeSet<String>> {
        let _ = code;
        None
    }

    /// Starts an instance under `epoch` and runs `main(input)` until the code is idle.
    fn start(
        &self,
        start: StartCode,
    ) -> impl Future<Output = Result<(Self::Instance, Vec<Emitted>), ExecutorError>> + Send;
}

/// A running code.
pub trait CodeInstance: Send {
    /// Delivers one completion, recorded at `now` (the run's new logical time), and runs the
    /// code until it is idle.
    fn deliver(
        &mut self,
        delivery: Delivery,
        now: Timestamp,
    ) -> impl Future<Output = Result<Vec<Emitted>, ExecutorError>> + Send;

    /// The memory the instance holds (its V8 heap), in bytes, as last measured. The engine's
    /// instance cache is bounded by the sum over the instances it keeps
    /// (`archi/03-execution.md`, "Sticky execution"). An executor that does not measure it
    /// reports 0, and the cache is then bounded by its number of instances only.
    fn memory_usage(&self) -> u64 {
        0
    }

    /// The CPU time of the instance's last call into the code: loading the code and
    /// `main(input)` once [`CodeExecutor::start`] returned it, then its last delivery, whether
    /// the call succeeded or not. It is the CPU time of the thread the code runs on, the clock
    /// of the CPU budget (`archi/03-execution.md`, "Resource limits"). The engine counts the
    /// calls that execute toward the namespace's `cpu_per_month`, and leaves out those of a
    /// replay (`archi/06-storage-and-scale.md`, "Quotas"). An executor that does not measure it
    /// reports zero.
    fn cpu_time(&self) -> Duration {
        Duration::ZERO
    }
}

/// An executor can be shared: an engine may borrow it, as test runs does to run scenarios with
/// the executor of its host.
impl<X: CodeExecutor> CodeExecutor for &X {
    type Instance = X::Instance;

    fn epochs(&self) -> &[Epoch] {
        (**self).epochs()
    }

    fn compile(&self, code: &Code, action_set: &ActionSet) -> Result<CompiledCode, InvalidCode> {
        (**self).compile(code, action_set)
    }

    fn action_imports(&self, code: &Code) -> Option<BTreeSet<String>> {
        (**self).action_imports(code)
    }

    fn start(
        &self,
        start: StartCode,
    ) -> impl Future<Output = Result<(Self::Instance, Vec<Emitted>), ExecutorError>> + Send {
        (**self).start(start)
    }
}
