//! Webhooks, both ways.
//!
//! - **Inbound**: `POST /hooks/{token}`, at the unguessable URL `webhook(name)` returned to a
//!   script. Anything posted there becomes a signal on the run. No API key: the token is the
//!   credential.
//! - **Outbound**: the harness subscribes (`POST /v1/subscriptions`) and receives lifecycle
//!   events ([`WebhookEvent`]), signed with HMAC-SHA256 following the Standard Webhooks
//!   specification (<https://www.standardwebhooks.com>): headers `webhook-id`,
//!   `webhook-timestamp` and `webhook-signature`, and a secret `whsec_…`. Any Standard Webhooks
//!   library verifies them; [`WebhookSecret::verify`] does in Rust.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hmac::{KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use utoipa::ToSchema;

use crate::Error;
use crate::common::{Labels, Timestamp};
use crate::namespaces::{NamespaceName, QuotaAlert};
use crate::runs::Run;

// -------------------------------------------------------------------------------------------
// Inbound

/// Headers read, in this order, as the delivery id of a request to `POST /hooks/{token}`. The
/// first one present becomes the signal's idempotency key, so a sender's retry delivers one
/// signal.
pub const DELIVERY_ID_HEADERS: [&str; 3] = ["Idempotency-Key", "webhook-id", "X-GitHub-Delivery"];

// -------------------------------------------------------------------------------------------
// Subscriptions

/// `POST /v1/subscriptions`: send these lifecycle events to `url`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateSubscription {
    /// An HTTPS URL of the harness.
    pub url: String,
    /// The events to send. Every event when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<WebhookEventType>,
    /// Only events about runs carrying these labels. A key limited to labels can only
    /// subscribe within them.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Subscription {
    pub id: String,
    pub url: String,
    pub events: Vec<WebhookEventType>,
    pub labels: Labels,
    pub created_at: Timestamp,
    /// The signing secret, `whsec_…`. Only in the answer to `POST /v1/subscriptions`: keep it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

/// `GET /v1/subscriptions`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionList {
    pub subscriptions: Vec<Subscription>,
}

// -------------------------------------------------------------------------------------------
// Outbound events

/// The kinds of lifecycle events.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
pub enum WebhookEventType {
    #[serde(rename = "run.completed")]
    RunCompleted,
    #[serde(rename = "run.failed")]
    RunFailed,
    #[serde(rename = "run.cancelled")]
    RunCancelled,
    #[serde(rename = "run.aborted")]
    RunAborted,
    #[serde(rename = "run.suspended")]
    RunSuspended,
    #[serde(rename = "run.quarantined")]
    RunQuarantined,
    #[serde(rename = "action.schema_changed")]
    ActionSchemaChanged,
    #[serde(rename = "credentials.expired")]
    CredentialsExpired,
    #[serde(rename = "quota.alert")]
    QuotaAlert,
}

impl WebhookEventType {
    pub const ALL: [WebhookEventType; 9] = [
        WebhookEventType::RunCompleted,
        WebhookEventType::RunFailed,
        WebhookEventType::RunCancelled,
        WebhookEventType::RunAborted,
        WebhookEventType::RunSuspended,
        WebhookEventType::RunQuarantined,
        WebhookEventType::ActionSchemaChanged,
        WebhookEventType::CredentialsExpired,
        WebhookEventType::QuotaAlert,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            WebhookEventType::RunCompleted => "run.completed",
            WebhookEventType::RunFailed => "run.failed",
            WebhookEventType::RunCancelled => "run.cancelled",
            WebhookEventType::RunAborted => "run.aborted",
            WebhookEventType::RunSuspended => "run.suspended",
            WebhookEventType::RunQuarantined => "run.quarantined",
            WebhookEventType::ActionSchemaChanged => "action.schema_changed",
            WebhookEventType::CredentialsExpired => "credentials.expired",
            WebhookEventType::QuotaAlert => "quota.alert",
        }
    }
}

