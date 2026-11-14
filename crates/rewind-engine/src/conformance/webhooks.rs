//! The conformance suite of `WebhookSender`: a delivery reaches its subscription only, with the
//! event's deterministic id and kind; a repeated delivery keeps its id; every kind of event is
//! delivered; a receiver that refuses is reported once the sender gives up.
//!
//! What the transport adds (the HMAC signature of an HTTP delivery, its retries) is checked by
//! the adapter's receiver: [`Receiver::received`] only returns what the receiver accepted, so a
//! receiver that verifies signatures makes the suite check them too.

use std::collections::BTreeMap;
use std::future::Future;

use rewind_core::{Payload, RunId, RunStatus, Seq, Timestamp};
use serde_json::json;

use super::{T0, tenant};
use crate::ports::{
    Month, NamespaceName, QuotaAlert, QuotaAlertKey, QuotaKind, Subscription, TenantId, Threshold,
    WebhookError, WebhookEvent, WebhookEventKind, WebhookEventType, WebhookSender,
};

/// The receiving side of a sender under test.
pub trait Receiver: Send + Sync {
    /// A subscription of `tenant`, named `id`, whose deliveries this receiver observes. With
    /// `accepting: false`, the receiver refuses every delivery to it.
    fn subscription(&self, tenant: &TenantId, id: &str, accepting: bool) -> Subscription;

    /// What the receiver accepted for the subscription `id`, in order: each event's id and kind,
    /// as the receiver read them.
    fn received(&self, id: &str) -> impl Future<Output = Vec<(String, WebhookEventType)>> + Send;
}

/// Runs every check, each on a new sender and receiver.
pub async fn run_all<S: WebhookSender, R: Receiver>(new: impl Fn() -> (S, R)) {
    for (name, check) in [
        ("deliveries_reach_their_subscription_only", 0),
        ("a_repeated_delivery_keeps_its_id", 1),
        ("every_kind_of_event_is_delivered", 2),
        ("a_refused_delivery_is_reported", 3),
    ] {
        eprintln!("webhooks conformance: {name}");
        let (sender, receiver) = new();
        match check {
            0 => deliveries_reach_their_subscription_only(&sender, &receiver).await,
            1 => a_repeated_delivery_keeps_its_id(&sender, &receiver).await,
            2 => every_kind_of_event_is_delivered(&sender, &receiver).await,
            _ => a_refused_delivery_is_reported(&sender, &receiver).await,
        }
    }
}

fn event(tenant: &TenantId, n: u128, index: u64, kind: WebhookEventKind) -> WebhookEvent {
    let run = RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0000 | n);
    WebhookEvent {
        id: format!("{run}/{index}"),
        tenant: tenant.clone(),
        namespace: Some(NamespaceName::new("alice").expect("valid")),
        run: Some(run),
        labels: BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
        occurred_at: Timestamp(T0.0 + index),
        kind,
    }
}

fn completed() -> WebhookEventKind {
    WebhookEventKind::RunCompleted {
        output: Payload::Value(json!({ "closed": 3 })),
    }
}

pub async fn deliveries_reach_their_subscription_only<S: WebhookSender, R: Receiver>(
    sender: &S,
    receiver: &R,
) {
    let acme = tenant("acme");
    let first = receiver.subscription(&acme, "first", true);
    let second = receiver.subscription(&acme, "second", true);
    let a = event(&acme, 1, 4, completed());
    let b = event(
        &acme,
        2,
        7,
        WebhookEventKind::RunSuspended {
            waiting_for: vec!["signal \"approval\"".into()],
        },
    );
    sender.send(&first, &a).await.expect("send");
    sender.send(&second, &b).await.expect("send");
    assert_eq!(
        receiver.received("first").await,
        [(a.id.clone(), WebhookEventType::RunCompleted)]
    );
    assert_eq!(
        receiver.received("second").await,
        [(b.id.clone(), WebhookEventType::RunSuspended)]
    );
}

pub async fn a_repeated_delivery_keeps_its_id<S: WebhookSender, R: Receiver>(
    sender: &S,
    receiver: &R,
) {
    let acme = tenant("acme");
    let subscription = receiver.subscription(&acme, "repeat", true);
    let a = event(&acme, 3, 2, completed());
    sender.send(&subscription, &a).await.expect("send");
    sender.send(&subscription, &a).await.expect("send again");
    assert_eq!(
        receiver.received("repeat").await,
        [
            (a.id.clone(), WebhookEventType::RunCompleted),
            (a.id.clone(), WebhookEventType::RunCompleted)
        ],
        "a receiver deduplicates on the id"
    );
}

pub async fn every_kind_of_event_is_delivered<S: WebhookSender, R: Receiver>(
    sender: &S,
    receiver: &R,
) {
    let acme = tenant("acme");
    let subscription = receiver.subscription(&acme, "kinds", true);
    let failed = |status, error: &str| WebhookEventKind::RunFailed {
        status,
        error: error.into(),
    };
    let kinds = [
        completed(),
        failed(RunStatus::Failed, "TypeError: x is undefined"),
        failed(
            RunStatus::Cancelled,
            "CancelledError: the run was cancelled",
        ),
        failed(RunStatus::Aborted, "limit_exceeded: CPU time"),
        WebhookEventKind::RunSuspended {
            waiting_for: vec!["tool ask_user #1".into()],
        },
        WebhookEventKind::RunQuarantined {
            reason: "history entry 1 is tool call #1 a({}), but the script emitted b({})".into(),
        },
        WebhookEventKind::ToolSchemaChanged {
            seq: Seq(2),
            tool: "github.close_issue".into(),
            message: "the tool's input schema refuses the call's arguments".into(),
        },
        WebhookEventKind::CredentialsExpired {
            seq: Seq(3),
            tool: "github.list_issues".into(),
            message: "401 Unauthorized".into(),
        },
        WebhookEventKind::QuotaAlert(QuotaAlert {
            key: QuotaAlertKey {
                tenant: acme.clone(),
                namespace: Some(NamespaceName::new("alice").expect("valid")),
                quota: QuotaKind::CpuPerMonth,
                period: Month::of(T0),
                threshold: Threshold::Near,
            },
            used: 8_000,
            limit: 10_000,
            raised_at: T0,
            sent_at: None,
        }),
    ];
    let mut expected = Vec::new();
    for (n, kind) in kinds.into_iter().enumerate() {
        let mut event = event(&acme, 10 + n as u128, n as u64, kind);
        if let WebhookEventKind::QuotaAlert(alert) = &event.kind {
            // About a namespace, not a run; no labels.
            event.id = alert.key.event_id();
            event.run = None;
            event.labels = BTreeMap::new();
        }
        sender.send(&subscription, &event).await.expect("send");
        expected.push((event.id.clone(), event.kind.event_type()));
    }
    let types: Vec<WebhookEventType> = expected.iter().map(|(_, kind)| *kind).collect();
    assert_eq!(types, WebhookEventType::ALL, "one event of each kind");
    assert_eq!(receiver.received("kinds").await, expected);
}

pub async fn a_refused_delivery_is_reported<S: WebhookSender, R: Receiver>(
    sender: &S,
    receiver: &R,
) {
    let acme = tenant("acme");
    let down = receiver.subscription(&acme, "down", false);
    let a = event(&acme, 20, 1, completed());
    let refused = sender.send(&down, &a).await;
    assert!(
        matches!(refused, Err(WebhookError::Delivery { .. })),
        "{refused:?}"
    );
    assert!(receiver.received("down").await.is_empty());
}
