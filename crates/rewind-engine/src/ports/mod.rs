//! What the engine needs from the outside world (`archi/07-code-architecture.md`, "Ports").
//!
//! Every source of non-determinism and every I/O goes through one of these traits: storage,
//! access, identity, payload values, the code sandbox, calls, type-checking,
//! projections, telemetry, secrets, webhooks, time, ids and seeds. Ports are shaped by the
//! domain's invariants, not by tables. They return `impl Future + Send`, and the engine is
//! generic over them (static dispatch): composition roots pick the concrete adapters.
//!
//! | Port | Adapters |
//! | --- | --- |
//! | [`CatalogStore`] | memory, `rewind-store-turso`, `rewind-store-postgres` |
//! | [`PartitionStore`] / [`PartitionLog`] | memory, `rewind-store-slatedb` (S3 or any S3-compatible service, local files, memory) |
//! | [`RunArchive`] | memory, `rewind-blob` (files, S3, and a cache in front of either) |
//! | [`Cluster`] / [`cluster::Membership`] | memory, `rewind-store-turso` (one node), `rewind-store-postgres` |
//! | [`NodeLink`] | in process ([`LocalLink`](crate::adapters::LocalLink)), HTTP/2 (`rewind-server`) |
//! | [`AccessStore`] | memory, `rewind-store-turso`, `rewind-store-postgres` |
//! | [`IdentityProvider`] | memory, `rewind-workos` (WorkOS) |
//! | [`BlobStore`] | memory, `rewind-blob` (files, S3) |
//! | [`CodeExecutor`] / [`CodeInstance`] | `rewind-runtime` (V8) |
//! | [`ActionDispatcher`] | static responses (memory), handlers, `rewind-mcp`, `rewind-openapi` |
//! | [`TypeChecker`] | static (memory), `rewind-check` (tsgo) |
//! | [`SpanExporter`] | memory, `rewind-otel` |
//! | [`SecretStore`] | memory, `rewind-store-turso`, `rewind-store-postgres` |
//! | [`WebhookSender`] | memory, HTTP (`rewind-server`) |
//! | [`legacy::LegacyRuns`], [`legacy::LegacyBlobs`] | memory; `rewind-store-turso`, `rewind-store-postgres`; `rewind-blob` (files, S3) |
//! | [`Clock`], [`Entropy`] | system, virtual or seeded |
//!
//! The ports with state ([`CatalogStore`], [`PartitionStore`], [`RunArchive`], [`Cluster`],
//! [`AccessStore`], [`BlobStore`], [`SecretStore`]), [`SpanExporter`] and [`WebhookSender`]
//! have a conformance suite in the module `conformance` (feature `conformance`) that every
//! adapter passes.
//!
//! What a run's history, inbox and call tasks are made of lives in `history`, what a search and
//! a summary are in `read_model` (`archi/05-testing-and-observability.md`, "Read model"): both
//! are re-exported here. The modules [`catalog`], [`partition`], [`archive`], [`cluster`] and
//! [`link`] are public: the names they share with other modules ([`partition::CreateRun`],
//! [`partition::Activation`], [`cluster::Membership`], [`link::HarnessTask`]) are read through
//! them; every other name is also re-exported here. The ports of `rewind server migrate-runs`
//! are read through [`legacy`] alone, and have no suite: the migration's tests load a database
//! the old tables filled. A span exporter and a
//! webhook sender have no read side: their suites read the backend through a collector or a
//! receiver the adapter's test provides. An identity provider's contract is not observable
//! through the port alone: its adapters test it.

pub mod access;
pub mod archive;
mod blob;
pub mod catalog;
mod checker;
mod clock;
pub mod cluster;
pub mod connections;
mod dispatcher;
mod executor;
pub mod identity;

pub mod legacy;
pub mod link;
mod namespaces;
pub mod partition;
mod read_model;
mod records;
mod secrets;
mod telemetry;
mod tenancy;
mod webhooks;

