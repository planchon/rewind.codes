//! What the engine needs from the outside world (`archi/07-code-architecture.md`, "Ports").
//!
//! Every source of non-determinism and every I/O goes through one of these traits: storage,
//! access, identity, payload values, the script sandbox, tool calls, type-checking,
//! projections, telemetry, secrets, webhooks, time, ids and seeds. Ports are shaped by the
//! domain's invariants, not by tables. They return `impl Future + Send`, and the engine is
//! generic over them (static dispatch): composition roots pick the concrete adapters.
//!
//! | Port | Adapters |
//! | --- | --- |
//! | [`JournalStore`] | memory, `rewind-store-turso`, `rewind-store-postgres` |
//! | [`AccessStore`] | memory, `rewind-store-turso`, `rewind-store-postgres` |
//! | [`IdentityProvider`] | memory, `rewind-workos` (WorkOS) |
//! | [`BlobStore`] | memory, `rewind-blob` (files, S3) |
//! | [`ScriptExecutor`] / [`ScriptInstance`] | `rewind-runtime` (V8) |
//! | [`ToolDispatcher`] | static responses (memory), mocks, `rewind-mcp` |
//! | [`TypeChecker`] | static (memory), `rewind-check` (tsgo) |
//! | [`ReadModel`] | memory, `rewind-store-turso`, `rewind-store-postgres` |
//! | [`SpanExporter`] | memory, `rewind-otel` |
//! | [`SecretStore`] | memory, `rewind-store-turso`, `rewind-store-postgres` |
//! | [`WebhookSender`] | memory, HTTP (`rewind-server`) |
//! | [`Clock`], [`Entropy`] | system, virtual or seeded |
//!
//! The ports with state ([`JournalStore`], [`AccessStore`], [`BlobStore`], [`ReadModel`],
//! [`SecretStore`]), [`SpanExporter`] and [`WebhookSender`] have a conformance suite in the
//! module `conformance` (feature `conformance`) that every adapter passes. A span exporter and a
//! webhook sender have no read side: their suites read the backend through a collector or a
//! receiver the adapter's test provides. An identity provider's contract is not observable
//! through the port alone: its adapters test it.

pub mod access;
mod blob;
mod checker;
mod clock;
mod dispatcher;
mod executor;
pub mod identity;
mod journal;
mod namespaces;
mod read_model;
mod secrets;
mod telemetry;
mod tenancy;
mod webhooks;

pub use access::{
    AccessStore, ApiKey, Grant, GrantClaim, GrantDeletion, GrantWrite, InvalidPrincipal, Principal,
    Role, RoleDeletion, Token, TokenKey,
};
pub use blob::{BlobError, BlobStore};
pub use checker::{
    CheckerError, Diagnostic, DiagnosticSource, Severity, ToolDeclarations, TypeChecker,
    WorkflowDeclarations,
};
pub use clock::{Clock, Entropy, VirtualTime};
pub use dispatcher::{Dispatched, ToolCall, ToolDispatcher};
pub use executor::{
    CompiledScript, Delivery, Emitted, ExecutorError, InvalidScript, ScriptExecutor,
    ScriptInstance, StartScript,
};
pub use identity::{
    AccessClaims, Authentication, Client, IdentityError, IdentityProvider, Invitation,
    InvitationId, InvitationState, MagicCode, Membership, MembershipId, MembershipStatus,
    Organization, OrganizationId, PendingToken, Provider, Session, SessionId, Tokens, User, UserId,
};
pub use journal::{
    AcquireRunnable, Activation, Archive, ClaimedTask, CreateRun, Created, Enqueued, ExpiredRuns,
    Failures, FeedConsumer, FeedItem, ForkParent, InboxItem, JournalStore, Lease, NewRun, NewTimer,
    NewToolTask, PAYLOAD_DELETION_TIMEOUT, ParkKind, ParkReason, ParkedScope, PayloadRecord,
    QUARANTINE_AFTER_FAILURES, Quarantine, QuarantineKind, RunCursor, RunFilter, RunMetadata,
    RunRecord, RunWorkflow, STICKY_GRACE, StartedBy, StoreError, StoredScript, StoredToolset,
    StoredWorkflow, TaskFilter, TaskRef, TaskState, TaskToken, TaskUpdate, ToolTask, TraceContext,
    Trigger, Watched, WebhookTarget, WebhookToken, WorkflowFilter, deferred_timeout_failure,
};
pub use namespaces::{
    Day, DayUsage, DeleteNamespace, DropNamespace, InvalidNamespace, Month, Namespace,
    NamespaceCursor, NamespaceFilter, NamespaceName, NamespaceOrder, NamespacePage, NamespaceUsage,
    NamespaceWrite, QuotaAlert, QuotaAlertKey, QuotaExceeded, QuotaKind, QuotaScope, Quotas,
    Threshold, Usage, UsageDelta, UsageOf, limits,
};
pub use read_model::{
    Ending, FacetValue, Facets, FailureGroup, FailureGroups, FailureWindow, LabelFacet, Latency,
    Predicate, ProjectedRun, QuarantineNote, Range, ReadModel, ReadModelError, RunOrder, RunQuery,
    RunSnapshot, RunStats, RunSummary, ScriptFilter, ScriptSummary, SearchCursor, StatsWindow,
    TaskPhase, TaskProgress, Term, TextMatch, ToolStats, ToolStatsFilter, Wait, WaitKind,
    WaitingFor,
};
pub use secrets::{Credentials, Secret, SecretError, SecretOwner, SecretStore, ServerCredentials};
pub use telemetry::{
    AttributeValue, ExportError, LogRecord, Span, SpanExporter, SpanId, SpanKind, SpanStatus,
    TraceId,
};
pub use tenancy::{InvalidTenant, Retention, RunRef, Tenant, TenantId, TenantScope};
pub use webhooks::{
    Subscription, WebhookError, WebhookEvent, WebhookEventKind, WebhookEventType, WebhookSender,
};
