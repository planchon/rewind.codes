//! Where a deployed server's MCP client goes (`McpEgress::checked`). The servers of these tests
//! listen on 127.0.0.1: for a deployment, internal services. They are refused unless the
//! operator's allow-list lets them through, and once let through, what they answer is never
//! quoted to the tenant. The embedded server's configuration (`McpEgress::open`) reaches them.

use std::collections::{BTreeMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use rewind_core::{Tool, ToolFailureKind, ToolRoute};
use rewind_engine::ports::ToolDispatcher;
use rewind_mcp::{
    InternalAllowList, McpConfig, McpDispatcher, McpEgress, McpError, NameResolver, Resolving,
};
use serde_json::json;

use crate::fixture::{self, Auth, Fixture};
use crate::{call, failed, http_server, open, result, tenant};

const REFUSAL: &str = "MCP servers are not reached there unless the operator allows it";

/// A deployed server's dispatcher, letting `internal` through, resolving with the system.
fn deployed(internal: &[&str]) -> McpDispatcher {
    McpDispatcher::new(McpConfig {
        egress: McpEgress::checked(InternalAllowList::parse(internal).unwrap()),
        ..McpConfig::default()
    })
}

/// A deployed server's dispatcher resolving with `resolver`.
fn deployed_with(resolver: Arc<Scripted>, internal: &[&str]) -> McpDispatcher {
    McpDispatcher::new(McpConfig {
        egress: McpEgress::checked_with(resolver, InternalAllowList::parse(internal).unwrap()),
        ..McpConfig::default()
    })
}

/// Answers names from a script: each name has a list of answers, the last one repeated.
#[derive(Debug, Default)]
struct Scripted(Mutex<BTreeMap<String, VecDeque<Vec<IpAddr>>>>);

impl Scripted {
    fn new(answers: &[(&str, &[&str])]) -> Arc<Self> {
        let mut script: BTreeMap<String, VecDeque<Vec<IpAddr>>> = BTreeMap::new();
        for (name, addresses) in answers {
            script
                .entry((*name).to_owned())
                .or_default()
                .push_back(addresses.iter().map(|a| a.parse().unwrap()).collect());
        }
        Arc::new(Self(Mutex::new(script)))
    }
}

impl NameResolver for Scripted {
    fn resolve(&self, host: &str) -> Resolving {
        let mut script = self.0.lock().unwrap();
        let answer = match script.get_mut(host) {
            Some(queue) if queue.len() > 1 => queue.pop_front(),
            Some(queue) => queue.front().cloned(),
            None => None,
        };
        Box::pin(async move {
            answer.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "unknown name"))
        })
    }
}

/// A server on 127.0.0.1 answering every request with `status`, `content_type` and `body`.
async fn serve_page(status: StatusCode, content_type: &'static str, body: &'static str) -> u16 {
    let router = axum::Router::new().fallback(move || async move {
        (status, [("content-type", content_type)], body).into_response()
    });
    serve(router).await
}

/// A server on 127.0.0.1 redirecting every request to `location` with a 307.
async fn serve_redirect(location: String) -> u16 {
    let router = axum::Router::new().fallback(move || {
        let location = location.clone();
        async move { (StatusCode::TEMPORARY_REDIRECT, [("location", location)]).into_response() }
    });
    serve(router).await
}

async fn serve(router: axum::Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await });
    port
}

