//! Quotas (`archi/06-storage-and-scale.md`, "Quotas").
//!
//! The counts a quota reads are the partitions' counters and gauges: an owner reports a
//! namespace's absolute counts to its organization actor after each batch that moved them, and
//! the actor admits or refuses each start against the namespace's limits and the tenant's
//! ceilings. A threshold crossed (80 % and 100 % of a limit, CPU time included) is recorded once
//! by the actor, with an outbox entry that sends it to the tenant's subscriptions as
//! `quota.alert` ([`quota_alert_event`]). CPU time is metering: it never ends, pauses or
//! changes a run.

use std::collections::BTreeMap;

use crate::ports::{QuotaAlert, Webhook, WebhookKind};

/// The event a quota alert is sent as: `quota.alert`, named by the alert's key, about the
/// namespace (none for the tenant's ceiling) and no run, without labels.
pub fn quota_alert_event(alert: &QuotaAlert) -> Webhook {
    Webhook {
        id: alert.key.event_id(),
        tenant: alert.key.tenant.clone(),
        namespace: alert.key.namespace.clone(),
        run: None,
        labels: BTreeMap::new(),
        occurred_at: alert.raised_at,
        kind: WebhookKind::QuotaAlert(alert.clone()),
    }
}
