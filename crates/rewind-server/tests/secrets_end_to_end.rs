//! Secrets attached to runs and signed calls, end to end over HTTP (`archi/04-tools.md`,
//! "Authenticating calls", "Secrets" and "Signed calls"; `archi/06-storage-and-scale.md`, "What
//! the caller may do"; `archi/07-code-architecture.md`, "HTTP API /v1"), on Turso and on Postgres:
//!
//! - an MCP server over streamable HTTP that requires a secret: its actions listed with an
//!   organization's secret and refused without one; Alice's run calls it with Alice's value and
//!   Bob's with Bob's; a start that attaches nothing is refused before any run exists, and nothing
//!   falls back on the organization's secret;
//! - signed calls: an MCP server and an API check every request with `rewind_outbound::verify`
//!   against the keys the Rewind server publishes at `/organizations/{organization}/jwks.json`, and
//!   learn the end user from `sub`; a replaced key still verifies; a signature the server refuses
//!   parks the call until the harness approves it;
//! - a secret deleted mid-run parks the call (`secret.expired`, `missing`), a 401 parks it
//!   too (`refused`), and putting the secret again sends it with the same idempotency key;
//! - a key limited to `env=dev` neither sees nor attaches the organization's `env=prod` secrets,
//!   and never writes a namespace's;
//! - a value is nowhere: not in a run, its history or cassette, its events, events or trace, a
//!   list, a usage, a problem, a webhook, the OpenTelemetry the server exports, nor the process's
//!   logs.
//!
//! The fake servers listen on 127.0.0.1, which a deployed server reaches with
//! `--allow-internal-addresses`. A deployed server calls APIs over https only, with the platform's
//! roots, which a test's certificate is not among: the signed API is called by the embedded
//! server, which reaches it over http, and whose keys the API reads over its socket.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use clap::Parser;
use rewind_api::action_sets::{Api, Auth, CreateActionSet, McpServer, OpenApiDocument};
use rewind_api::calls::{CallState, ParkedReason};
use rewind_api::common::{Labels, RunStatus};
use rewind_api::namespaces::NamespaceName;
use rewind_api::runs::{Run, RunFilter, StartRun, Waiting};
use rewind_api::secrets::{AttachmentProblemKind, PutSecret};
use rewind_api::webhooks::{
    CreateSubscription, SecretExpired, SecretExpiredReason, Webhook, WebhookType,
};
use rewind_api::{Client, Problem, ProblemType};
use rewind_outbound::{Expected, verify};
use rewind_server::ServerArgs;
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
use support::{ADMIN_KEY, Postgres, TestServer, eventually, wait};

/// The public URL the deployed servers of these tests sign with: the issuer of their JWTs is
/// `https://rewind.test/organizations/{organization}`, and the fake servers read the keys at the server's
/// own address, as a server behind a proxy would.
const PUBLIC_URL: &str = "https://rewind.test";

/// `_meta` of an MCP `tools/call`: the call's idempotency key.
const IDEMPOTENCY_META: &str = "codes.rewind/idempotency-key";

// -------------------------------------------------------------------------------------------
// What leaves the server: its logs, its OpenTelemetry, its webhooks

/// The process's logs: the Rewind crates' at every level, the others' from `info`, without
/// colors. Every test of this file writes here; each looks for its own values.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The capture of the process's logs, installed by the first test that asks.
fn logs() -> &'static Logs {
    static LOGS: OnceLock<Logs> = OnceLock::new();
    LOGS.get_or_init(|| {
        use tracing_subscriber::Layer as _;
        use tracing_subscriber::layer::SubscriberExt as _;
        use tracing_subscriber::util::SubscriberInitExt as _;
        let logs = Logs::default();
        let writer = logs.clone();
        let filter = tracing_subscriber::filter::Targets::new()
            .with_target("rewind", tracing::Level::TRACE)
            .with_default(tracing::Level::INFO);
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .with_filter(filter);
        tracing_subscriber::registry()
            .with(layer)
            .try_init()
            .expect("no other subscriber in this test binary");
        logs
    })
}

