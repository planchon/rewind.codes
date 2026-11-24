//! The `PartitionStore` port: one partition of the run plane, with one writer
//! (`archi/07-code-architecture.md`, "`PartitionStore`"; `archi/06-storage-and-scale.md`, "A
//! partition", "Partitions, owners and nodes").
//!
//! ```text
//!  PartitionStore::open(partition, node) ──► PartitionLog      the only writer: fences the last
//!       recover(horizon) ──► Recovery                           read once, after the open
//!       run, summary, history, inbox, tasks, payload, watchers, timers, started, signal_key,
//!       webhook, search, counters, gauges, shapes, organization                  reads
//!       commit(Batch [Change, …])                               atomic, durable once it returns
//! ```
//!
//! A partition holds, for each run of its namespaces: its head, its history per attempt, its
//! inbox, its signal keys, its call tasks, its timers, its payload records, its watchers, its
//! webhook tokens, its summary in the run index, and its start key; and for the partition: the
//! counters, the outbox, and the states of the organizations homed there. The keys they are
//! stored under belong to the adapter: the engine never composes one.

mod counters;
mod outbox;
mod run;
mod search;
mod timers;

use std::collections::BTreeMap;
use std::future::Future;
use std::num::NonZeroU16;

use rewind_core::{Digest, HistoryEvent, PartitionId, RunId, Seq, Shape, Timestamp};

pub use counters::{
    ActionCounts, CounterDelta, Counters, FailureCounts, Gauges, Hour, LATEST_RUNS, LatestRun,
    RunCounts, Scope, ScriptCounts, Sketch, Window,
};
pub use outbox::{Admission, OrganizationState, OutboxChange, OutboxEntry, OutboxId, Projection};
pub use run::{Attempt, RunHead, TaskChange, TaskRecord, Watcher};
pub use search::{Found, IndexedSummary, Page, SCAN_BUDGET, SearchPosition};
pub use timers::{DueTimer, TimerChange, TimerKind};

use crate::ports::cluster::NodeId;
use crate::ports::namespaces::NamespaceName;
use crate::ports::read_model::{RunOrder, RunQuery};
use crate::ports::records::{InboxItem, PayloadRecord, Quarantine, WebhookTarget};
use crate::ports::tenancy::{RunRef, TenantId};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PartitionError {
    /// Another `open` of the partition succeeded since this log was opened: this log writes
    /// nothing any more, and its owner drops the partition at once.
    #[error("partition {partition} was opened by another writer")]
    Fenced { partition: PartitionId },
    /// A change a single writer may get wrong, refused with the whole batch: see
    /// [`PartitionLog::commit`].
    #[error("conflict: {0}")]
    Conflict(String),
    /// The backend cannot be reached; the call may be tried again.
    #[error("the partition is unavailable: {0}")]
    Unavailable(String),
    /// A record that cannot be read back.
    #[error("corrupt partition record: {0}")]
    Corrupt(String),
    #[error("partition backend error: {0}")]
    Backend(String),
}

/// The partitions of a deployment (`archi/07-code-architecture.md`, "`PartitionStore`").
///
/// The contract, checked by the conformance suite (feature `conformance`, module
/// `conformance::partition`):
///
/// - **Fencing.** Once another `open` of the same partition succeeded, every `commit` of the
///   earlier log fails with [`PartitionError::Fenced`], and none of its changes is visible.
/// - **Durability.** A commit that returned is durable: a later `open`, on any node, reads it.
/// - **Atomicity.** A batch is atomic: either all its changes are read, or none (after a crash,
///   or when the batch is refused).
/// - The other contracts are [`PartitionLog`]'s.
pub trait PartitionStore: Send + Sync {
    type Log: PartitionLog;

    /// Opens a partition for `node`, which becomes its only writer: the log opened before, by
    /// any node, is fenced.
    fn open(
        &self,
        partition: PartitionId,
        node: &NodeId,
    ) -> impl Future<Output = Result<Self::Log, PartitionError>> + Send;
}

