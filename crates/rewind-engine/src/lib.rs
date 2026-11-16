//! Application layer of Rewind (`archi/07-code-architecture.md`).
//!
//! - [`ports`]: what the engine needs from the outside world, as traits. Adapters implement
//!   them; the engine never names Postgres, S3, V8, MCP or OpenTelemetry.
//! - [`Engine`]: the use cases (start, signal, cancel, activate with sticky execution, fire
//!   timers, expire runs, quarantine, replay, fork, export), the tool tasks' (dispatch,
//!   host-tool workers, approvals, parked tasks and credentials, backoff), and the driver
//!   (`run_until_idle`, in real or virtual time).
//! - [`replay`]: rebuilding a script from its history.
//! - [`projector`]: the feed's consumers: read model, run telemetry ([`telemetry`]) and
//!   lifecycle webhooks, projected from committed entries; the snapshot of a run and the
//!   service state (task states, quarantine) the read model keeps.
//! - [`failures`]: the signatures and fingerprints that group failed, cancelled and aborted runs.
//! - [`metrics`]: what the engine measures, through the `tracing` facade.
//! - [`retention`]: archives, deletion, payload cleanup, the erasure of deleted namespaces.
//! - [`quotas`]: the CPU time a worker counts per namespace and writes about every 10 s, and the
//!   sending of quota alerts.
//! - [`mock`]: mock mode: the dispatcher that answers from a test's rules, mock handlers,
//!   cassette and static responses.
//! - [`testing`]: tests run in virtual time against a script, several at once; the checks of a
//!   test against its toolset and its workflow's interface.
//! - [`trace`]: the LLM trace of a run.
//! - [`adapters`]: in-memory storage, access, read model, secrets, telemetry and webhooks; a
//!   fake identity provider; static tool responses and type-checking; system and virtual time;
//!   system and seeded entropy. With a virtual clock they make mock mode and the test harness.
//! - `conformance` (feature): the suites every adapter of `JournalStore`, `AccessStore`,
//!   `BlobStore`, `ReadModel`, `SecretStore`, `SpanExporter` and `WebhookSender` must pass.
//!
//! The engine is deterministic given its ports. Service telemetry (spans, logs and measurements)
//! goes through the `tracing` facade; what a script did is recorded in the journal, never
//! emitted from here.

#![forbid(unsafe_code)]

mod activation;
pub mod adapters;
mod cache;
#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
mod driver;
mod engine;
mod export;
pub mod failures;
mod fork;
pub mod metrics;
pub mod mock;
mod payloads;
pub mod ports;
pub mod projector;
pub mod quotas;
pub mod replay;
pub mod retention;
pub mod schema;
mod tasks;
pub mod telemetry;
pub mod testing;
pub mod text;
pub mod trace;
mod workflows;

pub use cache::CacheConfig;
pub use driver::{DriveOptions, DriveReport, ToolOrder};
pub use engine::{
    ActivationOutcome, ActivationReport, Adapters, Engine, EngineConfig, EngineError, Limits,
    ReplayVerdict, StartError, StartRun, idempotency_key,
};
pub use fork::{Compatibility, ForkError, ForkPoint, ForkRun};
pub use payloads::PayloadTiers;
pub use tasks::{Backoff, HostClaim, HostFailure, HostTask};
pub use text::NulCharacter;
pub use workflows::{RegisterError, RegisterWorkflow, SignalRefused, StartWorkflow};