fn holds(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

/// A fake OpenTelemetry Collector: the bodies the server exported over OTLP/HTTP (protobuf, in
/// which strings are their UTF-8 bytes).
#[derive(Clone, Default)]
struct Collector(Arc<Mutex<Vec<(String, Bytes)>>>);

impl Collector {
    fn holds(&self, text: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .iter()
            .any(|(_, body)| holds(body, text))
    }

    fn exports(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// A receiver of webhooks: each delivery's body.
#[derive(Clone, Default)]
struct Hooks(Arc<Mutex<Vec<String>>>);

impl Hooks {
    fn bodies(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    /// The `secret.expired` deliveries about `run`.
    fn expired(&self, run: &str) -> Vec<SecretExpired> {
        self.bodies()
            .iter()
            .filter_map(|body| match serde_json::from_str::<Webhook>(body) {
                Ok(Webhook::SecretExpired { data, .. }) if data.run_id == run => Some(data),
                _ => None,
            })
            .collect()
    }
}

/// What a scenario's server reaches outside: a Collector for its OTLP, a receiver for its
/// webhooks.
struct Outside {
    collector: Collector,
    collector_url: String,
    hooks: Hooks,
    hooks_url: String,
}

impl Outside {
    async fn serve() -> Self {
        logs();
        let collector = Collector::default();
        let kept = collector.clone();
        let router = axum::Router::new().fallback(move |request: Request| {
            let kept = kept.clone();
            async move {
                let path = request.uri().path().to_owned();
                let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                kept.0.lock().unwrap().push((path, body));
                StatusCode::OK
            }
        });
        let collector_url = listen(router).await;
        let hooks = Hooks::default();
        let kept = hooks.clone();
        let router = axum::Router::new().fallback(move |body: String| {
            let kept = kept.clone();
            async move {
                kept.0.lock().unwrap().push(body);
                StatusCode::NO_CONTENT
            }
        });
        let hooks_url = format!("{}/hook", listen(router).await);
        Self {
            collector,
            collector_url,
            hooks,
            hooks_url,
        }
    }

    /// The options of a deployed server whose fake servers, Collector and webhook receiver
    /// listen on 127.0.0.1.
    fn args(&self) -> Vec<String> {
        [
            "--allow-internal-addresses",
            "127.0.0.1",
            "--allow-private-webhooks",
            "--public-url",
            PUBLIC_URL,
            "--otlp-endpoint",
            &self.collector_url,
            "--otlp-protocol",
            "http",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    async fn on_turso(&self) -> TestServer {
        let args = self.args();
        TestServer::start(&args.iter().map(String::as_str).collect::<Vec<_>>()).await
    }

    async fn on_postgres(&self, postgres: &Postgres) -> TestServer {
        let args = self.args();
        let dir = tempfile::tempdir().unwrap();
        let blobs = dir.path().join("blobs");
        TestServer::start_on(
            dir,
            &postgres.url(),
            &blobs,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .await
    }

    /// Subscribes `client`'s tenant to `secret.expired`.
    async fn subscribe(&self, client: &Client) {
        client
            .create_subscription(&CreateSubscription {
                url: self.hooks_url.clone(),
                types: vec![WebhookType::SecretExpired],
                labels: Labels::new(),
            })
            .await
            .unwrap();
    }
}

/// Serves `router` on 127.0.0.1: its URL.
async fn listen(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}")
}

// -------------------------------------------------------------------------------------------
// The fake servers

/// One HTTP request a fake server received.
#[derive(Debug, Clone, Default)]
struct Received {
    method: String,
    /// The URL as called: `http://{host}{path and query}`.
    url: String,
    body: Bytes,
    authorization: Option<String>,
    /// The JSON-RPC method of an MCP message.
    rpc: Option<String>,
    /// The call's idempotency key: `_meta` of an MCP `tools/call`, or the `Idempotency-Key`
    /// header of an API call.
    key: Option<String>,
    status: u16,
    /// A signed request's JWT, its `kid`, and its claims once checked.
    token: Option<String>,
    kid: Option<String>,
    claims: Option<Value>,
    /// Why the check refused it.
    refusal: Option<String>,
}

impl Received {
    fn is_call_of(&self, run: &str) -> bool {
        self.key
            .as_deref()
            .is_some_and(|key| key.starts_with(&format!("{run}/")))
    }
}

/// The request whole, and what it says: its JSON-RPC method and `_meta` key, its headers.
async fn read(request: Request) -> (Parts, Received) {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    let message = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    let received = Received {
        method: parts.method.to_string(),
        url: format!("http://{}{}", header("host").unwrap_or_default(), parts.uri),
        authorization: header("authorization"),
        rpc: message["method"].as_str().map(str::to_owned),
        key: message["params"]["_meta"][IDEMPOTENCY_META]
            .as_str()
            .map(str::to_owned)
            .or_else(|| header("idempotency-key")),
        body,
        ..Received::default()
    };
    (parts, received)
}

fn unauthorized(why: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [("www-authenticate", "Bearer")],
        why.to_owned(),
    )
        .into_response()
}

/// The claims a signed request was verified with, handed to the handler.
#[derive(Debug, Clone)]
struct Verified(Value);

/// An MCP server with one action, `whoami`, which answers who calls: the end user its signature
/// names (`sub`), else whom the `Authorization` header belongs to.
#[derive(Clone, Default)]
struct Whoami {
    users: Arc<Mutex<BTreeMap<String, String>>>,
}

impl ServerHandler for Whoami {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let Value::Object(schema) = json!({ "type": "object" }) else {
            unreachable!()
        };
        Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "whoami",
            "Who calls.",
            Arc::new(schema),
        )]))
    }

    async fn call_tool(
        &self,
        _request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let parts = context.extensions.get::<Parts>();
        let signed = parts
            .and_then(|parts| parts.extensions.get::<Verified>())
            .and_then(|verified| verified.0["sub"].as_str().map(str::to_owned));
        let keyed = parts
            .and_then(|parts| parts.headers.get("authorization"))
            .and_then(|value| value.to_str().ok())
            .and_then(|value| self.users.lock().unwrap().get(value).cloned());
        let user = signed.or(keyed).unwrap_or_else(|| "nobody".into());
        Ok(CallToolResult::success(vec![ContentBlock::text(
            json!({ "user": user }).to_string(),
        )])
        .into())
    }
}

fn mcp_service(handler: Whoami) -> StreamableHttpService<Whoami, LocalSessionManager> {
    StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_sse_keep_alive(None),
    )
}

