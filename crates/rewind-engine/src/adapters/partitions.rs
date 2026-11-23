use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use rewind_core::{Digest, HistoryEntry, PartitionId, RunId, Seq, Shape, Timestamp};

use crate::ports::partition::{Activation, CreateRun};
use crate::ports::{
    Attempt, Batch, Change, CountedNamespace, CounterDelta, Counters, DueTimer, Found, Gauges,
    Hour, InboxItem, IndexedSummary, NamespaceName, NodeId, OrganizationState, OutboxChange,
    OutboxEntry, OutboxId, Page, PartitionError, PartitionLog, PartitionStore, PayloadRecord,
    QUARANTINE_AFTER_FAILURES, Quarantine, QuarantineKind, QuarantineNote, Recovery, RunHead,
    RunOrder, RunQuery, RunRef, Scope, SearchPosition, TaskChange, TaskRecord, TenantId,
    TimerChange, TimerKind, Watcher, WebhookTarget, Window,
};

/// `PartitionStore` in memory: one map of records per partition, shared by every log opened on
/// it and by the clones of the store, so a partition opened again, by another node, reads what
/// the previous owner committed.
///
/// Fencing counts the opens: each `open` of a partition makes the log it returns the current
/// one, and a commit of an earlier log fails with [`PartitionError::Fenced`]. A commit applies
/// its batch to a copy of the partition and keeps the copy only if every change applied: a
/// batch is atomic. It passes the conformance suite: it is the reference implementation of the
/// contract.
#[derive(Debug, Clone, Default)]
pub struct MemoryPartitions {
    partitions: Arc<Mutex<BTreeMap<PartitionId, Slot>>>,
}

#[derive(Debug, Default)]
struct Slot {
    /// How many times the partition was opened: the generation of its current log.
    opens: u64,
    state: PartitionState,
}

/// A log of a [`MemoryPartitions`].
#[derive(Debug, Clone)]
pub struct MemoryPartitionLog {
    partition: PartitionId,
    node: NodeId,
    /// The open that returned this log: it commits while no later open succeeded.
    generation: u64,
    partitions: Arc<Mutex<BTreeMap<PartitionId, Slot>>>,
}

#[derive(Debug, Clone, Default)]
struct PartitionState {
    /// Run ids are unique across tenants.
    runs: BTreeMap<RunId, RunRecords>,
    start_keys: BTreeMap<(TenantId, NamespaceName, String), RunId>,
    /// Token hash → (run, signal name).
    webhooks: BTreeMap<Digest, (RunId, String)>,
    /// Each timer by its kind, with its due time.
    timers: BTreeMap<TimerKind, Timestamp>,
    outbox: BTreeMap<OutboxId, OutboxEntry>,
    /// The counters of each namespace and hour, as the deltas added them.
    hours: BTreeMap<(TenantId, NamespaceName, Hour), Counters>,
    shapes: BTreeMap<(TenantId, NamespaceName, String), Shape>,
    organizations: BTreeMap<TenantId, OrganizationState>,
}

#[derive(Debug, Clone)]
struct RunRecords {
    head: RunHead,
    summary: Option<IndexedSummary>,
    history: BTreeMap<Attempt, Vec<HistoryEntry>>,
    inbox: BTreeMap<u64, InboxItem>,
    signal_keys: BTreeSet<String>,
    tasks: BTreeMap<Seq, TaskRecord>,
    payloads: BTreeMap<Digest, PayloadRecord>,
    watchers: BTreeMap<RunId, Watcher>,
    start_key: Option<String>,
}

impl MemoryPartitions {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, BTreeMap<PartitionId, Slot>>, PartitionError> {
        lock(&self.partitions)
    }
}

fn lock(
    partitions: &Mutex<BTreeMap<PartitionId, Slot>>,
) -> Result<MutexGuard<'_, BTreeMap<PartitionId, Slot>>, PartitionError> {
    partitions
        .lock()
        .map_err(|_| PartitionError::Backend("the memory partitions' lock is poisoned".into()))
}

