//! `McpDispatcher` against MCP servers started by the tests: the fixture of `fixture.rs`, over
//! streamable HTTP and over stdio, and a reference server when one is given.
//!
//! A custom harness (libtest-mimic): with `REWIND_MCP_FIXTURE=stdio`, this binary is the stdio
//! server the stdio tests start. `REWIND_MCP_REFERENCE_SERVER` names the command line of a
//! reference MCP server that has the `echo` tool of `@modelcontextprotocol/server-everything`,
//! such as `npx -y @modelcontextprotocol/server-everything`; without it, that test is ignored.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod egress;
mod fixture;

use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant};

use fixture::{Auth, Fixture};
use libtest_mimic::{Arguments, Trial};
use rewind_core::{
    ActionSet, McpServer, McpTransport, RunId, SecretPlacement, Seq, Timestamp, Tool, ToolAuth,
    ToolFailure, ToolFailureKind, ToolRoute,
};
use rewind_engine::adapters::MemorySecrets;
use rewind_engine::ports::{
    CallAuth, CallClaims, Dispatched, NamespaceName, ParkKind, RunRef, Secret, SecretStore,
    SigningKey, TaskToken, TenantId, ToolCall, ToolDispatcher, TraceContext,
};
use rewind_mcp::{IDEMPOTENCY_KEY_META, ListingAuth, McpConfig, McpDispatcher, McpError};
use rewind_outbound::ToolEgress;
use serde_json::{Value, json};

fn main() -> std::process::ExitCode {
    if std::env::var("REWIND_MCP_FIXTURE").as_deref() == Ok("stdio") {
        fixture::serve_stdio();
        return std::process::ExitCode::SUCCESS;
    }
    let mut trials = vec![
        test(
            "discovery_pins_the_server_tools",
            discovery_pins_the_server_tools(),
        ),
        test(
            "a_tool_list_without_end_is_refused",
            a_tool_list_without_end_is_refused(),
        ),
        test(
            "content_and_structured_results",
            content_and_structured_results(),
        ),
        test(
            "idempotency_key_and_trace_context_in_meta",
            idempotency_key_and_trace_context_in_meta(),
        ),
        test("errors_are_mapped", errors_are_mapped()),
        test(
            "headers_reach_the_server_and_a_401_parks",
            headers_reach_the_server_and_a_401_parks(false),
        ),
        test(
            "a_401_with_a_challenge_parks_too",
            headers_reach_the_server_and_a_401_parks(true),
        ),
        test(
            "a_secret_goes_into_its_header",
            a_secret_goes_into_its_header(),
        ),
        test(
            "signed_sessions_sign_every_request",
            signed_sessions_sign_every_request(),
        ),
        test(
            "a_stdio_server_gets_its_secret_in_its_variable",
            a_stdio_server_gets_its_secret_in_its_variable(),
        ),
        test(
            "the_execution_timeout_cancels_the_call",
            the_execution_timeout_cancels_the_call(),
        ),
        test(
            "a_schema_change_parks_the_call",
            a_schema_change_parks_the_call(),
        ),
        test(
            "connections_are_shared_and_replaced",
            connections_are_shared_and_replaced(),
        ),
        test(
            "an_unreachable_server_is_transient",
            an_unreachable_server_is_transient(),
        ),
        test(
            "a_stdio_server_gets_its_own_environment_only",
            a_stdio_server_gets_its_own_environment_only(),
        ),
        test(
            "a_missing_stdio_command_fails_for_good",
            a_missing_stdio_command_fails_for_good(),
        ),
        test(
            "egress::a_local_server_is_refused_by_a_deployed_server",
            egress::a_local_server_is_refused_by_a_deployed_server(),
        ),
        test(
            "egress::an_allowed_internal_server_is_reached",
            egress::an_allowed_internal_server_is_reached(),
        ),
        test(
            "egress::an_internal_server_s_answers_are_not_quoted",
            egress::an_internal_server_s_answers_are_not_quoted(),
        ),
        test(
            "egress::redirects_go_nowhere_else",
            egress::redirects_go_nowhere_else(),
        ),
    ];
    let reference = std::env::var("REWIND_MCP_REFERENCE_SERVER").ok();
    trials.push(
        test("reference_server", reference_server(reference.clone()))
            .with_ignored_flag(reference.is_none()),
    );
    libtest_mimic::run(&Arguments::from_args(), trials).exit_code()
}

