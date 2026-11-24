//! The action dispatcher of Rewind outside test runs (`archi/07-code-architecture.md`, "Crates";
//! `archi/04-tools.md`, "Routes").
//!
//! | Route | Dispatcher | What it does |
//! | --- | --- | --- |
//! | MCP actions | [`McpDispatcher`] | An MCP client (`rmcp`), over streamable HTTP or stdio: discovery (`tools/list`, pinned by the action set), calls with the idempotency key and the trace context in `_meta`, the attached secret or the signature in the transport, the execution timeout, the schema check before each call |
//! | Built-ins | [`HttpAction`] | `http`, with the egress rules: allow-list, then `rewind-outbound`'s checks (one DNS resolution, private and metadata addresses refused, a connection to the checked address), redirects checked again, bounded responses. `webhook` is answered by the engine |
//!
//! A composition root plugs them into the engine's `RoutingDispatcher` (`mcp` and `builtins`).
//! Where a deployment's HTTP servers may be is `rewind-outbound`'s [`ActionEgress`], with the
//! operator's internal hosts (`--allow-internal-addresses`). The dispatchers never write to the
//! history; a secret or a key goes only into the transport, never into a result, a failure or a
//! log.
//!
//! How a delivery proves who calls (`archi/04-tools.md`, "Authenticating calls"), from the
//! [`CallAuth`] the engine resolved:
//!
//! | `CallAuth` | The session |
//! | --- | --- |
//! | `None` | Carries nothing |
//! | `Secret` | Carries the secret in the header (format, encoding) or the variable of the server's `auth`, and is pooled apart from the sessions of other secrets |
//! | `Signed` | Signs each HTTP request with the tenant's key: the call's `tools/call` with its claims, the others with the reduced set; pooled by key and end user |
//!
//! What becomes of an attempt:
//!
//! ```text
//! connection (pooled by tenant, server and identity) ── 401 ──► Park(credentials_refused)
//!   │   only where McpConfig::egress lets it go           ├ unreachable ──► Transient
//!   │                                                     └ address refused ──► Failed
//!   ▼ unless approved after a schema change: the server's current tool
//!   │   gone, args refused by a new input schema, output schema changed ──► Park(schema_changed)
//!   ▼ tools/call with _meta { codes.rewind/idempotency-key, traceparent, tracestate }
//!   ├─ result ──► Result: structuredContent (output schema), else { content, structuredContent? }
//!   ├─ isError, JSON-RPC error ──► Failed (internal error -32603: Transient)
//!   └─ transport failure, 429, 5xx ──► Transient; past the execution timeout ──► Transient(timeout)
//! ```

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::time::Duration;

use rewind_core::{Action, ActionAuth, ActionFailureKind, ActionRoute, McpServer};
use rewind_engine::ports::{
    ActionDispatcher, Call, CallAuth, Dispatched, ParkKind, ParkReason, Secret, SigningKey,
    TenantId,
};
use rewind_outbound::{ActionEgress, CallIdentity};
use rmcp::model::{CallToolRequestParams, JsonObject, MetaObject, RequestMetaObject};
use serde_json::Value;

mod connection;
mod failure;
mod http;
mod result;
mod schema;
mod transport;

pub use http::{HttpAction, HttpConfig, http_action, http_input_schema, http_output_schema};

use connection::{Credentials, Pool};
use failure::Failure;

/// The `_meta` key of the idempotency key (`archi/04-tools.md`, "Routes").
pub const IDEMPOTENCY_KEY_META: &str = "codes.rewind/idempotency-key";

