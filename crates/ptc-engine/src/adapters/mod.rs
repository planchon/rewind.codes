//! Adapters that need no infrastructure: in-memory storage, static tool responses, system
//! and virtual time, system and seeded entropy. Wired together with a virtual clock, they make
//! the engine's mock mode and its test harness (`archi/07-code-architecture.md`, principle 6).

mod blobs;
mod dispatcher;
mod entropy;
mod journal;
mod time;

pub use blobs::MemoryBlobs;
pub use dispatcher::{StaticDispatcher, StaticResponsesError};
pub use entropy::{SeededEntropy, SystemEntropy};
pub use journal::MemoryJournal;
pub use time::{SystemClock, VirtualClock};
