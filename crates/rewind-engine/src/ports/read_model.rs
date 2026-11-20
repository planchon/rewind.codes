use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::ops::Bound;

use rewind_core::{
    Callsite, Digest, HistoryEntry, RunId, RunMode, RunStatus, Seq, Shape, Timestamp,
    ToolFailureKind,
};

use crate::ports::journal::{Quarantine, QuarantineKind};
use crate::ports::namespaces::NamespaceName;
use crate::ports::tenancy::{RunRef, TenantId, TenantScope};

/// What the projector knows of a run besides its entries: the run's metadata, from its record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectedRun {
    pub run: RunRef,
    pub namespace: NamespaceName,
    pub script: Digest,
    /// The first line of the script that is not blank, trimmed: searched as text.
    pub headline: String,
    pub toolset: Digest,
    pub labels: BTreeMap<String, String>,
    /// For a fork, the run it was forked from.
    pub parent: Option<RunId>,
    /// For a run of a workflow, the workflow's name.
    pub workflow: Option<String>,
    /// For a run a script started, that run.
    pub started_by: Option<RunId>,
}

/// The run as a whole, folded by the projector from its history up to the last entry it
/// applies (`archi/05-testing-and-observability.md`, "Read model"). Of the snapshots of a run,
/// the read model keeps the one with the highest `history_len`; with equal lengths they are
/// equal, whichever arrives first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSnapshot {
    /// The entries folded.
    pub history_len: u64,
    pub status: RunStatus,
    /// From `run_started`; `None` before it.
    pub mode: Option<RunMode>,
    /// With `active`: what the run waits for, by `since`, then kind, then seq and name; empty
    /// otherwise. A signal's timeout timer is part of the signal's wait, not a wait of its own.
    pub waits: Vec<Wait>,
    /// With `failed`, `cancelled` or `aborted`: how the run ended.
    pub ending: Option<Ending>,
}

/// Something an active run waits for from the outside world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wait {
    pub kind: WaitKind,
    /// The timer's or the call's seq; for a signal, its timeout timer's, if it has one.
    pub seq: Option<Seq>,
    /// The signal's name, or the call's tool; `None` for a timer.
    pub name: Option<String>,
    /// `recorded_at` of the entry that opened the wait (`signal_awaited`, `start_timer`,
    /// `schedule_tool_call`).
    pub since: Timestamp,
    /// When the wait ends by itself: a timer's due time, a signal's timeout.
    pub until: Option<Timestamp>,
    pub callsite: Option<Callsite>,
    /// For a call, the furthest state its task reached ([`ReadModel::record_tasks`]). Filled
    /// by the read model when it returns the wait; ignored by [`ReadModel::apply`].
    pub task: Option<TaskPhase>,
}

