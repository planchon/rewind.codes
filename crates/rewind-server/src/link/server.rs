//! A node's side of the link: an axum router on a listener of its own, HTTP/2 without TLS
//! (`archi/06-storage-and-scale.md`, "Partitions, owners and nodes": the edge).
//!
//! Every request is authenticated by the deployment's secret and must speak this build's
//! [`LINK_VERSION`]: a request without the secret is answered 401, one of another version 400,
//! naming both versions. A request the node cannot take (it has stopped) is answered 503.

use std::future::Future;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use futures_util::StreamExt as _;
use rewind_engine::node::runtime::NodeHandle;
use rewind_engine::plane::Plane;
use rewind_engine::ports::LinkError;
use rewind_engine::ports::link;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::dto::{self, Invalid, LINK_VERSION, VERSION_HEADER};
use super::{ACTIVATE, CALL, ClusterSecret, SUBSCRIBE};

/// The longest a subscription stays silent: a comment is sent after this much time without a
/// change, so that the connection is seen alive on both sides.
pub const KEEP_ALIVE: Duration = Duration::from_secs(15);

/// Serves the node link of `node` on `listener` until `shutdown` completes, then stops
/// gracefully: no new connection, the subscriptions ended, the requests in flight answered.
///
/// `listener` serves HTTP/2 with prior knowledge (and HTTP/1.1), without TLS: the link runs on
/// the private network of the deployment.
pub fn serve<P: Plane>(
    listener: TcpListener,
    node: NodeHandle<P>,
    secret: ClusterSecret,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> JoinHandle<()> {
    let (stop, stopping) = watch::channel(false);
    let router = router(node, secret, stopping);
    tokio::spawn(async move {
        let graceful = async move {
            shutdown.await;
            stop.send_replace(true);
        };
        if let Err(error) = axum::serve(listener, router)
            .with_graceful_shutdown(graceful)
            .await
        {
            tracing::error!(%error, "the node link's listener failed");
        }
    })
}

/// What the handlers share.
struct Link<P: Plane> {
    node: NodeHandle<P>,
    /// Set once the server stops: the subscriptions end.
    stopping: watch::Receiver<bool>,
}

impl<P: Plane> Clone for Link<P> {
    fn clone(&self) -> Self {
        Self {
            node: self.node.clone(),
            stopping: self.stopping.clone(),
        }
    }
}

fn router<P: Plane>(
    node: NodeHandle<P>,
    secret: ClusterSecret,
    stopping: watch::Receiver<bool>,
) -> Router {
    Router::new()
        .route(CALL, post(call::<P>))
        .route(ACTIVATE, post(activate::<P>))
        .route(SUBSCRIBE, post(subscribe::<P>))
        // A history or an activation of the deployment's own nodes: it is held in memory on both
        // sides anyway.
        .layer(DefaultBodyLimit::disable())
        .route_layer(middleware::from_fn_with_state(secret, admit))
        .with_state(Link { node, stopping })
}

/// Lets through a request that carries the deployment's secret and speaks this link's version.
async fn admit(State(secret): State<ClusterSecret>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if !presented.is_some_and(|presented| secret.matches(presented)) {
        return (
            StatusCode::UNAUTHORIZED,
            [(WWW_AUTHENTICATE, "Bearer")],
            "the request does not carry the cluster's secret",
        )
            .into_response();
    }
    match version(request.headers()) {
        Some(LINK_VERSION) => next.run(request).await,
        Some(other) => refused(format!(
            "this node speaks the node link version {LINK_VERSION}, the request version {other}: \
             every node of a deployment runs the same link"
        )),
        None => refused(format!(
            "the request names no version of the node link ({VERSION_HEADER}); this node speaks \
             version {LINK_VERSION}"
        )),
    }
}

fn version(headers: &HeaderMap) -> Option<u32> {
    headers.get(VERSION_HEADER)?.to_str().ok()?.parse().ok()
}

fn refused(message: String) -> Response {
    (StatusCode::BAD_REQUEST, message).into_response()
}

fn invalid(invalid: &Invalid) -> Response {
    refused(invalid.to_string())
}

/// The node could not take the request: it has stopped.
fn unavailable(error: &LinkError) -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()
}

async fn call<P: Plane>(
    State(link): State<Link<P>>,
    Json(request): Json<dto::NodeRequest>,
) -> Response {
    let request = match link::NodeRequest::try_from(request) {
        Ok(request) => request,
        Err(error) => return invalid(&error),
    };
    match link.node.serve(request).await {
        Ok(reply) => Json(dto::NodeReply::from(&reply)).into_response(),
        Err(error) => unavailable(&error),
    }
}

async fn activate<P: Plane>(
    State(link): State<Link<P>>,
    Json(request): Json<dto::ActivationRequest>,
) -> Response {
    let request = match link::ActivationRequest::try_from(request) {
        Ok(request) => request,
        Err(error) => return invalid(&error),
    };
    match link.node.serve_activation(request).await {
        Ok(result) => Json(dto::ActivationResult::from(&result)).into_response(),
        Err(error) => unavailable(&error),
    }
}

/// The changes of the key, as server-sent events `changed`, until the subscriber closes the
/// stream or the server stops; a comment after [`KEEP_ALIVE`] without a change.
async fn subscribe<P: Plane>(
    State(link): State<Link<P>>,
    Json(subscribe): Json<dto::Subscribe>,
) -> Response {
    let key = match link::SubscriptionKey::try_from(subscribe.key) {
        Ok(key) => key,
        Err(error) => return invalid(&error),
    };
    let changes = match link.node.subscribe(key, subscribe.from) {
        Ok(changes) => changes,
        Err(error) => return unavailable(&error),
    };
    let mut stopping = link.stopping;
    let stopped = async move {
        let _ = stopping.wait_for(|stopping| *stopping).await;
    };
    let events = changes
        .map(|changed| {
            Event::default()
                .event("changed")
                .json_data(dto::Changed::from(&changed))
        })
        .take_until(stopped);
    Sse::new(events)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}
