//! `ApiDispatcher` against a fake API on 127.0.0.1 (`archi/04-tools.md`, "API actions" and "Signed
//! calls"): each answer of the table, the attached secret in its header, signed requests
//! checked as a customer's server checks them (the JWKS built from the public key, `aud`,
//! `exp`, `htm`, `htu`, `body`, the labels), redirects within the origin signed again, the
//! egress checks of a deployment, and OpenAPI documents read from their URL.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use rewind_core::{
    Action, ActionAuth, ActionFailure, ActionFailureKind, Api, RunId, SecretPlacement, Seq,
    Timestamp,
};
use rewind_engine::adapters::MemorySecrets;
use rewind_engine::ports::{
    ActionDispatcher, Call, CallAuth, CallClaims, Dispatched, NamespaceName, ParkKind, RunRef,
    Secret, SecretStore, SigningKey, TaskToken, TenantId, TraceContext,
};
use rewind_openapi::{ApiConfig, ApiDispatcher, RETRY_AFTER_DATA, ReadOptions, fetch, read};
use rewind_outbound::{ActionEgress, InternalAllowList};
use serde_json::{Value, json};

const ISSUER: &str = "https://rewind.example/organizations/acme";

/// What the fake API saw of one request.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    uri: String,
    headers: HeaderMap,
    body: Bytes,
    claims: Option<Value>,
}

/// How the fake API checks who calls.
#[derive(Debug, Clone)]
enum Check {
    Nothing,
    Key(&'static str),
    Signed { jwks: Value },
}

#[derive(Debug, Clone)]
struct Fake {
    check: Check,
    base: Arc<Mutex<String>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

/// Serves the fake API; its base URL `http://127.0.0.1:<port>/v1`.
async fn serve(check: Check) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let fake = Fake {
        check,
        base: Arc::default(),
        seen: Arc::default(),
    };
    let router = Router::new().fallback(handle).with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    let base = format!("http://{address}/v1");
    *fake.base.lock().unwrap() = base.clone();
    (base, fake.seen)
}

async fn handle(State(fake): State<Fake>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    let host = parts.headers["host"].to_str().unwrap().to_owned();
    let url = format!("http://{host}{}", parts.uri);
    let claims = match &fake.check {
        Check::Nothing => None,
        Check::Key(key) => {
            if parts.headers.get("x-api-key").and_then(|v| v.to_str().ok()) != Some(key) {
                return (StatusCode::UNAUTHORIZED, "bad key").into_response();
            }
            None
        }
        Check::Signed { jwks } => {
            let token = parts
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap_or_default();
            let audience = fake.base.lock().unwrap().clone();
            let expected = rewind_outbound::Expected {
                issuer: ISSUER,
                audience: &audience,
                method: parts.method.as_str(),
                url: &url,
                body: &body,
            };
            match rewind_outbound::verify(token, jwks, &expected) {
                Ok(claims) => {
                    // An organization with several environments checks its own label.
                    if claims["labels"]["env"] != "prod" && claims.get("sub").is_some() {
                        return (StatusCode::UNAUTHORIZED, "not prod").into_response();
                    }
                    Some(claims)
                }
                Err(reason) => {
                    return (
                        StatusCode::UNAUTHORIZED,
                        [("www-authenticate", "Bearer error=\"invalid_token\"")],
                        reason,
                    )
                        .into_response();
                }
            }
        }
    };
    fake.seen.lock().unwrap().push(Seen {
        method: parts.method.to_string(),
        uri: parts.uri.to_string(),
        headers: parts.headers.clone(),
        body: body.clone(),
        claims,
    });
    let path = parts.uri.path().strip_prefix("/v1").unwrap_or_default();
    let json = |status: StatusCode, value: Value| {
        (
            status,
            [("content-type", "application/json")],
            value.to_string(),
        )
            .into_response()
    };
    match path {
        "/measures" => json(
            StatusCode::CREATED,
            json!({ "created": serde_json::from_slice::<Value>(&body).unwrap() }),
        ),
        "/text" => (StatusCode::OK, [("content-type", "text/plain")], "hello").into_response(),
        "/problem" => (
            StatusCode::NOT_FOUND,
            [("content-type", "application/problem+json")],
            json!({ "type": "about:blank", "title": "Not found", "detail": "no measure m-9" })
                .to_string(),
        )
            .into_response(),
        "/plain-error" => (StatusCode::BAD_REQUEST, "the limit is too high").into_response(),
        "/limited" => (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "1")]).into_response(),
        "/limited-long" => {
            (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "3600")]).into_response()
        }
        "/down" => (StatusCode::SERVICE_UNAVAILABLE, "maintenance").into_response(),
        "/slow" => StatusCode::REQUEST_TIMEOUT.into_response(),
        "/unauthorized" => (
            StatusCode::UNAUTHORIZED,
            [("www-authenticate", "Bearer realm=\"fake\"")],
        )
            .into_response(),
        "/moved" => (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "/v1/measures/m-1?from=moved")],
        )
            .into_response(),
        "/elsewhere" => (
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "http://127.0.0.2:9/v1/text")],
        )
            .into_response(),
        "/big" => json(StatusCode::OK, json!({ "data": "x".repeat(4096) })),
        "/sleepy" => {
            tokio::time::sleep(Duration::from_secs(5)).await;
            StatusCode::OK.into_response()
        }
        "/search" => json(StatusCode::OK, json!({ "query": parts.uri.query() })),
        "/openapi.json" => json(StatusCode::OK, document()),
        "/huge.json" => (
            StatusCode::OK,
            [("content-type", "application/json")],
            Body::from(vec![b' '; 2 * 1024 * 1024 + 1]),
        )
            .into_response(),
        "/not-json" => (StatusCode::OK, "<html>admin console</html>").into_response(),
        other if other.starts_with("/measures/") && parts.method == "DELETE" => {
            StatusCode::NO_CONTENT.into_response()
        }
        other if other.starts_with("/measures/") => json(
            StatusCode::OK,
            json!({ "id": other.trim_start_matches("/measures/") }),
        ),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The fake API's OpenAPI document.