impl Wait {
    /// What the run waits for, as queries name it: a call's task waiting for the harness
    /// (approval, parked, deferred) names the wait; otherwise its kind does.
    pub fn waiting_for(&self) -> WaitingFor {
        match (self.kind, self.task) {
            (WaitKind::Call | WaitKind::Host, Some(TaskPhase::Approval)) => WaitingFor::Approval,
            (WaitKind::Call | WaitKind::Host, Some(TaskPhase::Parked)) => WaitingFor::Parked,
            (WaitKind::Call | WaitKind::Host, Some(TaskPhase::Deferred)) => WaitingFor::Deferred,
            (WaitKind::Signal, _) => WaitingFor::Signal,
            (WaitKind::Timer, _) => WaitingFor::Timer,
            (WaitKind::Call, _) => WaitingFor::Call,
            (WaitKind::Host, _) => WaitingFor::Host,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WaitKind {
    /// `await signal(name)` with an empty mailbox.
    Signal,
    /// A durable timer: `sleep`, `setTimeout`, `setInterval`.
    Timer,
    /// A tool call Rewind sends itself (MCP tools, built-ins).
    Call,
    /// A host tool's call, answered by the harness's worker.
    Host,
}

/// What a waiting run waits for, as the query language and the statistics name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WaitingFor {
    Signal,
    Timer,
    Call,
    Host,
    /// A call under the `approval` policy, not approved yet.
    Approval,
    /// A call parked by a schema change, a refused call or a missing secret.
    Parked,
    /// A host tool's call whose handler answers later.
    Deferred,
}

impl WaitingFor {
    pub const ALL: [WaitingFor; 7] = [
        WaitingFor::Signal,
        WaitingFor::Timer,
        WaitingFor::Call,
        WaitingFor::Host,
        WaitingFor::Approval,
        WaitingFor::Parked,
        WaitingFor::Deferred,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            WaitingFor::Signal => "signal",
            WaitingFor::Timer => "timer",
            WaitingFor::Call => "call",
            WaitingFor::Host => "host",
            WaitingFor::Approval => "approval",
            WaitingFor::Parked => "parked",
            WaitingFor::Deferred => "deferred",
        }
    }
}

/// How a run that failed, was cancelled or was aborted ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ending {
    /// The script error's name (`ToolError`, `TypeError`, `CancelledError`…), or the abort's
    /// kind (`limit_exceeded`…).
    pub error: String,
    pub message: String,
    /// The innermost line of `script.ts` in the error's stack or message, if any.
    pub line: Option<u32>,
    /// The error's name and its message with the parts that vary replaced by placeholders: what
    /// the runs of a failure group share, for display.
    pub signature: String,
    /// The failure group: the first 16 hexadecimal digits of the SHA-256 of `signature`.
    pub fingerprint: String,
}

/// The state of a call's task, as whoever changes it records it. Tasks are service state:
/// they are not in the journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TaskPhase {
    /// Waits for the harness's approval (`approval` policy).
    Approval,
    /// Waits to be sent.
    Ready,
    /// An attempt is in flight.
    Claimed,
    /// A host tool's handler answers later.
    Deferred,
    /// Waits for the harness after a schema change, a refused call or a missing secret.
    Parked,
}

/// A call's task at some point of its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TaskProgress {
    pub seq: Seq,
    /// Claims so far: 0 before the first.
    pub attempt: u32,
    pub phase: TaskPhase,
}

impl TaskProgress {
    /// The order a task's states follow, which only moves forward: by attempt, then within an
    /// attempt approval or claim, then parked or deferred, then ready again. Of the records of
    /// a call, the read model keeps the highest.
    pub fn rank(&self) -> (u32, u8) {
        let step = match self.phase {
            TaskPhase::Approval | TaskPhase::Claimed => 0,
            TaskPhase::Deferred | TaskPhase::Parked => 1,
            TaskPhase::Ready => 2,
        };
        (self.attempt, step)
    }
}

/// A run's quarantine, as the read model keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineNote {
    pub kind: QuarantineKind,
    pub reason: String,
    pub since: Timestamp,
}

impl From<&Quarantine> for QuarantineNote {
    fn from(quarantine: &Quarantine) -> Self {
        Self {
            kind: quarantine.kind,
            reason: quarantine.reason.clone(),
            since: quarantine.since,
        }
    }
}

/// One run of the read model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    pub run: RunRef,
    pub namespace: NamespaceName,
    pub script: Digest,
    pub headline: String,
    pub toolset: Digest,
    /// `active` until a terminal entry is projected; never `pending`.
    pub status: RunStatus,
    pub mode: Option<RunMode>,
    pub labels: BTreeMap<String, String>,
    pub parent: Option<RunId>,
    /// For a run of a workflow, the workflow's name.
    pub workflow: Option<String>,
    /// For a run a script started, that run.
    pub started_by: Option<RunId>,
    /// `recorded_at` of `run_started`, once projected.
    pub started_at: Option<Timestamp>,
    /// `recorded_at` of the terminal entry.
    pub ended_at: Option<Timestamp>,
    /// The latest `recorded_at` projected.
    pub last_activity_at: Timestamp,
    /// `schedule_tool_call` entries projected.
    pub tool_calls: u64,
    /// `tool_call_failed` entries projected.
    pub errors: u64,
    /// The highest index projected, plus one.
    pub history_len: u64,
    /// The latest snapshot's waits, in its order, each call's with its task's furthest state.
    pub waits: Vec<Wait>,
    /// The earliest `until` of the waits: when the run wakes by itself.
    pub wakes_at: Option<Timestamp>,
    pub ending: Option<Ending>,
    pub quarantine: Option<QuarantineNote>,
}