impl fmt::Display for WebhookEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The body of an outbound webhook: `{ "type", "timestamp", "data" }`, as Standard Webhooks
/// recommends. The delivery's id is the `webhook-id` header; a retried delivery keeps it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type")]
pub enum WebhookEvent {
    /// The run completed: `data.run.output` holds what `main` returned.
    #[serde(rename = "run.completed")]
    RunCompleted {
        timestamp: Timestamp,
        data: RunEvent,
    },
    /// The script failed: `data.run.error` says how.
    #[serde(rename = "run.failed")]
    RunFailed {
        timestamp: Timestamp,
        data: RunEvent,
    },
    /// The run ended `cancelled`.
    #[serde(rename = "run.cancelled")]
    RunCancelled {
        timestamp: Timestamp,
        data: RunEvent,
    },
    /// The host ended the run: `data.run.abort` says why.
    #[serde(rename = "run.aborted")]
    RunAborted {
        timestamp: Timestamp,
        data: RunEvent,
    },
    /// An activation left the run waiting for something other than its calls in flight:
    /// a signal, a timer, or a call that waits for the harness. `data.run.waitingFor` says
    /// what.
    #[serde(rename = "run.suspended")]
    RunSuspended {
        timestamp: Timestamp,
        data: RunEvent,
    },
    /// The run was quarantined: `data.run.quarantine` says why. Release it or give it up.
    #[serde(rename = "run.quarantined")]
    RunQuarantined {
        timestamp: Timestamp,
        data: RunEvent,
    },
    /// A call was parked because the server's schema of the action changed during the run.
    #[serde(rename = "action.schema_changed")]
    ActionSchemaChanged {
        timestamp: Timestamp,
        data: ActionSchemaChanged,
    },
    /// A call was parked because the server refused the secret or the signature (401), or
    /// because the secret the run attached no longer exists.
    #[serde(rename = "credentials.expired")]
    CredentialsExpired {
        timestamp: Timestamp,
        data: CredentialsExpired,
    },
    /// A namespace, or the tenant (`namespace` null), crossed 80 % or 100 % of a limit; for
    /// `active_runs`, a first refusal of the month. Sent once per namespace, quota, month and
    /// threshold; its `webhook-id` is `quota.alert:{namespace or *}:{quota}:{period}:{threshold}`.
    #[serde(rename = "quota.alert")]
    QuotaAlert {
        timestamp: Timestamp,
        data: QuotaAlert,
    },
}

impl WebhookEvent {
    pub fn event_type(&self) -> WebhookEventType {
        match self {
            WebhookEvent::RunCompleted { .. } => WebhookEventType::RunCompleted,
            WebhookEvent::RunFailed { .. } => WebhookEventType::RunFailed,
            WebhookEvent::RunCancelled { .. } => WebhookEventType::RunCancelled,
            WebhookEvent::RunAborted { .. } => WebhookEventType::RunAborted,
            WebhookEvent::RunSuspended { .. } => WebhookEventType::RunSuspended,
            WebhookEvent::RunQuarantined { .. } => WebhookEventType::RunQuarantined,
            WebhookEvent::ActionSchemaChanged { .. } => WebhookEventType::ActionSchemaChanged,
            WebhookEvent::CredentialsExpired { .. } => WebhookEventType::CredentialsExpired,
            WebhookEvent::QuotaAlert { .. } => WebhookEventType::QuotaAlert,
        }
    }

    pub fn timestamp(&self) -> Timestamp {
        match self {
            WebhookEvent::RunCompleted { timestamp, .. }
            | WebhookEvent::RunFailed { timestamp, .. }
            | WebhookEvent::RunCancelled { timestamp, .. }
            | WebhookEvent::RunAborted { timestamp, .. }
            | WebhookEvent::RunSuspended { timestamp, .. }
            | WebhookEvent::RunQuarantined { timestamp, .. }
            | WebhookEvent::ActionSchemaChanged { timestamp, .. }
            | WebhookEvent::CredentialsExpired { timestamp, .. }
            | WebhookEvent::QuotaAlert { timestamp, .. } => *timestamp,
        }
    }
}

/// The run an event is about, in the state the event left it in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunEvent {
    pub run: Run,
}

/// A call parked because the action no longer matches its pinned schema
/// (`archi/04-tools.md`, "When an action set changes during a run"). Approve the call to continue
/// against the new schema, reject it, fork the run, or cancel it. The pinned schema is in the
/// action set; `message` says what changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActionSchemaChanged {
    pub run_id: String,
    pub namespace: NamespaceName,
    pub labels: Labels,
    /// For `POST /v1/calls/{id}/approve` and `/reject`.
    pub call_id: String,
    pub seq: u32,
    pub action: String,
    /// The action's MCP server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// The run's action set, where the pinned schema is.
    pub action_set: String,
    /// What changed, as the dispatcher saw it: the action is gone, its input schema refuses the
    /// call's arguments, or its output schema changed. Written for the harness developer and
    /// the model.
    pub message: String,
}

