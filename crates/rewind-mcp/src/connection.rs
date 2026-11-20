//! Connections to MCP servers, kept open between calls.
//!
//! A connection belongs to one tenant, one server (as pinned: its transport) and one set of
//! [`TransportSecrets`]: the secret a run attached is injected into the transport (an HTTP
//! header, or the environment of a stdio process), so another secret needs another connection. Calls share a connection
//! concurrently; the store already bounds the calls in flight per server and worker.
//!
//! A connection unused for [`McpConfig::idle_timeout`] is closed when the pool is next used, as
//! is the least recently used one beyond [`McpConfig::max_connections`]; a connection in use is
//! never closed. A connection whose transport failed is dropped, and the next call opens a new
//! one. Closing a stdio connection stops its process.
//!
//! Each connection keeps the server's tool list, to check calls against it
//! ([`crate::schema`]), for [`McpConfig::tools_ttl`], or until the server says its list changed
//! (`notifications/tools/list_changed`).
//!
//! A connection over streamable HTTP has its own client, which goes only where
//! [`McpConfig::egress`] lets it ([`crate::transport`]); one that reached an internal server
//! never quotes the server's answers in its failures.

use std::collections::{BTreeMap, HashMap};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use reqwest::header::{HeaderName, HeaderValue};
use rewind_core::{McpServer, McpTransport, ToolFailureKind, canonical_json};
use rewind_engine::ports::{Secret, TenantId};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    PaginatedRequestParams,
};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, RoleClient, ServiceError, ServiceExt};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::OnceCell;

use crate::McpConfig;
use crate::failure::{self, Answers, Failure, bounded};
use crate::transport::{self, Internal};

/// Pages of `tools/list` read at most: a server whose list never ends is refused, instead of
/// being read until the timeout.
pub(crate) const MAX_TOOL_PAGES: usize = 100;
/// Tools of one server read at most.
pub(crate) const MAX_TOOLS: usize = 10_000;

/// What goes into a connection's transport to prove who calls: HTTP headers of a streamable
/// HTTP server, variables of a stdio server's process. Built from the secret a run attached and
/// the server's `auth` (`archi/04-tools.md`, "Authenticating calls"). Never printed: `Debug`
/// shows the names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct TransportSecrets {
    pub headers: BTreeMap<String, Secret>,
    pub env: BTreeMap<String, Secret>,
}

impl std::fmt::Debug for TransportSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportSecrets")
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// The open connections, by tenant, server and secrets.
#[derive(Debug)]
pub(crate) struct Pool {
    config: McpConfig,
    entries: Mutex<HashMap<Key, Entry>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    tenant: String,
    server: String,
    /// SHA-256 of the transport and the credentials. The secrets themselves are not kept.
    fingerprint: [u8; 32],
}

/// Without the fingerprint: an unsalted digest of a short secret can be reversed by trying
/// every secret.
impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Key")
            .field("tenant", &self.tenant)
            .field("server", &self.server)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Entry {
    /// Filled once connected; concurrent calls wait for the same attempt.
    connection: Arc<OnceCell<Arc<Connection>>>,
    last_used: Instant,
}

impl Pool {
    pub(crate) fn new(config: McpConfig) -> Self {
        Self {
            config,
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn config(&self) -> &McpConfig {
        &self.config
    }

    /// An open connection to the server, opened if needed, within the connect timeout.
    pub(crate) async fn connection(
        &self,
        tenant: &TenantId,
        server: &McpServer,
        credentials: Option<&TransportSecrets>,
    ) -> Result<Lease<'_>, Failure> {
        let key = Key::new(tenant, server, credentials);
        // A connection found closed is replaced once.
        for _ in 0..2 {
            let cell = self.entry(&key);
            let connection = cell
                .get_or_try_init(|| async {
                    match tokio::time::timeout(
                        self.config.connect_timeout,
                        connect(server, credentials, &self.config),
                    )
                    .await
                    {
                        Ok(connected) => connected.map(Arc::new),
                        Err(_) => Err(failure::timed_out(
                            &server.name,
                            self.config.connect_timeout,
                        )),
                    }
                })
                .await?
                .clone();
            if connection.is_closed() {
                self.forget_connection(&key, &connection);
                continue;
            }
            return Ok(Lease {
                pool: self,
                key,
                connection,
            });
        }
        Err(Failure::transient(
            ToolFailureKind::Unavailable,
            format!("the connection to {} closed as it opened", server.name),
        ))
    }

    /// Closes every connection.
    pub(crate) fn close_all(&self) {
        self.lock().clear();
    }

    /// How many connections are open or opening.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    fn entry(&self, key: &Key) -> Arc<OnceCell<Arc<Connection>>> {
        let now = Instant::now();
        let mut entries = self.lock();
        let entry = entries.entry(key.clone()).or_insert_with(|| Entry {
            connection: Arc::new(OnceCell::new()),
            last_used: now,
        });
        entry.last_used = now;
        let cell = entry.connection.clone();
        self.sweep(&mut entries, now);
        cell
    }

