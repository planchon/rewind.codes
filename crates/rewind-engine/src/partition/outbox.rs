//! The outbox consumers of a partition (`archi/05-testing-and-observability.md`, "The
//! projector"; `archi/06-storage-and-scale.md`, "The outbox").
//!
//! An event is written in the batch of the fact that causes it. Once the batch is durable, its
//! projection consumes it: the spans and logs go to the `SpanExporter`, a webhook or a
//! `quota.alert` to the tenant's subscriptions through the `WebhookSender`. A later batch removes
//! the event once it is exported or delivered. A failed delivery's next try is a timer of the
//! wheel; after the last try the event is given up. Recovery hands the consumers what is left.

use std::sync::Arc;
use std::time::Duration;

use crate::node::message::job;
use crate::partition::commit::Unit;
use crate::partition::{Owner, OwnerMsg, runs::Step};
use crate::plane::Plane;
use crate::ports::partition::Change;
use crate::ports::{
    CatalogStore, DueTimer, OutboxChange, OutboxEntry, OutboxId, Projection, SpanExporter,
    TimerChange, TimerKind, Webhook, WebhookSender,
};

impl<P: Plane> Owner<P> {
    /// Hands a durable outbox entry to its projection.
    pub(crate) fn project(&mut self, entry: OutboxEntry) {
        let attempts = self
            .outbox
            .get(&entry.id)
            .map_or(0, |(_, attempts)| *attempts);
        self.outbox.insert(entry.id, (entry.clone(), attempts));
        self.consume(entry);
    }

    fn consume(&mut self, entry: OutboxEntry) {
        let plane = Arc::clone(&self.plane);
        let partition = self.partition;
        let id = entry.id;
        self.out.push(job(async move {
            let result = match entry.projection {
                Projection::Otel { spans, logs } => export(&*plane, spans, logs).await,
                Projection::Webhooks(event) => deliver(&*plane, &event).await,
                Projection::QuotaAlert(alert) => {
                    deliver(&*plane, &crate::quotas::quota_alert_event(&alert)).await
                }
            };
            crate::node::message::Message::Owner(
                partition,
                OwnerMsg::Step(Box::new(Step::Consumed { id, result })),
            )
        }));
    }

    /// An event was exported or delivered, or its try failed.
    pub(crate) fn consumed(&mut self, id: OutboxId, result: Result<(), String>) {
        let Some((entry, attempts)) = self.outbox.get_mut(&id) else {
            return;
        };
        let mut unit = Unit::new();
        match result {
            Ok(()) => {
                self.outbox.remove(&id);
                unit.change(Change::Outbox(OutboxChange::Remove(id)));
            }
            Err(error) => {
                *attempts += 1;
                let tries = *attempts;
                let tenant = entry.tenant.clone();
                let projection = entry.projection.name();
                if tries >= self.config.webhook_attempts {
                    tracing::error!(entry = %id, projection, %error, "an outbox entry was given up");
                    self.outbox.remove(&id);
                    unit.change(Change::Outbox(OutboxChange::Remove(id)));
                } else {
                    tracing::warn!(entry = %id, projection, tries, %error, "an outbox entry is tried again");
                    let delay = self
                        .config
                        .backoff
                        .delay(&format!("outbox/{id}"), tries)
                        .max(Duration::from_millis(1));
                    unit.change(Change::Timer(TimerChange::Set(DueTimer {
                        at: self.now().saturating_add(delay),
                        kind: TimerKind::WebhookRetry { tenant, entry: id },
                    })));
                }
            }
        }
        self.push(unit);
    }

    /// The next try of an event.
    pub(crate) fn redeliver(&mut self, id: OutboxId) {
        if let Some((entry, _)) = self.outbox.get(&id) {
            let entry = entry.clone();
            self.consume(entry);
        }
    }
}

async fn export<P: Plane>(
    plane: &P,
    spans: Vec<crate::ports::Span>,
    logs: Vec<crate::ports::LogRecord>,
) -> Result<(), String> {
    if !spans.is_empty() {
        plane
            .spans()
            .export(spans)
            .await
            .map_err(|error| error.to_string())?;
    }
    if !logs.is_empty() {
        plane
            .spans()
            .export_logs(logs)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Sends an event to every subscription of its tenant that takes it; fails if one delivery
/// failed, for the whole event to be tried again (receivers deduplicate by its id).
async fn deliver<P: Plane>(plane: &P, event: &Webhook) -> Result<(), String> {
    let subscriptions = plane
        .catalog()
        .subscriptions(&event.tenant)
        .await
        .map_err(|error| error.to_string())?;
    let mut failed = None;
    for subscription in subscriptions {
        if !subscription.accepts(event) {
            continue;
        }
        if let Err(error) = plane.webhooks().send(&subscription, event).await {
            failed = Some(format!("{}: {error}", subscription.id));
        }
    }
    failed.map_or(Ok(()), Err)
}
