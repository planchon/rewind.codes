//! Rewind's MCP server against a fake API and connector: the tools each caller sees, the
//! organization of a call, the permissions, the bounds, and whole exchanges through rmcp's
//! client, over a stream and over Streamable HTTP without session.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rewind_agent_tools::definitions::MCP_TOOLS;
use rewind_agent_tools::{Api, ApiError, McpApi, Organization};
use rewind_api::access::Permission;
use rewind_api::common::Duration;
use rewind_api::events::Entry;
use rewind_api::headers::IdempotencyKey;
use rewind_api::namespaces::{NamespaceList, NamespaceListQuery, NamespaceName};
use rewind_api::runs::{
    CancelRun, ForkRun, Run, RunFilter, RunPage, SignalAccepted, StartRun, Trace,
};
use rewind_api::scripts::{CheckRequest, CheckResult, Script, TestRequest};
use rewind_api::secrets::SecretList;
use rewind_api::testing::{Test, TestRun};
use rewind_api::toolsets::{CreateToolset, SearchQuery, ToolSearchResults, Toolset, ToolsetList};
use rewind_api::workflows::{
    CreateWorkflow, RecordTests, RecordedTests, Workflow, WorkflowVersions,
};
use rewind_api::{Problem, ProblemType};
use rewind_mcp_server::{Connector, ConnectorError, INSTRUCTIONS};
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt as _};
use serde_json::{Value, json};

/// A connection's access `read` (`archi/06-storage-and-scale.md`, "Connections").
const READ: [Permission; 4] = [
    Permission::RunsRead,
    Permission::ToolsetsRead,
    Permission::NamespacesRead,
    Permission::SecretsRead,
];

/// A connection's access `run`.
const RUN: [Permission; 7] = [
    Permission::RunsRead,
    Permission::ToolsetsRead,
    Permission::NamespacesRead,
    Permission::SecretsRead,
    Permission::RunsStart,
    Permission::RunsControl,
    Permission::SecretsUse,
];

fn organization(id: &str, name: Option<&str>, permissions: &[Permission]) -> Organization {
    Organization {
        id: id.into(),
        name: name.map(str::to_owned),
        permissions: permissions.to_vec(),
    }
}

/// What the fake API was asked: `{operation} org={id} ns={namespace}`.
type Log = Arc<Mutex<Vec<String>>>;

/// A run of the fake API: its handle is `alice/script/<id>`.
fn run(id: &str, output: Value) -> Run {
    serde_json::from_value(json!({
        "id": id, "handle": format!("alice/script/{id}"), "namespace": "alice",
        "status": "completed", "script": "s", "toolset": "t",
        "epoch": 1, "mode": "live", "labels": { "via": "claude" }, "traceId": "0", "historyLen": 3,
        "createdAt": "2026-11-19T10:00:00Z", "updatedAt": "2026-11-19T10:00:01Z",
        "endedAt": "2026-11-19T10:00:01Z", "expiresAt": "2026-12-19T10:00:00Z",
        "output": { "digest": "d", "bytes": 2, "preview": "{}", "value": output }
    }))
    .unwrap()
}

#[derive(Debug, Clone)]
struct FakeApi {
    organization: String,
    namespace: Option<String>,
    log: Log,
}

impl FakeApi {
    fn saw(&self, operation: &str) {
        self.log.lock().unwrap().push(format!(
            "{operation} org={} ns={}",
            self.organization,
            self.namespace.as_deref().unwrap_or("-")
        ));
    }
}

fn unused(operation: &str) -> ApiError {
    ApiError::fault(format!("the tests do not call {operation}"))
}

impl Api for FakeApi {
    async fn start_run(
        &self,
        start: &StartRun,
        _key: Option<&IdempotencyKey>,
    ) -> Result<Run, ApiError> {
        self.saw("start_run");
        let output = if start.script.contains("big") {
            json!("x".repeat(300_000))
        } else {
            json!({ "ok": true, "toolset": start.toolset, "wait": start.wait })
        };
        Ok(run("r1", output))
    }