fn conflict(message: impl Into<String>) -> PartitionError {
    PartitionError::Conflict(message.into())
}

impl PartitionStore for MemoryPartitions {
    type Log = MemoryPartitionLog;

    async fn open(
        &self,
        partition: PartitionId,
        node: &NodeId,
    ) -> Result<MemoryPartitionLog, PartitionError> {
        let mut partitions = self.lock()?;
        let slot = partitions.entry(partition).or_default();
        slot.opens += 1;
        Ok(MemoryPartitionLog {
            partition,
            node: node.clone(),
            generation: slot.opens,
            partitions: Arc::clone(&self.partitions),
        })
    }
}

impl MemoryPartitionLog {
    /// The node that opened it.
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// Reads the partition as it is now.
    fn read<T>(&self, read: impl FnOnce(&PartitionState) -> T) -> Result<T, PartitionError> {
        let partitions = lock(&self.partitions)?;
        let empty = PartitionState::default();
        let state = partitions
            .get(&self.partition)
            .map_or(&empty, |slot| &slot.state);
        Ok(read(state))
    }
}

impl PartitionState {
    fn records(&self, run: &RunRef) -> Option<&RunRecords> {
        self.runs
            .get(&run.id)
            .filter(|records| records.head.run.tenant == run.tenant)
    }

    fn records_mut(&mut self, run: &RunRef) -> Result<&mut RunRecords, PartitionError> {
        self.runs
            .get_mut(&run.id)
            .filter(|records| records.head.run.tenant == run.tenant)
            .ok_or_else(|| conflict(format!("run {run} does not exist or has been erased")))
    }

    fn apply(&mut self, change: Change) -> Result<(), PartitionError> {
        match change {
            Change::CreateRun(create) => self.create_run(*create),
            Change::Enqueue {
                run,
                item,
                signal_key,
            } => {
                let records = self.records_mut(&run)?;
                if records.head.is_ended()
                    || signal_key
                        .as_ref()
                        .is_some_and(|key| records.signal_keys.contains(key))
                {
                    return Ok(());
                }
                if records.inbox.contains_key(&item.id) {
                    return Err(conflict(format!(
                        "the inbox of run {run} already holds an item {}",
                        item.id
                    )));
                }
                records.inbox.insert(item.id, item);
                records.signal_keys.extend(signal_key);
                Ok(())
            }
            Change::Activation(activation) => self.activate(*activation),
            Change::Task { run, seq, change } => {
                let records = self.records_mut(&run)?;
                if records.head.is_ended() {
                    return Ok(());
                }
                match change {
                    TaskChange::Put(record) => {
                        if record.task.task.run != run || record.task.task.seq != seq {
                            return Err(conflict(format!(
                                "the task {} is written as {}/{}",
                                record.task.task, run.id, seq.0
                            )));
                        }
                        records.tasks.insert(seq, *record);
                    }
                    TaskChange::Delete => {
                        records.tasks.remove(&seq);
                    }
                }
                Ok(())
            }
            Change::Timer(timer) => self.timer(timer),
            Change::Failure { run, error, at } => {
                let records = self.records_mut(&run)?;
                records.head.failures = records.head.failures.saturating_add(1);
                if records.head.failures >= QUARANTINE_AFTER_FAILURES
                    && records.head.quarantine.is_none()
                {
                    let quarantine = Quarantine {
                        kind: QuarantineKind::Failures,
                        reason: error,
                        since: at,
                        retried_by: None,
                    };
                    records.quarantine(Some(quarantine));
                }
                Ok(())
            }
            Change::Interrupted { run } => {
                let head = &mut self.records_mut(&run)?.head;
                head.attempts = head.attempts.saturating_add(1);
                Ok(())
            }
            Change::Quarantine { run, quarantine } => {
                self.records_mut(&run)?.quarantine(Some(quarantine));
                Ok(())
            }
            Change::Release { run } => {
                let records = self.records_mut(&run)?;
                records.quarantine(None);
                records.head.failures = 0;
                records.head.attempts = 0;
                Ok(())
            }
            Change::Watch { run, watcher } => {
                self.records_mut(&run)?
                    .watchers
                    .entry(watcher.run)
                    .or_insert(watcher);
                Ok(())
            }
            Change::Seal { run, attempt } => self.seal(&run, attempt),
            Change::Erase { run } => {
                self.erase(&run);
                Ok(())
            }
            Change::EraseNamespace { tenant, namespace } => {
                if self.runs.values().any(|records| {
                    records.head.run.tenant == tenant && records.head.namespace == namespace
                }) {
                    return Err(conflict(format!(
                        "the namespace {namespace} still holds runs in this partition"
                    )));
                }
                self.hours
                    .retain(|(of, name, _), _| !(*of == tenant && *name == namespace));
                self.shapes
                    .retain(|(of, name, _), _| !(*of == tenant && *name == namespace));
                self.start_keys
                    .retain(|(of, name, _), _| !(*of == tenant && *name == namespace));
                Ok(())
            }
            Change::Summary { run, summary } => {
                if summary.summary.run != run {
                    return Err(conflict(format!(
                        "the summary of run {run} names another run"
                    )));
                }
                self.records_mut(&run)?.summary = Some(*summary);
                Ok(())
            }
            Change::Outbox(OutboxChange::Add(entry)) => self.add_outbox(*entry),
            Change::Outbox(OutboxChange::Remove(id)) => {
                if let Some(entry) = self.outbox.remove(&id) {
                    self.timers.remove(&TimerKind::WebhookRetry {
                        tenant: entry.tenant,
                        entry: id,
                    });
                }
                Ok(())
            }
            Change::Counters(deltas) => {
                for delta in deltas {
                    self.count(delta);
                }
                Ok(())
            }
            Change::WebhookToken { run, name, hash } => {
                let records = self.records_mut(&run)?;
                if records.head.is_ended() {
                    return Ok(());
                }
                match self.webhooks.get(&hash) {
                    Some((bound, bound_name)) if *bound != run.id || *bound_name != name => {
                        Err(conflict(format!(
                            "a webhook token of run {run} is bound to another run or signal"
                        )))
                    }
                    _ => {
                        self.webhooks.insert(hash, (run.id, name));
                        Ok(())
                    }
                }
            }
            Change::Organization { tenant, state } => {
                self.organizations.insert(tenant, state);
                Ok(())
            }
        }
    }

