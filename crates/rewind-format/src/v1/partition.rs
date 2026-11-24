//! The partition records, v1 (`archi/02-history.md`, "Stored format v1";
//! `archi/06-storage-and-scale.md`, "A partition").
//!
//! One record per kind of thing a partition holds. Each is written `{"v":1,…}` by
//! [`crate::partition::encode`], its fields in the order they are declared here, and read back by
//! [`crate::partition::decode`], which refuses an unknown version, field or variant. Run ids are
//! UUID strings, digests and trace ids lowercase hex, times milliseconds since the Unix epoch, and
//! payload values their canonical JSON (RFC 8785). Optional fields and empty collections are left
//! out.

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{
    ActionFailureKind, ActionPolicy, ActionRoute, Digest, Event, JsonValue, Payload, RunId,
    RunMode, RunStatus, ServerSecrets, Shape as CoreShape,
};
use rewind_core::{LogLevel, WorkflowOp};
use serde::{Deserialize, Serialize};

// -------------------------------------------------------------------------------------------
// The run's records

/// A run's head: its service state and its metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunHead {
    #[serde(with = "with::run_id")]
    pub run: RunId,
    pub tenant: String,
    pub namespace: String,
    /// What ran, the middle of the run's handle; `script` when left out.
    #[serde(default = "script_origin", skip_serializing_if = "is_script_origin")]
    pub origin: String,
    /// The characters of its handle's suffix; 12 when left out.
    #[serde(default = "suffix_len", skip_serializing_if = "is_suffix_len")]
    pub suffix_len: u8,
    #[serde(with = "with::digest")]
    pub code: Digest,
    #[serde(with = "with::digest")]
    pub action_set: Digest,
    pub status: Status,
    pub epoch: u32,
    pub attempt: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed: Option<u32>,
    pub failures: u32,
    pub unfinished: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine: Option<Quarantine>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waits: Vec<Wait>,
    pub expires_at: u64,
    pub history_len: u64,
    #[serde(default, skip_serializing_if = "Metadata::is_empty")]
    pub metadata: Metadata,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
}

/// The run metadata a head keeps (`archi/05-testing-and-observability.md`, "Run metadata").
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_context: Option<TraceContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ForkSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<RunWorkflow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<StartedBy>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub allowed_labels: BTreeMap<String, String>,
    /// By server or API: `org/<name>` or `ns/<name>`.
    #[serde(
        default,
        with = "with::server_secrets",
        skip_serializing_if = "ServerSecrets::is_empty"
    )]
    pub secrets: ServerSecrets,
}