    async fn get_run(&self, id: &str, _wait: Option<Duration>) -> Result<Run, ApiError> {
        self.saw("get_run");
        if id == "missing" {
            return Err(Problem::new(ProblemType::NotFound, format!("no run {id}")).into());
        }
        if id == "broken" {
            return Err(
                Problem::new(ProblemType::Unavailable, "the store cannot be reached").into(),
            );
        }
        Ok(run(id, json!(1)))
    }

    async fn list_runs(&self, _filter: &RunFilter) -> Result<RunPage, ApiError> {
        self.saw("list_runs");
        Ok(RunPage {
            runs: Vec::new(),
            next_cursor: None,
        })
    }

    async fn run_trace(&self, run: &str) -> Result<Trace, ApiError> {
        self.saw("run_trace");
        Ok(serde_json::from_value(
            json!({ "runId": run, "format": "llm", "text": "[0] run_started" }),
        )
        .unwrap())
    }

    async fn run_entry(&self, _run: &str, _idx: u64) -> Result<Entry, ApiError> {
        Err(unused("run_entry"))
    }

    async fn signal_run(
        &self,
        run: &str,
        name: &str,
        _payload: &Value,
        _key: Option<&IdempotencyKey>,
    ) -> Result<SignalAccepted, ApiError> {
        self.saw("signal_run");
        Ok(SignalAccepted {
            run_id: run.into(),
            name: name.into(),
            duplicate: false,
        })
    }

    async fn cancel_run(&self, _run: &str, _cancel: &CancelRun) -> Result<Run, ApiError> {
        Err(unused("cancel_run"))
    }

    async fn fork_run(
        &self,
        _run: &str,
        _fork: &ForkRun,
        _key: Option<&IdempotencyKey>,
    ) -> Result<Run, ApiError> {
        Err(unused("fork_run"))
    }

    async fn check(&self, check: &CheckRequest) -> Result<CheckResult, ApiError> {
        self.saw(&format!(
            "check toolset={}",
            check.toolset.as_deref().unwrap_or("-")
        ));
        Ok(CheckResult {
            ok: true,
            diagnostics: Vec::new(),
        })
    }

    async fn test(&self, _test: &TestRequest) -> Result<TestRun, ApiError> {
        Err(unused("test"))
    }

    async fn get_script(&self, _digest: &str) -> Result<Script, ApiError> {
        Err(unused("get_script"))
    }

    async fn get_payload(&self, _digest: &str, _run: Option<&str>) -> Result<Value, ApiError> {
        Err(unused("get_payload"))
    }

    async fn get_test(&self, _digest: &str) -> Result<Test, ApiError> {
        Err(unused("get_test"))
    }

    async fn create_toolset(&self, _toolset: &CreateToolset) -> Result<Toolset, ApiError> {
        Err(unused("create_toolset"))
    }

    async fn search_tools(
        &self,
        toolset: &str,
        query: &SearchQuery,
    ) -> Result<ToolSearchResults, ApiError> {
        self.saw(&format!(
            "search_tools toolset={toolset} query={}",
            query.query
        ));
        Ok(ToolSearchResults { tools: Vec::new() })
    }

    async fn create_workflow(&self, _workflow: &CreateWorkflow) -> Result<Workflow, ApiError> {
        Err(unused("create_workflow"))
    }

    async fn workflow_versions(&self, _name: &str) -> Result<WorkflowVersions, ApiError> {
        Err(unused("workflow_versions"))
    }

    async fn record_tests(
        &self,
        _workflow: &str,
        _record: &RecordTests,
    ) -> Result<RecordedTests, ApiError> {
        Err(unused("record_tests"))
    }

    async fn list_secrets(&self) -> Result<SecretList, ApiError> {
        self.saw("list_secrets");
        Ok(serde_json::from_value(json!({ "secrets": [{
            "name": "zendesk", "scope": "org", "reference": "org/zendesk",
            "createdBy": "user:u", "createdAt": "2026-11-19T10:00:00Z",
            "updatedBy": "user:u", "updatedAt": "2026-11-19T10:00:00Z", "reads": []
        }] }))
        .unwrap())
    }

