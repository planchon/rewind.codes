//! `rewind mcp` end to end: an MCP client (rmcp's) on the command's streams, the command in this
//! process, and a stand-in embedded server on the socket of the state directory, found running
//! with this CLI's version. Initialize, the tools, a check through the API.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use rewind_api::admin::{ServerInfo, ServerRole};
use rewind_api::scripts::{CheckRequest, CheckResult, Diagnostic, Location, Severity};
use rewind_cli::{Console, Exit};
use rmcp::ServiceExt as _;
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};

/// What the stand-in server was asked: the namespace of each check, and its script.
type Seen = Arc<Mutex<Vec<(Option<String>, CheckRequest)>>>;

fn app(seen: Seen) -> Router {
    Router::new()
        .route(
            "/v1/server",
            get(|| async {
                axum::Json(ServerInfo {
                    version: rewind_cli::embedded::VERSION.into(),
                    epochs: vec![1],
                    roles: vec![ServerRole::Api, ServerRole::Worker],
                    embedded: true,
                    virtual_time: false,
                    mcp_url: None,
                    oauth: false,
                })
            }),
        )
        .route(
            "/v1/check",
            post(
                move |headers: HeaderMap, axum::Json(request): axum::Json<CheckRequest>| {
                    let seen = seen.clone();
                    async move {
                        let namespace = headers
                            .get("rewind-namespace")
                            .map(|value| value.to_str().unwrap().to_owned());
                        let refused = request.script.contains("42");
                        seen.lock().unwrap().push((namespace, request));
                        axum::Json(if refused {
                            CheckResult {
                                ok: false,
                                diagnostics: vec![Diagnostic {
                                    severity: Severity::Error,
                                    code: "TS2322".into(),
                                    message: "Type 'number' is not assignable to type 'string'."
                                        .into(),
                                    location: Some(Location {
                                        line: 1,
                                        column: 7,
                                        end_line: None,
                                        end_column: None,
                                    }),
                                    frame: None,
                                    hint: None,
                                }],
                            }
                        } else {
                            CheckResult {
                                ok: true,
                                diagnostics: Vec::new(),
                            }
                        })
                    }
                },
            ),
        )
}

fn arguments(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().cloned().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn rewind_mcp_serves_the_agent_tools_against_the_embedded_server() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join(".rewind");
    std::fs::create_dir_all(&state).unwrap();
    let listener = tokio::net::UnixListener::bind(state.join("server.sock")).unwrap();
    let seen = Seen::default();
    tokio::spawn(axum::serve(listener, app(seen.clone())).into_future());

    let (client_side, command_side) = tokio::io::duplex(1 << 20);
    let (command_input, command_output) = tokio::io::split(command_side);
    let (console, captured) = Console::captured();
    let mut console = console.with_mcp_streams(command_input, command_output);
    let argv = vec![
        "--dir".to_owned(),
        state.display().to_string(),
        "mcp".to_owned(),
    ];
    let command = tokio::spawn(async move { rewind_cli::run(argv, &mut console).await });

    let (client_input, client_output) = tokio::io::split(client_side);
    let client = ().serve((client_input, client_output)).await.unwrap();
    let info = client.peer_info().unwrap();
    assert_eq!(info.server_info.as_ref().unwrap().name, "Rewind");
    assert!(info.instructions.as_deref().unwrap().contains("list_orgs"));

    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert_eq!(names, rewind_agent_tools::definitions::MCP_TOOLS);

    let orgs = client
        .call_tool(CallToolRequestParams::new("list_orgs"))
        .await
        .unwrap();
    assert_eq!(
        orgs.content[0].as_text().unwrap().text,
        "You reach one organization: the tools act in it without `org`.\n- id local: you may read runs and record tests, check, test, run and retry scripts, signal and cancel runs, read toolsets and workflows, list namespaces."
    );

    let checked = client
        .call_tool(
            CallToolRequestParams::new("check_script").with_arguments(arguments(json!({
                "namespace": "alice",
                "code": "export default async () => 1;"
            }))),
        )
        .await
        .unwrap();
    assert_eq!(checked.is_error, Some(false));
    assert_eq!(
        checked.content[0].as_text().unwrap().text,
        "The script type-checks."
    );

    let refused = client
        .call_tool(
            CallToolRequestParams::new("check_script").with_arguments(arguments(json!({
                "org": "local",
                "namespace": "bob",
                "code": "const x: string = 42;\nexport default async () => x;"
            }))),
        )
        .await
        .unwrap();
    assert_eq!(refused.is_error, Some(true));
    assert_eq!(
        refused.content[0].as_text().unwrap().text,
        "The script does not type-check: 1 error.\nscript.ts:1:7 error TS2322: Type 'number' is not assignable to type 'string'."
    );

    let checks = seen.lock().unwrap().clone();
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0].0.as_deref(), Some("alice"));
    assert_eq!(checks[0].1.script, "export default async () => 1;");
    assert_eq!(checks[0].1.toolset, None);
    assert_eq!(checks[1].0.as_deref(), Some("bob"));

    client.cancel().await.unwrap();
    let exit = command.await.unwrap();
    assert_eq!(exit, Exit::Success, "{}", captured.stderr());
    assert_eq!(captured.stdout(), "", "stdout belongs to the MCP client");
}

#[tokio::test]
async fn rewind_mcp_never_serves_a_remote_server() {
    let (mut console, captured) = Console::captured();
    let exit = rewind_cli::run(["--url", "https://rewind.example.com", "mcp"], &mut console).await;
    assert_eq!(exit, Exit::Usage);
    assert_eq!(
        captured.stderr().trim_end(),
        "error: rewind mcp serves the embedded server of this directory only, to debug the agent tools: leave out --url (REWIND_URL). Claude reaches a deployed Rewind at its /mcp URL, with OAuth"
    );
}

#[tokio::test]
async fn rewind_mcp_says_it_is_for_debugging() {
    let (mut console, captured) = Console::captured();
    let exit = rewind_cli::run(["mcp", "--help"], &mut console).await;
    assert_eq!(exit, Exit::Success);
    let help = captured.stdout();
    assert!(
        help.contains("to debug them with an MCP client on this machine"),
        "{help}"
    );
    assert!(
        help.contains("MCP Inspector") && help.contains("Claude Code"),
        "{help}"
    );
    assert!(help.contains("Never a remote server"), "{help}");
}
