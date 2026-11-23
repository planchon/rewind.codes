//! `PartitionStore` and `PartitionLog` on SlateDB: opening a partition, fencing, the reads.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use object_store::ObjectStore;
use rewind_core::{Digest, HistoryEntry, PartitionId, RunId, Shape, Timestamp};
use rewind_engine::ports::{
    Attempt, Batch, CountedNamespace, Counters, DueTimer, Found, Gauges, InboxItem, IndexedSummary,
    NamespaceName, NodeId, OrganizationState, Page, PartitionError, PartitionLog, PartitionStore,
    PayloadRecord, Recovery, RunHead, RunOrder, RunQuery, RunRef, Scope, TaskRecord, TenantId,
    Watcher, WebhookTarget, Window,
};
use rewind_format::partition as record;
use slatedb::admin::Admin;
use slatedb::config::{DurabilityLevel, ReadOptions, ScanOptions};
use slatedb::db_cache::moka::{MokaCache, MokaCacheOptions};
use slatedb::db_cache::{DbCache, SplitCache};
use slatedb::{CompactorBuilder, Db, IterationOrder};

use crate::codec::{self, decode};
use crate::config::{Bucket, Options};
use crate::{Error, keys};

/// `PartitionStore` on SlateDB (`archi/07-code-architecture.md`, "`PartitionStore`";
/// `archi/06-storage-and-scale.md`, "A partition").
///
/// Each partition is one SlateDB database at `partitions/{id}/` of the bucket. [`open`] opens
/// it as its writer: SlateDB raises the writer epoch in the manifest and writes a fence into the
/// WAL, so the previous writer's next write fails and it learns it is fenced, wherever it runs.
/// Within one store, a log opened before is also fenced at once, before it writes anything. An
/// open claims the writer and compactor epochs after the ones it read when it began: when another
/// open ended in between, it fails as fenced rather than fencing that later writer.
///
/// Records are the partition records of `rewind-format`, under the keys of [`crate::keys`].
/// The clones of a store share its object store, its block cache and its count of opens.
///
/// [`open`]: PartitionStore::open
#[derive(Clone)]
pub struct SlatePartitions {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    object_store: Arc<dyn ObjectStore>,
    root: String,
    options: Options,
    cache: Arc<dyn DbCache>,
    /// How many times each partition was opened by this store: the generation of its current
    /// log.
    opens: Mutex<HashMap<PartitionId, u64>>,
}

impl std::fmt::Debug for SlatePartitions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlatePartitions")
            .field("object_store", &self.inner.object_store.to_string())
            .field("root", &self.inner.root)
            .field("options", &self.inner.options)
            .finish_non_exhaustive()
    }
}

impl SlatePartitions {
    /// A store of the partitions in `bucket`; nothing is read until a partition is opened.
    pub fn new(bucket: &Bucket, options: Options) -> Result<Self, Error> {
        let (object_store, root) = bucket.open()?;
        Ok(Self::on(object_store, root, options))
    }

