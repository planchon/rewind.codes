//! The tool dispatcher of ai-ptc outside mock mode (`archi/07-code-architecture.md`, "Crates";
//! `archi/04-tools.md`, "Routes").
//!
//! | Route | Dispatcher | What it does |
//! | --- | --- | --- |
//! | MCP tools | [`McpDispatcher`] | An MCP client (`rmcp`), over streamable HTTP or stdio: discovery (`tools/list`, pinned by the toolset), calls with the idempotency key and the trace context in `_meta`, credentials injected into the transport, the execution timeout, the schema check before each call |
//! | Built-ins | [`HttpTool`] | `http`, with the egress rules: allow-list, one DNS resolution, private and metadata addresses refused, a connection to the checked address, redirects checked again, bounded responses. `webhook` is answered by the engine |
//!
//! A composition root plugs them into the engine's `RoutingDispatcher` (`mcp` and `builtins`).
//! The dispatchers never write to the journal; credentials go only into the transport, never
//! into a result, a failure or a log.
//!
//! What becomes of an attempt:
//!
//! ```text
//! connection (pooled by tenant, server and credentials) ── 401 ──► Park(credentials_expired)
//!   │                                                     └ unreachable ──► Transient
//!   ▼ unless approved after a schema change: the server's current tool
//!   │   gone, args refused by a new input schema, output schema changed ──► Park(schema_changed)
//!   ▼ tools/call with _meta { io.ai-ptc/idempotency-key, traceparent, tracestate }
//!   ├─ result ──► Result: structuredContent (output schema), else { content, structuredContent? }
//!   ├─ isError, JSON-RPC error ──► Failed (internal error -32603: Transient)
//!   └─ transport failure, 429, 5xx ──► Transient; past the execution timeout ──► Transient(timeout)
//! ```

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use ptc_core::{McpServer, Tool, ToolFailureKind, ToolRoute};
use ptc_engine::ports::{
    Credentials, Dispatched, ParkKind, ParkReason, ServerCredentials, TenantId, ToolCall,
    ToolDispatcher,
};
use rmcp::model::{CallToolRequestParams, JsonObject, MetaObject, RequestMetaObject};
use serde_json::Value;

mod connection;
mod failure;
mod http;
mod result;
mod schema;

pub use http::{
    HttpConfig, HttpTool, NameResolver, Resolving, SystemResolver, http_input_schema,
    http_output_schema, http_tool,
};

use connection::Pool;
use failure::Failure;

/// The `_meta` key of the idempotency key (`archi/04-tools.md`, "Routes").
pub const IDEMPOTENCY_KEY_META: &str = "io.ai-ptc/idempotency-key";

/// How [`McpDispatcher`] manages its connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpConfig {
    /// How long opening a session may take: connecting or starting the process, then
    /// initializing. Listing the tools at discovery gets as long again.
    pub connect_timeout: Duration,
    /// A connection unused this long is closed.
    pub idle_timeout: Duration,
    /// How long the tool list of a connection is trusted for the schema check, unless the server
    /// says it changed.
    pub tools_ttl: Duration,
    /// Connections open at most; beyond, the least recently used idle one is closed.
    pub max_connections: usize,
    /// The variables of the dispatcher's environment a stdio server inherits. Nothing else is
    /// inherited: the dispatcher's own secrets stay out of the servers it starts.
    pub inherited_env: Vec<String>,
    /// Logs what stdio servers write to their standard error, at debug level. Off by default:
    /// a server may print its own secrets there.
    pub log_stderr: bool,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(5 * 60),
            tools_ttl: Duration::from_secs(60),
            max_connections: 256,
            inherited_env: [
                "PATH",
                "HOME",
                "USER",
                "LOGNAME",
                "SHELL",
                "TMPDIR",
                "TEMP",
                "TMP",
                "LANG",
                "LC_ALL",
                "SYSTEMROOT",
                "WINDIR",
                "APPDATA",
                "LOCALAPPDATA",
                "USERPROFILE",
                "PATHEXT",
                "COMSPEC",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            log_stderr: false,
        }
    }
}