/// An MCP server that takes a secret: it answers only the `Authorization` values it accepts,
/// 401 otherwise, and records every request.
#[derive(Clone, Default)]
struct Keyed {
    whoami: Whoami,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Keyed {
    fn accept(&self, value: &str, who: &str) {
        self.whoami
            .users
            .lock()
            .unwrap()
            .insert(format!("Bearer {value}"), who.to_owned());
    }

    fn refuse(&self, value: &str) {
        self.whoami
            .users
            .lock()
            .unwrap()
            .remove(&format!("Bearer {value}"));
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    fn calls_of(&self, run: &str) -> Vec<Received> {
        self.received()
            .into_iter()
            .filter(|received| received.rpc.as_deref() == Some("tools/call"))
            .filter(|received| received.is_call_of(run))
            .collect()
    }

    /// Serves it at `/mcp` on 127.0.0.1: its URL.
    async fn serve(&self) -> String {
        let router = axum::Router::new()
            .nest_service("/mcp", mcp_service(self.whoami.clone()))
            .layer(middleware::from_fn_with_state(self.clone(), keyed_gate));
        format!("{}/mcp", listen(router).await)
    }
}

async fn keyed_gate(State(keyed): State<Keyed>, request: Request, next: Next) -> Response {
    let (parts, mut received) = read(request).await;
    let accepted = received
        .authorization
        .as_ref()
        .is_some_and(|value| keyed.whoami.users.lock().unwrap().contains_key(value));
    received.status = if accepted { 200 } else { 401 };
    let body = received.body.clone();
    keyed.received.lock().unwrap().push(received);
    if !accepted {
        return unauthorized("this token is not ours");
    }
    next.run(Request::from_parts(parts, Body::from(body))).await
}

/// A read of the public keys: the status and the `Cache-Control` answered.
type Fetched = (u16, Option<String>);

/// Where a server of the customer reads Rewind's public keys.
#[derive(Clone)]
enum KeysAt {
    /// The deployed server's own address.
    Http(String),
    /// The embedded server's socket.
    Socket(PathBuf),
}

/// A server of the customer that checks Rewind's signature on every request, as 04 says a
/// receiving server does: the keys of the issuer it expects, read from the Rewind server's
/// JWKS endpoint without credentials; `rewind_outbound::verify` (signature, `iss`, `aud`, `exp`,
/// `iat`, `htm`, `htu`, `body`); and, on a call, the labels it requires.
#[derive(Clone)]
struct Checker {
    issuer: String,
    keys: KeysAt,
    /// Its own audience: the MCP server's URL, or the API's base URL.
    audience: Arc<Mutex<String>>,
    /// The labels a call must carry.
    labels: Arc<Mutex<BTreeMap<String, String>>>,
    received: Arc<Mutex<Vec<Received>>>,
    /// Each read of the keys: the status and `Cache-Control` answered.
    fetches: Arc<Mutex<Vec<Fetched>>>,
}

impl Checker {
    fn new(issuer: &str, keys: KeysAt) -> Self {
        Self {
            issuer: issuer.to_owned(),
            keys,
            audience: Arc::default(),
            labels: Arc::default(),
            received: Arc::default(),
            fetches: Arc::default(),
        }
    }

    fn require_labels(&self, labels: &[(&str, &str)]) {
        *self.labels.lock().unwrap() = labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
    }

    /// The public keys, read now, without credentials; none once the Rewind server stopped (the
    /// end of a session may arrive after it).
    async fn jwks(&self) -> Value {
        let sent = match &self.keys {
            KeysAt::Http(url) => reqwest::Client::new().get(url).send().await,
            KeysAt::Socket(socket) => {
                reqwest::Client::builder()
                    .unix_socket(socket.clone())
                    .build()
                    .unwrap()
                    .get("http://localhost/organizations/local/jwks.json")
                    .send()
                    .await
            }
        };
        let Ok(response) = sent else {
            return json!({ "keys": [] });
        };
        let cache = response
            .headers()
            .get("cache-control")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        self.fetches
            .lock()
            .unwrap()
            .push((response.status().as_u16(), cache));
        response
            .json()
            .await
            .unwrap_or_else(|_| json!({ "keys": [] }))
    }

    /// The checks of a receiving server on `received`: its claims, or why it is refused.
    async fn check(&self, received: &Received) -> Result<Value, String> {
        let token = received
            .authorization
            .as_deref()
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or("no bearer token")?;
        let jwks = self.jwks().await;
        let audience = self.audience.lock().unwrap().clone();
        let claims = verify(
            token,
            &jwks,
            &Expected {
                issuer: &self.issuer,
                audience: &audience,
                method: &received.method,
                url: &received.url,
                body: &received.body,
            },
        )?;
        if claims.get("sub").is_some() {
            let required = self.labels.lock().unwrap().clone();
            for (key, value) in &required {
                if claims["labels"][key].as_str() != Some(value) {
                    return Err(format!("labels: {key}={value} is required"));
                }
            }
        }
        Ok(claims)
    }

    fn received(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    fn calls_of(&self, run: &str) -> Vec<Received> {
        self.received()
            .into_iter()
            .filter(|received| received.claims_or_token_names_run(run))
            .collect()
    }

    /// Serves `/mcp` with the action `whoami`: its URL, its audience.
    async fn serve_mcp(&self) -> String {
        let router = axum::Router::new()
            .nest_service("/mcp", mcp_service(Whoami::default()))
            .layer(middleware::from_fn_with_state(self.clone(), checked_gate));
        let url = format!("{}/mcp", listen(router).await);
        *self.audience.lock().unwrap() = url.clone();
        url
    }
}

impl Received {
    /// A call of `run`: its idempotency key names it.
    fn claims_or_token_names_run(&self, run: &str) -> bool {
        self.is_call_of(run)
            || self
                .claims
                .as_ref()
                .is_some_and(|claims| claims["run"].as_str() == Some(run))
    }
}

async fn checked_gate(State(checker): State<Checker>, request: Request, next: Next) -> Response {
    let (mut parts, mut received) = read(request).await;
    received.token = received
        .authorization
        .as_deref()
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    received.kid = received
        .token
        .as_deref()
        .and_then(|token| jsonwebtoken::decode_header(token).ok())
        .and_then(|header| header.kid);
    let checked = checker.check(&received).await;
    received.status = if checked.is_ok() { 200 } else { 401 };
    let body = received.body.clone();
    match checked {
        Ok(claims) => {
            received.claims = Some(claims.clone());
            checker.received.lock().unwrap().push(received);
            parts.extensions.insert(Verified(claims));
            next.run(Request::from_parts(parts, Body::from(body))).await
        }
        Err(why) => {
            received.refusal = Some(why.clone());
            checker.received.lock().unwrap().push(received);
            unauthorized(&why)
        }
    }
}

// -------------------------------------------------------------------------------------------
// Helpers

/// A value no other test uses.
fn unique(what: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{what}-{nanos:x}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn ns(name: &str) -> NamespaceName {
    name.parse().unwrap()
}

fn value(value: &str) -> PutSecret {
    PutSecret {
        value: value.into(),
        labels: Labels::new(),
    }
}

fn labelled(value: &str, labels: &[(&str, &str)]) -> PutSecret {
    PutSecret {
        value: value.into(),
        labels: labels.iter().copied().collect(),
    }
}

fn problem(error: rewind_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

fn attachments(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(server, secret)| ((*server).to_owned(), (*secret).to_owned()))
        .collect()
}

/// The action set of an MCP server `github` that takes a secret in `Authorization`, listed with
/// `listing` attached.
fn github_action_set(url: &str, listing: Option<&str>) -> CreateActionSet {
    CreateActionSet {
        mcp: vec![McpServer {
            name: "github".into(),
            url: Some(url.to_owned()),
            auth: Some(Auth::Secret {
                header: Some("authorization".into()),
                format: Some("Bearer {secret}".into()),
                encoding: None,
                env: None,
            }),
            ..McpServer::default()
        }],
        secrets: listing
            .map(|secret| attachments(&[("github", secret)]))
            .unwrap_or_default(),
        ..CreateActionSet::default()
    }
}

const WHOAMI: &str = r#"import { github } from "rewind:actions";

export default async function main() {
  return (await github.whoami({})).json();
}
"#;

/// Starts `code` in `namespace` with these attachments and labels, waiting up to 60 s.
async fn start(
    client: &Client,
    namespace: &str,
    code: &str,
    action_set: &str,
    secrets: &[(&str, &str)],
    labels: &[(&str, &str)],
) -> Result<Run, rewind_api::Error> {
    let mut request = StartRun::new(code);
    request.action_set = Some(action_set.to_owned());
    request.secrets = attachments(secrets);
    request.labels = labels.iter().copied().collect();
    request.wait = wait("60s");
    client
        .with_namespace(ns(namespace))
        .start_run(&request, None)
        .await
}

/// The run's output, once completed.
fn output(run: &Run) -> Value {
    assert_eq!(run.status, RunStatus::Completed, "{run:#?}");
    run.output.clone().unwrap().value.unwrap()
}

/// The ids of the runs of `namespace`.
async fn runs_in(client: &Client, namespace: &str) -> Vec<String> {
    client
        .with_namespace(ns(namespace))
        .list_runs(&RunFilter::default())
        .await
        .unwrap()
        .runs
        .into_iter()
        .map(|run| run.id)
        .collect()
}

/// The call of `run` parked now: its task, its seq and why.
async fn parked(client: &Client, run: &str) -> (String, u32, ParkedReason) {
    eventually(|| async {
        let run = client.get_run(run, None).await.unwrap();
        run.waiting_for.iter().find_map(|waiting| match waiting {
            Waiting::Call {
                seq,
                call_id,
                state: CallState::Parked,
                parked: Some(reason),
                ..
            } => Some((call_id.clone(), *seq, *reason)),
            _ => None,
        })
    })
    .await
}

/// Waits until `run` waits for the signal `name`.
async fn waits_for(client: &Client, run: &str, name: &str) {
    eventually(|| async {
        let run = client.get_run(run, None).await.unwrap();
        run.waiting_for
            .iter()
            .any(
                |waiting| matches!(waiting, Waiting::Signal { name: waited, .. } if waited == name),
            )
            .then_some(())
    })
    .await;
}

// -------------------------------------------------------------------------------------------
// Where a value must never be

/// What a scenario left behind that a secret's value must never be in.
#[derive(Default)]
struct Left {
    /// Runs: each is read whole (the run, its history and cassette, its events, events and
    /// trace), and its spans must have reached the Collector.
    runs: Vec<String>,
    /// Namespaces whose secrets are listed.
    namespaces: Vec<String>,
    /// The organization's secrets, and the namespaces' (`namespace`, `name`), whose usage is read.
    org_secrets: Vec<String>,
    ns_secrets: Vec<(String, String)>,
    /// Problems answered along the way.
    problems: Vec<Problem>,
}

/// GETs `path` with the operator's key, naming no namespace: its status and body.
async fn raw(server: &TestServer, path: &str) -> (u16, String) {
    let response = reqwest::Client::new()
        .get(format!("{}{path}", server.url))
        .bearer_auth(ADMIN_KEY)
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

/// No value of `values` is in any answer of the API about what the scenario left, a problem,
/// a webhook, the OpenTelemetry the server exported, nor the process's logs.
async fn nothing_holds(server: &TestServer, outside: &Outside, left: &Left, values: &[&str]) {
    let mut paths = vec![
        "/v1/runs".to_owned(),
        "/v1/runs/stats".to_owned(),
        "/v1/secrets".to_owned(),
        "/v1/action-sets".to_owned(),
        "/v1/scripts".to_owned(),
        "/v1/actions/stats".to_owned(),
        "/v1/subscriptions".to_owned(),
        "/v1/organization/signing-keys".to_owned(),
        "/organizations/acme/jwks.json".to_owned(),
    ];
    for run in &left.runs {
        let (status, body) = raw(server, &format!("/v1/runs/{run}")).await;
        assert_eq!(status, 200, "{body}");
        let history: Value = serde_json::from_str(&body).unwrap();
        let events = history["historyLen"].as_u64().unwrap();
        assert!(events > 0);
        paths.extend([
            format!("/v1/runs/{run}"),
            format!("/v1/runs/{run}/export"),
            format!("/v1/runs/{run}/export?as=cassette"),
            format!("/v1/runs/{run}/transcript"),
            format!("/v1/runs/{run}/events"),
        ]);
        paths.extend((0..events).map(|idx| format!("/v1/runs/{run}/events/{idx}")));
    }
    for namespace in &left.namespaces {
        paths.push(format!("/v1/namespaces/{namespace}/secrets"));
    }
    for name in &left.org_secrets {
        paths.push(format!("/v1/secrets/{name}/usage"));
    }
    for (namespace, name) in &left.ns_secrets {
        paths.push(format!("/v1/namespaces/{namespace}/secrets/{name}/usage"));
    }
    for path in &paths {
        let (status, body) = raw(server, path).await;
        assert_eq!(status, 200, "GET {path}: {body}");
        for value in values {
            assert!(!body.contains(value), "GET {path} answers a secret's value");
        }
    }
    for problem in &left.problems {
        let text = format!("{problem:?}");
        for value in values {
            assert!(
                !text.contains(value),
                "a problem holds a secret's value: {text}"
            );
        }
    }
    for body in outside.hooks.bodies() {
        for value in values {
            assert!(!body.contains(value), "a webhook holds a secret's value");
        }
    }
    // Each run's spans reached the Collector: what it holds is what the server exported.
    for run in &left.runs {
        eventually(|| async { outside.collector.holds(run).then_some(()) }).await;
    }
    for value in values {
        assert!(
            !outside.collector.holds(value),
            "the OpenTelemetry exported holds a secret's value"
        );
    }
    let logs = logs().0.lock().unwrap().clone();
    assert!(!logs.is_empty(), "the process's logs are captured");
    for value in values {
        assert!(
            !holds(&logs, value),
            "the process's logs hold a secret's value"
        );
    }
    eprintln!(
        "searched {} answers, {} problems, {} webhooks, {} OTLP exports and {} bytes of logs",
        paths.len(),
        left.problems.len(),
        outside.hooks.bodies().len(),
        outside.collector.exports(),
        logs.len()
    );
}

// -------------------------------------------------------------------------------------------
// Scenarios

/// Listed with an organization's secret, refused without; each end user's run calls with its
/// own secret; nothing attached, or a secret the namespace lacks, is refused before any run
/// exists, and nothing falls back on the organization's secret.
async fn each_end_user_calls_with_their_own_secret(server: &TestServer, outside: &Outside) {
    let listing = unique("gh-listing");
    let organization = unique("gh-organization");
    let alice = unique("gh-alice");
    let bob = unique("gh-bob");
    let github = Keyed::default();
    github.accept(&listing, "the listing");
    github.accept(&organization, "the organization");
    github.accept(&alice, "alice");
    github.accept(&bob, "bob");
    let url = github.serve().await;
    let customer = server.tenant_key("acme", "harness").await;
    let mut left = Left::default();

    // Listed without a secret: the server refuses, and the refusal says to attach one.
    let refused = problem(
        customer
            .create_action_set(&github_action_set(&url, None))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::ActionSetUnavailable));
    assert!(
        refused
            .detail
            .contains("attach an organization's secret for github in secrets"),
        "{}",
        refused.detail
    );
    assert!(
        github
            .received()
            .iter()
            .all(|received| received.authorization.is_none() && received.status == 401)
    );
    left.problems.push(refused);
    // A secret that does not exist yet.
    let unknown = problem(
        customer
            .create_action_set(&github_action_set(&url, Some("org/github-listing")))
            .await
            .unwrap_err(),
    );
    assert_eq!(
        (
            unknown.status,
            unknown.kind(),
            unknown.attachments[0].problem
        ),
        (
            422,
            Some(ProblemType::SecretRequired),
            AttachmentProblemKind::Unknown
        )
    );
    left.problems.push(unknown);

    customer
        .put_secret("github-listing", &value(&listing))
        .await
        .unwrap();
    customer
        .put_secret("github", &value(&organization))
        .await
        .unwrap();
    let action_set = customer
        .create_action_set(&github_action_set(&url, Some("org/github-listing")))
        .await
        .unwrap();
    assert_eq!(
        action_set
            .actions
            .iter()
            .map(|action| action.name.as_str())
            .collect::<Vec<_>>(),
        ["github.whoami", "signalUrl"]
    );
    assert!(
        github.received().iter().any(|received| {
            received.rpc.as_deref() == Some("tools/list")
                && received.authorization == Some(format!("Bearer {listing}"))
        }),
        "the listing carried the organization's secret attached for it"
    );
    let read = customer
        .secret_usage("github-listing")
        .await
        .unwrap()
        .secret;
    let last = read.last_read.expect("the listing read it");
    assert_eq!(
        (last.run, last.namespace),
        (None, None),
        "a registration's read names no run"
    );
    assert_eq!(read.reads.iter().map(|day| day.reads).sum::<u64>(), 1);

    customer
        .put_namespace_secret(&ns("alice"), "github", &value(&alice))
        .await
        .unwrap();
    customer
        .put_namespace_secret(&ns("bob"), "github", &value(&bob))
        .await
        .unwrap();
    let mut runs = BTreeMap::new();
    for user in ["alice", "bob"] {
        let run = start(
            &customer,
            user,
            WHOAMI,
            &action_set.id,
            &[("github", "ns/github")],
            &[],
        )
        .await
        .unwrap();
        assert_eq!(output(&run), json!({ "user": user }));
        assert_eq!(run.secrets, attachments(&[("github", "ns/github")]));
        runs.insert(user, run);
    }
    // Each call carried its own end user's secret, once, with its idempotency key.
    for (user, secret) in [("alice", &alice), ("bob", &bob)] {
        let run = &runs[user];
        let calls = github.calls_of(&run.id);
        assert_eq!(
            calls
                .iter()
                .map(|call| (call.authorization.clone(), call.status))
                .collect::<Vec<_>>(),
            [(Some(format!("Bearer {secret}")), 200)],
            "{user}'s run"
        );
        let usage = customer
            .namespace_secret_usage(&ns(user), "github")
            .await
            .unwrap();
        let last = usage.secret.last_read.unwrap();
        assert_eq!(
            (last.run.as_deref(), last.namespace),
            (Some(run.id.as_str()), Some(ns(user)))
        );
        assert_eq!(last.run_handle.as_deref(), Some(run.handle.as_str()));
        assert!(usage.secret.reads.iter().map(|day| day.reads).sum::<u64>() >= 1);
    }

    // Nothing attached: refused before any run exists, naming what may be attached.
    let before = runs_in(&customer, "alice").await;
    let missing = problem(
        start(&customer, "alice", WHOAMI, &action_set.id, &[], &[])
            .await
            .unwrap_err(),
    );
    assert_eq!(
        (missing.status, missing.kind()),
        (422, Some(ProblemType::SecretRequired))
    );
    let attachment = &missing.attachments[0];
    assert_eq!(
        (attachment.server.as_str(), attachment.problem),
        ("github", AttachmentProblemKind::Missing)
    );
    for attachable in ["org/github", "org/github-listing", "ns/github"] {
        assert!(
            attachment.attachable.iter().any(|name| name == attachable),
            "{:?}",
            attachment.attachable
        );
    }
    assert_eq!(runs_in(&customer, "alice").await, before);
    left.problems.push(missing);

    // Carol has no github of her own: ns/github is unknown in her namespace, and her run never
    // falls back on the organization's.
    let carol = problem(
        start(
            &customer,
            "carol",
            WHOAMI,
            &action_set.id,
            &[("github", "ns/github")],
            &[],
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(
        (carol.kind(), carol.attachments[0].problem),
        (
            Some(ProblemType::SecretRequired),
            AttachmentProblemKind::Unknown
        )
    );
    assert!(runs_in(&customer, "carol").await.is_empty());
    left.problems.push(carol);
    assert!(
        github
            .received()
            .iter()
            .all(|received| received.authorization != Some(format!("Bearer {organization}"))),
        "nothing was sent with the organization's github"
    );

    left.runs = runs.values().map(|run| run.id.clone()).collect();
    left.namespaces = vec!["alice".into(), "bob".into(), "carol".into()];
    left.org_secrets = vec!["github".into(), "github-listing".into()];
    left.ns_secrets = vec![
        ("alice".into(), "github".into()),
        ("bob".into(), "github".into()),
    ];
    nothing_holds(
        server,
        outside,
        &left,
        &[&listing, &organization, &alice, &bob],
    )
    .await;
}

const THREE_CALLS: &str = r#"import { github } from "rewind:actions";
import { signal } from "rewind";

export default async function main() {
  const first = (await github.whoami({})).json();
  await signal("go");
  const second = (await github.whoami({})).json();
  await signal("again");
  const third = (await github.whoami({})).json();
  return { first, second, third };
}
"#;

/// A secret deleted mid-run parks the next call before anything is sent (`missing`); a 401
/// parks it (`refused`); each time, putting the secret again sends the call with its
/// idempotency key, and the run ends as if nothing happened.
async fn a_call_waits_until_its_secret_is_put_again(server: &TestServer, outside: &Outside) {
    let first = unique("gh-alice-v1");
    let second = unique("gh-alice-v2");
    let listing = unique("gh-listing");
    let github = Keyed::default();
    github.accept(&listing, "the listing");
    github.accept(&first, "alice");
    let url = github.serve().await;
    let customer = server.tenant_key("acme", "harness").await;
    outside.subscribe(&customer).await;
    customer
        .put_secret("github-listing", &value(&listing))
        .await
        .unwrap();
    let action_set = customer
        .create_action_set(&github_action_set(&url, Some("org/github-listing")))
        .await
        .unwrap();
    customer
        .put_namespace_secret(&ns("alice"), "github", &value(&first))
        .await
        .unwrap();
    let alice = customer.with_namespace(ns("alice"));
    let mut request = StartRun::new(THREE_CALLS);
    request.action_set = Some(action_set.id.clone());
    request.secrets = attachments(&[("github", "ns/github")]);
    let run = alice.start_run(&request, None).await.unwrap();
    waits_for(&alice, &run.id, "go").await;

    // Deleted: the next call is parked before anything is sent.
    customer
        .delete_namespace_secret(&ns("alice"), "github")
        .await
        .unwrap();
    alice
        .signal_run(&run.id, "go", &json!(null), None)
        .await
        .unwrap();
    let (task, seq, reason) = parked(&alice, &run.id).await;
    assert_eq!(reason, ParkedReason::SecretMissing);
    assert_eq!(task, format!("{}.{seq}", run.id));
    let key = format!("{}/{seq}", run.id);
    let hook = eventually(|| async { outside.hooks.expired(&run.id).into_iter().next() }).await;
    assert_eq!(
        (
            hook.reason,
            hook.secret.as_deref(),
            hook.server.as_deref(),
            hook.seq,
            hook.call_id.as_str()
        ),
        (
            SecretExpiredReason::Missing,
            Some("ns/github"),
            Some("github"),
            seq,
            task.as_str()
        )
    );
    assert!(
        hook.message
            .contains("PUT /v1/namespaces/alice/secrets/github"),
        "{}",
        hook.message
    );
    assert!(
        github
            .calls_of(&run.id)
            .iter()
            .all(|call| call.key.as_deref() != Some(key.as_str())),
        "nothing was sent for the parked call"
    );

    // Put again: the call is sent, with its idempotency key.
    customer
        .put_namespace_secret(&ns("alice"), "github", &value(&first))
        .await
        .unwrap();
    waits_for(&alice, &run.id, "again").await;
    let sent: Vec<_> = github
        .calls_of(&run.id)
        .into_iter()
        .filter(|call| call.key.as_deref() == Some(key.as_str()))
        .map(|call| (call.authorization, call.status))
        .collect();
    assert_eq!(sent, [(Some(format!("Bearer {first}")), 200)]);

    // The server no longer takes the first value: its 401 parks the next call.
    github.refuse(&first);
    alice
        .signal_run(&run.id, "again", &json!(null), None)
        .await
        .unwrap();
    let (task, seq, reason) = parked(&alice, &run.id).await;
    assert_eq!(reason, ParkedReason::CredentialsRefused);
    let key = format!("{}/{seq}", run.id);
    let hook = eventually(|| async {
        outside
            .hooks
            .expired(&run.id)
            .into_iter()
            .find(|hook| hook.reason == SecretExpiredReason::Refused)
    })
    .await;
    assert_eq!(
        (hook.secret.as_deref(), hook.seq, hook.call_id.as_str()),
        (Some("ns/github"), seq, task.as_str())
    );
    let usage = customer
        .namespace_secret_usage(&ns("alice"), "github")
        .await
        .unwrap();
    assert_eq!(
        usage.waiting_runs,
        std::slice::from_ref(&run.id),
        "{usage:?}"
    );

    // A new value: the same call is sent again with the same key, and the run ends.
    github.accept(&second, "alice");
    customer
        .put_namespace_secret(&ns("alice"), "github", &value(&second))
        .await
        .unwrap();
    let ended = alice.get_run(&run.id, wait("60s")).await.unwrap();
    assert_eq!(
        output(&ended),
        json!({ "first": { "user": "alice" }, "second": { "user": "alice" }, "third": { "user": "alice" } })
    );
    let sent: Vec<_> = github
        .calls_of(&run.id)
        .into_iter()
        .filter(|call| call.key.as_deref() == Some(key.as_str()))
        .map(|call| (call.authorization, call.status))
        .collect();
    assert_eq!(
        sent,
        [
            (Some(format!("Bearer {first}")), 401),
            (Some(format!("Bearer {second}")), 200)
        ]
    );

    let left = Left {
        runs: vec![run.id.clone()],
        namespaces: vec!["alice".into()],
        org_secrets: vec!["github-listing".into()],
        ns_secrets: vec![("alice".into(), "github".into())],
        problems: Vec::new(),
    };
    nothing_holds(server, outside, &left, &[&first, &second, &listing]).await;
}

const CRM: &str = r#"import { crm } from "rewind:actions";

export default async function main() {
  return (await crm.whoami({})).json();
}
"#;

/// A signed MCP server checks every request against the published keys and learns the end user
/// from `sub`; a replaced key still verifies; a signature the server refuses (its labels) parks
/// the call until the harness approves it.
async fn a_signed_server_checks_every_request(server: &TestServer, outside: &Outside) {
    let crm = Checker::new(
        &format!("{PUBLIC_URL}/organizations/acme"),
        KeysAt::Http(format!("{}/organizations/acme/jwks.json", server.url)),
    );
    crm.require_labels(&[("env", "prod")]);
    let url = crm.serve_mcp().await;
    let customer = server.tenant_key("acme", "harness").await;
    let owner = server.tenant_key("acme", "owner").await;
    outside.subscribe(&customer).await;

    let action_set = customer
        .create_action_set(&CreateActionSet {
            mcp: vec![McpServer {
                name: "crm".into(),
                url: Some(url.clone()),
                auth: Some(Auth::Rewind {}),
                ..McpServer::default()
            }],
            ..CreateActionSet::default()
        })
        .await
        .unwrap();
    let listed = crm.received();
    assert!(listed.len() >= 2, "initialize and tools/list: {listed:?}");
    for received in &listed {
        assert_eq!(received.refusal, None, "{received:?}");
        let claims = received.claims.as_ref().unwrap();
        assert_eq!(claims["iss"], "https://rewind.test/organizations/acme");
        assert_eq!(claims["aud"], url.as_str());
        assert!(claims.get("sub").is_none(), "a listing acts for no one");
    }
    let keys = owner.signing_keys().await.unwrap();
    assert_eq!(keys.issuer, "https://rewind.test/organizations/acme");
    let first_kid = keys.keys[0].kid.clone();
    assert!(keys.keys[0].retired_at.is_none());

    let prod = [("env", "prod"), ("team", "support")];
    let run = start(&customer, "alice", CRM, &action_set.id, &[], &prod)
        .await
        .unwrap();
    assert_eq!(
        output(&run),
        json!({ "user": "alice" }),
        "the end user, from sub"
    );
    let calls: Vec<_> = crm
        .calls_of(&run.id)
        .into_iter()
        .filter(|received| received.rpc.as_deref() == Some("tools/call"))
        .collect();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let call = &calls[0];
    let claims = call.claims.as_ref().unwrap();
    let key = call.key.clone().unwrap();
    assert_eq!(claims["iss"], "https://rewind.test/organizations/acme");
    assert_eq!(claims["aud"], url.as_str());
    assert_eq!(claims["htm"], "POST");
    assert_eq!(claims["htu"], url.as_str());
    assert!(claims["body"].is_string(), "a POST's body is hashed");
    assert_eq!(claims["sub"], "alice");
    assert_eq!(
        claims["labels"],
        json!({ "env": "prod", "team": "support" })
    );
    assert_eq!(claims["run"], run.id.as_str());
    assert_eq!(claims["action"], "crm.whoami");
    assert_eq!(
        claims["tool"], claims["action"],
        "the old name, for verifiers on SDK 0.3.0"
    );
    assert_eq!(claims["idempotency_key"], key.as_str());
    assert_eq!(claims["jti"], format!("{key}/1"));
    assert_eq!(
        claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
        60
    );
    assert_eq!(call.kid.as_deref(), Some(first_kid.as_str()));
    assert!(
        crm.received()
            .iter()
            .all(|received| received.refusal.is_none()),
        "every request of the session verified"
    );
    let fetches = crm.fetches.lock().unwrap().clone();
    assert!(!fetches.is_empty());
    assert!(
        fetches
            .iter()
            .all(|fetch| *fetch == (200, Some("max-age=300".to_owned()))),
        "the keys are read without credentials, cacheable 5 minutes: {fetches:?}"
    );

    // Replaced: the old key stops signing and stays published, so a request it signed still
    // verifies; the next calls are signed with the new one.
    let rotated = owner.rotate_signing_key().await.unwrap();
    assert_ne!(rotated.kid, first_kid);
    let jwks = crm.jwks().await;
    let still = verify(
        call.token.as_deref().unwrap(),
        &jwks,
        &Expected {
            issuer: "https://rewind.test/organizations/acme",
            audience: &url,
            method: &call.method,
            url: &call.url,
            body: &call.body,
        },
    );
    assert!(still.is_ok(), "{still:?}");
    let listed = owner.signing_keys().await.unwrap();
    assert_eq!(
        listed
            .keys
            .iter()
            .map(|key| (key.kid.clone(), key.retired_at.is_some()))
            .collect::<Vec<_>>(),
        [(rotated.kid.clone(), false), (first_kid.clone(), true)]
    );
    let bob = start(&customer, "bob", CRM, &action_set.id, &[], &prod)
        .await
        .unwrap();
    assert_eq!(output(&bob), json!({ "user": "bob" }));
    let call = crm
        .calls_of(&bob.id)
        .into_iter()
        .find(|received| received.rpc.as_deref() == Some("tools/call"))
        .unwrap();
    assert_eq!(call.kid.as_deref(), Some(rotated.kid.as_str()));
    assert_eq!(call.claims.unwrap()["sub"], "bob");

    // A run of another environment: the server refuses its signature, the call is parked, and
    // once the server takes it, the harness approves it and it is sent with the same key.
    let mut request = StartRun::new(CRM);
    request.action_set = Some(action_set.id.clone());
    request.labels = [("env", "dev")].into_iter().collect();
    let carol = customer.with_namespace(ns("carol"));
    let dev = carol.start_run(&request, None).await.unwrap();
    let (task, seq, reason) = parked(&carol, &dev.id).await;
    assert_eq!(reason, ParkedReason::CredentialsRefused);
    let hook = eventually(|| async { outside.hooks.expired(&dev.id).into_iter().next() }).await;
    assert_eq!(
        (hook.reason, hook.secret, hook.server.as_deref()),
        (SecretExpiredReason::Refused, None, Some("crm"))
    );
    crm.require_labels(&[]);
    customer.approve_call(&task).await.unwrap();
    let ended = carol.get_run(&dev.id, wait("60s")).await.unwrap();
    assert_eq!(output(&ended), json!({ "user": "carol" }));
    let key = format!("{}/{seq}", dev.id);
    let sent: Vec<_> = crm
        .calls_of(&dev.id)
        .into_iter()
        .filter(|received| received.key.as_deref() == Some(key.as_str()))
        .map(|received| received.refusal)
        .collect();
    assert_eq!(
        sent,
        [Some("labels: env=prod is required".to_owned()), None]
    );
}

/// The signed API: one GET with a path and a query, one POST with a body and an
/// `Idempotency-Key`; each request checked against the keys the embedded server publishes.
fn signed_api_document(base: &str) -> Value {
    json!({
        "openapi": "3.1.0",
        "info": { "title": "notes", "version": "1" },
        "servers": [{ "url": base }],
        "paths": {
            "/me/{id}": {
                "get": {
                    "operationId": "whoAmI",
                    "parameters": [
                        { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } },
                        { "name": "verbose", "in": "query", "schema": { "type": "boolean" } }
                    ],
                    "responses": { "200": { "description": "who", "content": { "application/json": { "schema": { "type": "object" } } } } }
                }
            },
            "/notes": {
                "post": {
                    "operationId": "addNote",
                    "parameters": [{ "name": "Idempotency-Key", "in": "header", "schema": { "type": "string" } }],
                    "requestBody": { "required": true, "content": { "application/json": { "schema": {
                        "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"]
                    } } } },
                    "responses": { "201": { "description": "added", "content": { "application/json": { "schema": { "type": "object" } } } } }
                }
            }
        }
    })
}

const NOTES: &str = r#"import { notes } from "rewind:actions";

export default async function main() {
  const me = await notes.whoAmI({ id: "a b/c", verbose: true });
  const added = await notes.addNote({ body: { text: "hello" } });
  return { me, added };
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signed_api_learns_the_end_user_from_the_signature() {
    logs();
    let dir = tempfile::tempdir().unwrap();
    let rewind_dir = dir.path().join(".rewind");
    let args = ServerArgs::try_parse_from([
        "rewind server",
        "--embedded",
        "--dir",
        rewind_dir.to_str().unwrap(),
        "--public-url",
        PUBLIC_URL,
    ])
    .unwrap();
    let running = rewind_server::start(args).await.unwrap();
    let socket = running.socket().unwrap().clone();
    let api = Checker::new(
        &format!("{PUBLIC_URL}/organizations/local"),
        KeysAt::Socket(socket.clone()),
    );
    api.require_labels(&[("env", "prod")]);
    let router = axum::Router::new()
        .route(
            "/v1/me/{id}",
            axum::routing::get(
                |axum::extract::Path(id): axum::extract::Path<String>,
                 axum::extract::Query(query): axum::extract::Query<BTreeMap<String, String>>,
                 axum::Extension(Verified(claims)): axum::Extension<Verified>| async move {
                    axum::Json(json!({
                        "id": id,
                        "verbose": query.get("verbose"),
                        "namespace": claims["sub"],
                        "labels": claims["labels"],
                        "action": claims["action"],
                    }))
                },
            ),
        )
        .route(
            "/v1/notes",
            axum::routing::post(
                |headers: axum::http::HeaderMap,
                 axum::Extension(Verified(claims)): axum::Extension<Verified>,
                 axum::Json(body): axum::Json<Value>| async move {
                    let key = headers["idempotency-key"].to_str().unwrap().to_owned();
                    (
                        StatusCode::CREATED,
                        axum::Json(json!({
                            "text": body["text"],
                            "namespace": claims["sub"],
                            "same_key": claims["idempotency_key"] == key.as_str(),
                        })),
                    )
                },
            ),
        )
        .layer(middleware::from_fn_with_state(api.clone(), checked_gate));
    let base = format!("{}/v1", listen(router).await);
    *api.audience.lock().unwrap() = base.clone();

    let client = Client::embedded(&socket).unwrap();
    let registered = client
        .create_action_set(&CreateActionSet {
            apis: vec![Api {
                name: "notes".into(),
                openapi: OpenApiDocument::Document(signed_api_document(&base)),
                base_url: None,
                auth: Some(Auth::Rewind {}),
                operations: None,
                concurrency: None,
                policies: BTreeMap::new(),
            }],
            ..CreateActionSet::default()
        })
        .await
        .unwrap();
    assert_eq!(registered.apis[0].auth, Some(Auth::Rewind {}));

    let run = start(
        &client,
        "alice",
        NOTES,
        &registered.id,
        &[],
        &[("env", "prod")],
    )
    .await
    .unwrap();
    assert_eq!(
        output(&run),
        json!({
            "me": { "id": "a b/c", "verbose": "true", "namespace": "alice", "labels": { "env": "prod" }, "action": "notes.whoAmI" },
            "added": { "text": "hello", "namespace": "alice", "same_key": true }
        })
    );
    let received = api.received();
    assert_eq!(received.len(), 2, "{received:?}");
    assert!(received.iter().all(|request| request.refusal.is_none()));
    let get = &received[0];
    assert_eq!(get.url, format!("{base}/me/a%20b%2Fc?verbose=true"));
    let claims = get.claims.as_ref().unwrap();
    assert_eq!(
        (&claims["htm"], &claims["htu"], &claims["aud"]),
        (&json!("GET"), &json!(get.url), &json!(base))
    );
    assert!(claims.get("body").is_none(), "a GET carries no body");
    let post = &received[1];
    let claims = post.claims.as_ref().unwrap();
    assert_eq!(claims["htm"], "POST");
    assert_eq!(
        claims["body"].as_str(),
        Some(rewind_outbound::body_digest(&post.body).as_str())
    );
    assert_eq!(
        post.key.as_deref(),
        claims["idempotency_key"].as_str(),
        "the Idempotency-Key header is the signed one"
    );
    assert_eq!(claims["jti"], format!("{}/1", post.key.as_deref().unwrap()));
    let fetches = api.fetches.lock().unwrap().clone();
    assert!(
        fetches
            .iter()
            .all(|fetch| *fetch == (200, Some("max-age=300".to_owned()))),
        "{fetches:?}"
    );
    running.stop().await;
}

/// A key limited to `env=dev` sees, lists for and attaches only the organization's secrets
/// carrying `env=dev`, and never writes a namespace's.
async fn labels_bound_what_a_key_attaches(server: &TestServer, outside: &Outside) {
    let prod = unique("gh-prod");
    let dev = unique("gh-dev");
    let github = Keyed::default();
    github.accept(&prod, "production");
    github.accept(&dev, "development");
    let url = github.serve().await;
    let customer = server.tenant_key("acme", "harness").await;
    customer
        .put_secret("github-prod", &labelled(&prod, &[("env", "prod")]))
        .await
        .unwrap();
    customer
        .put_secret("github-dev", &labelled(&dev, &[("env", "dev")]))
        .await
        .unwrap();
    let key = server
        .admin
        .create_api_key(&rewind_api::access::CreateApiKey {
            organization_id: Some("acme".into()),
            name: "the dev environment".into(),
            role: "harness".into(),
            allowed_labels: [("env", "dev")].into_iter().collect(),
        })
        .await
        .unwrap();
    let limited = server.bare(key.key.as_deref().unwrap());
    let mut left = Left::default();

    assert_eq!(
        limited
            .secrets()
            .await
            .unwrap()
            .secrets
            .iter()
            .map(|secret| secret.reference.as_str())
            .collect::<Vec<_>>(),
        ["org/github-dev"]
    );
    let usage = problem(limited.secret_usage("github-prod").await.unwrap_err());
    assert_eq!(usage.status, 404);
    left.problems.push(usage);

    // Its listing: the production secret is unknown to it.
    let listing = problem(
        limited
            .create_action_set(&github_action_set(&url, Some("org/github-prod")))
            .await
            .unwrap_err(),
    );
    assert_eq!(
        (listing.kind(), listing.attachments[0].problem),
        (
            Some(ProblemType::SecretRequired),
            AttachmentProblemKind::Unknown
        )
    );
    assert_eq!(listing.attachments[0].attachable, ["org/github-dev"]);
    left.problems.push(listing);
    let action_set = limited
        .create_action_set(&github_action_set(&url, Some("org/github-dev")))
        .await
        .unwrap();

    // Its runs: the production secret is unknown, refused before any run exists.
    let refused = problem(
        start(
            &limited,
            "alice",
            WHOAMI,
            &action_set.id,
            &[("github", "org/github-prod")],
            &[],
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(
        (
            refused.status,
            refused.kind(),
            refused.attachments[0].problem
        ),
        (
            422,
            Some(ProblemType::SecretRequired),
            AttachmentProblemKind::Unknown
        )
    );
    assert!(
        !refused.attachments[0]
            .attachable
            .iter()
            .any(|name| name == "org/github-prod"),
        "{:?}",
        refused.attachments[0].attachable
    );
    assert!(runs_in(&limited, "alice").await.is_empty());
    left.problems.push(refused);
    let run = start(
        &limited,
        "alice",
        WHOAMI,
        &action_set.id,
        &[("github", "org/github-dev")],
        &[],
    )
    .await
    .unwrap();
    assert_eq!(output(&run), json!({ "user": "development" }));
    assert_eq!(run.labels.0["env"], "dev", "its runs take its labels");
    assert!(
        github
            .received()
            .iter()
            .all(|received| received.authorization != Some(format!("Bearer {prod}"))),
        "the production secret was never sent"
    );

    // A namespace's secret serves the end user's runs whatever their labels: it never writes
    // one, nor deletes or replaces the organization's of another environment.
    for (attempt, status) in [
        (
            limited
                .put_namespace_secret(&ns("alice"), "github", &value("from dev"))
                .await
                .map(|_| ()),
            403,
        ),
        (limited.delete_secret("github-prod").await, 404),
        (
            limited
                .put_secret("github-prod", &value("from dev"))
                .await
                .map(|_| ()),
            403,
        ),
    ] {
        let refused = problem(attempt.unwrap_err());
        assert_eq!(refused.status, status, "{refused:?}");
        left.problems.push(refused);
    }
    assert!(
        customer
            .namespace_secrets(&ns("alice"))
            .await
            .unwrap()
            .secrets
            .is_empty()
    );
    let unchanged = customer.secret_usage("github-prod").await.unwrap();
    assert!(unchanged.secret.updated_by.starts_with("key:"));
    assert_eq!(unchanged.secret.labels.0["env"], "prod");

    left.runs = vec![run.id.clone()];
    left.namespaces = vec!["alice".into()];
    left.org_secrets = vec!["github-prod".into(), "github-dev".into()];
    nothing_holds(server, outside, &left, &[&prod, &dev]).await;
}

// -------------------------------------------------------------------------------------------
// On Turso

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_end_user_calls_with_their_own_secret_on_turso() {
    let outside = Outside::serve().await;
    let server = outside.on_turso().await;
    each_end_user_calls_with_their_own_secret(&server, &outside).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_waits_until_its_secret_is_put_again_on_turso() {
    let outside = Outside::serve().await;
    let server = outside.on_turso().await;
    a_call_waits_until_its_secret_is_put_again(&server, &outside).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signed_server_checks_every_request_on_turso() {
    let outside = Outside::serve().await;
    let server = outside.on_turso().await;
    a_signed_server_checks_every_request(&server, &outside).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn labels_bound_what_a_key_attaches_on_turso() {
    let outside = Outside::serve().await;
    let server = outside.on_turso().await;
    labels_bound_what_a_key_attaches(&server, &outside).await;
    server.stop().await;
}

// -------------------------------------------------------------------------------------------
// On Postgres (Docker): skipped, with a message, when Docker is absent, unless
// `REWIND_REQUIRE_DOCKER` is set

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_end_user_calls_with_their_own_secret_on_postgres() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let outside = Outside::serve().await;
    let server = outside.on_postgres(&postgres).await;
    each_end_user_calls_with_their_own_secret(&server, &outside).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_waits_until_its_secret_is_put_again_on_postgres() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let outside = Outside::serve().await;
    let server = outside.on_postgres(&postgres).await;
    a_call_waits_until_its_secret_is_put_again(&server, &outside).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signed_server_checks_every_request_on_postgres() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let outside = Outside::serve().await;
    let server = outside.on_postgres(&postgres).await;
    a_signed_server_checks_every_request(&server, &outside).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn labels_bound_what_a_key_attaches_on_postgres() {
    let Some(postgres) = Postgres::start() else {
        return;
    };
    let outside = Outside::serve().await;
    let server = outside.on_postgres(&postgres).await;
    labels_bound_what_a_key_attaches(&server, &outside).await;
    server.stop().await;
}
