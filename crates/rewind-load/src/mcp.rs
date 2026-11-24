//! The fake MCP server of the load: two actions, served over streamable HTTP with `rmcp`, which
//! record every call by its idempotency key.
//!
//! | Action | Answers |
//! | --- | --- |
//! | `echo` | its arguments, `{ n, pad }` |
//! | `flaky` | a JSON-RPC internal error (a transient failure) at the first call of each key, then `{ n, calls }` |
//!
//! The record shows how many times each call reached the server: the load compares it with the
//! failures each call met, which proves no call was sent again beyond its retries.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::Error;

/// The `_meta` key of the idempotency key, as `rewind-mcp` sends it (`archi/04-tools.md`,
/// "Routes").
pub(crate) const IDEMPOTENCY_KEY_META: &str = "codes.rewind/idempotency-key";

/// The calls of one idempotency key.
#[derive(Debug, Clone)]
pub(crate) struct Calls {
    pub(crate) action: String,
    pub(crate) count: u32,
    /// When the first call arrived.
    pub(crate) first: Instant,
}

/// What the server saw, by idempotency key (`{run_id}/{seq}`).
#[derive(Debug, Default)]
pub(crate) struct Record {
    pub(crate) calls: HashMap<String, Calls>,
    /// Calls without an idempotency key: never expected.
    pub(crate) without_key: u64,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct FakeServer {
    record: Arc<Mutex<Record>>,
}

impl FakeServer {
    pub(crate) fn record(&self) -> MutexGuard<'_, Record> {
        self.record
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records a call; returns how many times this key was called, this call included.
    pub(crate) fn called(&self, key: Option<&str>, action: &str) -> u32 {
        let mut record = self.record();
        let Some(key) = key else {
            record.without_key += 1;
            return 1;
        };
        let calls = record.calls.entry(key.to_owned()).or_insert_with(|| Calls {
            action: action.to_owned(),
            count: 0,
            first: Instant::now(),
        });
        calls.count += 1;
        calls.count
    }
}

fn schema(value: Value) -> Arc<Map<String, Value>> {
    match value {
        Value::Object(object) => Arc::new(object),
        _ => Arc::new(Map::new()),
    }
}

/// The actions, with output schemas: a script gets their structured content as it is.
fn tools() -> Vec<Tool> {
    let input = json!({
        "type": "object",
        "properties": { "n": { "type": "integer" } },
        "required": ["n"]
    });
    let padded = json!({
        "type": "object",
        "properties": { "n": { "type": "integer" }, "pad": { "type": "string" } },
        "required": ["n", "pad"]
    });
    let mut echo = Tool::new("echo", "Returns its arguments.", schema(padded.clone()));
    echo.output_schema = Some(schema(padded));
    let mut flaky = Tool::new(
        "flaky",
        "Fails transiently the first time, then returns its argument.",
        schema(input),
    );
    flaky.output_schema = Some(schema(json!({
        "type": "object",
        "properties": { "n": { "type": "integer" }, "calls": { "type": "integer" } },
        "required": ["n", "calls"]
    })));
    vec![echo, flaky]
}

impl ServerHandler for FakeServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let mut meta = context.meta.0.0.clone();
        if let Some(own) = &request.meta {
            meta.extend(own.0.0.clone());
        }
        let key = meta.get(IDEMPOTENCY_KEY_META).and_then(Value::as_str);
        let argument = |name: &str| {
            request
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get(name))
                .cloned()
                .unwrap_or(Value::Null)
        };
        let n = argument("n");
        let action = request.name.as_ref();
        let calls = self.called(key, action);
        let structured = match action {
            "echo" => json!({ "n": n, "pad": argument("pad") }),
            "flaky" if calls == 1 => {
                return Err(ErrorData::internal_error(
                    "flaky: the first attempt of each call fails",
                    None,
                ));
            }
            "flaky" => json!({ "n": n, "calls": calls }),
            other => {
                return Err(ErrorData::invalid_params(
                    format!("unknown action {other}"),
                    None,
                ));
            }
        };
        let mut result = CallToolResult::success(vec![ContentBlock::text(structured.to_string())]);
        result.structured_content = Some(structured);
        Ok(result.into())
    }
}

/// Serves `server` at `/mcp` on `listen` until `stop`, accepting the `Host` of `url`.
pub(crate) async fn serve(
    server: FakeServer,
    listen: SocketAddr,
    url: &url::Url,
    stop: CancellationToken,
) -> Result<SocketAddr, Error> {
    let mut hosts = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
    if let Some(host) = url.host_str() {
        hosts.push(host.to_owned());
    }
    let sessions = server.clone();
    let service = StreamableHttpService::new(
        move || Ok(sessions.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_sse_keep_alive(None)
            .with_allowed_hosts(hosts)
            .with_cancellation_token(stop.child_token()),
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .map_err(|error| Error::new(format!("cannot listen on {listen}: {error}")))?;
    let address = listener
        .local_addr()
        .map_err(|error| Error::new(error.to_string()))?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { stop.cancelled_owned().await })
            .await;
    });
    Ok(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_are_counted_by_key() {
        let server = FakeServer::default();
        assert_eq!(server.called(Some("r/1"), "echo"), 1);
        assert_eq!(server.called(Some("r/2"), "flaky"), 1);
        assert_eq!(server.called(Some("r/2"), "flaky"), 2);
        assert_eq!(server.called(None, "echo"), 1);
        let record = server.record();
        assert_eq!(record.calls["r/2"].count, 2);
        assert_eq!(record.calls["r/2"].action, "flaky");
        assert_eq!(record.without_key, 1);
    }
}