fn test<F>(name: &str, body: F) -> Trial
where
    F: Future<Output = ()> + Send + 'static,
{
    Trial::test(name, move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(body);
        Ok(())
    })
}

// -------------------------------------------------------------------------------------------
// Helpers

/// The configuration of the embedded server: the fixture servers listen on 127.0.0.1, which a
/// deployment's checks refuse (`egress.rs`). Stdio servers are left to the default checks,
/// which do not concern them.
fn open_config() -> McpConfig {
    McpConfig {
        egress: ToolEgress::open(),
        ..McpConfig::default()
    }
}

fn open() -> McpDispatcher {
    McpDispatcher::new(open_config())
}

fn http_server(url: &str) -> McpServer {
    McpServer::new(
        "fx",
        McpTransport::Http {
            url: url.to_owned(),
        },
    )
}

fn stdio_server(extra_env: &[(&str, &str)]) -> McpServer {
    let mut env: BTreeMap<String, String> =
        BTreeMap::from([("REWIND_MCP_FIXTURE".into(), "stdio".into())]);
    for (name, value) in extra_env {
        env.insert((*name).to_owned(), (*value).to_owned());
    }
    McpServer::new(
        "fx",
        McpTransport::Stdio {
            command: std::env::current_exe().unwrap().display().to_string(),
            args: Vec::new(),
            env,
        },
    )
}

fn tenant() -> TenantId {
    TenantId::local()
}

/// The tools of a server as pinned, by local name.
async fn pin(
    dispatcher: &McpDispatcher,
    server: &McpServer,
    auth: ListingAuth,
) -> BTreeMap<String, Tool> {
    dispatcher
        .list_tools(&tenant(), server, auth)
        .await
        .unwrap()
        .into_iter()
        .map(|tool| (tool.local_name().to_owned(), tool))
        .collect()
}

fn call(server: &McpServer, tool: &Tool, args: Value) -> ToolCall {
    let run = RunRef::new(tenant(), RunId::from_u128(42));
    ToolCall {
        idempotency_key: format!("{}/3", run.id),
        run,
        namespace: NamespaceName::new("alice").unwrap(),
        labels: BTreeMap::new(),
        seq: Seq(3),
        tool: tool.clone(),
        server: Some(server.clone()),
        api: None,
        args,
        attempt: 1,
        schema_change_approved: false,
        token: TaskToken("token".into()),
        trace_context: None,
    }
}

/// A server whose secret goes in `Authorization: Bearer {secret}`.
fn bearer_server(url: &str) -> McpServer {
    let mut server = http_server(url);
    server.auth = ToolAuth::Secret(SecretPlacement::bearer());
    server
}

/// The secret a run attached for a `bearer_server`.
fn bearer(token: &str) -> CallAuth {
    CallAuth::Secret {
        value: Secret::new(token),
        placement: SecretPlacement::bearer(),
    }
}

/// The tenant's signing key, as the memory store makes it.
async fn signing_key() -> SigningKey {
    MemorySecrets::new()
        .signing_key(&tenant(), Timestamp(0))
        .await
        .unwrap()
}

const ISSUER: &str = "https://rewind.example/organizations/local";

/// The claims the engine resolves for `call`.
fn claims(call: &ToolCall) -> CallClaims {
    CallClaims {
        issuer: ISSUER.into(),
        subject: call.namespace.clone(),
        labels: call.labels.clone(),
        run: call.run.id,
        tool: call.tool.name.clone(),
        idempotency_key: call.idempotency_key.clone(),
        seq: call.seq,
        attempt: call.attempt,
    }
}