    async fn namespace_secrets(&self, namespace: &str) -> Result<SecretList, ApiError> {
        self.saw(&format!("namespace_secrets of={namespace}"));
        Ok(serde_json::from_value(json!({ "secrets": [{
            "name": "github", "scope": "ns", "namespace": namespace, "reference": "ns/github",
            "createdBy": "token:alice", "createdAt": "2026-11-19T10:00:00Z",
            "updatedBy": "token:alice", "updatedAt": "2026-11-19T10:00:00Z", "reads": []
        }] }))
        .unwrap())
    }
}

impl McpApi for FakeApi {
    async fn list_namespaces(&self, query: &NamespaceListQuery) -> Result<NamespaceList, ApiError> {
        self.saw(&format!(
            "list_namespaces prefix={}",
            query.prefix.as_deref().unwrap_or("-")
        ));
        Ok(serde_json::from_value(json!({
            "month": "2026-11",
            "total": 1,
            "namespaces": [{
                "name": "alice",
                "displayName": "Alice Martin",
                "limits": {}, "exceptions": {},
                "usage": { "cpu": "1s", "activeRuns": 1, "runsStarted": 12, "scripts": 3 },
                "createdAt": "2026-11-01T00:00:00Z", "lastActiveAt": "2026-11-18T10:00:00Z"
            }]
        }))
        .unwrap())
    }

    async fn list_toolsets(&self) -> Result<ToolsetList, ApiError> {
        self.saw("list_toolsets");
        Ok(serde_json::from_value(json!({
            "toolsets": [{
                "id": "9f2c".repeat(16),
                "servers": [{ "name": "github", "transport": "http", "tools": 2 }],
                "tools": ["github.list_issues", "github.close_issue"]
            }]
        }))
        .unwrap())
    }
}

#[derive(Debug, Clone)]
struct FakeConnector {
    organizations: Vec<Organization>,
    log: Log,
}

impl FakeConnector {
    fn new(organizations: Vec<Organization>) -> (Self, Log) {
        let log = Log::default();
        (
            Self {
                organizations,
                log: log.clone(),
            },
            log,
        )
    }
}

impl Connector for FakeConnector {
    type Api = FakeApi;

    async fn organizations(&self) -> Result<Vec<Organization>, ConnectorError> {
        Ok(self.organizations.clone())
    }

    fn api(
        &self,
        organization: &Organization,
        namespace: Option<&NamespaceName>,
    ) -> Result<FakeApi, ConnectorError> {
        Ok(FakeApi {
            organization: organization.id.clone(),
            namespace: namespace.map(|namespace| namespace.as_str().to_owned()),
            log: self.log.clone(),
        })
    }
}

