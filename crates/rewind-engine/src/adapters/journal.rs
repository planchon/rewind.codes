use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rewind_core::{
    CancelReason, Completion, Digest, Epoch, HistoryEntry, Interruption, Payload, RunId, Script,
    Seq, Timestamp, ToolFailure, ToolOutcome, ToolPolicy, ToolRoute, Toolset, Workflow,
    end_signal_name,
};

use crate::adapters::random_token;
use crate::ports::{
    AcquireRunnable, Activation, Archive, ClaimedTask, CompiledScript, CreateRun, Created,
    Enqueued, ExpiredRuns, Failures, FeedConsumer, FeedItem, InboxItem, JournalStore, Lease,
    NewRun, PAYLOAD_DELETION_TIMEOUT, ParkKind, ParkReason, ParkedScope, PayloadRecord,
    QUARANTINE_AFTER_FAILURES, Quarantine, QuarantineKind, RunCursor, RunFilter, RunRecord, RunRef,
    SecretId, SecretUsage, StoreError, StoredScript, StoredToolset, StoredWorkflow, Subscription,
    TEST_GRACE, TaskFilter, TaskRef, TaskState, TaskToken, TaskUpdate, Tenant, TenantId,
    TenantScope, TestRecord, ToolDeclarations, ToolTask, Trigger, Watched, WebhookTarget,
    WebhookToken, WorkflowFilter, deferred_timeout_failure,
};
use crate::ports::{
    Day, DayUsage, DeleteNamespace, DisplayName, DropNamespace, Month, Namespace, NamespaceFilter,
    NamespaceName, NamespaceOrder, NamespacePage, NamespaceUsage, NamespaceWrite, QuotaAlert,
    QuotaAlertKey, QuotaExceeded, QuotaKind, QuotaScope, Quotas, Threshold, Usage, UsageDelta,
    UsageOf,
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
    idempotency_keys: HashMap<(TenantId, NamespaceName, String), RunId>,
    tenants: BTreeMap<TenantId, Tenant>,
    namespaces: BTreeMap<(TenantId, NamespaceName), Namespace>,
    /// Use per day; `None` is the tenant's own row.
    usage: BTreeMap<(TenantId, Option<NamespaceName>, Day), DayUsage>,
    /// Runs kept per script, per namespace.
    namespace_scripts: BTreeMap<(TenantId, NamespaceName, Digest), u64>,
    alerts: BTreeMap<QuotaAlertKey, AlertRow>,
    subscriptions: BTreeMap<(TenantId, String), Subscription>,
    payloads: BTreeMap<(TenantId, Digest), StoredPayload>,
    /// Token → (run, signal name).
    webhook_tokens: HashMap<String, (RunId, String)>,
    feed: BTreeMap<u64, FeedRow>,
    next_inbox_id: u64,
    next_feed_id: u64,
    /// Workflow versions, with the order they were stored in.
    workflows: BTreeMap<(TenantId, Digest), (u64, StoredWorkflow)>,
    /// The index of the tests whose documents are blobs.
    tests: BTreeMap<(TenantId, Digest), StoredTest>,
    /// (tenant, test, version): the tests of each version's suite.
    workflow_tests: BTreeSet<(TenantId, Digest, Digest)>,
    next_workflow: u64,
    /// Run → its watchers: watcher → (`seq` of the start, attached).
    watchers: BTreeMap<RunId, BTreeMap<RunId, (Seq, bool)>>,
}

#[derive(Debug, Clone)]
struct StoredTest {
    record: TestRecord,
    /// When it was stored or reserved last: its grace starts there.
    reserved_at: Timestamp,
    deletion_started: Option<Timestamp>,
}

impl State {
    fn test_referenced(&self, tenant: &TenantId, digest: Digest) -> bool {
        self.workflow_tests
            .range((tenant.clone(), digest, Digest::from_bytes([0; 32]))..)
            .next()
            .is_some_and(|(owner, test, _)| owner == tenant && *test == digest)
    }
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
    args: rewind_core::Payload,
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
struct AlertRow {
    alert: QuotaAlert,
    claimed_until: Option<Timestamp>,
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
    fn tenant(&self, tenant: &TenantId) -> Tenant {
        self.tenants
            .get(tenant)
            .cloned()
            .unwrap_or_else(|| Tenant::new(tenant.clone()))
    }

    /// The namespace's limits: its exceptions over the tenant's defaults.
    fn limits(&self, tenant: &TenantId, namespace: &NamespaceName) -> Quotas {
        let record = self.tenant(tenant);
        match self.namespaces.get(&(tenant.clone(), namespace.clone())) {
            Some(found) => found.exceptions.over(&record.namespace_defaults),
            None => record.namespace_defaults,
        }
    }