    /// A store on an object store built by the caller, the partitions under `root`.
    pub fn on(object_store: Arc<dyn ObjectStore>, root: String, options: Options) -> Self {
        let block: Arc<dyn DbCache> = Arc::new(MokaCache::new_with_opts(MokaCacheOptions {
            max_capacity: options.block_cache_bytes,
            time_to_live: None,
            time_to_idle: None,
        }));
        let meta: Arc<dyn DbCache> = Arc::new(MokaCache::new_with_opts(MokaCacheOptions {
            max_capacity: options.meta_cache_bytes,
            time_to_live: None,
            time_to_idle: None,
        }));
        let cache = SplitCache::new()
            .with_block_cache(Some(block))
            .with_meta_cache(Some(meta))
            .build();
        Self {
            inner: Arc::new(StoreInner {
                object_store,
                root,
                options,
                cache: Arc::new(cache),
                opens: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The object store the partitions live in.
    pub fn object_store(&self) -> Arc<dyn ObjectStore> {
        Arc::clone(&self.inner.object_store)
    }

    /// The path of a partition's database in the object store.
    pub fn path(&self, partition: PartitionId) -> String {
        format!("{}partitions/{}", self.inner.root, partition.get())
    }
}

impl StoreInner {
    fn generation(&self, partition: PartitionId) -> u64 {
        self.opens
            .lock()
            .map(|opens| opens.get(&partition).copied().unwrap_or(0))
            .unwrap_or(u64::MAX)
    }
}

impl PartitionStore for SlatePartitions {
    type Log = SlateLog;

    async fn open(
        &self,
        partition: PartitionId,
        node: &NodeId,
    ) -> Result<SlateLog, PartitionError> {
        let path = self.path(partition);
        // The open claims the writer epoch, and its compactor the compactor epoch, after the ones
        // it reads first: an open that began before another one ended fails, fenced, instead of
        // fencing that later writer or its compactor. A node whose open was under way when the
        // partition moved away never takes it back.
        let (writer, compactor) =
            Admin::builder(path.clone(), Arc::clone(&self.inner.object_store))
                .build()
                .read_manifest(None)
                .await
                .map_err(|err| crate::error::partition(partition, err))?
                .map_or((0, 0), |manifest| {
                    (manifest.writer_epoch(), manifest.compactor_epoch())
                });
        let mut settings = self.inner.options.settings();
        let compaction = settings.compactor_options.take();
        let mut builder = Db::builder(path.clone(), Arc::clone(&self.inner.object_store))
            .with_settings(settings)
            .with_db_cache(Arc::clone(&self.inner.cache), u64::from(partition.get()))
            .with_expected_epoch(writer);
        if let Some(options) = compaction {
            builder = builder.with_compactor_builder(
                CompactorBuilder::new(path, Arc::clone(&self.inner.object_store))
                    .with_options(options)
                    .with_expected_epoch(compactor),
            );
        }
        let db = builder
            .build()
            .await
            .map_err(|err| crate::error::opening(partition, err))?;
        let generation = {
            let mut opens = self
                .inner
                .opens
                .lock()
                .map_err(|_| PartitionError::Backend("the store's lock is poisoned".into()))?;
            let opened = opens.entry(partition).or_default();
            *opened += 1;
            *opened
        };
        tracing::debug!(partition = partition.get(), node = %node, "partition opened");
        Ok(SlateLog {
            inner: Arc::new(LogInner {
                partition,
                node: node.clone(),
                generation,
                store: Arc::clone(&self.inner),
                db,
                commits: tokio::sync::Mutex::new(()),
                fenced: AtomicBool::new(false),
                failed: AtomicBool::new(false),
            }),
        })
    }
}

/// An open partition of a [`SlatePartitions`]: its reads, and its commits while no other open
/// fenced it. Its clones share the database; the last one dropped closes it.
///
/// Reads see what is durable: a batch is read once its commit returned. A commit applies its
/// changes in one SlateDB transaction, which reads what the batch wrote before, checks what a
/// single writer may get wrong, and writes nothing when a change is refused; the transaction is
/// then written as one write batch and the WAL flushed: the commit returns once the batch is
/// durable. Commits of one log run one at a time. A commit whose flush fails leaves the log
/// unable to commit: its owner opens the partition again.
#[derive(Clone)]
pub struct SlateLog {
    pub(crate) inner: Arc<LogInner>,
}

pub(crate) struct LogInner {
    pub(crate) partition: PartitionId,
    node: NodeId,
    generation: u64,
    store: Arc<StoreInner>,
    pub(crate) db: Db,
    pub(crate) commits: tokio::sync::Mutex<()>,
    /// A commit was refused as fenced.
    pub(crate) fenced: AtomicBool,
    /// A batch reached the database but its flush failed: it may or may not be durable.
    pub(crate) failed: AtomicBool,
}

impl std::fmt::Debug for SlateLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlateLog")
            .field("partition", &self.inner.partition)
            .field("node", &self.inner.node)
            .field("generation", &self.inner.generation)
            .finish_non_exhaustive()
    }
}

impl Drop for LogInner {
    fn drop(&mut self) {
        let db = self.db.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(err) = db.close().await {
                    tracing::debug!(error = %err, "closing a partition");
                }
            });
        }
    }
}

impl SlateLog {
    /// The node that opened it.
    pub fn node(&self) -> &NodeId {
        &self.inner.node
    }

    /// Closes the database: its background tasks stop, and what is not durable is lost. Every
    /// clone of the log stops working.
    pub async fn close(&self) -> Result<(), PartitionError> {
        self.inner.db.close().await.map_err(|err| self.error(err))
    }

    /// The SlateDB database, for measurements.
    pub fn db(&self) -> &Db {
        &self.inner.db
    }