fn result(dispatched: Dispatched) -> Value {
    match dispatched {
        Dispatched::Result(value) => value,
        other => panic!("expected a result, got {other:?}"),
    }
}

/// The JSON a tool returned as its text content.
fn text_json(result: &Value) -> Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

fn failed(dispatched: Dispatched) -> ToolFailure {
    match dispatched {
        Dispatched::Failed(failure) => failure,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn transient(dispatched: Dispatched) -> ToolFailure {
    match dispatched {
        Dispatched::Transient(failure) => failure,
        other => panic!("expected a transient failure, got {other:?}"),
    }
}

// -------------------------------------------------------------------------------------------
// Tests

async fn discovery_pins_the_server_tools() {
    let server = fixture::serve_http(Fixture::new(), Auth::default()).await;
    let dispatcher = open();
    let mcp = http_server(&server.url);
    let tools = dispatcher
        .list_tools(&tenant(), &mcp, ListingAuth::None)
        .await
        .unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "fx.env",
            "fx.fail",
            "fx.get_issue",
            "fx.internal",
            "fx.invalid",
            "fx.lookup",
            "fx.meta",
            "fx.slow"
        ]
    );
    let issue = tools
        .iter()
        .find(|tool| tool.name == "fx.get_issue")
        .unwrap();
    assert_eq!(issue.description.as_deref(), Some("Get one issue."));
    assert_eq!(issue.input_schema["required"], json!(["number"]));
    assert!(issue.output_schema.is_some());
    assert_eq!(
        issue.policy.execution_timeout,
        Some(Duration::from_secs(300))
    );
    // What discovery returns is an action set.
    let action_set = ActionSet::new([mcp], tools).unwrap();
    assert_eq!(action_set.tools().len(), 8);
}

async fn a_tool_list_without_end_is_refused() {
    let fixture = Fixture::new();
    fixture.state().endless_list = true;
    let server = fixture::serve_http(fixture.clone(), Auth::default()).await;
    let mcp = http_server(&server.url);
    // Listing went on until the timeout, the connection's list locked meanwhile.
    let dispatcher = McpDispatcher::new(McpConfig {
        connect_timeout: Duration::from_secs(20),
        ..open_config()
    });
    let started = Instant::now();
    let error = dispatcher
        .list_tools(&tenant(), &mcp, ListingAuth::None)
        .await
        .unwrap_err();
    assert!(
        matches!(&error, McpError::Refused { message, .. } if message.contains("more than 100 pages")),
        "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(fixture.state().pages, 100);

    // A call checks its tool against the list: it fails at once, for good.
    let tool = Tool::new(
        "fx.t0",
        ToolRoute::Mcp {
            server: "fx".into(),
        },
        json!({ "type": "object" }),
    );
    let failure = failed(
        dispatcher
            .dispatch(call(&mcp, &tool, json!({})), CallAuth::None)
            .await,
    );
    assert_eq!(failure.kind, ToolFailureKind::Unavailable);
    assert!(
        failure.message.contains("Rewind reads no further"),
        "{failure:?}"
    );
}

async fn content_and_structured_results() {
    let server = fixture::serve_http(Fixture::new(), Auth::default()).await;
    let dispatcher = open();
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, ListingAuth::None).await;

    let lookup = result(
        dispatcher
            .dispatch(
                call(&mcp, &tools["lookup"], json!({ "id": "c-7" })),
                CallAuth::None,
            )
            .await,
    );
    assert_eq!(lookup["content"][0]["type"], "text");
    assert_eq!(text_json(&lookup), json!({ "id": "c-7", "name": "Ada" }));
    assert!(lookup.get("structuredContent").is_none());

    let issue = result(
        dispatcher
            .dispatch(
                call(&mcp, &tools["get_issue"], json!({ "number": 4 })),
                CallAuth::None,
            )
            .await,
    );
    assert_eq!(issue, json!({ "number": 4, "title": "Crash" }));
}

