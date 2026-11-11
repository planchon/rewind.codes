use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use ptc_core::{
    CancelReason, Completion, Digest, Epoch, HistoryEntry, RunId, RunStatus, Script, Seq,
    Timestamp, Toolset,
};

use crate::ports::{
    Activation, CompiledScript, Created, Enqueued, ExpiredRuns, InboxItem, JournalStore, Lease,
    NewRun, RunFilter, RunRecord, StoreError, StoredScript, ToolTask, Trigger,
};

/// `JournalStore` in memory: engine tests and mock mode. It is the reference implementation
/// of the store contract, and passes the conformance suite.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    scripts: HashMap<Digest, StoredScript>,
    toolsets: HashMap<Digest, Toolset>,
    runs: BTreeMap<RunId, Run>,
    idempotency_keys: HashMap<String, RunId>,
    next_inbox_id: u64,
}

#[derive(Debug)]
struct Run {
    record: RunRecord,
    fence: u64,
    lease_until: Option<Timestamp>,
    expiry_enqueued: bool,
    history: Vec<HistoryEntry>,
    inbox: Vec<InboxItem>,
    signal_keys: BTreeSet<String>,
    timers: BTreeMap<Seq, Timestamp>,
    tool_tasks: BTreeMap<Seq, Task>,
}

#[derive(Debug)]
struct Task {
    tool: String,
    args: ptc_core::Payload,
    attempt: u32,
    claimed_until: Option<Timestamp>,
}

impl MemoryJournal {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, StoreError> {
        self.state
            .lock()
            .map_err(|_| StoreError::Backend("the memory journal's lock is poisoned".into()))
    }
}

impl State {
    fn run(&self, run_id: RunId) -> Result<&Run, StoreError> {
        self.runs
            .get(&run_id)
            .ok_or(StoreError::RunNotFound(run_id))
    }

    fn run_mut(&mut self, run_id: RunId) -> Result<&mut Run, StoreError> {
        self.runs
            .get_mut(&run_id)
            .ok_or(StoreError::RunNotFound(run_id))
    }

    /// The run, if `lease` is still its current lease.
    fn leased(&mut self, lease: &Lease) -> Result<&mut Run, StoreError> {
        let run = self.run_mut(lease.run_id)?;
        if run.fence != lease.fence {
            return Err(StoreError::Fenced {
                run_id: lease.run_id,
                fence: lease.fence,
            });
        }
        Ok(run)
    }

    fn push_inbox(
        &mut self,
        run_id: RunId,
        trigger: Trigger,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let id = self.next_inbox_id;
        self.next_inbox_id += 1;
        self.run_mut(run_id)?.inbox.push(InboxItem {
            id,
            trigger,
            received_at: now,
        });
        Ok(())
    }
}

impl Run {
    fn is_terminal(&self) -> bool {
        self.record.status.is_terminal()
    }

    fn is_leased(&self, now: Timestamp) -> bool {
        self.lease_until.is_some_and(|until| until > now)
    }

    fn lease(&mut self, now: Timestamp, duration: Duration) -> Lease {
        self.fence += 1;
        self.lease_until = Some(now.saturating_add(duration));
        Lease {
            run_id: self.record.id,
            fence: self.fence,
        }
    }
}

impl JournalStore for MemoryJournal {
    async fn put_script(
        &self,
        script: &Script,
        compiled: &CompiledScript,
    ) -> Result<(), StoreError> {
        self.lock()?
            .scripts
            .entry(script.digest())
            .or_insert_with(|| StoredScript {
                script: script.clone(),
                compiled: compiled.clone(),
            });
        Ok(())
    }

    async fn get_script(&self, digest: Digest) -> Result<Option<StoredScript>, StoreError> {
        Ok(self.lock()?.scripts.get(&digest).cloned())
    }

    async fn put_toolset(&self, toolset: &Toolset) -> Result<(), StoreError> {
        self.lock()?
            .toolsets
            .entry(toolset.digest())
            .or_insert_with(|| toolset.clone());
        Ok(())
    }

    async fn get_toolset(&self, digest: Digest) -> Result<Option<Toolset>, StoreError> {
        Ok(self.lock()?.toolsets.get(&digest).cloned())
    }

