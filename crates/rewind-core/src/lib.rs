//! Domain of Rewind: the run journal and the pure logic built on it.
//!
//! - [`event`]: the four families of journal events and [`HistoryEntry`] (`archi/02-journal.md`).
//! - [`Payload`]: a JSON value identified by the digest of its canonical form (RFC 8785).
//! - [`RunState`]: the state of a run, a pure function of its history.
//! - [`ReplayCursor`]: the determinism check (`archi/03-execution.md`, "Replay").
//! - [`Script`] and [`Toolset`]: the immutable, content-addressed inputs of a run. The toolset
//!   is pinned: its servers and APIs with how each authenticates ([`ToolAuth`]), and for each
//!   tool its route, schemas and policy.
//! - [`SecretRef`] and [`Attachments`]: the secrets a run or a workflow version attaches, by
//!   name, never by value (`archi/04-tools.md`, "Secrets").
//! - [`JournalDocument`]: a run's journal with its script and toolset, replayable on its own.
//! - [`Cassette`]: the mocks recorded from a run, derived from its history.
//! - [`Shape`]: the type of a tool's results, as observed in journals.
//! - [`Workflow`]: a script with a name and a typed interface, that scripts start and signal;
//!   [`RunEnd`], how a run's end reaches the runs that watch it; its [`Suite`] of tests.
//! - [`Test`]: the world a run meets and what the outside must see at the end; [`RunPath`],
//!   the path a run took.
//!
//! This crate is the domain and nothing else: no I/O, no async, no V8, no serde, no telemetry.
//! It knows nothing about how events are stored or transported, nor where time, ids and seeds
//! come from (`archi/07-code-architecture.md`, principle 1).

#![forbid(unsafe_code)]

mod artifacts;
mod canonical;
mod cassette;
mod digest;
pub mod event;
mod history;
mod ids;
mod journal;
mod payload;
mod replay;
mod secret;
mod shape;
mod state;
mod test;
mod time;
mod toolset;
mod workflow;

pub use artifacts::Script;
pub use canonical::{canonical_json, canonicalize};
pub use cassette::{Cassette, CassetteError, Matching, RecordedCall, RecordedSignal};
pub use digest::{Digest, InvalidDigest};
pub use event::{
    AbortKind, AbortReason, Annotation, Callsite, CancelReason, Command, Completion, Event,
    HistoryEntry, Lifecycle, LogLevel, RunMode, RunStarted, ScriptError, ToolFailure,
    ToolFailureKind, ToolOutcome,
};
pub use history::{HistoryError, validate_history};
pub use ids::{Epoch, RunId, Seed, Seq};
pub use journal::{JournalDocument, JournalError};
pub use payload::Payload;
pub use replay::{Next, NonDeterminism, ReplayCursor, Replayed};
pub use secret::{
    Attachments, InvalidSecretName, InvalidSecretRef, MAX_SECRET_VALUE_BYTES, RESERVED_HEADERS,
    SECRET_PLACEHOLDER, SecretEncoding, SecretName, SecretPlacement, SecretRef, SecretScope,
    ToolAuth,
};
pub use shape::{Field, Shape};
pub use state::{AwaitedSignal, RunOutcome, RunState, RunStatus};
pub use test::{
    CallOrder, Expectations, ExpectedCall, ExpectedError, InvalidTest, Observed, ObservedCall,
    PathStep, RecordError, RunPath, SignalWhen, StepKind, Test, TestOrigin, TestSignal, ToolRule,
    World, contains,
};
pub use time::{DurationError, Timestamp, parse_duration};
pub use toolset::{
    Api, ApiOperation, ApiParameter, Builtin, HttpMethod, Interruption, McpServer, McpTransport,
    ParameterLocation, RESERVED_WORDS, Tool, ToolPolicy, ToolRoute, Toolset, ToolsetError,
};
pub use workflow::{
    ERASED_TEST_REASON, InvalidWorkflow, PinnedWorkflow, RESERVED_SIGNAL_PREFIX, RetiredTest,
    RunEnd, Suite, SuiteTest, UpdatedTest, WORKFLOWS_NAMESPACE, Workflow, WorkflowOp,
    WorkflowSchemas, end_signal_name, ended_run, is_reserved_signal, starter_ended,
};

/// The JSON value type carried by payloads.
pub use serde_json::Value as JsonValue;
