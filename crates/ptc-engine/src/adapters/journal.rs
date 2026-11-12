use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use ptc_core::{
    CancelReason, Completion, Digest, Epoch, HistoryEntry, RunId, Script, Seq, Timestamp,
    ToolFailure, ToolOutcome, ToolPolicy, ToolRoute, Toolset,
};

use crate::adapters::random_token;
use crate::ports::{
    AcquireRunnable, Activation, ApiKey, Archive, ClaimedTask, CompiledScript, CreateRun, Created,
    Enqueued, ExpiredRuns, Failures, FeedConsumer, FeedItem, InboxItem, JournalStore, Lease,
    NewRun, PAYLOAD_DELETION_TIMEOUT, ParkKind, ParkReason, ParkedScope, PayloadRecord,
    QUARANTINE_AFTER_FAILURES, Quarantine, QuarantineKind, RunCursor, RunFilter, RunRecord, RunRef,
    StoreError, StoredScript, StoredToolset, Subscription, TaskFilter, TaskRef, TaskState,
    TaskToken, TaskUpdate, Tenant, TenantId, TenantScope, ToolDeclarations, ToolTask, Trigger,
    WebhookTarget, WebhookToken, deferred_timeout_failure,
};

/// `JournalStore` in memory: engine tests and mock mode. It is the reference implementation
/// of the store contract, and passes the conformance suite.
#[derive(Debug, Default)]
pub struct MemoryJournal {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    scripts: HashMap<(TenantId, Digest), StoredScript>,
    toolsets: HashMap<(TenantId, Digest), StoredToolset>,
    /// Run ids are unique across tenants.
    runs: BTreeMap<RunId, Run>,
    idempotency_keys: HashMap<(TenantId, String), RunId>,
    tenants: BTreeMap<TenantId, Tenant>,
    api_keys: HashMap<Digest, ApiKey>,
    subscriptions: BTreeMap<(TenantId, String), Subscription>,
    payloads: BTreeMap<(TenantId, Digest), StoredPayload>,
    /// Token → (run, signal name).
    webhook_tokens: HashMap<String, (RunId, String)>,
    feed: BTreeMap<u64, FeedRow>,
    next_inbox_id: u64,
    next_feed_id: u64,
}

#[derive(Debug)]
struct Run {
    record: RunRecord,
    fence: u64,
    lease: Option<HeldLease>,
    expiry_enqueued: bool,
    history: Vec<HistoryEntry>,
    inbox: Vec<InboxItem>,
    signal_keys: BTreeSet<String>,
    timers: BTreeMap<Seq, Timestamp>,
    tasks: BTreeMap<Seq, Task>,
    /// Signal name → webhook token.
    webhook_tokens: BTreeMap<String, String>,
}

#[derive(Debug)]
struct HeldLease {
    owner: String,
    until: Timestamp,
}

#[derive(Debug)]
struct Task {
    tool: String,
    route: ToolRoute,
    args: ptc_core::Payload,
    policy: ToolPolicy,
    server_concurrency: Option<u32>,
    state: TaskState,
    attempt: u32,
    schema_change_approved: bool,
    /// The token of the current claim, or of the deferral.
    token: Option<String>,
    created_at: Timestamp,
}

#[derive(Debug)]
struct StoredPayload {
    record: PayloadRecord,
    refs: BTreeSet<RunId>,
    unreferenced_since: Option<Timestamp>,
    /// When a cleanup started deleting the payload: its blob may be gone.
    deletion_started: Option<Timestamp>,
}

#[derive(Debug)]
struct FeedRow {
    item: FeedItem,
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
    fn run(&self, run: &RunRef) -> Result<&Run, StoreError> {
        self.runs
            .get(&run.id)
            .filter(|found| found.record.run.tenant == run.tenant)
            .ok_or(StoreError::RunNotFound(run.id))
    }

    fn run_mut(&mut self, run: &RunRef) -> Result<&mut Run, StoreError> {
        self.runs
            .get_mut(&run.id)
            .filter(|found| found.record.run.tenant == run.tenant)
            .ok_or(StoreError::RunNotFound(run.id))
    }

    /// The run, if `lease` is still its current lease.
    fn leased(&mut self, lease: &Lease) -> Result<&mut Run, StoreError> {
        let run = self.run_mut(&lease.run)?;
        if run.fence != lease.fence {
            return Err(StoreError::Fenced {
                run_id: lease.run.id,
                fence: lease.fence,
            });
        }
        Ok(run)
    }

    fn push_inbox(&mut self, run_id: RunId, trigger: Trigger, now: Timestamp) {
        let id = self.next_inbox_id;
        self.next_inbox_id += 1;
        if let Some(run) = self.runs.get_mut(&run_id) {
            run.inbox.push(InboxItem {
                id,
                trigger,
                received_at: now,
            });
        }
    }

    /// A commit's payloads of the blob tier need a reserved row whose deletion has not
    /// started: their blob may be gone otherwise.
    fn check_reserved(
        &self,
        tenant: &TenantId,
        payloads: &[PayloadRecord],
    ) -> Result<(), StoreError> {
        for payload in payloads.iter().filter(|payload| payload.value.is_none()) {
            let reserved = self
                .payloads
                .get(&(tenant.clone(), payload.digest))
                .is_some_and(|stored| stored.deletion_started.is_none());
            if !reserved {
                return Err(unreserved(payload.digest));
            }
        }
        Ok(())
    }

    /// Whether the run has feed items left to apply: its history is still needed.
    fn has_feed_items(&self, run_id: RunId) -> bool {
        self.feed.values().any(|row| row.item.run.id == run_id)
    }

    fn retention(&self, tenant: &TenantId) -> crate::ports::Retention {
        self.tenants
            .get(tenant)
            .map(|found| found.retention)
            .unwrap_or_default()
    }

