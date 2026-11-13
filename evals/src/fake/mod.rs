//! Deterministic fake MCP servers: an issue tracker, a chat, a CRM, a shop, an ops platform and
//! an office suite, each built from a fixed fixture, served with `rmcp` over streamable HTTP on
//! a local port, and called by the real `McpDispatcher`.
//!
//! A server answers from its state and the virtual time of the episode only, so the same calls
//! give the same answers on every run. Each keeps the calls it received (with their
//! idempotency key) and shows its state as JSON, for the tasks' checkers.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rewind_core::Timestamp;
use rewind_engine::ports::Clock;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::clock::SharedClock;

pub mod chat;
pub mod crm;
pub mod office;
pub mod ops;
pub mod shop;
pub mod tracker;

/// What a server knows of a call besides its arguments.
#[derive(Debug, Clone)]
pub struct CallContext {
    /// The episode's virtual time.
    pub now: Timestamp,
    /// `_meta["codes.rewind/idempotency-key"]`: the same for every attempt of one call.
    pub idempotency_key: Option<String>,
}

/// A server's answer to a call.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// A result as JSON in a text block, as most servers answer: the script reads it with
    /// `.json()`.
    Json(Value),
    /// A result as `structuredContent` (and as text), for a tool with an output schema.
    Structured(Value),
    /// `isError: true`: the tool reports a failure. The script gets a `ToolError` with this
    /// message, and `data` as its `data`.
    Error {
        message: String,
        data: Option<Value>,
    },
    /// JSON-RPC internal error: a transient failure, which the engine retries with backoff.
    Internal(String),
}

impl Reply {
    pub fn error(message: impl Into<String>) -> Self {
        Reply::Error {
            message: message.into(),
            data: None,
        }
    }
}

/// A tool of a fake server.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub input: Value,
    pub output: Option<Value>,
}

impl ToolSpec {
    pub fn new(name: &'static str, description: &'static str, input: Value) -> Self {
        Self {
            name,
            description,
            input,
            output: None,
        }
    }

    pub fn output(mut self, schema: Value) -> Self {
        self.output = Some(schema);
        self
    }
}

/// A request a server sends to a URL it was given (a webhook), at a time of the episode.
#[derive(Debug, Clone, PartialEq)]
pub struct Outbound {
    /// Unique per server: the delivery id, used as the signal's idempotency key.
    pub id: String,
    pub url: String,
    pub payload: Value,
    pub at: Timestamp,
}

/// The requests a server will send, in time order.
#[derive(Debug, Default)]
pub struct Outbox(Mutex<Vec<Outbound>>);

impl Outbox {
    pub fn push(&self, outbound: Outbound) {
        let mut queue = lock(&self.0);
        queue.push(outbound);
        queue.sort_by_key(|outbound| outbound.at);
    }

    /// The time of the next request.
    pub fn next(&self) -> Option<Timestamp> {
        lock(&self.0).first().map(|outbound| outbound.at)
    }

    /// Removes and returns the requests due at `now`.
    pub fn take_due(&self, now: Timestamp) -> Vec<Outbound> {
        let mut queue = lock(&self.0);
        let due = queue
            .iter()
            .take_while(|outbound| outbound.at <= now)
            .count();
        queue.drain(..due).collect()
    }
}

/// The behavior of a fake server.
pub trait Service: Send + Sync + 'static {
    /// The server's name: the namespace of its tools in `rewind:tools`.
    fn name(&self) -> &'static str;
    fn tools(&self) -> Vec<ToolSpec>;
    /// Answers a call. The arguments were checked against the tool's input schema by the
    /// sandbox before the call was recorded.
    fn call(&self, context: &CallContext, tool: &str, args: &Map<String, Value>) -> Reply;
    /// The server's state, for checkers and reports.
    fn snapshot(&self) -> Value;
    /// The webhooks the server sends, if it sends any.
    fn outbox(&self) -> Option<&Outbox> {
        None
    }
}

/// A call a server received.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CallRecord {
    pub tool: String,
    pub args: Value,
    pub idempotency_key: Option<String>,
    #[serde(serialize_with = "serialize_time")]
    pub at: Timestamp,
}