    /// Whether another open fenced this log, as far as it knows: a later open by the same store,
    /// a commit refused as fenced, or SlateDB's own finding.
    pub(crate) fn fenced(&self) -> bool {
        self.inner.fenced.load(Ordering::SeqCst)
            || self.inner.store.generation(self.inner.partition) != self.inner.generation
            || matches!(
                self.inner.db.status().close_reason,
                Some(slatedb::CloseReason::Fenced)
            )
    }

    /// A SlateDB error of this log; a fenced one also fences the log.
    pub(crate) fn error(&self, err: slatedb::Error) -> PartitionError {
        let error = crate::error::partition(self.inner.partition, err);
        if matches!(error, PartitionError::Fenced { .. }) {
            self.inner.fenced.store(true, Ordering::SeqCst);
        }
        error
    }

    pub(crate) fn fence(&self) -> PartitionError {
        self.inner.fenced.store(true, Ordering::SeqCst);
        PartitionError::Fenced {
            partition: self.inner.partition,
        }
    }

    async fn get<R: record::Record>(&self, key: keys::Key) -> Result<Option<R>, PartitionError> {
        let bytes = self
            .inner
            .db
            .get_with_options(key.as_bytes(), &read_options())
            .await
            .map_err(|err| self.error(err))?;
        bytes.map(|bytes| decode(&bytes)).transpose()
    }

    /// The entries under `prefix`, in `order`, after `after` (a suffix of `prefix`).
    async fn scan(
        &self,
        prefix: &keys::Key,
        after: Bound<Vec<u8>>,
        order: IterationOrder,
    ) -> Result<slatedb::DbIterator, PartitionError> {
        let range = match order {
            IterationOrder::Ascending => (after, Bound::Unbounded),
            IterationOrder::Descending => (Bound::Unbounded, after),
        };
        let options = ScanOptions {
            order,
            ..scan_options()
        };
        self.inner
            .db
            .scan_prefix_with_options(prefix.as_bytes(), range, &options)
            .await
            .map_err(|err| self.error(err))
    }

    /// Every record under `prefix`, in key order.
    async fn records<R: record::Record>(
        &self,
        prefix: &keys::Key,
    ) -> Result<Vec<(Vec<u8>, R)>, PartitionError> {
        let mut iterator = self
            .scan(prefix, Bound::Unbounded, IterationOrder::Ascending)
            .await?;
        let mut records = Vec::new();
        while let Some(entry) = iterator.next().await.map_err(|err| self.error(err))? {
            records.push((entry.key.to_vec(), decode(&entry.value)?));
        }
        Ok(records)
    }

    /// Every key under `prefix`, in order.
    async fn keys(&self, prefix: &keys::Key) -> Result<Vec<Vec<u8>>, PartitionError> {
        let mut iterator = self
            .scan(prefix, Bound::Unbounded, IterationOrder::Ascending)
            .await?;
        let mut found = Vec::new();
        while let Some(entry) = iterator.next().await.map_err(|err| self.error(err))? {
            found.push(entry.key.to_vec());
        }
        Ok(found)
    }

    /// The head of the run, when it is the tenant's.
    async fn head(&self, run: &RunRef) -> Result<Option<RunHead>, PartitionError> {
        match self.get::<record::RunHead>(keys::head(run.id)).await? {
            Some(head) if head.tenant == run.tenant.as_str() => codec::head_of(head).map(Some),
            _ => Ok(None),
        }
    }
}

/// Reads see only what is durable.
fn read_options() -> ReadOptions {
    ReadOptions {
        durability_filter: DurabilityLevel::Remote,
        ..ReadOptions::default()
    }
}

/// Scans see only what is durable, keep the blocks they read in the cache (a partition's scans
/// read runs, read again soon), and read ahead [`READ_AHEAD`] at a time: object storage charges
/// by request, and a run's history in a cold table is a few requests rather than one per block.
pub(crate) fn scan_options() -> ScanOptions {
    ScanOptions {
        durability_filter: DurabilityLevel::Remote,
        cache_blocks: true,
        read_ahead_bytes: READ_AHEAD,
        max_fetch_tasks: 2,
        ..ScanOptions::default()
    }
}

/// What a scan reads from a table at a time, at least.
const READ_AHEAD: usize = 256 * 1024;

impl PartitionLog for SlateLog {
    fn partition(&self) -> PartitionId {
        self.inner.partition
    }