/// A call parked because its server refused the secret or the signature (`refused`), or
/// because the secret the run attached no longer exists (`missing`, nothing was sent). Put the
/// secret again (`PUT /v1/secrets/{name}`, `PUT /v1/namespaces/{name}/secrets/{secret}`), or,
/// for a refused signature, approve the call once the server accepts the key: the call is then
/// sent again with the same idempotency key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct CredentialsExpired {
    pub run_id: String,
    pub namespace: NamespaceName,
    pub labels: Labels,
    pub call_id: String,
    pub seq: u32,
    pub action: String,
    /// The action's MCP server or API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    pub action_set: String,
    pub reason: CredentialsExpiredReason,
    /// The secret the run attached for the server, as attached (`org/<name>`, `ns/<name>`);
    /// absent for a signed call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// What the server answered, or which secret is missing.
    pub message: String,
}

/// Why a call waits in `credentials.expired`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CredentialsExpiredReason {
    /// The server answered 401 to the secret or to the signature.
    Refused,
    /// The secret the run attached no longer exists: nothing was sent.
    Missing,
}

// -------------------------------------------------------------------------------------------
// Signatures (Standard Webhooks)

/// The delivery's unique id. A retried delivery keeps it: receivers deduplicate on it.
pub const WEBHOOK_ID: &str = "webhook-id";
/// When the delivery was signed, in seconds since the Unix epoch.
pub const WEBHOOK_TIMESTAMP: &str = "webhook-timestamp";
/// `v1,<base64 HMAC-SHA256>`; several, separated by spaces, while a secret is rotated.
pub const WEBHOOK_SIGNATURE: &str = "webhook-signature";
/// A delivery signed further than this from the receiver's clock is refused: replays of old
/// deliveries fail.
pub const SIGNATURE_TOLERANCE: std::time::Duration = std::time::Duration::from_secs(5 * 60);

const SECRET_PREFIX: &str = "whsec_";

/// The secret a subscription's deliveries are signed with: `whsec_` and the key in base64.
#[derive(Clone, PartialEq, Eq)]
pub struct WebhookSecret(Vec<u8>);

impl WebhookSecret {
    /// A secret made of these key bytes (at least 24 of them, from a CSPRNG).
    pub fn from_bytes(key: impl Into<Vec<u8>>) -> Result<Self, Error> {
        let key = key.into();
        if key.len() < 24 {
            return Err(Error::invalid(
                "webhook secret",
                "a secret needs at least 24 random bytes",
            ));
        }
        Ok(Self(key))
    }

    /// Reads `whsec_<base64>` (the prefix is optional).
    pub fn parse(text: &str) -> Result<Self, Error> {
        let encoded = text.trim();
        let encoded = encoded.strip_prefix(SECRET_PREFIX).unwrap_or(encoded);
        let key = BASE64
            .decode(encoded)
            .map_err(|error| Error::invalid("webhook secret", error.to_string()))?;
        Self::from_bytes(key)
    }

    /// `whsec_<base64>`, as given to the subscriber once.
    pub fn expose(&self) -> String {
        format!("{SECRET_PREFIX}{}", BASE64.encode(&self.0))
    }

    /// The `webhook-signature` header of a delivery: `v1,<base64>` of the HMAC-SHA256 of
    /// `{id}.{timestamp}.{body}`.
    pub fn sign(&self, id: &str, timestamp: i64, body: &[u8]) -> String {
        format!(
            "v1,{}",
            BASE64.encode(self.mac(id, timestamp, body).finalize().into_bytes())
        )
    }

    /// Checks that a delivery was signed with this secret, recently, and returns its event.
    /// `now` is the receiver's clock.
    pub fn verify(
        &self,
        headers: &SignedHeaders<'_>,
        body: &[u8],
        now: Timestamp,
    ) -> Result<WebhookEvent, Error> {
        let timestamp: i64 = headers.timestamp.trim().parse().map_err(|_| {
            Error::Signature(format!(
                "invalid {WEBHOOK_TIMESTAMP}: {:?}",
                headers.timestamp
            ))
        })?;
        let skew = now.as_second().abs_diff(timestamp);
        if skew > SIGNATURE_TOLERANCE.as_secs() {
            return Err(Error::Signature(format!(
                "the delivery was signed {skew} s away from now, more than the {} s allowed",
                SIGNATURE_TOLERANCE.as_secs()
            )));
        }
        let signed = headers
            .signature
            .split_whitespace()
            .filter_map(|candidate| candidate.strip_prefix("v1,"))
            .filter_map(|encoded| BASE64.decode(encoded).ok())
            .any(|signature| {
                self.mac(headers.id, timestamp, body)
                    .verify_slice(&signature)
                    .is_ok()
            });
        if !signed {
            return Err(Error::Signature(
                "no v1 signature matches this secret".into(),
            ));
        }
        serde_json::from_slice(body)
            .map_err(|error| Error::Decode(format!("invalid webhook event: {error}")))
    }

