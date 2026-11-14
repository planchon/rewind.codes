//! What the worker role does for quotas (`archi/06-storage-and-scale.md`, "Quotas"): it counts
//! the CPU time of the scripts it runs, writes it to the store about every 10 s, and sends the
//! alerts the store records.
//!
//! ```text
//! activation: a call into the script executes → meter[(tenant, namespace, UTC day)] += its CPU
//!             a call of a replay              → not counted: the cost of the instance cache
//! every lease renewal period (10 s):
//!     record_usage(the whole milliseconds counted) → the alerts of cpu_per_month it crossed
//!     the remainders below a millisecond stay counted; a failed write puts everything back
//! quota alerts: claim_quota_alerts → quota.alert to the tenant's subscriptions that take it
//!               → ack_quota_alerts
//! ```
//!
//! CPU time is metering: it never ends, pauses or changes a run, and it is not in the journal.
//! A worker that dies loses what it counted since its last write, at most 10 s.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use rewind_core::Timestamp;

use crate::engine::{Engine, EngineError};
use crate::ports::{
    BlobStore, Clock, Day, Entropy, JournalStore, NamespaceName, QuotaAlert, ScriptExecutor,
    SecretStore, StoreError, Subscription, TenantId, ToolDispatcher, UsageDelta, WebhookEvent,
    WebhookEventKind, WebhookSender,
};

/// The CPU time a worker counted and has not written yet, per tenant, namespace and UTC day.
#[derive(Debug, Default)]
pub(crate) struct CpuMeter {
    counted: Mutex<BTreeMap<(TenantId, NamespaceName, Day), Duration>>,
}

impl CpuMeter {
    /// Adds the CPU time of a call that executed.
    pub(crate) fn count(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        day: Day,
        cpu: Duration,
    ) {
        if cpu.is_zero() {
            return;
        }
        let mut counted = self
            .counted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let total = counted
            .entry((tenant.clone(), namespace.clone(), day))
            .or_default();
        *total = total.saturating_add(cpu);
    }

    /// Takes the whole milliseconds counted, as the deltas to write: the store counts
    /// milliseconds. What is left below a millisecond stays counted.
    fn take(&self) -> Vec<UsageDelta> {
        let mut counted = self
            .counted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut deltas = Vec::new();
        counted.retain(|(tenant, namespace, day), total| {
            let whole = Duration::from_millis(u64::try_from(total.as_millis()).unwrap_or(u64::MAX));
            if !whole.is_zero() {
                deltas.push(UsageDelta {
                    tenant: tenant.clone(),
                    namespace: namespace.clone(),
                    day: *day,
                    cpu: whole,
                });
                *total = total.saturating_sub(whole);
            }
            !total.is_zero()
        });
        deltas
    }

    /// Puts back deltas that were not written.
    fn restore(&self, deltas: Vec<UsageDelta>) {
        for delta in deltas {
            self.count(&delta.tenant, &delta.namespace, delta.day, delta.cpu);
        }
    }

    /// What is counted and not written yet.
    fn pending(&self) -> Vec<UsageDelta> {
        let counted = self
            .counted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        counted
            .iter()
            .map(|((tenant, namespace, day), cpu)| UsageDelta {
                tenant: tenant.clone(),
                namespace: namespace.clone(),
                day: *day,
                cpu: *cpu,
            })
            .collect()
    }
}

