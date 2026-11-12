//! Application layer of ai-ptc (`archi/07-code-architecture.md`).
//!
//! - [`ports`]: what the engine needs from the outside world, as traits. Adapters implement
//!   them; the engine never names Postgres, S3, V8, MCP or OpenTelemetry.
//! - [`Engine`]: the use cases (start, signal, cancel, activate, fire timers, expire runs,
//!   quarantine, replay), the tool tasks' (dispatch, host-tool workers, approvals, parked tasks
//!   and credentials, backoff), and the driver (`run_until_idle`, in real or virtual time).
//! - [`replay`]: rebuilding a script from its history.
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
#[cfg(any(test, feature = "conformance"))]
pub mod conformance;
mod driver;
mod engine;
mod payloads;
pub mod ports;
pub mod replay;
mod tasks;

pub use driver::{DriveOptions, DriveReport, ToolOrder};
pub use engine::{
    ActivationOutcome, ActivationReport, Adapters, Engine, EngineConfig, EngineError, Limits,
    ReplayVerdict, StartError, StartRun, idempotency_key,
};
pub use tasks::{Backoff, HostClaim, HostFailure, HostTask};