    fn create_run(&mut self, create: CreateRun) -> Result<(), PartitionError> {
        let CreateRun {
            run,
            mut head,
            start_key,
            entries,
            payloads,
            summary,
        } = create;
        if head.run != run || summary.summary.run != run {
            return Err(conflict(format!(
                "the head or the summary of the creation of run {run} names another run"
            )));
        }
        if self.runs.contains_key(&run.id) {
            return Err(conflict(format!("run {} already exists", run.id)));
        }
        if let Some(key) = &start_key {
            let slot = (run.tenant.clone(), head.namespace.clone(), key.clone());
            if self.start_keys.contains_key(&slot) {
                return Err(conflict(format!(
                    "the start key {key:?} is already used in the namespace {}",
                    head.namespace
                )));
            }
            self.start_keys.insert(slot, run.id);
        }
        continue_history(&run, head.attempt, 0, &entries)?;
        head.history_len = entries.len() as u64;
        head.sealed = None;
        let attempt = head.attempt;
        self.runs.insert(
            run.id,
            RunRecords {
                head,
                summary: Some(summary),
                history: BTreeMap::from([(attempt, entries)]),
                inbox: BTreeMap::new(),
                signal_keys: BTreeSet::new(),
                tasks: BTreeMap::new(),
                payloads: payloads
                    .into_iter()
                    .map(|payload| (payload.digest, payload))
                    .collect(),
                watchers: BTreeMap::new(),
                start_key,
            },
        );
        Ok(())
    }

