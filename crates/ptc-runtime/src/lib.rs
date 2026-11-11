//! The V8 adapter of ai-ptc: the deterministic sandbox behind the engine's `ScriptExecutor`
//! port (`archi/01-product.md`, "What exists in the sandbox", and `archi/03-execution.md`).
//!
//! The runtime only executes: it takes a script and a start, receives deliveries, and
//! returns what the script emitted. It knows nothing about history, replay, storage or
//! dispatch. Inside the isolate:
//!
//! - time is logical (the `recorded_at` of the last delivered trigger), and randomness is
//!   seeded (sfc32 for `Math.random`, ChaCha20 for `crypto`);
//! - every interaction with the outside world is a command with a `seq`;
//! - the script's terminal command follows the rules of "How a script ends": it waits for
//!   the tool calls in flight, an unhandled rejection fails the run, and a script that nothing
//!   can wake fails with `DeadlockError`.
//!
//! Each instance runs on its own thread, under the JavaScript glue of its run's epoch.

#![forbid(unsafe_code)]

mod compile;
mod epochs;
mod executor;
mod isolate;
mod loader;
mod ops;
mod watchdog;

pub use executor::{SetupError, V8Executor, V8Instance};
pub use isolate::SandboxLimits;
