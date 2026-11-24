//! Connections to MCP servers, kept open between calls.
//!
//! A connection belongs to one tenant, one server (as pinned: its transport) and one identity
//! ([`Credentials`]): the secret a run uses is injected into the transport (an HTTP header,
//! or the environment of a stdio process), and a signed session signs as one tenant's key for
//! one end user, so another secret, key or end user needs another connection. The pool tells
//! them apart by an HMAC of the transport and the credentials under a key drawn when the pool is
//! made: it never holds a value in clear, nor a digest of one that could be reversed by trying
//! every short secret. Calls share a connection concurrently; the store already bounds the calls
//! in flight per server and node.
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

use hmac::Mac as _;
use rewind_core::{ActionFailureKind, McpServer, McpTransport, SecretPlacement, canonical_json};
use rewind_engine::ports::{Secret, SigningKey, TenantId};
use rewind_outbound::{Placed, Redirects, Signer, place};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, ClientConfig, Implementation,
    PaginatedRequestParams,
};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientHandler, RoleClient, ServiceError, ServiceExt};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::OnceCell;

use crate::McpConfig;
use crate::failure::{self, Answers, Failure, bounded};
use crate::transport::{self, Internal, SessionSigner, SigningClient};

/// Pages of `tools/list` read at most: a server whose list never ends is refused, instead of
/// being read until the timeout.
pub(crate) const MAX_TOOL_PAGES: usize = 100;
/// Tools of one server read at most.
pub(crate) const MAX_TOOLS: usize = 10_000;

/// How a session proves who calls (`archi/04-tools.md`, "Authenticating calls"), built from what
/// the engine resolved for a call or what the registration gives a listing. Never printed:
/// `Debug` shows its kind only.
#[derive(Clone)]
pub(crate) enum Credentials {
    /// Nothing: a public server.
    None,
    /// The secret used, put where the server's `auth` says.
    Secret {
        placement: SecretPlacement,
        value: Secret,
    },
    /// A JWT per HTTP request, signed with the tenant's key, issued by `issuer`. `subject` is the
    /// end user of the calls the session carries; `None` for a listing at registration.
    Signed {
        key: SigningKey,
        issuer: String,
        subject: Option<String>,
    },
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Credentials::None => "Credentials::None",
            Credentials::Secret { .. } => "Credentials::Secret(<redacted>)",
            Credentials::Signed { .. } => "Credentials::Signed(<redacted>)",
        })
    }
}

impl Credentials {
    /// What tells two sessions apart, values included: only ever hashed with the pool's key.
    fn identity(&self) -> Value {
        match self {
            Credentials::None => Value::Null,
            Credentials::Secret { placement, value } => {
                let placement = match placement {
                    SecretPlacement::Header {
                        name,
                        format,
                        encoding,
                    } => json!({
                        "header": name.to_ascii_lowercase(),
                        "format": format,
                        "encoding": encoding.as_str(),
                    }),
                    SecretPlacement::Env { variable } => json!({ "env": variable }),
                };
                json!({ "secret": { "placement": placement, "value": value.expose() } })
            }
            Credentials::Signed {
                key,
                issuer,
                subject,
            } => json!({
                "signed": { "issuer": issuer, "kid": key.public.kid, "subject": subject }
            }),
        }
    }
}

