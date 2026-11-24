//! A batch applied in one SlateDB transaction (`archi/02-history.md`, "The commit invariant";
//! `archi/07-code-architecture.md`, "`PartitionStore`").
//!
//! The changes apply in order, each reading what the ones before it wrote. A change the single
//! writer may get wrong refuses the batch, and the transaction is dropped: nothing was written.
//! Otherwise the transaction is written as one write batch and the WAL flushed: the commit returns
//! once the batch is durable. The changes keep, with the records the port names, the indexes of
//! the [key layout](crate::keys): the run's index entries, the heads of the runs not ended and of
//! the attempts not sealed, the timers by kind, the back references of a run's signal URL tokens,
//! outbox entries and start key, and the gauges of each namespace.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, PoisonError};

use rewind_core::HistoryEvent;
use rewind_engine::ports::partition::{Activation, CreateRun};
use rewind_engine::ports::{
    Attempt, Batch, Change, CounterDelta, DueTimer, Gauges, IndexedSummary, OutboxChange,
    OutboxEntry, OutboxId, PartitionError, QUARANTINE_AFTER_FAILURES, Quarantine, QuarantineKind,
    QuarantineNote, RunHead, RunOrder, RunRef, TaskChange, TimerChange, TimerKind,
};
use rewind_format::partition as record;
use slatedb::bytes::Bytes;
use slatedb::config::{DurabilityLevel, ScanOptions};
use slatedb::{DbTransaction, IsolationLevel};

use crate::codec::{self, decode, encode};
use crate::keys::{self, Key};
use crate::store::SlateLog;

fn conflict(message: impl Into<String>) -> PartitionError {
    PartitionError::Conflict(message.into())
}

pub(crate) async fn commit(log: &SlateLog, batch: Batch) -> Result<(), PartitionError> {
    let _one_at_a_time = log.inner.commits.lock().await;
    if log.fenced() {
        return Err(log.fence());
    }
    if log.inner.failed.load(Ordering::SeqCst) {
        return Err(PartitionError::Backend(format!(
            "a batch of partition {} may not be durable: open the partition again",
            log.inner.partition
        )));
    }
    if batch.is_empty() {
        return Ok(());
    }
    let transaction = log
        .inner
        .db
        .begin(IsolationLevel::Snapshot)
        .map_err(|err| log.error(err))?;
    let apply = Apply {
        log,
        txn: &transaction,
        runs: Mutex::new(HashMap::new()),
    };
    for change in batch.changes {
        apply.change(change).await?;
    }
    let Some(written) = transaction.commit().await.map_err(|err| log.error(err))? else {
        // Every change of the batch changed nothing.
        return Ok(());
    };
    let durable = match log.inner.db.flush().await {
        Ok(()) => written.await_durable().await,
        Err(err) => Err(err),
    };
    durable.map_err(|err| {
        let error = log.error(err);
        if !matches!(error, PartitionError::Fenced { .. }) {
            log.inner.failed.store(true, Ordering::SeqCst);
        }
        error
    })
}

/// A run as a change finds it: its head, and its summary once it has one.
#[derive(Clone)]
struct RunState {
    head: RunHead,
    summary: Option<IndexedSummary>,
}

impl RunState {
    fn gauges(&self) -> Gauges {
        let mut gauges = Gauges::default();
        gauges.count(&self.head, self.summary.as_ref());
        gauges
    }

    /// Sets or lifts the quarantine, in the head and in the summary.
    fn quarantine(&mut self, quarantine: Option<Quarantine>) {
        if let Some(summary) = &mut self.summary {
            summary.summary.quarantine = quarantine.as_ref().map(QuarantineNote::from);
        }
        self.head.quarantine = quarantine;
    }
}

struct Apply<'a> {
    log: &'a SlateLog,
    txn: &'a DbTransaction,
    /// The runs the transaction read or wrote, as they are in it: `None` for one that does not
    /// exist.
    runs: Mutex<HashMap<RunRef, Option<RunState>>>,
}

fn missing(run: &RunRef) -> PartitionError {
    conflict(format!("run {run} does not exist or has been erased"))
}

