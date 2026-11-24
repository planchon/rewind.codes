//! The V8 adapter of Rewind: the deterministic sandbox behind the engine's `CodeExecutor`
//! port (`archi/01-product.md`, "What exists in the sandbox", and `archi/03-execution.md`).
//!
//! The runtime only executes: it takes a code and a start, receives deliveries, and
//! returns what the code emitted. It knows nothing about history, replay, storage or
//! dispatch. Inside the isolate:
//!
//! - time is logical (the `recorded_at` of the last delivered trigger), and randomness is
//!   seeded (sfc32 for `Math.random`, ChaCha20 for `crypto`);
//! - the web-standard globals (`URL`, `TextEncoder`, `structuredClone`…) come from `deno_web`;
//! - every interaction with the outside world is a command with a `seq`: a call, whose
//!   args are checked against the action's pinned schema first, `signalUrl(name)`, `fetch` when
//!   the action set has the `http` action, a timer;
//! - the code's terminal command follows the rules of "How a code ends": it waits for
//!   the calls in flight, an unhandled rejection fails the run, and a code that nothing
//!   can wake fails with `DeadlockError`.
//!
//! Each instance runs on its own thread, under the JavaScript glue of its run's epoch, started
//! from that epoch's V8 snapshot, with a CPU budget per call measured on the thread's CPU clock.
//!
//! [`sandbox_lib`] holds the declarations the type-checker gives workflows: exactly what the
//! sandbox has. [`builtins`] holds what the glue sends to the built-in actions. [`TsMocks`] runs
//! TypeScript handlers in an isolate of their own, as the dispatcher of test runs.

// Denied, not forbidden: `memory` alone allows it, for what V8 offers only through raw
// pointers (archi/07-code-architecture.md, "Dependency rules").
#![deny(unsafe_code)]

pub mod builtins;
mod compile;
mod epochs;
mod executor;
mod handlers;
mod instance;
mod isolate;
mod loader;
mod memory;
mod ops;
pub mod sandbox_lib;
mod snapshot;
mod vendored;
mod watchdog;

pub use executor::{SetupError, V8Executor, V8Instance};
pub use handlers::{HandlerError, TsHandlerLoader, TsHandlers};
pub use isolate::SandboxLimits;
