//! Outbound lifecycle webhooks: the HTTP adapter of the `WebhookSender` port
//! (`archi/07-code-architecture.md`, "The other ports").
//!
//! Each delivery is a `POST` of the API's `WebhookEvent` (`{type, timestamp, data}`), signed
//! following Standard Webhooks with the subscription's secret: `webhook-id` (the event's
//! deterministic id, the same on every retry), `webhook-timestamp` and `webhook-signature`.
//! A delivery that fails (a network error, a timeout, a 408, 429 or 5xx) is retried with
//! backoff; another 4xx is not. The sender reports an error once it gives up.

use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ptc_api::common::{Labels, timestamp};
use ptc_api::webhooks::{
    CredentialsExpired, RunEvent, ToolSchemaChanged, WEBHOOK_ID, WEBHOOK_SIGNATURE,
    WEBHOOK_TIMESTAMP, WebhookSecret,
};
use ptc_core::ToolRoute;
use ptc_engine::ports::{
    JournalStore, RunRef, Subscription, TaskRef, WebhookError, WebhookEvent, WebhookEventKind,
    WebhookSender,
};

use crate::views::{Views, task_id};
use crate::wiring::blobs::Blobs;
use crate::wiring::store::Store;

/// How deliveries are retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Retries {
    /// Deliveries, the first included.
    pub(crate) attempts: u32,
    /// The wait before the second; each next wait is four times longer.
    pub(crate) first_delay: Duration,
    /// One delivery's timeout.
    pub(crate) timeout: Duration,
}

impl Default for Retries {
    /// 5 deliveries over about 5 minutes: after 1 s, 4 s, 16 s and 64 s.
    fn default() -> Self {
        Self {
            attempts: 5,
            first_delay: Duration::from_secs(1),
            timeout: Duration::from_secs(10),
        }
    }
}

/// Writes the body of a delivery.
pub(crate) trait Render: Send + Sync {
    fn render(
        &self,
        subscription: &Subscription,
        event: &WebhookEvent,
    ) -> impl Future<Output = Result<Vec<u8>, WebhookError>> + Send;
}

/// The HTTP sender.
#[derive(Debug, Clone)]
pub(crate) struct HttpWebhooks<R> {
    client: reqwest::Client,
    render: R,
    retries: Retries,
}

