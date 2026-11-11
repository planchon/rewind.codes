//! Domain of ai-ptc: the run journal and the pure logic built on it.
//!
//! - [`event`]: the four families of journal events and [`HistoryEntry`] (`archi/02-journal.md`).
//! - [`Payload`]: a JSON value identified by the digest of its canonical form (RFC 8785).
//! - [`RunState`]: the state of a run, a pure function of its history.
//! - [`ReplayCursor`]: the determinism check (`archi/03-execution.md`, "Replay").
//! - [`Script`] and [`Toolset`]: the immutable, content-addressed inputs of a run.
//!
//! This crate is the domain and nothing else: no I/O, no async, no V8, no serde, no telemetry.
//! It knows nothing about how events are stored or transported, nor where time, ids and seeds
//! come from (`archi/07-code-architecture.md`, principle 1).

#![forbid(unsafe_code)]

mod artifacts;
mod canonical;
mod digest;
pub mod event;
mod history;
mod ids;
mod payload;
mod replay;
mod state;
mod time;

pub use artifacts::{Script, ToolSpec, Toolset};
pub use canonical::{canonical_json, canonicalize};
pub use digest::{Digest, InvalidDigest};
pub use event::{
    AbortKind, AbortReason, Annotation, Callsite, CancelReason, Command, Completion, Event,
    HistoryEntry, Lifecycle, LogLevel, RunMode, RunStarted, ScriptError, ToolFailure,
    ToolFailureKind,
};
pub use history::{HistoryError, validate_history};
pub use ids::{Epoch, RunId, Seed, Seq};
pub use payload::Payload;
pub use replay::{Next, NonDeterminism, ReplayCursor, Replayed};
pub use state::{AwaitedSignal, RunOutcome, RunState, RunStatus};
pub use time::{DurationError, Timestamp, parse_duration};

/// The JSON value type carried by payloads.
pub use serde_json::Value as JsonValue;
