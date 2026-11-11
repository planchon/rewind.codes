//! Application layer of ai-ptc (`archi/07-code-architecture.md`).
//!
//! - [`ports`]: what the engine needs from the outside world, as traits. Adapters implement
//!   them; the engine never names Postgres, S3, V8, MCP or OpenTelemetry.
//! - [`Engine`]: the use cases (start, signal, cancel, activate, fire timers, expire runs,
//!   dispatch tool calls, quarantine, replay) and the driver (`run_until_idle`, in real or
//!   virtual time).
//! - [`replay`]: rebuilding a script from its history.
//! - [`adapters`]: in-memory storage, static tool responses, system and virtual time, system
//!   and seeded entropy. With a virtual clock they make mock mode and the test harness.
//! - `conformance` (feature): the suite every `JournalStore` adapter must pass.
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

pub use driver::{DriveOptions, DriveReport, ToolOrder};
pub use engine::{
    ActivationOutcome, ActivationReport, Adapters, Engine, EngineConfig, EngineError, Limits,
    ReplayVerdict, StartError, StartRun, idempotency_key,
};
