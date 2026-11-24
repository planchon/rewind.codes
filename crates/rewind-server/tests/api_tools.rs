//! Action sets with APIs read from OpenAPI documents, and MCP servers listed with Rewind's signature
//! (`archi/04-tools.md`, "API actions" and "Signed calls"). The servers of these tests listen on
//! 127.0.0.1: the embedded server reaches them, calls included; a deployed server reaches them
//! only through `--allow-internal-addresses`, and calls APIs over https only.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::{Arc, Mutex};

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use clap::Parser;
use rewind_api::access::BuiltInRole;
use rewind_api::action_sets::{
    ActionPolicy, Api, Auth, CreateActionSet, McpServer, OnInterruption, OpenApiDocument,
};
use rewind_api::common::RunStatus;
use rewind_api::runs::StartRun;
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
use serde_json::{Value, json};
use support::{TestServer, wait};

/// The fake API's document: two operations kept, one left out.
fn document(server: &str) -> Value {
    json!({
        "openapi": "3.0.3",
        "info": { "title": "measures", "version": "1" },
        "servers": [{ "url": server }],
        "paths": {
            "/measures": {
                "post": {
                    "operationId": "createMeasure",
                    "parameters": [{ "name": "Idempotency-Key", "in": "header", "schema": { "type": "string" } }],
                    "requestBody": { "required": true, "content": { "application/json": { "schema": {
                        "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"]
                    } } } },
                    "responses": { "201": { "description": "created", "content": { "application/json": { "schema": {
                        "type": "object", "properties": { "created": { "type": "string" }, "key": { "type": "string" } }
                    } } } } }
                }
            },
            "/measures/{id}": {
                "get": {
                    "operationId": "getMeasure",
                    "summary": "Get a measure.",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "it", "content": { "application/json": { "schema": {
                        "type": "object", "properties": { "id": { "type": "string" }, "value": { "type": "number" } }
                    } } } } }
                },
                "put": {
                    "operationId": "importMeasure",
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "requestBody": { "content": { "text/csv": {} } },
                    "responses": { "204": { "description": "imported" } }
                }
            }
        }
    })
}

