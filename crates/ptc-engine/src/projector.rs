//! The projector (`archi/05-testing-and-observability.md`, "The feed and the projector").
//!
//! Every commit writes one feed item per consumer: the range of entries it committed. The
//! projector claims a consumer's items the way the dispatcher claims tool tasks, applies them,
//! and acknowledges them:
//!
//! | Consumer | Applies the entries to |
//! | --- | --- |
//! | `read_model` | the `ReadModel`: the entries, with the run's snapshot ([`run_snapshot`]) and its tasks' states, never sampled |
//! | `otel` | the `SpanExporter`: the spans that end in the range, and the logs; then the run metrics of [`metrics`](crate::metrics) |
//! | `webhooks` | the `WebhookSender`: `run.completed`, `run.failed`, `run.suspended` |
//!
//! Delivery is at least once, and every projection is idempotent: the read model is keyed by
//! `(run, index)`, spans have deterministic ids, and lifecycle events carry a deterministic id
//! for receivers to deduplicate. An item that cannot be applied stays claimed until its claim
//! expires, then it is applied again. Run metrics are not idempotent: they are recorded once the
//! item is acknowledged, so an item applied again after a failure is not counted twice.
//!
//! A composition root that schedules its own deliveries (concurrently, under a claim as long as
//! they may last) claims the `webhooks` items itself and turns each into its deliveries with
//! [`webhook_deliveries`].
//!
//! The read model also keeps service state that is not in the journal: the furthest state of
//! each call's task, and quarantine. The projector records the states of a run's open tasks after
//! each item it applies, so that a task created waiting for an approval shows up; whoever changes
//! a task or a quarantine since records it through [`Projector::record_tasks`] and
//! [`Projector::record_quarantine`].
//!
//! A run's snapshot is folded from its history up to the item's last entry ([`RunState`]):
//!
//! - a signal awaited with an empty mailbox is a `signal` wait since its `signal_awaited`, until
//!   its timeout timer's due time; that timer is not a wait of its own;
//! - a pending timer is a `timer` wait since its `start_timer`, until its due time;
//! - a pending call is a `host` wait when the pinned toolset routes its tool to the host, a
//!   `call` wait otherwise, since its `schedule_tool_call`;
//! - waits are ordered by `since`, then kind, then seq and name;
//! - a failure or an abort gives the run's [`Ending`](crate::ports::Ending), its signature and
//!   fingerprint ([`failures`]).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use ptc_core::{
    Annotation, Callsite, Command, Completion, Digest, Event, HistoryEntry, Lifecycle, Payload,
    RunState, RunStatus, ScriptError, Seq, Timestamp, ToolRoute, Toolset,
};

use crate::failures;
use crate::metrics::RunMeasurement;
use crate::ports::{
    ExportError, FeedConsumer, FeedItem, JournalStore, PayloadRecord, ProjectedRun, QuarantineNote,
    ReadModel, ReadModelError, RunRecord, RunRef, RunSnapshot, SpanExporter, StoreError,
    Subscription, TaskPhase, TaskProgress, TaskState, ToolTask, Wait, WaitKind, WebhookEvent,
    WebhookEventKind, WebhookSender,
};
use crate::telemetry::{RunTelemetry, TelemetryConfig};

/// How the projector claims and applies feed items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectorConfig {
    /// This projector's name, as the owner of its claims.
    pub owner: String,
    /// How long a claim lasts: an item not acknowledged by then is applied again.
    pub lease_duration: Duration,
    /// Feed items claimed at once, per consumer.
    pub batch: usize,
    /// The same as the engine's: sampling, and the labels exported as span attributes.
    pub telemetry: TelemetryConfig,
}