    fn activate(&mut self, activation: Activation) -> Result<(), PartitionError> {
        let Activation {
            run,
            attempt,
            entries,
            consumed,
            payloads,
            tasks,
            timers,
            mut head,
            summary,
            counters,
            outbox,
        } = activation;
        if head.run != run || summary.summary.run != run {
            return Err(conflict(format!(
                "the head or the summary of an activation of run {run} names another run"
            )));
        }
        let records = self.records_mut(&run)?;
        if records.head.is_ended() {
            return Err(conflict(format!("the attempt of run {run} has ended")));
        }
        if attempt != records.head.attempt || head.attempt != attempt {
            return Err(conflict(format!(
                "run {run} is at attempt {}, not {attempt}",
                records.head.attempt
            )));
        }
        let history = records.history.entry(attempt).or_default();
        continue_history(&run, attempt, history.len() as u64, &entries)?;
        history.extend(entries);
        head.history_len = history.len() as u64;
        head.sealed = records.head.sealed;
        for id in &consumed {
            if records.inbox.remove(id).is_none() {
                return Err(conflict(format!(
                    "the inbox of run {run} has no item {id} to consume"
                )));
            }
        }
        for payload in payloads {
            records.payloads.insert(payload.digest, payload);
        }
        for task in tasks {
            let seq = task.task.task.seq;
            if task.task.task.run != run {
                return Err(conflict(format!(
                    "an activation of run {run} creates a task of run {}",
                    task.task.task.run
                )));
            }
            if records.tasks.contains_key(&seq) {
                return Err(conflict(format!("run {run} already has a task {}", seq.0)));
            }
            records.tasks.insert(seq, task);
        }
        let ended = head.is_ended();
        records.head = head;
        records.summary = Some(summary);
        for timer in timers {
            self.timer(timer)?;
        }
        for delta in counters {
            self.count(delta);
        }
        for entry in outbox {
            self.add_outbox(entry)?;
        }
        if ended {
            self.end(&run);
        }
        Ok(())
    }

    /// Clears what the ended attempt of a run had pending: its timers, tasks, inbox and
    /// webhook tokens.
    fn end(&mut self, run: &RunRef) {
        self.timers
            .retain(|kind, _| !(kind.run() == Some(run) && kind.ends_with_the_attempt()));
        self.webhooks.retain(|_, (bound, _)| *bound != run.id);
        if let Some(records) = self.runs.get_mut(&run.id) {
            records.tasks.clear();
            records.inbox.clear();
        }
    }

    fn timer(&mut self, change: TimerChange) -> Result<(), PartitionError> {
        match change {
            TimerChange::Set(DueTimer { at, kind }) => {
                match &kind {
                    TimerKind::WebhookRetry { tenant, entry } => {
                        if !self
                            .outbox
                            .get(entry)
                            .is_some_and(|known| known.tenant == *tenant)
                        {
                            return Err(conflict(format!(
                                "the outbox has no entry {entry} of tenant {tenant} to retry"
                            )));
                        }
                    }
                    other => {
                        if let Some(run) = other.run() {
                            self.records_mut(run)?;
                        }
                    }
                }
                self.timers.insert(kind, at);
            }
            TimerChange::Cancel(kind) => {
                self.timers.remove(&kind);
            }
        }
        Ok(())
    }

    fn seal(&mut self, run: &RunRef, attempt: Attempt) -> Result<(), PartitionError> {
        let Some(records) = self
            .runs
            .get_mut(&run.id)
            .filter(|records| records.head.run.tenant == run.tenant)
        else {
            return Ok(());
        };
        if records.head.sealed.is_some_and(|sealed| sealed >= attempt) {
            return Ok(());
        }
        if attempt != records.head.attempt || !records.head.is_ended() {
            return Err(conflict(format!(
                "attempt {attempt} of run {run} has not ended"
            )));
        }
        records.history.remove(&attempt);
        records.inbox.clear();
        records.tasks.clear();
        records.payloads.clear();
        records.signal_keys.clear();
        records.head.sealed = Some(attempt);
        Ok(())
    }