/// An open partition: its reads, and its commits while no other `open` fenced it.
///
/// Every read is scoped by the tenant: a run of another tenant is not found. A fenced log may
/// still answer reads, from what it last saw; only its commits are refused.
///
/// The contracts of the reads and of [`commit`](PartitionLog::commit):
///
/// - **Single writer.** The owner is the only writer, so the store checks what a single writer
///   may get wrong and refuses the batch ([`PartitionError::Conflict`]): events appended
///   anywhere but at the attempt's history length; a start key already used in the
///   namespace; a consumed inbox item that does not exist; a change to a run that does not exist
///   or has been erased. Also refused: a run created twice, an inbox item or an outbox entry
///   numbered twice, a task created twice, a webhook token bound to two runs, an activation of
///   an attempt that is not the run's or has ended, the seal of an attempt that has not ended.
/// - **Signal keys.** An `Enqueue` with a signal key already seen for that run, or recorded by a
///   `SignalKey`, changes nothing.
/// - **Ends.** A terminal `Activation` (its head's status is terminal) clears the run's timers
///   of the attempt, call tasks and inbox, and its webhook tokens stop resolving; nothing is
///   enqueued for an ended attempt (an `Enqueue` changes nothing).
/// - **Activations** write, with the events, the payload records of the payloads that left the
///   events, the run's head and summary, its index entries, the counters it moves and its
///   outbox entries.
/// - **Seal and erase.** `Seal` deletes the attempt's history, inbox, call tasks and payload
///   records, and keeps the run's head (sealed), summary, index entries and signal keys: a key
///   is kept for the life of the run, so a late retry of a signal is acknowledged
///   (`archi/06-storage-and-scale.md`, "Idempotency of the API"). `Erase` deletes everything of
///   the run: its records, summary, start key, signal keys, webhook tokens, timers, outbox
///   events, and what it held in the gauges. Neither is refused for a run
///   already sealed or erased.
/// - **Search.** `search` evaluates every term of the query on the summaries of the scope, in
///   the order asked, reads at most `budget` summaries, and returns a cursor when it stopped
///   before the end. Summaries are kept for ended runs until their erasure.
/// - **Counters.** `counters` sums the hourly counters of the scope over the window (whole
///   hours); `gauges` reads the exact gauges.
/// - **Timers** are ordered by due time, then by run and kind; `timers(from, to)` includes
///   `from` and excludes `to`.
/// - **Ranges of runs.** `runs_in_range(tenant, low, high)` reads the heads of the tenant's runs
///   whose id is between `low` and `high`, both included, by id, sealed runs included: the
///   runs a handle's suffix names ([`rewind_core::suffix_ranges`]).
/// - **Recovery.** `recover(horizon)` returns what [`Recovery`] lists.
pub trait PartitionLog: Send + Sync {
    /// The partition this log writes.
    fn partition(&self) -> PartitionId;

    /// What an owner reads once, after the open.
    fn recover(
        &self,
        horizon: Timestamp,
    ) -> impl Future<Output = Result<Recovery, PartitionError>> + Send;

    fn run(
        &self,
        run: &RunRef,
    ) -> impl Future<Output = Result<Option<RunHead>, PartitionError>> + Send;

    /// The heads of the tenant's runs whose id is between `low` and `high`, both included, by
    /// id.
    fn runs_in_range(
        &self,
        tenant: &TenantId,
        low: RunId,
        high: RunId,
    ) -> impl Future<Output = Result<Vec<RunHead>, PartitionError>> + Send;

    /// The run's summary in the index, once an activation wrote one.
    fn summary(
        &self,
        run: &RunRef,
    ) -> impl Future<Output = Result<Option<IndexedSummary>, PartitionError>> + Send;

    /// The events of an attempt from index `from`, in order. Empty once the attempt is sealed.
    fn history(
        &self,
        run: &RunRef,
        attempt: Attempt,
        from: u64,
    ) -> impl Future<Output = Result<Vec<HistoryEvent>, PartitionError>> + Send;

    /// The run's inbox, in the order of the items' ids.
    fn inbox(
        &self,
        run: &RunRef,
    ) -> impl Future<Output = Result<Vec<InboxItem>, PartitionError>> + Send;