    async fn recover(&self, horizon: Timestamp) -> Result<Recovery, PartitionError> {
        let live: Vec<RunHead> = self
            .records::<record::RunHead>(&keys::Key::new(keys::LIVE))
            .await?
            .into_iter()
            .map(|(_, head)| codec::head_of(head))
            .collect::<Result<_, _>>()?;
        let with_inbox: BTreeSet<RunId> = self
            .keys(&keys::Key::new(keys::INBOX))
            .await?
            .iter()
            .filter_map(|key| {
                let (_, rest) = keys::read_str(key.get(1..)?)?;
                keys::run_from(rest)
            })
            .collect();
        let runnable = live
            .iter()
            .filter(|head| head.quarantine.is_none() && with_inbox.contains(&head.run.id))
            .cloned()
            .collect();
        let quarantined = live
            .iter()
            .filter(|head| head.quarantine.is_some())
            .cloned()
            .collect();
        let timers = self.timers(Timestamp(0), horizon).await?;
        let mut tasks: Vec<TaskRecord> = self
            .records::<record::Task>(&keys::Key::new(keys::TASK))
            .await?
            .into_iter()
            .map(|(_, task)| codec::task_of(task))
            .filter(|task| task.as_ref().map_or(true, TaskRecord::is_pending))
            .collect::<Result<_, _>>()?;
        tasks.sort_by(|a, b| {
            (a.task.task.run.id, a.task.task.seq).cmp(&(b.task.task.run.id, b.task.task.seq))
        });
        let unsealed = self
            .records::<record::RunHead>(&keys::Key::new(keys::UNSEALED))
            .await?
            .into_iter()
            .map(|(_, head)| {
                let head = codec::head_of(head)?;
                Ok((head.run, head.attempt))
            })
            .collect::<Result<_, PartitionError>>()?;
        let outbox = self
            .records::<record::Outbox>(&keys::Key::new(keys::OUTBOX))
            .await?
            .into_iter()
            .map(|(_, entry)| codec::outbox_of(entry))
            .collect::<Result<_, _>>()?;
        let mut counted: BTreeMap<(TenantId, NamespaceName), Gauges> = BTreeMap::new();
        for (key, gauges) in self
            .records::<record::Gauges>(&keys::Key::new(keys::GAUGES))
            .await?
        {
            counted.insert(namespace_of(&key)?, codec::gauges_of(gauges));
        }
        for key in self.keys(&keys::Key::new(keys::COUNTED)).await? {
            counted.entry(namespace_of(&key)?).or_default();
        }
        let counters = counted
            .into_iter()
            .map(|((tenant, namespace), gauges)| CountedNamespace {
                tenant,
                namespace,
                gauges,
            })
            .collect();
        let mut organizations = BTreeMap::new();
        for (key, state) in self
            .records::<record::Organization>(&keys::Key::new(keys::ORGANIZATION))
            .await?
        {
            let (tenant, _) = key
                .get(1..)
                .and_then(keys::read_str)
                .ok_or_else(|| codec::corrupt("an organization's key"))?;
            organizations.insert(codec::tenant(tenant)?, codec::organization_of(state)?);
        }
        Ok(Recovery {
            runnable,
            quarantined,
            timers,
            tasks,
            unsealed,
            outbox,
            counters,
            organizations,
        })
    }

    async fn run(&self, run: &RunRef) -> Result<Option<RunHead>, PartitionError> {
        self.head(run).await
    }

    async fn runs_in_range(
        &self,
        tenant: &TenantId,
        low: RunId,
        high: RunId,
    ) -> Result<Vec<RunHead>, PartitionError> {
        if high < low {
            return Ok(Vec::new());
        }
        let range = (
            Bound::Included(keys::head(low).into_bytes()),
            Bound::Included(keys::head(high).into_bytes()),
        );
        let mut iterator = self
            .inner
            .db
            .scan_with_options(range, &scan_options())
            .await
            .map_err(|err| self.error(err))?;
        let mut heads = Vec::new();
        while let Some(entry) = iterator.next().await.map_err(|err| self.error(err))? {
            let head: record::RunHead = decode(&entry.value)?;
            if head.tenant == tenant.as_str() {
                heads.push(codec::head_of(head)?);
            }
        }
        Ok(heads)
    }

    async fn summary(&self, run: &RunRef) -> Result<Option<IndexedSummary>, PartitionError> {
        self.get::<record::Summary>(keys::summary(run))
            .await?
            .map(codec::summary_of)
            .transpose()
    }