fn document() -> Value {
    let ok = json!({ "200": { "description": "ok" } });
    let get = |id: &str| json!({ "get": { "operationId": id, "responses": ok } });
    json!({
        "openapi": "3.1.0",
        "info": { "title": "fake", "version": "1" },
        "servers": [{ "url": "/v1" }],
        "paths": {
            "/measures": { "post": {
                "operationId": "createMeasure",
                "parameters": [{ "name": "Idempotency-Key", "in": "header", "schema": {} }],
                "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object" } } } },
                "responses": { "201": { "description": "created" } }
            } },
            "/measures/{id}": {
                "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                "get": { "operationId": "getMeasure", "responses": ok },
                "delete": { "operationId": "deleteMeasure", "responses": { "204": { "description": "gone" } } }
            },
            "/search": { "get": {
                "operationId": "search",
                "parameters": [
                    { "name": "tag", "in": "query", "schema": { "type": "array", "items": { "type": "string" } } },
                    { "name": "limit", "in": "query", "schema": { "type": "integer" } },
                    { "name": "X-Source", "in": "header", "schema": { "type": "string" } }
                ],
                "responses": ok
            } },
            "/text": get("text"),
            "/problem": get("problem"),
            "/plain-error": get("plainError"),
            "/limited": get("limited"),
            "/limited-long": get("limitedLong"),
            "/down": get("down"),
            "/slow": get("slow"),
            "/unauthorized": get("unauthorized"),
            "/moved": get("moved"),
            "/elsewhere": get("elsewhere"),
            "/big": get("big"),
            "/sleepy": get("sleepy")
        }
    })
}

/// The fake API at `base`, read from its document, with `auth`.
fn api(base: &str, auth: ActionAuth) -> (Api, BTreeMap<String, Action>) {
    let mut options = ReadOptions::new("fake");
    options.base_url = Some(base.to_owned());
    options.allow_http = true;
    options.auth = Some(auth);
    let read = read(&document(), &options).unwrap();
    let actions = read
        .actions
        .into_iter()
        .map(|action| (action.name.trim_start_matches("fake.").to_owned(), action))
        .collect();
    (read.api, actions)
}

fn call(api: &Api, action: &Action, args: Value) -> Call {
    let run = RunRef::new(TenantId::new("acme").unwrap(), RunId::from_u128(42));
    Call {
        idempotency_key: format!("{}/7", run.id),
        run,
        namespace: NamespaceName::new("alice").unwrap(),
        labels: BTreeMap::from([("env".into(), "prod".into())]),
        seq: Seq(7),
        action: action.clone(),
        server: None,
        api: Some(api.clone()),
        args,
        delivery: 1,
        schema_change_approved: false,
        token: TaskToken("token".into()),
        trace_context: Some(TraceContext {
            traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
            tracestate: Some("vendor=1".into()),
        }),
    }
}

