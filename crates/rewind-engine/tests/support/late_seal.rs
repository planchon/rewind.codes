//! A partition store whose seal lands between two reads of a job: the memory partitions, where
//! the batch that seals an attempt is held once it is committed, and applied at the next read
//! of a history. The driver runs a job to its end before the next one; with this store, a job
//! that reads a run's head, then its entries, sees the seal committed between the two, as a
//! node does when the sealer's batch and a read run at once.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use rewind_core::{Digest, HistoryEntry, PartitionId, Shape, Timestamp};
use rewind_engine::adapters::{MemoryPartitionLog, MemoryPartitions};
use rewind_engine::ports::partition::Change;
use rewind_engine::ports::{
    Attempt, Batch, Counters, DueTimer, Found, Gauges, InboxItem, IndexedSummary, NamespaceName,
    NodeId, OrganizationState, Page, PartitionError, PartitionLog, PartitionStore, PayloadRecord,
    Recovery, RunHead, RunOrder, RunQuery, RunRef, Scope, TaskRecord, TenantId, Watcher,
    WebhookTarget, Window,
};

#[derive(Debug, Default)]
struct Held {
    /// The next batch that seals an attempt is held.
    armed: bool,
    /// The batch held, applied at the next read of a history.
    batch: Option<Batch>,
    /// How many held batches were applied.
    applied: usize,
}

/// [`MemoryPartitions`] whose next seal is held until a history is read.
#[derive(Debug, Clone, Default)]
pub struct LateSeal {
    inner: MemoryPartitions,
    held: Arc<Mutex<Held>>,
}

impl LateSeal {
    pub fn new() -> Self {
        Self::default()
    }

    /// Holds the next batch that seals an attempt until a history is read.
    pub fn hold_next_seal(&self) {
        self.held.lock().unwrap().armed = true;
    }

    /// Whether a batch is held.
    pub fn holding(&self) -> bool {
        self.held.lock().unwrap().batch.is_some()
    }

    /// How many held batches were applied.
    pub fn applied(&self) -> usize {
        self.held.lock().unwrap().applied
    }
}

/// A log of a [`LateSeal`].
#[derive(Debug, Clone)]
pub struct LateSealLog {
    inner: MemoryPartitionLog,
    held: Arc<Mutex<Held>>,
}

impl PartitionStore for LateSeal {
    type Log = LateSealLog;

    async fn open(
        &self,
        partition: PartitionId,
        node: &NodeId,
    ) -> Result<LateSealLog, PartitionError> {
        Ok(LateSealLog {
            inner: self.inner.open(partition, node).await?,
            held: Arc::clone(&self.held),
        })
    }
}

impl PartitionLog for LateSealLog {
    fn partition(&self) -> PartitionId {
        self.inner.partition()
    }

    async fn recover(&self, horizon: Timestamp) -> Result<Recovery, PartitionError> {
        self.inner.recover(horizon).await
    }

    async fn run(&self, run: &RunRef) -> Result<Option<RunHead>, PartitionError> {
        self.inner.run(run).await
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
        let batch = self.held.lock().unwrap().batch.take();
        if let Some(batch) = batch {
            self.inner.commit(batch).await?;
            self.held.lock().unwrap().applied += 1;
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
            let mut held = self.held.lock().unwrap();
            if seals && held.armed {
                held.armed = false;
                held.batch = Some(batch);
                return Ok(());
            }
        }
        self.inner.commit(batch).await
    }
}