    async fn history(
        &self,
        run: &RunRef,
        attempt: Attempt,
        from: u64,
    ) -> Result<Vec<HistoryEntry>, PartitionError> {
        let prefix = keys::history_of(run, attempt);
        let start = if from == 0 {
            Bound::Unbounded
        } else {
            Bound::Included(from.to_be_bytes().to_vec())
        };
        let mut iterator = self
            .inner
            .db
            .scan_prefix_with_options(
                prefix.as_bytes(),
                (start, Bound::Unbounded),
                &scan_options(),
            )
            .await
            .map_err(|err| self.error(err))?;
        let mut entries = Vec::new();
        while let Some(entry) = iterator.next().await.map_err(|err| self.error(err))? {
            entries.push(codec::entry_of(decode(&entry.value)?));
        }
        Ok(entries)
    }

    async fn inbox(&self, run: &RunRef) -> Result<Vec<InboxItem>, PartitionError> {
        self.records::<record::InboxItem>(&keys::of_run(keys::INBOX, run))
            .await?
            .into_iter()
            .map(|(_, item)| codec::inbox_of(item))
            .collect()
    }

    async fn tasks(&self, run: &RunRef) -> Result<Vec<TaskRecord>, PartitionError> {
        self.records::<record::Task>(&keys::of_run(keys::TASK, run))
            .await?
            .into_iter()
            .map(|(_, task)| codec::task_of(task))
            .collect()
    }

    async fn payload(
        &self,
        run: &RunRef,
        digest: Digest,
    ) -> Result<Option<PayloadRecord>, PartitionError> {
        Ok(self
            .get::<record::PayloadValue>(keys::payload(run, digest))
            .await?
            .map(codec::payload_of))
    }

    async fn watchers(&self, run: &RunRef) -> Result<Vec<Watcher>, PartitionError> {
        Ok(self
            .records::<record::Watcher>(&keys::of_run(keys::WATCHER, run))
            .await?
            .into_iter()
            .map(|(_, watcher)| codec::watcher_of(watcher))
            .collect())
    }