    /// CPU time and runs created in a month, of a namespace or of the tenant (`None`).
    fn month_usage(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        month: Month,
    ) -> DayUsage {
        let mut total = DayUsage::default();
        for ((owner, of, day), usage) in &self.usage {
            if owner == tenant && of.as_ref() == namespace && month.contains(*day) {
                total.cpu += usage.cpu;
                total.runs_started += usage.runs_started;
            }
        }
        total
    }

    fn active_runs(&self, tenant: &TenantId, namespace: Option<&NamespaceName>) -> u64 {
        self.runs
            .values()
            .filter(|run| {
                run.record.run.tenant == *tenant
                    && namespace.is_none_or(|name| run.record.namespace == *name)
                    && !run.is_terminal()
            })
            .count() as u64
    }

    /// Distinct scripts with a run kept: of a namespace, or summed over the tenant's.
    fn scripts(&self, tenant: &TenantId, namespace: Option<&NamespaceName>) -> u64 {
        self.namespace_scripts
            .keys()
            .filter(|(owner, name, _)| owner == tenant && namespace.is_none_or(|n| name == n))
            .count() as u64
    }

    fn usage_of(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        month: Month,
        days: bool,
    ) -> Usage {
        let total = self.month_usage(tenant, namespace, month);
        Usage {
            cpu: total.cpu,
            runs_started: total.runs_started,
            active_runs: self.active_runs(tenant, namespace),
            scripts: self.scripts(tenant, namespace),
            days: if days {
                self.usage
                    .iter()
                    .filter(|((owner, of, day), _)| {
                        owner == tenant && of.as_ref() == namespace && month.contains(*day)
                    })
                    .map(|((_, _, day), usage)| (*day, usage.clone()))
                    .collect()
            } else {
                Vec::new()
            },
        }
    }

    /// Records an alert unless one with this key exists. Returns it when recorded.
    fn raise(
        &mut self,
        key: QuotaAlertKey,
        used: u64,
        limit: u64,
        now: Timestamp,
    ) -> Option<QuotaAlert> {
        if self.alerts.contains_key(&key) {
            return None;
        }
        let alert = QuotaAlert {
            key: key.clone(),
            used,
            limit,
            raised_at: now,
            sent_at: None,
        };
        self.alerts.insert(
            key,
            AlertRow {
                alert: alert.clone(),
                claimed_until: None,
            },
        );
        Some(alert)
    }

    /// Records the alerts of the thresholds a use crosses from `before` to `after`.
    #[allow(clippy::too_many_arguments)]
    fn raise_crossed(
        &mut self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        quota: QuotaKind,
        month: Month,
        (before, after): (u64, u64),
        limit: Option<u64>,
        now: Timestamp,
    ) -> Vec<QuotaAlert> {
        let Some(limit) = limit else {
            return Vec::new();
        };
        Threshold::crossed(before, after, limit)
            .into_iter()
            .filter_map(|threshold| {
                self.raise(
                    QuotaAlertKey {
                        tenant: tenant.clone(),
                        namespace: namespace.cloned(),
                        quota,
                        period: month,
                        threshold,
                    },
                    after,
                    limit,
                    now,
                )
            })
            .collect()
    }

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

    /// Enqueues a completion unless the run has ended or the key was seen.
    fn enqueue(
        &mut self,
        run_id: RunId,
        completion: Completion,
        idempotency_key: Option<String>,
        now: Timestamp,
    ) -> Enqueued {
        let Some(run) = self.runs.get_mut(&run_id) else {
            return Enqueued::RunEnded;
        };
        if run.is_terminal() {
            return Enqueued::RunEnded;
        }
        if let Some(key) = idempotency_key
            && !run.signal_keys.insert(key)
        {
            return Enqueued::Duplicate;
        }
        self.push_inbox(run_id, Trigger::Completion(completion), now);
        Enqueued::Accepted
    }

