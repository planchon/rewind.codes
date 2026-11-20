//! An MCP server for the tests, built with `rmcp`: served over streamable HTTP behind a token
//! check, or over stdio by this test binary itself (`REWIND_MCP_FIXTURE=stdio`).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
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

/// What the server knows and saw, shared by its sessions and the test.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    pub(crate) tools: Vec<Tool>,
    pub(crate) calls: Vec<Recorded>,
    /// Sessions opened (HTTP).
    pub(crate) sessions: usize,
    /// Calls of `slow` cancelled by the client.
    pub(crate) cancelled: usize,
    /// `tools/list` never ends: each page holds one more tool and the cursor of the next.
    pub(crate) endless_list: bool,
    /// Pages of `tools/list` served.
    pub(crate) pages: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct Recorded {
    pub(crate) name: String,
    pub(crate) arguments: Value,
    pub(crate) meta: Value,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Fixture {
    pub(crate) state: Arc<Mutex<Shared>>,
}

fn object(value: Value) -> Arc<Map<String, Value>> {
    match value {
        Value::Object(object) => Arc::new(object),
        _ => unreachable!("schemas are objects"),
    }
}

pub(crate) fn tool(name: &str, description: &str, input: Value, output: Option<Value>) -> Tool {
    let mut tool = Tool::new(name.to_owned(), description.to_owned(), object(input));
    tool.output_schema = output.map(object);
    tool
}

/// The tools of the fixture.
pub(crate) fn tools() -> Vec<Tool> {
    let id = json!({ "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"] });
    vec![
        tool("lookup", "Find a customer by id.", id.clone(), None),
        tool(
            "get_issue",
            "Get one issue.",
            json!({ "type": "object", "properties": { "number": { "type": "integer" } }, "required": ["number"] }),
            Some(json!({
                "type": "object",
                "properties": { "number": { "type": "integer" }, "title": { "type": "string" } },
                "required": ["number", "title"]
            })),
        ),
        tool(
            "fail",
            "Always reports an error.",
            json!({ "type": "object" }),
            None,
        ),
        tool(
            "invalid",
            "Always refuses its arguments.",
            json!({ "type": "object" }),
            None,
        ),
        tool(
            "internal",
            "Always fails inside.",
            json!({ "type": "object" }),
            None,
        ),
        tool(
            "slow",
            "Answers after `ms` milliseconds.",
            json!({ "type": "object", "properties": { "ms": { "type": "integer" } } }),
            None,
        ),
        tool(
            "meta",
            "Returns the request's _meta.",
            json!({ "type": "object" }),
            None,
        ),
        tool(
            "env",
            "Returns parts of its environment.",
            json!({ "type": "object" }),
            None,
        ),
    ]
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let fixture = Self::default();
        fixture.state().tools = tools();
        fixture
    }

    pub(crate) fn state(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.state.lock().unwrap()
    }
}

fn text(value: Value) -> CallToolResponse {
    CallToolResult::success(vec![ContentBlock::text(value.to_string())]).into()
}

impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let mut state = self.state();
        state.pages += 1;
        if state.endless_list {
            let page: u64 = request
                .and_then(|request| request.cursor)
                .and_then(|cursor| cursor.parse().ok())
                .unwrap_or(0);
            let mut result = ListToolsResult::with_all_items(vec![tool(
                &format!("t{page}"),
                "One more.",
                json!({ "type": "object" }),
                None,
            )]);
            result.next_cursor = Some((page + 1).to_string());
            return Ok(result);
        }
        Ok(ListToolsResult::with_all_items(state.tools.clone()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let arguments = Value::Object(request.arguments.clone().unwrap_or_default());
        let mut meta = context.meta.0.0.clone();
        if let Some(own) = &request.meta {
            meta.extend(own.0.0.clone());
        }
        self.state().calls.push(Recorded {
            name: request.name.to_string(),
            arguments: arguments.clone(),
            meta: Value::Object(meta.clone()),
        });
        match request.name.as_ref() {
            "lookup" => Ok(text(json!({ "id": arguments["id"], "name": "Ada" }))),
            "get_issue" => {
                let mut result = CallToolResult::success(vec![ContentBlock::text("issue")]);
                result.structured_content =
                    Some(json!({ "number": arguments["number"], "title": "Crash" }));
                Ok(result.into())
            }
            "fail" => {
                Ok(CallToolResult::error(vec![ContentBlock::text("no such customer")]).into())
            }
            "invalid" => Err(ErrorData::invalid_params(
                "bad arguments",
                Some(json!({ "field": "x" })),
            )),
            "internal" => Err(ErrorData::internal_error("database down", None)),
            "slow" => {
                let ms = arguments["ms"].as_u64().unwrap_or(0);
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(ms)) => Ok(text(json!("done"))),
                    () = context.ct.cancelled() => {
                        self.state().cancelled += 1;
                        Ok(text(json!("cancelled")))
                    }
                }
            }
            "meta" => Ok(text(Value::Object(meta))),
            "env" => Ok(text(json!({
                "token": std::env::var("FIXTURE_TOKEN").ok(),
                "manifest_dir": std::env::var("CARGO_MANIFEST_DIR").ok(),
                "path": std::env::var("PATH").is_ok(),
            }))),
            other => Err(ErrorData::invalid_params(
                format!("unknown tool {other}"),
                None,
            )),
        }
    }
}