    async fn timers(
        &self,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<DueTimer>, PartitionError> {
        if to <= from {
            return Ok(Vec::new());
        }
        let range = (
            Bound::Included(keys::due_from(from.0).into_bytes()),
            Bound::Excluded(keys::due_from(to.0).into_bytes()),
        );
        let mut iterator = self
            .inner
            .db
            .scan_with_options(range, &scan_options())
            .await
            .map_err(|err| self.error(err))?;
        let mut timers = Vec::new();
        while let Some(entry) = iterator.next().await.map_err(|err| self.error(err))? {
            timers.push(codec::timer_of(decode(&entry.value)?)?);
        }
        Ok(timers)
    }

    async fn started(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        key: &str,
    ) -> Result<Option<RunId>, PartitionError> {
        Ok(self
            .get::<record::StartKey>(keys::start_key(tenant, namespace, key))
            .await?
            .map(|start| start.run))
    }

    async fn signal_key(&self, run: &RunRef, key: &str) -> Result<bool, PartitionError> {
        Ok(self
            .get::<record::SignalKey>(keys::signal_key(run, key))
            .await?
            .is_some())
    }

    async fn webhook(&self, token_hash: Digest) -> Result<Option<WebhookTarget>, PartitionError> {
        self.get::<record::WebhookToken>(keys::webhook(token_hash))
            .await?
            .map(codec::webhook_of)
            .transpose()
    }

    async fn search(
        &self,
        scope: &Scope,
        query: &RunQuery,
        order: RunOrder,
        page: &Page,
    ) -> Result<Found, PartitionError> {
        if query.tenant != *scope.tenant() {
            return Ok(Found::default());
        }
        let prefix = keys::index_family(keys::index_of(scope.tenant(), scope.namespace()), order);
        let after = match page.cursor {
            Some(cursor) => Bound::Excluded(keys::position(order, cursor.key, cursor.run)),
            None => Bound::Unbounded,
        };
        let direction = match order {
            RunOrder::Started => IterationOrder::Descending,
            _ => IterationOrder::Ascending,
        };
        let mut iterator = self.scan(&prefix, after, direction).await?;
        let mut found = Vec::new();
        let mut read = 0;
        let mut last = page.cursor;
        while found.len() < page.limit && read < page.budget {
            let Some(entry) = iterator.next().await.map_err(|err| self.error(err))? else {
                return Ok(Found {
                    summaries: found,
                    cursor: None,
                });
            };
            read += 1;
            let indexed = codec::summary_of(decode(&entry.value)?)?;
            last = Some(indexed.summary.cursor(order));
            if indexed.matches(query) {
                found.push(indexed.summary);
            }
        }
        let more = iterator
            .next()
            .await
            .map_err(|err| self.error(err))?
            .is_some();
        Ok(Found {
            summaries: found,
            cursor: if more { last } else { None },
        })
    }

    async fn counters(&self, scope: &Scope, window: Window) -> Result<Counters, PartitionError> {
        let mut total = Counters::default();
        if window.is_empty() {
            return Ok(total);
        }
        let tenant = scope.tenant();
        let (prefix, range) = match scope.namespace() {
            Some(namespace) => (
                keys::hours_of(tenant, namespace),
                (
                    Bound::Included(window.from.0.to_be_bytes().to_vec()),
                    Bound::Excluded(window.to.0.to_be_bytes().to_vec()),
                ),
            ),
            None => (
                keys::tenant_hours_of(tenant),
                (
                    Bound::Included(window.from.0.to_be_bytes().to_vec()),
                    Bound::Excluded(window.to.0.to_be_bytes().to_vec()),
                ),
            ),
        };
        let mut iterator = self
            .inner
            .db
            .scan_prefix_with_options(prefix.as_bytes(), range, &scan_options())
            .await
            .map_err(|err| self.error(err))?;
        while let Some(entry) = iterator.next().await.map_err(|err| self.error(err))? {
            let counts = codec::counters_of(decode(&entry.value)?);
            let namespace = match scope.namespace() {
                Some(namespace) => namespace.clone(),
                None => {
                    // `D t hour n`: the namespace follows the hour.
                    let rest = entry
                        .key
                        .get(prefix.as_bytes().len() + 4..)
                        .ok_or_else(|| codec::corrupt("a tenant's hour key"))?;
                    let (name, _) = keys::read_str(rest)
                        .ok_or_else(|| codec::corrupt("a tenant's hour key"))?;
                    codec::namespace(name)?
                }
            };
            total.add_for(&namespace, &counts);
        }
        Ok(total)
    }

    async fn gauges(&self, scope: &Scope) -> Result<Gauges, PartitionError> {
        match scope.namespace() {
            Some(namespace) => Ok(self
                .get::<record::Gauges>(keys::gauges(scope.tenant(), namespace))
                .await?
                .map(codec::gauges_of)
                .unwrap_or_default()),
            None => {
                let mut total = Gauges::default();
                for (_, gauges) in self
                    .records::<record::Gauges>(&keys::gauges_of(scope.tenant()))
                    .await?
                {
                    total.add(&codec::gauges_of(gauges));
                }
                Ok(total)
            }
        }
    }

    async fn shapes(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        tools: &[String],
    ) -> Result<BTreeMap<String, Shape>, PartitionError> {
        let prefix = keys::shapes_of(tenant, namespace);
        let mut shapes: BTreeMap<String, Shape> = BTreeMap::new();
        for (key, shape) in self.records::<record::Shape>(&prefix).await? {
            let rest = &key[prefix.as_bytes().len()..];
            let rest = match namespace {
                Some(_) => rest,
                None => {
                    keys::read_str(rest)
                        .ok_or_else(|| codec::corrupt("a shape's key"))?
                        .1
                }
            };
            let (tool, _) = keys::read_str(rest).ok_or_else(|| codec::corrupt("a shape's key"))?;
            if !tools.contains(&tool) {
                continue;
            }
            let merged = match shapes.remove(&tool) {
                Some(known) => known.merge(shape.shape),
                None => shape.shape,
            };
            shapes.insert(tool, merged);
        }
        Ok(shapes)
    }

    async fn organization(
        &self,
        tenant: &TenantId,
    ) -> Result<Option<OrganizationState>, PartitionError> {
        self.get::<record::Organization>(keys::organization(tenant))
            .await?
            .map(codec::organization_of)
            .transpose()
    }

    async fn commit(&self, batch: Batch) -> Result<(), PartitionError> {
        crate::commit::commit(self, batch).await
    }
}

/// The tenant and namespace of a `G t n` or `N t n` key.
fn namespace_of(key: &[u8]) -> Result<(TenantId, NamespaceName), PartitionError> {
    let corrupt = || codec::corrupt("a namespace's key");
    let (tenant, rest) = keys::read_str(key.get(1..).ok_or_else(corrupt)?).ok_or_else(corrupt)?;
    let (namespace, _) = keys::read_str(rest).ok_or_else(corrupt)?;
    Ok((codec::tenant(tenant)?, codec::namespace(namespace)?))
}
