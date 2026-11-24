//! The `NodeLink` port: what the nodes of a deployment send each other
//! (`archi/07-code-architecture.md`, "`Cluster` and `NodeLink`", "Inside a node";
//! `archi/04-tools.md`, "Call lifecycle").
//!
//! ```text
//!  edge ── Partition(PartitionRequest) ──────────► owner of the partition    (start, signal, …, reads)
//!  owner ─ Organization(OrgRequest) ─────────────► organization actor        (admission, counts, slots, host tasks)
//!  edge ── Organization(Attach / Detach) ────────► organization actor        (an SDK stream opened or closed)
//!  org ─── Owner(OwnerNotice) ───────────────────► owner of a run            (slot granted, stream lost, counts wanted)
//!  org ─── Edge(EdgeNotice::HandOut) ────────────► edge holding the stream   (a host task, with its token)
//!  edge ── subscribe(SubscriptionKey, from) ─────► owner                     (a stream of Changed)
//!  owner ─ activate(ActivationRequest) ──────────► node of the run's epoch   (a remote executor)
//! ```
//!
//! Every message is a domain type: the node link's wire form (HTTP/2 and JSON) belongs to
//! `rewind-server`. An owner that no longer holds a partition answers [`NodeReply::NotOwner`];
//! the router reads the partition map again and retries.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::num::NonZeroU32;
use std::time::Duration;

use futures_util::Stream;
use rewind_core::{
    Digest, HistoryEntry, JsonValue, PartitionId, RunHandle, RunId, RunStarted, Seq, Shape,
    Timestamp,
};

use crate::ports::archive::SealedAttempt;
use crate::ports::cluster::{NodeId, PartitionEpoch};
use crate::ports::journal::{
    Created, Enqueued, InboxItem, PayloadRecord, QuarantineKind, RunMetadata, StoredActionSet,
    StoredScript, TaskRef, TaskToken, TraceContext,
};
use crate::ports::namespaces::{Month, NamespaceName, QuotaAlert, QuotaExceeded, Usage};
use crate::ports::partition::{
    Activation, Counters, Found, Gauges, IndexedSummary, Page, Projection, RunHead, Scope,
    TaskRecord, Window,
};
use crate::ports::read_model::{RunOrder, RunQuery, RunSummary};
use crate::ports::tenancy::{RunRef, TenantId};

/// The link could not carry a message: the node is unreachable, or it dropped the request.
/// The caller decides: the edge answers 503, an owner leaves the effect to be rebuilt from the
/// records.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("node {0} cannot be reached")]
    Unreachable(NodeId),
    #[error("the node link failed: {0}")]
    Failed(String),
}

/// Requests, subscriptions and activations sent to another node.
///
/// The contract is not observable through the port alone: the in-process adapter
/// ([`LocalLink`](crate::adapters::LocalLink)) and the HTTP/2 adapter of `rewind-server` test it.
/// A request is answered once, or fails with a [`LinkError`]; the receiver of a request it no
/// longer owns answers [`NodeReply::NotOwner`].
pub trait NodeLink: Send + Sync {
    /// The changes a subscription streams.
    type Changes: Stream<Item = Changed> + Send + Unpin + 'static;

    fn call(
        &self,
        node: &NodeId,
        request: NodeRequest,
    ) -> impl Future<Output = Result<NodeReply, LinkError>> + Send;

    /// Subscribes to the changes of a key at the owner of its partition (a run, a namespace), or
    /// at every node (a tenant). `from`: for a run, the history index the stream starts at.
    fn subscribe(
        &self,
        node: &NodeId,
        key: SubscriptionKey,
        from: Option<u64>,
    ) -> impl Future<Output = Result<Self::Changes, LinkError>> + Send;

    /// Runs an activation on a node whose build supports the run's epoch: a remote executor.
    fn activate(
        &self,
        node: &NodeId,
        request: ActivationRequest,
    ) -> impl Future<Output = Result<ActivationResult, LinkError>> + Send;
}

// -------------------------------------------------------------------------------------------
// Requests and replies