impl Metadata {
    pub fn is_empty(&self) -> bool {
        *self == Metadata::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceContext {
    pub traceparent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracestate: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkSource {
    #[serde(with = "with::run_id")]
    pub run: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
}

/// The workflow a run is a run of, and the version it executes: the workflow's id, its name
/// when the run was created, the version's number and digest, and whether the version has an
/// interface (its runs then accept only the signals it declares).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunWorkflow {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub version: u32,
    #[serde(with = "with::digest")]
    pub digest: Digest,
    #[serde(default, skip_serializing_if = "is_false")]
    pub interface: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartedBy {
    #[serde(with = "with::run_id")]
    pub run: RunId,
    pub seq: u32,
    pub detached: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Quarantine {
    pub kind: QuarantineKind,
    pub reason: String,
    pub since: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retried_by: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineKind {
    NonDeterminism,
    Failures,
}

/// What a run waits for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wait {
    pub kind: WaitKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub since: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub callsite: Option<Callsite>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskPhase>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitKind {
    Signal,
    Timer,
    Call,
    Harness,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    Approval,
    Ready,
    Claimed,
    Deferred,
    Parked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Callsite {
    pub line: u32,
    pub column: u32,
}

/// An event of an attempt's history: the event in the stored format v1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryEvent {
    pub index: u64,
    pub recorded_at: u64,
    #[serde(with = "with::event")]
    pub event: Event,
}

/// A completion, or the run's `run_started`, not yet delivered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboxItem {
    pub id: u64,
    pub received_at: u64,
    /// A `run_started` or a completion.
    #[serde(with = "with::event")]
    pub trigger: Event,
}

/// A signal key the run took: the key itself is the record's place.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalKey {}

/// A call's task.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub tenant: String,
    #[serde(with = "with::run_id")]
    pub run: RunId,
    pub seq: u32,
    #[serde(with = "with::digest")]
    pub action_set: Digest,
    pub action: String,
    #[serde(with = "with::route")]
    pub route: ActionRoute,
    #[serde(with = "with::payload")]
    pub args: Payload,
    #[serde(with = "with::policy")]
    pub policy: ActionPolicy,
    pub state: TaskState,
    pub delivery: u32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub schema_change_approved: bool,
    pub created_at: u64,
    #[serde(
        default,
        with = "with::digest_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub token_hash: Option<Digest>,
}

/// The origin of a run without a workflow and whose code has no title
/// ([`rewind_core::SCRIPT_ORIGIN`]).
fn script_origin() -> String {
    rewind_core::SCRIPT_ORIGIN.to_owned()
}

fn is_script_origin(origin: &str) -> bool {
    origin == rewind_core::SCRIPT_ORIGIN
}

/// The characters of a handle's suffix that tells its run apart ([`rewind_core::SUFFIX_LEN`]).
fn suffix_len() -> u8 {
    rewind_core::SUFFIX_LEN as u8
}

fn is_suffix_len(len: &u8) -> bool {
    usize::from(*len) == rewind_core::SUFFIX_LEN
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// `{"type":"approval"}`, `{"type":"ready","next_delivery_at":…}`,
/// `{"type":"claimed","owner":…,"until":…}`, `{"type":"deferred","until":…}` or
/// `{"type":"parked","kind":…,"message":…}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskState {
    Approval {},
    Ready { next_delivery_at: u64 },
    Claimed { owner: String, until: u64 },
    Deferred { until: u64 },
    Parked { kind: ParkKind, message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkKind {
    SchemaChanged,
    CredentialsRefused,
    SecretMissing,
}

/// A timer: when it is due, and what fires.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timer {
    pub at: u64,
    pub tenant: String,
    pub kind: TimerKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimerKind {
    Code {
        #[serde(with = "with::run_id")]
        run: RunId,
        seq: u32,
    },
    CallTimeout {
        #[serde(with = "with::run_id")]
        run: RunId,
        seq: u32,
    },
    DeferredDeadline {
        #[serde(with = "with::run_id")]
        run: RunId,
        seq: u32,
    },
    RedeliveryBackoff {
        #[serde(with = "with::run_id")]
        run: RunId,
        seq: u32,
    },
    RunDeadline {
        #[serde(with = "with::run_id")]
        run: RunId,
    },
    ActivationRetry {
        #[serde(with = "with::run_id")]
        run: RunId,
    },
    RetentionEnd {
        #[serde(with = "with::run_id")]
        run: RunId,
    },
    WebhookRetry {
        entry: u64,
    },
}

/// A payload of a run kept outside its events: its size and preview, and its value up to the
/// partition's tier, in its canonical form.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayloadValue {
    #[serde(with = "with::digest")]
    pub digest: Digest,
    pub size: u64,
    pub preview: String,
    #[serde(
        default,
        with = "with::json_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub value: Option<JsonValue>,
}

/// A run that waits for this run's end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Watcher {
    #[serde(with = "with::run_id")]
    pub run: RunId,
    pub seq: u32,
    pub attached: bool,
}

/// What a signal URL token, known by its SHA-256, is bound to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignalUrlToken {
    pub tenant: String,
    #[serde(with = "with::run_id")]
    pub run: RunId,
    pub name: String,
}

/// A run's summary in the index, with what the search reads beside it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub tenant: String,
    #[serde(with = "with::run_id")]
    pub run: RunId,
    pub namespace: String,
    /// As the run's head has them.
    #[serde(default = "script_origin", skip_serializing_if = "is_script_origin")]
    pub origin: String,
    #[serde(default = "suffix_len", skip_serializing_if = "is_suffix_len")]
    pub suffix_len: u8,
    #[serde(with = "with::digest")]
    pub code: Digest,
    pub headline: String,
    #[serde(with = "with::digest")]
    pub action_set: Digest,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<Mode>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(
        default,
        with = "with::run_id_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub source: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<RunWorkflow>,
    #[serde(
        default,
        with = "with::run_id_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub started_by: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
    pub last_activity_at: u64,
    pub calls: u64,
    pub errors: u64,
    pub history_len: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waits: Vec<Wait>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wakes_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ending: Option<Ending>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine: Option<QuarantineNote>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub actions: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub call_errors: BTreeSet<FailureKind>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ending {
    pub error: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub signature: String,
    pub fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuarantineNote {
    pub kind: QuarantineKind,
    pub reason: String,
    pub since: u64,
}

/// The run a start key started in its namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartKey {
    #[serde(with = "with::run_id")]
    pub run: RunId,
}

// -------------------------------------------------------------------------------------------
// Counters, gauges and shapes

/// The counters of one namespace and hour.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Counters {
    pub created: u64,
    #[serde(default, skip_serializing_if = "RunCounts::is_empty")]
    pub runs: RunCounts,
    #[serde(default, skip_serializing_if = "Sketch::is_empty")]
    pub durations: Sketch,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cpu_ms: u64,
    /// By workflow id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workflows: BTreeMap<String, WorkflowCounts>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub failures: BTreeMap<String, FailureCounts>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, ActionCounts>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunCounts {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub started: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub ended: BTreeMap<Status, u64>,
}

impl RunCounts {
    pub fn is_empty(&self) -> bool {
        *self == RunCounts::default()
    }
}

/// A sketch of durations: count, sum, maximum, and the counts by bucket.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sketch {
    pub count: u64,
    pub sum_ms: u64,
    pub max_ms: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub buckets: BTreeMap<u16, u64>,
}

impl Sketch {
    pub fn is_empty(&self) -> bool {
        *self == Sketch::default()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureCounts {
    pub signature: String,
    pub error: String,
    pub runs: u64,
    /// By workflow id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workflows: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lines: BTreeMap<u32, u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub latest: Vec<LatestRun>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LatestRun {
    #[serde(with = "with::run_id")]
    pub run: RunId,
    pub ended_at: u64,
    pub status: Status,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionCounts {
    pub calls: u64,
    pub completed: u64,
    pub failed: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub errors: BTreeMap<FailureKind, u64>,
    pub redeliveries: u64,
    #[serde(default, skip_serializing_if = "Sketch::is_empty")]
    pub latency: Sketch,
}

/// The runs of one workflow.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCounts {
    #[serde(default, skip_serializing_if = "RunCounts::is_empty")]
    pub runs: RunCounts,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub actions: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_started_at: Option<u64>,
}

/// The shape of the results observed for one action in one namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shape {
    /// `"unknown"`, `"null"`, `"boolean"`, `"number"`, `"string"`, `{"array":…}`,
    /// `{"object":{"key":{"optional":false,"shape":…}}}` or `{"union":[…]}`.
    #[serde(with = "with::shape")]
    pub shape: CoreShape,
}

/// What holds now among the runs a namespace keeps in the partition.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gauges {
    pub active_runs: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub waiting: BTreeMap<WaitingFor, u64>,
    pub quarantined: u64,
    /// The runs kept, by workflow id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub workflows: BTreeMap<String, u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingFor {
    Signal,
    Timer,
    Call,
    Harness,
    Approval,
    Parked,
    Deferred,
}

// -------------------------------------------------------------------------------------------
// The outbox and the tenants

/// A delivery waiting to leave the partition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outbox {
    pub id: u64,
    pub tenant: String,
    #[serde(
        default,
        with = "with::run_id_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub run: Option<RunId>,
    pub projection: Projection,
}