impl RunSummary {
    /// The cursor after this run, in `order`.
    pub fn cursor(&self, order: RunOrder) -> SearchCursor {
        SearchCursor {
            key: match order {
                RunOrder::Started => None,
                RunOrder::Activity => Some(self.last_activity_at),
                RunOrder::Wakes => self.wakes_at,
            },
            run: self.run.id,
        }
    }
}

/// Which runs `search_runs`, `run_stats` and `failure_groups` consider: the tenant's runs that
/// match every term (`archi/05-testing-and-observability.md`, "Query language").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunQuery {
    pub tenant: TenantId,
    /// All must hold; none: every run of the tenant.
    pub terms: Vec<Term>,
}

impl RunQuery {
    /// Every run of the tenant.
    pub fn new(tenant: TenantId) -> Self {
        Self {
            tenant,
            terms: Vec::new(),
        }
    }

    /// Adds a term that must hold.
    pub fn with(mut self, predicate: Predicate) -> Self {
        self.terms.push(Term {
            negated: false,
            predicate,
        });
        self
    }

    /// Adds a term that must not hold.
    pub fn without(mut self, predicate: Predicate) -> Self {
        self.terms.push(Term {
            negated: true,
            predicate,
        });
        self
    }
}

/// A predicate, or its negation: a negated term matches exactly the runs its predicate does
/// not match (a run without the value a predicate reads, such as `ended_at` for a run that has
/// not ended, does not match the predicate, so it matches its negation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Term {
    pub negated: bool,
    pub predicate: Predicate,
}

/// A typed predicate on a run. A list inside a predicate means "any of".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Predicate {
    /// The run is in one of these namespaces. The server adds it, not negated, for a token or a
    /// `Rewind-Namespace` header.
    Namespace(BTreeSet<NamespaceName>),
    /// The run's id, hyphenated and lowercase, starts with one of these.
    Id(Vec<String>),
    Status(BTreeSet<RunStatus>),
    /// The run carries the label `key` with a value one of these matches.
    Label {
        key: String,
        values: Vec<TextMatch>,
    },
    /// The script's digest, in lowercase hexadecimal, starts with one of these.
    Script(Vec<String>),
    /// The toolset's digest, in lowercase hexadecimal, starts with one of these.
    Toolset(Vec<String>),
    /// The run scheduled at least one call to a tool one of these matches.
    Tool(Vec<TextMatch>),
    /// The run ended with an error (or abort kind) named in `names`, or at least one of its
    /// calls failed with a kind in `kinds`.
    Error {
        names: BTreeSet<String>,
        kinds: BTreeSet<ToolFailureKind>,
    },
    /// The run waits (status `active`, at least one wait) for one of these; empty: for
    /// anything.
    Waiting(BTreeSet<WaitingFor>),
    /// The run waits for a signal whose name one of these matches.
    Signal(Vec<TextMatch>),
    Started(Range<Timestamp>),
    Ended(Range<Timestamp>),
    /// The latest `recorded_at` projected.
    Activity(Range<Timestamp>),
    /// From start to end, in milliseconds: ended runs only.
    Duration(Range<u64>),
    /// Tool calls scheduled.
    Calls(Range<u64>),
    /// Tool calls failed.
    Errors(Range<u64>),
    /// Forks of this run.
    Parent(RunId),
    /// Runs of a workflow whose name one of these matches.
    Workflow(Vec<TextMatch>),
    /// Runs this run started.
    StartedBy(RunId),
    /// The run's ending has one of these fingerprints.
    Fingerprint(Vec<String>),
    /// The run is quarantined.
    Quarantined,
    Mode(Vec<RunMode>),
    /// Lowercase; found, case-insensitively, in the ending's message or error, the headline, a
    /// label's value or the run's id.
    Text(String),
}