    /// Closes the connections idle too long, then the least recently used beyond the maximum.
    /// A connection in use is never closed.
    fn sweep(&self, entries: &mut HashMap<Key, Entry>, now: Instant) {
        let in_use = |entry: &Entry| {
            Arc::strong_count(&entry.connection) > 1
                || entry
                    .connection
                    .get()
                    .is_some_and(|connection| Arc::strong_count(connection) > 1)
        };
        entries.retain(|_, entry| {
            in_use(entry)
                || now.saturating_duration_since(entry.last_used) < self.config.idle_timeout
        });
        while entries.len() > self.config.max_connections {
            let oldest = entries
                .iter()
                .filter(|(_, entry)| !in_use(entry))
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            match oldest {
                Some(key) => {
                    entries.remove(&key);
                }
                None => break,
            }
        }
    }

    /// Drops a broken connection, unless it was already replaced.
    fn forget_connection(&self, key: &Key, connection: &Arc<Connection>) {
        let mut entries = self.lock();
        if let Some(entry) = entries.get(key)
            && entry
                .connection
                .get()
                .is_none_or(|current| Arc::ptr_eq(current, connection))
        {
            entries.remove(key);
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Key, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Key {
    fn new(tenant: &TenantId, server: &McpServer, credentials: Option<&TransportSecrets>) -> Self {
        let transport = match &server.transport {
            McpTransport::Http { url } => json!({ "http": url }),
            McpTransport::Stdio { command, args, env } => {
                json!({ "stdio": { "command": command, "args": args, "env": env } })
            }
        };
        let secrets = credentials.map(|credentials| {
            let expose = |map: &std::collections::BTreeMap<
                String,
                rewind_engine::ports::Secret,
            >| {
                map.iter()
                    .map(|(name, secret)| (name.clone(), Value::String(secret.expose().to_owned())))
                    .collect::<serde_json::Map<String, Value>>()
            };
            json!({ "headers": expose(&credentials.headers), "env": expose(&credentials.env) })
        });
        let fingerprint = Sha256::digest(
            canonical_json(&json!({ "transport": transport, "credentials": secrets })).as_bytes(),
        );
        Self {
            tenant: tenant.as_str().to_owned(),
            server: server.name.clone(),
            fingerprint: fingerprint.into(),
        }
    }
}

/// A connection, borrowed from the pool for one call.
#[derive(Debug)]
pub(crate) struct Lease<'a> {
    pool: &'a Pool,
    key: Key,
    connection: Arc<Connection>,
}

impl Lease<'_> {
    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    /// The connection is broken: the next call opens a new one.
    pub(crate) fn forget(&self) {
        self.pool.forget_connection(&self.key, &self.connection);
    }
}

/// An open MCP session.
pub(crate) struct Connection {
    client: RunningService<RoleClient, Handler>,
    listed: tokio::sync::Mutex<Option<Listed>>,
    changed: Arc<AtomicBool>,
    tools_ttl: Duration,
    /// Whether the session reached an internal server.
    internal: Internal,
}

#[derive(Debug)]
struct Listed {
    at: Instant,
    tools: Arc<BTreeMap<String, rmcp::model::Tool>>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl Connection {
    fn is_closed(&self) -> bool {
        self.client.is_closed()
    }

    /// Whether failures may quote what the server answered.
    pub(crate) fn answers(&self) -> Answers {
        self.internal.answers()
    }

    /// The server's tools, by name, as it lists them now. A failure says whether the connection
    /// is broken.
    pub(crate) async fn tools(
        &self,
        server: &str,
    ) -> Result<Arc<BTreeMap<String, rmcp::model::Tool>>, (Failure, bool)> {
        let mut listed = self.listed.lock().await;
        let changed = self.changed.swap(false, Ordering::SeqCst);
        let fresh = listed
            .as_ref()
            .filter(|listed| !changed && listed.at.elapsed() < self.tools_ttl);
        if let Some(fresh) = fresh {
            return Ok(fresh.tools.clone());
        }
        match self.list(server).await {
            Ok(tools) => {
                let tools: BTreeMap<String, rmcp::model::Tool> = tools
                    .into_iter()
                    .map(|tool| (tool.name.to_string(), tool))
                    .collect();
                let tools = Arc::new(tools);
                *listed = Some(Listed {
                    at: Instant::now(),
                    tools: tools.clone(),
                });
                Ok(tools)
            }
            Err(error) => {
                if changed {
                    self.changed.store(true, Ordering::SeqCst);
                }
                Err(error)
            }
        }
    }

    /// Every page of `tools/list`, within [`MAX_TOOL_PAGES`] and [`MAX_TOOLS`].
    async fn list(&self, server: &str) -> Result<Vec<rmcp::model::Tool>, (Failure, bool)> {
        let mut tools = Vec::new();
        let mut cursor = None;
        for _ in 0..MAX_TOOL_PAGES {
            let page = self
                .client
                .peer()
                .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
                .await
                .map_err(|error| failure::of_service(server, &error, self.answers()))?;
            tools.extend(page.tools);
            if tools.len() > MAX_TOOLS {
                break;
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                return Ok(tools);
            }
        }
        Err((
            Failure::permanent(
                ToolFailureKind::Unavailable,
                format!(
                    "{server} lists more than {MAX_TOOLS} tools, or more than {MAX_TOOL_PAGES} \
                     pages of them: Rewind reads no further"
                ),
                None,
            ),
            false,
        ))
    }

    pub(crate) async fn call(
        &self,
        params: CallToolRequestParams,
    ) -> Result<CallToolResult, ServiceError> {
        self.client.call_tool(params).await
    }
}

/// The client side of a session: it declares no capability, and notes when the server's tool
/// list changes.
#[derive(Debug, Clone)]
pub(crate) struct Handler {
    changed: Arc<AtomicBool>,
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("rewind", env!("CARGO_PKG_VERSION")),
        )
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changed.store(true, Ordering::SeqCst);
    }
}

