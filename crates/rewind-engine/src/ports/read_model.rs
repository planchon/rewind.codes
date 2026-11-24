//! The read model of the run index (`archi/05-testing-and-observability.md`, "Read model",
//! "Query language"): one summary per run, live or ended, that the run's owner writes in its
//! partition in the batch that commits what changed the run, an activation's entries among
//! them, so a summary is as recent as the partition's last durable batch; and the searches
//! that read the summaries of the partitions of a scope, in an order, by pages.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use rewind_core::{ActionFailureKind, Callsite, Digest, RunId, RunMode, RunStatus, Seq, Timestamp};

use crate::ports::namespaces::NamespaceName;
use crate::ports::records::{Quarantine, QuarantineKind};
use crate::ports::tenancy::{RunRef, TenantId};

/// The run as a whole, folded from its history ([`run_snapshot`](crate::projector::run_snapshot)):
/// its status and mode, what it waits for, how it ended.
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
    /// The signal's name, or the call's action; `None` for a timer.
    pub name: Option<String>,
    /// `recorded_at` of the entry that opened the wait (`signal_awaited`, `start_timer`,
    /// `schedule_call`).
    pub since: Timestamp,
    /// When the wait ends by itself: a timer's due time, a signal's timeout.
    pub until: Option<Timestamp>,
    pub callsite: Option<Callsite>,
    /// For a call, the phase of its task, as the owner's call tasks have it when the summary
    /// is written.
    pub task: Option<TaskPhase>,
}

impl Wait {
    /// What the run waits for, as queries name it: a call's task waiting for the harness
    /// (approval, parked, deferred) names the wait; otherwise its kind does.
    pub fn waiting_for(&self) -> WaitingFor {
        match (self.kind, self.task) {
            (WaitKind::Call | WaitKind::Harness, Some(TaskPhase::Approval)) => WaitingFor::Approval,
            (WaitKind::Call | WaitKind::Harness, Some(TaskPhase::Parked)) => WaitingFor::Parked,
            (WaitKind::Call | WaitKind::Harness, Some(TaskPhase::Deferred)) => WaitingFor::Deferred,
            (WaitKind::Signal, _) => WaitingFor::Signal,
            (WaitKind::Timer, _) => WaitingFor::Timer,
            (WaitKind::Call, _) => WaitingFor::Call,
            (WaitKind::Harness, _) => WaitingFor::Harness,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WaitKind {
    /// `await signal(name)` with an empty mailbox.
    Signal,
    /// A durable timer: `sleep`, `setTimeout`, `setInterval`.
    Timer,
    /// A call Rewind sends itself (MCP actions, built-ins).
    Call,
    /// A harness action's call, answered by an SDK worker through its stream.
    Harness,
}

/// What a waiting run waits for, as the query language and the statistics name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WaitingFor {
    Signal,
    Timer,
    Call,
    Harness,
    /// A call under the `approval` policy, not approved yet.
    Approval,
    /// A call parked by a schema change, a refused call or a missing secret.
    Parked,
    /// A harness action's call whose handler answers later.
    Deferred,
}

impl WaitingFor {
    pub const ALL: [WaitingFor; 7] = [
        WaitingFor::Signal,
        WaitingFor::Timer,
        WaitingFor::Call,
        WaitingFor::Harness,
        WaitingFor::Approval,
        WaitingFor::Parked,
        WaitingFor::Deferred,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            WaitingFor::Signal => "signal",
            WaitingFor::Timer => "timer",
            WaitingFor::Call => "call",
            WaitingFor::Harness => "harness",
            WaitingFor::Approval => "approval",
            WaitingFor::Parked => "parked",
            WaitingFor::Deferred => "deferred",
        }
    }
}

/// How a run that failed, was cancelled or was aborted ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ending {
    /// The code error's name (`ActionError`, `TypeError`, `CancelledError`…), or the abort's
    /// kind (`limit_exceeded`…).
    pub error: String,
    pub message: String,
    /// The innermost line of `workflow.ts` in the error's stack or message, if any.
    pub line: Option<u32>,
    /// The error's name and its message with the parts that vary replaced by placeholders: what
    /// the runs of a failure group share, for display.
    pub signature: String,
    /// The failure group: the first 16 hexadecimal digits of the SHA-256 of `signature`.
    pub fingerprint: String,
}

/// The state of a call's task, as whoever changes it records it. Tasks are service state:
/// they are not in the history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TaskPhase {
    /// Waits for the harness's approval (`approval` policy).
    Approval,
    /// Waits to be sent.
    Ready,
    /// An attempt is in flight.
    Claimed,
    /// A harness action's handler answers later.
    Deferred,
    /// Waits for the harness after a schema change, a refused call or a missing secret.
    Parked,
}

/// A run's quarantine, as its summary shows it.
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