/// A request to a node.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeRequest {
    /// To the owner of `partition`, from an edge.
    Partition {
        partition: PartitionId,
        request: Box<PartitionRequest>,
    },
    /// To the organization actor of `tenant`, on the owner of its home partition.
    Organization {
        tenant: TenantId,
        request: Box<OrgRequest>,
    },
    /// To the owner of `partition`, from an organization actor.
    Owner {
        partition: PartitionId,
        notice: Box<OwnerNotice>,
    },
    /// To the edge of a node: a host task handed on an SDK stream it holds.
    Edge(Box<EdgeNotice>),
}

/// The answer to a [`NodeRequest`].
#[derive(Clone, Debug, PartialEq)]
pub enum NodeReply {
    Partition(Box<PartitionReply>),
    Organization(OrgReply),
    /// The request was taken: a notice, or a request answered later by a notice.
    Done,
    /// This node does not hold the partition (or the home partition of the tenant) any more:
    /// the router reads the partition map again and retries.
    NotOwner {
        partition: PartitionId,
    },
}

/// Why an owner refused a request (`archi/07-code-architecture.md`, "HTTP API `/v1`" maps each
/// to its status).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("run {0} not found")]
    RunNotFound(RunId),
    #[error("run {0} has ended")]
    RunEnded(RunId),
    /// A signal the run does not take: a name Rewind keeps, or for a run of a workflow a signal
    /// its version does not declare, or a payload that does not match.
    #[error("{0}")]
    Signal(String),
    /// The namespace's limit or the tenant's ceiling is reached (429).
    #[error("the {} quota {} is reached: {} of {}", scope_name(.0), .0.quota, .0.used, .0.limit)]
    Quota(QuotaExceeded),
    /// The namespace is being erased: it takes no run (409).
    #[error("the namespace is being erased: it takes no run")]
    NamespaceDeleting,
    /// No such call task: it was answered, its run ended, or it was handed out again.
    #[error(
        "the task no longer exists: it was answered, its run ended, or it was handed out again"
    )]
    TaskGone,
    /// Only a task waiting for the harness (`approval` or `parked`) is approved or rejected.
    #[error("task {task} is {state}: only a task waiting for the harness is approved or rejected")]
    TaskNotWaiting { task: String, state: &'static str },
    #[error("the result is {size} bytes as canonical JSON, above the limit of {max} bytes")]
    ResultTooLarge { size: usize, max: usize },
    /// The run is a fork of a run this partition does not hold, or of a run without history.
    #[error("run {0} cannot be forked: it has not started, or it is not found")]
    ParentNotStarted(RunId),
    /// Something the owner needs could not be read or written; the request may be tried again.
    #[error("unavailable: {0}")]
    Unavailable(String),
}

fn scope_name(exceeded: &QuotaExceeded) -> &'static str {
    match exceeded.scope {
        crate::ports::QuotaScope::Namespace => "namespace's",
        crate::ports::QuotaScope::Tenant => "organization's",
    }
}