impl<R> HttpWebhooks<R> {
    pub(crate) fn new(render: R, retries: Retries) -> Self {
        let client = reqwest::Client::builder()
            .timeout(retries.timeout)
            .user_agent(concat!("ai-ptc/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default();
        Self {
            client,
            render,
            retries,
        }
    }
}

impl<R: Render> WebhookSender for HttpWebhooks<R> {
    async fn send(
        &self,
        subscription: &Subscription,
        event: &WebhookEvent,
    ) -> Result<(), WebhookError> {
        let failed = |message: String| WebhookError::Delivery {
            url: subscription.url.clone(),
            message,
        };
        let body = self.render.render(subscription, event).await?;
        let secret = WebhookSecret::parse(subscription.secret.expose())
            .map_err(|error| failed(format!("the subscription's secret is invalid: {error}")))?;
        let mut delay = self.retries.first_delay;
        let mut last = String::new();
        for attempt in 1..=self.retries.attempts.max(1) {
            if attempt > 1 {
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(4);
            }
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
                });
            let response = self
                .client
                .post(&subscription.url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(WEBHOOK_ID, &event.id)
                .header(WEBHOOK_TIMESTAMP, now.to_string())
                .header(WEBHOOK_SIGNATURE, secret.sign(&event.id, now, &body))
                .body(body.clone())
                .send()
                .await;
            match response {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => {
                    let status = response.status();
                    last = format!("the receiver answered {status}");
                    let transient = status.is_server_error()
                        || status == reqwest::StatusCode::REQUEST_TIMEOUT
                        || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                    if !transient {
                        return Err(failed(last));
                    }
                }
                Err(error) => last = format!("the delivery failed: {error}"),
            }
            tracing::debug!(url = %subscription.url, event = %event.id, attempt, %last, "webhook delivery failed");
        }
        Err(failed(format!(
            "{last}, after {} deliveries",
            self.retries.attempts
        )))
    }
}

/// Renders events as the API's `WebhookEvent`, with the run's state read from the store when
/// the delivery is sent.
#[derive(Debug, Clone)]
pub(crate) struct ServerRender {
    pub(crate) store: Store,
    pub(crate) blobs: Blobs,
}

impl Render for ServerRender {
    async fn render(
        &self,
        _subscription: &Subscription,
        event: &WebhookEvent,
    ) -> Result<Vec<u8>, WebhookError> {
        let failed = |message: String| WebhookError::Delivery {
            url: String::new(),
            message,
        };
        let run = RunRef::new(event.tenant.clone(), event.run);
        let record = self
            .store
            .get_run(&run)
            .await
            .map_err(|error| failed(format!("the run cannot be read: {error}")))?
            .ok_or_else(|| failed(format!("run {} no longer exists", event.run)))?;
        let at = timestamp(event.occurred_at);
        let task = |seq| {
            task_id(&TaskRef {
                run: run.clone(),
                seq,
            })
        };
        let body = match &event.kind {
            WebhookEventKind::ToolSchemaChanged { seq, tool, message } => {
                ptc_api::webhooks::WebhookEvent::ToolSchemaChanged {
                    timestamp: at,
                    data: ToolSchemaChanged {
                        run_id: run.id.to_string(),
                        labels: Labels::from(event.labels.clone()),
                        task_id: task(*seq),
                        seq: seq.0,
                        tool: tool.clone(),
                        server: mcp_server(&self.store, &record, tool).await,
                        toolset: record.toolset.to_hex(),
                        message: message.clone(),
                    },
                }
            }
            WebhookEventKind::CredentialsExpired { seq, tool, message } => {
                ptc_api::webhooks::WebhookEvent::CredentialsExpired {
                    timestamp: at,
                    data: CredentialsExpired {
                        run_id: run.id.to_string(),
                        labels: Labels::from(event.labels.clone()),
                        task_id: task(*seq),
                        seq: seq.0,
                        tool: tool.clone(),
                        server: mcp_server(&self.store, &record, tool).await,
                        toolset: record.toolset.to_hex(),
                        message: message.clone(),
                    },
                }
            }
            kind => {
                let views = Views {
                    store: &self.store,
                    blobs: &self.blobs,
                };
                let data = RunEvent {
                    run: views
                        .run(&record)
                        .await
                        .map_err(|error| failed(error.0.detail.clone()))?,
                };
                use ptc_api::webhooks::WebhookEvent as Api;
                use ptc_engine::ports::WebhookEventType as Type;
                match kind.event_type() {
                    Type::RunCompleted => Api::RunCompleted {
                        timestamp: at,
                        data,
                    },
                    Type::RunFailed => Api::RunFailed {
                        timestamp: at,
                        data,
                    },
                    Type::RunCancelled => Api::RunCancelled {
                        timestamp: at,
                        data,
                    },
                    Type::RunAborted => Api::RunAborted {
                        timestamp: at,
                        data,
                    },
                    Type::RunSuspended => Api::RunSuspended {
                        timestamp: at,
                        data,
                    },
                    _ => Api::RunQuarantined {
                        timestamp: at,
                        data,
                    },
                }
            }
        };
        serde_json::to_vec(&body).map_err(|error| failed(error.to_string()))
    }
}

/// The MCP server of a tool of the run's toolset.
async fn mcp_server(
    store: &Store,
    record: &ptc_engine::ports::RunRecord,
    tool: &str,
) -> Option<String> {
    let stored = store
        .get_toolset(&record.run.tenant, record.toolset)
        .await
        .ok()??;
    match &stored.toolset.tool(tool)?.route {
        ToolRoute::Mcp { server } => Some(server.clone()),
        _ => None,
    }
}

/// The server's sender.
pub(crate) type Webhooks = HttpWebhooks<ServerRender>;