async fn idempotency_key_and_trace_context_in_meta() {
    let fixture = Fixture::new();
    let server = fixture::serve_http(fixture.clone(), Auth::default()).await;
    let dispatcher = open();
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, ListingAuth::None).await;
    let mut meta_call = call(&mcp, &tools["meta"], json!({}));
    meta_call.attempt = 2;
    meta_call.trace_context = Some(TraceContext {
        traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
        tracestate: Some("vendor=1".into()),
    });
    let expected_key = meta_call.idempotency_key.clone();
    result(dispatcher.dispatch(meta_call, CallAuth::None).await);
    let recorded = fixture.state().calls.last().cloned().unwrap();
    assert_eq!(recorded.name, "meta");
    assert_eq!(recorded.arguments, json!({}));
    assert_eq!(recorded.meta[IDEMPOTENCY_KEY_META], expected_key.as_str());
    assert_eq!(
        recorded.meta["traceparent"],
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
    );
    assert_eq!(recorded.meta["tracestate"], "vendor=1");
    assert!(
        expected_key.ends_with("/3"),
        "the same key for every attempt: {{run_id}}/{{seq}}"
    );
}

async fn errors_are_mapped() {
    let server = fixture::serve_http(Fixture::new(), Auth::default()).await;
    let dispatcher = open();
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, ListingAuth::None).await;

    let fail = failed(
        dispatcher
            .dispatch(call(&mcp, &tools["fail"], json!({})), CallAuth::None)
            .await,
    );
    assert_eq!(fail.kind, ToolFailureKind::Error);
    assert_eq!(fail.message, "no such customer");

    let invalid = failed(
        dispatcher
            .dispatch(call(&mcp, &tools["invalid"], json!({})), CallAuth::None)
            .await,
    );
    assert_eq!(invalid.kind, ToolFailureKind::Error);
    assert_eq!(invalid.message, "fx answered with an error: bad arguments");
    assert_eq!(invalid.data, Some(json!({ "field": "x" })));

    let internal = transient(
        dispatcher
            .dispatch(call(&mcp, &tools["internal"], json!({})), CallAuth::None)
            .await,
    );
    assert_eq!(internal.kind, ToolFailureKind::Unavailable);

    let not_an_object = failed(
        dispatcher
            .dispatch(call(&mcp, &tools["lookup"], json!([1])), CallAuth::None)
            .await,
    );
    assert_eq!(
        not_an_object.message,
        "the arguments of fx.lookup must be an object"
    );
}

/// A server that refuses a call's credentials answers 401: the call is parked.
async fn headers_reach_the_server_and_a_401_parks(challenge: bool) {
    let auth = Auth {
        challenge,
        ..Auth::default()
    };
    *auth.token.lock().unwrap() = Some("first".into());
    let fixture = Fixture::new();
    let server = fixture::serve_http(fixture.clone(), auth.clone()).await;
    let dispatcher = open();
    let mcp = bearer_server(&server.url);

    // Without its secret, discovery is refused.
    let refused = dispatcher
        .list_tools(&tenant(), &mcp, ListingAuth::None)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, McpError::Unauthorized { .. }),
        "{refused:?}"
    );

    let tools = pin(&dispatcher, &mcp, ListingAuth::Secret(Secret::new("first"))).await;
    let lookup = call(&mcp, &tools["lookup"], json!({ "id": "c-1" }));
    result(dispatcher.dispatch(lookup.clone(), bearer("first")).await);

    // Without its secret, or with another, a call is parked.
    for auth in [CallAuth::None, bearer("second")] {
        let Dispatched::Park(parked) = dispatcher.dispatch(lookup.clone(), auth).await else {
            panic!("a call refused with a 401 is parked");
        };
        assert_eq!(parked.kind, ParkKind::CredentialsRefused);
        assert!(
            parked
                .message
                .starts_with("fx refused the credentials (HTTP 401)"),
            "{}",
            parked.message
        );
        assert!(
            !parked.message.contains("second"),
            "the message never holds the secret"
        );
    }
}