/// `{"type":"otel","spans":[…],"logs":[…]}`, `{"type":"webhooks","webhook":{…}}` or
/// `{"type":"quota_alert","alert":{…}}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Projection {
    Otel {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        spans: Vec<Span>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        logs: Vec<Log>,
    },
    Webhooks {
        webhook: Webhook,
    },
    QuotaAlert {
        alert: QuotaAlert,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Span {
    #[serde(with = "with::hex")]
    pub trace_id: [u8; 16],
    #[serde(with = "with::hex")]
    pub span_id: [u8; 8],
    #[serde(
        default,
        with = "with::hex_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub parent: Option<[u8; 8]>,
    pub name: String,
    pub kind: SpanKind,
    pub start: u64,
    pub end: u64,
    pub status: SpanStatus,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Attribute>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpanKind {
    Internal,
    Client,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SpanStatus {
    Unset {},
    Ok {},
    Error { message: String },
}

/// `{"string":…}`, `{"int":…}`, `{"float":…}` or `{"bool":…}`. A float that is not finite is
/// written `"NaN"`, `"Infinity"` or `"-Infinity"`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Attribute {
    String(String),
    Int(i64),
    Float(#[serde(with = "with::float")] f64),
    Bool(bool),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Log {
    #[serde(with = "with::hex")]
    pub trace_id: [u8; 16],
    #[serde(
        default,
        with = "with::hex_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub span_id: Option<[u8; 8]>,
    pub time: u64,
    pub level: Level,
    pub message: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub attributes: BTreeMap<String, Attribute>,
}

/// A webhook of a run, or a quota alert, for the tenant's subscriptions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    pub id: String,
    pub tenant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(
        default,
        with = "with::run_id_opt",
        skip_serializing_if = "Option::is_none"
    )]
    pub run: Option<RunId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    pub occurred_at: u64,
    pub kind: WebhookKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WebhookKind {
    RunCompleted {
        #[serde(with = "with::payload")]
        output: Payload,
    },
    RunFailed {
        status: Status,
        error: String,
    },
    RunWaiting {
        waiting_for: Vec<String>,
    },
    RunQuarantined {
        reason: String,
    },
    ActionSchemaChanged {
        seq: u32,
        action: String,
        message: String,
    },
    SecretExpired {
        seq: u32,
        action: String,
        message: String,
        reason: SecretExpiredReason,
        /// `org/<name>` or `ns/<name>`.
        #[serde(
            default,
            with = "with::secret_opt",
            skip_serializing_if = "Option::is_none"
        )]
        secret: Option<rewind_core::SecretRef>,
    },
    QuotaAlert {
        alert: QuotaAlert,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretExpiredReason {
    Refused,
    Missing,
}

/// A quota threshold crossed, once per tenant, namespace (or the tenant), quota, month and
/// threshold.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaAlert {
    pub tenant: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub quota: QuotaKind,
    pub year: i16,
    pub month: i8,
    pub threshold: Threshold,
    pub used: u64,
    pub limit: u64,
    pub raised_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaKind {
    CpuPerMonth,
    ActiveRuns,
    RunsPerMonth,
    Workflows,
    MaxTtl,
    MaxCodeLines,
}

/// `near`: 80 % of the limit; `reached`: 100 %.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Threshold {
    Near,
    Reached,
}

/// What an tenant actor keeps in its home partition.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantState {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<QuotaAlert>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub admissions: Vec<Admission>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admission {
    #[serde(with = "with::run_id")]
    pub run: RunId,
    pub namespace: String,
    /// The id of the workflow the run is a run of; none for a test run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub admitted_at: u64,
}

// -------------------------------------------------------------------------------------------
// The enumerations of rewind-core

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    Active,
    Completed,
    Failed,
    Cancelled,
    Aborted,
}