/// How a predicate matches a string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextMatch {
    /// Any value: the label is set, a signal is awaited, a call was scheduled.
    Any,
    Exact(String),
    Prefix(String),
}

impl TextMatch {
    pub fn matches(&self, value: &str) -> bool {
        match self {
            TextMatch::Any => true,
            TextMatch::Exact(exact) => value == exact,
            TextMatch::Prefix(prefix) => value.starts_with(prefix.as_str()),
        }
    }
}

/// A range of values, bounds included or not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range<T> {
    pub start: Bound<T>,
    pub end: Bound<T>,
}

impl<T: PartialOrd> Range<T> {
    pub fn contains(&self, value: &T) -> bool {
        let above = match &self.start {
            Bound::Included(start) => value >= start,
            Bound::Excluded(start) => value > start,
            Bound::Unbounded => true,
        };
        let below = match &self.end {
            Bound::Included(end) => value <= end,
            Bound::Excluded(end) => value < end,
            Bound::Unbounded => true,
        };
        above && below
    }
}

/// The order of `search_runs`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RunOrder {
    /// Newest run id first (run ids are UUID v7: newest created first).
    #[default]
    Started,
    /// Latest activity first, then newest run id first.
    Activity,
    /// Earliest `wakes_at` first, the runs without one last, then oldest run id first.
    Wakes,
}

/// Where a page of `search_runs` starts: after this run, in the page's order. Its `key` is the
/// run's sort key in that order (`None` for `started`, and for `wakes` when the run has no
/// `wakes_at`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SearchCursor {
    pub key: Option<Timestamp>,
    pub run: RunId,
}

/// The window of `run_stats`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatsWindow {
    /// Runs started at or after; `None`: no lower bound.
    pub since: Option<Timestamp>,
    /// Runs started before; `None`: no upper bound.
    pub until: Option<Timestamp>,
    /// The width of the series' buckets, in milliseconds, from `since` (required with it) up to
    /// `until` (required with it); `None`: no series.
    pub bucket_ms: Option<u64>,
    /// Values per facet, and label keys, at most.
    pub top: usize,
}

/// The runs a query matches. Everything counts the runs that match the query and started in
/// the window, except `waiting` and `quarantined`, which describe the runs that match the query
/// now, whenever they started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunStats {
    pub runs: u64,
    /// Every status, zeros included.
    pub statuses: BTreeMap<RunStatus, u64>,
    /// Per bucket from `since`, the runs started in it, by status (every status, zeros
    /// included). Empty without a bucket.
    pub series: Vec<BTreeMap<RunStatus, u64>>,
    /// From start to end, over the runs that ended; `None` when none has.
    pub durations: Option<Latency>,
    /// Waiting runs by what they wait for (every kind, zeros included); a run waiting for
    /// several kinds counts once in each.
    pub waiting: BTreeMap<WaitingFor, u64>,
    pub quarantined: u64,
    pub facets: Facets,
}

/// The most frequent values among the runs a query matches, each with its number of runs,
/// most frequent first, then by value; at most `top` per facet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facets {
    pub namespaces: Vec<FacetValue>,
    pub statuses: Vec<FacetValue>,
    /// The `top` most frequent label keys, each with its most frequent values.
    pub labels: Vec<LabelFacet>,
    pub tools: Vec<FacetValue>,
    /// The endings' errors (names and abort kinds) and the kinds of the calls that failed.
    pub errors: Vec<FacetValue>,
    /// By digest, in lowercase hexadecimal.
    pub scripts: Vec<FacetValue>,
    pub waiting: Vec<FacetValue>,
    /// The signals waited for.
    pub signals: Vec<FacetValue>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetValue {
    pub value: String,
    pub runs: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelFacet {
    pub key: String,
    /// Runs carrying the key.
    pub runs: u64,
    pub values: Vec<FacetValue>,
}

/// The window of `failure_groups`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailureWindow {
    /// Runs ended at or after; `None`: no lower bound.
    pub since: Option<Timestamp>,
    /// Runs ended before; `None`: no upper bound.
    pub until: Option<Timestamp>,
    /// The width of each group's series' buckets, as in [`StatsWindow`].
    pub bucket_ms: Option<u64>,
    /// Groups returned at most.
    pub limit: usize,
}

