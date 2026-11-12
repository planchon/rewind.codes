use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use ptc_core::{Payload, RunId, RunStatus, Seq, Timestamp};

use crate::ports::secrets::Secret;
use crate::ports::tenancy::TenantId;

/// Where a tenant wants lifecycle events delivered (`POST /v1/subscriptions`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscription {
    pub tenant: TenantId,
    pub id: String,
    pub url: String,
    /// The HMAC key every delivery is signed with.
    pub secret: Secret,
    /// The events delivered. Empty: every event.
    pub events: BTreeSet<WebhookEventType>,
    /// When not empty, only the events of runs carrying all of these labels: a subscription
    /// created with an API key limited to labels sees only those runs.
    pub labels: BTreeMap<String, String>,
}

impl Subscription {
    /// Whether this subscription receives `event`.
    pub fn accepts(&self, event: &WebhookEvent) -> bool {
        event.tenant == self.tenant
            && (self.events.is_empty() || self.events.contains(&event.kind.event_type()))
            && self
                .labels
                .iter()
                .all(|(key, value)| event.labels.get(key) == Some(value))
    }
}

/// The kinds of lifecycle events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WebhookEventType {
    RunCompleted,
    /// The script failed.
    RunFailed,
    /// The run ended `cancelled`.
    RunCancelled,
    /// The host ended the run.
    RunAborted,
    /// The run waits for the outside world: a signal, a host tool, an approval.
    RunSuspended,
    /// The run is set aside (`archi/03-execution.md`, "Non-determinism and quarantine").
    RunQuarantined,
    ToolSchemaChanged,
    CredentialsExpired,
}

impl WebhookEventType {
    pub const ALL: [WebhookEventType; 8] = [
        WebhookEventType::RunCompleted,
        WebhookEventType::RunFailed,
        WebhookEventType::RunCancelled,
        WebhookEventType::RunAborted,
        WebhookEventType::RunSuspended,
        WebhookEventType::RunQuarantined,
        WebhookEventType::ToolSchemaChanged,
        WebhookEventType::CredentialsExpired,
    ];

    /// The event's name on the wire, such as `run.completed`.
    pub fn as_str(self) -> &'static str {
        match self {
            WebhookEventType::RunCompleted => "run.completed",
            WebhookEventType::RunFailed => "run.failed",
            WebhookEventType::RunCancelled => "run.cancelled",
            WebhookEventType::RunAborted => "run.aborted",
            WebhookEventType::RunSuspended => "run.suspended",
            WebhookEventType::RunQuarantined => "run.quarantined",
            WebhookEventType::ToolSchemaChanged => "tool.schema_changed",
            WebhookEventType::CredentialsExpired => "credentials.expired",
        }
    }
}

/// A lifecycle event of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct WebhookEvent {
    /// Deterministic, so that a receiver can deduplicate a delivery repeated after a crash:
    /// `{run_id}/{index}` for an event projected from the journal entry `index`,
    /// `{run_id}/{seq}/{attempt}` for an event about a tool task.
    pub id: String,
    pub tenant: TenantId,
    pub run: RunId,
    /// The run's labels, for the subscriptions' filters.
    pub labels: BTreeMap<String, String>,
    pub occurred_at: Timestamp,
    pub kind: WebhookEventKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WebhookEventKind {
    RunCompleted {
        output: Payload,
    },
    RunFailed {
        /// `failed`, `cancelled` or `aborted`.
        status: RunStatus,
        /// The error, written for a model: the script's error or the abort reason.
        error: String,
    },
    RunSuspended {
        /// What the run waits for, such as `signal "approval"` or `tool github.close_issue`.
        waiting_for: Vec<String>,
    },
    RunQuarantined {
        reason: String,
    },
    ToolSchemaChanged {
        seq: Seq,
        tool: String,
        message: String,
    },
    CredentialsExpired {
        seq: Seq,
        tool: String,
        message: String,
    },
}

impl WebhookEventKind {
    pub fn event_type(&self) -> WebhookEventType {
        match self {
            WebhookEventKind::RunCompleted { .. } => WebhookEventType::RunCompleted,
            WebhookEventKind::RunFailed { status, .. } => match status {
                RunStatus::Cancelled => WebhookEventType::RunCancelled,
                RunStatus::Aborted => WebhookEventType::RunAborted,
                _ => WebhookEventType::RunFailed,
            },
            WebhookEventKind::RunSuspended { .. } => WebhookEventType::RunSuspended,
            WebhookEventKind::RunQuarantined { .. } => WebhookEventType::RunQuarantined,
            WebhookEventKind::ToolSchemaChanged { .. } => WebhookEventType::ToolSchemaChanged,
            WebhookEventKind::CredentialsExpired { .. } => WebhookEventType::CredentialsExpired,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    /// The receiver did not accept the delivery, after the sender's retries.
    #[error("the delivery to {url} failed: {message}")]
    Delivery { url: String, message: String },
}

/// Lifecycle events to the harness's subscriptions (`archi/07-code-architecture.md`, "The other
/// ports").
///
/// Each delivery is signed with HMAC (SHA-256) using the subscription's secret, and retried when
/// it fails; an error means the sender gave up. Delivery is at least once: events carry a
/// deterministic id for receivers to deduplicate.
pub trait WebhookSender: Send + Sync {
    fn send(
        &self,
        subscription: &Subscription,
        event: &WebhookEvent,
    ) -> impl Future<Output = Result<(), WebhookError>> + Send;
}