/// Why the tools of an MCP server could not be listed, what registering a toolset reports to
/// the harness developer, or why the `http` tool could not be set up. The message names the
/// server.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
    /// The server could not be reached or started, or its connection closed.
    #[error("{message}")]
    Unreachable { server: String, message: String },
    /// The server refused the credentials (HTTP 401).
    #[error("{message}")]
    Unauthorized { server: String, message: String },
    /// The server did not answer in time.
    #[error("{message}")]
    Timeout { server: String, message: String },
    /// The server answered, but not with its tools.
    #[error("{message}")]
    Refused { server: String, message: String },
    /// The HTTP client of the `http` tool could not be built.
    #[error("{message}")]
    Client { message: String },
}

impl McpError {
    /// The MCP server the error is about.
    pub fn server(&self) -> Option<&str> {
        match self {
            McpError::Unreachable { server, .. }
            | McpError::Unauthorized { server, .. }
            | McpError::Timeout { server, .. }
            | McpError::Refused { server, .. } => Some(server),
            McpError::Client { .. } => None,
        }
    }
}

/// Calls the tools of MCP servers: the `mcp` route of the engine's `RoutingDispatcher`. Cheap
/// to clone: clones share their connections.
#[derive(Debug, Clone)]
pub struct McpDispatcher {
    pool: Arc<Pool>,
}

impl Default for McpDispatcher {
    fn default() -> Self {
        Self::new(McpConfig::default())
    }
}

impl McpDispatcher {
    pub fn new(config: McpConfig) -> Self {
        Self {
            pool: Arc::new(Pool::new(config)),
        }
    }

    /// Discovery: the tools of a server, as a toolset pins them (`archi/04-tools.md`, "Toolsets
    /// and code generation"), sorted by name. Each is named `{server}.{tool}`, with its
    /// description (or its title), its input and output schemas, and the default policy of an
    /// MCP tool; the harness's policies are applied by whoever builds the toolset.
    pub async fn list_tools(
        &self,
        tenant: &TenantId,
        server: &McpServer,
        credentials: Option<&ServerCredentials>,
    ) -> Result<Vec<Tool>, McpError> {
        let lease = self
            .pool
            .connection(tenant, server, credentials)
            .await
            .map_err(|failure| failure.into_error(&server.name))?;
        let timeout = self.pool.config().connect_timeout;
        let listed = match tokio::time::timeout(timeout, lease.connection().tools(&server.name)).await
        {
            Ok(Ok(listed)) => listed,
            Ok(Err((failure, broken))) => {
                if broken {
                    lease.forget();
                }
                return Err(failure.into_error(&server.name));
            }
            Err(_) => {
                return Err(failure::timed_out(&server.name, timeout).into_error(&server.name));
            }
        };
        Ok(listed
            .values()
            .map(|tool| pinned(&server.name, tool))
            .collect())
    }

    /// Closes every connection, and stops the stdio servers.
    pub fn close(&self) {
        self.pool.close_all();
    }

    /// How many connections are open or opening.
    pub fn open_connections(&self) -> usize {
        self.pool.len()
    }