    async fn create_run(
        &self,
        run: NewRun,
        idempotency_key: Option<String>,
    ) -> Result<Created, StoreError> {
        let mut state = self.lock()?;
        if let Some(key) = &idempotency_key
            && let Some(existing) = state.idempotency_keys.get(key)
        {
            return Ok(Created {
                run_id: *existing,
                created: false,
            });
        }
        if state.runs.contains_key(&run.id) {
            return Err(StoreError::Conflict(format!(
                "run {} already exists",
                run.id
            )));
        }
        let record = RunRecord {
            id: run.id,
            script: run.started.script,
            toolset: run.started.toolset,
            status: RunStatus::Pending,
            epoch: run.started.epoch,
            expires_at: run.started.expires_at,
            history_len: 0,
            quarantine: None,
            metadata: run.metadata,
            idempotency_key: idempotency_key.clone(),
            created_at: run.created_at,
            updated_at: run.created_at,
        };
        state.runs.insert(
            run.id,
            Run {
                record,
                fence: 0,
                lease_until: None,
                expiry_enqueued: false,
                history: Vec::new(),
                inbox: Vec::new(),
                signal_keys: BTreeSet::new(),
                timers: BTreeMap::new(),
                tool_tasks: BTreeMap::new(),
            },
        );
        if let Some(key) = idempotency_key {
            state.idempotency_keys.insert(key, run.id);
        }
        state.push_inbox(run.id, Trigger::RunStarted(run.started), run.created_at)?;
        Ok(Created {
            run_id: run.id,
            created: true,
        })
    }

    async fn enqueue(
        &self,
        run_id: RunId,
        completion: Completion,
        idempotency_key: Option<String>,
        now: Timestamp,
    ) -> Result<Enqueued, StoreError> {
        let mut state = self.lock()?;
        let run = state.run_mut(run_id)?;
        if run.is_terminal() {
            return Ok(Enqueued::RunEnded);
        }
        if let Some(key) = idempotency_key
            && !run.signal_keys.insert(key)
        {
            return Ok(Enqueued::Duplicate);
        }
        state.push_inbox(run_id, Trigger::Completion(completion), now)?;
        Ok(Enqueued::Accepted)
    }

    async fn acquire_runnable(
        &self,
        _owner: &str,
        epochs: &[Epoch],
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        let mut state = self.lock()?;
        Ok(state
            .runs
            .values_mut()
            .filter(|run| {
                !run.is_terminal()
                    && run.record.quarantine.is_none()
                    && !run.is_leased(now)
                    && epochs.contains(&run.record.epoch)
            })
            .filter_map(|run| Some((run.inbox.first()?.id, run)))
            .min_by_key(|(oldest, _)| *oldest)
            .map(|(_, run)| run.lease(now, lease_duration)))
    }

    async fn acquire_run(
        &self,
        run_id: RunId,
        _owner: &str,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        let mut state = self.lock()?;
        let run = state.run_mut(run_id)?;
        if run.is_terminal() || run.is_leased(now) {
            return Ok(None);
        }
        Ok(Some(run.lease(now, lease_duration)))
    }

    async fn load_history(
        &self,
        run_id: RunId,
        from: u64,
    ) -> Result<Vec<HistoryEntry>, StoreError> {
        let state = self.lock()?;
        let history = &state.run(run_id)?.history;
        let from = usize::try_from(from)
            .unwrap_or(usize::MAX)
            .min(history.len());
        Ok(history[from..].to_vec())
    }

    async fn take_inbox(&self, lease: &Lease) -> Result<Vec<InboxItem>, StoreError> {
        let mut state = self.lock()?;
        Ok(state.leased(lease)?.inbox.clone())
    }

    async fn commit_activation(
        &self,
        lease: &Lease,
        activation: Activation,
    ) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        let run = state.leased(lease)?;

        // Validate everything before writing anything: the commit is all or nothing.
        let next = run.history.len() as u64;
        for (offset, entry) in activation.entries.iter().enumerate() {
            if entry.index != next + offset as u64 {
                return Err(StoreError::Conflict(format!(
                    "entry index {} does not continue the history of run {} (next index {})",
                    entry.index,
                    lease.run_id,
                    next + offset as u64
                )));
            }
        }