/// A client of the server over an in-memory stream, as `rewind mcp` serves it on stdio.
async fn client(connector: FakeConnector) -> RunningService<RoleClient, ()> {
    let (client_side, server_side) = tokio::io::duplex(4 << 20);
    let (server_read, server_write) = tokio::io::split(server_side);
    tokio::spawn(rewind_mcp_server::serve(
        connector,
        server_read,
        server_write,
    ));
    let (client_read, client_write) = tokio::io::split(client_side);
    ().serve((client_read, client_write)).await.unwrap()
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> CallToolResult {
    client
        .call_tool(
            CallToolRequestParams::new(name.to_owned())
                .with_arguments(arguments.as_object().cloned().unwrap()),
        )
        .await
        .unwrap()
}

fn text(result: &CallToolResult) -> &str {
    &result.content[0].as_text().unwrap().text
}

fn names(tools: &[Tool]) -> Vec<&str> {
    tools.iter().map(|tool| tool.name.as_ref()).collect()
}

#[tokio::test]
async fn the_tools_listed_are_those_one_organization_of_the_caller_allows() {
    let (reader, _) = FakeConnector::new(vec![organization("org_1", Some("Acme"), &READ)]);
    let client = client(reader).await;
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(
        names(&tools),
        [
            "list_orgs",
            "list_namespaces",
            "list_toolsets",
            "search_tools",
            "list_secrets",
            "get_run",
            "list_runs",
            "get_workflow",
            "record_tests"
        ]
    );

    let (nothing, _) = FakeConnector::new(Vec::new());
    let client = self::client(nothing).await;
    assert_eq!(
        names(&client.list_all_tools().await.unwrap()),
        ["list_orgs"]
    );

    let (both, _) = FakeConnector::new(vec![
        organization("org_1", Some("Acme"), &READ),
        organization("org_2", Some("Globex"), &RUN),
    ]);
    let client = self::client(both).await;
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(names(&tools), MCP_TOOLS);
    assert!(!names(&tools).contains(&"save_workflow"));
}

#[tokio::test]
async fn every_tool_has_a_title_and_its_annotations() {
    let (connector, _) = FakeConnector::new(vec![organization("org_1", None, &RUN)]);
    let client = client(connector).await;
    for tool in client.list_all_tools().await.unwrap() {
        let name = tool.name.as_ref();
        assert!(
            tool.title.as_deref().is_some_and(|title| !title.is_empty()),
            "{name}"
        );
        let annotations = tool.annotations.as_ref().unwrap();
        assert_eq!(annotations.title, tool.title, "{name}");
        let read_only = annotations.read_only_hint.unwrap();
        assert_eq!(
            annotations.destructive_hint,
            Some(name == "cancel_run"),
            "{name}"
        );
        assert_eq!(
            annotations.open_world_hint,
            Some(matches!(name, "run_script" | "retry_run")),
            "{name}"
        );
        assert!(annotations.idempotent_hint.is_some(), "{name}");
        assert_eq!(
            read_only,
            !matches!(
                name,
                "run_script" | "retry_run" | "signal_run" | "cancel_run" | "record_tests"
            ),
            "{name}"
        );
        let org = tool
            .input_schema
            .get("properties")
            .and_then(|p| p.get("org"));
        assert_eq!(org.is_some(), name != "list_orgs", "{name}");
    }
}

#[tokio::test]
async fn initialize_names_rewind_and_says_how_to_go_from_an_organization_to_a_run() {
    let (connector, _) = FakeConnector::new(vec![organization("org_1", None, &RUN)]);
    let client = client(connector).await;
    let info = client.peer_info().unwrap();
    let server = info.server_info.as_ref().unwrap();
    assert_eq!(server.name, "Rewind");
    assert_eq!(server.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(info.instructions.as_deref(), Some(INSTRUCTIONS));
    for step in [
        "list_orgs",
        "list_namespaces",
        "list_toolsets",
        "search_tools",
        "list_secrets",
        "check_script",
        "test_script",
        "run_script",
    ] {
        assert!(INSTRUCTIONS.contains(step), "{step}");
    }
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn list_secrets_names_what_a_run_may_attach_in_the_namespace_given() {
    let (connector, log) = FakeConnector::new(vec![organization("org_1", Some("Acme"), &RUN)]);
    let client = client(connector).await;
    let listed = call(&client, "list_secrets", json!({ "namespace": "alice" })).await;
    assert_eq!(listed.is_error, Some(false));
    assert_eq!(
        text(&listed),
        "The secrets you may attach, never a value:\n- org/zendesk, the organization's\n- ns/github, the end user's in the namespace alice\nAttach one in `secrets`, by the MCP server or API whose calls need it, such as {\"github\": \"ns/github\"}."
    );
    let seen = log.lock().unwrap().clone();
    assert_eq!(
        &seen[seen.len() - 2..],
        [
            "list_secrets org=org_1 ns=alice",
            "namespace_secrets of=alice org=org_1 ns=alice"
        ]
    );
}

#[tokio::test]
async fn the_organization_is_one_by_default_and_named_by_id_or_name_otherwise() {
    let (one, log) = FakeConnector::new(vec![organization("org_1", Some("Acme"), &RUN)]);
    let client = client(one).await;
    let listed = call(&client, "list_toolsets", json!({})).await;
    assert_eq!(listed.is_error, Some(false));
    assert!(
        text(&listed).contains(&"9f2c".repeat(16)),
        "{}",
        text(&listed)
    );
    assert_eq!(
        log.lock().unwrap().last().unwrap(),
        "list_toolsets org=org_1 ns=-"
    );

    let (several, log) = FakeConnector::new(vec![
        organization("org_1", Some("Acme"), &RUN),
        organization("org_2", Some("acme"), &RUN),
        organization("org_3", Some("Globex"), &RUN),
    ]);
    let client = self::client(several).await;
    let missing = call(&client, "list_runs", json!({})).await;
    assert_eq!(missing.is_error, Some(true));
    assert_eq!(
        text(&missing),
        "list_runs: you reach 3 organizations: give one as `org`, by its id or its name:\n- Acme (id org_1)\n- acme (id org_2)\n- Globex (id org_3)"
    );
    let named = call(
        &client,
        "list_runs",
        json!({ "org": "GLOBEX", "namespace": "bob" }),
    )
    .await;
    assert_eq!(named.is_error, Some(false), "{}", text(&named));
    assert_eq!(
        log.lock().unwrap().last().unwrap(),
        "list_runs org=org_3 ns=bob"
    );
    let by_id = call(&client, "list_runs", json!({ "org": "org_2" })).await;
    assert_eq!(by_id.is_error, Some(false));
    assert_eq!(
        log.lock().unwrap().last().unwrap(),
        "list_runs org=org_2 ns=-"
    );
    let ambiguous = call(&client, "list_runs", json!({ "org": "Acme" })).await;
    assert_eq!(ambiguous.is_error, Some(true));
    assert_eq!(
        text(&ambiguous),
        "list_runs: 2 of the organizations you reach are named \"Acme\": give one of their ids as `org`:\n- Acme (id org_1)\n- acme (id org_2)"
    );
    let unknown = call(&client, "list_runs", json!({ "org": "Initech" })).await;
    assert_eq!(unknown.is_error, Some(true));
    assert!(
        text(&unknown).starts_with("list_runs: you reach no organization \"Initech\". Give one of these as `org`, by its id or its name:\n- Acme (id org_1)"),
        "{}",
        text(&unknown)
    );

    let orgs = call(&client, "list_orgs", json!({})).await;
    assert_eq!(orgs.is_error, Some(false));
    assert!(
        text(&orgs).starts_with("You reach 3 organizations: give one as `org`, by its id or its name.\n- Acme (id org_1): you may read runs and record tests,"),
        "{}",
        text(&orgs)
    );
}

#[tokio::test]
async fn a_tool_needs_its_permission_in_the_organization_of_the_call() {
    let (connector, log) = FakeConnector::new(vec![
        organization("org_1", Some("Acme"), &READ),
        organization("org_2", Some("Globex"), &RUN),
    ]);
    let client = client(connector).await;
    let refused = call(
        &client,
        "run_script",
        json!({ "org": "Acme", "namespace": "alice", "code": "export default async () => 1;" }),
    )
    .await;
    assert_eq!(refused.is_error, Some(true));
    assert_eq!(
        text(&refused),
        "run_script: you may not use it in the organization Acme (id org_1): it needs the permission runs:start, which you do not hold there. list_orgs says what you may do in each organization."
    );
    assert!(log.lock().unwrap().is_empty(), "nothing reached the API");

    let ran = call(
        &client,
        "run_script",
        json!({ "org": "Globex", "namespace": "alice", "toolset": "ts1", "code": "export default async () => 1;" }),
    )
    .await;
    assert_eq!(ran.is_error, Some(false), "{}", text(&ran));
    assert!(
        text(&ran).starts_with("Run alice/script/r1 completed"),
        "{}",
        text(&ran)
    );
    // The default wait of the MCP set, and the call's toolset.
    assert!(text(&ran).contains("\"wait\":\"30s\""), "{}", text(&ran));
    assert!(text(&ran).contains("\"toolset\":\"ts1\""), "{}", text(&ran));
    assert_eq!(log.lock().unwrap()[0], "start_run org=org_2 ns=alice");
}

#[tokio::test]
async fn checking_testing_and_running_need_a_namespace() {
    let (connector, log) = FakeConnector::new(vec![organization("org_1", None, &RUN)]);
    let client = client(connector).await;
    for tool in ["check_script", "test_script", "run_script"] {
        let refused = call(
            &client,
            tool,
            json!({ "code": "export default async () => 1;" }),
        )
        .await;
        assert_eq!(refused.is_error, Some(true));
        assert_eq!(
            text(&refused),
            format!(
                "{tool}: `namespace` must be a non-empty string: the namespace (the end user) the script acts for, a name list_namespaces gives or a new one."
            )
        );
    }
    let invalid = call(
        &client,
        "check_script",
        json!({ "namespace": "alice smith", "code": "export default async () => 1;" }),
    )
    .await;
    assert_eq!(invalid.is_error, Some(true));
    assert_eq!(
        text(&invalid),
        "check_script: `namespace`: invalid namespace: \"alice smith\": expected 1 to 128 ASCII letters, digits, -, _, ., : and @"
    );
    assert!(log.lock().unwrap().is_empty());

    let checked = call(
        &client,
        "check_script",
        json!({ "namespace": "alice", "code": "/** Count to one */\nexport default async () => 1;" }),
    )
    .await;
    assert_eq!(checked.is_error, Some(false));
    assert_eq!(text(&checked), "The script type-checks.");
    assert_eq!(log.lock().unwrap()[0], "check toolset=- org=org_1 ns=alice");

    let search = call(&client, "search_tools", json!({ "query": "issues" })).await;
    assert_eq!(
        text(&search),
        "search_tools: `toolset` must be a non-empty string: a toolset's id, as list_toolsets gives it."
    );
}

#[tokio::test]
async fn a_result_holds_at_most_80_000_characters_and_pages_the_rest() {
    let (connector, _) = FakeConnector::new(vec![organization("org_1", None, &RUN)]);
    let client = client(connector).await;
    let ran = call(
        &client,
        "run_script",
        json!({ "namespace": "alice", "code": "// big\nexport default async () => 1;" }),
    )
    .await;
    let content = text(&ran);
    assert!(content.len() <= 80_000, "{}", content.len());
    assert!(
        content.starts_with("Run alice/script/r1 completed")
            && content.contains("the first 40000 are shown, read the rest with get_run({\"run_id\":\"alice/script/r1\",\"offset\":40000})"),
        "{}",
        &content[..300]
    );
}

#[tokio::test]
async fn refusals_are_results_and_faults_are_errors() {
    let (connector, _) = FakeConnector::new(vec![organization("org_1", None, &RUN)]);
    let client = client(connector).await;
    let missing = call(&client, "get_run", json!({ "run_id": "missing" })).await;
    assert_eq!(missing.is_error, Some(true));
    assert_eq!(text(&missing), "get_run failed: no run missing");

    let broken = client
        .call_tool(
            CallToolRequestParams::new("get_run")
                .with_arguments(json!({ "run_id": "broken" }).as_object().cloned().unwrap()),
        )
        .await
        .unwrap_err();
    assert!(
        broken.to_string().contains("the store cannot be reached"),
        "{broken}"
    );

    let unknown = client
        .call_tool(CallToolRequestParams::new("save_workflow"))
        .await
        .unwrap_err();
    assert!(
        unknown.to_string().contains("is not one of Rewind's tools"),
        "{unknown}"
    );
}

/// What the host found when it authenticated the request.
#[derive(Debug, Clone)]
struct Authenticated(String);

/// The host's part: a request without a Bearer is 401, before the MCP server.
async fn authenticate(mut request: Request, next: Next) -> Response {
    let bearer = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    match bearer {
        Some(token) => {
            request.extensions_mut().insert(Authenticated(token));
            next.run(request).await
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

#[tokio::test]
async fn a_whole_exchange_over_streamable_http_without_session() {
    let log = Log::default();
    let connect_log = log.clone();
    let service = rewind_mcp_server::http_service(
        move |parts: &http::request::Parts| {
            let caller = parts.extensions.get::<Authenticated>()?;
            let permissions: &[Permission] = if caller.0 == "rewind_ct_reader" {
                &READ
            } else {
                &RUN
            };
            Some(FakeConnector {
                organizations: vec![organization("org_1", Some("Acme"), permissions)],
                log: connect_log.clone(),
            })
        },
        Vec::new(),
    );
    let router = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn(authenticate));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let url = format!("http://{address}/mcp");

    let http = reqwest::Client::new();
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1" }
        }
    });
    let unauthorized = http.post(&url).json(&initialize).send().await.unwrap();
    assert_eq!(unauthorized.status(), 401);

    let post = |token: &'static str, body: Value| {
        http.post(&url)
            .bearer_auth(token)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2025-06-18")
            .json(&body)
            .send()
    };
    let answered = post("rewind_ct_runner", initialize.clone()).await.unwrap();
    assert_eq!(answered.status(), 200);
    assert_eq!(
        answered.headers().get("content-type").unwrap(),
        "application/json"
    );
    assert!(answered.headers().get("mcp-session-id").is_none());
    let body: Value = answered.json().await.unwrap();
    assert_eq!(body["result"]["serverInfo"]["name"], "Rewind");
    assert_eq!(body["result"]["instructions"], INSTRUCTIONS);

    let listed: Value = post(
        "rewind_ct_reader",
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let tools: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(
        tools.contains(&"get_run") && !tools.contains(&"run_script"),
        "{tools:?}"
    );
    assert_eq!(listed["result"]["cacheScope"], "private");

    let called: Value = post(
        "rewind_ct_runner",
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "list_namespaces", "arguments": { "prefix": "al" } }
        }),
    )
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(called["result"]["isError"], false);
    let listed_text = called["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        listed_text.starts_with("1 namespace starting with \"al\", the most active this month (2026-11) first:\n- alice (\"Alice Martin\"): 12 runs this month, 1 active"),
        "{listed_text}"
    );
    assert!(
        listed_text.ends_with("Give one's name, not its display name in parentheses, as `namespace` to check_script, test_script and run_script."),
        "{listed_text}"
    );
    assert_eq!(
        log.lock().unwrap().last().unwrap(),
        "list_namespaces prefix=al org=org_1 ns=-"
    );

    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let refused = http
            .request(method.clone(), &url)
            .bearer_auth("rewind_ct_runner")
            .header("accept", "text/event-stream")
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 405, "{method}");
    }
}

#[tokio::test]
async fn a_streamable_http_client_goes_from_initialize_to_a_tool_call() {
    let service = rewind_mcp_server::http_service(
        |_parts: &http::request::Parts| {
            Some(FakeConnector {
                organizations: vec![organization("org_1", Some("Acme"), &RUN)],
                log: Log::default(),
            })
        },
        vec!["127.0.0.1".into()],
    );
    let router = Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let transport = StreamableHttpClientTransport::from_uri(format!("http://{address}/mcp"));
    let client = ().serve(transport).await.unwrap();
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(names(&tools), MCP_TOOLS);
    let signalled = call(
        &client,
        "signal_run",
        json!({ "run_id": "r9", "name": "approval", "payload": { "ok": true } }),
    )
    .await;
    assert_eq!(signalled.is_error, Some(false));
    assert_eq!(
        text(&signalled),
        "Sent the signal \"approval\" to run alice/script/r9. Read what the run does next with get_run({\"run_id\":\"alice/script/r9\",\"wait\":\"30s\"})."
    );
}