    async fn attempt(
        &self,
        call: &ToolCall,
        server: &McpServer,
        credentials: Option<&ServerCredentials>,
    ) -> Dispatched {
        let lease = match self
            .pool
            .connection(&call.run.tenant, server, credentials)
            .await
        {
            Ok(lease) => lease,
            Err(failure) => return failure.into_dispatched(),
        };
        let local = call.tool.local_name();
        if !call.schema_change_approved {
            match lease.connection().tools(&server.name).await {
                Ok(tools) => {
                    if let Some(message) =
                        schema::mismatch(&call.tool, tools.get(local), &call.args)
                    {
                        return Dispatched::Park(ParkReason {
                            kind: ParkKind::SchemaChanged,
                            message,
                        });
                    }
                }
                Err((failure, broken)) => {
                    if broken {
                        lease.forget();
                    }
                    return failure.into_dispatched();
                }
            }
        }
        let arguments = match &call.args {
            Value::Object(arguments) => arguments.clone(),
            Value::Null => JsonObject::new(),
            _ => {
                return Failure::permanent(
                    ToolFailureKind::Error,
                    format!("the arguments of {} must be an object", call.tool.name),
                    None,
                )
                .into_dispatched();
            }
        };
        let mut params = CallToolRequestParams::new(local.to_owned()).with_arguments(arguments);
        params.meta = Some(meta(call));
        match lease.connection().call(params).await {
            Ok(result) => result::outcome(&call.tool, result),
            Err(error) => {
                let (failure, broken) = failure::of_service(&server.name, &error);
                if broken {
                    lease.forget();
                }
                failure.into_dispatched()
            }
        }
    }
}

impl ToolDispatcher for McpDispatcher {
    async fn dispatch(&self, call: ToolCall, credentials: Option<Credentials>) -> Dispatched {
        let ToolRoute::Mcp { server: name } = &call.tool.route else {
            return Failure::permanent(
                ToolFailureKind::Unavailable,
                format!("{} is not an MCP tool", call.tool.name),
                None,
            )
            .into_dispatched();
        };
        let Some(server) = call.server.as_ref().filter(|server| &server.name == name) else {
            return Failure::permanent(
                ToolFailureKind::Unavailable,
                format!("the toolset does not say how to reach the MCP server {name}"),
                None,
            )
            .into_dispatched();
        };
        let credentials = credentials.and_then(|mut credentials| credentials.servers.remove(name));
        let attempt = self.attempt(&call, server, credentials.as_ref());
        let dispatched = match call.tool.policy.execution_timeout {
            Some(timeout) => match tokio::time::timeout(timeout, attempt).await {
                Ok(dispatched) => dispatched,
                // Dropping the call sends `notifications/cancelled` to the server.
                Err(_) => failure::timed_out(&call.tool.name, timeout).into_dispatched(),
            },
            None => attempt.await,
        };
        tracing::debug!(
            run = %call.run,
            seq = call.seq.0,
            tool = %call.tool.name,
            attempt = call.attempt,
            outcome = outcome_name(&dispatched),
            "MCP call"
        );
        dispatched
    }
}

/// `_meta` of a call: the idempotency key, and the W3C trace context so that the server's own
/// spans join the run's trace.
fn meta(call: &ToolCall) -> RequestMetaObject {
    let mut meta = JsonObject::new();
    meta.insert(
        IDEMPOTENCY_KEY_META.to_owned(),
        Value::String(call.idempotency_key.clone()),
    );
    if let Some(trace) = &call.trace_context {
        meta.insert(
            "traceparent".to_owned(),
            Value::String(trace.traceparent.clone()),
        );
        if let Some(state) = &trace.tracestate {
            meta.insert("tracestate".to_owned(), Value::String(state.clone()));
        }
    }
    RequestMetaObject(MetaObject(meta))
}

/// A listed tool, as a toolset pins it.
fn pinned(server: &str, tool: &rmcp::model::Tool) -> Tool {
    let mut pinned = Tool::new(
        format!("{server}.{}", tool.name),
        ToolRoute::Mcp {
            server: server.to_owned(),
        },
        Value::Object((*tool.input_schema).clone()),
    );
    pinned.description = tool
        .description
        .as_deref()
        .or(tool.title.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    pinned.output_schema = tool
        .output_schema
        .as_ref()
        .map(|schema| Value::Object((**schema).clone()));
    pinned
}

fn outcome_name(dispatched: &Dispatched) -> &'static str {
    match dispatched {
        Dispatched::Result(_) => "result",
        Dispatched::Failed(_) => "failed",
        Dispatched::Transient(_) => "transient",
        Dispatched::Deferred => "deferred",
        Dispatched::Park(_) => "parked",
    }
}