    /// The run's call tasks, by seq.
    fn tasks(
        &self,
        run: &RunRef,
    ) -> impl Future<Output = Result<Vec<TaskRecord>, PartitionError>> + Send;

    /// A payload record of the run.
    fn payload(
        &self,
        run: &RunRef,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<PayloadRecord>, PartitionError>> + Send;

    /// The runs that watch the run's end, by run id.
    fn watchers(
        &self,
        run: &RunRef,
    ) -> impl Future<Output = Result<Vec<Watcher>, PartitionError>> + Send;

    /// The timers due from `from` included to `to` excluded, in order.
    fn timers(
        &self,
        from: Timestamp,
        to: Timestamp,
    ) -> impl Future<Output = Result<Vec<DueTimer>, PartitionError>> + Send;

    /// The run a start key started in the namespace.
    fn started(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        key: &str,
    ) -> impl Future<Output = Result<Option<RunId>, PartitionError>> + Send;

    /// Whether the run's inbox took a signal under this key.
    fn signal_key(
        &self,
        run: &RunRef,
        key: &str,
    ) -> impl Future<Output = Result<bool, PartitionError>> + Send;

    /// The run and signal a webhook token is bound to, by the token's SHA-256; `None` once the
    /// run has ended.
    fn webhook(
        &self,
        token_hash: Digest,
    ) -> impl Future<Output = Result<Option<WebhookTarget>, PartitionError>> + Send;

    /// A page of the summaries of the scope that match the query, in `order`. A query of
    /// another tenant than the scope's matches nothing.
    fn search(
        &self,
        scope: &Scope,
        query: &RunQuery,
        order: RunOrder,
        page: &Page,
    ) -> impl Future<Output = Result<Found, PartitionError>> + Send;

    /// The counters of the scope summed over the window's hours.
    fn counters(
        &self,
        scope: &Scope,
        window: Window,
    ) -> impl Future<Output = Result<Counters, PartitionError>> + Send;

    fn gauges(&self, scope: &Scope) -> impl Future<Output = Result<Gauges, PartitionError>> + Send;

    /// The shapes observed in the results of these actions, in a namespace or across the
    /// tenant's, for those observed at least once.
    fn shapes(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        actions: &[String],
    ) -> impl Future<Output = Result<BTreeMap<String, Shape>, PartitionError>> + Send;

    /// The state of an organization homed in the partition.
    fn organization(
        &self,
        tenant: &TenantId,
    ) -> impl Future<Output = Result<Option<OrganizationState>, PartitionError>> + Send;

    /// Applies a batch, its changes in order, atomically and durably; refused whole with
    /// [`PartitionError::Fenced`] once another node opened the partition, or
    /// [`PartitionError::Conflict`] (see [`PartitionLog`]). An empty batch writes nothing.
    fn commit(&self, batch: Batch) -> impl Future<Output = Result<(), PartitionError>> + Send;
}

/// What an owner reads once, after it opened the partition (`archi/06-storage-and-scale.md`,
/// "Partitions, owners and nodes": recovery).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Recovery {
    /// The runs that are runnable: a non-empty inbox, not quarantined, not ended. By run.
    pub runnable: Vec<RunHead>,
    /// The quarantined runs that have not ended, by run.
    pub quarantined: Vec<RunHead>,
    /// The timers due before the horizon, in order.
    pub timers: Vec<DueTimer>,
    /// The call tasks `ready`, `deferred` or waiting for their approval, by run and seq.
    pub tasks: Vec<TaskRecord>,
    /// The attempts that ended and are not sealed, by run.
    pub unsealed: Vec<(RunRef, Attempt)>,
    /// The outbox, by id.
    pub outbox: Vec<OutboxEntry>,
    /// Every namespace with something counted in the partition (a run kept, an hour of
    /// counters), with its gauges, by tenant and name: the owner reads each one's month with
    /// [`PartitionLog::counters`] and reports both to its organization.
    pub counters: Vec<CountedNamespace>,
    /// The states of the organizations homed here.
    pub organizations: BTreeMap<TenantId, OrganizationState>,
}

/// A namespace with something counted in a partition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CountedNamespace {
    pub tenant: TenantId,
    pub namespace: NamespaceName,
    pub gauges: Gauges,
}