/// What an edge asks the owner of a partition.
#[derive(Clone, Debug, PartialEq)]
pub enum PartitionRequest {
    /// Starts a run (or a fork) the edge checked: answered `Started`.
    Start(Box<StartAtOwner>),
    /// Answered `Enqueued`.
    Signal(SignalAtOwner),
    /// `cancel_requested { reason }`: answered `Enqueued`.
    Cancel {
        run: RunRef,
        message: Option<String>,
    },
    /// Lifts a quarantine: answered `Done(was quarantined)`.
    Release { run: RunRef },
    /// Ends a quarantined run (`run_aborted`): answered `Done(was quarantined)`.
    GiveUp { run: RunRef },
    /// Erases a run now (`DELETE /v1/runs/{id}`): answered `Done(existed)`.
    Erase { run: RunRef },
    /// A host tool's handler, or the handler of a deferred call, answers through its token:
    /// answered `Answered`. The token names the run; `tenant` is the caller's.
    Answer {
        tenant: TenantId,
        token: TaskToken,
        answer: TaskAnswer,
    },
    /// The harness decides on a task that waits for it: answered `Answered`.
    Decide { task: TaskRef, decision: Decision },
    /// A request posted to a webhook's URL: the signal it is bound to. Answered `Enqueued`.
    Webhook {
        token: String,
        payload: JsonValue,
        key: Option<String>,
    },
    /// The run, live or sealed: answered `Run`.
    GetRun { run: RunRef },
    /// The run of the tenant a handle names, live or sealed: answered `Run`
    /// (`archi/06-storage-and-scale.md`, "Run handles").
    Find { tenant: TenantId, handle: RunHandle },
    /// The entries of the run's attempt from `from`, from its partition or its sealed journal:
    /// answered `History`. With `values`, every payload up to 64 KiB is inlined as a value, as
    /// the sealed journal holds it; larger ones stay digests, their values in the run's blobs.
    History {
        run: RunRef,
        from: u64,
        values: bool,
    },
    /// The run's call tasks, as recorded but for those in flight (sent by the owner's node and
    /// not answered, or handed on an SDK stream), which read `claimed`: answered `Tasks`.
    Tasks { run: RunRef },
    /// A page of the partition's summaries: answered `Found`.
    Search {
        scope: Scope,
        query: RunQuery,
        order: RunOrder,
        page: Page,
    },
    /// The partition's counters over a window, in buckets of `bucket` whole hours from the
    /// window's start (the last one cut at its end), or as one bucket when `None`, and its
    /// gauges: answered `Counted`, one `Counters` per bucket.
    Counters {
        scope: Scope,
        window: Window,
        bucket: Option<NonZeroU32>,
    },
    /// The use in a month of each namespace of the scope the partition holds: CPU time and runs
    /// created over the month, with its days when `days`; active runs and distinct scripts now.
    /// Answered `Used`; a namespace that used nothing is left out.
    Usage {
        scope: Scope,
        month: Month,
        days: bool,
    },
    /// The quota alerts of a month the tenant's organization actor recorded: of one namespace,
    /// or every alert of the tenant (its own included), the newest first. To the owner of the
    /// tenant's home partition: answered `Alerts`.
    QuotaAlerts {
        tenant: TenantId,
        namespace: Option<NamespaceName>,
        month: Month,
    },
    /// The shapes observed for some actions: answered `Shapes`.
    Shapes {
        tenant: TenantId,
        namespace: Option<NamespaceName>,
        actions: Vec<String>,
    },
}

/// What the owner of a partition answers.
#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "a reply is moved once, to its caller: its size does not matter"
)]
pub enum PartitionReply {
    Started(Result<Created, Refusal>),
    Enqueued(Result<Enqueued, Refusal>),
    Done(Result<bool, Refusal>),
    Answered(Result<(), Refusal>),
    Run(Result<Option<RunView>, Refusal>),
    History(Result<Option<Vec<HistoryEntry>>, Refusal>),
    Tasks(Result<Option<Vec<TaskRecord>>, Refusal>),
    Found(Result<Found, Refusal>),
    Counted(Result<(Vec<Counters>, Gauges), Refusal>),
    Shapes(Result<BTreeMap<String, Shape>, Refusal>),
    Used(Result<BTreeMap<NamespaceName, Usage>, Refusal>),
    Alerts(Result<Vec<QuotaAlert>, Refusal>),
}

/// A run as its owner knows it: its head and its summary, and the handles of the runs its
/// metadata names (its parent, the run that started it) while they are kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunView {
    pub head: RunHead,
    pub summary: Option<RunSummary>,
    pub parent: Option<RunHandle>,
    pub started_by: Option<RunHandle>,
}

/// A start the edge checked (script, TTL, input, attachments, namespace): the owner answers a
/// known start key with its run, asks the organization actor to admit it, then creates it.
#[derive(Clone, Debug, PartialEq)]
pub struct StartAtOwner {
    /// The new run, its id drawn for the namespace's partition.
    pub run: RunRef,
    pub namespace: NamespaceName,
    /// What ran, the middle of the run's handle ([`RunHead::origin`]). The owner gives the run
    /// its suffix.
    pub origin: String,
    pub started: RunStarted,
    pub metadata: RunMetadata,
    /// Unique in the namespace for as long as the run is kept.
    pub start_key: Option<String>,
    /// The first line of the script that is not blank: what its summary shows.
    pub headline: String,
    pub created_at: Timestamp,
}

