//! The V8 adapter of ai-ptc: the deterministic sandbox behind the engine's `ScriptExecutor`
//! port (`archi/01-product.md`, "What exists in the sandbox", and `archi/03-execution.md`).
//!
//! The runtime only executes: it takes a script and a start, receives deliveries, and
//! returns what the script emitted. It knows nothing about history, replay, storage or
//! dispatch. Inside the isolate:
//!
//! - time is logical (the `recorded_at` of the last delivered trigger), and randomness is
//!   seeded (sfc32 for `Math.random`, ChaCha20 for `crypto`);
//! - the web-standard globals (`URL`, `TextEncoder`, `structuredClone`…) come from `deno_web`;
//! - every interaction with the outside world is a command with a `seq`: a tool call, whose
//!   args are checked against the tool's pinned schema first, `webhook(name)`, `fetch` when
//!   the toolset has the `http` tool, a timer;
//! - the script's terminal command follows the rules of "How a script ends": it waits for
//!   the tool calls in flight, an unhandled rejection fails the run, and a script that nothing
//!   can wake fails with `DeadlockError`.
//!
//! Each instance runs on its own thread, under the JavaScript glue of its run's epoch, started
//! from that epoch's V8 snapshot, with a CPU budget per call measured on the thread's CPU clock.
//!
//! [`sandbox_lib`] holds the declarations the type-checker gives scripts: exactly what the
//! sandbox has. [`builtins`] holds what the glue sends to the built-in tools. [`TsMocks`] runs
//! TypeScript mock handlers in an isolate of their own, as the dispatcher of mock mode.

// Denied, not forbidden: `memory` alone allows it, for what V8 offers only through raw
// pointers (archi/07-code-architecture.md, "Dependency rules").
#![deny(unsafe_code)]

pub mod builtins;
mod compile;
mod epochs;
mod executor;
mod instance;
mod isolate;
mod loader;
mod memory;
mod mocks;
mod ops;
pub mod sandbox_lib;
mod snapshot;
mod vendored;
mod watchdog;

pub use executor::{SetupError, V8Executor, V8Instance};
pub use isolate::SandboxLimits;
pub use mocks::{MockError, TsMocks};