impl<J, B, X, D, C, E, S, W> Engine<J, B, X, D, C, E, S, W>
where
    J: JournalStore,
    B: BlobStore,
    X: ScriptExecutor,
    D: ToolDispatcher,
    C: Clock,
    E: Entropy,
    S: SecretStore,
    W: WebhookSender,
{
    /// Counts the CPU time of a call into a script that executed, toward its namespace's
    /// `cpu_per_month`, on the UTC day of now.
    pub(crate) fn count_cpu(&self, tenant: &TenantId, namespace: &NamespaceName, cpu: Duration) {
        self.cpu
            .count(tenant, namespace, Day::of(self.clock.now()), cpu);
    }

    /// Writes the CPU time this worker counted since its last write to the store, with the
    /// tenant's own rows, and returns the alerts of `cpu_per_month` the writes crossed. The worker
    /// role calls it about every 10 s, the period of its lease renewals, and once when it stops.
    /// Whole milliseconds are written; what a failed write did not write stays counted.
    pub async fn flush_usage(&self) -> Result<Vec<QuotaAlert>, EngineError> {
        let deltas = self.cpu.take();
        if deltas.is_empty() {
            return Ok(Vec::new());
        }
        match self.journal.record_usage(&deltas, self.clock.now()).await {
            Ok(alerts) => {
                for alert in &alerts {
                    tracing::info!(
                        tenant = %alert.key.tenant,
                        namespace = alert.key.namespace.as_ref().map_or("*", NamespaceName::as_str),
                        quota = %alert.key.quota,
                        threshold = alert.key.threshold.percent(),
                        "a quota alert was raised"
                    );
                }
                Ok(alerts)
            }
            Err(error) => {
                self.cpu.restore(deltas);
                Err(error.into())
            }
        }
    }

    /// The CPU time this worker counted and has not written yet, by tenant, namespace and day.
    pub fn unwritten_usage(&self) -> Vec<UsageDelta> {
        self.cpu.pending()
    }
}

/// The event a quota alert is sent as: `quota.alert`, named by the alert's key, about the
/// namespace (none for the tenant's ceiling) and no run, without labels.
pub fn quota_alert_event(alert: &QuotaAlert) -> WebhookEvent {
    WebhookEvent {
        id: alert.key.event_id(),
        tenant: alert.key.tenant.clone(),
        namespace: alert.key.namespace.clone(),
        run: None,
        labels: BTreeMap::new(),
        occurred_at: alert.raised_at,
        kind: WebhookEventKind::QuotaAlert(alert.clone()),
    }
}

/// The deliveries of a quota alert: its event, to each subscription of its tenant that takes
/// `quota.alert`. A subscription limited to labels does not: an alert carries none.
pub async fn quota_alert_deliveries<J: JournalStore>(
    journal: &J,
    alert: &QuotaAlert,
) -> Result<Vec<(Subscription, WebhookEvent)>, StoreError> {
    let event = quota_alert_event(alert);
    Ok(journal
        .subscriptions(&alert.key.tenant)
        .await?
        .into_iter()
        .filter(|subscription| subscription.accepts(&event))
        .map(|subscription| (subscription, event.clone()))
        .collect())
}

/// Sends the quota alerts the store recorded: claims those not sent yet, as the projector claims
/// feed items, sends each as `quota.alert` to the subscriptions of its tenant that take it, and
/// acknowledges it. Delivery is at least once: an alert claimed and not acknowledged is sent
/// again once its claim expires, with the same id. A delivery the sender gave up on is logged,
/// and its alert acknowledged all the same, as a lifecycle event is.
#[derive(Debug)]
pub struct QuotaAlerts<'a, J, W> {
    pub journal: &'a J,
    pub webhooks: &'a W,
    /// The owner of the claims.
    pub owner: &'a str,
    /// How long a claim lasts: longer than the deliveries of a batch may take, which are sent
    /// at once.
    pub lease_duration: Duration,
    /// Alerts claimed at once.
    pub batch: usize,
}

impl<J: JournalStore, W: WebhookSender> QuotaAlerts<'_, J, W> {
    /// Claims, sends and acknowledges one batch. Returns how many alerts were acknowledged.
    pub async fn send(&self, now: Timestamp) -> Result<usize, StoreError> {
        let alerts = self
            .journal
            .claim_quota_alerts(self.owner, now, self.lease_duration, self.batch)
            .await?;
        if alerts.is_empty() {
            return Ok(0);
        }
        let mut deliveries = Vec::new();
        for alert in &alerts {
            deliveries.extend(quota_alert_deliveries(self.journal, alert).await?);
        }
        let sent = futures_util::future::join_all(deliveries.iter().map(
            |(subscription, event)| async move {
                (
                    subscription,
                    event,
                    self.webhooks.send(subscription, event).await,
                )
            },
        ))
        .await;
        for (subscription, event, result) in sent {
            if let Err(error) = result {
                tracing::warn!(
                    subscription = %subscription.id,
                    event = %event.id,
                    %error,
                    "a quota alert was not delivered"
                );
            }
        }
        let keys: Vec<_> = alerts.into_iter().map(|alert| alert.key).collect();
        self.journal.ack_quota_alerts(&keys, now).await?;
        Ok(keys.len())
    }
}