/// The attached secret goes into the header of the server's `auth`, with its format and
/// encoding, on every request of the session.
async fn a_secret_goes_into_its_header() {
    let auth = Auth::default();
    // `Basic {secret}` in base64: the fixture expects `Bearer <token>`, so the token is what
    // the encoded secret reads as.
    *auth.token.lock().unwrap() = Some("YWRhOnMzY3JldA==".into());
    let fixture = Fixture::new();
    let server = fixture::serve_http(fixture.clone(), auth).await;
    let dispatcher = open();
    let mut mcp = http_server(&server.url);
    let placement = SecretPlacement::Header {
        name: "Authorization".into(),
        format: Some("Bearer {secret}".into()),
        encoding: rewind_core::SecretEncoding::Base64,
    };
    mcp.auth = ToolAuth::Secret(placement.clone());
    let tools = pin(
        &dispatcher,
        &mcp,
        ListingAuth::Secret(Secret::new("ada:s3cret")),
    )
    .await;
    let lookup = call(&mcp, &tools["lookup"], json!({ "id": "c-2" }));
    let found = result(
        dispatcher
            .dispatch(
                lookup,
                CallAuth::Secret {
                    value: Secret::new("ada:s3cret"),
                    placement,
                },
            )
            .await,
    );
    assert_eq!(text_json(&found)["id"], "c-2");
    // A listing secret on a server that takes none is refused before anything is sent.
    let refused = dispatcher
        .list_tools(
            &tenant(),
            &http_server(&server.url),
            ListingAuth::Secret(Secret::new("x")),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&refused, McpError::Refused { message, .. } if message == "fx: its auth takes no secret"),
        "{refused:?}"
    );
}

/// A server whose `auth` is `rewind` checks a JWT on each HTTP request of the session: the
/// call's `tools/call` with the call's claims, the others with the reduced set. A key it does
/// not publish is refused (401), and the call is parked.
async fn signed_sessions_sign_every_request() {
    let key = signing_key().await;
    let signed = fixture::Signed {
        jwks: json!({ "keys": [rewind_outbound::jwk(&key.public.kid, &key.public.key)] }),
        issuer: ISSUER.into(),
        seen: Default::default(),
    };
    let fixture = Fixture::new();
    let auth = Auth {
        signed: Some(signed.clone()),
        ..Auth::default()
    };
    let server = fixture::serve_http(fixture.clone(), auth).await;
    let dispatcher = open();
    let mut mcp = http_server(&server.url);
    mcp.auth = ToolAuth::Rewind;

    // The listing at registration: the reduced set, on every request of its session.
    let tools = pin(
        &dispatcher,
        &mcp,
        ListingAuth::Signed {
            key: key.clone(),
            issuer: ISSUER.into(),
        },
    )
    .await;
    {
        let seen = signed.seen.lock().unwrap();
        let methods: Vec<Option<&str>> = seen.iter().map(|seen| seen.rpc.as_deref()).collect();
        assert!(methods.contains(&Some("initialize")), "{methods:?}");
        assert!(methods.contains(&Some("tools/list")), "{methods:?}");
        for seen in seen.iter() {
            let mut names: Vec<&str> = seen
                .claims
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            names.sort_unstable();
            let reduced = ["aud", "exp", "htm", "htu", "iat", "iss", "jti"];
            let with_body = ["aud", "body", "exp", "htm", "htu", "iat", "iss", "jti"];
            assert!(
                names == reduced || names == with_body,
                "{} {:?}: {names:?}",
                seen.http,
                seen.rpc
            );
        }
        let jtis: std::collections::BTreeSet<&str> = seen
            .iter()
            .map(|seen| seen.claims["jti"].as_str().unwrap())
            .collect();
        assert_eq!(jtis.len(), seen.len(), "one jti per request");
    }
    let sessions = fixture.state().sessions;

    // A call: its `tools/call` carries the call's claims.
    let mut lookup = call(&mcp, &tools["lookup"], json!({ "id": "c-3" }));
    lookup.labels = BTreeMap::from([("env".into(), "prod".into())]);
    lookup.attempt = 2;
    let auth = CallAuth::Signed {
        key: key.clone(),
        claims: claims(&lookup),
    };
    result(dispatcher.dispatch(lookup.clone(), auth).await);
    let called = signed
        .seen
        .lock()
        .unwrap()
        .iter()
        .rev()
        .find(|seen| seen.rpc.as_deref() == Some("tools/call"))
        .cloned()
        .unwrap();
    let run = lookup.run.id.to_string();
    assert_eq!(called.claims["sub"], "alice");
    assert_eq!(called.claims["labels"], json!({ "env": "prod" }));
    assert_eq!(called.claims["run"], run.as_str());
    assert_eq!(called.claims["tool"], "fx.lookup");
    assert_eq!(called.claims["idempotency_key"], format!("{run}/3"));
    assert_eq!(called.claims["jti"], format!("{run}/3/2"));
    assert_eq!(called.claims["aud"], server.url.as_str());
    assert_eq!(called.claims["htu"], server.url.as_str());
    assert_eq!(called.claims["htm"], "POST");
    // The schema check's listing on the call's session carries the reduced set.
    assert!(
        signed
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|seen| seen.rpc.as_deref() != Some("tools/call"))
            .all(|seen| seen.claims.get("sub").is_none())
    );
    // The call's end user has a session of their own, apart from the listing's.
    assert_eq!(fixture.state().sessions, sessions + 1);
    let mut bob = lookup.clone();
    bob.namespace = NamespaceName::new("bob").unwrap();
    let auth = CallAuth::Signed {
        key: key.clone(),
        claims: claims(&bob),
    };
    result(dispatcher.dispatch(bob, auth).await);
    assert_eq!(fixture.state().sessions, sessions + 2);

    // Another tenant's key is not in this server's JWKS: refused, parked.
    let other = MemorySecrets::new()
        .signing_key(&TenantId::new("other").unwrap(), Timestamp(0))
        .await
        .unwrap();
    let auth = CallAuth::Signed {
        key: other,
        claims: claims(&lookup),
    };
    let Dispatched::Park(parked) = dispatcher.dispatch(lookup, auth).await else {
        panic!("a refused signature parks the call");
    };
    assert_eq!(parked.kind, ParkKind::CredentialsRefused);
}