/// A run as the run index holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSummary {
    pub run: RunRef,
    pub namespace: NamespaceName,
    /// What ran, and the characters of its handle's suffix, as its record has them.
    pub origin: String,
    pub suffix_len: u8,
    pub code: Digest,
    pub headline: String,
    pub action_set: Digest,
    /// `pending` until its first activation commits, then `active` until a terminal entry.
    pub status: RunStatus,
    pub mode: Option<RunMode>,
    pub labels: BTreeMap<String, String>,
    pub parent: Option<RunId>,
    /// For a run of a workflow, the workflow's name.
    pub workflow: Option<String>,
    /// For a run a code started, that run.
    pub started_by: Option<RunId>,
    /// `recorded_at` of `run_started`, once committed.
    pub started_at: Option<Timestamp>,
    /// `recorded_at` of the terminal entry.
    pub ended_at: Option<Timestamp>,
    /// `recorded_at` of the last entry; the run's creation before its first.
    pub last_activity_at: Timestamp,
    /// The `schedule_call` entries of its history.
    pub calls: u64,
    /// The `call_failed` entries of its history.
    pub errors: u64,
    /// The entries of its history.
    pub history_len: u64,
    /// The latest snapshot's waits, in its order, each call's with its task's furthest state.
    pub waits: Vec<Wait>,
    /// The earliest `until` of the waits: when the run wakes by itself.
    pub wakes_at: Option<Timestamp>,
    pub ending: Option<Ending>,
    pub quarantine: Option<QuarantineNote>,
}

impl RunSummary {
    /// The run's handle, `<namespace>/<origin>/<suffix>`.
    pub fn handle(&self) -> rewind_core::RunHandle {
        rewind_core::RunHandle::new(
            self.namespace.as_str(),
            &self.origin,
            self.run.id,
            usize::from(self.suffix_len),
        )
    }

    /// The cursor after this run, in `order`.
    pub fn cursor(&self, order: RunOrder) -> SearchCursor {
        SearchCursor {
            key: match order {
                RunOrder::Started | RunOrder::Oldest => None,
                RunOrder::Activity => Some(self.last_activity_at.0),
                RunOrder::Wakes => self.wakes_at.map(|at| at.0),
                RunOrder::Duration => self.duration(),
                RunOrder::Calls => Some(self.calls),
            },
            run: self.run.id,
        }
    }

    /// How long the run took, in milliseconds, once it has started and ended: `None` before.
    /// An end recorded before the start counts as 0.
    pub fn duration(&self) -> Option<u64> {
        let (started, ended) = (self.started_at?, self.ended_at?);
        Some(ended.0.saturating_sub(started.0))
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
    /// The code's digest, in lowercase hexadecimal, starts with one of these.
    Script(Vec<String>),
    /// The action set's digest, in lowercase hexadecimal, starts with one of these.
    ActionSet(Vec<String>),
    /// The run scheduled at least one call to an action one of these matches.
    Action(Vec<TextMatch>),
    /// The run ended with an error (or abort kind) named in `names`, or at least one of its
    /// calls failed with a kind in `kinds`.
    Error {
        names: BTreeSet<String>,
        kinds: BTreeSet<ActionFailureKind>,
    },
    /// The run waits (status `active`, at least one wait) for one of these; empty: for
    /// anything.
    Waiting(BTreeSet<WaitingFor>),
    /// The run waits for a signal whose name one of these matches.
    Signal(Vec<TextMatch>),
    Started(Range<Timestamp>),
    Ended(Range<Timestamp>),
    /// `recorded_at` of the run's last entry.
    Activity(Range<Timestamp>),
    /// From start to end, in milliseconds: ended runs only.
    Duration(Range<u64>),
    /// Calls scheduled.
    Calls(Range<u64>),
    /// Calls failed.
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
    /// Oldest run id first.
    Oldest,
    /// Latest activity first, then newest run id first.
    Activity,
    /// Earliest `wakes_at` first, the runs without one last, then oldest run id first.
    Wakes,
    /// Longest [`RunSummary::duration`] first, then newest run id first; the runs that have not
    /// ended last, newest run id first.
    Duration,
    /// Most `calls` first, then newest run id first.
    Calls,
}

impl RunOrder {
    pub const ALL: [RunOrder; 6] = [
        RunOrder::Started,
        RunOrder::Oldest,
        RunOrder::Activity,
        RunOrder::Wakes,
        RunOrder::Duration,
        RunOrder::Calls,
    ];
}

/// Where a page of `search_runs` starts: after this run, in the page's order. Its `key` is the
/// run's sort key in that order: `None` for `started` and `oldest`; milliseconds since the epoch
/// for `activity` and `wakes` (`None` without a `wakes_at`); milliseconds for `duration`
/// (`None` for a run that has not ended); a count for `calls`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SearchCursor {
    pub key: Option<u64>,
    pub run: RunId,
}

/// Latency of calls with an outcome: logical time from `schedule_call` to the completion.
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
