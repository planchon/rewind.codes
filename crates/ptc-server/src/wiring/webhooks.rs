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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::body::Bytes;
    use axum::extract::Path;
    use axum::http::{HeaderMap, StatusCode};
    use ptc_api::webhooks::{SignedHeaders, WebhookSecret};
    use ptc_engine::conformance::webhooks::{Receiver, run_all};
    use ptc_engine::ports::{
        Secret, Subscription, TenantId, WebhookError, WebhookEvent, WebhookEventKind,
        WebhookEventType, WebhookSender,
    };

    use super::{HttpWebhooks, Render, Retries};

    /// Writes the port's event as `{"id", "type"}`: what the receiver reads back.
    #[derive(Debug, Clone, Copy)]
    struct Plain;

    impl Render for Plain {
        async fn render(
            &self,
            _subscription: &Subscription,
            event: &WebhookEvent,
        ) -> Result<Vec<u8>, WebhookError> {
            Ok(serde_json::to_vec(&serde_json::json!({
                "id": event.id,
                "type": event.kind.event_type().as_str(),
            }))
            .unwrap_or_default())
        }
    }

    /// An HTTP receiver: it verifies each delivery's signature with its subscription's secret,
    /// keeps what it accepted, and refuses everything sent to `/down/…`.
    struct HttpReceiver {
        base: String,
        secrets: Arc<Mutex<BTreeMap<String, WebhookSecret>>>,
        received: Arc<Mutex<Vec<(String, String, WebhookEventType)>>>,
    }

    impl HttpReceiver {
        /// Called inside the runtime: binds synchronously, serves on a task.
        fn start() -> Self {
            let secrets: Arc<Mutex<BTreeMap<String, WebhookSecret>>> = Arc::default();
            let received: Arc<Mutex<Vec<(String, String, WebhookEventType)>>> = Arc::default();
            let (keys, kept) = (secrets.clone(), received.clone());
            let app = axum::Router::new()
                .route(
                    "/up/{id}",
                    axum::routing::post(
                        move |Path(id): Path<String>, headers: HeaderMap, body: Bytes| {
                            let (keys, kept) = (keys.clone(), kept.clone());
                            async move {
                                let Some(secret) =
                                    keys.lock().ok().and_then(|keys| keys.get(&id).cloned())
                                else {
                                    return StatusCode::NOT_FOUND;
                                };
                                let header = |name: &str| {
                                    headers
                                        .get(name)
                                        .and_then(|value| value.to_str().ok())
                                        .unwrap_or_default()
                                        .to_owned()
                                };
                                let (webhook_id, timestamp, signature) = (
                                    header("webhook-id"),
                                    header("webhook-timestamp"),
                                    header("webhook-signature"),
                                );
                                let signed = SignedHeaders {
                                    id: &webhook_id,
                                    timestamp: &timestamp,
                                    signature: &signature,
                                };
                                // The body is not an API event: check the signature by hand.
                                let Ok(at) = timestamp.parse::<i64>() else {
                                    return StatusCode::BAD_REQUEST;
                                };
                                if secret.sign(signed.id, at, &body) != signed.signature {
                                    return StatusCode::UNAUTHORIZED;
                                }
                                let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body)
                                else {
                                    return StatusCode::BAD_REQUEST;
                                };
                                let kind = WebhookEventType::ALL
                                    .into_iter()
                                    .find(|kind| value["type"] == kind.as_str());
                                match (kind, value["id"].as_str()) {
                                    (Some(kind), Some(event)) if event == webhook_id => {
                                        if let Ok(mut kept) = kept.lock() {
                                            kept.push((id, event.to_owned(), kind));
                                        }
                                        StatusCode::NO_CONTENT
                                    }
                                    _ => StatusCode::BAD_REQUEST,
                                }
                            }
                        },
                    ),
                )
                .route(
                    "/down/{id}",
                    axum::routing::post(|| async { StatusCode::SERVICE_UNAVAILABLE }),
                );
            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .and_then(|listener| listener.set_nonblocking(true).map(|()| listener))
                .and_then(tokio::net::TcpListener::from_std)
                .unwrap_or_else(|error| panic!("{error}"));
            let addr = listener
                .local_addr()
                .unwrap_or_else(|error| panic!("{error}"));
            tokio::spawn(async move { axum::serve(listener, app).await });
            Self {
                base: format!("http://{addr}"),
                secrets,
                received,
            }
        }
    }

    impl Receiver for HttpReceiver {
        fn subscription(&self, tenant: &TenantId, id: &str, accepting: bool) -> Subscription {
            let secret = WebhookSecret::from_bytes([id.len() as u8; 32])
                .unwrap_or_else(|error| panic!("{error}"));
            if let Ok(mut secrets) = self.secrets.lock() {
                secrets.insert(id.to_owned(), secret.clone());
            }
            let path = if accepting { "up" } else { "down" };
            Subscription {
                tenant: tenant.clone(),
                id: id.to_owned(),
                url: format!("{}/{path}/{id}", self.base),
                secret: Secret::new(secret.expose()),
                events: BTreeSet::new(),
                labels: BTreeMap::new(),
            }
        }

        async fn received(&self, id: &str) -> Vec<(String, WebhookEventType)> {
            self.received
                .lock()
                .map(|received| {
                    received
                        .iter()
                        .filter(|(subscription, _, _)| subscription == id)
                        .map(|(_, event, kind)| (event.clone(), *kind))
                        .collect()
                })
                .unwrap_or_default()
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_http_sender_conforms() {
        let retries = Retries {
            attempts: 3,
            first_delay: Duration::from_millis(10),
            timeout: Duration::from_secs(5),
        };
        run_all(|| (HttpWebhooks::new(Plain, retries), HttpReceiver::start())).await;
    }

    #[tokio::test]
    async fn a_receiver_that_refuses_for_good_is_not_retried() {
        let app =
            axum::Router::new().route("/gone", axum::routing::post(|| async { StatusCode::GONE }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("{error}"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        let sender = HttpWebhooks::new(
            Plain,
            Retries {
                attempts: 5,
                first_delay: Duration::from_secs(60),
                timeout: Duration::from_secs(5),
            },
        );
        let subscription = Subscription {
            tenant: TenantId::local(),
            id: "s".into(),
            url: format!("http://{addr}/gone"),
            secret: Secret::new(
                WebhookSecret::from_bytes([1u8; 32])
                    .unwrap_or_else(|error| panic!("{error}"))
                    .expose(),
            ),
            events: BTreeSet::new(),
            labels: BTreeMap::new(),
        };
        let event = WebhookEvent {
            id: "r/1".into(),
            tenant: TenantId::local(),
            run: ptc_core::RunId::from_u128(1),
            labels: BTreeMap::new(),
            occurred_at: ptc_core::Timestamp(0),
            kind: WebhookEventKind::RunQuarantined { reason: "x".into() },
        };
        let started = std::time::Instant::now();
        let refused = sender.send(&subscription, &event).await;
        assert!(matches!(refused, Err(WebhookError::Delivery { .. })));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "a 410 is final"
        );
    }
}