fn serialize_time<S: serde::Serializer>(
    time: &Timestamp,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&crate::clock::rfc3339(*time))
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic in a handler leaves the state as it was at the panic: still the best answer.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
struct Handler {
    service: Arc<dyn Service>,
    clock: SharedClock,
    calls: Arc<Mutex<Vec<CallRecord>>>,
}

fn object(value: Value) -> Arc<Map<String, Value>> {
    match value {
        Value::Object(object) => Arc::new(object),
        _ => Arc::new(Map::new()),
    }
}

fn text_of(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

impl ServerHandler for Handler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = self
            .service
            .tools()
            .into_iter()
            .map(|spec| {
                let mut tool = Tool::new(spec.name, spec.description, object(spec.input));
                tool.output_schema = spec.output.map(object);
                tool
            })
            .collect();
        Ok(ListToolsResult::with_all_items(tools))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.clone().unwrap_or_default();
        let mut meta = context.meta.0.0.clone();
        if let Some(own) = &request.meta {
            meta.extend(own.0.0.clone());
        }
        let idempotency_key = meta
            .get(rewind_mcp::IDEMPOTENCY_KEY_META)
            .and_then(Value::as_str)
            .map(str::to_owned);
        let call = CallContext {
            now: self.clock.now(),
            idempotency_key: idempotency_key.clone(),
        };
        lock(&self.calls).push(CallRecord {
            tool: request.name.to_string(),
            args: Value::Object(args.clone()),
            idempotency_key,
            at: call.now,
        });
        if !self
            .service
            .tools()
            .iter()
            .any(|tool| tool.name == request.name.as_ref())
        {
            return Err(ErrorData::invalid_params(
                format!("unknown tool {}", request.name),
                None,
            ));
        }
        match self.service.call(&call, &request.name, &args) {
            Reply::Json(value) => {
                Ok(CallToolResult::success(vec![ContentBlock::text(text_of(&value))]).into())
            }
            Reply::Structured(value) => {
                let mut result = CallToolResult::success(vec![ContentBlock::text(text_of(&value))]);
                result.structured_content = Some(value);
                Ok(result.into())
            }
            Reply::Error { message, data } => {
                let mut result = CallToolResult::error(vec![ContentBlock::text(message)]);
                result.structured_content = data;
                Ok(result.into())
            }
            Reply::Internal(message) => Err(ErrorData::internal_error(message, None)),
        }
    }
}

/// A fake server listening on a local port.
#[derive(Clone)]
pub struct Running {
    pub service: Arc<dyn Service>,
    /// `http://127.0.0.1:<port>/mcp`.
    pub url: String,
    calls: Arc<Mutex<Vec<CallRecord>>>,
    stop: CancellationToken,
}

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Running")
            .field("server", &self.service.name())
            .field("url", &self.url)
            .finish_non_exhaustive()
    }
}

impl Running {
    /// The calls received so far, in order.
    pub fn calls(&self) -> Vec<CallRecord> {
        lock(&self.calls).clone()
    }

    pub fn stop(&self) {
        self.stop.cancel();
    }
}

/// Serves `service` over streamable HTTP at `/mcp`, on a free port of 127.0.0.1, until
/// [`Running::stop`].
pub async fn serve(service: Arc<dyn Service>, clock: SharedClock) -> std::io::Result<Running> {
    let stop = CancellationToken::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let handler = Handler {
        service: service.clone(),
        clock,
        calls: calls.clone(),
    };
    let mcp = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_sse_keep_alive(None)
            .with_cancellation_token(stop.child_token()),
    );
    let router = axum::Router::new().nest_service("/mcp", mcp);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let shutdown = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
            .await;
    });
    Ok(Running {
        service,
        url: format!("http://{address}/mcp"),
        calls,
        stop,
    })
}

// -------------------------------------------------------------------------------------------
// Helpers for the servers

/// Reading arguments. The sandbox checked them against the input schema, so a missing
/// required argument is a bug of the schema: it answers as a server would.
pub(crate) struct Args<'a>(pub(crate) &'a Map<String, Value>);