/// The runs with an ending that a query matches and that ended in the window, by fingerprint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureGroups {
    pub runs: u64,
    /// Groups in all, beyond the limit included.
    pub groups: u64,
    /// The most runs first, then the latest ending first, then by fingerprint.
    pub top: Vec<FailureGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureGroup {
    pub fingerprint: String,
    pub signature: String,
    pub error: String,
    pub runs: u64,
    /// Per bucket from `since`, the runs that ended in it. Empty without a bucket.
    pub series: Vec<u64>,
    /// Distinct scripts.
    pub scripts: u64,
    pub first_ended_at: Timestamp,
    pub last_ended_at: Timestamp,
    /// The line most of its runs ended at, the lowest on a tie; `None` when none has one.
    pub line: Option<u32>,
    /// Its runs' labels, as `key:value`, most frequent first, then by value; at most 5.
    pub labels: Vec<FacetValue>,
    /// The run that ended last (the highest run id on a tie), its status and message.
    pub latest: RunId,
    pub latest_status: RunStatus,
    pub latest_message: String,
}

/// Which scripts `scripts` returns: every distinct script of the tenant, ordered by digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptFilter {
    pub tenant: TenantId,
    /// Only the runs of this namespace count.
    pub namespace: Option<NamespaceName>,
    /// Only the runs started at or after this time count.
    pub since: Option<Timestamp>,
    /// Only this script.
    pub digest: Option<Digest>,
    /// Only the runs carrying all of these labels count.
    pub labels: BTreeMap<String, String>,
    /// Only the scripts whose counted runs called this tool.
    pub tool: Option<String>,
}

impl ScriptFilter {
    /// Every script of the tenant, every run counted.
    pub fn new(tenant: TenantId) -> Self {
        Self {
            tenant,
            namespace: None,
            since: None,
            digest: None,
            labels: BTreeMap::new(),
            tool: None,
        }
    }
}

/// A script, across its runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptSummary {
    pub script: Digest,
    pub runs: u64,
    /// Runs by status.
    pub statuses: BTreeMap<RunStatus, u64>,
    /// The tools its runs called.
    pub tools: BTreeSet<String>,
    /// The labels its runs carried, as `key:value`, with the number of runs carrying each.
    pub labels: BTreeMap<String, u64>,
    pub first_started_at: Option<Timestamp>,
    pub last_started_at: Option<Timestamp>,
}

/// Which calls `tool_stats` aggregates, per tool, ordered by tool name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolStatsFilter {
    pub tenant: TenantId,
    /// Only the calls of this namespace's runs.
    pub namespace: Option<NamespaceName>,
    pub toolset: Option<Digest>,
    /// Calls scheduled at or after.
    pub since: Option<Timestamp>,
    /// Calls scheduled before.
    pub until: Option<Timestamp>,
}

/// The calls of one tool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolStats {
    pub tool: String,
    /// Calls scheduled.
    pub calls: u64,
    pub completed: u64,
    pub failed: u64,
    pub errors_by_kind: BTreeMap<ToolFailureKind, u64>,
    /// Attempts beyond the first, over the calls: what [`ReadModel::record_tasks`] recorded.
    pub retries: u64,
    /// Over the calls with an outcome; `None` when no call has one.
    pub latency: Option<Latency>,
}

/// Latency of calls with an outcome: logical time from `schedule_tool_call` to the completion.
/// Percentiles are nearest-rank: the p-th percentile of n sorted latencies is the one at
/// position `ceil(p × n / 100)`, counting from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Latency {
    pub count: u64,
    /// Rounded down.
    pub mean_ms: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub max_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReadModelError {
    #[error("read model backend error: {0}")]
    Backend(String),
}