        if let Some(last) = activation.entries.last() {
            run.record.updated_at = last.recorded_at;
        }
        run.history.extend(activation.entries);
        run.record.history_len = run.history.len() as u64;
        run.record.status = activation.status;
        let consumed: BTreeSet<u64> = activation.consumed.into_iter().collect();
        run.inbox.retain(|item| !consumed.contains(&item.id));
        for task in activation.tool_calls {
            run.tool_tasks.insert(
                task.seq,
                Task {
                    tool: task.tool,
                    args: task.args,
                    attempt: 0,
                    claimed_until: None,
                },
            );
        }
        for timer in activation.timers {
            run.timers.insert(timer.seq, timer.fire_at);
        }
        for seq in activation.cancelled_timers {
            run.timers.remove(&seq);
        }
        if activation.status.is_terminal() {
            run.timers.clear();
            run.tool_tasks.clear();
            run.inbox.clear();
            run.record.quarantine = None;
        }
        Ok(())
    }

    async fn release(&self, lease: &Lease) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        if let Ok(run) = state.leased(lease) {
            run.lease_until = None;
        }
        Ok(())
    }

    async fn quarantine(&self, lease: &Lease, mismatch: String) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        let run = state.leased(lease)?;
        run.record.quarantine = Some(mismatch);
        run.lease_until = None;
        Ok(())
    }

    async fn release_quarantine(&self, run_id: RunId) -> Result<bool, StoreError> {
        let mut state = self.lock()?;
        Ok(state.run_mut(run_id)?.record.quarantine.take().is_some())
    }

    async fn fire_due_timers(&self, now: Timestamp) -> Result<usize, StoreError> {
        let mut state = self.lock()?;
        let mut due: Vec<(Timestamp, RunId, Seq)> = state
            .runs
            .values()
            .flat_map(|run| {
                run.timers
                    .iter()
                    .filter(|(_, fire_at)| **fire_at <= now)
                    .map(|(seq, fire_at)| (*fire_at, run.record.id, *seq))
            })
            .collect();
        due.sort();
        for (_, run_id, seq) in &due {
            state.run_mut(*run_id)?.timers.remove(seq);
            state.push_inbox(
                *run_id,
                Trigger::Completion(Completion::TimerFired { seq: *seq }),
                now,
            )?;
        }
        Ok(due.len())
    }

    async fn next_timer(&self) -> Result<Option<Timestamp>, StoreError> {
        Ok(self
            .lock()?
            .runs
            .values()
            .flat_map(|run| run.timers.values().copied())
            .min())
    }

    async fn expire_due_runs(&self, now: Timestamp) -> Result<ExpiredRuns, StoreError> {
        let mut state = self.lock()?;
        let mut expired = ExpiredRuns::default();
        let due: Vec<RunId> = state
            .runs
            .values()
            .filter(|run| !run.is_terminal() && run.record.expires_at <= now)
            .map(|run| run.record.id)
            .collect();
        for run_id in due {
            let run = state.run_mut(run_id)?;
            if run.record.quarantine.is_some() {
                expired.quarantined.push(run_id);
            } else if !run.expiry_enqueued {
                run.expiry_enqueued = true;
                let cancel = Completion::CancelRequested {
                    reason: CancelReason::Ttl,
                };
                state.push_inbox(run_id, Trigger::Completion(cancel), now)?;
                expired.cancelled.push(run_id);
            }
        }
        Ok(expired)
    }

    async fn next_deadline(&self) -> Result<Option<Timestamp>, StoreError> {
        Ok(self
            .lock()?
            .runs
            .values()
            .filter(|run| !run.is_terminal() && !run.expiry_enqueued)
            .map(|run| run.record.expires_at)
            .min())
    }

    async fn claim_tool_tasks(
        &self,
        _owner: &str,
        now: Timestamp,
        lease_duration: Duration,
        limit: usize,
    ) -> Result<Vec<ToolTask>, StoreError> {
        let mut state = self.lock()?;
        let mut claimed = Vec::new();
        'runs: for run in state.runs.values_mut() {
            for (seq, task) in &mut run.tool_tasks {
                if claimed.len() >= limit {
                    break 'runs;
                }
                if task.claimed_until.is_some_and(|until| until > now) {
                    continue;
                }
                task.attempt += 1;
                task.claimed_until = Some(now.saturating_add(lease_duration));
                claimed.push(ToolTask {
                    run_id: run.record.id,
                    seq: *seq,
                    tool: task.tool.clone(),
                    args: task.args.clone(),
                    attempt: task.attempt,
                });
            }
        }
        Ok(claimed)
    }

    async fn complete_tool_task(
        &self,
        task: &ToolTask,
        outcome: Completion,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        let Some(run) = state.runs.get_mut(&task.run_id) else {
            return Ok(());
        };
        if run.tool_tasks.remove(&task.seq).is_none() {
            // The run ended, or another attempt already completed the task: dropped.
            return Ok(());
        }
        state.push_inbox(task.run_id, Trigger::Completion(outcome), now)
    }

    async fn get_run(&self, run_id: RunId) -> Result<Option<RunRecord>, StoreError> {
        Ok(self.lock()?.runs.get(&run_id).map(|run| run.record.clone()))
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<Vec<RunRecord>, StoreError> {
        let state = self.lock()?;
        let mut runs: Vec<RunRecord> = state
            .runs
            .values()
            .map(|run| &run.record)
            .filter(|record| filter.status.is_none_or(|status| record.status == status))
            .filter(|record| {
                filter
                    .labels
                    .iter()
                    .all(|(key, value)| record.metadata.labels.get(key) == Some(value))
            })
            .cloned()
            .collect();
        runs.sort_by_key(|record| (record.created_at, record.id));
        Ok(runs)
    }
}
