//! Outbound lifecycle webhooks: the HTTP adapter of the `WebhookSender` port
//! (`archi/07-code-architecture.md`, "The other ports").
//!
//! Each delivery is a `POST` of the API's `WebhookEvent` (`{type, timestamp, data}`), signed
//! following Standard Webhooks with the subscription's secret: `webhook-id` (the event's
//! deterministic id, the same on every retry), `webhook-timestamp` and `webhook-signature`.
//! The sender makes one attempt per delivery, of at most [`DELIVERY_TIMEOUT`]: the owner of the
//! run's partition keeps a delivery that failed in its outbox and tries it again on a timer
//! (`archi/06-storage-and-scale.md`, "The outbox").
//!
//! Where a delivery may go is [`WebhookEgress`]: a deployed server sends only to `https://`
//! URLs whose addresses are public, with the egress checks of the `http` tool (`rewind-mcp`'s
//! [`Egress`]): the name is resolved once, a private, loopback, link-local or cloud-metadata
//! address is refused, and the connection goes to the address that was checked.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::Url;
use rewind_api::common::{Labels, timestamp};
use rewind_api::webhooks::{
    CredentialsExpired, RunEvent, ToolSchemaChanged, WEBHOOK_ID, WEBHOOK_SIGNATURE,
    WEBHOOK_TIMESTAMP, WebhookSecret,
};
use rewind_engine::ports::{
    CredentialsExpiredReason, RunRef, Subscription, TaskRef, WebhookError, WebhookEvent,
    WebhookEventKind, WebhookSender,
};
use rewind_outbound::{Egress, NameResolver, SystemResolver};

use crate::app::App;
use crate::views::task_id;

/// The longest one delivery takes, from its connection to the receiver's answer.
pub(crate) const DELIVERY_TIMEOUT: Duration = Duration::from_secs(10);

/// How the reason of a refused address ends.
const REFUSAL: &str = "webhooks are not sent there";

/// Where lifecycle webhooks may go.
#[derive(Debug, Clone)]
pub(crate) enum WebhookEgress {
    /// A deployed server: `https://` URLs whose addresses are public, checked when a
    /// subscription is created and at each delivery, where the connection goes to the address
    /// that was checked.
    Public(Egress),
    /// The embedded server (one local tenant), or `--allow-private-webhooks` on a trusted
    /// network: any `http://` or `https://` URL.
    Open,
}

impl WebhookEgress {
    /// The checks of a deployed server, with the system's resolver.
    pub(crate) fn public() -> Self {
        Self::public_with(Arc::new(SystemResolver))
    }

    /// The checks of a deployed server, resolving names with `resolver`.
    pub(crate) fn public_with(resolver: Arc<dyn NameResolver>) -> Self {
        Self::Public(Egress::new(resolver, REFUSAL))
    }

    /// Checks the URL of a new subscription, its name resolved now: the reason it is refused.
    pub(crate) async fn check_subscription(&self, url: &str) -> Result<(), String> {
        let parsed = self.check_url(url)?;
        match self {
            WebhookEgress::Public(egress) => egress.check(&parsed).await,
            WebhookEgress::Open => Ok(()),
        }
    }

    /// Checks a URL without resolving its name, which the client's resolver checks when it
    /// connects: its scheme, and its host when it is an address.
    fn check_url(&self, url: &str) -> Result<Url, String> {
        let parsed = Url::parse(url).map_err(|error| format!("{url:?} is not a URL: {error}"))?;
        match (self, parsed.scheme()) {
            (WebhookEgress::Public(egress), "https") => egress.check_url(&parsed)?,
            (WebhookEgress::Public(_), _) => {
                return Err(format!("webhooks are sent to https:// URLs, not {url:?}"));
            }
            (WebhookEgress::Open, "http" | "https") => {}
            (WebhookEgress::Open, _) => {
                return Err(format!(
                    "webhooks are sent to http:// and https:// URLs, not {url:?}"
                ));
            }
        }
        Ok(parsed)
    }

