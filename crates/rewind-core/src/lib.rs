//! Domain of Rewind: the run history and the pure logic built on it.
//!
//! - [`event`]: the four families of history events and [`HistoryEvent`] (`archi/02-history.md`).
//! - [`Payload`]: a JSON value identified by the digest of its canonical form (RFC 8785).
//! - [`RunState`]: the state of a run, a pure function of its history.
//! - [`ReplayCursor`]: the determinism check (`archi/03-execution.md`, "Replay").
//! - [`Code`] and [`ActionSet`]: the immutable, content-addressed inputs of a run. The action set
//!   is pinned: its servers and APIs with how each authenticates ([`ActionAuth`]), and for each
//!   action its route, schemas and policy.
//! - [`SecretRef`] and [`Attachments`]: the secrets a run or a workflow version attaches, by
//!   name, never by value (`archi/04-tools.md`, "Secrets").
//! - [`RunExport`]: a run's history with its code and action set, replayable on its own.
//! - [`Cassette`]: the mocks recorded from a run, derived from its history.
//! - [`Shape`]: the type of an action's results, as observed in histories.
//! - [`RunId`] and [`PartitionId`]: a run id names the partition its run lives in.
//! - [`Workflow`]: a code with a name and a typed interface, that workflows start and signal;
//!   [`RunEnd`], how a run's end reaches the runs that watch it; its [`Suite`] of tests.
//! - [`Test`]: the world a run meets and what the outside must see at the end; [`RunPath`],
//!   the path a run took.
//! - [`RunHandle`]: a run's readable name, `<namespace>/<origin>/<suffix>`, whose suffix is the
//!   start of its UUID; [`code_doc`], a code's title and description from its top comment.
//!
//! This crate is the domain and nothing else: no I/O, no async, no V8, no serde, no telemetry.
//! It knows nothing about how events are stored or transported, nor where time, ids and seeds
//! come from (`archi/07-code-architecture.md`, principle 1).

#![forbid(unsafe_code)]

mod action_set;
mod artifacts;
mod canonical;
mod cassette;
mod digest;
mod doc;
pub mod event;
mod handle;
mod history;
mod ids;
mod payload;
mod replay;
mod run_export;
mod secret;
mod shape;
mod state;
mod test;
mod time;
mod workflow;

pub use action_set::{
    Action, ActionPolicy, ActionRoute, ActionSet, ActionSetError, Api, ApiOperation, ApiParameter,
    Builtin, HttpMethod, McpServer, McpTransport, ParameterLocation, RESERVED_WORDS,
};
pub use artifacts::Code;
pub use canonical::{canonical_json, canonicalize};
pub use cassette::{Cassette, CassetteError, Matching, RecordedCall, RecordedSignal};
pub use digest::{Digest, InvalidDigest};
pub use doc::{CodeDoc, MAX_DESCRIPTION_CHARS, MAX_TITLE_CHARS, code_doc};
pub use event::{
    AbortKind, AbortReason, ActionFailure, ActionFailureKind, ActionOutcome, Annotation, Callsite,
    CancelReason, Command, Completion, Event, HistoryEvent, Lifecycle, LogLevel, RunMode,
    RunStarted, ScriptError,
};
pub use handle::{
    InvalidRunHandle, MAX_ORIGIN_LEN, MAX_SUFFIX_LEN, RunHandle, SCRIPT_ORIGIN, SUFFIX_LEN,
    is_origin, origin_slug, run_suffix, suffix_len_apart, suffix_neighbours, suffix_ranges,
};
pub use history::{HistoryError, validate_history};
pub use ids::{Epoch, PartitionId, RunId, Seed, Seq};
pub use payload::Payload;
pub use replay::{Next, NonDeterminism, ReplayCursor, Replayed};
pub use run_export::{RunExport, RunExportError};
pub use secret::{
    ActionAuth, Attachments, InvalidSecretName, InvalidSecretRef, MAX_SECRET_VALUE_BYTES,
    RESERVED_HEADERS, SECRET_PLACEHOLDER, SecretEncoding, SecretName, SecretPlacement, SecretRef,
    SecretScope,
};
pub use shape::{Field, Shape};
pub use state::{AwaitedSignal, RunOutcome, RunState, RunStatus};
pub use test::{
    ActionRule, CallOrder, Expectations, ExpectedCall, ExpectedError, InvalidTest, Observed,
    ObservedCall, PathStep, RecordError, RunPath, SignalWhen, StepKind, Test, TestOrigin,
    TestSignal, World, contains,
};
pub use time::{DurationError, Timestamp, parse_duration};
pub use workflow::{
    ERASED_TEST_REASON, InvalidWorkflow, PinnedWorkflow, RESERVED_SIGNAL_PREFIX, RetiredTest,
    RunEnd, Suite, SuiteTest, UpdatedTest, WORKFLOWS_GROUP, Workflow, WorkflowOp, WorkflowSchemas,
    end_signal_name, ended_run, is_reserved_signal, starter_ended,
};

/// The JSON value type carried by payloads.
pub use serde_json::Value as JsonValue;