    /// The task holding `token`.
    fn task_by_token(&mut self, token: &TaskToken) -> Option<(RunId, Seq)> {
        self.runs.values().find_map(|run| {
            run.tasks.iter().find_map(|(seq, task)| {
                (task.token.as_deref() == Some(token.0.as_str()))
                    .then_some((run.record.run.id, *seq))
            })
        })
    }

    /// Deletes a task and enqueues its outcome.
    fn finish_task(&mut self, run_id: RunId, seq: Seq, outcome: ToolOutcome, now: Timestamp) {
        if let Some(run) = self.runs.get_mut(&run_id) {
            run.tasks.remove(&seq);
        }
        let completion = outcome.into_completion(seq);
        self.push_inbox(run_id, Trigger::Completion(completion), now);
    }

    /// Clears what a run that has ended no longer needs.
    fn end_run(&mut self, run_id: RunId) {
        let Some(run) = self.runs.get_mut(&run_id) else {
            return;
        };
        run.timers.clear();
        run.tasks.clear();
        run.inbox.clear();
        run.record.quarantine = None;
        for token in std::mem::take(&mut run.webhook_tokens).into_values() {
            self.webhook_tokens.remove(&token);
        }
    }
}

impl Run {
    fn is_terminal(&self) -> bool {
        self.record.status.is_terminal()
    }

    fn is_leased(&self, now: Timestamp) -> bool {
        self.lease.as_ref().is_some_and(|lease| lease.until > now)
    }

    fn lease(&mut self, owner: &str, now: Timestamp, duration: Duration) -> Lease {
        self.fence += 1;
        self.lease = Some(HeldLease {
            owner: owner.to_owned(),
            until: now.saturating_add(duration),
        });
        Lease {
            run: self.record.run.clone(),
            fence: self.fence,
        }
    }

    fn view(&self, seq: Seq, task: &Task) -> ToolTask {
        ToolTask {
            task: TaskRef {
                run: self.record.run.clone(),
                seq,
            },
            toolset: self.record.toolset,
            tool: task.tool.clone(),
            route: task.route.clone(),
            args: task.args.clone(),
            policy: task.policy,
            state: task.state.clone(),
            attempt: task.attempt,
            schema_change_approved: task.schema_change_approved,
            created_at: task.created_at,
        }
    }
}

impl Task {
    /// A call in flight at `now`: claimed and not expired, or deferred.
    fn in_flight(&self, now: Timestamp) -> bool {
        match &self.state {
            TaskState::Claimed { until, .. } => *until > now,
            TaskState::Deferred { .. } => true,
            _ => false,
        }
    }

    /// When the task became claimable, if it is claimable at `now`.
    fn due_at(&self, now: Timestamp) -> Option<Timestamp> {
        match &self.state {
            TaskState::Ready { next_attempt_at } if *next_attempt_at <= now => {
                Some(*next_attempt_at)
            }
            TaskState::Claimed { until, .. } if *until <= now => Some(*until),
            _ => None,
        }
    }

    fn mcp_server(&self) -> Option<&str> {
        match &self.route {
            ToolRoute::Mcp { server } => Some(server),
            _ => None,
        }
    }

    fn claimed_by(&self, owner: &str, now: Timestamp) -> bool {
        matches!(&self.state, TaskState::Claimed { owner: holder, until } if holder == owner && *until > now)
    }
}

fn matches_filter(filter: &TaskFilter, run: &Run, task: &Task) -> bool {
    match filter {
        TaskFilter::All => true,
        TaskFilter::Dispatched { tenants } => {
            task.route.is_dispatched() && tenants.contains(&run.record.run.tenant)
        }
        TaskFilter::Host {
            tenant,
            toolset,
            tools,
        } => {
            matches!(task.route, ToolRoute::Host { .. })
                && run.record.run.tenant == *tenant
                && toolset.is_none_or(|digest| digest == run.record.toolset)
                && (tools.is_empty() || tools.contains(&task.tool))
        }
    }
}

impl JournalStore for MemoryJournal {
    async fn put_script(
        &self,
        tenant: &TenantId,
        script: &Script,
        compiled: &CompiledScript,
    ) -> Result<(), StoreError> {
        self.lock()?
            .scripts
            .entry((tenant.clone(), script.digest()))
            .or_insert_with(|| StoredScript {
                script: script.clone(),
                compiled: compiled.clone(),
            });
        Ok(())
    }