/// The open connections, by tenant, server and identity.
pub(crate) struct Pool {
    config: McpConfig,
    /// The key of the fingerprints: random, drawn when the pool is made, never printed.
    fingerprints: [u8; 32],
    entries: Mutex<HashMap<Key, Entry>>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pool")
            .field("config", &self.config)
            .field("connections", &self.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    tenant: String,
    server: String,
    /// HMAC-SHA-256 of the transport and the credentials under the pool's key. The secrets
    /// themselves are not kept.
    fingerprint: [u8; 32],
}

/// Without the fingerprint, which is no one's business.
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
        let mut fingerprints = [0u8; 32];
        if let Err(error) = getrandom::fill(&mut fingerprints) {
            // Without randomness, the fingerprints are still keyed by the process's start.
            tracing::warn!(%error, "no random key for the MCP pool's fingerprints");
            let started = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            fingerprints[..16].copy_from_slice(&started.to_le_bytes());
        }
        Self {
            config,
            fingerprints,
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
        credentials: &Credentials,
    ) -> Result<Lease<'_>, Failure> {
        let key = Key::new(&self.fingerprints, tenant, server, credentials);
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
            ActionFailureKind::Unavailable,
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
    fn new(
        pool_key: &[u8; 32],
        tenant: &TenantId,
        server: &McpServer,
        credentials: &Credentials,
    ) -> Self {
        let transport = match &server.transport {
            McpTransport::Http { url } => json!({ "http": url }),
            McpTransport::Stdio { command, args, env } => {
                json!({ "stdio": { "command": command, "args": args, "env": env } })
            }
        };
        let identity = json!({ "transport": transport, "credentials": credentials.identity() });
        let mut mac = <hmac::Hmac<Sha256> as hmac::KeyInit>::new_from_slice(pool_key)
            .unwrap_or_else(|_| unreachable!("HMAC takes keys of any length"));
        mac.update(canonical_json(&identity).as_bytes());
        Self {
            tenant: tenant.as_str().to_owned(),
            server: server.name.clone(),
            fingerprint: mac.finalize().into_bytes().into(),
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
    /// The signer of a signed session's requests.
    signer: Option<Arc<SessionSigner>>,
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

    /// The signer of a signed session, with which a call registers its claims.
    pub(crate) fn signer(&self) -> Option<&Arc<SessionSigner>> {
        self.signer.as_ref()
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
                ActionFailureKind::Unavailable,
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
    credentials: &Credentials,
    config: &McpConfig,
) -> Result<Connection, Failure> {
    let changed = Arc::new(AtomicBool::new(false));
    let handler = Handler {
        changed: changed.clone(),
    };
    let name = server.name.as_str();
    let mut internal = Internal::default();
    let mut signer = None;
    let client = match &server.transport {
        McpTransport::Http { url } => {
            let transport_config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
            match credentials {
                Credentials::Signed { key, issuer, .. } => {
                    let session = Signer::new(&key.public.kid, key.private.expose())
                        .map_err(|error| unavailable(error.to_string()))?;
                    let (http, reached) =
                        transport::client(name, url, &config.egress, Redirects::Signed)?;
                    internal = reached;
                    let session =
                        Arc::new(SessionSigner::new(session, issuer.clone(), url.clone()));
                    signer = Some(session.clone());
                    let client = SigningClient {
                        inner: http,
                        signer: session,
                    };
                    handler
                        .serve(StreamableHttpClientTransport::with_client(
                            client,
                            transport_config,
                        ))
                        .await
                }
                Credentials::Secret { placement, value } => {
                    let Placed::Header {
                        name: header,
                        value,
                    } = place(placement, value.expose())
                        .map_err(|error| unavailable(format!("{name}: {error}")))?
                    else {
                        return Err(unavailable(format!(
                            "{name} is an HTTP server: its secret goes in a header, not in a \
                             variable"
                        )));
                    };
                    let (http, reached) =
                        transport::client(name, url, &config.egress, Redirects::WithinOrigin)?;
                    internal = reached;
                    let headers = HashMap::from([(header, value)]);
                    handler
                        .serve(StreamableHttpClientTransport::with_client(
                            http,
                            transport_config.custom_headers(headers),
                        ))
                        .await
                }
                Credentials::None => {
                    let (http, reached) =
                        transport::client(name, url, &config.egress, Redirects::WithinOrigin)?;
                    internal = reached;
                    handler
                        .serve(StreamableHttpClientTransport::with_client(
                            http,
                            transport_config,
                        ))
                        .await
                }
            }
            .map_err(|error| failure::of_initialize(name, &error, internal.answers()))?
        }
        McpTransport::Stdio { command, args, env } => {
            let secret = match credentials {
                Credentials::None => None,
                Credentials::Secret { placement, value } => {
                    match place(placement, value.expose())
                        .map_err(|error| unavailable(format!("{name}: {error}")))?
                    {
                        Placed::Env { variable, value } => Some((variable, value)),
                        Placed::Header { .. } => {
                            return Err(unavailable(format!(
                                "{name} is a stdio server: its secret goes in a variable, not in \
                                 a header"
                            )));
                        }
                    }
                }
                Credentials::Signed { .. } => {
                    return Err(unavailable(format!(
                        "{name} is a stdio server, which cannot check a signature"
                    )));
                }
            };
            let mut process = tokio::process::Command::new(command);
            process.args(args).env_clear();
            for variable in &config.inherited_env {
                if let Some(value) = std::env::var_os(variable) {
                    process.env(variable, value);
                }
            }
            process.envs(env);
            if let Some((variable, value)) = secret {
                process.env(variable, value);
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
        signer,
    })
}

/// A session that cannot be opened as its credentials say: it will not open by retrying.
fn unavailable(message: String) -> Failure {
    Failure::permanent(ActionFailureKind::Unavailable, message, None)
}

#[cfg(test)]
mod tests {
    use rewind_core::{SecretEncoding, Timestamp};
    use rewind_engine::ports::{PrivateKey, PublicKey};

    use super::*;

    fn server() -> McpServer {
        McpServer::new(
            "crm",
            McpTransport::Http {
                url: "https://mcp.example.com".into(),
            },
        )
    }

    fn secret(value: &str) -> Credentials {
        Credentials::Secret {
            placement: SecretPlacement::bearer(),
            value: Secret::new(value),
        }
    }

    fn signed(kid: &str, subject: Option<&str>) -> Credentials {
        Credentials::Signed {
            key: SigningKey {
                public: PublicKey {
                    kid: kid.into(),
                    key: [1; 32],
                    created_at: Timestamp(0),
                    retired_at: None,
                },
                private: PrivateKey::from_seed([2; 32]),
            },
            issuer: "https://rewind.example/organizations/acme".into(),
            subject: subject.map(str::to_owned),
        }
    }

    #[test]
    fn the_pool_tells_identities_apart_without_keeping_them() {
        let tenant = TenantId::new("acme").unwrap();
        let pool_key = [5u8; 32];
        let key = |credentials: &Credentials| Key::new(&pool_key, &tenant, &server(), credentials);
        let first = key(&secret("1234"));
        assert_eq!(first, key(&secret("1234")));
        assert_ne!(first, key(&secret("5678")));
        assert_ne!(first, key(&Credentials::None));
        let encoded = Credentials::Secret {
            placement: SecretPlacement::Header {
                name: "Authorization".into(),
                format: Some("Bearer {secret}".into()),
                encoding: SecretEncoding::Base64,
            },
            value: Secret::new("1234"),
        };
        assert_ne!(first, key(&encoded));
        // One session per end user and per key; the listing's apart.
        let alice = key(&signed("k1", Some("alice")));
        assert_eq!(alice, key(&signed("k1", Some("alice"))));
        assert_ne!(alice, key(&signed("k1", Some("bob"))));
        assert_ne!(alice, key(&signed("k2", Some("alice"))));
        assert_ne!(alice, key(&signed("k1", None)));
        // Another pool, another key: a fingerprint says nothing outside its pool.
        assert_ne!(
            first.fingerprint,
            Key::new(&[6u8; 32], &tenant, &server(), &secret("1234")).fingerprint
        );
        // Nothing printed holds the fingerprint, nor a secret.
        let debug = format!("{first:?} {:?}", secret("1234"));
        assert!(
            !debug.contains(&format!("{:?}", first.fingerprint)),
            "{debug}"
        );
        assert!(!debug.contains("1234"), "{debug}");
        assert!(debug.contains("acme") && debug.contains("crm"), "{debug}");
    }
}