/// Opens a session with the server, its credentials injected into the transport.
async fn connect(
    server: &McpServer,
    credentials: Option<&TransportSecrets>,
    config: &McpConfig,
) -> Result<Connection, Failure> {
    let changed = Arc::new(AtomicBool::new(false));
    let handler = Handler {
        changed: changed.clone(),
    };
    let name = server.name.as_str();
    let internal = Internal::default();
    let client = match &server.transport {
        McpTransport::Http { url } => {
            let headers = headers(name, credentials)?;
            let http = transport::client(name, url, &config.egress, &internal)?;
            let transport = StreamableHttpClientTransport::with_client(
                http,
                StreamableHttpClientTransportConfig::with_uri(url.as_str()).custom_headers(headers),
            );
            handler
                .serve(transport)
                .await
                .map_err(|error| failure::of_initialize(name, &error, internal.answers()))?
        }
        McpTransport::Stdio { command, args, env } => {
            let mut process = tokio::process::Command::new(command);
            process.args(args).env_clear();
            for variable in &config.inherited_env {
                if let Some(value) = std::env::var_os(variable) {
                    process.env(variable, value);
                }
            }
            process.envs(env);
            if let Some(credentials) = credentials {
                for (variable, secret) in &credentials.env {
                    process.env(variable, secret.expose());
                }
            }
            process.kill_on_drop(true);
            let stderr = if config.log_stderr {
                Stdio::piped()
            } else {
                Stdio::null()
            };
            let (transport, stderr) = TokioChildProcess::builder(process)
                .stderr(stderr)
                .spawn()
                .map_err(|error| failure::of_spawn(name, &error))?;
            if let Some(stderr) = stderr {
                let server = name.to_owned();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        tracing::debug!(server = %server, line = %bounded(&line), "MCP server stderr");
                    }
                });
            }
            handler
                .serve(transport)
                .await
                .map_err(|error| failure::of_initialize(name, &error, Answers::Quoted))?
        }
    };
    tracing::debug!(server = name, "MCP session opened");
    Ok(Connection {
        client,
        listed: tokio::sync::Mutex::new(None),
        changed,
        tools_ttl: config.tools_ttl,
        internal,
    })
}

/// The HTTP headers of the credentials, marked sensitive.
fn headers(
    server: &str,
    credentials: Option<&TransportSecrets>,
) -> Result<HashMap<HeaderName, HeaderValue>, Failure> {
    let mut headers = HashMap::new();
    let Some(credentials) = credentials else {
        return Ok(headers);
    };
    for (name, secret) in &credentials.headers {
        let invalid = || {
            Failure::permanent(
                ToolFailureKind::Unavailable,
                format!("the credentials of {server} hold an invalid HTTP header {name:?}"),
                None,
            )
        };
        let header = HeaderName::try_from(name.as_str()).map_err(|_| invalid())?;
        let mut value = HeaderValue::from_str(secret.expose()).map_err(|_| invalid())?;
        value.set_sensitive(true);
        headers.insert(header, value);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pool_s_debug_output_holds_no_digest_of_credentials() {
        let server = McpServer::new(
            "crm",
            McpTransport::Http {
                url: "https://mcp.example.com".into(),
            },
        );
        let credentials = TransportSecrets {
            headers: BTreeMap::from([("authorization".into(), Secret::new("Bearer 1234"))]),
            env: BTreeMap::new(),
        };
        let key = Key::new(&TenantId::new("acme").unwrap(), &server, Some(&credentials));
        let debug = format!("{key:?}");
        // An unsalted digest of a short secret can be reversed by trying them all.
        assert!(
            !debug.contains(&format!("{:?}", key.fingerprint)),
            "{debug}"
        );
        assert!(debug.contains("acme") && debug.contains("crm"), "{debug}");
    }
}