/// What the HTTP server accepts: no token, or `Authorization: Bearer <token>`; or, when
/// `signed` is set, only requests whose JWT checks against its keys.
#[derive(Debug, Clone, Default)]
pub(crate) struct Auth {
    pub(crate) token: Arc<Mutex<Option<String>>>,
    /// Send `WWW-Authenticate` with the 401.
    pub(crate) challenge: bool,
    pub(crate) signed: Option<Signed>,
}

/// A server that checks Rewind's signature, as a customer's does.
#[derive(Debug, Clone)]
pub(crate) struct Signed {
    /// `{"keys": [...]}`, as `{iss}/jwks.json` publishes them.
    pub(crate) jwks: Value,
    pub(crate) issuer: String,
    /// What each request carried: its HTTP method, its JSON-RPC method, the JWT's claims.
    pub(crate) seen: Arc<Mutex<Vec<Seen>>>,
}

#[derive(Debug, Clone)]
pub(crate) struct Seen {
    pub(crate) http: String,
    pub(crate) rpc: Option<String>,
    pub(crate) claims: Value,
}

/// The URL this server answers at, which the signature's audience must be.
#[derive(Debug, Clone)]
struct Audience(Arc<Mutex<String>>);

async fn check(
    State((auth, audience)): State<(Auth, Audience)>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(signed) = &auth.signed {
        let (parts, body) = request.into_parts();
        let body = axum::body::to_bytes(body, 1 << 20).await.unwrap();
        let token = parts
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .unwrap_or_default();
        let host = parts
            .headers
            .get("host")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let url = format!("http://{host}{}", parts.uri);
        let audience = audience.0.lock().unwrap().clone();
        let expected = rewind_outbound::Expected {
            issuer: &signed.issuer,
            audience: &audience,
            method: parts.method.as_str(),
            url: &url,
            body: &body,
        };
        match rewind_outbound::verify(token, &signed.jwks, &expected) {
            Ok(claims) => {
                let rpc = serde_json::from_slice::<Value>(&body)
                    .ok()
                    .and_then(|message| message["method"].as_str().map(str::to_owned));
                signed.seen.lock().unwrap().push(Seen {
                    http: parts.method.to_string(),
                    rpc,
                    claims,
                });
            }
            Err(reason) => return (StatusCode::UNAUTHORIZED, reason).into_response(),
        }
        return next
            .run(Request::from_parts(parts, axum::body::Body::from(body)))
            .await;
    }
    let expected = auth.token.lock().unwrap().clone();
    if let Some(expected) = expected {
        let given = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        if given != Some(format!("Bearer {expected}").as_str()) {
            return if auth.challenge {
                (
                    StatusCode::UNAUTHORIZED,
                    [("www-authenticate", "Bearer realm=\"fixture\"")],
                    "token expired",
                )
                    .into_response()
            } else {
                (StatusCode::UNAUTHORIZED, "token expired").into_response()
            };
        }
    }
    next.run(request).await
}

/// A running HTTP server; dropping it stops the server.
#[derive(Debug)]
pub(crate) struct Server {
    pub(crate) url: String,
    stop: CancellationToken,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Serves the fixture over streamable HTTP at `/mcp`, on a free port of 127.0.0.1; `/moved`
/// redirects there with a 307.
pub(crate) async fn serve_http(fixture: Fixture, auth: Auth) -> Server {
    let audience = Audience(Arc::new(Mutex::new(String::new())));
    let stop = CancellationToken::new();
    let sessions = fixture.clone();
    let service = StreamableHttpService::new(
        move || {
            sessions.state().sessions += 1;
            Ok(sessions.clone())
        },
        Arc::new(LocalSessionManager::default()),
        // The egress tests reach it under names of their own, which their resolvers answer.
        StreamableHttpServerConfig::default()
            .with_sse_keep_alive(None)
            .disable_allowed_hosts()
            .with_cancellation_token(stop.child_token()),
    );
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .route(
            "/moved",
            axum::routing::any(|| async {
                (StatusCode::TEMPORARY_REDIRECT, [("location", "/mcp")]).into_response()
            }),
        )
        .layer(middleware::from_fn_with_state(
            (auth, audience.clone()),
            check,
        ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    *audience.0.lock().unwrap() = format!("http://{address}/mcp");
    let shutdown = stop.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
            .await;
    });
    Server {
        url: format!("http://{address}/mcp"),
        stop,
    }
}

/// Serves the fixture on this process's standard input and output, until the client leaves.
pub(crate) fn serve_stdio() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        use rmcp::ServiceExt;
        let service = Fixture::new()
            .serve(rmcp::transport::stdio())
            .await
            .unwrap();
        let _ = service.waiting().await;
    });
}
