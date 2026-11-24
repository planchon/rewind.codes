//! Where a server's MCP client goes (`archi/04-tools.md`, "Action sets and code generation";
//! `archi/06-storage-and-scale.md`, "Security"). The MCP servers of these tests listen on
//! 127.0.0.1: for a deployed server, an internal service, refused unless the operator allows it
//! (`--allow-internal-addresses`), and never quoted to the tenant once allowed. The embedded server
//! reaches any address.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use axum::response::IntoResponse;
use clap::Parser;
use rewind_api::access::BuiltInRole;
use rewind_api::action_sets::{CreateActionSet, McpServer};
use rewind_api::{Client, ProblemType};
use rewind_server::ServerArgs;
use rmcp::ServerHandler;
use rmcp::model::{
    ErrorData, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::json;
use support::TestServer;

/// An MCP server with one action, `find`, counting the sessions opened.
#[derive(Debug, Clone, Default)]
struct Finder {
    sessions: Arc<AtomicUsize>,
}

impl ServerHandler for Finder {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let schema = json!({ "type": "object", "properties": { "q": { "type": "string" } } });
        let serde_json::Value::Object(schema) = schema else {
            unreachable!()
        };
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "find",
            "Finds things.",
            Arc::new(schema),
        )]))
    }
}

/// Serves `finder` over streamable HTTP on 127.0.0.1, and at `/admin` a page that is not MCP:
/// the server's URL, and the page's.
async fn serve(finder: Finder) -> (String, String) {
    let sessions = finder.clone();
    let service = StreamableHttpService::new(
        move || {
            sessions.sessions.fetch_add(1, Ordering::SeqCst);
            Ok(sessions.clone())
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_sse_keep_alive(None),
    );
    let router = axum::Router::new().nest_service("/mcp", service).route(
        "/admin",
        axum::routing::any(|| async {
            (StatusCode::NOT_FOUND, "the admin console: secret=hunter2").into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    (
        format!("http://{address}/mcp"),
        format!("http://{address}/admin"),
    )
}

fn action_set(url: &str) -> CreateActionSet {
    CreateActionSet {
        mcp: vec![McpServer {
            name: "finder".into(),
            url: Some(url.to_owned()),
            ..McpServer::default()
        }],
        ..CreateActionSet::default()
    }
}

fn problem(error: rewind_api::Error) -> rewind_api::Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deployed_server_refuses_mcp_servers_on_internal_addresses() {
    let finder = Finder::default();
    let (url, _) = serve(finder.clone()).await;
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;

    let refused = problem(
        harness
            .create_action_set(&action_set(&url))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::ActionSetUnavailable));
    assert_eq!(
        refused.detail,
        "finder cannot be reached: 127.0.0.1 is a loopback address: MCP servers and APIs are not \
         reached there unless the operator allows it"
    );
    let metadata = problem(
        harness
            .create_action_set(&action_set("http://169.254.169.254/latest/meta-data/"))
            .await
            .unwrap_err(),
    );
    assert!(
        metadata
            .detail
            .contains("169.254.169.254 is a link-local (cloud metadata) address"),
        "{}",
        metadata.detail
    );
    assert_eq!(finder.sessions.load(Ordering::SeqCst), 0);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_operator_allows_internal_mcp_servers_whose_answers_stay_unquoted() {
    let finder = Finder::default();
    let (url, admin) = serve(finder.clone()).await;
    let server = TestServer::start(&["--allow-internal-addresses", "127.0.0.1"]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;

    let registered = harness.create_action_set(&action_set(&url)).await.unwrap();
    let names: Vec<&str> = registered
        .actions
        .iter()
        .map(|action| action.name.as_str())
        .collect();
    assert_eq!(names, ["finder.find", "webhook"]);
    assert_eq!(finder.sessions.load(Ordering::SeqCst), 1);

    // What the internal server answers to a URL that is not MCP stays out of the problem.
    let refused = problem(
        harness
            .create_action_set(&action_set(&admin))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::ActionSetUnavailable));
    assert_eq!(
        refused.detail,
        "finder refused the request (HTTP 404); the answer of an internal server is not quoted"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_embedded_server_reaches_mcp_servers_anywhere() {
    let finder = Finder::default();
    let (url, admin) = serve(finder.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let rewind_dir = dir.path().join(".rewind");
    let args = ServerArgs::try_parse_from([
        "rewind server",
        "--embedded",
        "--dir",
        rewind_dir.to_str().unwrap(),
    ])
    .unwrap();
    let running = rewind_server::start(args).await.unwrap();
    let client = Client::embedded(running.socket().unwrap()).unwrap();

    let registered = client.create_action_set(&action_set(&url)).await.unwrap();
    assert_eq!(registered.actions.len(), 2);
    assert_eq!(finder.sessions.load(Ordering::SeqCst), 1);
    // Its developer reads what their own server answered.
    let refused = problem(
        client
            .create_action_set(&action_set(&admin))
            .await
            .unwrap_err(),
    );
    assert!(
        refused.detail.contains("secret=hunter2"),
        "{}",
        refused.detail
    );
    running.stop().await;
}
