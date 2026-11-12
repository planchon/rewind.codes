//! Application layer of ai-ptc (`archi/07-code-architecture.md`).
//!
//! - [`ports`]: what the engine needs from the outside world, as traits. Adapters implement
//!   them; the engine never names Postgres, S3, V8, MCP or OpenTelemetry.
//! - [`Engine`]: the use cases (start, signal, cancel, activate with sticky execution, fire
//!   timers, expire runs, dispatch tool calls, quarantine, replay, fork, export) and the driver
//!   (`run_until_idle`, in real or virtual time).
//! - [`replay`]: rebuilding a script from its history.
//! - [`projector`]: the feed's consumers: read model, run telemetry ([`telemetry`]) and
//!   lifecycle webhooks, projected from committed entries.
//! - [`retention`]: archives, deletion, payload cleanup.
//! - [`mock`]: mock mode: cassettes, mocks, and scenarios run in virtual time.
//! - [`trace`]: the LLM trace of a run.
//! - [`adapters`]: in-memory storage, read model, secrets, telemetry and webhooks; static tool
//!   responses and type-checking; system and virtual time; system and seeded entropy. With a
//!   virtual clock they make mock mode and the test harness.
//! - `conformance` (feature): the suites every adapter of `JournalStore`, `BlobStore`,
//!   `ReadModel` and `SecretStore` must pass.
//!
//! The engine is deterministic given its ports. Service telemetry goes through the `tracing`
//! facade; what a script did is recorded in the journal, never emitted from here.

#![forbid(unsafe_code)]

mod activation;
pub mod adapters;
mod cache;
#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
mod driver;
mod engine;
mod export;
mod fork;
pub mod mock;
mod payloads;
pub mod ports;
pub mod projector;
pub mod replay;
pub mod retention;
pub mod telemetry;
pub mod trace;

pub use cache::CacheConfig;
pub use driver::{DriveOptions, DriveReport, ToolOrder};
pub use engine::{
    ActivationOutcome, ActivationReport, Adapters, Engine, EngineConfig, EngineError, Limits,
    ReplayVerdict, StartError, StartRun, idempotency_key,
};
pub use fork::{Compatibility, ForkError, ForkPoint, ForkRun};
pub use payloads::PayloadTiers;