/// A signal for a run.
#[derive(Clone, Debug, PartialEq)]
pub struct SignalAtOwner {
    pub run: RunRef,
    pub name: String,
    pub payload: JsonValue,
    /// A second signal with the same key is acknowledged and ignored.
    pub key: Option<String>,
}

/// A handler's answer to a task, through its token.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskAnswer {
    /// `POST /v1/tasks/{token}/complete`.
    Complete(JsonValue),
    /// `POST /v1/tasks/{token}/fail`.
    Fail(HostFailure),
    /// `POST /v1/tasks/{token}/defer`: answered later through the same token, within its
    /// timeout (bounded by the action's execution timeout and the run's deadline).
    Defer { timeout: Option<Duration> },
}

/// A failure reported by a handler.
#[derive(Clone, Debug, PartialEq)]
pub struct HostFailure {
    /// The message of the `ActionError`, written for the model.
    pub message: String,
    pub data: Option<JsonValue>,
    /// A transient failure: under `retry`, the call is made again after a backoff, up to the
    /// maximum of attempts. Delivered, it has the kind `unavailable`; otherwise `error`.
    pub retryable: bool,
}

/// What the harness decides for a task that waits for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// A task under `approval` is sent; a task parked by a schema change is sent against the
    /// action's current schema; a task parked by a refusal is sent again.
    Approve,
    /// The call fails with an `ActionError` carrying the message.
    Reject(Option<String>),
}

// -------------------------------------------------------------------------------------------
// The organization actor

/// What an owner, or an edge, asks the organization actor of a tenant.
#[derive(Clone, Debug, PartialEq)]
pub enum OrgRequest {
    /// Admits a start: answered `Admission`.
    Admit(Admit),
    /// The absolute counts of one namespace in a partition, after a batch that moved them:
    /// answered `Done`.
    Counts(CountsReport),
    /// A slot of an MCP server's or an API's concurrency, for a call that is ready: answered
    /// `Done`, granted later by [`OwnerNotice::SlotGranted`].
    Slot(SlotRequest),
    /// The call that held a slot is done: answered `Done`.
    SlotFreed { task: TaskRef, server: String },
    /// A host task to hand to an SDK stream: answered `Done`.
    HostTask(Box<HostTaskOffer>),
    /// A host task is answered, or no longer exists: its stream's slot is free. `gone`: it was
    /// not answered through the stream it was handed on (its run ended, its call timed out or
    /// was cancelled), and the stream hears it. Answered `Done`.
    HostTaskDone { task: TaskRef, gone: bool },
    /// An SDK stream opened at an edge: answered `Done`.
    Attach(StreamAttach),
    /// An SDK stream closed: its tasks wait for its worker for their grace. Answered `Done`.
    Detach { stream: StreamId },
}

/// What the organization actor answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OrgReply {
    /// The start is admitted, or refused for a quota.
    Admission(Result<(), Refusal>),
}

/// A start to admit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admit {
    /// The partition of the run and the epoch of its owner: an admission of an owner that is
    /// gone is dropped when the next owner reports.
    pub partition: PartitionId,
    pub epoch: PartitionEpoch,
    pub run: RunId,
    pub namespace: NamespaceName,
    pub script: Digest,
    pub at: Timestamp,
}

/// The counts of a namespace in a partition, as absolute values: a report lost is corrected by
/// the next (`archi/06-storage-and-scale.md`, "Quotas").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CountsReport {
    pub partition: PartitionId,
    pub epoch: PartitionEpoch,
    pub namespace: NamespaceName,
    /// Active runs, and runs kept per script.
    pub gauges: Gauges,
    /// Runs created this month.
    pub created_this_month: u64,
    /// CPU time this month.
    pub cpu_this_month: Duration,
    /// The admitted runs these counts include: their admissions are released.
    pub included: Vec<RunId>,
    /// The admitted runs that were not created after all: released too.
    pub abandoned: Vec<RunId>,
    pub at: Timestamp,
}