    fn mac(&self, id: &str, timestamp: i64, body: &[u8]) -> hmac::Hmac<Sha256> {
        let mut mac = <hmac::Hmac<Sha256> as KeyInit>::new_from_slice(&self.0)
            .unwrap_or_else(|_| unreachable!("HMAC takes keys of any length"));
        mac.update(id.as_bytes());
        mac.update(b".");
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(body);
        mac
    }
}

/// Never printed.
impl fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebhookSecret(…)")
    }
}

/// The three signature headers of a delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignedHeaders<'a> {
    pub id: &'a str,
    pub timestamp: &'a str,
    pub signature: &'a str,
}

impl<'a> SignedHeaders<'a> {
    /// Reads them from a request's headers.
    pub fn from_headers(headers: &'a reqwest::header::HeaderMap) -> Result<Self, Error> {
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| Error::Signature(format!("missing {name} header")))
        };
        Ok(Self {
            id: get(WEBHOOK_ID)?,
            timestamp: get(WEBHOOK_TIMESTAMP)?,
            signature: get(WEBHOOK_SIGNATURE)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn event_body() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "type": "credentials.expired",
            "timestamp": "2026-11-09T10:02:11Z",
            "data": {
                "runId": "r1", "namespace": "alice", "labels": {}, "callId": "t1", "seq": 2,
                "action": "github.list_issues",
                "server": "github", "actionSet": "ab", "reason": "refused", "secret": "ns/github",
                "message": "401 Unauthorized"
            }
        }))
        .unwrap()
    }

    /// The test vector of the Standard Webhooks specification.
    #[test]
    fn signatures_follow_standard_webhooks() {
        let secret = WebhookSecret::parse("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
        assert_eq!(
            secret.sign(
                "msg_p5jXN8AQM9LWM0D4loKWxJek",
                1614265330,
                br#"{"test": 2432232314}"#
            ),
            "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
        );
        assert_eq!(secret.expose(), "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw");
    }

    #[test]
    fn a_delivery_signed_with_the_secret_verifies() {
        let secret = WebhookSecret::from_bytes([7u8; 32]).unwrap();
        let body = event_body();
        let now = Timestamp::from_second(1_762_682_531).unwrap();
        let signature = secret.sign("msg_1", now.as_second(), &body);
        let headers = SignedHeaders {
            id: "msg_1",
            timestamp: &now.as_second().to_string(),
            signature: &format!("v1,b2xkIHNpZ25hdHVyZQ== {signature}"),
        };
        let event = secret.verify(&headers, &body, now).unwrap();
        assert_eq!(event.event_type(), WebhookEventType::CredentialsExpired);

        // Another secret, another id, a changed body, an old delivery: refused.
        let other = WebhookSecret::from_bytes([8u8; 32]).unwrap();
        assert!(matches!(
            other.verify(&headers, &body, now),
            Err(Error::Signature(_))
        ));
        let wrong_id = SignedHeaders {
            id: "msg_2",
            ..headers
        };
        assert!(matches!(
            secret.verify(&wrong_id, &body, now),
            Err(Error::Signature(_))
        ));
        let mut changed = body.clone();
        changed.push(b' ');
        assert!(matches!(
            secret.verify(&headers, &changed, now),
            Err(Error::Signature(_))
        ));
        let later = Timestamp::from_second(now.as_second() + 301).unwrap();
        assert!(matches!(
            secret.verify(&headers, &body, later),
            Err(Error::Signature(_))
        ));
    }

    #[test]
    fn short_or_malformed_secrets_are_refused() {
        assert!(WebhookSecret::from_bytes([1u8; 16]).is_err());
        assert!(WebhookSecret::parse("whsec_not base64!").is_err());
        assert_eq!(
            format!("{:?}", WebhookSecret::from_bytes([1u8; 32]).unwrap()),
            "WebhookSecret(…)"
        );
    }

    #[test]
    fn event_types_are_written_with_dots() {
        for kind in WebhookEventType::ALL {
            assert_eq!(serde_json::to_value(kind).unwrap(), json!(kind.as_str()));
        }
    }
}