/// Changes applied together, in order: of one run or of many.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Batch {
    pub changes: Vec<Change>,
}

impl Batch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, change: impl Into<Change>) {
        self.changes.push(change.into());
    }

    /// The batch with one more change.
    pub fn with(mut self, change: impl Into<Change>) -> Self {
        self.push(change);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn len(&self) -> usize {
        self.changes.len()
    }
}

/// One change of a [`Batch`].
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    CreateRun(Box<CreateRun>),
    /// Puts an item in the run's inbox. With a signal key already seen for the run, or for an
    /// ended attempt, changes nothing. The item's id is unique in the run's inbox.
    Enqueue {
        run: RunRef,
        item: InboxItem,
        signal_key: Option<String>,
    },
    /// Records a signal key as taken by the run, without an item: an `Enqueue` under it changes
    /// nothing from then on. The keys of the signals a run moved by `migrate-runs` took. Changes
    /// nothing for an ended attempt.
    SignalKey {
        run: RunRef,
        key: String,
    },
    Activation(Box<Activation>),
    /// Writes or deletes one call task. Changes nothing for an ended attempt.
    Task {
        run: RunRef,
        seq: Seq,
        change: TaskChange,
    },
    /// Sets or cancels a timer. A timer of a run needs the run; a webhook's retry, its outbox
    /// event.
    Timer(TimerChange),
    /// An activation of the run failed and committed nothing: its failures + 1. The
    /// [`QUARANTINE_AFTER_FAILURES`]-th in a row quarantines the run (kind `failures`, `error`
    /// as its reason, since `at`).
    ///
    /// [`QUARANTINE_AFTER_FAILURES`]: crate::ports::QUARANTINE_AFTER_FAILURES
    Failure {
        run: RunRef,
        error: String,
        at: Timestamp,
    },
    /// An activation of the run may have been running when the previous owner died: its
    /// attempts + 1, in the recovery batch.
    Interrupted {
        run: RunRef,
    },
    /// Sets the run's quarantine, in its head and its summary.
    Quarantine {
        run: RunRef,
        quarantine: Quarantine,
    },
    /// Lifts the run's quarantine, and resets its failures and attempts.
    Release {
        run: RunRef,
    },
    /// Makes `watcher` a watcher of the run. Watching a run twice changes nothing.
    Watch {
        run: RunRef,
        watcher: Watcher,
    },
    /// Seals an ended attempt into the archive: see [`PartitionLog`].
    Seal {
        run: RunRef,
        attempt: Attempt,
    },
    /// Erases the run: see [`PartitionLog`].
    Erase {
        run: RunRef,
    },
    /// Erases what the partition keeps of a namespace beside its runs: its hourly counters, its
    /// observed shapes and its start keys; what each hour's counters consumed
    /// ([`Counters::consumed`]) is added to the tenant's `erased` for that hour
    /// ([`NamespaceName::erased`]). A `Conflict` while a run of the namespace is kept
    /// (`archi/06-storage-and-scale.md`, "Retention and deletion").
    ///
    /// [`Counters::consumed`]: crate::ports::Counters::consumed
    /// [`NamespaceName::erased`]: crate::ports::NamespaceName::erased
    EraseNamespace {
        tenant: TenantId,
        namespace: NamespaceName,
        erased: NamespaceName,
    },
    /// Writes the run's summary in the index as it is now, outside an activation: what its
    /// waits' call tasks became (approved, parked, deferred, in flight again). A change to a run
    /// that does not exist or has been erased is refused, as any other.
    Summary {
        run: RunRef,
        summary: Box<IndexedSummary>,
    },
    Outbox(OutboxChange),
    Counters(Vec<CounterDelta>),
    /// Binds a webhook token, by its SHA-256, to the run's signal `name`.
    WebhookToken {
        run: RunRef,
        name: String,
        hash: Digest,
    },
    /// Writes the state of an organization homed in the partition.
    Organization {
        tenant: TenantId,
        state: OrganizationState,
    },
}