/// A stdio server gets the attached secret in the variable its `auth` names, and nothing of the
/// dispatcher's own environment.
async fn a_stdio_server_gets_its_secret_in_its_variable() {
    let dispatcher = McpDispatcher::default();
    let mut mcp = stdio_server(&[]);
    let placement = SecretPlacement::Env {
        variable: "FIXTURE_TOKEN".into(),
    };
    mcp.auth = ToolAuth::Secret(placement.clone());
    let tools = pin(&dispatcher, &mcp, ListingAuth::Secret(Secret::new("s-1"))).await;
    let env = result(
        dispatcher
            .dispatch(
                call(&mcp, &tools["env"], json!({})),
                CallAuth::Secret {
                    value: Secret::new("s-2"),
                    placement,
                },
            )
            .await,
    );
    assert_eq!(text_json(&env)["token"], "s-2");
    assert_eq!(text_json(&env)["manifest_dir"], Value::Null);
    dispatcher.close();
}

async fn the_execution_timeout_cancels_the_call() {
    let fixture = Fixture::new();
    let server = fixture::serve_http(fixture.clone(), Auth::default()).await;
    let dispatcher = open();
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, ListingAuth::None).await;
    let mut slow = tools["slow"].clone();
    slow.policy.execution_timeout = Some(Duration::from_millis(300));
    let started = Instant::now();
    let timed_out = transient(
        dispatcher
            .dispatch(call(&mcp, &slow, json!({ "ms": 10_000 })), CallAuth::None)
            .await,
    );
    assert_eq!(timed_out.kind, ToolFailureKind::Timeout);
    assert_eq!(timed_out.message, "fx.slow did not answer within 300ms");
    assert!(started.elapsed() < Duration::from_secs(5));
    // The server learns that the call was abandoned (`notifications/cancelled`).
    let deadline = Instant::now() + Duration::from_secs(5);
    while fixture.state().cancelled == 0 {
        assert!(
            Instant::now() < deadline,
            "the server was not told the call was cancelled"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn a_schema_change_parks_the_call() {
    let fixture = Fixture::new();
    let server = fixture::serve_http(fixture.clone(), Auth::default()).await;
    // The tool list is listed again before each call.
    let dispatcher = McpDispatcher::new(McpConfig {
        tools_ttl: Duration::ZERO,
        ..open_config()
    });
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, ListingAuth::None).await;
    let lookup = call(&mcp, &tools["lookup"], json!({ "id": "c-1" }));

    // A new optional property: the call still fits.
    fixture.state().tools[0] = fixture::tool(
        "lookup",
        "Find a customer by id.",
        json!({ "type": "object", "properties": { "id": { "type": "string" }, "fields": { "type": "array" } }, "required": ["id"] }),
        None,
    );
    result(dispatcher.dispatch(lookup.clone(), CallAuth::None).await);

    // A new required property: the call no longer fits, and waits for the harness.
    fixture.state().tools[0] = fixture::tool(
        "lookup",
        "Find a customer by id.",
        json!({ "type": "object", "properties": { "id": { "type": "string" }, "tenant": { "type": "string" } }, "required": ["id", "tenant"] }),
        None,
    );
    let Dispatched::Park(parked) = dispatcher.dispatch(lookup.clone(), CallAuth::None).await else {
        panic!("a call that no longer fits is parked");
    };
    assert_eq!(parked.kind, ParkKind::SchemaChanged);
    assert!(
        parked.message.starts_with(
            "fx.lookup changed on its server: the call's arguments no longer match its input schema"
        ),
        "{}",
        parked.message
    );
    let calls = fixture.state().calls.len();

    // Approved by the harness: sent against the new schema without checking it again.
    let mut approved = lookup.clone();
    approved.schema_change_approved = true;
    result(dispatcher.dispatch(approved, CallAuth::None).await);
    assert_eq!(fixture.state().calls.len(), calls + 1);

    // A changed output schema, and a removed tool.
    fixture.state().tools[1] = fixture::tool(
        "get_issue",
        "Get one issue.",
        json!({ "type": "object" }),
        Some(json!({ "type": "object" })),
    );
    let Dispatched::Park(parked) = dispatcher
        .dispatch(
            call(&mcp, &tools["get_issue"], json!({ "number": 1 })),
            CallAuth::None,
        )
        .await
    else {
        panic!("a changed output schema parks the call");
    };
    assert!(
        parked.message.contains("output schema"),
        "{}",
        parked.message
    );
    fixture.state().tools.remove(0);
    let Dispatched::Park(parked) = dispatcher.dispatch(lookup, CallAuth::None).await else {
        panic!("a removed tool parks the call");
    };
    assert_eq!(
        parked.message,
        "fx.lookup changed on its server: the server no longer lists the tool \"lookup\""
    );
}

async fn connections_are_shared_and_replaced() {
    let fixture = Fixture::new();
    let auth = Auth::default();
    let server = fixture::serve_http(fixture.clone(), auth).await;
    let dispatcher = open();
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, ListingAuth::None).await;
    let lookup = call(&mcp, &tools["lookup"], json!({ "id": "c-1" }));

    // Concurrent calls share the session discovery opened.
    let calls: Vec<_> = (0..4)
        .map(|_| dispatcher.dispatch(lookup.clone(), CallAuth::None))
        .collect();
    for dispatched in futures_util::future::join_all(calls).await {
        result(dispatched);
    }
    assert_eq!(fixture.state().sessions, 1);
    assert_eq!(dispatcher.open_connections(), 1);

    // Other secrets, another session.
    pin(
        &dispatcher,
        &bearer_server(&server.url),
        ListingAuth::Secret(Secret::new("other")),
    )
    .await;
    assert_eq!(fixture.state().sessions, 2);
    assert_eq!(dispatcher.open_connections(), 2);

    // Closed, then opened again on demand.
    dispatcher.close();
    assert_eq!(dispatcher.open_connections(), 0);
    result(dispatcher.dispatch(lookup, CallAuth::None).await);
    assert_eq!(fixture.state().sessions, 3);
}