impl Apply<'_> {
    fn error(&self, err: slatedb::Error) -> PartitionError {
        self.log.error(err)
    }

    async fn get<R: record::Record>(&self, key: &Key) -> Result<Option<R>, PartitionError> {
        let bytes = self
            .txn
            .get(key.as_bytes())
            .await
            .map_err(|err| self.error(err))?;
        bytes.map(|bytes| decode(&bytes)).transpose()
    }

    async fn exists(&self, key: &Key) -> Result<bool, PartitionError> {
        Ok(self
            .txn
            .get(key.as_bytes())
            .await
            .map_err(|err| self.error(err))?
            .is_some())
    }

    fn put<R: record::Record>(&self, key: &Key, value: &R) -> Result<(), PartitionError> {
        self.txn
            .put(key.as_bytes(), encode(value))
            .map_err(|err| self.error(err))
    }

    /// Writes a key of an index whose value is empty.
    fn mark(&self, key: &Key) -> Result<(), PartitionError> {
        self.txn
            .put(key.as_bytes(), [])
            .map_err(|err| self.error(err))
    }

    fn delete(&self, key: &[u8]) -> Result<(), PartitionError> {
        self.txn.delete(key).map_err(|err| self.error(err))
    }

    /// Every event under `prefix`, the batch's writes included.
    async fn scan(&self, prefix: &Key) -> Result<Vec<(Bytes, Bytes)>, PartitionError> {
        let options = ScanOptions {
            durability_filter: DurabilityLevel::Memory,
            ..crate::store::scan_options()
        };
        let mut iterator = self
            .txn
            .scan_prefix_with_options(prefix.as_bytes(), .., &options)
            .await
            .map_err(|err| self.error(err))?;
        let mut entries = Vec::new();
        while let Some(entry) = iterator.next().await.map_err(|err| self.error(err))? {
            entries.push((entry.key, entry.value));
        }
        Ok(entries)
    }

    /// Whether anything is under `prefix`.
    async fn any(&self, prefix: &Key) -> Result<bool, PartitionError> {
        let mut iterator = self
            .txn
            .scan_prefix(prefix.as_bytes(), ..)
            .await
            .map_err(|err| self.error(err))?;
        Ok(iterator
            .next()
            .await
            .map_err(|err| self.error(err))?
            .is_some())
    }

    /// Deletes every key under `prefix`.
    async fn delete_all(&self, prefix: &Key) -> Result<(), PartitionError> {
        for (key, _) in self.scan(prefix).await? {
            self.delete(&key)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Runs

    /// Reads the run into the transaction's memory, unless a change before read or wrote it:
    /// the changes of a batch to one run (its thousands of timers set, fired) decode its head
    /// and its summary, which hold every wait of the run, once.
    async fn remember(&self, run: &RunRef) -> Result<(), PartitionError> {
        if self.runs().contains_key(run) {
            return Ok(());
        }
        let state = match self.get::<record::RunHead>(&keys::head(run.id)).await? {
            Some(head) if head.tenant == run.tenant.as_str() => {
                let head = codec::head_of(head)?;
                let summary = self
                    .get::<record::Summary>(&keys::summary(run))
                    .await?
                    .map(codec::summary_of)
                    .transpose()?;
                Some(RunState { head, summary })
            }
            _ => None,
        };
        self.runs().insert(run.clone(), state);
        Ok(())
    }

    fn runs(&self) -> std::sync::MutexGuard<'_, HashMap<RunRef, Option<RunState>>> {
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The run, when it exists and is the tenant's.
    async fn load(&self, run: &RunRef) -> Result<Option<RunState>, PartitionError> {
        self.remember(run).await?;
        Ok(self.runs().get(run).cloned().flatten())
    }

    /// The run a change is about: refused when it does not exist or has been erased.
    async fn existing(&self, run: &RunRef) -> Result<RunState, PartitionError> {
        self.load(run).await?.ok_or_else(|| missing(run))
    }

    /// Whether the attempt of the run a change is about has ended: refused when the run does
    /// not exist or has been erased.
    async fn existing_ended(&self, run: &RunRef) -> Result<bool, PartitionError> {
        self.remember(run).await?;
        self.runs()
            .get(run)
            .and_then(Option::as_ref)
            .map(|state| state.head.is_ended())
            .ok_or_else(|| missing(run))
    }

    /// Writes what changed of a run, `before` as it was and `after` as it is (`None`: erased):
    /// its head and the indexes of its head, its summary and its index entries, its gauges.
    async fn write_run(
        &self,
        before: Option<&RunState>,
        after: Option<&RunState>,
    ) -> Result<(), PartitionError> {
        let Some(state) = after.or(before) else {
            return Ok(());
        };
        let run = state.head.run.clone();
        self.runs().insert(run.clone(), after.cloned());
        match after {
            Some(after) => {
                let head = codec::head_record(&after.head);
                self.put(&keys::head(run.id), &head)?;
                if after.head.is_ended() {
                    self.delete(keys::live(run.id).as_bytes())?;
                } else {
                    self.put(&keys::live(run.id), &head)?;
                }
                if after.head.is_unsealed() {
                    self.put(&keys::unsealed(run.id), &head)?;
                } else {
                    self.delete(keys::unsealed(run.id).as_bytes())?;
                }
            }
            None => {
                self.delete(keys::head(run.id).as_bytes())?;
                self.delete(keys::live(run.id).as_bytes())?;
                self.delete(keys::unsealed(run.id).as_bytes())?;
            }
        }

        let old: Vec<Vec<u8>> = before
            .and_then(|state| state.summary.as_ref())
            .map(|indexed| index_keys(&indexed.summary))
            .unwrap_or_default();
        let new_summary = after.and_then(|state| state.summary.as_ref());
        let new: Vec<Vec<u8>> = new_summary
            .map(|indexed| index_keys(&indexed.summary))
            .unwrap_or_default();
        let kept: BTreeSet<&Vec<u8>> = new.iter().collect();
        for key in old.iter().filter(|key| !kept.contains(key)) {
            self.delete(key)?;
        }
        match new_summary {
            Some(indexed) => {
                let summary = encode(&codec::summary_record(indexed));
                self.txn
                    .put(keys::summary(&run).as_bytes(), &summary)
                    .map_err(|err| self.error(err))?;
                for key in &new {
                    self.txn.put(key, &summary).map_err(|err| self.error(err))?;
                }
            }
            None => self.delete(keys::summary(&run).as_bytes())?,
        }

        let held = before.map(RunState::gauges).unwrap_or_default();
        let holds = after.map(RunState::gauges).unwrap_or_default();
        if held != holds {
            let key = keys::gauges(&run.tenant, &state.head.namespace);
            let mut gauges = self
                .get::<record::Gauges>(&key)
                .await?
                .map(codec::gauges_of)
                .unwrap_or_default();
            subtract(&mut gauges, &held);
            gauges.add(&holds);
            if gauges == Gauges::default() {
                self.delete(key.as_bytes())?;
            } else {
                self.put(&key, &codec::gauges_record(&gauges))?;
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Changes

    async fn change(&self, change: Change) -> Result<(), PartitionError> {
        match change {
            Change::CreateRun(create) => self.create_run(*create).await,
            Change::Enqueue {
                run,
                item,
                signal_key,
            } => {
                let ended = self.existing_ended(&run).await?;
                if ended {
                    return Ok(());
                }
                if let Some(key) = &signal_key
                    && self.exists(&keys::signal_key(&run, key)).await?
                {
                    return Ok(());
                }
                let at = keys::inbox(&run, item.id);
                if self.exists(&at).await? {
                    return Err(conflict(format!(
                        "the inbox of run {run} already holds an item {}",
                        item.id
                    )));
                }
                self.put(&at, &codec::inbox_record(&item))?;
                if let Some(key) = &signal_key {
                    self.put(&keys::signal_key(&run, key), &record::SignalKey {})?;
                }
                Ok(())
            }
            Change::SignalKey { run, key } => {
                let state = self.existing(&run).await?;
                if !state.head.is_ended() {
                    self.put(&keys::signal_key(&run, &key), &record::SignalKey {})?;
                }
                Ok(())
            }
            Change::Activation(activation) => self.activate(*activation).await,
            Change::Task { run, seq, change } => {
                let ended = self.existing_ended(&run).await?;
                if ended {
                    return Ok(());
                }
                match change {
                    TaskChange::Put(task) => {
                        if task.task.task.run != run || task.task.task.seq != seq {
                            return Err(conflict(format!(
                                "the task {} is written as {}/{}",
                                task.task.task, run.id, seq.0
                            )));
                        }
                        self.put(&keys::task(&run, seq), &codec::task_record(&task))
                    }
                    TaskChange::Delete => self.delete(keys::task(&run, seq).as_bytes()),
                }
            }
            Change::Timer(timer) => self.timer(timer).await,
            Change::Failure { run, error, at } => {
                let before = self.existing(&run).await?;
                let mut after = before.clone();
                after.head.failures = after.head.failures.saturating_add(1);
                if after.head.failures >= QUARANTINE_AFTER_FAILURES
                    && after.head.quarantine.is_none()
                {
                    after.quarantine(Some(Quarantine {
                        kind: QuarantineKind::Failures,
                        reason: error,
                        since: at,
                        retried_by: None,
                    }));
                }
                self.write_run(Some(&before), Some(&after)).await
            }
            Change::Interrupted { run } => {
                let before = self.existing(&run).await?;
                let mut after = before.clone();
                after.head.unfinished = after.head.unfinished.saturating_add(1);
                self.write_run(Some(&before), Some(&after)).await
            }
            Change::Summary { run, summary } => {
                if summary.summary.run != run {
                    return Err(PartitionError::Conflict(format!(
                        "the summary of run {run} names another run"
                    )));
                }
                let before = self.existing(&run).await?;
                let mut after = before.clone();
                after.summary = Some(*summary);
                self.write_run(Some(&before), Some(&after)).await
            }
            Change::Quarantine { run, quarantine } => {
                let before = self.existing(&run).await?;
                let mut after = before.clone();
                after.quarantine(Some(quarantine));
                self.write_run(Some(&before), Some(&after)).await
            }
            Change::Release { run } => {
                let before = self.existing(&run).await?;
                let mut after = before.clone();
                after.quarantine(None);
                after.head.failures = 0;
                after.head.unfinished = 0;
                self.write_run(Some(&before), Some(&after)).await
            }
            Change::Watch { run, watcher } => {
                self.existing_ended(&run).await?;
                let at = keys::watcher(&run, watcher.run);
                if !self.exists(&at).await? {
                    self.put(&at, &codec::watcher_record(&watcher))?;
                }
                Ok(())
            }
            Change::Seal { run, attempt } => self.seal(&run, attempt).await,
            Change::Erase { run } => self.erase(&run).await,
            Change::EraseNamespace {
                tenant,
                namespace,
                erased,
            } => {
                let runs =
                    keys::index_family(keys::index_of(&tenant, Some(&namespace)), RunOrder::Oldest);
                if self.any(&runs).await? {
                    return Err(conflict(format!(
                        "the namespace {namespace} still holds runs in this partition"
                    )));
                }
                let hours = keys::hours_of(&tenant, &namespace);
                for (key, value) in self.scan(&hours).await? {
                    let hour = key
                        .get(hours.as_bytes().len()..)
                        .and_then(|rest| rest.try_into().ok())
                        .map(|bytes: [u8; 4]| rewind_engine::ports::Hour(u32::from_be_bytes(bytes)))
                        .ok_or_else(|| codec::corrupt("an hour's key"))?;
                    let counts = codec::counters_of(decode::<record::Counters>(&value)?);
                    self.delete(&key)?;
                    self.delete(keys::tenant_hour(&tenant, hour, &namespace).as_bytes())?;
                    // What the namespace consumed stays in its tenant's counts.
                    self.count(CounterDelta::Hour {
                        tenant: tenant.clone(),
                        namespace: erased.clone(),
                        hour,
                        counts: counts.consumed(),
                    })
                    .await?;
                }
                self.delete_all(&keys::shapes_of(&tenant, Some(&namespace)))
                    .await?;
                self.delete_all(&keys::start_keys_of(&tenant, &namespace))
                    .await?;
                self.delete(keys::counted(&tenant, &namespace).as_bytes())?;
                self.delete(keys::gauges(&tenant, &namespace).as_bytes())
            }
            Change::Outbox(OutboxChange::Add(entry)) => self.add_outbox(&entry).await,
            Change::Outbox(OutboxChange::Remove(id)) => {
                let Some(entry) = self.get::<record::Outbox>(&keys::outbox(id)).await? else {
                    return Ok(());
                };
                let entry = codec::outbox_of(entry)?;
                self.delete(keys::outbox(id).as_bytes())?;
                if let Some(run) = entry.run {
                    let run = RunRef::new(entry.tenant.clone(), run);
                    self.delete(keys::run_outbox(&run, id).as_bytes())?;
                }
                self.cancel(&TimerKind::WebhookRetry {
                    tenant: entry.tenant,
                    entry: id,
                })
                .await
            }
            Change::Counters(deltas) => {
                for delta in deltas {
                    self.count(delta).await?;
                }
                Ok(())
            }
            Change::SignalUrlToken { run, name, hash } => {
                let ended = self.existing_ended(&run).await?;
                if ended {
                    return Ok(());
                }
                if let Some(bound) = self
                    .get::<record::SignalUrlToken>(&keys::signal_url(hash))
                    .await?
                    && (bound.run != run.id || bound.name != name)
                {
                    return Err(conflict(format!(
                        "a signal URL token of run {run} is bound to another run or signal"
                    )));
                }
                self.put(
                    &keys::signal_url(hash),
                    &codec::signal_url_record(&run, &name),
                )?;
                self.mark(&keys::run_signal_url(&run, hash))
            }
            Change::Tenant { tenant, state } => self.put(
                &keys::tenant_state(&tenant),
                &codec::tenant_state_record(&state),
            ),
        }
    }

    async fn create_run(&self, create: CreateRun) -> Result<(), PartitionError> {
        let CreateRun {
            run,
            mut head,
            start_key,
            events: entries,
            payloads,
            summary,
        } = create;
        if head.run != run || summary.summary.run != run {
            return Err(conflict(format!(
                "the head or the summary of the creation of run {run} names another run"
            )));
        }
        if self.exists(&keys::head(run.id)).await? {
            return Err(conflict(format!("run {} already exists", run.id)));
        }
        if let Some(key) = &start_key {
            let at = keys::start_key(&run.tenant, &head.namespace, key);
            if self.exists(&at).await? {
                return Err(conflict(format!(
                    "the start key {key:?} is already used in the namespace {}",
                    head.namespace
                )));
            }
            self.put(&at, &record::StartKey { run: run.id })?;
            self.mark(&keys::run_start_key(&run, key))?;
        }
        continue_history(&run, head.attempt, 0, &entries)?;
        head.history_len = entries.len() as u64;
        head.sealed = None;
        for entry in &entries {
            self.put(
                &keys::history(&run, head.attempt, entry.index),
                &codec::event_record(entry),
            )?;
        }
        for payload in &payloads {
            self.put(
                &keys::payload(&run, payload.digest),
                &codec::payload_record(payload),
            )?;
        }
        let state = RunState {
            head,
            summary: Some(summary),
        };
        self.write_run(None, Some(&state)).await
    }

    async fn activate(&self, activation: Activation) -> Result<(), PartitionError> {
        let Activation {
            run,
            attempt,
            events: entries,
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
        let before = self.existing(&run).await?;
        if before.head.is_ended() {
            return Err(conflict(format!("the attempt of run {run} has ended")));
        }
        if attempt != before.head.attempt || head.attempt != attempt {
            return Err(conflict(format!(
                "run {run} is at attempt {}, not {attempt}",
                before.head.attempt
            )));
        }
        continue_history(&run, attempt, before.head.history_len, &entries)?;
        for entry in &entries {
            self.put(
                &keys::history(&run, attempt, entry.index),
                &codec::event_record(entry),
            )?;
        }
        head.history_len = before.head.history_len + entries.len() as u64;
        head.sealed = before.head.sealed;
        for id in &consumed {
            let at = keys::inbox(&run, *id);
            if !self.exists(&at).await? {
                return Err(conflict(format!(
                    "the inbox of run {run} has no item {id} to consume"
                )));
            }
            self.delete(at.as_bytes())?;
        }
        for payload in &payloads {
            self.put(
                &keys::payload(&run, payload.digest),
                &codec::payload_record(payload),
            )?;
        }
        for task in &tasks {
            let seq = task.task.task.seq;
            if task.task.task.run != run {
                return Err(conflict(format!(
                    "an activation of run {run} creates a task of run {}",
                    task.task.task.run
                )));
            }
            let at = keys::task(&run, seq);
            if self.exists(&at).await? {
                return Err(conflict(format!("run {run} already has a task {}", seq.0)));
            }
            self.put(&at, &codec::task_record(task))?;
        }
        let ended = head.is_ended();
        let after = RunState {
            head,
            summary: Some(summary),
        };
        self.write_run(Some(&before), Some(&after)).await?;
        for timer in timers {
            self.timer(timer).await?;
        }
        for delta in counters {
            self.count(delta).await?;
        }
        for entry in &outbox {
            self.add_outbox(entry).await?;
        }
        if ended {
            self.end(&run).await?;
        }
        Ok(())
    }

    /// Clears what the ended attempt of a run had pending: its timers, tasks, inbox and webhook
    /// tokens.
    async fn end(&self, run: &RunRef) -> Result<(), PartitionError> {
        for (_, timer) in self.scan(&keys::timers_of(run)).await? {
            let timer = codec::timer_of(decode(&timer)?)?;
            if timer.kind.ends_with_the_attempt() {
                self.delete(keys::due(timer.at.0, &timer.kind).as_bytes())?;
                self.delete(keys::timer(&timer.kind).as_bytes())?;
            }
        }
        let signal_urls = keys::of_run(keys::RUN_SIGNAL_URL, run);
        for (key, _) in self.scan(&signal_urls).await? {
            let hash = hash_after(&key, &signal_urls)?;
            self.delete(keys::signal_url(hash).as_bytes())?;
            self.delete(&key)?;
        }
        self.delete_all(&keys::of_run(keys::TASK, run)).await?;
        self.delete_all(&keys::of_run(keys::INBOX, run)).await
    }

    async fn timer(&self, change: TimerChange) -> Result<(), PartitionError> {
        match change {
            TimerChange::Set(DueTimer { at, kind }) => {
                match &kind {
                    TimerKind::WebhookRetry { tenant, entry } => {
                        let known = self.get::<record::Outbox>(&keys::outbox(*entry)).await?;
                        if !known.is_some_and(|known| known.tenant == tenant.as_str()) {
                            return Err(conflict(format!(
                                "the outbox has no entry {entry} of tenant {tenant} to retry"
                            )));
                        }
                    }
                    other => {
                        if let Some(run) = other.run() {
                            self.existing_ended(run).await?;
                        }
                    }
                }
                self.cancel(&kind).await?;
                let timer = codec::timer_record(&DueTimer {
                    at,
                    kind: kind.clone(),
                });
                self.put(&keys::timer(&kind), &timer)?;
                self.put(&keys::due(at.0, &kind), &timer)
            }
            TimerChange::Cancel(kind) => self.cancel(&kind).await,
        }
    }

    /// Cancels a timer, when it is set.
    async fn cancel(&self, kind: &TimerKind) -> Result<(), PartitionError> {
        let at = keys::timer(kind);
        if let Some(timer) = self.get::<record::Timer>(&at).await? {
            self.delete(keys::due(timer.at, kind).as_bytes())?;
            self.delete(at.as_bytes())?;
        }
        Ok(())
    }

    async fn seal(&self, run: &RunRef, attempt: Attempt) -> Result<(), PartitionError> {
        let Some(before) = self.load(run).await? else {
            return Ok(());
        };
        if before.head.sealed.is_some_and(|sealed| sealed >= attempt) {
            return Ok(());
        }
        if attempt != before.head.attempt || !before.head.is_ended() {
            return Err(conflict(format!(
                "attempt {attempt} of run {run} has not ended"
            )));
        }
        self.delete_all(&keys::history_of(run, attempt)).await?;
        // The signal keys stay for the life of the run: `erase` deletes them.
        for family in [keys::INBOX, keys::TASK, keys::PAYLOAD] {
            self.delete_all(&keys::of_run(family, run)).await?;
        }
        let mut after = before.clone();
        after.head.sealed = Some(attempt);
        self.write_run(Some(&before), Some(&after)).await
    }

    async fn erase(&self, run: &RunRef) -> Result<(), PartitionError> {
        let Some(before) = self.load(run).await? else {
            return Ok(());
        };
        let start_keys = keys::of_run(keys::RUN_START_KEY, run);
        for (key, _) in self.scan(&start_keys).await? {
            let (start_key, _) = key
                .get(start_keys.as_bytes().len()..)
                .and_then(keys::read_str)
                .ok_or_else(|| codec::corrupt("a run's start key"))?;
            let at = keys::start_key(&run.tenant, &before.head.namespace, &start_key);
            if self
                .get::<record::StartKey>(&at)
                .await?
                .is_some_and(|start| start.run == run.id)
            {
                self.delete(at.as_bytes())?;
            }
        }
        let signal_urls = keys::of_run(keys::RUN_SIGNAL_URL, run);
        for (key, _) in self.scan(&signal_urls).await? {
            self.delete(keys::signal_url(hash_after(&key, &signal_urls)?).as_bytes())?;
        }
        let entries = keys::of_run(keys::RUN_OUTBOX, run);
        for (key, _) in self.scan(&entries).await? {
            let id = key
                .get(entries.as_bytes().len()..)
                .and_then(|rest| rest.try_into().ok())
                .map(|bytes: [u8; 8]| OutboxId(u64::from_be_bytes(bytes)))
                .ok_or_else(|| codec::corrupt("a run's outbox entry"))?;
            self.delete(keys::outbox(id).as_bytes())?;
            self.cancel(&TimerKind::WebhookRetry {
                tenant: run.tenant.clone(),
                entry: id,
            })
            .await?;
        }
        for (key, timer) in self.scan(&keys::timers_of(run)).await? {
            let timer: record::Timer = decode(&timer)?;
            self.delete(keys::due(timer.at, &codec::timer_of(timer)?.kind).as_bytes())?;
            self.delete(&key)?;
        }
        for family in keys::RUN_FAMILIES {
            self.delete_all(&keys::of_run(family, run)).await?;
        }
        self.write_run(Some(&before), None).await
    }

    async fn add_outbox(&self, entry: &OutboxEntry) -> Result<(), PartitionError> {
        let at = keys::outbox(entry.id);
        if self.exists(&at).await? {
            return Err(conflict(format!(
                "the outbox already holds an entry {}",
                entry.id
            )));
        }
        self.put(&at, &codec::outbox_record(entry))?;
        if let Some(run) = entry.run {
            self.mark(&keys::run_outbox(
                &RunRef::new(entry.tenant.clone(), run),
                entry.id,
            ))?;
        }
        Ok(())
    }

    async fn count(&self, delta: CounterDelta) -> Result<(), PartitionError> {
        match delta {
            CounterDelta::Hour {
                tenant,
                namespace,
                hour,
                counts,
            } => {
                let at = keys::hour(&tenant, &namespace, hour);
                let mut total = self
                    .get::<record::Counters>(&at)
                    .await?
                    .map(codec::counters_of)
                    .unwrap_or_default();
                total.add(&counts);
                let total = codec::counters_record(&total);
                self.put(&at, &total)?;
                self.put(&keys::tenant_hour(&tenant, hour, &namespace), &total)?;
                self.mark(&keys::counted(&tenant, &namespace))
            }
            CounterDelta::Shape {
                tenant,
                namespace,
                action,
                shape,
            } => {
                let at = keys::shape(&tenant, &namespace, &action);
                let merged = match self.get::<record::Shape>(&at).await? {
                    Some(known) => known.shape.merge(shape),
                    None => shape,
                };
                self.put(&at, &record::Shape { shape: merged })
            }
        }
    }
}

/// The index keys of a summary.
fn index_keys(summary: &rewind_engine::ports::RunSummary) -> Vec<Vec<u8>> {
    keys::index_entries(summary)
        .into_iter()
        .map(Key::into_bytes)
        .collect()
}

/// The 32-byte hash after `prefix` in a key.
fn hash_after(key: &[u8], prefix: &Key) -> Result<rewind_core::Digest, PartitionError> {
    key.get(prefix.as_bytes().len()..)
        .and_then(|rest| rest.try_into().ok())
        .map(rewind_core::Digest::from_bytes)
        .ok_or_else(|| codec::corrupt("a run's signal URL token"))
}

/// Takes from `gauges` what `held` counted.
fn subtract(gauges: &mut Gauges, held: &Gauges) {
    gauges.active_runs = gauges.active_runs.saturating_sub(held.active_runs);
    gauges.quarantined = gauges.quarantined.saturating_sub(held.quarantined);
    for (kind, count) in &held.waiting {
        if let Some(left) = gauges.waiting.get_mut(kind) {
            *left = left.saturating_sub(*count);
            if *left == 0 {
                gauges.waiting.remove(kind);
            }
        }
    }
    for (code, count) in &held.scripts {
        if let Some(left) = gauges.scripts.get_mut(code) {
            *left = left.saturating_sub(*count);
            if *left == 0 {
                gauges.scripts.remove(code);
            }
        }
    }
}

/// Refuses events that do not continue the attempt's history at `len`, index by index.
fn continue_history(
    run: &RunRef,
    attempt: Attempt,
    len: u64,
    entries: &[HistoryEvent],
) -> Result<(), PartitionError> {
    for (offset, entry) in entries.iter().enumerate() {
        let expected = len + offset as u64;
        if entry.index != expected {
            return Err(conflict(format!(
                "attempt {attempt} of run {run} continues at event {expected}, not {}",
                entry.index
            )));
        }
    }
    Ok(())
}