fn signed(key: &SigningKey, call: &Call) -> CallAuth {
    CallAuth::Signed {
        key: key.clone(),
        claims: CallClaims {
            issuer: ISSUER.into(),
            subject: call.namespace.clone(),
            labels: call.labels.clone(),
            run: call.run.id,
            action: call.action.name.clone(),
            idempotency_key: call.idempotency_key.clone(),
            seq: call.seq,
            delivery: call.delivery,
        },
    }
}

async fn signing_key(tenant: &str) -> SigningKey {
    MemorySecrets::new()
        .signing_key(&TenantId::new(tenant).unwrap(), Timestamp(0))
        .await
        .unwrap()
}

fn open() -> ApiDispatcher {
    ApiDispatcher::new(ApiConfig {
        egress: ActionEgress::open(),
        max_response_bytes: 1024,
        ..ApiConfig::default()
    })
}

fn result(dispatched: Dispatched) -> Value {
    match dispatched {
        Dispatched::Result(value) => value,
        other => panic!("expected a result, got {other:?}"),
    }
}

fn failed(dispatched: Dispatched) -> ActionFailure {
    match dispatched {
        Dispatched::Failed(failure) => failure,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn transient(dispatched: Dispatched) -> ActionFailure {
    match dispatched {
        Dispatched::Transient(failure) => failure,
        other => panic!("expected a transient failure, got {other:?}"),
    }
}

#[tokio::test]
async fn signed_calls_check_against_the_jwks() {
    let key = signing_key("acme").await;
    let jwks = json!({ "keys": [rewind_outbound::jwk(&key.public.kid, &key.public.key)] });
    let (base, seen) = serve(Check::Signed { jwks }).await;
    let (api, actions) = api(&base, ActionAuth::Rewind);
    let dispatcher = open();

    let get = call(&api, &actions["getMeasure"], json!({ "id": "m 1" }));
    let run = get.run.id.to_string();
    assert_eq!(
        result(dispatcher.dispatch(get.clone(), signed(&key, &get)).await),
        json!({ "id": "m%201" })
    );
    let request = seen.lock().unwrap().last().cloned().unwrap();
    let claims = request.claims.unwrap();
    assert_eq!(claims["sub"], "alice");
    assert_eq!(claims["labels"], json!({ "env": "prod" }));
    assert_eq!(claims["run"], run.as_str());
    assert_eq!(claims["action"], "fake.getMeasure");
    assert_eq!(
        claims["tool"], claims["action"],
        "the old name, for verifiers on SDK 0.3.0"
    );
    assert_eq!(claims["idempotency_key"], format!("{run}/7"));
    assert_eq!(claims["jti"], format!("{run}/7/1"));
    assert_eq!(claims["aud"], base.as_str());
    assert_eq!(claims["htm"], "GET");
    assert_eq!(claims["htu"], format!("{base}/measures/m%201"));
    assert!(claims.get("body").is_none());
    assert_eq!(
        request.headers["traceparent"],
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
    );
    assert_eq!(request.headers["tracestate"], "vendor=1");
    assert!(request.headers.get("idempotency-key").is_none());

    // A body: its digest is signed; the operation declares Idempotency-Key.
    let create = call(
        &api,
        &actions["createMeasure"],
        json!({ "body": { "name": "w" } }),
    );
    assert_eq!(
        result(
            dispatcher
                .dispatch(create.clone(), signed(&key, &create))
                .await
        ),
        json!({ "created": { "name": "w" } })
    );
    let request = seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(request.body, "{\"name\":\"w\"}");
    assert_eq!(request.headers["idempotency-key"], format!("{run}/7"));
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(
        request.claims.unwrap()["body"],
        rewind_outbound::body_digest(b"{\"name\":\"w\"}")
    );

    // A redirect within the origin: followed, its request signed for its own URL.
    let moved = call(&api, &actions["moved"], json!({}));
    assert_eq!(
        result(
            dispatcher
                .dispatch(moved.clone(), signed(&key, &moved))
                .await
        ),
        json!({ "id": "m-1" })
    );
    let requests = seen.lock().unwrap().clone();
    let [.., first, second] = requests.as_slice() else {
        panic!("two requests");
    };
    assert_eq!(first.uri, "/v1/moved");
    assert_eq!(second.uri, "/v1/measures/m-1?from=moved");
    assert_eq!(
        second.claims.as_ref().unwrap()["htu"],
        format!("{base}/measures/m-1?from=moved")
    );

    // Another tenant's key, or the labels of another environment: refused, parked.
    let other = signing_key("other").await;
    let Dispatched::Park(parked) = dispatcher.dispatch(get.clone(), signed(&other, &get)).await
    else {
        panic!("a refused signature parks the call");
    };
    assert_eq!(parked.kind, ParkKind::CredentialsRefused);
    assert_eq!(
        parked.message,
        "fake refused the credentials (HTTP 401) of fake.getMeasure: Bearer \
         error=\"invalid_token\""
    );
    let mut dev = get.clone();
    dev.labels = BTreeMap::from([("env".into(), "dev".into())]);
    let Dispatched::Park(_) = dispatcher.dispatch(dev.clone(), signed(&key, &dev)).await else {
        panic!("a server checks its labels");
    };
}

#[tokio::test]
async fn a_secret_goes_in_its_header() {
    let (base, seen) = serve(Check::Key("k-1")).await;
    let placement = SecretPlacement::header("X-Api-Key");
    let (api, actions) = api(&base, ActionAuth::Secret(placement.clone()));
    let dispatcher = open();
    let get = call(&api, &actions["getMeasure"], json!({ "id": "m-2" }));
    let auth = |value: &str| CallAuth::Secret {
        value: Secret::new(value),
        placement: placement.clone(),
    };
    assert_eq!(
        result(dispatcher.dispatch(get.clone(), auth("k-1")).await),
        json!({ "id": "m-2" })
    );
    assert_eq!(seen.lock().unwrap()[0].headers["x-api-key"], "k-1");
    let Dispatched::Park(parked) = dispatcher.dispatch(get, auth("k-2")).await else {
        panic!("a refused secret parks the call");
    };
    assert_eq!(
        parked.message,
        "fake refused the credentials (HTTP 401) of fake.getMeasure"
    );
    assert!(!parked.message.contains("k-2"));
}

#[tokio::test]
async fn each_answer_has_its_outcome() {
    let (base, seen) = serve(Check::Nothing).await;
    let (api, actions) = api(&base, ActionAuth::None);
    let dispatcher = open();
    let send = |name: &str, args: Value| {
        dispatcher.dispatch(call(&api, &actions[name], args), CallAuth::None)
    };

    // 2xx.
    assert_eq!(
        result(send("text", json!({})).await),
        json!({ "contentType": "text/plain", "text": "hello" })
    );
    assert_eq!(
        result(send("deleteMeasure", json!({ "id": "m-3" })).await),
        Value::Null
    );
    assert_eq!(
        result(
            send(
                "search",
                json!({ "tag": ["a", "b c"], "limit": 5, "X-Source": "script" })
            )
            .await
        ),
        json!({ "query": "limit=5&tag=a&tag=b+c" })
    );
    assert_eq!(
        seen.lock().unwrap().last().unwrap().headers["x-source"],
        "script"
    );
    let big = failed(send("big", json!({})).await);
    assert_eq!(big.kind, ActionFailureKind::Error);
    assert_eq!(
        big.message,
        "the answer of fake.big is larger than 1024 bytes"
    );

    // 401 parks.
    let Dispatched::Park(parked) = send("unauthorized", json!({})).await else {
        panic!("401 parks");
    };
    assert_eq!(parked.kind, ParkKind::CredentialsRefused);

    // 408, 429 and 5xx are transient; a Retry-After within reach is waited out.
    let started = Instant::now();
    let limited = transient(send("limited", json!({})).await);
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(limited.message, "fake answered HTTP 429 to fake.limited");
    assert_eq!(limited.data, Some(json!({ "status": 429 })));
    let started = Instant::now();
    let long = transient(send("limitedLong", json!({})).await);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(long.data.unwrap()[RETRY_AFTER_DATA], 3600);
    let down = transient(send("down", json!({})).await);
    assert_eq!(
        down.message,
        "fake answered HTTP 503 to fake.down: maintenance"
    );
    transient(send("slow", json!({})).await);

    // Another status fails with the problem's title and detail, or the start of the body.
    let problem = failed(send("problem", json!({})).await);
    assert_eq!(problem.kind, ActionFailureKind::Error);
    assert_eq!(
        problem.message,
        "fake answered HTTP 404 to fake.problem: Not found: no measure m-9"
    );
    assert_eq!(
        problem.data,
        Some(json!({ "status": 404, "title": "Not found", "detail": "no measure m-9" }))
    );
    let plain = failed(send("plainError", json!({})).await);
    assert_eq!(
        plain.data,
        Some(json!({ "status": 400, "body": "the limit is too high" }))
    );

    // A redirect to another origin is refused, for good.
    let elsewhere = failed(send("elsewhere", json!({})).await);
    assert_eq!(elsewhere.kind, ActionFailureKind::Unavailable);
    assert!(
        elsewhere
            .message
            .contains("redirects fake.elsewhere to another origin"),
        "{}",
        elsewhere.message
    );

    // Past the execution timeout: transient.
    let mut sleepy = call(&api, &actions["sleepy"], json!({}));
    sleepy.action.policy.execution_timeout = Some(Duration::from_millis(300));
    let timed_out = transient(dispatcher.dispatch(sleepy, CallAuth::None).await);
    assert_eq!(timed_out.kind, ActionFailureKind::Timeout);
    assert_eq!(timed_out.message, "fake.sleepy did not answer within 300ms");

    // Nothing listening: transient.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let (gone, gone_actions) = self::api(&format!("http://127.0.0.1:{port}/v1"), ActionAuth::None);
    let unreachable = transient(
        dispatcher
            .dispatch(
                call(&gone, &gone_actions["text"], json!({})),
                CallAuth::None,
            )
            .await,
    );
    assert_eq!(unreachable.kind, ActionFailureKind::Unavailable);
    assert!(
        unreachable
            .message
            .starts_with("fake could not be reached for fake.text"),
        "{}",
        unreachable.message
    );
}

#[tokio::test]
async fn a_deployed_server_refuses_internal_apis_and_quotes_nothing_of_allowed_ones() {
    let (base, seen) = serve(Check::Nothing).await;
    let (api, actions) = api(&base, ActionAuth::None);
    let deployed = ApiDispatcher::new(ApiConfig::default());
    let refused = failed(
        deployed
            .dispatch(call(&api, &actions["text"], json!({})), CallAuth::None)
            .await,
    );
    assert_eq!(refused.kind, ActionFailureKind::Unavailable);
    assert_eq!(
        refused.message,
        "fake cannot be reached: 127.0.0.1 is a loopback address: MCP servers and APIs are not \
         reached there unless the operator allows it"
    );
    assert!(seen.lock().unwrap().is_empty(), "nothing was sent");

    let allowed = ApiDispatcher::new(ApiConfig {
        egress: ActionEgress::checked(InternalAllowList::parse(&["127.0.0.1"]).unwrap()),
        ..ApiConfig::default()
    });
    assert_eq!(
        result(
            allowed
                .dispatch(call(&api, &actions["text"], json!({})), CallAuth::None)
                .await
        ),
        json!({ "contentType": "text/plain", "text": "hello" })
    );
    let problem = failed(
        allowed
            .dispatch(call(&api, &actions["problem"], json!({})), CallAuth::None)
            .await,
    );
    assert_eq!(
        problem.message,
        "fake answered HTTP 404 to fake.problem; the answer of an internal server is not quoted"
    );
    assert_eq!(problem.data, Some(json!({ "status": 404 })));
}

#[tokio::test]
async fn documents_are_read_from_their_url_through_the_egress_checks() {
    let (base, _) = serve(Check::Nothing).await;
    let fetched = fetch(&format!("{base}/openapi.json"), &ActionEgress::open())
        .await
        .unwrap();
    assert!(!fetched.internal);
    assert_eq!(fetched.url.as_str(), format!("{base}/openapi.json"));
    // Its relative server resolves against where it was read.
    let mut options = ReadOptions::new("fake");
    options.document_url = Some(fetched.url.clone());
    options.allow_http = true;
    assert_eq!(
        read(&fetched.document, &options).unwrap().api.base_url,
        base
    );

    let refused = fetch(&format!("{base}/openapi.json"), &ActionEgress::default())
        .await
        .unwrap_err();
    assert!(
        refused.0.ends_with(
            "cannot be read: 127.0.0.1 is a loopback address: MCP servers and APIs are not \
             reached there unless the operator allows it"
        ),
        "{refused}"
    );
    let huge = fetch(&format!("{base}/huge.json"), &ActionEgress::open())
        .await
        .unwrap_err();
    assert!(huge.0.ends_with("is larger than 2 MiB"), "{huge}");
    let internal = ActionEgress::checked(InternalAllowList::parse(&["127.0.0.1"]).unwrap());
    let console = fetch(&format!("{base}/not-json"), &internal)
        .await
        .unwrap_err();
    assert!(
        console
            .0
            .ends_with("is not JSON; the answer of an internal server is not quoted"),
        "{console}"
    );
    assert!(!console.0.contains("admin"));
    let missing = fetch(&format!("{base}/gone.json"), &ActionEgress::open())
        .await
        .unwrap_err();
    assert!(missing.0.ends_with("answered HTTP 404"), "{missing}");
}