async fn an_unreachable_server_is_transient() {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let dispatcher = McpDispatcher::new(McpConfig {
        connect_timeout: Duration::from_secs(5),
        ..open_config()
    });
    let mcp = http_server(&format!("http://127.0.0.1:{port}/mcp"));
    let error = dispatcher
        .list_tools(&tenant(), &mcp, ListingAuth::None)
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Unreachable { .. }), "{error:?}");
    let tool = Tool::new(
        "fx.lookup",
        rewind_core::ToolRoute::Mcp {
            server: "fx".into(),
        },
        json!({}),
    );
    let unreachable = transient(
        dispatcher
            .dispatch(call(&mcp, &tool, json!({})), CallAuth::None)
            .await,
    );
    assert_eq!(unreachable.kind, ToolFailureKind::Unavailable);
    assert!(
        unreachable.message.starts_with("fx could not be reached"),
        "{}",
        unreachable.message
    );
}

async fn a_stdio_server_gets_its_own_environment_only() {
    // A variable of the dispatcher's own environment (cargo sets it for tests), which a stdio
    // server must not inherit.
    assert!(
        std::env::var("CARGO_MANIFEST_DIR").is_ok(),
        "run these tests with cargo test"
    );
    let dispatcher = McpDispatcher::default();
    let mcp = stdio_server(&[("MODE", "test")]);
    let tools = pin(&dispatcher, &mcp, ListingAuth::None).await;
    assert_eq!(tools.len(), 8);
    let env = result(
        dispatcher
            .dispatch(call(&mcp, &tools["env"], json!({})), CallAuth::None)
            .await,
    );
    assert_eq!(
        text_json(&env),
        json!({ "token": null, "manifest_dir": null, "path": true })
    );
    let issue = result(
        dispatcher
            .dispatch(
                call(&mcp, &tools["get_issue"], json!({ "number": 9 })),
                CallAuth::None,
            )
            .await,
    );
    assert_eq!(issue, json!({ "number": 9, "title": "Crash" }));
    dispatcher.close();
}

