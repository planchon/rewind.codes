//! What the engine needs from the outside world (`archi/07-code-architecture.md`, "Ports").
//!
//! Every source of non-determinism and every I/O goes through one of these traits: storage,
//! payload values, the script sandbox, tool calls, time, ids and seeds. Ports are shaped by
//! the domain's invariants, not by tables. They return `impl Future + Send`, and the engine
//! is generic over them (static dispatch): composition roots pick the concrete adapters.

mod blob;
mod clock;
mod dispatcher;
mod executor;
mod journal;

pub use blob::{BlobError, BlobStore};
pub use clock::{Clock, Entropy, VirtualTime};
pub use dispatcher::{ToolCall, ToolDispatcher};
pub use executor::{
    CompiledScript, Delivery, Emitted, ExecutorError, InvalidScript, ScriptExecutor,
    ScriptInstance, StartScript,
};
pub use journal::{
    Activation, Created, Enqueued, ExpiredRuns, InboxItem, JournalStore, Lease, NewRun, NewTimer,
    NewToolTask, RunFilter, RunMetadata, RunRecord, StoreError, StoredScript, ToolTask,
    TraceContext, Trigger,
};