/// A run created: its head, the start key it answers to, and for a fork its copied history.
#[derive(Clone, Debug, PartialEq)]
pub struct CreateRun {
    pub run: RunRef,
    pub head: RunHead,
    /// Unique in the run's namespace for as long as the run is kept.
    pub start_key: Option<String>,
    /// A fork's copied history: the first events of the head's attempt, from index 0.
    pub events: Vec<HistoryEvent>,
    /// The payload records of the copied events.
    pub payloads: Vec<PayloadRecord>,
    /// The run's summary from its creation, `pending` until its first activation: a pending
    /// run is listed and searched as any other.
    pub summary: IndexedSummary,
}

/// Everything one activation of a run commits (`archi/02-history.md`, "The commit invariant"):
/// the events, complete "trigger → response" groups, and what they caused.
#[derive(Clone, Debug, PartialEq)]
pub struct Activation {
    pub run: RunRef,
    /// The run's attempt: the head's.
    pub attempt: Attempt,
    /// Appended at the attempt's history length.
    pub events: Vec<HistoryEvent>,
    /// The ids of the inbox items delivered or dropped.
    pub consumed: Vec<u64>,
    /// The payloads that left the events: digest, size, preview, the value up to 64 KiB.
    pub payloads: Vec<PayloadRecord>,
    /// The call tasks the new calls created.
    pub tasks: Vec<TaskRecord>,
    pub timers: Vec<TimerChange>,
    /// The run's head after the activation, written as it is but for `history_len` and
    /// `sealed`, which the store keeps.
    pub head: RunHead,
    pub summary: IndexedSummary,
    pub counters: Vec<CounterDelta>,
    pub outbox: Vec<OutboxEntry>,
}

impl From<CreateRun> for Change {
    fn from(create: CreateRun) -> Self {
        Change::CreateRun(Box::new(create))
    }
}

impl From<Activation> for Change {
    fn from(activation: Activation) -> Self {
        Change::Activation(Box::new(activation))
    }
}

impl From<TimerChange> for Change {
    fn from(timer: TimerChange) -> Self {
        Change::Timer(timer)
    }
}

impl From<OutboxChange> for Change {
    fn from(outbox: OutboxChange) -> Self {
        Change::Outbox(outbox)
    }
}

/// The partition of a namespace, chosen when the namespace is created and kept in its row:
/// `hash(tenant, name) mod partitions`, the hash being the first 8 bytes of the SHA-256 of
/// `{tenant}/{name}`, big-endian.
pub fn namespace_partition(
    tenant: &TenantId,
    name: &NamespaceName,
    partitions: NonZeroU16,
) -> PartitionId {
    hashed(format!("{tenant}/{name}").as_bytes(), partitions)
}

/// The home partition of a tenant, where its organization actor lives and keeps its state:
/// `hash(tenant) mod partitions`, as [`namespace_partition`] hashes.
pub fn home_partition(tenant: &TenantId, partitions: NonZeroU16) -> PartitionId {
    hashed(tenant.as_str().as_bytes(), partitions)
}

fn hashed(bytes: &[u8], partitions: NonZeroU16) -> PartitionId {
    let digest = Digest::of(bytes);
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest.as_bytes()[..8]);
    let id = u64::from_be_bytes(first) % u64::from(partitions.get());
    PartitionId::new(u16::try_from(id).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_spread_over_the_partitions_and_keep_theirs() {
        let partitions = NonZeroU16::new(64).unwrap();
        let tenant = TenantId::new("acme").unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for n in 0..1_000 {
            let name = NamespaceName::new(format!("user-{n}")).unwrap();
            let partition = namespace_partition(&tenant, &name, partitions);
            assert!(partition.get() < 64);
            assert_eq!(partition, namespace_partition(&tenant, &name, partitions));
            seen.insert(partition);
        }
        assert_eq!(seen.len(), 64, "every partition takes some namespaces");
        let one = NonZeroU16::new(1).unwrap();
        assert_eq!(
            namespace_partition(&tenant, &NamespaceName::new("a").unwrap(), one),
            PartitionId::new(0)
        );
        assert!(home_partition(&tenant, partitions).get() < 64);
        assert_eq!(
            home_partition(&tenant, partitions),
            home_partition(&tenant, partitions)
        );
    }
}