impl Default for ProjectorConfig {
    fn default() -> Self {
        Self {
            owner: "projector".into(),
            lease_duration: Duration::from_secs(30),
            batch: 100,
            telemetry: TelemetryConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectorError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    ReadModel(#[from] ReadModelError),
    #[error(transparent)]
    Export(#[from] ExportError),
}

/// One lifecycle event, for one subscription that accepts it.
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookDelivery {
    pub subscription: Subscription,
    pub event: WebhookEvent,
}

/// The deliveries a feed item of the `webhooks` consumer calls for: the lifecycle events of its
/// entries, each to the tenant's subscriptions that accept it. A run deleted or archived since
/// calls for none.
pub async fn webhook_deliveries<J: JournalStore>(
    journal: &J,
    item: &FeedItem,
) -> Result<Vec<WebhookDelivery>, ProjectorError> {
    let Some(record) = journal.get_run(&item.run).await? else {
        return Ok(Vec::new());
    };
    let history = match journal.load_history(&item.run, 0).await {
        Ok(history) => history,
        Err(StoreError::Archived(_) | StoreError::RunNotFound(_)) => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let to = usize::try_from(item.to_idx)
        .unwrap_or(usize::MAX)
        .min(history.len());
    let from = usize::try_from(item.from_idx).unwrap_or(usize::MAX).min(to);
    deliveries_of(journal, &record, &history[..to], from).await
}

/// The deliveries of the lifecycle events of the entries from `from` to the end of `history`.
async fn deliveries_of<J: JournalStore>(
    journal: &J,
    record: &RunRecord,
    history: &[HistoryEntry],
    from: usize,
) -> Result<Vec<WebhookDelivery>, ProjectorError> {
    let toolset = journal
        .get_toolset(&record.run.tenant, record.toolset)
        .await?
        .map(|stored| stored.toolset);
    let events = lifecycle_events(record, toolset.as_ref(), history, from);
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let subscriptions = journal.subscriptions(&record.run.tenant).await?;
    Ok(events
        .iter()
        .flat_map(|event| {
            subscriptions
                .iter()
                .filter(|subscription| subscription.accepts(event))
                .map(|subscription| WebhookDelivery {
                    subscription: subscription.clone(),
                    event: event.clone(),
                })
        })
        .collect())
}

/// What one pass of the projector did, per consumer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Projected {
    /// Feed items applied and acknowledged.
    pub applied: BTreeMap<FeedConsumer, usize>,
    /// Feed items that could not be applied: they are applied again once their claim expires.
    pub failed: BTreeMap<FeedConsumer, usize>,
}

impl Projected {
    pub fn total(&self) -> usize {
        self.applied.values().sum()
    }
}

/// Projects committed entries to the read model, run telemetry and lifecycle webhooks. It
/// borrows its adapters: the composition root shares the journal with the engine.
#[derive(Debug)]
pub struct Projector<'a, J, R, T, W> {
    pub journal: &'a J,
    pub read_model: &'a R,
    pub exporter: &'a T,
    pub webhooks: &'a W,
    pub config: ProjectorConfig,
}

impl<J, R, T, W> Projector<'_, J, R, T, W>
where
    J: JournalStore,
    R: ReadModel,
    T: SpanExporter,
    W: WebhookSender,
{
    /// Claims, applies and acknowledges a batch of feed items of every consumer.
    pub async fn project(&self, now: Timestamp) -> Result<Projected, ProjectorError> {
        let mut projected = Projected::default();
        for consumer in FeedConsumer::ALL {
            let (applied, failed) = self.project_consumer(consumer, now).await?;
            projected.applied.insert(consumer, applied);
            projected.failed.insert(consumer, failed);
        }
        Ok(projected)
    }

    /// Projects until no feed item is left, or only items that fail. Returns the items applied.
    pub async fn project_all(&self, now: Timestamp) -> Result<usize, ProjectorError> {
        let mut total = 0;
        loop {
            let projected = self.project(now).await?;
            if projected.total() == 0 {
                return Ok(total);
            }
            total += projected.total();
        }
    }

    /// Claims, applies and acknowledges a batch of one consumer's feed items. Returns how many
    /// were applied and how many failed.
    pub async fn project_consumer(
        &self,
        consumer: FeedConsumer,
        now: Timestamp,
    ) -> Result<(usize, usize), ProjectorError> {
        let items = self
            .journal
            .claim_feed(
                consumer,
                &self.config.owner,
                now,
                self.config.lease_duration,
                self.config.batch,
            )
            .await?;
        let mut applied = Vec::new();
        let mut measurements = Vec::new();
        let mut failed = 0;
        for item in &items {
            match self.apply(item).await {
                Ok(measured) => {
                    applied.push(item.id);
                    measurements.extend(measured);
                }
                Err(error) => {
                    failed += 1;
                    tracing::warn!(
                        consumer = %consumer,
                        run = %item.run,
                        %error,
                        "a feed item could not be applied: it is applied again later"
                    );
                }
            }
        }
        self.journal.ack_feed(&applied).await?;
        for measurement in &measurements {
            measurement.emit();
        }
        Ok((applied.len(), failed))
    }

    /// Applies one feed item, and returns the run metrics it measures. A run deleted or archived
    /// since has nothing left to project.
    async fn apply(&self, item: &FeedItem) -> Result<Vec<RunMeasurement>, ProjectorError> {
        let Some(record) = self.journal.get_run(&item.run).await? else {
            return Ok(Vec::new());
        };
        let history = match self.journal.load_history(&item.run, 0).await {
            Ok(history) => history,
            Err(StoreError::Archived(_) | StoreError::RunNotFound(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let to = usize::try_from(item.to_idx)
            .unwrap_or(usize::MAX)
            .min(history.len());
        let from = usize::try_from(item.from_idx).unwrap_or(usize::MAX).min(to);
        match item.consumer {
            FeedConsumer::ReadModel => {
                self.apply_read_model(&record, &history[..to], from).await?;
            }
            FeedConsumer::Otel => return self.apply_otel(&record, &history[..to], item).await,
            FeedConsumer::Webhooks => self.apply_webhooks(&record, &history[..to], from).await?,
        }
        Ok(Vec::new())
    }

    /// Applies the entries from `from` to the end of `history`, with the snapshot of `history`,
    /// then records the states of the run's open tasks. The read model gets the entries with the
    /// values of the table tier loaded, so that the shapes of most results are observed; values
    /// in the blob store are not downloaded.
    async fn apply_read_model(
        &self,
        record: &RunRecord,
        history: &[HistoryEntry],
        from: usize,
    ) -> Result<(), ProjectorError> {
        let tenant = &record.run.tenant;
        let mut entries = history[from..].to_vec();
        for entry in &mut entries {
            if let Event::Completion(Completion::ToolCallCompleted { result, .. }) =
                &mut entry.event
                && let Payload::Digest(digest) = result
                && let Some(value) = self
                    .journal
                    .get_payload(&record.run.tenant, *digest)
                    .await?
                    .and_then(|row| row.value)
            {
                *result = Payload::Value(value);
            }
        }
        let headline = self
            .journal
            .get_script(tenant, record.script)
            .await?
            .map(|stored| headline(stored.script.source()))
            .unwrap_or_default();
        let toolset = self
            .journal
            .get_toolset(tenant, record.toolset)
            .await?
            .map(|stored| stored.toolset);
        let run = ProjectedRun {
            run: record.run.clone(),
            script: record.script,
            headline,
            toolset: record.toolset,
            labels: record.metadata.labels.clone(),
            parent: record.metadata.parent.map(|parent| parent.run),
        };
        let snapshot = run_snapshot(history, toolset.as_ref());
        self.read_model.apply(&run, &entries, &snapshot).await?;
        self.record_tasks(&record.run).await
    }

    /// Records the states of a run's open tasks, as the store holds them now, in the read model.
    /// Whoever creates, claims, defers, approves, parks or resumes a task calls this after.
    pub async fn record_tasks(&self, run: &RunRef) -> Result<(), ProjectorError> {
        let tasks: Vec<TaskProgress> = self
            .journal
            .tool_tasks(run)
            .await?
            .iter()
            .map(task_progress)
            .collect();
        if !tasks.is_empty() {
            self.read_model.record_tasks(run, &tasks).await?;
        }
        Ok(())
    }

    /// Records a run's quarantine, as the store holds it now, in the read model: the worker that
    /// quarantined the run, and the endpoints that release it or give it up, call this after, with
    /// the time of the change. A run that no longer exists records nothing.
    pub async fn record_quarantine(
        &self,
        run: &RunRef,
        at: Timestamp,
    ) -> Result<(), ProjectorError> {
        let Some(record) = self.journal.get_run(run).await? else {
            return Ok(());
        };
        let note = record.quarantine.as_ref().map(QuarantineNote::from);
        self.read_model
            .record_quarantine(run, note.as_ref(), at)
            .await?;
        Ok(())
    }

    /// Exports the spans and logs of a sampled run, and returns the metrics of every run.
    async fn apply_otel(
        &self,
        record: &RunRecord,
        history: &[HistoryEntry],
        item: &FeedItem,
    ) -> Result<Vec<RunMeasurement>, ProjectorError> {
        let payloads = self.payload_rows(&record.run, history).await?;
        let telemetry = RunTelemetry {
            record,
            config: &self.config.telemetry,
            payloads: &payloads,
        };
        let measurements = if telemetry.measured(history) {
            telemetry.measurements(history, item.from_idx, item.to_idx)
        } else {
            Vec::new()
        };
        if !telemetry.exported(history) {
            return Ok(measurements);
        }
        let spans = telemetry.spans(history, item.from_idx, item.to_idx);
        if !spans.is_empty() {
            self.exporter.export(spans).await?;
        }
        let logs = telemetry.logs(history, item.from_idx, item.to_idx, item.fence);
        if !logs.is_empty() {
            self.exporter.export_logs(logs).await?;
        }
        Ok(measurements)
    }

    /// The `payloads` rows of the payloads held by digest: spans show their size and preview.
    async fn payload_rows(
        &self,
        run: &RunRef,
        history: &[HistoryEntry],
    ) -> Result<BTreeMap<Digest, PayloadRecord>, StoreError> {
        let mut rows = BTreeMap::new();
        for entry in history {
            if let Some(Payload::Digest(digest)) = crate::payloads::payload_of(&entry.event)
                && !rows.contains_key(digest)
                && let Some(row) = self.journal.get_payload(&run.tenant, *digest).await?
            {
                rows.insert(*digest, row);
            }
        }
        Ok(rows)
    }

    /// Sends the lifecycle events of the entries from `from` to the end of `history`. A
    /// subscription that refuses a delivery after the sender's retries is reported and skipped:
    /// the item is not applied again for it.
    async fn apply_webhooks(
        &self,
        record: &RunRecord,
        history: &[HistoryEntry],
        from: usize,
    ) -> Result<(), ProjectorError> {
        for WebhookDelivery {
            subscription,
            event,
        } in deliveries_of(self.journal, record, history, from).await?
        {
            if let Err(error) = self.webhooks.send(&subscription, &event).await {
                tracing::warn!(
                    subscription = %subscription.id,
                    event = %event.id,
                    %error,
                    "a lifecycle event was not delivered"
                );
            }
        }
        Ok(())
    }

    /// Sends `run.quarantined` for a run the engine just quarantined. Quarantine is not in the
    /// journal, so no feed item carries it: the worker that quarantined the run calls this.
    pub async fn notify_quarantined(&self, run: &RunRef) -> Result<(), ProjectorError> {
        let Some(record) = self.journal.get_run(run).await? else {
            return Ok(());
        };
        let Some(quarantine) = &record.quarantine else {
            return Ok(());
        };
        let event = WebhookEvent {
            id: format!("{}/quarantined/{}", run.id, quarantine.since.as_millis()),
            tenant: run.tenant.clone(),
            run: run.id,
            labels: record.metadata.labels.clone(),
            occurred_at: quarantine.since,
            kind: WebhookEventKind::RunQuarantined {
                reason: quarantine.reason.clone(),
            },
        };
        for subscription in self.journal.subscriptions(&run.tenant).await? {
            if subscription.accepts(&event)
                && let Err(error) = self.webhooks.send(&subscription, &event).await
            {
                tracing::warn!(subscription = %subscription.id, %error, "run.quarantined was not delivered");
            }
        }
        Ok(())
    }
}

/// The lifecycle events of the entries from `from` to the end of `history`: the run's end
/// (`run.completed`, `run.failed`), or, when the range leaves the run waiting for the outside
/// world only (signals, host tools), `run.suspended`. Each is named by the index of the entry
/// it is projected from.
pub(crate) fn lifecycle_events(
    record: &RunRecord,
    toolset: Option<&Toolset>,
    history: &[HistoryEntry],
    from: usize,
) -> Vec<WebhookEvent> {
    let event = |index: u64, occurred_at: Timestamp, kind: WebhookEventKind| WebhookEvent {
        id: format!("{}/{index}", record.run.id),
        tenant: record.run.tenant.clone(),
        run: record.run.id,
        labels: record.metadata.labels.clone(),
        occurred_at,
        kind,
    };
    let Some(last) = history.last() else {
        return Vec::new();
    };
    if history.len() <= from {
        return Vec::new();
    }
    let ended = history[from..].iter().find_map(|entry| {
        let kind = match &entry.event {
            Event::Command(Command::CompleteRun { output }) => WebhookEventKind::RunCompleted {
                output: output.clone(),
            },
            Event::Command(Command::FailRun { error }) => WebhookEventKind::RunFailed {
                status: if error.name == ScriptError::CANCELLED {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Failed
                },
                error: format!("{}: {}", error.name, error.message),
            },
            Event::Lifecycle(Lifecycle::RunAborted(reason)) => WebhookEventKind::RunFailed {
                status: RunStatus::Aborted,
                error: reason.to_string(),
            },
            _ => return None,
        };
        Some(event(entry.index, entry.recorded_at, kind))
    });
    if let Some(ended) = ended {
        return vec![ended];
    }
    let Ok(state) = RunState::from_history(history) else {
        return Vec::new();
    };
    let waiting_for = waiting_for(&state, toolset);
    if waiting_for.is_empty() {
        return Vec::new();
    }
    vec![event(
        last.index,
        last.recorded_at,
        WebhookEventKind::RunSuspended { waiting_for },
    )]
}

/// What a run that has not ended waits for from the outside world: signals and host tools.
/// Empty when it is still running something of its own: a call ai-ptc dispatches.
fn waiting_for(state: &RunState, toolset: Option<&Toolset>) -> Vec<String> {
    let route = |tool: &str| {
        toolset
            .and_then(|toolset| toolset.tool(tool))
            .map(|t| &t.route)
    };
    let mut waiting: Vec<String> = state
        .awaited_signals()
        .iter()
        .map(|wait| format!("signal {:?}", wait.name))
        .collect();
    for (seq, tool) in state.pending_tool_calls() {
        match route(tool) {
            Some(ToolRoute::Host { .. }) => waiting.push(format!("tool {tool} {seq}")),
            _ => return Vec::new(),
        }
    }
    waiting
}

/// A task's state as the read model keeps it: its seq, its attempt and its phase.
pub fn task_progress(task: &ToolTask) -> TaskProgress {
    TaskProgress {
        seq: task.task.seq,
        attempt: task.attempt,
        phase: match task.state {
            TaskState::Approval => TaskPhase::Approval,
            TaskState::Ready { .. } => TaskPhase::Ready,
            TaskState::Claimed { .. } => TaskPhase::Claimed,
            TaskState::Deferred { .. } => TaskPhase::Deferred,
            TaskState::Parked(_) => TaskPhase::Parked,
        },
    }
}

/// The first line of a script that is not blank, trimmed: what free text searches in a script.
pub fn headline(source: &str) -> String {
    source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// The snapshot of a run, folded from its history (see the [module](self)): its status and
/// mode, what it waits for while active, and how it ended. The pinned toolset tells the calls
/// of host tools from the calls ai-ptc sends; without it, every call is a `call`. A history that
/// cannot be folded past an entry gives the snapshot of the entries before it.
pub fn run_snapshot(history: &[HistoryEntry], toolset: Option<&Toolset>) -> RunSnapshot {
    let mut state = RunState::new();
    for entry in history {
        if let Err(error) = state.apply(entry) {
            tracing::warn!(%error, "a run's history cannot be folded: its snapshot stops before");
            break;
        }
    }
    let folded = &history[..usize::try_from(state.history_len())
        .unwrap_or(usize::MAX)
        .min(history.len())];
    let waits = if state.status() == RunStatus::Active {
        waits(folded, &state, toolset)
    } else {
        Vec::new()
    };
    RunSnapshot {
        history_len: state.history_len(),
        status: state.status(),
        mode: state.mode(),
        waits,
        ending: state.outcome().and_then(failures::ending),
    }
}

/// The waits of an active run, by `since`, then kind, then seq and name.
fn waits(history: &[HistoryEntry], state: &RunState, toolset: Option<&Toolset>) -> Vec<Wait> {
    // The entry that opened each timer and call: when, and where in the script.
    let mut opened: BTreeMap<Seq, (Timestamp, Option<Callsite>)> = BTreeMap::new();
    for entry in history {
        if let Event::Command(
            Command::ScheduleToolCall { seq, callsite, .. }
            | Command::StartTimer { seq, callsite, .. },
        ) = &entry.event
        {
            opened.insert(*seq, (entry.recorded_at, *callsite));
        }
    }
    let timers = state.pending_timers();
    let timeouts: BTreeSet<Seq> = state
        .awaited_signals()
        .iter()
        .filter_map(|signal| signal.timeout)
        .collect();
    let mut waits = Vec::new();
    for signal in state.awaited_signals() {
        let Some(entry) = usize::try_from(signal.since)
            .ok()
            .and_then(|index| history.get(index))
        else {
            continue;
        };
        let callsite = match &entry.event {
            Event::Annotation(Annotation::SignalAwaited { callsite, .. }) => *callsite,
            _ => None,
        };
        waits.push(Wait {
            kind: WaitKind::Signal,
            seq: signal.timeout,
            name: Some(signal.name.clone()),
            since: entry.recorded_at,
            until: signal.timeout.and_then(|timer| timers.get(&timer)).copied(),
            callsite,
            task: None,
        });
    }
    for (seq, fire_at) in timers {
        if timeouts.contains(seq) {
            continue;
        }
        let Some((since, callsite)) = opened.get(seq) else {
            continue;
        };
        waits.push(Wait {
            kind: WaitKind::Timer,
            seq: Some(*seq),
            name: None,
            since: *since,
            until: Some(*fire_at),
            callsite: *callsite,
            task: None,
        });
    }
    for (seq, tool) in state.pending_tool_calls() {
        let Some((since, callsite)) = opened.get(seq) else {
            continue;
        };
        let host = toolset
            .and_then(|toolset| toolset.tool(tool))
            .is_some_and(|tool| matches!(tool.route, ToolRoute::Host { .. }));
        waits.push(Wait {
            kind: if host { WaitKind::Host } else { WaitKind::Call },
            seq: Some(*seq),
            name: Some(tool.clone()),
            since: *since,
            until: None,
            callsite: *callsite,
            task: None,
        });
    }
    waits.sort_by(|a, b| (a.since, a.kind, a.seq, &a.name).cmp(&(b.since, b.kind, b.seq, &b.name)));
    waits
}