    fn client(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        match self {
            WebhookEgress::Public(egress) => egress.apply(builder),
            WebhookEgress::Open => builder,
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
    timeout: Duration,
    egress: WebhookEgress,
}

impl<R> HttpWebhooks<R> {
    /// A sender whose deliveries last at most `timeout`.
    pub(crate) fn new(render: R, timeout: Duration, egress: WebhookEgress) -> Self {
        let builder = reqwest::Client::builder()
            .user_agent(concat!("Rewind/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none());
        let client = egress.client(builder).build().unwrap_or_default();
        Self {
            client,
            render,
            timeout,
            egress,
        }
    }
}

impl<R: Render> WebhookSender for HttpWebhooks<R> {
    /// One delivery: a failure (a network error, the timeout, a 408, 429 or 5xx, another 4xx, a
    /// redirect, a refused address) is an error the owner tries again later.
    async fn send(
        &self,
        subscription: &Subscription,
        event: &WebhookEvent,
    ) -> Result<(), WebhookError> {
        let failed = |message: String| WebhookError::Delivery {
            url: subscription.url.clone(),
            message,
        };
        let url = self.egress.check_url(&subscription.url).map_err(failed)?;
        let body = self.render.render(subscription, event).await?;
        let secret = WebhookSecret::parse(subscription.secret.expose())
            .map_err(|error| failed(format!("the subscription's secret is invalid: {error}")))?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
            });
        let response = self
            .client
            .post(url)
            .timeout(self.timeout)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(WEBHOOK_ID, &event.id)
            .header(WEBHOOK_TIMESTAMP, now.to_string())
            .header(WEBHOOK_SIGNATURE, secret.sign(&event.id, now, &body))
            .body(body)
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => Ok(()),
            Ok(response) => Err(failed(format!(
                "the receiver answered {}",
                response.status()
            ))),
            Err(error) => Err(failed(
                Egress::refusal_of(&error)
                    .unwrap_or_else(|| format!("the delivery failed: {error}")),
            )),
        }
    }
}

/// Renders events as the API's `WebhookEvent`, with the run's state read from its owner when the
/// delivery is sent.
#[derive(Debug, Clone)]
pub(crate) struct ServerRender {
    /// The server, set once it is wired: the sender is part of the plane the server holds.
    pub(crate) app: Arc<std::sync::OnceLock<std::sync::Weak<App>>>,
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
        let at = timestamp(event.occurred_at);
        if let WebhookEventKind::QuotaAlert(alert) = &event.kind {
            let body = rewind_api::webhooks::WebhookEvent::QuotaAlert {
                timestamp: at,
                data: crate::views::quota_alert(alert),
            };
            return serde_json::to_vec(&body).map_err(|error| failed(error.to_string()));
        }
        let run_id = event
            .run
            .ok_or_else(|| failed("a run's event without its run".to_owned()))?;
        let run = RunRef::new(event.tenant.clone(), run_id);
        let app = self
            .app
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| failed("the server is stopping".to_owned()))?;
        let view = app
            .read_run(&run)
            .await
            .map_err(|error| failed(format!("the run cannot be read: {}", error.0.detail)))?
            .ok_or_else(|| failed(format!("run {run_id} no longer exists")))?;
        let record = &view.head;
        let task = |seq| {
            task_id(&TaskRef {
                run: run.clone(),
                seq,
            })
        };
        let body = match &event.kind {
            WebhookEventKind::ToolSchemaChanged { seq, tool, message } => {
                rewind_api::webhooks::WebhookEvent::ToolSchemaChanged {
                    timestamp: at,
                    data: ToolSchemaChanged {
                        run_id: run.id.to_string(),
                        namespace: crate::views::namespace(&record.namespace),
                        labels: Labels::from(event.labels.clone()),
                        task_id: task(*seq),
                        seq: seq.0,
                        tool: tool.clone(),
                        server: mcp_server(&app, record, tool).await,
                        action_set: record.action_set.to_hex(),
                        message: message.clone(),
                    },
                }
            }
            WebhookEventKind::CredentialsExpired {
                seq,
                tool,
                message,
                reason,
                secret,
            } => rewind_api::webhooks::WebhookEvent::CredentialsExpired {
                timestamp: at,
                data: CredentialsExpired {
                    run_id: run.id.to_string(),
                    namespace: crate::views::namespace(&record.namespace),
                    labels: Labels::from(event.labels.clone()),
                    task_id: task(*seq),
                    seq: seq.0,
                    tool: tool.clone(),
                    server: mcp_server(&app, record, tool).await,
                    action_set: record.action_set.to_hex(),
                    reason: match reason {
                        CredentialsExpiredReason::Refused => {
                            rewind_api::webhooks::CredentialsExpiredReason::Refused
                        }
                        CredentialsExpiredReason::Missing => {
                            rewind_api::webhooks::CredentialsExpiredReason::Missing
                        }
                    },
                    secret: secret.as_ref().map(ToString::to_string),
                    message: message.clone(),
                },
            },
            kind => {
                let data = RunEvent {
                    run: app
                        .views()
                        .run(&view)
                        .await
                        .map_err(|error| failed(error.0.detail.clone()))?,
                };
                use rewind_api::webhooks::WebhookEvent as Api;
                use rewind_engine::ports::WebhookEventType as Type;
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

/// The MCP server of a tool of the run's action set.
async fn mcp_server(app: &App, head: &rewind_engine::ports::RunHead, tool: &str) -> Option<String> {
    use rewind_engine::ports::CatalogStore as _;
    let stored = app
        .catalog()
        .get_action_set(&head.run.tenant, head.action_set)
        .await
        .ok()??;
    stored
        .action_set
        .tool(tool)?
        .route
        .server()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::body::Bytes;
    use axum::extract::Path;
    use axum::http::{HeaderMap, StatusCode};
    use rewind_api::webhooks::{SignedHeaders, WebhookSecret};
    use rewind_engine::conformance::webhooks::{Receiver, run_all};
    use rewind_engine::ports::{
        Secret, Subscription, TenantId, WebhookError, WebhookEvent, WebhookEventKind,
        WebhookEventType, WebhookSender,
    };

    use super::{HttpWebhooks, Render, WebhookEgress};

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
        run_all(|| {
            (
                HttpWebhooks::new(Plain, Duration::from_secs(5), WebhookEgress::Open),
                HttpReceiver::start(),
            )
        })
        .await;
    }

    #[tokio::test]
    async fn a_delivery_is_one_attempt_whatever_the_receiver_answers() {
        let tried: Arc<Mutex<usize>> = Arc::default();
        let counted = tried.clone();
        let app = axum::Router::new()
            .route("/gone", axum::routing::post(|| async { StatusCode::GONE }))
            .route(
                "/busy",
                axum::routing::post(move || {
                    if let Ok(mut tried) = counted.lock() {
                        *tried += 1;
                    }
                    async { StatusCode::SERVICE_UNAVAILABLE }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("{error}"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        let sender = HttpWebhooks::new(Plain, Duration::from_secs(5), WebhookEgress::Open);
        for (path, status) in [("gone", "410 Gone"), ("busy", "503 Service Unavailable")] {
            let refused = sender
                .send(&subscription(format!("http://{addr}/{path}")), &event())
                .await;
            let Err(WebhookError::Delivery { message, .. }) = refused else {
                panic!("{path} refuses the delivery");
            };
            assert_eq!(message, format!("the receiver answered {status}"));
        }
        assert_eq!(count(&tried), 1, "a 503 is not tried again by the sender");
    }

    /// Answers every name with the same addresses.
    #[derive(Debug)]
    struct Fixed(Vec<std::net::IpAddr>);

    impl rewind_outbound::NameResolver for Fixed {
        fn resolve(&self, _host: &str) -> rewind_outbound::Resolving {
            let addresses = self.0.clone();
            Box::pin(async move { Ok(addresses) })
        }
    }

    /// A receiver that accepts connections and never answers: its address, and how many
    /// connections it accepted.
    async fn silent() -> (std::net::SocketAddr, Arc<Mutex<usize>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("{error}"));
        let accepted = Arc::new(Mutex::new(0));
        let counted = accepted.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                if let Ok(mut count) = counted.lock() {
                    *count += 1;
                }
                held.push(socket);
            }
        });
        (addr, accepted)
    }

    fn subscription(url: String) -> Subscription {
        Subscription {
            tenant: TenantId::local(),
            id: "s".into(),
            url,
            secret: Secret::new(
                WebhookSecret::from_bytes([1u8; 32])
                    .unwrap_or_else(|error| panic!("{error}"))
                    .expose(),
            ),
            events: BTreeSet::new(),
            labels: BTreeMap::new(),
        }
    }

    fn event() -> WebhookEvent {
        WebhookEvent {
            id: "r/1".into(),
            tenant: TenantId::local(),
            namespace: None,
            run: Some(rewind_core::RunId::from_u128(1)),
            labels: BTreeMap::new(),
            occurred_at: rewind_core::Timestamp(0),
            kind: WebhookEventKind::RunQuarantined { reason: "x".into() },
        }
    }

    fn count(accepted: &Mutex<usize>) -> usize {
        accepted.lock().map(|count| *count).unwrap_or_default()
    }

    #[tokio::test]
    async fn a_delivery_ends_by_its_timeout() {
        let (addr, accepted) = silent().await;
        let sender = HttpWebhooks::new(Plain, Duration::from_millis(500), WebhookEgress::Open);
        let started = tokio::time::Instant::now();
        let given_up = sender
            .send(&subscription(format!("http://{addr}/hook")), &event())
            .await;
        let elapsed = started.elapsed();
        let Err(WebhookError::Delivery { message, .. }) = given_up else {
            panic!("the receiver never answers");
        };
        assert!(message.starts_with("the delivery failed"), "{message}");
        assert!(
            elapsed >= Duration::from_millis(450) && elapsed < Duration::from_millis(1500),
            "{elapsed:?}"
        );
        assert_eq!(count(&accepted), 1);
    }

    #[tokio::test]
    async fn a_deployed_server_sends_to_public_https_addresses_only() {
        let (addr, accepted) = silent().await;
        let port = addr.port();
        let egress = || {
            WebhookEgress::public_with(Arc::new(Fixed(vec![std::net::IpAddr::from([
                127, 0, 0, 1,
            ])])))
        };
        let sender = HttpWebhooks::new(Plain, Duration::from_secs(1), egress());
        for (url, reason) in [
            (
                format!("https://hooks.test:{port}/hook"),
                "hooks.test resolves to 127.0.0.1, a loopback address: webhooks are not sent there",
            ),
            (
                format!("https://127.0.0.1:{port}/hook"),
                "127.0.0.1 is a loopback address: webhooks are not sent there",
            ),
            (
                format!("http://hooks.test:{port}/hook"),
                "webhooks are sent to https:// URLs",
            ),
        ] {
            let started = std::time::Instant::now();
            let refused = sender.send(&subscription(url.clone()), &event()).await;
            let Err(WebhookError::Delivery { message, .. }) = refused else {
                panic!("{url} is refused");
            };
            assert!(message.starts_with(reason), "{url}: {message}");
            assert!(
                started.elapsed() < Duration::from_millis(400),
                "{url}: a refusal is final"
            );
        }
        assert_eq!(count(&accepted), 0, "nothing connected");

        // The same checks when a subscription is created, its name resolved then.
        let refused = egress()
            .check_subscription("https://hooks.test/hook")
            .await
            .unwrap_err();
        assert!(refused.contains("a loopback address"), "{refused}");
        let open = WebhookEgress::Open;
        assert!(
            open.check_subscription(&format!("http://127.0.0.1:{port}/hook"))
                .await
                .is_ok()
        );
        assert!(open.check_subscription("ftp://example.com/").await.is_err());
    }
}