/// Serves the fake API on 127.0.0.1, with its document at `/openapi.json` (its server
/// `document_server`) and a page that is no document at `/admin`: its base URL `…/v1`, and
/// the idempotency keys it received.
async fn serve_api(document_server: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
    let keys: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = keys.clone();
    let router = axum::Router::new()
        .route(
            "/v1/measures/{id}",
            axum::routing::get(
                |axum::extract::Path(id): axum::extract::Path<String>| async move {
                    axum::Json(json!({ "id": id, "value": 3.5 }))
                },
            ),
        )
        .route(
            "/v1/measures",
            axum::routing::post(
                move |headers: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| {
                    let seen = seen.clone();
                    async move {
                        let key = headers["idempotency-key"].to_str().unwrap().to_owned();
                        seen.lock().unwrap().push(key.clone());
                        (
                            StatusCode::CREATED,
                            axum::Json(json!({ "created": body["name"], "key": key })),
                        )
                    }
                },
            ),
        )
        .route(
            "/openapi.json",
            axum::routing::get(move || async move { axum::Json(document(document_server)) }),
        )
        .route(
            "/admin",
            axum::routing::get(|| async { axum::Json(json!({ "secret": "hunter2" })) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    (format!("http://{address}/v1"), keys)
}

fn api(document: OpenApiDocument, base_url: Option<String>) -> CreateActionSet {
    CreateActionSet {
        apis: vec![Api {
            name: "dax".into(),
            openapi: document,
            base_url,
            auth: None,
            operations: None,
            concurrency: None,
            policies: [(
                "getMeasure".to_owned(),
                ActionPolicy {
                    approval: Some(false),
                    ..ActionPolicy::default()
                },
            )]
            .into(),
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

const SCRIPT: &str = r#"import { dax } from "rewind:actions";

export default async function () {
  const measure = await dax.getMeasure({ id: "m-1" });
  const created = await dax.createMeasure({ body: { name: "weight" } });
  return { value: measure.value, created: created.created, key: created.key };
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_embedded_server_reads_an_api_and_calls_it() {
    let (base, keys) = serve_api("https://unused.example.com/v1").await;
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
    let client = Client::embedded(running.socket().unwrap())
        .unwrap()
        .with_namespace(support::test_namespace());

    // Over http, on the embedded server.
    let registered = client
        .create_action_set(&api(
            OpenApiDocument::Document(document("https://unused.example.com/v1")),
            Some(base.clone()),
        ))
        .await
        .unwrap();
    let names: Vec<&str> = registered
        .actions
        .iter()
        .map(|action| action.name.as_str())
        .collect();
    assert_eq!(names, ["dax.createMeasure", "dax.getMeasure", "webhook"]);
    assert_eq!(registered.apis.len(), 1);
    assert_eq!(registered.apis[0].base_url, base);
    assert_eq!(registered.apis[0].actions, 2);
    assert_eq!(registered.skipped.len(), 1);
    assert_eq!(registered.skipped[0].path, "/measures/{id}");
    assert_eq!(registered.skipped[0].method, "put");
    assert_eq!(
        registered.skipped[0].reason,
        "its body is text/csv, and Rewind sends application/json only"
    );
    let create = registered
        .actions
        .iter()
        .find(|action| action.name == "dax.createMeasure")
        .unwrap();
    // A POST that declares Idempotency-Key may be sent again.
    assert_eq!(create.policy.on_interruption, Some(OnInterruption::Retry));
    assert!(
        registered.declarations.dts.contains("getMeasure"),
        "{}",
        registered.declarations.dts
    );

    let mut start = StartRun::new(SCRIPT);
    start.action_set = Some(registered.id.clone());
    start.wait = wait("60s");
    let run = client.start_run(&start, None).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{run:?}");
    let output = run.output.unwrap().value.unwrap();
    let key = format!("{}/2", run.id);
    assert_eq!(
        output,
        json!({ "value": 3.5, "created": "weight", "key": key })
    );
    assert_eq!(*keys.lock().unwrap(), [key]);
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deployed_server_reads_documents_through_the_egress_checks() {
    let (base, _) = serve_api("https://api.example.com/v1").await;
    let document_url = base.replace("/v1", "/openapi.json");
    let admin_url = base.replace("/v1", "/admin");

    // Without the operator's leave, a document on 127.0.0.1 is not read.
    let closed = TestServer::start(&[]).await;
    let harness = closed.key(BuiltInRole::Harness, &[]).await;
    let refused = problem(
        harness
            .create_action_set(&api(OpenApiDocument::Url(document_url.clone()), None))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::ActionSetUnavailable));
    assert_eq!(
        refused.detail,
        format!(
            "API dax: the OpenAPI document {document_url} cannot be read: 127.0.0.1 is a \
             loopback address: MCP servers and APIs are not reached there unless the operator \
             allows it"
        )
    );
    // Calls go over https only.
    let http = problem(
        harness
            .create_action_set(&api(
                OpenApiDocument::Document(document("https://api.example.com/v1")),
                Some(base.clone()),
            ))
            .await
            .unwrap_err(),
    );
    assert_eq!(http.kind(), Some(ProblemType::InvalidActionSet));
    assert!(
        http.detail
            .ends_with("is http: a deployed server calls APIs over https"),
        "{}",
        http.detail
    );
    closed.stop().await;

    let open = TestServer::start(&["--allow-internal-addresses", "127.0.0.1"]).await;
    let harness = open.key(BuiltInRole::Harness, &[]).await;
    let registered = harness
        .create_action_set(&api(OpenApiDocument::Url(document_url), None))
        .await
        .unwrap();
    assert_eq!(registered.apis[0].base_url, "https://api.example.com/v1");
    assert_eq!(registered.skipped.len(), 1);
    // Registered again: the same action set, and its skipped operations said again.
    let again = harness
        .create_action_set(&api(
            OpenApiDocument::Document(document("https://api.example.com/v1")),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(again.id, registered.id);
    assert_eq!(again.skipped.len(), 1);

    // What an internal address answers is not quoted.
    let withheld = problem(
        harness
            .create_action_set(&api(OpenApiDocument::Url(admin_url), None))
            .await
            .unwrap_err(),
    );
    assert_eq!(withheld.kind(), Some(ProblemType::InvalidActionSet));
    assert_eq!(
        withheld.detail,
        "API dax: its OpenAPI document, read from an internal address, cannot be read as an \
         API; the answer of an internal server is not quoted"
    );
    assert!(!withheld.detail.contains("hunter2"));

    // A policy for an operation the API does not have.
    let mut unknown = api(
        OpenApiDocument::Document(document("https://api.example.com/v1")),
        None,
    );
    unknown.apis[0]
        .policies
        .insert("dropMeasure".into(), ActionPolicy::default());
    let refused = problem(harness.create_action_set(&unknown).await.unwrap_err());
    assert_eq!(
        refused.detail,
        "API dax: a policy names \"dropMeasure\", which it does not have"
    );
    open.stop().await;
}

/// An MCP server with one action, which records the claims of the JWT of each request. It does
/// not check the signature: the tenant's key is the server's own, and its JWKS is another
/// workstream's endpoint.
#[derive(Debug, Clone, Default)]
struct Signed {
    claims: Arc<Mutex<Vec<Value>>>,
}

impl ServerHandler for Signed {
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
}

async fn record(State(signed): State<Signed>, request: Request, next: Next) -> Response {
    let claims = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .and_then(|token| token.split('.').nth(1))
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
        .and_then(|payload| serde_json::from_slice::<Value>(&payload).ok());
    match claims {
        Some(claims) => signed.claims.lock().unwrap().push(claims),
        None => return (StatusCode::UNAUTHORIZED, "no JWT").into_response(),
    }
    next.run(request).await
}

async fn serve_signed(signed: Signed) -> String {
    let sessions = signed.clone();
    let service = StreamableHttpService::new(
        move || Ok(sessions.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default().with_sse_keep_alive(None),
    );
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(signed, record));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{address}/mcp")
}

fn signed_action_set(url: &str) -> CreateActionSet {
    CreateActionSet {
        mcp: vec![McpServer {
            name: "crm".into(),
            url: Some(url.to_owned()),
            auth: Some(Auth::Rewind {}),
            ..McpServer::default()
        }],
        ..CreateActionSet::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_signed_server_is_listed_with_the_reduced_claims() {
    let signed = Signed::default();
    let url = serve_signed(signed.clone()).await;
    let server = TestServer::in_memory(&[
        "--allow-internal-addresses",
        "127.0.0.1",
        "--public-url",
        "https://rewind.example",
    ])
    .await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let registered = harness
        .create_action_set(&signed_action_set(&url))
        .await
        .unwrap();
    assert_eq!(registered.actions[0].name, "crm.whoami");
    let claims = signed.claims.lock().unwrap().clone();
    assert!(claims.len() >= 2, "initialize and tools/list: {claims:?}");
    for claims in &claims {
        assert_eq!(claims["iss"], "https://rewind.example/organizations/acme");
        assert_eq!(claims["aud"], url.as_str());
        assert_eq!(claims["htu"], url.as_str());
        assert!(claims.get("sub").is_none(), "a listing acts for no one");
    }
    server.stop().await;

    // Without a public URL, no one could fetch the keys: the server does not sign.
    let unsigned = TestServer::in_memory(&["--allow-internal-addresses", "127.0.0.1"]).await;
    let harness = unsigned.key(BuiltInRole::Harness, &[]).await;
    let refused = problem(
        harness
            .create_action_set(&signed_action_set(&url))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::ActionSetUnavailable));
    assert!(
        refused.detail.starts_with(
            "MCP server crm checks Rewind's signature, and this server has no public URL"
        ),
        "{}",
        refused.detail
    );
    unsigned.stop().await;
}
