//! What leaves a partition, and what an tenant actor keeps in its home partition
//! (`archi/06-storage-and-scale.md`, "The outbox", "Quotas").

use std::fmt;

use rewind_core::{RunId, Timestamp, WorkflowId};

use crate::ports::namespaces::{NamespaceName, QuotaAlert};
use crate::ports::telemetry::{LogRecord, Span};
use crate::ports::tenancy::TenantId;
use crate::ports::webhooks::Webhook;

/// An outbox entry, numbered by the owner: unique in its partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OutboxId(pub u64);

impl fmt::Display for OutboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A delivery waiting to leave the partition. It is written in the batch of the fact that
/// causes it; once that batch is durable its projection consumes it, and the event is removed
/// in a later batch, once exported or delivered. Recovery hands the projections what is left.
#[derive(Clone, Debug, PartialEq)]
pub struct OutboxEntry {
    pub id: OutboxId,
    pub tenant: TenantId,
    /// The run it is about; `None` for a quota alert. Erasing the run removes the event.
    pub run: Option<RunId>,
    pub projection: Projection,
}

/// The three projections fed through the outbox.
#[derive(Clone, Debug, PartialEq)]
pub enum Projection {
    /// Spans and logs derived from committed events, for the `SpanExporter`.
    Otel {
        spans: Vec<Span>,
        logs: Vec<LogRecord>,
    },
    /// A webhook for the tenant's subscriptions, through the `WebhookSender`.
    Webhooks(Webhook),
    /// A quota threshold crossed, sent to the subscriptions as `quota.alert`.
    QuotaAlert(QuotaAlert),
}

impl Projection {
    /// As the API and the stored records name it: `otel`, `webhooks`, `quota_alert`.
    pub fn name(&self) -> &'static str {
        match self {
            Projection::Otel { .. } => "otel",
            Projection::Webhooks(_) => "webhooks",
            Projection::QuotaAlert(_) => "quota_alert",
        }
    }
}

/// An outbox entry added or removed.
#[derive(Clone, Debug, PartialEq)]
pub enum OutboxChange {
    Add(Box<OutboxEntry>),
    /// The event was exported or delivered. Removing an event that is not there changes nothing.
    Remove(OutboxId),
}

/// What an tenant actor keeps in its tenant's home partition: the alerts it recorded, and
/// the admissions it holds until the owners' counts include them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TenantState {
    /// Each threshold crossed, recorded once per tenant, namespace (or the tenant), quota,
    /// month and threshold.
    pub alerts: Vec<QuotaAlert>,
    pub admissions: Vec<Admission>,
}

/// A start the tenant actor admitted that the counts of the run's owner do not include
/// yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admission {
    pub run: RunId,
    pub namespace: NamespaceName,
    /// The workflow the run is a run of; none for a test run.
    pub workflow: Option<WorkflowId>,
    pub admitted_at: Timestamp,
}
