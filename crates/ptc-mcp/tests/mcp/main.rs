//! `McpDispatcher` against MCP servers started by the tests: the fixture of `fixture.rs`, over
//! streamable HTTP and over stdio, and a reference server when one is given.
//!
//! A custom harness (libtest-mimic): with `PTC_MCP_FIXTURE=stdio`, this binary is the stdio
//! server the stdio tests start. `PTC_MCP_REFERENCE_SERVER` names the command line of a
//! reference MCP server that has the `echo` tool of `@modelcontextprotocol/server-everything`,
//! such as `npx -y @modelcontextprotocol/server-everything`; without it, that test is ignored.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod fixture;

use std::collections::BTreeMap;
use std::future::Future;
use std::time::{Duration, Instant};

use fixture::{Auth, Fixture};
use libtest_mimic::{Arguments, Trial};
use ptc_core::{
    McpServer, McpTransport, RunId, Seq, Tool, ToolFailure, ToolFailureKind, ToolRoute, Toolset,
};
use ptc_engine::ports::{
    Credentials, Dispatched, ParkKind, RunRef, Secret, ServerCredentials, TaskToken, TenantId,
    ToolCall, ToolDispatcher, TraceContext,
};
use ptc_mcp::{IDEMPOTENCY_KEY_META, McpConfig, McpDispatcher, McpError};
use serde_json::{Value, json};

fn main() -> std::process::ExitCode {
    if std::env::var("PTC_MCP_FIXTURE").as_deref() == Ok("stdio") {
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
            "credentials_go_in_the_headers_and_a_401_parks",
            credentials_go_in_the_headers_and_a_401_parks(false),
        ),
        test(
            "a_401_with_a_challenge_parks_too",
            credentials_go_in_the_headers_and_a_401_parks(true),
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
            "stdio_server_with_credentials_in_its_environment",
            stdio_server_with_credentials_in_its_environment(),
        ),
        test(
            "a_missing_stdio_command_fails_for_good",
            a_missing_stdio_command_fails_for_good(),
        ),
    ];
    let reference = std::env::var("PTC_MCP_REFERENCE_SERVER").ok();
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
        BTreeMap::from([("PTC_MCP_FIXTURE".into(), "stdio".into())]);
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
    credentials: Option<&ServerCredentials>,
) -> BTreeMap<String, Tool> {
    dispatcher
        .list_tools(&tenant(), server, credentials)
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
        seq: Seq(3),
        tool: tool.clone(),
        server: Some(server.clone()),
        args,
        attempt: 1,
        schema_change_approved: false,
        token: TaskToken("token".into()),
        trace_context: None,
    }
}

fn bearer(token: &str) -> Credentials {
    Credentials {
        servers: BTreeMap::from([(
            "fx".to_owned(),
            ServerCredentials {
                headers: BTreeMap::from([(
                    "Authorization".to_owned(),
                    Secret::new(format!("Bearer {token}")),
                )]),
                env: BTreeMap::new(),
            },
        )]),
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
    let dispatcher = McpDispatcher::default();
    let mcp = http_server(&server.url);
    let tools = dispatcher.list_tools(&tenant(), &mcp, None).await.unwrap();
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
    // What discovery returns is a toolset.
    let toolset = Toolset::new([mcp], tools).unwrap();
    assert_eq!(toolset.tools().len(), 8);
}

async fn a_tool_list_without_end_is_refused() {
    let fixture = Fixture::new();
    fixture.state().endless_list = true;
    let server = fixture::serve_http(fixture.clone(), Auth::default()).await;
    let mcp = http_server(&server.url);
    // Listing went on until the timeout, the connection's list locked meanwhile.
    let dispatcher = McpDispatcher::new(McpConfig {
        connect_timeout: Duration::from_secs(20),
        ..McpConfig::default()
    });
    let started = Instant::now();
    let error = dispatcher.list_tools(&tenant(), &mcp, None).await.unwrap_err();
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
    let failure = failed(dispatcher.dispatch(call(&mcp, &tool, json!({})), None).await);
    assert_eq!(failure.kind, ToolFailureKind::Unavailable);
    assert!(failure.message.contains("ai-ptc reads no further"), "{failure:?}");
}

async fn content_and_structured_results() {
    let server = fixture::serve_http(Fixture::new(), Auth::default()).await;
    let dispatcher = McpDispatcher::default();
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, None).await;

    let lookup = result(
        dispatcher
            .dispatch(call(&mcp, &tools["lookup"], json!({ "id": "c-7" })), None)
            .await,
    );
    assert_eq!(lookup["content"][0]["type"], "text");
    assert_eq!(text_json(&lookup), json!({ "id": "c-7", "name": "Ada" }));
    assert!(lookup.get("structuredContent").is_none());

    let issue = result(
        dispatcher
            .dispatch(
                call(&mcp, &tools["get_issue"], json!({ "number": 4 })),
                None,
            )
            .await,
    );
    assert_eq!(issue, json!({ "number": 4, "title": "Crash" }));
}