pub use access::{
    AccessStore, ApiKey, Grant, GrantClaim, GrantDeletion, GrantWrite, InvalidPrincipal, Principal,
    Role, RoleDeletion, Token, TokenKey,
};
pub use archive::{ArchiveError, RunArchive, SealedAttempt};
pub use blob::{BlobError, BlobOwner, BlobStore};
pub use catalog::{
    CatalogError, CatalogStore, NamespaceFilter, NamespacePage, TEST_DELETION_TIMEOUT,
};
pub use checker::{
    ActionDeclarations, CheckerError, Diagnostic, DiagnosticSource, Severity, TypeChecker,
    WorkflowDeclarations,
};
pub use clock::{Clock, Entropy, VirtualTime};
pub use cluster::{
    Cluster, ClusterError, ControlPlaneChange, Departed, InvalidNodeId, Moves, NodeId, NodeInfo,
    PartitionGeneration, PartitionMap, PartitionOwner,
};
pub use connections::{
    Access, ClientAuthMethod, ClientKind, CodeRedemption, Connection, ConnectionToken,
    ConnectionTokenKind, NewConnection, NewToken, OAuthClient, OAuthCode, OpenedOrganization,
    Rotation,
};
pub use dispatcher::{ActionDispatcher, Call, CallAuth, CallClaims, Dispatched, RETRY_AFTER_DATA};
pub use executor::{
    CodeExecutor, CodeInstance, CompiledCode, Emitted, ExecutorError, Handoff, InvalidCode,
    StartCode,
};
pub use identity::{
    AccessClaims, Authentication, Client, IdentityError, IdentityProvider, Invitation,
    InvitationId, InvitationState, MagicCode, Membership, MembershipId, MembershipStatus,
    Organization, OrganizationId, PendingToken, Provider, Session, SessionId, Tokens, User, UserId,
};
pub use link::{LinkError, NodeLink};
pub use namespaces::{
    Day, DayUsage, DeleteNamespace, DisplayName, DropNamespace, InvalidDisplayName,
    InvalidNamespace, Month, Namespace, NamespaceName, NamespaceWrite, QuotaAlert, QuotaAlertKey,
    QuotaExceeded, QuotaKind, QuotaScope, Quotas, Threshold, Usage, limits,
};
pub use partition::{
    ActionCounts, Admission, Attempt, Batch, Change, CountedNamespace, CounterDelta, Counters,
    DueTimer, FailureCounts, Found, Gauges, Hour, IndexedSummary, LATEST_RUNS, LatestRun,
    OutboxChange, OutboxEntry, OutboxId, Page, PartitionError, PartitionLog, PartitionStore,
    Projection, Recovery, RunCounts, RunHead, SCAN_BUDGET, Scope, ScriptCounts, SearchPosition,
    Sketch, TaskChange, TaskRecord, TenantState, TimerChange, TimerKind, Watcher, Window,
    home_partition, namespace_partition,
};
pub use read_model::{
    Ending, Latency, Predicate, QuarantineNote, Range, RunOrder, RunQuery, RunSnapshot, RunSummary,
    SearchCursor, TaskPhase, Term, TextMatch, Wait, WaitKind, WaitingFor,
};
pub use records::{
    CallTask, Created, Enqueued, ForkSource, InboxItem, ParkKind, ParkReason, PayloadRecord,
    QUARANTINE_AFTER_FAILURES, Quarantine, QuarantineKind, RunMetadata, RunWorkflow, ScriptTitle,
    SignalUrlTarget, SignalUrlToken, StartedBy, StoreError, StoredActionSet, StoredCode,
    StoredWorkflow, TEST_GRACE, TaskRef, TaskState, TaskToken, TestRecord, TraceContext, Trigger,
    WorkflowFilter, deferred_timeout_failure,
};
pub use secrets::{
    DailyReads, LastRead, MasterKeyId, NewSecret, PrivateKey, PublicKey, PutSecret,
    RETIRED_KEY_PUBLISHED, Secret, SecretError, SecretId, SecretMeta, SecretNamespace,
    SecretReader, SecretStore, SigningKey,
};
pub use telemetry::{
    AttributeValue, ExportError, LogRecord, Span, SpanExporter, SpanId, SpanKind, SpanStatus,
    TraceId,
};
pub use tenancy::{InvalidTenant, Retention, RunRef, Tenant, TenantId};
pub use webhooks::{
    SecretExpiredReason, Subscription, Webhook, WebhookError, WebhookKind, WebhookSender,
    WebhookType,
};