/// Projections for questions across runs (`archi/05-testing-and-observability.md`, "Read
/// model"), filled by the projector from committed entries, never sampled.
///
/// The contract, checked by the conformance suite:
///
/// - **Idempotent and order-independent.** `apply` is keyed by `(run, index)`: applying the
///   same entries twice, or the batches of a run in any order, gives the same projections. Of
///   the snapshots of a run, the one with the highest `history_len` is kept.
/// - **Tenants.** Every query is scoped by a tenant.
/// - **Queries.** `search_runs`, `run_stats` and `failure_groups` consider exactly the runs
///   that match every term of the query: each predicate and its negation are checked, the
///   runs without the value a predicate reads included.
/// - **Observed shapes.** The results of `tool_call_completed` entries whose value is held are
///   folded into the shape of their tool ([`Shape::merge`]); a result held only by its digest
///   contributes nothing.
/// - **Service state.** A task's state and a run's quarantine live outside the journal:
///   whoever changes them records them. `record_tasks` keeps, per call, the highest attempt and
///   the state of the highest [`TaskProgress::rank`]; `record_quarantine` keeps the record with
///   the latest `at` (a quarantine over a release at the same `at`). Both in any order, before
///   or after the run's entries are projected.
/// - **Retention.** `purge` and `delete_run` remove everything of a run: summary, labels,
///   waits, calls, task states, quarantine.
pub trait ReadModel: Send + Sync {
    /// Projects committed entries of a run, with the snapshot of its history up to the last of
    /// them.
    fn apply(
        &self,
        run: &ProjectedRun,
        entries: &[HistoryEntry],
        snapshot: &RunSnapshot,
    ) -> impl Future<Output = Result<(), ReadModelError>> + Send;

    /// A page of the runs the query matches, in `order`, after `after`.
    fn search_runs(
        &self,
        query: &RunQuery,
        order: RunOrder,
        after: Option<&SearchCursor>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<RunSummary>, ReadModelError>> + Send;

    /// Counts, series, durations and facets of the runs the query matches.
    fn run_stats(
        &self,
        query: &RunQuery,
        window: &StatsWindow,
    ) -> impl Future<Output = Result<RunStats, ReadModelError>> + Send;

    /// The runs with an ending the query matches, grouped by fingerprint.
    fn failure_groups(
        &self,
        query: &RunQuery,
        window: &FailureWindow,
    ) -> impl Future<Output = Result<FailureGroups, ReadModelError>> + Send;

    fn scripts(
        &self,
        filter: &ScriptFilter,
    ) -> impl Future<Output = Result<Vec<ScriptSummary>, ReadModelError>> + Send;

    fn tool_stats(
        &self,
        filter: &ToolStatsFilter,
    ) -> impl Future<Output = Result<Vec<ToolStats>, ReadModelError>> + Send;

    /// The shapes observed in the results of these tools, for those observed at least once.
    fn observed_shapes(
        &self,
        tenant: &TenantId,
        tools: &[String],
    ) -> impl Future<Output = Result<BTreeMap<String, Shape>, ReadModelError>> + Send;

    /// Records the state of some of a run's tasks, as the store holds them now. Each call keeps
    /// its highest attempt, and its state of the highest rank.
    fn record_tasks(
        &self,
        run: &RunRef,
        tasks: &[TaskProgress],
    ) -> impl Future<Output = Result<(), ReadModelError>> + Send;

    /// The highest attempt recorded for the calls of a run, by seq.
    fn attempts(
        &self,
        run: &RunRef,
    ) -> impl Future<Output = Result<BTreeMap<Seq, u32>, ReadModelError>> + Send;

    /// Records that a run was quarantined (`Some`) or released or given up (`None`) at `at`.
    fn record_quarantine(
        &self,
        run: &RunRef,
        quarantine: Option<&QuarantineNote>,
        at: Timestamp,
    ) -> impl Future<Output = Result<(), ReadModelError>> + Send;

    /// Removes the runs of the tenants in scope that ended before `ended_before`. Returns how
    /// many runs were removed.
    fn purge(
        &self,
        ended_before: Timestamp,
        tenants: &TenantScope,
    ) -> impl Future<Output = Result<usize, ReadModelError>> + Send;

    /// Removes everything of a run. Returns whether the run was projected.
    fn delete_run(&self, run: &RunRef)
    -> impl Future<Output = Result<bool, ReadModelError>> + Send;
}