async fn idempotency_key_and_trace_context_in_meta() {
    let fixture = Fixture::new();
    let server = fixture::serve_http(fixture.clone(), Auth::default()).await;
    let dispatcher = McpDispatcher::default();
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, None).await;
    let mut meta_call = call(&mcp, &tools["meta"], json!({}));
    meta_call.attempt = 2;
    meta_call.trace_context = Some(TraceContext {
        traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
        tracestate: Some("vendor=1".into()),
    });
    let expected_key = meta_call.idempotency_key.clone();
    result(dispatcher.dispatch(meta_call, None).await);
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
    let dispatcher = McpDispatcher::default();
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, None).await;

    let fail = failed(
        dispatcher
            .dispatch(call(&mcp, &tools["fail"], json!({})), None)
            .await,
    );
    assert_eq!(fail.kind, ToolFailureKind::Error);
    assert_eq!(fail.message, "no such customer");

    let invalid = failed(
        dispatcher
            .dispatch(call(&mcp, &tools["invalid"], json!({})), None)
            .await,
    );
    assert_eq!(invalid.kind, ToolFailureKind::Error);
    assert_eq!(invalid.message, "fx answered with an error: bad arguments");
    assert_eq!(invalid.data, Some(json!({ "field": "x" })));

    let internal = transient(
        dispatcher
            .dispatch(call(&mcp, &tools["internal"], json!({})), None)
            .await,
    );
    assert_eq!(internal.kind, ToolFailureKind::Unavailable);

    let not_an_object = failed(
        dispatcher
            .dispatch(call(&mcp, &tools["lookup"], json!([1])), None)
            .await,
    );
    assert_eq!(
        not_an_object.message,
        "the arguments of fx.lookup must be an object"
    );
}

async fn credentials_go_in_the_headers_and_a_401_parks(challenge: bool) {
    let auth = Auth {
        challenge,
        ..Auth::default()
    };
    *auth.token.lock().unwrap() = Some("first".into());
    let fixture = Fixture::new();
    let server = fixture::serve_http(fixture.clone(), auth.clone()).await;
    let dispatcher = McpDispatcher::default();
    let mcp = http_server(&server.url);

    // Without credentials, discovery is refused.
    let refused = dispatcher
        .list_tools(&tenant(), &mcp, None)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, McpError::Unauthorized { .. }),
        "{refused:?}"
    );

    let first = bearer("first");
    let tools = pin(&dispatcher, &mcp, first.servers.get("fx")).await;
    let lookup = call(&mcp, &tools["lookup"], json!({ "id": "c-1" }));
    result(
        dispatcher
            .dispatch(lookup.clone(), Some(first.clone()))
            .await,
    );

    // Without credentials, a call is parked.
    let Dispatched::Park(parked) = dispatcher.dispatch(lookup.clone(), None).await else {
        panic!("a call without credentials is parked");
    };
    assert_eq!(parked.kind, ParkKind::CredentialsExpired);

    // The token expires on the server: the open session's next call is parked too.
    *auth.token.lock().unwrap() = Some("second".into());
    let Dispatched::Park(parked) = dispatcher
        .dispatch(lookup.clone(), Some(first.clone()))
        .await
    else {
        panic!("a call refused with a 401 is parked");
    };
    assert_eq!(parked.kind, ParkKind::CredentialsExpired);
    assert!(
        parked
            .message
            .starts_with("fx refused the credentials (HTTP 401)"),
        "{}",
        parked.message
    );
    assert!(
        !parked.message.contains("first"),
        "the message never holds the credentials"
    );

    // Renewed credentials: the call goes through, on a new session.
    result(dispatcher.dispatch(lookup, Some(bearer("second"))).await);
}