    async fn get_script(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredScript>, StoreError> {
        Ok(self.lock()?.scripts.get(&(tenant.clone(), digest)).cloned())
    }

    async fn put_toolset(
        &self,
        tenant: &TenantId,
        toolset: &Toolset,
        declarations: &ToolDeclarations,
    ) -> Result<(), StoreError> {
        self.lock()?
            .toolsets
            .entry((tenant.clone(), toolset.digest()))
            .or_insert_with(|| StoredToolset {
                toolset: toolset.clone(),
                declarations: declarations.clone(),
            });
        Ok(())
    }

    async fn get_toolset(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredToolset>, StoreError> {
        Ok(self
            .lock()?
            .toolsets
            .get(&(tenant.clone(), digest))
            .cloned())
    }

    async fn list_toolsets(&self, tenant: &TenantId) -> Result<Vec<StoredToolset>, StoreError> {
        let mut toolsets: Vec<StoredToolset> = self
            .lock()?
            .toolsets
            .iter()
            .filter(|((owner, _), _)| owner == tenant)
            .map(|(_, stored)| stored.clone())
            .collect();
        toolsets.sort_by_key(|stored| stored.toolset.digest());
        Ok(toolsets)
    }

    async fn create_run(
        &self,
        run: NewRun,
        idempotency_key: Option<String>,
    ) -> Result<CreateRun, StoreError> {
        let mut state = self.lock()?;
        let tenant = run.run.tenant.clone();
        if let Some(key) = &idempotency_key
            && let Some(existing) = state.idempotency_keys.get(&(tenant.clone(), key.clone()))
        {
            return Ok(CreateRun::Created(Created {
                run: RunRef::new(tenant, *existing),
                created: false,
            }));
        }
        if state.runs.contains_key(&run.run.id) {
            return Err(StoreError::Conflict(format!(
                "run {} already exists",
                run.run.id
            )));
        }
        if let Some(quota) = state
            .tenants
            .get(&tenant)
            .and_then(|found| found.active_run_quota)
        {
            let active = state
                .runs
                .values()
                .filter(|existing| existing.record.run.tenant == tenant && !existing.is_terminal())
                .count();
            if active >= quota as usize {
                return Ok(CreateRun::QuotaExceeded { quota });
            }
        }
        let record = RunRecord {
            run: run.run.clone(),
            script: run.started.script,
            toolset: run.started.toolset,
            status: ptc_core::RunStatus::Pending,
            epoch: run.started.epoch,
            expires_at: run.started.expires_at,
            history_len: 0,
            quarantine: None,
            failures: 0,
            sticky_worker: None,
            metadata: run.metadata,
            idempotency_key: idempotency_key.clone(),
            created_at: run.created_at,
            updated_at: run.created_at,
            ended_at: None,
            archive: None,
        };
        state.runs.insert(
            run.run.id,
            Run {
                record,
                fence: 0,
                lease: None,
                expiry_enqueued: false,
                history: Vec::new(),
                inbox: Vec::new(),
                signal_keys: BTreeSet::new(),
                timers: BTreeMap::new(),
                tasks: BTreeMap::new(),
                webhook_tokens: BTreeMap::new(),
            },
        );
        if let Some(key) = idempotency_key {
            state.idempotency_keys.insert((tenant, key), run.run.id);
        }
        state.push_inbox(run.run.id, Trigger::RunStarted(run.started), run.created_at);
        Ok(CreateRun::Created(Created {
            run: run.run,
            created: true,
        }))
    }

    async fn enqueue(
        &self,
        run: &RunRef,
        completion: Completion,
        idempotency_key: Option<String>,
        now: Timestamp,
    ) -> Result<Enqueued, StoreError> {
        let mut state = self.lock()?;
        let found = state.run_mut(run)?;
        if found.is_terminal() {
            return Ok(Enqueued::RunEnded);
        }
        if let Some(key) = idempotency_key
            && !found.signal_keys.insert(key)
        {
            return Ok(Enqueued::Duplicate);
        }
        state.push_inbox(run.id, Trigger::Completion(completion), now);
        Ok(Enqueued::Accepted)
    }

    async fn get_run(&self, run: &RunRef) -> Result<Option<RunRecord>, StoreError> {
        let state = self.lock()?;
        Ok(state.run(run).ok().map(|found| found.record.clone()))
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<Vec<RunRecord>, StoreError> {
        let state = self.lock()?;
        let mut runs: Vec<RunRecord> = state
            .runs
            .values()
            .map(|run| &run.record)
            .filter(|record| record.run.tenant == filter.tenant)
            .filter(|record| filter.status.is_none_or(|status| record.status == status))
            .filter(|record| {
                filter
                    .quarantined
                    .is_none_or(|quarantined| record.quarantine.is_some() == quarantined)
            })
            .filter(|record| {
                filter
                    .labels
                    .iter()
                    .all(|(key, value)| record.metadata.labels.get(key) == Some(value))
            })
            .filter(|record| {
                filter
                    .after
                    .is_none_or(|after| RunCursor::from(*record) > after)
            })
            .cloned()
            .collect();
        runs.sort_by_key(|record| RunCursor::from(record));
        if let Some(limit) = filter.limit {
            runs.truncate(limit);
        }
        Ok(runs)
    }

    async fn acquire_runnable(
        &self,
        request: &AcquireRunnable<'_>,
    ) -> Result<Option<Lease>, StoreError> {
        let mut state = self.lock()?;
        let now = request.now;
        // (oldest inbox item id, its arrival, run, sticky worker)
        let eligible: Vec<(u64, Timestamp, RunRef, Option<String>)> = state
            .runs
            .values()
            .filter(|run| {
                !run.is_terminal()
                    && run.record.quarantine.is_none()
                    && !run.is_leased(now)
                    && request.epochs.contains(&run.record.epoch)
                    && request.tenants.contains(&run.record.run.tenant)
            })
            .filter_map(|run| {
                let oldest = run.inbox.first()?;
                Some((
                    oldest.id,
                    oldest.received_at,
                    run.record.run.clone(),
                    run.record.sticky_worker.clone(),
                ))
            })
            .collect();

        let mine = eligible
            .iter()
            .filter(|(.., sticky)| sticky.as_deref() == Some(request.owner))
            .min_by_key(|(oldest, ..)| *oldest);
        let chosen = match mine {
            Some((_, _, run, _)) => Some(run.clone()),
            None => {
                let open: Vec<&(u64, Timestamp, RunRef, Option<String>)> = eligible
                    .iter()
                    .filter(|(_, received_at, _, sticky)| {
                        sticky.is_none() || received_at.saturating_add(request.sticky_grace) <= now
                    })
                    .collect();
                let tenants: BTreeSet<&TenantId> =
                    open.iter().map(|(_, _, run, _)| &run.tenant).collect();
                let tenants: Vec<&TenantId> = tenants.into_iter().collect();
                if tenants.is_empty() {
                    None
                } else {
                    let tenant = tenants[(request.draw % tenants.len() as u64) as usize];
                    open.iter()
                        .filter(|(_, _, run, _)| &run.tenant == tenant)
                        .min_by_key(|(oldest, ..)| *oldest)
                        .map(|(_, _, run, _)| run.clone())
                }
            }
        };
        let Some(chosen) = chosen else {
            return Ok(None);
        };
        let run = state.run_mut(&chosen)?;
        Ok(Some(run.lease(request.owner, now, request.lease_duration)))
    }

    async fn acquire_run(
        &self,
        run: &RunRef,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        let mut state = self.lock()?;
        let found = state.run_mut(run)?;
        if found.is_terminal() || found.is_leased(now) {
            return Ok(None);
        }
        Ok(Some(found.lease(owner, now, lease_duration)))
    }

    async fn acquire_quarantined(
        &self,
        owner: &str,
        build: &str,
        epochs: &[Epoch],
        tenants: &TenantScope,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        let mut state = self.lock()?;
        let chosen = state
            .runs
            .values()
            .filter(|run| !run.is_terminal() && !run.is_leased(now))
            .filter(|run| epochs.contains(&run.record.epoch))
            .filter(|run| tenants.contains(&run.record.run.tenant))
            .filter_map(|run| {
                let quarantine = run.record.quarantine.as_ref()?;
                (quarantine.retried_by.as_deref() != Some(build))
                    .then_some((quarantine.since, run.record.run.clone()))
            })
            .min();
        let Some((_, chosen)) = chosen else {
            return Ok(None);
        };
        let run = state.run_mut(&chosen)?;
        if let Some(quarantine) = &mut run.record.quarantine {
            quarantine.retried_by = Some(build.to_owned());
        }
        Ok(Some(run.lease(owner, now, lease_duration)))
    }

    async fn renew(
        &self,
        lease: &Lease,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        let run = state.leased(lease)?;
        let until = now.saturating_add(lease_duration);
        match &mut run.lease {
            Some(held) => held.until = until,
            None => {
                return Err(StoreError::Fenced {
                    run_id: lease.run.id,
                    fence: lease.fence,
                });
            }
        }
        Ok(())
    }

    async fn load_history(&self, run: &RunRef, from: u64) -> Result<Vec<HistoryEntry>, StoreError> {
        let state = self.lock()?;
        let found = state.run(run)?;
        if found.record.archive.is_some() {
            return Err(StoreError::Archived(run.id));
        }
        let from = usize::try_from(from)
            .unwrap_or(usize::MAX)
            .min(found.history.len());
        Ok(found.history[from..].to_vec())
    }

    async fn take_inbox(&self, lease: &Lease) -> Result<Vec<InboxItem>, StoreError> {
        let mut state = self.lock()?;
        Ok(state.leased(lease)?.inbox.clone())
    }

    async fn commit_activation(
        &self,
        lease: &Lease,
        activation: Activation,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        state.leased(lease)?;
        // Validate everything before writing anything: the commit is all or nothing.
        state.check_reserved(&lease.run.tenant, &activation.payloads)?;
        let run = state.leased(lease)?;
        let next = run.history.len() as u64;
        for (offset, entry) in activation.entries.iter().enumerate() {
            if entry.index != next + offset as u64 {
                return Err(StoreError::Conflict(format!(
                    "entry index {} does not continue the history of run {} (next index {})",
                    entry.index,
                    lease.run.id,
                    next + offset as u64
                )));
            }
        }

        let owner = run.lease.as_ref().map(|held| held.owner.clone());
        let from_idx = next;
        if let Some(last) = activation.entries.last() {
            run.record.updated_at = last.recorded_at;
            if activation.status.is_terminal() {
                run.record.ended_at = Some(last.recorded_at);
            }
        }
        let has_entries = !activation.entries.is_empty();
        run.history.extend(activation.entries);
        run.record.history_len = run.history.len() as u64;
        run.record.status = activation.status;
        run.record.failures = 0;
        if owner.is_some() {
            run.record.sticky_worker = owner;
        }
        let consumed: BTreeSet<u64> = activation.consumed.into_iter().collect();
        run.inbox.retain(|item| !consumed.contains(&item.id));
        for task in activation.tool_calls {
            let state = if task.policy.approval {
                TaskState::Approval
            } else {
                TaskState::Ready {
                    next_attempt_at: now,
                }
            };
            run.tasks.insert(
                task.seq,
                Task {
                    tool: task.tool,
                    route: task.route,
                    args: task.args,
                    policy: task.policy,
                    server_concurrency: task.server_concurrency,
                    state,
                    attempt: 0,
                    schema_change_approved: false,
                    token: None,
                    created_at: now,
                },
            );
        }
        for timer in activation.timers {
            run.timers.insert(timer.seq, timer.fire_at);
        }
        for seq in activation.cancelled_timers {
            run.timers.remove(&seq);
        }
        let to_idx = run.record.history_len;
        let terminal = activation.status.is_terminal();

        for payload in activation.payloads {
            let key = (lease.run.tenant.clone(), payload.digest);
            let stored = state.payloads.entry(key).or_insert_with(|| StoredPayload {
                record: payload.clone(),
                refs: BTreeSet::new(),
                unreferenced_since: None,
                deletion_started: None,
            });
            if stored.record.value.is_none() && payload.value.is_some() {
                stored.record.value = payload.value;
            }
            stored.refs.insert(lease.run.id);
            stored.unreferenced_since = None;
            // A payload of the blob tier was checked above: only a value stops a deletion.
            stored.deletion_started = None;
        }
        if has_entries {
            for consumer in FeedConsumer::ALL {
                let id = state.next_feed_id;
                state.next_feed_id += 1;
                state.feed.insert(
                    id,
                    FeedRow {
                        item: FeedItem {
                            id,
                            consumer,
                            run: lease.run.clone(),
                            fence: lease.fence,
                            from_idx,
                            to_idx,
                        },
                        claimed_until: None,
                    },
                );
            }
        }
        if terminal {
            state.end_run(lease.run.id);
        }
        Ok(())
    }

    async fn release(&self, lease: &Lease) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        if let Ok(run) = state.leased(lease) {
            run.lease = None;
        }
        Ok(())
    }

    async fn forget_sticky(&self, run: &RunRef, owner: &str) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        if let Ok(found) = state.run_mut(run)
            && found.record.sticky_worker.as_deref() == Some(owner)
        {
            found.record.sticky_worker = None;
        }
        Ok(())
    }

    async fn record_failure(
        &self,
        lease: &Lease,
        error: String,
        now: Timestamp,
    ) -> Result<Failures, StoreError> {
        let mut state = self.lock()?;
        let run = state.leased(lease)?;
        run.record.failures += 1;
        let count = run.record.failures;
        let quarantined = count >= QUARANTINE_AFTER_FAILURES;
        if quarantined {
            let retried_by = run
                .record
                .quarantine
                .take()
                .and_then(|previous| previous.retried_by);
            run.record.quarantine = Some(Quarantine {
                kind: QuarantineKind::Failures,
                reason: error,
                since: now,
                retried_by,
            });
            run.lease = None;
        }
        Ok(Failures { count, quarantined })
    }

    async fn quarantine(
        &self,
        lease: &Lease,
        mismatch: String,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        let run = state.leased(lease)?;
        let retried_by = run
            .record
            .quarantine
            .take()
            .and_then(|previous| previous.retried_by);
        run.record.quarantine = Some(Quarantine {
            kind: QuarantineKind::NonDeterminism,
            reason: mismatch,
            since: now,
            retried_by,
        });
        run.lease = None;
        Ok(())
    }

    async fn release_quarantine(&self, run: &RunRef) -> Result<bool, StoreError> {
        let mut state = self.lock()?;
        let found = state.run_mut(run)?;
        found.record.failures = 0;
        Ok(found.record.quarantine.take().is_some())
    }

    async fn fire_due_timers(&self, now: Timestamp, limit: usize) -> Result<usize, StoreError> {
        let mut state = self.lock()?;
        let mut due: Vec<(Timestamp, RunId, Seq)> = state
            .runs
            .values()
            .flat_map(|run| {
                run.timers
                    .iter()
                    .filter(|(_, fire_at)| **fire_at <= now)
                    .map(|(seq, fire_at)| (*fire_at, run.record.run.id, *seq))
            })
            .collect();
        due.sort();
        due.truncate(limit);
        for (_, run_id, seq) in &due {
            if let Some(run) = state.runs.get_mut(run_id) {
                run.timers.remove(seq);
            }
            state.push_inbox(
                *run_id,
                Trigger::Completion(Completion::TimerFired { seq: *seq }),
                now,
            );
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

    async fn expire_due_runs(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<ExpiredRuns, StoreError> {
        let mut state = self.lock()?;
        let mut due: Vec<(Timestamp, RunId)> = state
            .runs
            .values()
            .filter(|run| !run.is_terminal() && run.record.expires_at <= now)
            .filter(|run| run.record.quarantine.is_some() || !run.expiry_enqueued)
            .map(|run| (run.record.expires_at, run.record.run.id))
            .collect();
        due.sort();
        due.truncate(limit);
        let mut expired = ExpiredRuns::default();
        for (_, run_id) in due {
            let Some(run) = state.runs.get_mut(&run_id) else {
                continue;
            };
            let run_ref = run.record.run.clone();
            if run.record.quarantine.is_some() {
                expired.quarantined.push(run_ref);
            } else {
                run.expiry_enqueued = true;
                let cancel = Completion::CancelRequested {
                    reason: CancelReason::Ttl,
                };
                state.push_inbox(run_id, Trigger::Completion(cancel), now);
                expired.cancelled.push(run_ref);
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

    async fn next_tool_attempt(&self, after: Timestamp) -> Result<Option<Timestamp>, StoreError> {
        Ok(self
            .lock()?
            .runs
            .values()
            .flat_map(|run| run.tasks.values())
            .filter_map(|task| match task.state {
                TaskState::Ready { next_attempt_at } if next_attempt_at > after => {
                    Some(next_attempt_at)
                }
                _ => None,
            })
            .min())
    }

    async fn claim_tool_tasks(
        &self,
        owner: &str,
        filter: &TaskFilter,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<ClaimedTask>, StoreError> {
        let mut state = self.lock()?;

        // Calls in flight, per (run, tool) and per (tenant, MCP server) under this owner.
        let mut per_tool: HashMap<(RunId, String), u32> = HashMap::new();
        let mut per_server: HashMap<(TenantId, String), u32> = HashMap::new();
        let mut due: Vec<(Timestamp, RunId, Seq)> = Vec::new();
        for run in state.runs.values() {
            for (seq, task) in &run.tasks {
                if task.in_flight(now) {
                    *per_tool
                        .entry((run.record.run.id, task.tool.clone()))
                        .or_default() += 1;
                }
                if task.claimed_by(owner, now)
                    && let Some(server) = task.mcp_server()
                {
                    *per_server
                        .entry((run.record.run.tenant.clone(), server.to_owned()))
                        .or_default() += 1;
                }
                if let Some(due_at) = task.due_at(now)
                    && !run.is_terminal()
                    && matches_filter(filter, run, task)
                {
                    due.push((due_at, run.record.run.id, *seq));
                }
            }
        }
        due.sort();

        let mut claimed = Vec::new();
        for (_, run_id, seq) in due {
            if claimed.len() >= limit {
                break;
            }
            let Some(run) = state.runs.get_mut(&run_id) else {
                continue;
            };
            let tenant = run.record.run.tenant.clone();
            let Some(task) = run.tasks.get_mut(&seq) else {
                continue;
            };
            let tool_key = (run_id, task.tool.clone());
            if per_tool.get(&tool_key).copied().unwrap_or(0) >= task.policy.concurrency {
                continue;
            }
            let server_key = task
                .mcp_server()
                .map(|server| (tenant.clone(), server.to_owned()));
            if let (Some(key), Some(max)) = (&server_key, task.server_concurrency)
                && per_server.get(key).copied().unwrap_or(0) >= max
            {
                continue;
            }
            *per_tool.entry(tool_key).or_default() += 1;
            if let Some(key) = server_key {
                *per_server.entry(key).or_default() += 1;
            }

            let previous_claim_expired = matches!(task.state, TaskState::Claimed { .. });
            let token = random_token("ptc_tt_");
            task.attempt += 1;
            task.token = Some(token.clone());
            task.state = TaskState::Claimed {
                owner: owner.to_owned(),
                until: now.saturating_add(task.policy.heartbeat_timeout),
            };
            let task = &run.tasks[&seq];
            claimed.push(ClaimedTask {
                task: run.view(seq, task),
                token: TaskToken(token),
                previous_claim_expired,
            });
        }
        Ok(claimed)
    }

    async fn heartbeat_tool_task(
        &self,
        token: &TaskToken,
        now: Timestamp,
    ) -> Result<Option<Timestamp>, StoreError> {
        let mut state = self.lock()?;
        let Some((run_id, seq)) = state.task_by_token(token) else {
            return Ok(None);
        };
        let Some(task) = state
            .runs
            .get_mut(&run_id)
            .and_then(|run| run.tasks.get_mut(&seq))
        else {
            return Ok(None);
        };
        let timeout = task.policy.heartbeat_timeout;
        Ok(match &mut task.state {
            TaskState::Claimed { until, .. } => {
                *until = now.saturating_add(timeout);
                Some(*until)
            }
            TaskState::Deferred { until } => Some(*until),
            _ => None,
        })
    }

    async fn retry_tool_task(
        &self,
        token: &TaskToken,
        next_attempt_at: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let mut state = self.lock()?;
        update_by_token(&mut state, token, |task| match task.state {
            TaskState::Claimed { .. } => {
                task.state = TaskState::Ready { next_attempt_at };
                task.token = None;
                TaskUpdate::Applied
            }
            ref other => TaskUpdate::InvalidState(other.clone()),
        })
    }

    async fn defer_tool_task(
        &self,
        token: &TaskToken,
        until: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let mut state = self.lock()?;
        update_by_token(&mut state, token, |task| match task.state {
            TaskState::Claimed { .. } => {
                task.state = TaskState::Deferred { until };
                TaskUpdate::Applied
            }
            ref other => TaskUpdate::InvalidState(other.clone()),
        })
    }

    async fn park_tool_task(
        &self,
        token: &TaskToken,
        reason: ParkReason,
    ) -> Result<TaskUpdate, StoreError> {
        let mut state = self.lock()?;
        update_by_token(&mut state, token, |task| match task.state {
            TaskState::Claimed { .. } => {
                task.state = TaskState::Parked(reason);
                task.token = None;
                TaskUpdate::Applied
            }
            ref other => TaskUpdate::InvalidState(other.clone()),
        })
    }

    async fn complete_tool_task(
        &self,
        token: &TaskToken,
        outcome: ToolOutcome,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let mut state = self.lock()?;
        let Some((run_id, seq)) = state.task_by_token(token) else {
            return Ok(TaskUpdate::NotFound);
        };
        state.finish_task(run_id, seq, outcome, now);
        Ok(TaskUpdate::Applied)
    }

    async fn approve_tool_task(
        &self,
        task: &TaskRef,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let mut state = self.lock()?;
        let Ok(run) = state.run_mut(&task.run) else {
            return Ok(TaskUpdate::NotFound);
        };
        let Some(found) = run.tasks.get_mut(&task.seq) else {
            return Ok(TaskUpdate::NotFound);
        };
        Ok(match &found.state {
            TaskState::Approval | TaskState::Parked(_) => {
                if matches!(&found.state, TaskState::Parked(reason) if reason.kind == ParkKind::SchemaChanged)
                {
                    found.schema_change_approved = true;
                }
                found.state = TaskState::Ready {
                    next_attempt_at: now,
                };
                TaskUpdate::Applied
            }
            other => TaskUpdate::InvalidState(other.clone()),
        })
    }

    async fn reject_tool_task(
        &self,
        task: &TaskRef,
        failure: ToolFailure,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let mut state = self.lock()?;
        let Ok(run) = state.run_mut(&task.run) else {
            return Ok(TaskUpdate::NotFound);
        };
        let Some(found) = run.tasks.get(&task.seq) else {
            return Ok(TaskUpdate::NotFound);
        };
        match &found.state {
            TaskState::Approval | TaskState::Parked(_) => {
                state.finish_task(task.run.id, task.seq, ToolOutcome::Failure(failure), now);
                Ok(TaskUpdate::Applied)
            }
            other => Ok(TaskUpdate::InvalidState(other.clone())),
        }
    }

    async fn resume_parked(
        &self,
        scope: &ParkedScope,
        now: Timestamp,
    ) -> Result<usize, StoreError> {
        let mut state = self.lock()?;
        let mut resumed = 0;
        for run in state.runs.values_mut() {
            let in_scope = match scope {
                ParkedScope::Run(target) => run.record.run == *target,
                ParkedScope::Toolset { tenant, toolset } => {
                    run.record.run.tenant == *tenant && run.record.toolset == *toolset
                }
            };
            if !in_scope {
                continue;
            }
            for task in run.tasks.values_mut() {
                if matches!(&task.state, TaskState::Parked(reason) if reason.kind == ParkKind::CredentialsExpired)
                {
                    task.state = TaskState::Ready {
                        next_attempt_at: now,
                    };
                    resumed += 1;
                }
            }
        }
        Ok(resumed)
    }

    async fn expire_deferred_tool_tasks(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<usize, StoreError> {
        let mut state = self.lock()?;
        let mut due: Vec<(Timestamp, RunId, Seq)> = state
            .runs
            .values()
            .flat_map(|run| {
                run.tasks.iter().filter_map(|(seq, task)| match task.state {
                    TaskState::Deferred { until } if until <= now => {
                        Some((until, run.record.run.id, *seq))
                    }
                    _ => None,
                })
            })
            .collect();
        due.sort();
        due.truncate(limit);
        for (_, run_id, seq) in &due {
            let failure = ToolOutcome::Failure(deferred_timeout_failure());
            state.finish_task(*run_id, *seq, failure, now);
        }
        Ok(due.len())
    }

    async fn tool_tasks(&self, run: &RunRef) -> Result<Vec<ToolTask>, StoreError> {
        let state = self.lock()?;
        let found = state.run(run)?;
        Ok(found
            .tasks
            .iter()
            .map(|(seq, task)| found.view(*seq, task))
            .collect())
    }

    async fn tool_task(&self, token: &TaskToken) -> Result<Option<ToolTask>, StoreError> {
        let mut state = self.lock()?;
        let Some((run_id, seq)) = state.task_by_token(token) else {
            return Ok(None);
        };
        Ok(state
            .runs
            .get(&run_id)
            .and_then(|run| run.tasks.get(&seq).map(|task| run.view(seq, task))))
    }

    async fn create_webhook_token(
        &self,
        run: &RunRef,
        name: &str,
    ) -> Result<WebhookToken, StoreError> {
        let mut state = self.lock()?;
        let found = state.run_mut(run)?;
        if found.is_terminal() {
            return Err(StoreError::Conflict(format!("run {} has ended", run.id)));
        }
        if let Some(token) = found.webhook_tokens.get(name) {
            return Ok(WebhookToken(token.clone()));
        }
        let token = random_token("ptc_wh_");
        found.webhook_tokens.insert(name.to_owned(), token.clone());
        state
            .webhook_tokens
            .insert(token.clone(), (run.id, name.to_owned()));
        Ok(WebhookToken(token))
    }

    async fn resolve_webhook_token(
        &self,
        token: &WebhookToken,
    ) -> Result<Option<WebhookTarget>, StoreError> {
        let state = self.lock()?;
        let Some((run_id, name)) = state.webhook_tokens.get(&token.0) else {
            return Ok(None);
        };
        Ok(state
            .runs
            .get(run_id)
            .filter(|run| !run.is_terminal())
            .map(|run| WebhookTarget {
                run: run.record.run.clone(),
                name: name.clone(),
            }))
    }

    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), StoreError> {
        self.lock()?.subscriptions.insert(
            (subscription.tenant.clone(), subscription.id.clone()),
            subscription.clone(),
        );
        Ok(())
    }

    async fn subscriptions(&self, tenant: &TenantId) -> Result<Vec<Subscription>, StoreError> {
        Ok(self
            .lock()?
            .subscriptions
            .values()
            .filter(|subscription| subscription.tenant == *tenant)
            .cloned()
            .collect())
    }

    async fn delete_subscription(&self, tenant: &TenantId, id: &str) -> Result<bool, StoreError> {
        Ok(self
            .lock()?
            .subscriptions
            .remove(&(tenant.clone(), id.to_owned()))
            .is_some())
    }

    async fn get_payload(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<PayloadRecord>, StoreError> {
        Ok(self
            .lock()?
            .payloads
            .get(&(tenant.clone(), digest))
            .map(|stored| stored.record.clone()))
    }

    async fn reserve_payloads(
        &self,
        tenant: &TenantId,
        payloads: &[PayloadRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, StoreError> {
        let mut state = self.lock()?;
        let mut deleting = Vec::new();
        for payload in payloads {
            let key = (tenant.clone(), payload.digest);
            match state.payloads.get_mut(&key) {
                Some(stored) if stored.deletion_started.is_some() => {
                    deleting.push(payload.digest);
                }
                Some(stored) => {
                    if stored.refs.is_empty() {
                        stored.unreferenced_since = Some(
                            stored
                                .unreferenced_since
                                .map_or(now, |since| since.max(now)),
                        );
                    }
                }
                None => {
                    state.payloads.insert(
                        key,
                        StoredPayload {
                            record: payload.clone(),
                            refs: BTreeSet::new(),
                            unreferenced_since: Some(now),
                            deletion_started: None,
                        },
                    );
                }
            }
        }
        Ok(deleting)
    }

    async fn unreferenced_payloads(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        let state = self.lock()?;
        Ok(state
            .payloads
            .iter()
            .filter(|((tenant, _), stored)| deletable(&state, tenant, stored, now))
            .map(|(key, _)| key.clone())
            .take(limit)
            .collect())
    }

    async fn forget_payloads(
        &self,
        payloads: &[(TenantId, Digest)],
        now: Timestamp,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        let mut state = self.lock()?;
        let forgettable: Vec<(TenantId, Digest)> = payloads
            .iter()
            .filter(|key| {
                state
                    .payloads
                    .get(*key)
                    .is_some_and(|stored| deletable(&state, &key.0, stored, now))
            })
            .cloned()
            .collect();
        for key in &forgettable {
            if let Some(stored) = state.payloads.get_mut(key) {
                stored.deletion_started = Some(now);
            }
        }
        Ok(forgettable)
    }

    async fn drop_payloads(
        &self,
        payloads: &[(TenantId, Digest)],
        started: Timestamp,
    ) -> Result<usize, StoreError> {
        let mut state = self.lock()?;
        let mut dropped = 0;
        for key in payloads {
            let deleted = state.payloads.get(key).is_some_and(|stored| {
                stored.refs.is_empty() && stored.deletion_started == Some(started)
            });
            if deleted {
                state.payloads.remove(key);
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    async fn runs_to_archive(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<RunRef>, StoreError> {
        let state = self.lock()?;
        let mut due: Vec<(Timestamp, RunRef)> = state
            .runs
            .values()
            .filter(|run| run.is_terminal() && run.record.archive.is_none())
            .filter(|run| !state.has_feed_items(run.record.run.id))
            .filter_map(|run| {
                let ended_at = run.record.ended_at?;
                let retention = state.retention(&run.record.run.tenant);
                (ended_at.saturating_add(retention.journal) <= now)
                    .then(|| (ended_at, run.record.run.clone()))
            })
            .collect();
        due.sort();
        Ok(due.into_iter().take(limit).map(|(_, run)| run).collect())
    }

    async fn archive_run(
        &self,
        run: &RunRef,
        document: Digest,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        let mut state = self.lock()?;
        let pending = state.has_feed_items(run.id);
        let Ok(found) = state.run_mut(run) else {
            return Ok(false);
        };
        if !found.is_terminal() || found.record.archive.is_some() || pending {
            return Ok(false);
        }
        found.history = Vec::new();
        found.inbox.clear();
        found.record.archive = Some(Archive {
            document,
            archived_at: now,
        });
        Ok(true)
    }

    async fn runs_to_delete(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<RunRef>, StoreError> {
        let state = self.lock()?;
        let mut due: Vec<(Timestamp, RunRef)> = state
            .runs
            .values()
            .filter_map(|run| {
                let archive = run.record.archive?;
                let retention = state.retention(&run.record.run.tenant);
                (archive.archived_at.saturating_add(retention.archive) <= now)
                    .then(|| (archive.archived_at, run.record.run.clone()))
            })
            .collect();
        due.sort();
        Ok(due.into_iter().take(limit).map(|(_, run)| run).collect())
    }

    async fn delete_run(&self, run: &RunRef, now: Timestamp) -> Result<bool, StoreError> {
        let mut state = self.lock()?;
        if state.run(run).is_err() {
            return Ok(false);
        }
        state.end_run(run.id);
        let Some(removed) = state.runs.remove(&run.id) else {
            return Ok(false);
        };
        if let Some(key) = removed.record.idempotency_key {
            state.idempotency_keys.remove(&(run.tenant.clone(), key));
        }
        state.feed.retain(|_, row| row.item.run.id != run.id);
        for ((tenant, _), stored) in state.payloads.iter_mut() {
            if *tenant == run.tenant && stored.refs.remove(&run.id) && stored.refs.is_empty() {
                stored.unreferenced_since = Some(now);
            }
        }
        Ok(true)
    }

    async fn put_tenant(&self, tenant: &Tenant) -> Result<(), StoreError> {
        self.lock()?
            .tenants
            .insert(tenant.id.clone(), tenant.clone());
        Ok(())
    }

    async fn get_tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StoreError> {
        Ok(self.lock()?.tenants.get(id).cloned())
    }

    async fn list_tenants(&self) -> Result<Vec<Tenant>, StoreError> {
        Ok(self.lock()?.tenants.values().cloned().collect())
    }

    async fn put_api_key(&self, key: &ApiKey) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        if state.api_keys.contains_key(&key.hash) {
            return Err(StoreError::Conflict(format!(
                "an API key with hash {} already exists",
                key.hash.short()
            )));
        }
        state.api_keys.insert(key.hash, key.clone());
        Ok(())
    }

    async fn find_api_key(&self, hash: Digest) -> Result<Option<ApiKey>, StoreError> {
        Ok(self.lock()?.api_keys.get(&hash).cloned())
    }

    async fn revoke_api_key(&self, hash: Digest, now: Timestamp) -> Result<bool, StoreError> {
        let mut state = self.lock()?;
        let Some(key) = state.api_keys.get_mut(&hash) else {
            return Ok(false);
        };
        if key.revoked_at.is_some() {
            return Ok(false);
        }
        key.revoked_at = Some(now);
        Ok(true)
    }

    async fn claim_feed(
        &self,
        consumer: FeedConsumer,
        _owner: &str,
        now: Timestamp,
        lease_duration: Duration,
        limit: usize,
    ) -> Result<Vec<FeedItem>, StoreError> {
        let mut state = self.lock()?;
        let mut claimed = Vec::new();
        for row in state.feed.values_mut() {
            if claimed.len() >= limit {
                break;
            }
            if row.item.consumer != consumer || row.claimed_until.is_some_and(|until| until > now) {
                continue;
            }
            row.claimed_until = Some(now.saturating_add(lease_duration));
            claimed.push(row.item.clone());
        }
        Ok(claimed)
    }

    async fn ack_feed(&self, ids: &[u64]) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        for id in ids {
            state.feed.remove(id);
        }
        Ok(())
    }
}

/// The conflict of a commit referencing a blob that may be gone.
fn unreserved(digest: Digest) -> StoreError {
    StoreError::Conflict(format!(
        "payload {} has no reserved row, or is being deleted: its blob may be gone",
        digest.short()
    ))
}

/// Applies `update` to the task holding `token`.
fn update_by_token(
    state: &mut State,
    token: &TaskToken,
    update: impl FnOnce(&mut Task) -> TaskUpdate,
) -> Result<TaskUpdate, StoreError> {
    let Some((run_id, seq)) = state.task_by_token(token) else {
        return Ok(TaskUpdate::NotFound);
    };
    Ok(state
        .runs
        .get_mut(&run_id)
        .and_then(|run| run.tasks.get_mut(&seq))
        .map_or(TaskUpdate::NotFound, update))
}

/// Whether cleanup may start deleting the payload at `now`: no run has referenced it for its
/// grace, and its deletion has not started, or started too long ago to be still going on.
fn deletable(state: &State, tenant: &TenantId, stored: &StoredPayload, now: Timestamp) -> bool {
    stored.refs.is_empty()
        && stored
            .unreferenced_since
            .is_some_and(|since| since.saturating_add(state.retention(tenant).payload_grace) <= now)
        && stored
            .deletion_started
            .is_none_or(|started| started.saturating_add(PAYLOAD_DELETION_TIMEOUT) <= now)
}