/// A listener on 127.0.0.1 that only counts the connections it accepts: an internal service.
async fn internal_service() -> (u16, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let counted = accepted.clone();
    tokio::spawn(async move {
        while listener.accept().await.is_ok() {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });
    (port, accepted)
}

fn refused_message(error: &McpError) -> &str {
    match error {
        McpError::Refused { message, .. } => message,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

pub(crate) async fn a_local_server_is_refused_by_a_deployed_server() {
    let fixture = Fixture::new();
    let server = fixture::serve_http(fixture.clone(), Auth::default()).await;
    let mcp = http_server(&server.url);
    let dispatcher = McpDispatcher::default();

    // Its address, at registration.
    let error = dispatcher
        .list_tools(&tenant(), &mcp, None)
        .await
        .unwrap_err();
    assert_eq!(
        error,
        McpError::Refused {
            server: "fx".into(),
            message: format!("fx cannot be reached: 127.0.0.1 is a loopback address: {REFUSAL}"),
        }
    );
    // A name resolving to it.
    let by_name = http_server(&server.url.replace("127.0.0.1", "localhost"));
    let error = dispatcher
        .list_tools(&tenant(), &by_name, None)
        .await
        .unwrap_err();
    let message = refused_message(&error);
    assert!(
        message.starts_with("fx cannot be reached: localhost resolves to ")
            && message.ends_with(&format!(", a loopback address: {REFUSAL}")),
        "{message}"
    );
    // A call, at each connection: it fails for good.
    let lookup = Tool::new(
        "fx.lookup",
        ToolRoute::Mcp {
            server: "fx".into(),
        },
        json!({ "type": "object" }),
    );
    let failure = failed(
        dispatcher
            .dispatch(call(&mcp, &lookup, json!({ "id": "c-1" })), None)
            .await,
    );
    assert_eq!(failure.kind, ToolFailureKind::Unavailable);
    assert_eq!(
        failure.message,
        format!("fx cannot be reached: 127.0.0.1 is a loopback address: {REFUSAL}")
    );
    // The cloud metadata endpoint, without even trying.
    let metadata = http_server("http://169.254.169.254/latest/meta-data/");
    let error = dispatcher
        .list_tools(&tenant(), &metadata, None)
        .await
        .unwrap_err();
    assert!(
        refused_message(&error).contains("a link-local (cloud metadata) address"),
        "{error:?}"
    );
    assert_eq!(fixture.state().sessions, 0, "nothing reached the server");

    // The embedded server's configuration reaches it.
    let tools = open().list_tools(&tenant(), &mcp, None).await.unwrap();
    assert_eq!(tools.len(), 8);
    assert_eq!(fixture.state().sessions, 1);
}

pub(crate) async fn an_allowed_internal_server_is_reached() {
    let fixture = Fixture::new();
    let server = fixture::serve_http(fixture.clone(), Auth::default()).await;
    let mcp = http_server(&server.url);

    // By network: discovery, then a call.
    let dispatcher = deployed(&["127.0.0.0/8"]);
    let tools = dispatcher.list_tools(&tenant(), &mcp, None).await.unwrap();
    assert_eq!(tools.len(), 8);
    let lookup = tools.iter().find(|tool| tool.name == "fx.lookup").unwrap();
    result(
        dispatcher
            .dispatch(call(&mcp, lookup, json!({ "id": "c-1" })), None)
            .await,
    );

    // By name: the name, not its addresses under another name.
    let dispatcher = deployed(&["localhost"]);
    let by_name = http_server(&server.url.replace("127.0.0.1", "localhost"));
    assert_eq!(
        dispatcher
            .list_tools(&tenant(), &by_name, None)
            .await
            .unwrap()
            .len(),
        8
    );
    let error = dispatcher
        .list_tools(&tenant(), &mcp, None)
        .await
        .unwrap_err();
    assert!(
        refused_message(&error).contains("127.0.0.1 is a loopback address"),
        "{error:?}"
    );

    // Another network lets nothing through here.
    let error = deployed(&["10.0.0.0/8"])
        .list_tools(&tenant(), &mcp, None)
        .await
        .unwrap_err();
    assert!(
        refused_message(&error).contains("127.0.0.1 is a loopback address"),
        "{error:?}"
    );
}

pub(crate) async fn an_internal_server_s_answers_are_not_quoted() {
    const SECRET: &str = "the admin console of the payments service: secret=hunter2";
    let page = serve_page(StatusCode::NOT_FOUND, "text/plain", SECRET).await;
    let json = serve_page(
        StatusCode::OK,
        "application/json",
        r#"{"secret":"hunter2"}"#,
    )
    .await;
    let failing = serve_page(StatusCode::BAD_GATEWAY, "text/plain", SECRET).await;
    for port in [page, json, failing] {
        let mcp = http_server(&format!("http://127.0.0.1:{port}/admin"));
        let internal = deployed(&["127.0.0.1"])
            .list_tools(&tenant(), &mcp, None)
            .await
            .unwrap_err();
        assert!(
            !internal.to_string().contains("hunter2")
                && internal
                    .to_string()
                    .contains("the answer of an internal server is not quoted"),
            "{port}: {internal:?}"
        );
        if port == page {
            assert_eq!(
                internal,
                McpError::Refused {
                    server: "fx".into(),
                    message: "fx refused the request (HTTP 404); the answer of an internal \
                              server is not quoted"
                        .into()
                }
            );
        }
        // The embedded server shows its developer what their own server answered (rmcp does
        // not read the body of a 5xx that is not JSON).
        let local = open().list_tools(&tenant(), &mcp, None).await.unwrap_err();
        assert_eq!(
            local.to_string().contains("hunter2"),
            port != failing,
            "{port}: {local:?}"
        );
    }

    // A name allowed, resolving to an internal address.
    let resolver = Scripted::new(&[("payments.internal", &["127.0.0.1"])]);
    let mcp = http_server(&format!("http://payments.internal:{page}/admin"));
    let internal = deployed_with(resolver, &["payments.internal"])
        .list_tools(&tenant(), &mcp, None)
        .await
        .unwrap_err();
    assert!(!internal.to_string().contains("hunter2"), "{internal:?}");
}

pub(crate) async fn redirects_go_nowhere_else() {
    let fixture = Fixture::new();
    let server = fixture::serve_http(fixture.clone(), Auth::default()).await;
    let port = server
        .url
        .split(':')
        .nth(2)
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    let port: u16 = port.parse().unwrap();

    // Within the server's origin, a 307 is followed: the session works through it.
    let resolver = Scripted::new(&[("mcp.test", &["127.0.0.1"])]);
    let moved = http_server(&format!("http://mcp.test:{port}/moved"));
    let tools = deployed_with(resolver, &["mcp.test"])
        .list_tools(&tenant(), &moved, None)
        .await
        .unwrap();
    assert_eq!(tools.len(), 8);

    // To a private address: another origin, never reached.
    let (internal_port, accepted) = internal_service().await;
    let redirecting = serve_redirect(format!("http://127.0.0.1:{internal_port}/mcp")).await;
    let resolver = Scripted::new(&[("mcp.test", &["127.0.0.1"])]);
    let mcp = http_server(&format!("http://mcp.test:{redirecting}/mcp"));
    let error = deployed_with(resolver, &["mcp.test"])
        .list_tools(&tenant(), &mcp, None)
        .await
        .unwrap_err();
    assert_eq!(
        refused_message(&error),
        "fx cannot be reached: it redirects to another origin: MCP requests go to the server's \
         URL only"
    );
    // The embedded server does not follow it either: the credentials would go with it.
    let by_address = http_server(&format!("http://127.0.0.1:{redirecting}/mcp"));
    let error = open()
        .list_tools(&tenant(), &by_address, None)
        .await
        .unwrap_err();
    assert!(
        refused_message(&error).contains("it redirects to another origin"),
        "{error:?}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(accepted.load(Ordering::SeqCst), 0, "nothing reached it");

    // Within the origin, when the name now resolves to a private address: each connection is
    // checked again, and the redirect's is refused.
    let resolver = Scripted::new(&[
        ("rebind.test", &["127.0.0.1"]),
        ("rebind.test", &["10.0.0.1"]),
    ]);
    let moved = http_server(&format!("http://rebind.test:{port}/moved"));
    let error = deployed_with(resolver, &["127.0.0.1"])
        .list_tools(&tenant(), &moved, None)
        .await
        .unwrap_err();
    assert_eq!(
        refused_message(&error),
        format!(
            "fx cannot be reached: rebind.test resolves to 10.0.0.1, a private address: {REFUSAL}"
        )
    );
}