async fn the_execution_timeout_cancels_the_call() {
    let fixture = Fixture::new();
    let server = fixture::serve_http(fixture.clone(), Auth::default()).await;
    let dispatcher = McpDispatcher::default();
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, None).await;
    let mut slow = tools["slow"].clone();
    slow.policy.execution_timeout = Some(Duration::from_millis(300));
    let started = Instant::now();
    let timed_out = transient(
        dispatcher
            .dispatch(call(&mcp, &slow, json!({ "ms": 10_000 })), None)
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
        ..McpConfig::default()
    });
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, None).await;
    let lookup = call(&mcp, &tools["lookup"], json!({ "id": "c-1" }));

    // A new optional property: the call still fits.
    fixture.state().tools[0] = fixture::tool(
        "lookup",
        "Find a customer by id.",
        json!({ "type": "object", "properties": { "id": { "type": "string" }, "fields": { "type": "array" } }, "required": ["id"] }),
        None,
    );
    result(dispatcher.dispatch(lookup.clone(), None).await);

    // A new required property: the call no longer fits, and waits for the harness.
    fixture.state().tools[0] = fixture::tool(
        "lookup",
        "Find a customer by id.",
        json!({ "type": "object", "properties": { "id": { "type": "string" }, "tenant": { "type": "string" } }, "required": ["id", "tenant"] }),
        None,
    );
    let Dispatched::Park(parked) = dispatcher.dispatch(lookup.clone(), None).await else {
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
    result(dispatcher.dispatch(approved, None).await);
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
            None,
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
    let Dispatched::Park(parked) = dispatcher.dispatch(lookup, None).await else {
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
    let dispatcher = McpDispatcher::default();
    let mcp = http_server(&server.url);
    let tools = pin(&dispatcher, &mcp, None).await;
    let lookup = call(&mcp, &tools["lookup"], json!({ "id": "c-1" }));

    // Concurrent calls share the session discovery opened.
    let calls: Vec<_> = (0..4)
        .map(|_| dispatcher.dispatch(lookup.clone(), None))
        .collect();
    for dispatched in futures_util::future::join_all(calls).await {
        result(dispatched);
    }
    assert_eq!(fixture.state().sessions, 1);
    assert_eq!(dispatcher.open_connections(), 1);

    // Other credentials, another session.
    result(
        dispatcher
            .dispatch(lookup.clone(), Some(bearer("other")))
            .await,
    );
    assert_eq!(fixture.state().sessions, 2);
    assert_eq!(dispatcher.open_connections(), 2);

    // Closed, then opened again on demand.
    dispatcher.close();
    assert_eq!(dispatcher.open_connections(), 0);
    result(dispatcher.dispatch(lookup, None).await);
    assert_eq!(fixture.state().sessions, 3);
}

async fn an_unreachable_server_is_transient() {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let dispatcher = McpDispatcher::new(McpConfig {
        connect_timeout: Duration::from_secs(5),
        ..McpConfig::default()
    });
    let mcp = http_server(&format!("http://127.0.0.1:{port}/mcp"));
    let error = dispatcher
        .list_tools(&tenant(), &mcp, None)
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Unreachable { .. }), "{error:?}");
    let tool = Tool::new(
        "fx.lookup",
        ptc_core::ToolRoute::Mcp {
            server: "fx".into(),
        },
        json!({}),
    );
    let unreachable = transient(
        dispatcher
            .dispatch(call(&mcp, &tool, json!({})), None)
            .await,
    );
    assert_eq!(unreachable.kind, ToolFailureKind::Unavailable);
    assert!(
        unreachable.message.starts_with("fx could not be reached"),
        "{}",
        unreachable.message
    );
}

async fn stdio_server_with_credentials_in_its_environment() {
    // A variable of the dispatcher's own environment (cargo sets it for tests), which a stdio
    // server must not inherit.
    assert!(
        std::env::var("CARGO_MANIFEST_DIR").is_ok(),
        "run these tests with cargo test"
    );
    let dispatcher = McpDispatcher::default();
    let mcp = stdio_server(&[("MODE", "test")]);
    let credentials = Credentials {
        servers: BTreeMap::from([(
            "fx".to_owned(),
            ServerCredentials {
                headers: BTreeMap::new(),
                env: BTreeMap::from([("FIXTURE_TOKEN".to_owned(), Secret::new("t0k3n"))]),
            },
        )]),
    };
    let tools = pin(&dispatcher, &mcp, credentials.servers.get("fx")).await;
    assert_eq!(tools.len(), 8);
    let env = result(
        dispatcher
            .dispatch(
                call(&mcp, &tools["env"], json!({})),
                Some(credentials.clone()),
            )
            .await,
    );
    assert_eq!(
        text_json(&env),
        json!({ "token": "t0k3n", "manifest_dir": null, "path": true })
    );
    let issue = result(
        dispatcher
            .dispatch(
                call(&mcp, &tools["get_issue"], json!({ "number": 9 })),
                Some(credentials),
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
            command: "/nonexistent/ptc-mcp-fixture".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        },
    );
    let tool = Tool::new(
        "fx.lookup",
        ptc_core::ToolRoute::Mcp {
            server: "fx".into(),
        },
        json!({}),
    );
    let failure = failed(
        dispatcher
            .dispatch(call(&mcp, &tool, json!({})), None)
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
    let tools = pin(&dispatcher, &mcp, None).await;
    let echo = tools
        .get("echo")
        .expect("the reference server has an echo tool");
    let echoed = result(
        dispatcher
            .dispatch(
                call(&mcp, echo, json!({ "message": "hello from ai-ptc" })),
                None,
            )
            .await,
    );
    let rendered = echoed.to_string();
    assert!(rendered.contains("hello from ai-ptc"), "{rendered}");
    dispatcher.close();
}