/// How [`McpDispatcher`] manages its connections.
#[derive(Debug, Clone)]
pub struct McpConfig {
    /// Where the streamable HTTP servers may be: with the egress checks of a deployment by
    /// default, anywhere for the embedded server and the CLI.
    pub egress: ActionEgress,
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
            egress: ActionEgress::default(),
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

/// Why the tools of an MCP server could not be listed, what registering an action set reports to
/// the harness developer, or why the `http` action could not be set up. The message names the
/// server.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
    /// The server could not be reached or started, or its connection closed.
    #[error("{message}")]
    Unreachable { server: String, message: String },
    /// The server refused the secret or the signature (HTTP 401).
    #[error("{message}")]
    Unauthorized { server: String, message: String },
    /// The server did not answer in time.
    #[error("{message}")]
    Timeout { server: String, message: String },
    /// The server answered, but not with its tools; or it may not be reached at all: its
    /// address is refused by the egress checks, or it redirects elsewhere.
    #[error("{message}")]
    Refused { server: String, message: String },
    /// The HTTP client of the `http` action could not be built.
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

/// Calls the actions of MCP servers: the `mcp` route of the engine's `RoutingDispatcher`. Cheap
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

    /// Discovery: the actions of a server, as an action set pins them (`archi/04-tools.md`, "Action
    /// sets and code generation"), sorted by name. Each is named `{server}.{action}`, with its
    /// description (or its title), its input and output schemas, and the default policy of an
    /// MCP action; the harness's policies are applied by whoever builds the action set. `auth` is
    /// how the listing proves who calls: the organization's secret the registration attaches, or a
    /// listing signed with the reduced set of claims.
    pub async fn list_tools(
        &self,
        tenant: &TenantId,
        server: &McpServer,
        auth: ListingAuth,
    ) -> Result<Vec<Action>, McpError> {
        let credentials = auth.credentials(server)?;
        let lease = self
            .pool
            .connection(tenant, server, &credentials)
            .await
            .map_err(|failure| failure.into_error(&server.name))?;
        let timeout = self.pool.config().connect_timeout;
        let listed =
            match tokio::time::timeout(timeout, lease.connection().tools(&server.name)).await {
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
        call: &Call,
        server: &McpServer,
        credentials: &Credentials,
        identity: Option<CallIdentity>,
    ) -> Dispatched {
        let lease = match self
            .pool
            .connection(&call.run.tenant, server, credentials)
            .await
        {
            Ok(lease) => lease,
            Err(failure) => return failure.into_dispatched(),
        };
        // A signed session signs the call's `tools/call` with its claims while it is in flight.
        let _signing = identity.and_then(|identity| {
            lease
                .connection()
                .signer()
                .map(|signer| signer.register(call.idempotency_key.clone(), identity))
        });
        let local = call.action.local_name();
        if !call.schema_change_approved {
            match lease.connection().tools(&server.name).await {
                Ok(tools) => {
                    if let Some(message) =
                        schema::mismatch(&call.action, tools.get(local), &call.args)
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
                    ActionFailureKind::Error,
                    format!("the arguments of {} must be an object", call.action.name),
                    None,
                )
                .into_dispatched();
            }
        };
        let mut params = CallToolRequestParams::new(local.to_owned()).with_arguments(arguments);
        params.meta = Some(meta(call));
        match lease.connection().call(params).await {
            Ok(result) => result::outcome(&call.action, result),
            Err(error) => {
                let (failure, broken) =
                    failure::of_service(&server.name, &error, lease.connection().answers());
                if broken {
                    lease.forget();
                }
                failure.into_dispatched()
            }
        }
    }
}

impl ActionDispatcher for McpDispatcher {
    async fn dispatch(&self, call: Call, auth: CallAuth) -> Dispatched {
        let ActionRoute::Mcp { server: name } = &call.action.route else {
            return Failure::permanent(
                ActionFailureKind::Unavailable,
                format!("{} is not an MCP action", call.action.name),
                None,
            )
            .into_dispatched();
        };
        let Some(server) = call.server.as_ref().filter(|server| &server.name == name) else {
            return Failure::permanent(
                ActionFailureKind::Unavailable,
                format!("the action set does not say how to reach the MCP server {name}"),
                None,
            )
            .into_dispatched();
        };
        let (credentials, identity) = credentials_of(auth);
        let attempt = self.attempt(&call, server, &credentials, identity);
        let dispatched = match call.action.policy.execution_timeout {
            Some(timeout) => match tokio::time::timeout(timeout, attempt).await {
                Ok(dispatched) => dispatched,
                // Dropping the call sends `notifications/cancelled` to the server.
                Err(_) => failure::timed_out(&call.action.name, timeout).into_dispatched(),
            },
            None => attempt.await,
        };
        tracing::debug!(
            run = %call.run,
            seq = call.seq.0,
            action = %call.action.name,
            delivery = call.delivery,
            outcome = outcome_name(&dispatched),
            "MCP call"
        );
        dispatched
    }
}

/// How a listing at registration proves who calls (`archi/04-tools.md`, "Authenticating calls"
/// and "Signed calls"). Never printed: `Debug` redacts the secret and the key.
#[derive(Debug, Clone, Default)]
pub enum ListingAuth {
    /// Nothing: a public server, or one whose secret the registration does not attach.
    #[default]
    None,
    /// The organization's secret the registration attaches for the server (`secrets`), put
    /// where the server's `auth` says.
    Secret(Secret),
    /// Each request signed with the tenant's key, issued by `issuer`
    /// (`{public-url}/organizations/{organization}`), with only `iss`, `aud`, `jti`, `htm`, `htu`,
    /// `body`, `iat` and `exp`.
    Signed { key: SigningKey, issuer: String },
}

impl ListingAuth {
    fn credentials(self, server: &McpServer) -> Result<Credentials, McpError> {
        let refused = |message: &str| McpError::Refused {
            server: server.name.clone(),
            message: format!("{}: {message}", server.name),
        };
        match (self, &server.auth) {
            (ListingAuth::None, _) => Ok(Credentials::None),
            (ListingAuth::Secret(value), ActionAuth::Secret(placement)) => {
                Ok(Credentials::Secret {
                    placement: placement.clone(),
                    value,
                })
            }
            (ListingAuth::Secret(_), _) => Err(refused("its auth takes no secret")),
            (ListingAuth::Signed { key, issuer }, ActionAuth::Rewind) => Ok(Credentials::Signed {
                key,
                issuer,
                subject: None,
            }),
            (ListingAuth::Signed { .. }, _) => Err(refused("its auth is not a signature")),
        }
    }
}

/// What the call's authentication puts into the session (`archi/04-tools.md`, "Authenticating
/// calls"): nothing; the attached secret where the server's `auth` says; or the tenant's key, on
/// a session of the run's end user, with the claims of the call for its own request.
fn credentials_of(auth: CallAuth) -> (Credentials, Option<CallIdentity>) {
    match auth {
        CallAuth::None => (Credentials::None, None),
        CallAuth::Secret { value, placement } => (Credentials::Secret { placement, value }, None),
        CallAuth::Signed { key, claims } => {
            let identity = CallIdentity {
                subject: claims.subject.as_str().to_owned(),
                labels: claims.labels.clone(),
                run: claims.run.to_string(),
                action: claims.action.clone(),
                idempotency_key: claims.idempotency_key.clone(),
                jti: claims.jti(),
            };
            let credentials = Credentials::Signed {
                key,
                issuer: claims.issuer,
                subject: Some(identity.subject.clone()),
            };
            (credentials, Some(identity))
        }
    }
}

/// `_meta` of a call: the idempotency key, and the W3C trace context so that the server's own
/// spans join the run's trace.
fn meta(call: &Call) -> RequestMetaObject {
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

/// A listed tool, as an action set pins it.
fn pinned(server: &str, tool: &rmcp::model::Tool) -> Action {
    let mut pinned = Action::new(
        format!("{server}.{}", tool.name),
        ActionRoute::Mcp {
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
