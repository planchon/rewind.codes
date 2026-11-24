//! Stopping a server: its node hands its partitions over at once, and the next owner rebuilds
//! the calls in flight from the records, as after a crash: a call under `retry` is sent
//! again with its idempotency key, so the handler recognizes it, and the run goes on.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rewind_api::access::BuiltInRole;
use rewind_api::action_sets::{ActionPolicy, CreateActionSet, McpServer, OnInterruption};
use rewind_api::common::RunStatus;
use rewind_api::runs::StartRun;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::{Value, json};
use support::{TestServer, eventually, wait};

/// An MCP server with one action, `slow`, which answers after `ms` milliseconds.
#[derive(Debug, Clone, Default)]
struct Slow {
    /// Calls received, and calls answered.
    calls: Arc<Mutex<(usize, usize)>>,
}

impl ServerHandler for Slow {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let Value::Object(schema) = json!({
            "type": "object",
            "properties": { "ms": { "type": "integer" } },
            "required": ["ms"]
        }) else {
            unreachable!()
        };
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "slow",
            "Answers after `ms` milliseconds.",
            Arc::new(schema),
        )]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.calls.lock().unwrap().0 += 1;
        let ms = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("ms"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(ms)).await;
        self.calls.lock().unwrap().1 += 1;
        Ok(CallToolResult::success(vec![ContentBlock::text("done")]).into())
    }
}

/// The MCP server listens on 127.0.0.1, which a deployed server reaches only when allowed.
const INTERNAL_MCP: &[&str] = &["--allow-internal-addresses", "127.0.0.1"];

/// Serves `slow` over streamable HTTP on 127.0.0.1; returns its URL.
async fn serve(slow: Slow) -> String {
    let service = StreamableHttpService::new(
        move || Ok(slow.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_sse_keep_alive(None),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}/mcp")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_in_flight_at_a_stop_is_sent_again_by_the_next_owner() {
    let slow = Slow::default();
    let url = serve(slow.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = format!("turso:{}", dir.path().join("control.db").display());
    let blobs = dir.path().join("blobs");

    // The first server runs on a runtime of its own, dropped once the server has stopped, as
    // a process that exits drops it: what the stop did not wait for is lost.
    let (path, first_store, first_blobs, counted) = (
        dir.path().to_owned(),
        store.clone(),
        blobs.clone(),
        slow.clone(),
    );
    let run = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let server =
                TestServer::start_in(&path, &first_store, &first_blobs, INTERNAL_MCP).await;
            let harness = server.key(BuiltInRole::Harness, &[]).await;
            // Retried: a call interrupted by the stop is sent again by the next owner.
            let policy = ActionPolicy {
                on_interruption: Some(OnInterruption::Retry),
                ..ActionPolicy::default()
            };
            let action_set = harness
                .create_action_set(&CreateActionSet {
                    mcp: vec![McpServer {
                        name: "fixture".into(),
                        url: Some(url),
                        policies: BTreeMap::from([("slow".to_owned(), policy)]),
                        ..McpServer::default()
                    }],
                    ..CreateActionSet::default()
                })
                .await
                .unwrap();
            let mut start = StartRun::new(
                r#"import { fixture } from "rewind:actions";
export default async function main() {
  const answer = await fixture.slow({ ms: 1500 });
  return answer.text();
}
"#,
            );
            start.action_set = Some(action_set.id);
            let run = harness.start_run(&start, None).await.unwrap();

            // The server stops while the MCP server works on the call.
            eventually(|| async { (counted.calls.lock().unwrap().0 == 1).then_some(()) }).await;
            server.stop().await;
            run.id
        })
    })
    .join()
    .unwrap();
    assert_eq!(slow.calls.lock().unwrap().0, 1, "one call before the stop");

    // Another server on the same store sends the call again and finishes the run with its
    // result.
    let server = TestServer::start_in(dir.path(), &store, &blobs, INTERNAL_MCP).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let ended = harness.get_run(&run, wait("30s")).await.unwrap();
    assert_eq!(ended.status, RunStatus::Completed, "{:?}", ended.error);
    assert_eq!(
        ended.output.and_then(|output| output.value),
        Some(json!("done"))
    );
    assert_eq!(
        slow.calls.lock().unwrap().0,
        2,
        "the call was sent again once"
    );
    server.stop().await;
}