impl Args<'_> {
    pub(crate) fn str(&self, key: &str) -> Result<&str, Reply> {
        self.0
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| Reply::error(format!("`{key}` must be a string")))
    }

    pub(crate) fn opt_str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    pub(crate) fn u64(&self, key: &str) -> Result<u64, Reply> {
        self.0
            .get(key)
            .and_then(Value::as_u64)
            .ok_or_else(|| Reply::error(format!("`{key}` must be a non-negative integer")))
    }

    pub(crate) fn opt_u64(&self, key: &str) -> Option<u64> {
        self.0.get(key).and_then(Value::as_u64)
    }

    pub(crate) fn strings(&self, key: &str) -> Vec<String> {
        self.0
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// One page of `items`: from the offset the cursor holds, at most `limit` items, and the
/// cursor of the next page if there is one. Cursors are opaque to scripts.
pub(crate) fn page<T: Clone>(
    items: &[T],
    cursor: Option<&str>,
    limit: usize,
) -> Result<(Vec<T>, Option<String>), Reply> {
    let start = match cursor {
        None | Some("") => 0,
        Some(cursor) => cursor
            .strip_prefix("page_")
            .and_then(|offset| offset.parse::<usize>().ok())
            .filter(|offset| *offset <= items.len())
            .ok_or_else(|| Reply::error(format!("invalid cursor {cursor:?}")))?,
    };
    let end = (start + limit).min(items.len());
    let next = (end < items.len()).then(|| format!("page_{end}"));
    Ok((items[start..end].to_vec(), next))
}

/// The JSON Schema of an object with these properties, of which `required` are required, and
/// no other.
pub(crate) fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// A tiny deterministic generator for the fixtures (SplitMix64).
#[derive(Debug, Clone)]
pub(crate) struct Fixed(u64);

impl Fixed {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// In `0..n`.
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    pub(crate) fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        let index = usize::try_from(self.below(items.len() as u64)).unwrap_or(0);
        &items[index]
    }
}

/// The fake servers an episode can use, by name.
pub fn by_name(name: &str) -> Option<Arc<dyn Service>> {
    Some(match name {
        "tracker" => Arc::new(tracker::Tracker::new()),
        "chat" => Arc::new(chat::Chat::new()),
        "crm" => Arc::new(crm::Crm::new()),
        "shop" => Arc::new(shop::Shop::new()),
        "ops" => Arc::new(ops::Ops::new()),
        "office" => Arc::new(office::Office::new()),
        _ => return None,
    })
}

/// Every server's name.
pub const SERVERS: [&str; 6] = ["tracker", "chat", "crm", "shop", "ops", "office"];

/// The state of a set of servers, by name.
pub fn snapshots(servers: &[Running]) -> BTreeMap<String, Value> {
    servers
        .iter()
        .map(|server| (server.service.name().to_owned(), server.service.snapshot()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_follow_their_cursors_to_the_end() {
        let items: Vec<u32> = (0..45).collect();
        let (first, next) = page(&items, None, 20).unwrap();
        assert_eq!(first.len(), 20);
        assert_eq!(next.as_deref(), Some("page_20"));
        let (second, next) = page(&items, next.as_deref(), 20).unwrap();
        assert_eq!(second[0], 20);
        let (last, next) = page(&items, next.as_deref(), 20).unwrap();
        assert_eq!(last.len(), 5);
        assert_eq!(next, None);
        assert!(page(&items, Some("bogus"), 20).is_err());
    }

    #[test]
    fn the_outbox_hands_out_what_is_due_in_time_order() {
        let outbox = Outbox::default();
        for (id, at) in [("b", 20), ("a", 10)] {
            outbox.push(Outbound {
                id: id.into(),
                url: "https://rewind.example/hooks/x".into(),
                payload: Value::Null,
                at: Timestamp(at),
            });
        }
        assert_eq!(outbox.next(), Some(Timestamp(10)));
        assert!(outbox.take_due(Timestamp(5)).is_empty());
        let due = outbox.take_due(Timestamp(15));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "a");
        assert_eq!(outbox.next(), Some(Timestamp(20)));
    }

    #[test]
    fn fixtures_are_the_same_every_time() {
        let mut a = Fixed::new(7);
        let mut b = Fixed::new(7);
        let first: Vec<u64> = (0..5).map(|_| a.below(100)).collect();
        let second: Vec<u64> = (0..5).map(|_| b.below(100)).collect();
        assert_eq!(first, second);
    }
}