impl From<RunStatus> for Status {
    fn from(status: RunStatus) -> Self {
        match status {
            RunStatus::Pending => Status::Pending,
            RunStatus::Active => Status::Active,
            RunStatus::Completed => Status::Completed,
            RunStatus::Failed => Status::Failed,
            RunStatus::Cancelled => Status::Cancelled,
            RunStatus::Aborted => Status::Aborted,
        }
    }
}

impl From<Status> for RunStatus {
    fn from(status: Status) -> Self {
        match status {
            Status::Pending => RunStatus::Pending,
            Status::Active => RunStatus::Active,
            Status::Completed => RunStatus::Completed,
            Status::Failed => RunStatus::Failed,
            Status::Cancelled => RunStatus::Cancelled,
            Status::Aborted => RunStatus::Aborted,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Live,
    Test,
}

impl From<RunMode> for Mode {
    fn from(mode: RunMode) -> Self {
        match mode {
            RunMode::Live => Mode::Live,
            RunMode::Test => Mode::Test,
        }
    }
}

impl From<Mode> for RunMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Live => RunMode::Live,
            Mode::Test => RunMode::Test,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Error,
    Unavailable,
    Timeout,
    OutcomeUnknown,
}

impl From<ActionFailureKind> for FailureKind {
    fn from(kind: ActionFailureKind) -> Self {
        match kind {
            ActionFailureKind::Error => FailureKind::Error,
            ActionFailureKind::Unavailable => FailureKind::Unavailable,
            ActionFailureKind::Timeout => FailureKind::Timeout,
            ActionFailureKind::OutcomeUnknown => FailureKind::OutcomeUnknown,
        }
    }
}

impl From<FailureKind> for ActionFailureKind {
    fn from(kind: FailureKind) -> Self {
        match kind {
            FailureKind::Error => ActionFailureKind::Error,
            FailureKind::Unavailable => ActionFailureKind::Unavailable,
            FailureKind::Timeout => ActionFailureKind::Timeout,
            FailureKind::OutcomeUnknown => ActionFailureKind::OutcomeUnknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl From<LogLevel> for Level {
    fn from(level: LogLevel) -> Self {
        match level {
            LogLevel::Debug => Level::Debug,
            LogLevel::Info => Level::Info,
            LogLevel::Warn => Level::Warn,
            LogLevel::Error => Level::Error,
        }
    }
}

impl From<Level> for LogLevel {
    fn from(level: Level) -> Self {
        match level {
            Level::Debug => LogLevel::Debug,
            Level::Info => LogLevel::Info,
            Level::Warn => LogLevel::Warn,
            Level::Error => LogLevel::Error,
        }
    }
}

// -------------------------------------------------------------------------------------------
// How the values of rewind-core are written

mod with {
    use super::*;
    use serde::de::{Deserializer, Error as _};
    use serde::ser::Serializer;

    use crate::v1::action_set::{PolicyV1, policy_dto, policy_from_dto};
    use crate::v1::events::{
        CanonicalJson, EventV1, PayloadV1, digest as parse_digest, hex as to_hex,
    };

    pub(super) mod digest {
        use super::*;

        pub(crate) fn serialize<S: Serializer>(digest: &Digest, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&digest.to_hex())
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Digest, D::Error> {
            let hex = String::deserialize(d)?;
            parse_digest(&hex).map_err(D::Error::custom)
        }
    }

    pub(super) mod digest_opt {
        use super::*;

        pub(crate) fn serialize<S: Serializer>(
            digest: &Option<Digest>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match digest {
                Some(digest) => s.serialize_some(&digest.to_hex()),
                None => s.serialize_none(),
            }
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<Digest>, D::Error> {
            Option::<String>::deserialize(d)?
                .map(|hex| parse_digest(&hex).map_err(D::Error::custom))
                .transpose()
        }
    }

    pub(super) mod run_id {
        use super::*;

        pub(crate) fn serialize<S: Serializer>(run: &RunId, s: S) -> Result<S::Ok, S::Error> {
            s.serialize_str(&run.to_string())
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<RunId, D::Error> {
            String::deserialize(d)?
                .parse::<RunId>()
                .map_err(|err| D::Error::custom(err.to_string()))
        }
    }

    pub(super) mod run_id_opt {
        use super::*;

        pub(crate) fn serialize<S: Serializer>(
            run: &Option<RunId>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match run {
                Some(run) => s.serialize_some(&run.to_string()),
                None => s.serialize_none(),
            }
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<RunId>, D::Error> {
            Option::<String>::deserialize(d)?
                .map(|text| {
                    text.parse::<RunId>()
                        .map_err(|err| D::Error::custom(err.to_string()))
                })
                .transpose()
        }
    }

    pub(super) mod event {
        use super::*;

        pub(crate) fn serialize<S: Serializer>(event: &Event, s: S) -> Result<S::Ok, S::Error> {
            EventV1::from(event).serialize(s)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Event, D::Error> {
            EventV1::deserialize(d)?
                .try_into()
                .map_err(D::Error::custom)
        }
    }

    pub(super) mod payload {
        use super::*;

        pub(crate) fn serialize<S: Serializer>(payload: &Payload, s: S) -> Result<S::Ok, S::Error> {
            PayloadV1::from(payload).serialize(s)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Payload, D::Error> {
            PayloadV1::deserialize(d)?
                .try_into()
                .map_err(D::Error::custom)
        }
    }

    pub(super) mod json_opt {
        use super::*;

        pub(crate) fn serialize<S: Serializer>(
            value: &Option<JsonValue>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match value {
                Some(value) => s.serialize_some(&CanonicalJson(value.clone())),
                None => s.serialize_none(),
            }
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<JsonValue>, D::Error> {
            Ok(Option::<CanonicalJson>::deserialize(d)?.map(|CanonicalJson(value)| value))
        }
    }

    pub(super) mod policy {
        use super::*;

        pub(crate) fn serialize<S: Serializer>(
            policy: &ActionPolicy,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            policy_dto(policy).serialize(s)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<ActionPolicy, D::Error> {
            Ok(policy_from_dto(PolicyV1::deserialize(d)?))
        }
    }

    /// A task's route, as an action set writes it, and the workflows' built-ins:
    /// `{"type":"builtin","builtin":"workflow","workflow":"triage","op":"start"}`.
    #[derive(Serialize, Deserialize)]
    #[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
    enum RouteV1 {
        Mcp {
            server: String,
        },
        Api {
            api: String,
        },
        Harness {
            #[serde(default, skip_serializing_if = "Option::is_none")]
            group: Option<String>,
        },
        Builtin {
            builtin: BuiltinV1,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            allowed_hosts: Option<Vec<String>>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            workflow: Option<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            op: Option<OpV1>,
        },
    }

    #[derive(Serialize, Deserialize, Clone, Copy)]
    #[serde(rename_all = "snake_case")]
    enum BuiltinV1 {
        Http,
        SignalUrl,
        Workflow,
    }

    #[derive(Serialize, Deserialize, Clone, Copy)]
    #[serde(rename_all = "snake_case")]
    enum OpV1 {
        Start,
        Signal,
    }

    pub(super) mod route {
        use super::*;
        use rewind_core::Builtin;

        pub(crate) fn serialize<S: Serializer>(
            route: &ActionRoute,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            let builtin = |builtin, allowed_hosts, workflow, op| RouteV1::Builtin {
                builtin,
                allowed_hosts,
                workflow,
                op,
            };
            let dto = match route {
                ActionRoute::Mcp { server } => RouteV1::Mcp {
                    server: server.clone(),
                },
                ActionRoute::Api { api } => RouteV1::Api { api: api.clone() },
                ActionRoute::Harness { group } => RouteV1::Harness {
                    group: group.clone(),
                },
                ActionRoute::Builtin(Builtin::Http { allowed_hosts }) => {
                    builtin(BuiltinV1::Http, Some(allowed_hosts.clone()), None, None)
                }
                ActionRoute::Builtin(Builtin::SignalUrl) => {
                    builtin(BuiltinV1::SignalUrl, None, None, None)
                }
                ActionRoute::Builtin(Builtin::Workflow { workflow, op }) => builtin(
                    BuiltinV1::Workflow,
                    None,
                    Some(workflow.clone()),
                    Some(match op {
                        WorkflowOp::Start => OpV1::Start,
                        WorkflowOp::Signal => OpV1::Signal,
                    }),
                ),
            };
            dto.serialize(s)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<ActionRoute, D::Error> {
            Ok(match RouteV1::deserialize(d)? {
                RouteV1::Mcp { server } => ActionRoute::Mcp { server },
                RouteV1::Api { api } => ActionRoute::Api { api },
                RouteV1::Harness { group } => ActionRoute::Harness { group },
                RouteV1::Builtin {
                    builtin: BuiltinV1::Http,
                    allowed_hosts: Some(allowed_hosts),
                    workflow: None,
                    op: None,
                } => ActionRoute::Builtin(Builtin::Http { allowed_hosts }),
                RouteV1::Builtin {
                    builtin: BuiltinV1::SignalUrl,
                    allowed_hosts: None,
                    workflow: None,
                    op: None,
                } => ActionRoute::Builtin(Builtin::SignalUrl),
                RouteV1::Builtin {
                    builtin: BuiltinV1::Workflow,
                    allowed_hosts: None,
                    workflow: Some(workflow),
                    op: Some(op),
                } => ActionRoute::Builtin(Builtin::Workflow {
                    workflow,
                    op: match op {
                        OpV1::Start => WorkflowOp::Start,
                        OpV1::Signal => WorkflowOp::Signal,
                    },
                }),
                RouteV1::Builtin { .. } => {
                    return Err(D::Error::custom(
                        "a built-in route takes \"allowed_hosts\" for http, \"workflow\" and \
                         \"op\" for a workflow, and nothing else",
                    ));
                }
            })
        }
    }

    pub(super) mod server_secrets {
        use super::*;
        use rewind_core::SecretRef;

        pub(crate) fn serialize<S: Serializer>(
            secrets: &ServerSecrets,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            s.collect_map(
                secrets
                    .iter()
                    .map(|(server, secret)| (server, secret.to_string())),
            )
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<ServerSecrets, D::Error> {
            BTreeMap::<String, String>::deserialize(d)?
                .into_iter()
                .map(|(server, secret)| {
                    let secret = SecretRef::parse(&secret)
                        .map_err(|err| D::Error::custom(err.to_string()))?;
                    Ok((server, secret))
                })
                .collect()
        }
    }

    pub(super) mod secret_opt {
        use super::*;
        use rewind_core::SecretRef;

        pub(crate) fn serialize<S: Serializer>(
            secret: &Option<SecretRef>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match secret {
                Some(secret) => s.serialize_some(&secret.to_string()),
                None => s.serialize_none(),
            }
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<SecretRef>, D::Error> {
            Option::<String>::deserialize(d)?
                .map(|text| {
                    SecretRef::parse(&text).map_err(|err| D::Error::custom(err.to_string()))
                })
                .transpose()
        }
    }

    /// Bytes as lowercase hex, of a fixed length.
    pub(super) mod hex {
        use super::*;

        pub(crate) fn serialize<S: Serializer, const N: usize>(
            bytes: &[u8; N],
            s: S,
        ) -> Result<S::Ok, S::Error> {
            s.serialize_str(&to_hex(bytes))
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
            d: D,
        ) -> Result<[u8; N], D::Error> {
            let text = String::deserialize(d)?;
            unhex(&text).ok_or_else(|| {
                D::Error::custom(format!(
                    "invalid id {text:?}: expected {} lowercase hex digits",
                    2 * N
                ))
            })
        }

        pub(super) fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
            let digits = text.as_bytes();
            if digits.len() != 2 * N {
                return None;
            }
            let nibble = |c: u8| match c {
                b'0'..=b'9' => Some(c - b'0'),
                b'a'..=b'f' => Some(c - b'a' + 10),
                _ => None,
            };
            let mut bytes = [0u8; N];
            for (i, byte) in bytes.iter_mut().enumerate() {
                *byte = nibble(digits[2 * i])? << 4 | nibble(digits[2 * i + 1])?;
            }
            Some(bytes)
        }
    }

    pub(super) mod hex_opt {
        use super::*;

        pub(crate) fn serialize<S: Serializer, const N: usize>(
            bytes: &Option<[u8; N]>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match bytes {
                Some(bytes) => s.serialize_some(&to_hex(bytes)),
                None => s.serialize_none(),
            }
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>, const N: usize>(
            d: D,
        ) -> Result<Option<[u8; N]>, D::Error> {
            Option::<String>::deserialize(d)?
                .map(|text| {
                    super::hex::unhex(&text).ok_or_else(|| {
                        D::Error::custom(format!(
                            "invalid id {text:?}: expected {} lowercase hex digits",
                            2 * N
                        ))
                    })
                })
                .transpose()
        }
    }

    pub(super) mod float {
        use super::*;

        #[derive(Serialize, Deserialize)]
        #[serde(untagged)]
        enum FloatV1 {
            Number(f64),
            Special(String),
        }

        pub(crate) fn serialize<S: Serializer>(value: &f64, s: S) -> Result<S::Ok, S::Error> {
            if value.is_finite() {
                s.serialize_f64(*value)
            } else if value.is_nan() {
                s.serialize_str("NaN")
            } else if *value > 0.0 {
                s.serialize_str("Infinity")
            } else {
                s.serialize_str("-Infinity")
            }
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
            match FloatV1::deserialize(d)? {
                FloatV1::Number(value) => Ok(value),
                FloatV1::Special(text) => match text.as_str() {
                    "NaN" => Ok(f64::NAN),
                    "Infinity" => Ok(f64::INFINITY),
                    "-Infinity" => Ok(f64::NEG_INFINITY),
                    _ => Err(D::Error::custom(format!("invalid float {text:?}"))),
                },
            }
        }
    }

    pub(super) mod shape {
        use super::*;
        use rewind_core::Field;
        use serde_json::{Map, Value, json};

        fn value(shape: &CoreShape) -> Value {
            match shape {
                CoreShape::Unknown => json!("unknown"),
                CoreShape::Null => json!("null"),
                CoreShape::Boolean => json!("boolean"),
                CoreShape::Number => json!("number"),
                CoreShape::String => json!("string"),
                CoreShape::Array(items) => json!({ "array": value(items) }),
                CoreShape::Object(fields) => {
                    let fields: Map<String, Value> = fields
                        .iter()
                        .map(|(key, field)| {
                            let field = json!({
                                "optional": field.optional,
                                "shape": value(&field.shape),
                            });
                            (key.clone(), field)
                        })
                        .collect();
                    json!({ "object": fields })
                }
                CoreShape::Union(shapes) => {
                    json!({ "union": shapes.iter().map(value).collect::<Vec<_>>() })
                }
            }
        }

        fn of(value: &Value) -> Option<CoreShape> {
            match value {
                Value::String(name) => match name.as_str() {
                    "unknown" => Some(CoreShape::Unknown),
                    "null" => Some(CoreShape::Null),
                    "boolean" => Some(CoreShape::Boolean),
                    "number" => Some(CoreShape::Number),
                    "string" => Some(CoreShape::String),
                    _ => None,
                },
                Value::Object(object) if object.len() == 1 => {
                    let (tag, inner) = object.iter().next()?;
                    match tag.as_str() {
                        "array" => Some(CoreShape::Array(Box::new(of(inner)?))),
                        "object" => {
                            let fields = inner
                                .as_object()?
                                .iter()
                                .map(|(key, field)| {
                                    let field = field.as_object()?;
                                    if field.len() != 2 {
                                        return None;
                                    }
                                    let shape = of(field.get("shape")?)?;
                                    let optional = field.get("optional")?.as_bool()?;
                                    Some((key.clone(), Field { shape, optional }))
                                })
                                .collect::<Option<_>>()?;
                            Some(CoreShape::Object(fields))
                        }
                        "union" => Some(CoreShape::Union(
                            inner.as_array()?.iter().map(of).collect::<Option<_>>()?,
                        )),
                        _ => None,
                    }
                }
                _ => None,
            }
        }

        pub(crate) fn serialize<S: Serializer>(shape: &CoreShape, s: S) -> Result<S::Ok, S::Error> {
            value(shape).serialize(s)
        }

        pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<CoreShape, D::Error> {
            let value = Value::deserialize(d)?;
            of(&value).ok_or_else(|| D::Error::custom(format!("invalid shape {value}")))
        }
    }
}