    fn erase(&mut self, run: &RunRef) {
        let Some(records) = self.records(run) else {
            return;
        };
        if let Some(key) = &records.start_key {
            self.start_keys.remove(&(
                run.tenant.clone(),
                records.head.namespace.clone(),
                key.clone(),
            ));
        }
        self.runs.remove(&run.id);
        self.webhooks.retain(|_, (bound, _)| *bound != run.id);
        let entries: Vec<OutboxId> = self
            .outbox
            .values()
            .filter(|entry| entry.tenant == run.tenant && entry.run == Some(run.id))
            .map(|entry| entry.id)
            .collect();
        for id in entries {
            self.outbox.remove(&id);
            self.timers.remove(&TimerKind::WebhookRetry {
                tenant: run.tenant.clone(),
                entry: id,
            });
        }
        self.timers.retain(|kind, _| kind.run() != Some(run));
    }

    fn add_outbox(&mut self, entry: OutboxEntry) -> Result<(), PartitionError> {
        if self.outbox.contains_key(&entry.id) {
            return Err(conflict(format!(
                "the outbox already holds an entry {}",
                entry.id
            )));
        }
        self.outbox.insert(entry.id, entry);
        Ok(())
    }

    fn count(&mut self, delta: CounterDelta) {
        match delta {
            CounterDelta::Hour {
                tenant,
                namespace,
                hour,
                counts,
            } => self
                .hours
                .entry((tenant, namespace, hour))
                .or_default()
                .add(&counts),
            CounterDelta::Shape {
                tenant,
                namespace,
                tool,
                shape,
            } => {
                let slot = self.shapes.entry((tenant, namespace, tool));
                match slot {
                    std::collections::btree_map::Entry::Occupied(mut known) => {
                        let merged = known.get().clone().merge(shape);
                        known.insert(merged);
                    }
                    std::collections::btree_map::Entry::Vacant(vacant) => {
                        vacant.insert(shape);
                    }
                }
            }
        }
    }

    fn gauges(&self, scope: &Scope) -> Gauges {
        let mut gauges = Gauges::default();
        for records in self.runs.values() {
            if scope.contains(&records.head.run.tenant, &records.head.namespace) {
                gauges.count(&records.head, records.summary.as_ref());
            }
        }
        gauges
    }

    fn recover(&self, horizon: Timestamp) -> Recovery {
        let heads = || self.runs.values().map(|records| &records.head);
        let runnable = self
            .runs
            .values()
            .filter(|records| {
                !records.head.is_ended()
                    && records.head.quarantine.is_none()
                    && !records.inbox.is_empty()
            })
            .map(|records| records.head.clone())
            .collect();
        let quarantined = heads()
            .filter(|head| !head.is_ended() && head.quarantine.is_some())
            .cloned()
            .collect();
        let mut timers: Vec<DueTimer> = self
            .timers
            .iter()
            .filter(|(_, at)| **at < horizon)
            .map(|(kind, at)| DueTimer {
                at: *at,
                kind: kind.clone(),
            })
            .collect();
        timers.sort();
        let tasks = self
            .runs
            .values()
            .flat_map(|records| records.tasks.values())
            .filter(|task| task.is_pending())
            .cloned()
            .collect();
        let unsealed = heads()
            .filter(|head| head.is_unsealed())
            .map(|head| (head.run.clone(), head.attempt))
            .collect();
        let mut namespaces: BTreeSet<(TenantId, NamespaceName)> = heads()
            .map(|head| (head.run.tenant.clone(), head.namespace.clone()))
            .collect();
        namespaces.extend(
            self.hours
                .keys()
                .map(|(tenant, namespace, _)| (tenant.clone(), namespace.clone())),
        );
        let counters = namespaces
            .into_iter()
            .map(|(tenant, namespace)| {
                let gauges = self.gauges(&Scope::Namespace {
                    tenant: tenant.clone(),
                    namespace: namespace.clone(),
                });
                CountedNamespace {
                    tenant,
                    namespace,
                    gauges,
                }
            })
            .collect();
        Recovery {
            runnable,
            quarantined,
            timers,
            tasks,
            unsealed,
            outbox: self.outbox.values().cloned().collect(),
            counters,
            organizations: self.organizations.clone(),
        }
    }

