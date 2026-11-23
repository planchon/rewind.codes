//! A partition store where the tests place what another job, or another node, does between two
//! reads of a job: the memory partitions, with two interleavings a node meets and the driver,
//! which runs a job to its end before the next one, never produces by itself.
//!
//! - **A seal between two reads.** The batch that seals an attempt is held once it is committed,
//!   and applied at the next read of a history: a job that reads a run's head, then its entries,
//!   sees the seal committed between the two, as when the sealer's batch and a read run at once.
//! - **A read that finds the partition opened elsewhere.** The next reads of a head answer
//!   `Fenced`, as SlateDB does once another node opened the partition.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rewind_core::{Digest, HistoryEntry, PartitionId, RunId, Shape, Timestamp};
use rewind_engine::adapters::{MemoryPartitionLog, MemoryPartitions};
use rewind_engine::ports::partition::Change;
use rewind_engine::ports::{
    Attempt, Batch, Counters, DueTimer, Found, Gauges, InboxItem, IndexedSummary, NamespaceName,
    NodeId, OrganizationState, Page, PartitionError, PartitionLog, PartitionStore, PayloadRecord,
    Recovery, RunHead, RunOrder, RunQuery, RunRef, Scope, TaskRecord, TenantId, Watcher,
    WebhookTarget, Window,
};

#[derive(Debug, Default)]
struct Interleavings {
    /// The next batch that seals an attempt is held.
    hold_seal: bool,
    /// The batch held, applied at the next read of a history.
    held: Option<Batch>,
    /// How many held batches were applied.
    applied: usize,
    /// How many of the next reads of a head answer `Fenced`.
    fence_reads: usize,
    /// How many times a partition was opened.
    opens: usize,
}

/// [`MemoryPartitions`] where the tests interleave a seal, or a fence, with a job's reads.
#[derive(Debug, Clone, Default)]
pub struct Interleaved {
    inner: MemoryPartitions,
    state: Arc<Mutex<Interleavings>>,
}

impl Interleaved {
    pub fn new() -> Self {
        Self::default()
    }

    /// Holds the next batch that seals an attempt until a history is read.
    pub fn hold_next_seal(&self) {
        self.state.lock().unwrap().hold_seal = true;
    }

    /// Whether a batch is held.
    pub fn holding(&self) -> bool {
        self.state.lock().unwrap().held.is_some()
    }

    /// How many held batches were applied.
    pub fn applied(&self) -> usize {
        self.state.lock().unwrap().applied
    }

    /// The next read of a run's head answers that another node opened the partition.
    pub fn fence_next_read(&self) {
        self.fence_next_reads(1);
    }

    /// The next `count` reads of a run's head answer that another node opened the partition.
    pub fn fence_next_reads(&self, count: usize) {
        self.state.lock().unwrap().fence_reads = count;
    }

    /// How many times a partition was opened.
    pub fn opens(&self) -> usize {
        self.state.lock().unwrap().opens
    }
}

/// A log of an [`Interleaved`] store.
#[derive(Debug, Clone)]
pub struct InterleavedLog {
    inner: MemoryPartitionLog,
    state: Arc<Mutex<Interleavings>>,
}

impl PartitionStore for Interleaved {
    type Log = InterleavedLog;

    async fn open(
        &self,
        partition: PartitionId,
        node: &NodeId,
    ) -> Result<InterleavedLog, PartitionError> {
        let inner = self.inner.open(partition, node).await?;
        self.state.lock().unwrap().opens += 1;
        Ok(InterleavedLog {
            inner,
            state: Arc::clone(&self.state),
        })
    }
}

impl PartitionLog for InterleavedLog {
    fn partition(&self) -> PartitionId {
        self.inner.partition()
    }

    async fn recover(&self, horizon: Timestamp) -> Result<Recovery, PartitionError> {
        self.inner.recover(horizon).await
    }

    async fn run(&self, run: &RunRef) -> Result<Option<RunHead>, PartitionError> {
        let fenced = {
            let mut state = self.state.lock().unwrap();
            let fenced = state.fence_reads > 0;
            state.fence_reads = state.fence_reads.saturating_sub(1);
            fenced
        };
        if fenced {
            return Err(PartitionError::Fenced {
                partition: self.partition(),
            });
        }
        self.inner.run(run).await
    }

    async fn runs_in_range(
        &self,
        tenant: &TenantId,
        low: RunId,
        high: RunId,
    ) -> Result<Vec<RunHead>, PartitionError> {
        self.inner.runs_in_range(tenant, low, high).await
    }

    async fn summary(&self, run: &RunRef) -> Result<Option<IndexedSummary>, PartitionError> {
        self.inner.summary(run).await
    }

    async fn history(
        &self,
        run: &RunRef,
        attempt: Attempt,
        from: u64,
    ) -> Result<Vec<HistoryEntry>, PartitionError> {
        let batch = self.state.lock().unwrap().held.take();
        if let Some(batch) = batch {
            self.inner.commit(batch).await?;
            self.state.lock().unwrap().applied += 1;
        }
        self.inner.history(run, attempt, from).await
    }

    async fn inbox(&self, run: &RunRef) -> Result<Vec<InboxItem>, PartitionError> {
        self.inner.inbox(run).await
    }

    async fn tasks(&self, run: &RunRef) -> Result<Vec<TaskRecord>, PartitionError> {
        self.inner.tasks(run).await
    }

    async fn payload(
        &self,
        run: &RunRef,
        digest: Digest,
    ) -> Result<Option<PayloadRecord>, PartitionError> {
        self.inner.payload(run, digest).await
    }

    async fn watchers(&self, run: &RunRef) -> Result<Vec<Watcher>, PartitionError> {
        self.inner.watchers(run).await
    }

    async fn timers(
        &self,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<DueTimer>, PartitionError> {
        self.inner.timers(from, to).await
    }

    async fn started(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        key: &str,
    ) -> Result<Option<rewind_core::RunId>, PartitionError> {
        self.inner.started(tenant, namespace, key).await
    }

    async fn signal_key(&self, run: &RunRef, key: &str) -> Result<bool, PartitionError> {
        self.inner.signal_key(run, key).await
    }

    async fn webhook(&self, token_hash: Digest) -> Result<Option<WebhookTarget>, PartitionError> {
        self.inner.webhook(token_hash).await
    }

    async fn search(
        &self,
        scope: &Scope,
        query: &RunQuery,
        order: RunOrder,
        page: &Page,
    ) -> Result<Found, PartitionError> {
        self.inner.search(scope, query, order, page).await
    }

    async fn counters(&self, scope: &Scope, window: Window) -> Result<Counters, PartitionError> {
        self.inner.counters(scope, window).await
    }

    async fn gauges(&self, scope: &Scope) -> Result<Gauges, PartitionError> {
        self.inner.gauges(scope).await
    }

    async fn shapes(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        tools: &[String],
    ) -> Result<BTreeMap<String, Shape>, PartitionError> {
        self.inner.shapes(tenant, namespace, tools).await
    }

    async fn organization(
        &self,
        tenant: &TenantId,
    ) -> Result<Option<OrganizationState>, PartitionError> {
        self.inner.organization(tenant).await
    }

    async fn commit(&self, batch: Batch) -> Result<(), PartitionError> {
        let seals = batch
            .changes
            .iter()
            .any(|change| matches!(change, Change::Seal { .. }));
        {
            let mut state = self.state.lock().unwrap();
            if seals && state.hold_seal {
                state.hold_seal = false;
                state.held = Some(batch);
                return Ok(());
            }
        }
        self.inner.commit(batch).await
    }
}
