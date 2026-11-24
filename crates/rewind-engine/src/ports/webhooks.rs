use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use rewind_core::{Payload, RunId, RunStatus, SecretRef, Seq, Timestamp};

#[cfg(doc)]
use crate::ports::namespaces::QuotaAlertKey;
use crate::ports::namespaces::{NamespaceName, QuotaAlert};
use crate::ports::secrets::Secret;
use crate::ports::tenancy::TenantId;

/// Where a tenant wants webhooks delivered (`POST /v1/subscriptions`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subscription {
    pub tenant: TenantId,
    pub id: String,
    pub url: String,
    /// The HMAC key every delivery is signed with.
    pub secret: Secret,
    /// The types of webhooks delivered. Empty: every type.
    pub types: BTreeSet<WebhookType>,
    /// When not empty, only the events of runs carrying all of these labels: a subscription
    /// created with an API key limited to labels sees only those runs.
    pub labels: BTreeMap<String, String>,
}

impl Subscription {
    /// Whether this subscription receives `event`.
    pub fn accepts(&self, event: &Webhook) -> bool {
        // A quota alert carries no labels: a subscription limited to labels does not get it.
        event.tenant == self.tenant
            && (self.types.is_empty() || self.types.contains(&event.kind.webhook_type()))
            && self
                .labels
                .iter()
                .all(|(key, value)| event.labels.get(key) == Some(value))
    }
}

/// The types of webhooks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WebhookType {
    RunCompleted,
    /// The code failed.
    RunFailed,
    /// The run ended `cancelled`.
    RunCancelled,
    /// The host ended the run.
    RunAborted,
    /// The run waits for the outside world: a signal, a harness action, an approval.
    RunWaiting,
    /// The run is set aside (`archi/03-execution.md`, "Non-determinism and quarantine").
    RunQuarantined,
    ActionSchemaChanged,
    /// A call parked because its server refused the secret or the signature, or because the
    /// secret the run attached no longer exists.
    SecretExpired,
    /// A namespace, or the tenant, crossed 80 % or 100 % of a limit
    /// (`archi/06-storage-and-scale.md`, "Quotas").
    QuotaAlert,
}

impl WebhookType {
    pub const ALL: [WebhookType; 9] = [
        WebhookType::RunCompleted,
        WebhookType::RunFailed,
        WebhookType::RunCancelled,
        WebhookType::RunAborted,
        WebhookType::RunWaiting,
        WebhookType::RunQuarantined,
        WebhookType::ActionSchemaChanged,
        WebhookType::SecretExpired,
        WebhookType::QuotaAlert,
    ];

    /// The event's name on the wire, such as `run.completed`.
    pub fn as_str(self) -> &'static str {
        match self {
            WebhookType::RunCompleted => "run.completed",
            WebhookType::RunFailed => "run.failed",
            WebhookType::RunCancelled => "run.cancelled",
            WebhookType::RunAborted => "run.aborted",
            WebhookType::RunWaiting => "run.waiting",
            WebhookType::RunQuarantined => "run.quarantined",
            WebhookType::ActionSchemaChanged => "action.schema_changed",
            WebhookType::SecretExpired => "secret.expired",
            WebhookType::QuotaAlert => "quota.alert",
        }
    }
}

/// A webhook of a run, or a quota alert.
#[derive(Clone, Debug, PartialEq)]
pub struct Webhook {
    /// Deterministic, so that a receiver can deduplicate a delivery repeated after a crash:
    /// `{run_id}/{index}` for an event projected from the event `index`,
    /// `{run_id}/{seq}/{attempt}` for an event about a call's task, the alert's
    /// [`QuotaAlertKey::event_id`] for a quota alert.
    pub id: String,
    pub tenant: TenantId,
    /// The run's namespace; for a quota alert, the namespace's, or `None` for the tenant's
    /// ceiling.
    pub namespace: Option<NamespaceName>,
    /// The run the event is about; `None` for a quota alert.
    pub run: Option<RunId>,
    /// The run's labels, for the subscriptions' filters.
    pub labels: BTreeMap<String, String>,
    pub occurred_at: Timestamp,
    pub kind: WebhookKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum WebhookKind {
    RunCompleted {
        output: Payload,
    },
    RunFailed {
        /// `failed`, `cancelled` or `aborted`.
        status: RunStatus,
        /// The error, written for a model: the code's error or the abort reason.
        error: String,
    },
    RunWaiting {
        /// What the run waits for, such as `signal "approval"` or `action github.close_issue`.
        waiting_for: Vec<String>,
    },
    RunQuarantined {
        reason: String,
    },
    ActionSchemaChanged {
        seq: Seq,
        action: String,
        message: String,
    },
    SecretExpired {
        seq: Seq,
        action: String,
        message: String,
        reason: SecretExpiredReason,
        /// The secret the run attached for the action's server, as attached (`org/<name>` or
        /// `ns/<name>`); `None` for a signed call.
        secret: Option<SecretRef>,
    },
    QuotaAlert(QuotaAlert),
}

/// Why a call waits in `secret.expired` (`archi/04-tools.md`, "Call lifecycle"). The
/// harness does the same in both cases: put the secret again, or approve the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SecretExpiredReason {
    /// The server answered 401 to the secret or to the signature.
    Refused,
    /// The secret the run attached no longer exists: nothing was sent.
    Missing,
}

impl SecretExpiredReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretExpiredReason::Refused => "refused",
            SecretExpiredReason::Missing => "missing",
        }
    }
}

impl WebhookKind {
    pub fn webhook_type(&self) -> WebhookType {
        match self {
            WebhookKind::RunCompleted { .. } => WebhookType::RunCompleted,
            WebhookKind::RunFailed { status, .. } => match status {
                RunStatus::Cancelled => WebhookType::RunCancelled,
                RunStatus::Aborted => WebhookType::RunAborted,
                _ => WebhookType::RunFailed,
            },
            WebhookKind::RunWaiting { .. } => WebhookType::RunWaiting,
            WebhookKind::RunQuarantined { .. } => WebhookType::RunQuarantined,
            WebhookKind::ActionSchemaChanged { .. } => WebhookType::ActionSchemaChanged,
            WebhookKind::SecretExpired { .. } => WebhookType::SecretExpired,
            WebhookKind::QuotaAlert(_) => WebhookType::QuotaAlert,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    /// The receiver did not accept the delivery, after the sender's retries.
    #[error("the delivery to {url} failed: {message}")]
    Delivery { url: String, message: String },
}

/// Webhooks to the harness's subscriptions (`archi/07-code-architecture.md`, "The other
/// ports").
///
/// Each delivery is signed with HMAC (SHA-256) using the subscription's secret, and retried when
/// it fails; an error means the sender gave up. Delivery is at least once: events carry a
/// deterministic id for receivers to deduplicate.
pub trait WebhookSender: Send + Sync {
    fn send(
        &self,
        subscription: &Subscription,
        event: &Webhook,
    ) -> impl Future<Output = Result<(), WebhookError>> + Send;
}