/// A slot of a server's concurrency, for one call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotRequest {
    pub partition: PartitionId,
    pub epoch: PartitionEpoch,
    pub task: TaskRef,
    /// The MCP server or the API.
    pub server: String,
    /// Its calls in flight in the organization, at most.
    pub limit: u32,
}

/// A host task handed to SDK streams: which streams may take it, and the token of this
/// hand-out.
#[derive(Clone, Debug, PartialEq)]
pub struct HostTaskOffer {
    pub partition: PartitionId,
    pub epoch: PartitionEpoch,
    pub task: HostTask,
    pub action_set: Digest,
    /// The grace a stream that closes gets to come back before the task is handed again.
    pub grace: Duration,
}

/// A host-tool call as an SDK worker receives it.
#[derive(Clone, Debug, PartialEq)]
pub struct HostTask {
    /// The token of this hand-out: it names the run, and changes with each hand-out.
    pub token: TaskToken,
    pub task: TaskRef,
    /// The end user the handler acts for.
    pub namespace: NamespaceName,
    pub action: String,
    pub args: JsonValue,
    /// The same on every attempt: the handler recognizes a call it already made.
    pub idempotency_key: String,
    /// From 1. Above 1, an earlier attempt failed or was interrupted.
    pub attempt: u32,
    /// The action's execution timeout, if it has one.
    pub execution_timeout: Option<Duration>,
    /// The run's labels.
    pub labels: BTreeMap<String, String>,
    pub trace_context: Option<TraceContext>,
}

/// An SDK stream, named by the edge that holds it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamId(pub String);

/// An SDK stream opened (`GET /v1/tasks/stream`): what it may be handed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamAttach {
    pub stream: StreamId,
    /// The worker behind it: a worker whose stream closed and that opens another within the
    /// grace keeps its tasks.
    pub worker: String,
    /// The node of the edge holding the stream.
    pub node: NodeId,
    /// The tasks of this namespace's runs only: an end user's token.
    pub namespace: Option<NamespaceName>,
    /// The tasks of the runs carrying all of these labels only. Empty: every run.
    pub labels: BTreeMap<String, String>,
    pub action_set: Option<Digest>,
    /// Empty: every host tool.
    pub actions: BTreeSet<String>,
    /// Tasks it holds at once.
    pub slots: u32,
}

/// What an organization actor tells an owner.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnerNotice {
    /// The call may be sent.
    SlotGranted { task: TaskRef },
    /// These host tasks were handed on a stream that closed and did not come back within its
    /// grace: each is interrupted (attempt + 1, or `outcome_unknown` under `at_most_once`).
    StreamLost { tasks: Vec<(TaskRef, TaskToken)> },
    /// A new organization actor asks for the counts of the tenant's namespaces.
    CountsWanted { tenant: TenantId },
}

/// What an organization actor tells the edge that holds an SDK stream, or the owner of a
/// partition every edge.
#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "a notice is boxed in its request, and moved once: its size does not matter"
)]
pub enum EdgeNotice {
    /// A host task handed on the stream.
    HandOut { stream: StreamId, task: HostTask },
    /// A task handed on the stream no longer waits for its handler's answer, under this token:
    /// its run ended, its call timed out or was cancelled, or it was handed out again. Its slot
    /// on the stream is free.
    Gone { stream: StreamId, token: TaskToken },
    /// The partition was opened, by a new owner or again by the same node: the organization
    /// actors homed there start without the streams their previous actors held. The edge
    /// attaches its streams of the tenants homed there again.
    Reopened { partition: PartitionId },
}

/// What an SDK stream receives (`GET /v1/tasks/stream`, its events `task` and `gone`).
#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "an event is moved once, to its stream: its size does not matter"
)]
pub enum StreamEvent {
    /// A host task handed on the stream.
    Task(HostTask),
    /// The task handed with this token no longer waits for an answer.
    Gone(TaskToken),
}

// -------------------------------------------------------------------------------------------
// Subscriptions