    /// What a run's end does to the runs around it: its end to each of its watchers, and a
    /// cancellation to each run it watches attached.
    fn propagate_end(
        &mut self,
        run_id: RunId,
        end: Option<&rewind_core::JsonValue>,
        now: Timestamp,
    ) {
        let name = end_signal_name(run_id);
        let watchers: Vec<RunId> = self
            .watchers
            .get(&run_id)
            .map(|watchers| watchers.keys().copied().collect())
            .unwrap_or_default();
        if let Some(end) = end {
            for watcher in watchers {
                let completion = Completion::SignalReceived {
                    name: name.clone(),
                    payload: Payload::Value(end.clone()),
                };
                self.enqueue(watcher, completion, Some(name.clone()), now);
            }
        }
        let attached: Vec<RunId> = self
            .watchers
            .iter()
            .filter(|(_, watchers)| matches!(watchers.get(&run_id), Some((_, true))))
            .map(|(child, _)| *child)
            .collect();
        for child in attached {
            let completion = Completion::CancelRequested {
                reason: rewind_core::starter_ended(run_id),
            };
            self.enqueue(child, completion, None, now);
        }
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
            namespace,
            labels,
            toolset,
            tools,
        } => {
            matches!(task.route, ToolRoute::Host { .. })
                && run.record.run.tenant == *tenant
                && namespace
                    .as_ref()
                    .is_none_or(|name| *name == run.record.namespace)
                && labels
                    .iter()
                    .all(|(key, value)| run.record.metadata.labels.get(key) == Some(value))
                && toolset.is_none_or(|digest| digest == run.record.toolset)
                && (tools.is_empty() || tools.contains(&task.tool))
        }
        TaskFilter::Interrupted {
            tenants,
            max_attempts,
        } => {
            matches!(task.route, ToolRoute::Host { .. })
                && tenants.contains(&run.record.run.tenant)
                && matches!(task.state, TaskState::Claimed { .. })
                && (task.policy.on_interruption == Interruption::AtMostOnce
                    || task.attempt >= *max_attempts)
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
        let name = run.namespace.clone();
        if let Some(key) = &idempotency_key
            && let Some(existing) =
                state
                    .idempotency_keys
                    .get(&(tenant.clone(), name.clone(), key.clone()))
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
        let now = run.created_at;
        let namespace = state
            .namespaces
            .entry((tenant.clone(), name.clone()))
            .or_insert_with(|| Namespace::new(tenant.clone(), name.clone(), now))
            .clone();
        if namespace.deleting_since.is_some() {
            return Ok(CreateRun::NamespaceDeleting);
        }
        let limits = state.limits(&tenant, &name);
        let ceiling = state.tenant(&tenant).quotas;
        let month = Month::of(now);
        let script = run.started.script;
        let new_script =
            !state
                .namespace_scripts
                .contains_key(&(tenant.clone(), name.clone(), script));
        let runs_of_namespace = state.month_usage(&tenant, Some(&name), month).runs_started;
        let runs_of_tenant = state.month_usage(&tenant, None, month).runs_started;
        let scripts_of_namespace = state.scripts(&tenant, Some(&name));
        let scripts_of_tenant = state.scripts(&tenant, None);
        let checks = [
            (
                QuotaKind::ActiveRuns,
                QuotaScope::Namespace,
                limits.active_runs.map(u64::from),
                state.active_runs(&tenant, Some(&name)),
            ),
            (
                QuotaKind::ActiveRuns,
                QuotaScope::Tenant,
                ceiling.active_runs.map(u64::from),
                state.active_runs(&tenant, None),
            ),
            (
                QuotaKind::RunsPerMonth,
                QuotaScope::Namespace,
                limits.runs_per_month,
                runs_of_namespace,
            ),
            (
                QuotaKind::RunsPerMonth,
                QuotaScope::Tenant,
                ceiling.runs_per_month,
                runs_of_tenant,
            ),
            (
                QuotaKind::Scripts,
                QuotaScope::Namespace,
                limits.scripts.filter(|_| new_script),
                scripts_of_namespace,
            ),
            (
                QuotaKind::Scripts,
                QuotaScope::Tenant,
                ceiling.scripts.filter(|_| new_script),
                scripts_of_tenant,
            ),
        ];
        for (quota, scope, limit, used) in checks {
            let Some(limit) = limit else { continue };
            if used < limit {
                continue;
            }
            if quota == QuotaKind::ActiveRuns {
                state.raise(
                    QuotaAlertKey {
                        tenant: tenant.clone(),
                        namespace: (scope == QuotaScope::Namespace).then(|| name.clone()),
                        quota,
                        period: month,
                        threshold: Threshold::Reached,
                    },
                    used,
                    limit,
                    now,
                );
            }
            return Ok(CreateRun::QuotaExceeded(QuotaExceeded {
                quota,
                scope,
                limit,
                used,
                resets_at: (quota == QuotaKind::RunsPerMonth).then(|| month.end()),
            }));
        }
        let day = Day::of(now);
        for of in [Some(name.clone()), None] {
            state
                .usage
                .entry((tenant.clone(), of, day))
                .or_default()
                .runs_started += 1;
        }
        *state
            .namespace_scripts
            .entry((tenant.clone(), name.clone(), script))
            .or_default() += 1;
        if let Some(found) = state.namespaces.get_mut(&(tenant.clone(), name.clone())) {
            found.last_active_at = found.last_active_at.max(now);
        }
        state.raise_crossed(
            &tenant,
            Some(&name),
            QuotaKind::RunsPerMonth,
            month,
            (runs_of_namespace, runs_of_namespace + 1),
            limits.runs_per_month,
            now,
        );
        state.raise_crossed(
            &tenant,
            None,
            QuotaKind::RunsPerMonth,
            month,
            (runs_of_tenant, runs_of_tenant + 1),
            ceiling.runs_per_month,
            now,
        );
        if new_script {
            state.raise_crossed(
                &tenant,
                Some(&name),
                QuotaKind::Scripts,
                month,
                (scripts_of_namespace, scripts_of_namespace + 1),
                limits.scripts,
                now,
            );
            state.raise_crossed(
                &tenant,
                None,
                QuotaKind::Scripts,
                month,
                (scripts_of_tenant, scripts_of_tenant + 1),
                ceiling.scripts,
                now,
            );
        }
        let record = RunRecord {
            run: run.run.clone(),
            namespace: name.clone(),
            script: run.started.script,
            toolset: run.started.toolset,
            status: rewind_core::RunStatus::Pending,
            epoch: run.started.epoch,
            expires_at: run.started.expires_at,
            history_len: 0,
            quarantine: None,
            failures: 0,
            attempts: 0,
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
            state
                .idempotency_keys
                .insert((tenant, name, key), run.run.id);
        }
        if let Some(started_by) = state.runs[&run.run.id].record.metadata.started_by {
            state
                .watchers
                .entry(run.run.id)
                .or_default()
                .insert(started_by.run, (started_by.seq, !started_by.detached));
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
        state.run_mut(run)?;
        Ok(state.enqueue(run.id, completion, idempotency_key, now))
    }

    async fn get_run(&self, run: &RunRef) -> Result<Option<RunRecord>, StoreError> {
        let state = self.lock()?;
        Ok(state.run(run).ok().map(|found| found.record.clone()))
    }

    async fn watch_run(
        &self,
        run: &RunRef,
        watcher: RunId,
        seq: Seq,
    ) -> Result<Option<Watched>, StoreError> {
        let mut state = self.lock()?;
        let Ok(found) = state.run(run) else {
            return Ok(None);
        };
        let ended = found.is_terminal();
        state
            .watchers
            .entry(run.id)
            .or_default()
            .entry(watcher)
            .or_insert((seq, false));
        Ok(Some(Watched { ended }))
    }

    async fn put_workflow(
        &self,
        tenant: &TenantId,
        workflow: &Workflow,
        now: Timestamp,
    ) -> Result<StoredWorkflow, StoreError> {
        let mut state = self.lock()?;
        let order = state.next_workflow;
        let key = (tenant.clone(), workflow.version());
        if !state.workflows.contains_key(&key) {
            for test in &workflow.suite().tests {
                let live = state
                    .tests
                    .get(&(tenant.clone(), test.test))
                    .is_some_and(|stored| stored.deletion_started.is_none());
                if !live {
                    return Err(missing_test(test.test));
                }
            }
            for test in &workflow.suite().tests {
                state
                    .workflow_tests
                    .insert((tenant.clone(), test.test, workflow.version()));
            }
        }
        let (_, stored) = state.workflows.entry(key).or_insert_with(|| {
            (
                order,
                StoredWorkflow {
                    workflow: workflow.clone(),
                    labels: workflow.labels().clone(),
                    created_at: now,
                },
            )
        });
        let stored = stored.clone();
        state.next_workflow += 1;
        Ok(stored)
    }

    async fn get_workflow(
        &self,
        tenant: &TenantId,
        version: Digest,
    ) -> Result<Option<StoredWorkflow>, StoreError> {
        let state = self.lock()?;
        Ok(state
            .workflows
            .get(&(tenant.clone(), version))
            .map(|(_, stored)| stored.clone()))
    }

    async fn workflow_versions(
        &self,
        tenant: &TenantId,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, StoreError> {
        let state = self.lock()?;
        let mut versions: Vec<&(u64, StoredWorkflow)> = state
            .workflows
            .iter()
            .filter(|((owner, _), (_, stored))| {
                owner == tenant && stored.workflow.name() == name && stored.carries(labels)
            })
            .map(|(_, row)| row)
            .collect();
        versions.sort_by(|(a_order, a), (b_order, b)| {
            (b.created_at, b_order).cmp(&(a.created_at, a_order))
        });
        Ok(versions
            .into_iter()
            .map(|(_, stored)| stored.clone())
            .collect())
    }

    async fn workflows_with_script(
        &self,
        tenant: &TenantId,
        script: Digest,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, StoreError> {
        let state = self.lock()?;
        let mut versions: Vec<&(u64, StoredWorkflow)> = state
            .workflows
            .iter()
            .filter(|((owner, _), (_, stored))| {
                owner == tenant && stored.workflow.script() == script && stored.carries(labels)
            })
            .map(|(_, row)| row)
            .collect();
        versions.sort_by(|(a_order, a), (b_order, b)| {
            (b.created_at, b_order).cmp(&(a.created_at, a_order))
        });
        Ok(versions
            .into_iter()
            .map(|(_, stored)| stored.clone())
            .collect())
    }

    async fn latest_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<Vec<StoredWorkflow>, StoreError> {
        let state = self.lock()?;
        let mut latest: BTreeMap<&str, &(u64, StoredWorkflow)> = BTreeMap::new();
        for ((owner, _), row) in &state.workflows {
            let (order, stored) = row;
            if owner != &filter.tenant || !stored.carries(&filter.labels) {
                continue;
            }
            let name = stored.workflow.name();
            if filter.after.as_deref().is_some_and(|after| name <= after) {
                continue;
            }
            let newer = latest.get(name).is_none_or(|(known_order, known)| {
                (stored.created_at, order) > (known.created_at, known_order)
            });
            if newer {
                latest.insert(name, row);
            }
        }
        Ok(latest
            .into_values()
            .take(filter.limit.unwrap_or(usize::MAX))
            .map(|(_, stored)| stored.clone())
            .collect())
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<Vec<RunRecord>, StoreError> {
        let state = self.lock()?;
        let mut runs: Vec<RunRecord> = state
            .runs
            .values()
            .map(|run| &run.record)
            .filter(|record| record.run.tenant == filter.tenant)
            .filter(|record| {
                filter
                    .namespace
                    .as_ref()
                    .is_none_or(|name| record.namespace == *name)
            })
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
        run.record.attempts = run.record.attempts.saturating_add(1);
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
        run.record.attempts = 0;
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
            state.propagate_end(lease.run.id, activation.end.as_ref(), now);
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
        kind: QuarantineKind,
        reason: String,
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
            kind,
            reason,
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
        found.record.attempts = 0;
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
            let token = random_token("rewind_tt_");
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
        let ParkedScope::Secret { tenant, secret } = scope;
        let mut state = self.lock()?;
        let mut resumed = 0;
        for run in state.runs.values_mut() {
            if run.record.run.tenant != *tenant {
                continue;
            }
            let (attached, namespace) = (&run.record.metadata.secrets, &run.record.namespace);
            for task in run.tasks.values_mut() {
                let reads_it = task
                    .route
                    .server()
                    .and_then(|server| attached.get(server))
                    .is_some_and(|attached| SecretId::resolve(attached, namespace) == *secret);
                if reads_it
                    && matches!(&task.state, TaskState::Parked(reason) if reason.kind.resumed_by_secret())
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

    #[expect(
        clippy::todo,
        reason = "the engine-server workstream answers what reads a secret"
    )]
    async fn secret_usage(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
        limit: usize,
    ) -> Result<SecretUsage, StoreError> {
        let _ = (tenant, secret, limit);
        todo!(
            "engine-server: the active runs whose metadata attaches the secret, those waiting on \
             a call to a server it serves, the latest versions attaching it"
        )
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
        let token = random_token("rewind_wh_");
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

    async fn reserve_tests(
        &self,
        tenant: &TenantId,
        tests: &[TestRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, StoreError> {
        let mut state = self.lock()?;
        let mut deleting = Vec::new();
        for record in tests {
            match state.tests.get_mut(&(tenant.clone(), record.digest)) {
                Some(stored) if stored.deletion_started.is_some() => deleting.push(record.digest),
                Some(stored) => stored.reserved_at = now,
                None => {
                    state.tests.insert(
                        (tenant.clone(), record.digest),
                        StoredTest {
                            record: record.clone(),
                            reserved_at: now,
                            deletion_started: None,
                        },
                    );
                }
            }
        }
        Ok(deleting)
    }

    async fn get_test(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<TestRecord>, StoreError> {
        let state = self.lock()?;
        Ok(state
            .tests
            .get(&(tenant.clone(), digest))
            .filter(|stored| stored.deletion_started.is_none())
            .map(|stored| stored.record.clone()))
    }

    async fn namespace_tests(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        limit: usize,
    ) -> Result<Vec<Digest>, StoreError> {
        let state = self.lock()?;
        Ok(state
            .tests
            .iter()
            .filter(|((owner, _), stored)| {
                owner == tenant && stored.record.namespace.as_ref() == Some(namespace)
            })
            .map(|((_, digest), _)| *digest)
            .take(limit)
            .collect())
    }

    async fn unreferenced_tests(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        let mut state = self.lock()?;
        let due: Vec<(TenantId, Digest)> = state
            .tests
            .iter()
            .filter(|((tenant, digest), stored)| {
                stored.reserved_at.saturating_add(TEST_GRACE) <= now
                    && stored.deletion_started.is_none_or(|started| {
                        started.saturating_add(PAYLOAD_DELETION_TIMEOUT) <= now
                    })
                    && !state.test_referenced(tenant, *digest)
                    && !state.payloads.contains_key(&(tenant.clone(), *digest))
            })
            .map(|(key, _)| key.clone())
            .take(limit)
            .collect();
        for key in &due {
            if let Some(stored) = state.tests.get_mut(key) {
                stored.deletion_started = Some(now);
            }
        }
        Ok(due)
    }

    async fn drop_tests(
        &self,
        tests: &[(TenantId, Digest)],
        erased: bool,
    ) -> Result<usize, StoreError> {
        let mut state = self.lock()?;
        let mut dropped = 0;
        for (tenant, digest) in tests {
            let key = (tenant.clone(), *digest);
            let droppable = erased
                || state.tests.get(&key).is_some_and(|stored| {
                    stored.deletion_started.is_some() && !state.test_referenced(tenant, *digest)
                });
            if !droppable {
                continue;
            }
            if state.tests.remove(&key).is_some() {
                dropped += 1;
            }
            if erased {
                state
                    .workflow_tests
                    .retain(|(owner, test, _)| !(owner == tenant && test == digest));
            }
        }
        Ok(dropped)
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
            .filter(|((tenant, digest), stored)| deletable(&state, tenant, *digest, stored, now))
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
                    .is_some_and(|stored| deletable(&state, &key.0, key.1, stored, now))
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
            state.idempotency_keys.remove(&(
                run.tenant.clone(),
                removed.record.namespace.clone(),
                key,
            ));
        }
        let counted = (
            run.tenant.clone(),
            removed.record.namespace.clone(),
            removed.record.script,
        );
        if let Some(count) = state.namespace_scripts.get_mut(&counted) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.namespace_scripts.remove(&counted);
            }
        }
        state.watchers.remove(&run.id);
        for watchers in state.watchers.values_mut() {
            watchers.remove(&run.id);
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

    async fn put_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        exceptions: Option<&Quotas>,
        now: Timestamp,
    ) -> Result<NamespaceWrite, StoreError> {
        let mut state = self.lock()?;
        let namespace = state
            .namespaces
            .entry((tenant.clone(), name.clone()))
            .or_insert_with(|| Namespace::new(tenant.clone(), name.clone(), now));
        if namespace.deleting_since.is_some() {
            return Ok(NamespaceWrite::Deleting);
        }
        if let Some(exceptions) = exceptions {
            namespace.exceptions = exceptions.clone();
        }
        Ok(NamespaceWrite::Written(namespace.clone()))
    }

    async fn get_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> Result<Option<Namespace>, StoreError> {
        Ok(self
            .lock()?
            .namespaces
            .get(&(tenant.clone(), name.clone()))
            .cloned())
    }

    async fn set_namespace_display_name(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        display_name: Option<&DisplayName>,
    ) -> Result<Option<NamespaceWrite>, StoreError> {
        let mut state = self.lock()?;
        let Some(namespace) = state.namespaces.get_mut(&(tenant.clone(), name.clone())) else {
            return Ok(None);
        };
        if namespace.deleting_since.is_some() {
            return Ok(Some(NamespaceWrite::Deleting));
        }
        namespace.display_name = display_name.cloned();
        Ok(Some(NamespaceWrite::Written(namespace.clone())))
    }

    async fn namespace_display_names(
        &self,
        tenant: &TenantId,
    ) -> Result<Vec<(NamespaceName, DisplayName)>, StoreError> {
        Ok(self
            .lock()?
            .namespaces
            .iter()
            .filter(|((of, _), _)| of == tenant)
            .filter_map(|((_, name), namespace)| {
                Some((name.clone(), namespace.display_name.clone()?))
            })
            .collect())
    }

    async fn list_namespaces(&self, filter: &NamespaceFilter) -> Result<NamespacePage, StoreError> {
        let state = self.lock()?;
        let key = |usage: &Usage| match filter.order {
            NamespaceOrder::Cpu => usage.of(QuotaKind::CpuPerMonth).unwrap_or(0),
            NamespaceOrder::Runs => usage.runs_started,
            NamespaceOrder::Scripts => usage.scripts,
            NamespaceOrder::Name => 0,
        };
        let mut listed: Vec<(u64, NamespaceUsage)> = state
            .namespaces
            .iter()
            .filter(|((tenant, name), namespace)| {
                *tenant == filter.tenant
                    && filter.name.as_ref().is_none_or(|only| only == name)
                    && filter.prefix.as_deref().is_none_or(|prefix| {
                        name.as_str().starts_with(prefix)
                            || namespace
                                .display_name
                                .as_ref()
                                .is_some_and(|display| display.starts_with(prefix))
                    })
            })
            .map(|((tenant, name), namespace)| {
                let usage = state.usage_of(tenant, Some(name), filter.month, false);
                let limits = state.limits(tenant, name);
                (
                    key(&usage),
                    NamespaceUsage {
                        namespace: namespace.clone(),
                        limits,
                        usage,
                    },
                )
            })
            .filter(|(_, listed)| {
                filter.above_percent.is_none_or(|percent| {
                    [
                        QuotaKind::CpuPerMonth,
                        QuotaKind::ActiveRuns,
                        QuotaKind::RunsPerMonth,
                        QuotaKind::Scripts,
                    ]
                    .into_iter()
                    .any(|quota| {
                        match (listed.limits.limit(quota), listed.usage.of(quota)) {
                            (Some(limit), Some(used)) if limit > 0 => {
                                u128::from(used) * 100 >= u128::from(limit) * u128::from(percent)
                            }
                            _ => false,
                        }
                    })
                })
            })
            .collect();
        listed.sort_by(|(a_key, a), (b_key, b)| {
            b_key
                .cmp(a_key)
                .then_with(|| a.namespace.name.cmp(&b.namespace.name))
        });
        let total = u64::try_from(listed.len()).unwrap_or(u64::MAX);
        let namespaces = listed
            .into_iter()
            .filter(|(key, listed)| {
                filter.after.as_ref().is_none_or(|after| {
                    *key < after.key || (*key == after.key && listed.namespace.name > after.name)
                })
            })
            .take(filter.limit)
            .map(|(_, listed)| listed)
            .collect();
        Ok(NamespacePage { namespaces, total })
    }

    async fn namespace_usage(
        &self,
        tenant: &TenantId,
        of: &UsageOf,
        month: Month,
    ) -> Result<Usage, StoreError> {
        let state = self.lock()?;
        let namespace = match of {
            UsageOf::Namespace(name) => Some(name),
            UsageOf::Tenant => None,
        };
        Ok(state.usage_of(tenant, namespace, month, true))
    }

    async fn namespace_has_script(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        script: Digest,
    ) -> Result<bool, StoreError> {
        Ok(self
            .lock()?
            .namespace_scripts
            .contains_key(&(tenant.clone(), name.clone(), script)))
    }

    async fn revoke_namespace_tokens(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        let mut state = self.lock()?;
        Ok(
            match state.namespaces.get_mut(&(tenant.clone(), name.clone())) {
                Some(namespace) => {
                    namespace.tokens_revoked_before = Some(now);
                    true
                }
                None => false,
            },
        )
    }

    async fn delete_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<DeleteNamespace, StoreError> {
        let mut state = self.lock()?;
        Ok(
            match state.namespaces.get_mut(&(tenant.clone(), name.clone())) {
                Some(namespace) => {
                    namespace.deleting_since.get_or_insert(now);
                    DeleteNamespace::Started
                }
                None => DeleteNamespace::NotFound,
            },
        )
    }

    async fn namespaces_to_erase(
        &self,
        limit: usize,
    ) -> Result<Vec<(TenantId, NamespaceName)>, StoreError> {
        let state = self.lock()?;
        let mut marked: Vec<(Timestamp, TenantId, NamespaceName)> = state
            .namespaces
            .values()
            .filter_map(|namespace| {
                namespace
                    .deleting_since
                    .map(|since| (since, namespace.tenant.clone(), namespace.name.clone()))
            })
            .collect();
        marked.sort();
        Ok(marked
            .into_iter()
            .take(limit)
            .map(|(_, tenant, name)| (tenant, name))
            .collect())
    }

    async fn drop_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> Result<DropNamespace, StoreError> {
        let mut state = self.lock()?;
        let key = (tenant.clone(), name.clone());
        if !state
            .namespaces
            .get(&key)
            .is_some_and(|namespace| namespace.deleting_since.is_some())
        {
            return Ok(DropNamespace::NotFound);
        }
        if state
            .runs
            .values()
            .any(|run| run.record.run.tenant == *tenant && run.record.namespace == *name)
        {
            return Ok(DropNamespace::RunsLeft);
        }
        state.namespaces.remove(&key);
        state
            .usage
            .retain(|(owner, of, _), _| !(owner == tenant && of.as_ref() == Some(name)));
        state.alerts.retain(|alert, _| {
            !(alert.tenant == *tenant && alert.namespace.as_ref() == Some(name))
        });
        state
            .namespace_scripts
            .retain(|(owner, of, _), _| !(owner == tenant && of == name));
        Ok(DropNamespace::Dropped)
    }

    async fn record_usage(
        &self,
        deltas: &[UsageDelta],
        now: Timestamp,
    ) -> Result<Vec<QuotaAlert>, StoreError> {
        let mut state = self.lock()?;
        let mut raised = Vec::new();
        let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        for delta in deltas {
            let month = Month::of_day(delta.day);
            let tenant = &delta.tenant;
            let exists = state
                .namespaces
                .contains_key(&(tenant.clone(), delta.namespace.clone()));
            let limits = state.limits(tenant, &delta.namespace);
            let ceiling = state.tenant(tenant).quotas;
            let added = millis(delta.cpu);
            if exists {
                let before = millis(state.month_usage(tenant, Some(&delta.namespace), month).cpu);
                state
                    .usage
                    .entry((tenant.clone(), Some(delta.namespace.clone()), delta.day))
                    .or_default()
                    .cpu += delta.cpu;
                if let Some(found) = state
                    .namespaces
                    .get_mut(&(tenant.clone(), delta.namespace.clone()))
                {
                    found.last_active_at = found.last_active_at.max(now);
                }
                raised.extend(state.raise_crossed(
                    tenant,
                    Some(&delta.namespace),
                    QuotaKind::CpuPerMonth,
                    month,
                    (before, before.saturating_add(added)),
                    limits.limit(QuotaKind::CpuPerMonth),
                    now,
                ));
            }
            let before = millis(state.month_usage(tenant, None, month).cpu);
            state
                .usage
                .entry((tenant.clone(), None, delta.day))
                .or_default()
                .cpu += delta.cpu;
            raised.extend(state.raise_crossed(
                tenant,
                None,
                QuotaKind::CpuPerMonth,
                month,
                (before, before.saturating_add(added)),
                ceiling.limit(QuotaKind::CpuPerMonth),
                now,
            ));
        }
        Ok(raised)
    }

    async fn quota_alerts(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        month: Month,
    ) -> Result<Vec<QuotaAlert>, StoreError> {
        let state = self.lock()?;
        let mut alerts: Vec<QuotaAlert> = state
            .alerts
            .values()
            .map(|row| &row.alert)
            .filter(|alert| {
                alert.key.tenant == *tenant
                    && alert.key.period == month
                    && namespace.is_none_or(|name| alert.key.namespace.as_ref() == Some(name))
            })
            .cloned()
            .collect();
        alerts.sort_by(|a, b| {
            b.raised_at
                .cmp(&a.raised_at)
                .then_with(|| a.key.cmp(&b.key))
        });
        Ok(alerts)
    }

    async fn claim_quota_alerts(
        &self,
        _owner: &str,
        now: Timestamp,
        lease_duration: Duration,
        limit: usize,
    ) -> Result<Vec<QuotaAlert>, StoreError> {
        let mut state = self.lock()?;
        let mut open: Vec<(Timestamp, QuotaAlertKey)> = state
            .alerts
            .iter()
            .filter(|(_, row)| {
                row.alert.sent_at.is_none() && row.claimed_until.is_none_or(|until| until <= now)
            })
            .map(|(key, row)| (row.alert.raised_at, key.clone()))
            .collect();
        open.sort();
        let mut claimed = Vec::new();
        for (_, key) in open.into_iter().take(limit) {
            if let Some(row) = state.alerts.get_mut(&key) {
                row.claimed_until = Some(now.saturating_add(lease_duration));
                claimed.push(row.alert.clone());
            }
        }
        Ok(claimed)
    }

    async fn ack_quota_alerts(
        &self,
        keys: &[QuotaAlertKey],
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        for key in keys {
            if let Some(row) = state.alerts.get_mut(key) {
                row.alert.sent_at.get_or_insert(now);
            }
        }
        Ok(())
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

/// A version referencing a test the store cannot keep for it.
fn missing_test(digest: Digest) -> StoreError {
    StoreError::Conflict(format!(
        "the test {digest} has no row, or its deletion has started: reserve it and write its \
         blob again before saving the version"
    ))
}

/// Whether cleanup may start deleting the payload at `now`: no run has referenced it for its
/// grace, and its deletion has not started, or started too long ago to be still going on.
fn deletable(
    state: &State,
    tenant: &TenantId,
    digest: Digest,
    stored: &StoredPayload,
    now: Timestamp,
) -> bool {
    !state.tests.contains_key(&(tenant.clone(), digest))
        && stored.refs.is_empty()
        && stored
            .unreferenced_since
            .is_some_and(|since| since.saturating_add(state.retention(tenant).payload_grace) <= now)
        && stored
            .deletion_started
            .is_none_or(|started| started.saturating_add(PAYLOAD_DELETION_TIMEOUT) <= now)
}
