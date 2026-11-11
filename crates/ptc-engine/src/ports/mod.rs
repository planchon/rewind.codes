//! What the engine needs from the outside world (`archi/07-code-architecture.md`, "Ports").
//!
//! Every source of non-determinism and every I/O goes through one of these traits: storage,
//! payload values, the script sandbox, tool calls, type-checking, projections, telemetry,
//! secrets, webhooks, time, ids and seeds. Ports are shaped by the domain's invariants, not by
//! tables. They return `impl Future + Send`, and the engine is generic over them (static
//! dispatch): composition roots pick the concrete adapters.
//!
//! | Port | Adapters |
//! | --- | --- |
//! | [`JournalStore`] | memory, `ptc-store-turso`, `ptc-store-postgres` |
//! | [`BlobStore`] | memory, `ptc-blob` (files, S3) |
//! | [`ScriptExecutor`] / [`ScriptInstance`] | `ptc-runtime` (V8) |
//! | [`ToolDispatcher`] | static responses (memory), mocks, `ptc-mcp` |
//! | [`TypeChecker`] | static (memory), `ptc-check` (tsgo) |
//! | [`ReadModel`] | memory, `ptc-store-turso`, `ptc-store-postgres` |
//! | [`SpanExporter`] | memory, `ptc-otel` |
//! | [`SecretStore`] | memory, `ptc-store-turso`, `ptc-store-postgres` |
//! | [`WebhookSender`] | memory, HTTP (`ptc-server`) |
//! | [`Clock`], [`Entropy`] | system, virtual or seeded |
//!
//! The ports with state have a conformance suite in the module `conformance` (feature
//! `conformance`) that every adapter passes.

mod blob;
mod checker;
mod clock;
mod dispatcher;
mod executor;
mod journal;
mod read_model;
mod secrets;
mod telemetry;
mod tenancy;
mod webhooks;

pub use blob::{BlobError, BlobStore};
pub use checker::{
    CheckerError, Diagnostic, DiagnosticSource, Severity, ToolDeclarations, TypeChecker,
};
pub use clock::{Clock, Entropy, VirtualTime};
pub use dispatcher::{Dispatched, ToolCall, ToolDispatcher};
pub use executor::{
    CompiledScript, Delivery, Emitted, ExecutorError, InvalidScript, ScriptExecutor,
    ScriptInstance, StartScript,
};
pub use journal::{
    AcquireRunnable, Activation, Archive, ClaimedTask, CreateRun, Created, Enqueued, ExpiredRuns,
    Failures, FeedConsumer, FeedItem, InboxItem, JournalStore, Lease, NewRun, NewTimer,
    NewToolTask, ParkKind, ParkReason, ParkedScope, PayloadRecord, QUARANTINE_AFTER_FAILURES,
    Quarantine, QuarantineKind, RunCursor, RunFilter, RunMetadata, RunRecord, STICKY_GRACE,
    StoreError, StoredScript, StoredToolset, TaskFilter, TaskRef, TaskState, TaskToken, TaskUpdate,
    ToolTask, TraceContext, Trigger, WebhookTarget, WebhookToken, deferred_timeout_failure,
};
pub use read_model::{
    Latency, ProjectedRun, ReadModel, ReadModelError, RunSearch, RunSummary, ScriptFilter,
    ScriptSummary, ToolStats, ToolStatsFilter,
};
pub use secrets::{Credentials, Secret, SecretError, SecretOwner, SecretStore, ServerCredentials};
pub use telemetry::{
    AttributeValue, ExportError, LogRecord, Span, SpanExporter, SpanId, SpanKind, SpanStatus,
    TraceId,
};
pub use tenancy::{ApiKey, InvalidTenant, Retention, RunRef, Tenant, TenantId, TenantScope};
pub use webhooks::{
    Subscription, WebhookError, WebhookEvent, WebhookEventKind, WebhookEventType, WebhookSender,
};
