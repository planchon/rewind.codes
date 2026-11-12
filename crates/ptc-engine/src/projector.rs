//! The projector (`archi/05-testing-and-observability.md`, "The feed and the projector").
//!
//! Every commit writes one feed item per consumer: the range of entries it committed. The
//! projector claims a consumer's items the way the dispatcher claims tool tasks, applies them,
//! and acknowledges them:
//!
//! | Consumer | Applies the entries to |
//! | --- | --- |
//! | `read_model` | the `ReadModel`: summaries and calls, never sampled |
//! | `otel` | the `SpanExporter`: the spans that end in the range, and the logs |
//! | `webhooks` | the `WebhookSender`: `run.completed`, `run.failed`, `run.suspended` |
//!
//! Delivery is at least once, and every projection is idempotent: the read model is keyed by
//! `(run, index)`, spans have deterministic ids, and lifecycle events carry a deterministic id
//! for receivers to deduplicate. An item that cannot be applied stays claimed until its claim
//! expires, then it is applied again.

use std::collections::BTreeMap;
use std::time::Duration;

use ptc_core::{
    Command, Completion, Digest, Event, HistoryEntry, Lifecycle, Payload, RunState, RunStatus,
    ScriptError, Timestamp, ToolRoute, Toolset,
};

use crate::ports::{
    ExportError, FeedConsumer, FeedItem, JournalStore, PayloadRecord, ProjectedRun, ReadModel,
    ReadModelError, RunRecord, RunRef, SpanExporter, StoreError, WebhookEvent, WebhookEventKind,
    WebhookSender,
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
        let mut failed = 0;
        for item in &items {
            match self.apply(item).await {
                Ok(()) => applied.push(item.id),
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
        Ok((applied.len(), failed))
    }

    /// Applies one feed item. A run deleted or archived since has nothing left to project.
    async fn apply(&self, item: &FeedItem) -> Result<(), ProjectorError> {
        let Some(record) = self.journal.get_run(&item.run).await? else {
            return Ok(());
        };
        let history = match self.journal.load_history(&item.run, 0).await {
            Ok(history) => history,
            Err(StoreError::Archived(_) | StoreError::RunNotFound(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let to = usize::try_from(item.to_idx)
            .unwrap_or(usize::MAX)
            .min(history.len());
        let from = usize::try_from(item.from_idx).unwrap_or(usize::MAX).min(to);
        match item.consumer {
            FeedConsumer::ReadModel => {
                self.apply_read_model(&record, &history[from..to]).await?;
            }
            FeedConsumer::Otel => self.apply_otel(&record, &history[..to], item).await?,
            FeedConsumer::Webhooks => self.apply_webhooks(&record, &history[..to], from).await?,
        }
        Ok(())
    }

    /// The read model gets the entries with the values of the table tier loaded, so that the
    /// shapes of most results are observed; values in the blob store are not downloaded.
    async fn apply_read_model(
        &self,
        record: &RunRecord,
        entries: &[HistoryEntry],
    ) -> Result<(), ProjectorError> {
        let mut entries = entries.to_vec();
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
        let run = ProjectedRun {
            run: record.run.clone(),
            script: record.script,
            toolset: record.toolset,
            labels: record.metadata.labels.clone(),
        };
        self.read_model.apply(&run, &entries).await?;
        Ok(())
    }

    async fn apply_otel(
        &self,
        record: &RunRecord,
        history: &[HistoryEntry],
        item: &FeedItem,
    ) -> Result<(), ProjectorError> {
        let payloads = self.payload_rows(&record.run, history).await?;
        let telemetry = RunTelemetry {
            record,
            config: &self.config.telemetry,
            payloads: &payloads,
        };
        if !telemetry.exported(history) {
            return Ok(());
        }
        let spans = telemetry.spans(history, item.from_idx, item.to_idx);
        if !spans.is_empty() {
            self.exporter.export(spans).await?;
        }
        let logs = telemetry.logs(history, item.from_idx, item.to_idx, item.fence);
        if !logs.is_empty() {
            self.exporter.export_logs(logs).await?;
        }
        Ok(())
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
        let toolset = self
            .journal
            .get_toolset(&record.run.tenant, record.toolset)
            .await?
            .map(|stored| stored.toolset);
        let events = lifecycle_events(record, toolset.as_ref(), history, from);
        if events.is_empty() {
            return Ok(());
        }
        let subscriptions = self.journal.subscriptions(&record.run.tenant).await?;
        for event in &events {
            for subscription in subscriptions.iter().filter(|sub| sub.accepts(event)) {
                if let Err(error) = self.webhooks.send(subscription, event).await {
                    tracing::warn!(
                        subscription = %subscription.id,
                        event = %event.id,
                        %error,
                        "a lifecycle event was not delivered"
                    );
                }
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
    let route = |tool: &str| toolset.and_then(|toolset| toolset.tool(tool)).map(|t| &t.route);
    let mut waiting: Vec<String> = state
        .awaited_signals()
        .iter()
        .map(|wait| format!("signal {:?}", wait.name))
        .collect();
    for (seq, tool) in state.pending_tool_calls() {
        match route(tool) {
            Some(ToolRoute::Host { .. }) => waiting.push(format!("tool {tool} ({seq})")),
            _ => return Vec::new(),
        }
    }
    waiting
}