    fn search(&self, scope: &Scope, query: &RunQuery, order: RunOrder, page: &Page) -> Found {
        let mut summaries: Vec<(SearchPosition, &IndexedSummary)> = self
            .runs
            .values()
            .filter(|records| scope.contains(&records.head.run.tenant, &records.head.namespace))
            .filter_map(|records| records.summary.as_ref())
            .map(|indexed| (SearchPosition::of_summary(order, &indexed.summary), indexed))
            .collect();
        summaries.sort_by(|(a, _), (b, _)| a.cmp(b));
        let after = page
            .cursor
            .as_ref()
            .map(|cursor| SearchPosition::of(order, cursor));
        let mut remaining = summaries
            .into_iter()
            .filter(|(position, _)| after.as_ref().is_none_or(|after| position > after))
            .peekable();
        let mut found = Vec::new();
        let mut read = 0;
        let mut last = page.cursor;
        while found.len() < page.limit && read < page.budget {
            let Some((_, indexed)) = remaining.next() else {
                return Found {
                    summaries: found,
                    cursor: None,
                };
            };
            read += 1;
            last = Some(indexed.summary.cursor(order));
            if indexed.matches(query) {
                found.push(indexed.summary.clone());
            }
        }
        Found {
            summaries: found,
            cursor: remaining.peek().is_some().then_some(last).flatten(),
        }
    }

    fn counters(&self, scope: &Scope, window: Window) -> Counters {
        let mut total = Counters::default();
        for ((tenant, namespace, hour), counts) in &self.hours {
            if scope.contains(tenant, namespace) && window.contains(*hour) {
                total.add_for(namespace, counts);
            }
        }
        total
    }
}

impl RunRecords {
    /// Sets or lifts the quarantine, in the head and in the summary.
    fn quarantine(&mut self, quarantine: Option<Quarantine>) {
        if let Some(summary) = &mut self.summary {
            summary.summary.quarantine = quarantine.as_ref().map(QuarantineNote::from);
        }
        self.head.quarantine = quarantine;
    }
}

/// Refuses entries that do not continue the attempt's history at `len`, index by index.
fn continue_history(
    run: &RunRef,
    attempt: Attempt,
    len: u64,
    entries: &[HistoryEntry],
) -> Result<(), PartitionError> {
    for (offset, entry) in entries.iter().enumerate() {
        let expected = len + offset as u64;
        if entry.index != expected {
            return Err(conflict(format!(
                "attempt {attempt} of run {run} continues at entry {expected}, not {}",
                entry.index
            )));
        }
    }
    Ok(())
}

impl PartitionLog for MemoryPartitionLog {
    fn partition(&self) -> PartitionId {
        self.partition
    }

    async fn recover(&self, horizon: Timestamp) -> Result<Recovery, PartitionError> {
        self.read(|state| state.recover(horizon))
    }

    async fn run(&self, run: &RunRef) -> Result<Option<RunHead>, PartitionError> {
        self.read(|state| state.records(run).map(|records| records.head.clone()))
    }

    async fn summary(&self, run: &RunRef) -> Result<Option<IndexedSummary>, PartitionError> {
        self.read(|state| {
            state
                .records(run)
                .and_then(|records| records.summary.clone())
        })
    }