async fn a_missing_stdio_command_fails_for_good() {
    let dispatcher = McpDispatcher::default();
    let mcp = McpServer::new(
        "fx",
        McpTransport::Stdio {
            command: "/nonexistent/rewind-mcp-fixture".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        },
    );
    let tool = Tool::new(
        "fx.lookup",
        rewind_core::ToolRoute::Mcp {
            server: "fx".into(),
        },
        json!({}),
    );
    let failure = failed(
        dispatcher
            .dispatch(call(&mcp, &tool, json!({})), CallAuth::None)
            .await,
    );
    assert_eq!(failure.kind, ToolFailureKind::Unavailable);
    assert!(
        failure
            .message
            .starts_with("the MCP server fx could not be started"),
        "{}",
        failure.message
    );
}

async fn reference_server(command_line: Option<String>) {
    let Some(command_line) = command_line else {
        return;
    };
    let mut words = command_line.split_whitespace().map(str::to_owned);
    let command = words.next().expect("a command");
    let mcp = McpServer::new(
        "everything",
        McpTransport::Stdio {
            command,
            args: words.collect(),
            env: BTreeMap::new(),
        },
    );
    let dispatcher = McpDispatcher::new(McpConfig {
        connect_timeout: Duration::from_secs(120),
        ..McpConfig::default()
    });
    let tools = pin(&dispatcher, &mcp, ListingAuth::None).await;
    let echo = tools
        .get("echo")
        .expect("the reference server has an echo tool");
    let echoed = result(
        dispatcher
            .dispatch(
                call(&mcp, echo, json!({ "message": "hello from Rewind" })),
                CallAuth::None,
            )
            .await,
    );
    let rendered = echoed.to_string();
    assert!(rendered.contains("hello from Rewind"), "{rendered}");
    dispatcher.close();
}