/// What an edge subscribes to (`archi/05-testing-and-observability.md`, "Dashboard": the change
/// stream).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SubscriptionKey {
    /// One run, at its owner.
    Run(RunRef),
    /// The runs of a namespace, at its owner.
    Namespace {
        tenant: TenantId,
        namespace: NamespaceName,
    },
    /// The runs of a tenant, at every node.
    Tenant(TenantId),
}

/// A run changed: sent once the batch that changed it is durable.
#[derive(Clone, Debug, PartialEq)]
pub struct Changed {
    pub run: RunRef,
    pub namespace: NamespaceName,
    /// Its summary as it is now; `None` once it is erased.
    pub summary: Option<RunSummary>,
    /// For a subscription to the run: the entries the batch committed, from the index the
    /// subscriber has.
    pub entries: Vec<HistoryEntry>,
}

// -------------------------------------------------------------------------------------------
// Remote executors

/// An activation an owner sends to a node of the run's epoch: what its executor needs.
#[derive(Clone, Debug, PartialEq)]
pub struct ActivationRequest {
    pub partition: PartitionId,
    pub head: RunHead,
    /// The run's summary as it is now: what the new one is built on.
    pub summary: Option<IndexedSummary>,
    pub inbox: Vec<InboxItem>,
    /// The attempt's history. `None` when the owner expects the executor's instance cache to
    /// hold the run at its history length: a miss answers [`ActivationResult::Miss`], and the
    /// owner sends it again with the history.
    pub history: Option<Vec<HistoryEntry>>,
    /// The payload records of the history and the inbox held by their digest.
    pub payloads: Vec<PayloadRecord>,
    /// For a fork's first activation: its parent's history and payloads.
    pub parent: Option<ParentHistory>,
    pub script: StoredScript,
    pub action_set: StoredActionSet,
    pub purpose: Purpose,
    /// The run's call tasks: what its summary shows of its calls.
    pub tasks: Vec<TaskRecord>,
}

/// The history a fork follows.
#[derive(Clone, Debug, PartialEq)]
pub struct ParentHistory {
    pub run: RunId,
    pub entries: Vec<HistoryEntry>,
    pub payloads: Vec<PayloadRecord>,
    /// The parent's sealed attempt, when it was read from the archive.
    pub sealed: Option<Box<SealedAttempt>>,
}

/// Why a run is activated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// It has something in its inbox.
    Runnable,
    /// It is quarantined, and a node of a new build tries it once.
    QuarantineRetry,
    /// It is quarantined, and the operator gives up on it, or its deadline passed: it ends with
    /// `run_aborted`.
    GiveUp,
}

/// What an activation did.
#[derive(Clone, Debug, PartialEq)]
pub enum ActivationResult {
    /// The changes to commit: the activation and what leaves the partition with it.
    Committed(Box<Committed>),
    /// Nothing is committed and the run is quarantined.
    Quarantined {
        kind: QuarantineKind,
        reason: String,
    },
    /// A quarantined run replays again: it leaves quarantine. `nothing_to_do`: its inbox was
    /// empty.
    Released,
    /// Nothing was committed; the owner records the failure.
    Failed { error: String },
    /// The executor's instance cache does not hold the run at its history length: send the
    /// history.
    Miss,
}

/// An activation to commit.
#[derive(Clone, Debug, PartialEq)]
pub struct Committed {
    /// Its outbox is empty: the owner numbers the entries of `projections` and adds them.
    pub activation: Activation,
    pub projections: Vec<Projection>,
    /// The run ended: the end its watchers receive as `rewind:ended:<run>`.
    pub end: Option<JsonValue>,
    /// The runs the attempt started with a call of a workflow's `start` whose end the
    /// history does not hold, by call: attached ones are cancelled when the run ends; a fork's
    /// first activation watches those it copied.
    pub children: Vec<(Seq, RunId, bool)>,
    /// A fork's first activation copied its parent's history.
    pub copied: bool,
    /// The CPU time of the calls into the script that executed, toward the namespace's
    /// `cpuPerMonth`.
    pub cpu: Duration,
    /// The run's mode, from its `run_started`.
    pub mode: Option<rewind_core::RunMode>,
    /// The run metrics of the new entries, recorded once the batch is durable.
    pub measurements: Vec<crate::metrics::RunMeasurement>,
}