    async fn history(
        &self,
        run: &RunRef,
        attempt: Attempt,
        from: u64,
    ) -> Result<Vec<HistoryEntry>, PartitionError> {
        self.read(|state| {
            state
                .records(run)
                .and_then(|records| records.history.get(&attempt))
                .map(|history| {
                    history
                        .iter()
                        .filter(|entry| entry.index >= from)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    async fn inbox(&self, run: &RunRef) -> Result<Vec<InboxItem>, PartitionError> {
        self.read(|state| {
            state
                .records(run)
                .map(|records| records.inbox.values().cloned().collect())
                .unwrap_or_default()
        })
    }

    async fn tasks(&self, run: &RunRef) -> Result<Vec<TaskRecord>, PartitionError> {
        self.read(|state| {
            state
                .records(run)
                .map(|records| records.tasks.values().cloned().collect())
                .unwrap_or_default()
        })
    }

    async fn payload(
        &self,
        run: &RunRef,
        digest: Digest,
    ) -> Result<Option<PayloadRecord>, PartitionError> {
        self.read(|state| {
            state
                .records(run)
                .and_then(|records| records.payloads.get(&digest).cloned())
        })
    }

    async fn watchers(&self, run: &RunRef) -> Result<Vec<Watcher>, PartitionError> {
        self.read(|state| {
            state
                .records(run)
                .map(|records| records.watchers.values().copied().collect())
                .unwrap_or_default()
        })
    }

    async fn timers(
        &self,
        from: Timestamp,
        to: Timestamp,
    ) -> Result<Vec<DueTimer>, PartitionError> {
        self.read(|state| {
            let mut timers: Vec<DueTimer> = state
                .timers
                .iter()
                .filter(|(_, at)| from <= **at && **at < to)
                .map(|(kind, at)| DueTimer {
                    at: *at,
                    kind: kind.clone(),
                })
                .collect();
            timers.sort();
            timers
        })
    }

    async fn started(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        key: &str,
    ) -> Result<Option<RunId>, PartitionError> {
        self.read(|state| {
            state
                .start_keys
                .get(&(tenant.clone(), namespace.clone(), key.to_owned()))
                .copied()
        })
    }

    async fn signal_key(&self, run: &RunRef, key: &str) -> Result<bool, PartitionError> {
        self.read(|state| {
            state
                .records(run)
                .is_some_and(|records| records.signal_keys.contains(key))
        })
    }

    async fn webhook(&self, token_hash: Digest) -> Result<Option<WebhookTarget>, PartitionError> {
        self.read(|state| {
            let (run, name) = state.webhooks.get(&token_hash)?;
            let records = state.runs.get(run)?;
            (!records.head.is_ended()).then(|| WebhookTarget {
                run: records.head.run.clone(),
                name: name.clone(),
            })
        })
    }

    async fn search(
        &self,
        scope: &Scope,
        query: &RunQuery,
        order: RunOrder,
        page: &Page,
    ) -> Result<Found, PartitionError> {
        self.read(|state| state.search(scope, query, order, page))
    }

    async fn counters(&self, scope: &Scope, window: Window) -> Result<Counters, PartitionError> {
        self.read(|state| state.counters(scope, window))
    }

    async fn gauges(&self, scope: &Scope) -> Result<Gauges, PartitionError> {
        self.read(|state| state.gauges(scope))
    }

    async fn shapes(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        tools: &[String],
    ) -> Result<BTreeMap<String, Shape>, PartitionError> {
        self.read(|state| {
            let mut shapes: BTreeMap<String, Shape> = BTreeMap::new();
            for ((of, name, tool), shape) in &state.shapes {
                if of != tenant
                    || namespace.is_some_and(|only| only != name)
                    || !tools.contains(tool)
                {
                    continue;
                }
                let merged = match shapes.remove(tool) {
                    Some(known) => known.merge(shape.clone()),
                    None => shape.clone(),
                };
                shapes.insert(tool.clone(), merged);
            }
            shapes
        })
    }

    async fn organization(
        &self,
        tenant: &TenantId,
    ) -> Result<Option<OrganizationState>, PartitionError> {
        self.read(|state| state.organizations.get(tenant).cloned())
    }

    async fn commit(&self, batch: Batch) -> Result<(), PartitionError> {
        let mut partitions = lock(&self.partitions)?;
        let slot = partitions.entry(self.partition).or_default();
        if slot.opens != self.generation {
            return Err(PartitionError::Fenced {
                partition: self.partition,
            });
        }
        if batch.is_empty() {
            return Ok(());
        }
        let mut state = slot.state.clone();
        for change in batch.changes {
            state.apply(change)?;
        }
        slot.state = state;
        Ok(())
    }
}
