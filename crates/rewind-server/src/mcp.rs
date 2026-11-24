//! `POST /mcp` (`archi/07-code-architecture.md`, "MCP and OAuth"): Rewind's MCP server
//! (`rewind-mcp-server`), for a connection's access token, an API key or an end user's token.
//!
//! - The request is authenticated here, before `rmcp` reads it. Without a valid Bearer it is 401,
//!   with `WWW-Authenticate` naming the resource's metadata when this server is an OAuth
//!   authorization server: that answer starts a client's OAuth.
//! - The caller goes in the request's extensions as a [`ServerConnector`], which the MCP service
//!   takes from there: its organizations, with the permissions it may use in each.
//! - Each tool calls `/v1` on this server over the loopback address, with the caller's Bearer,
//!   `Rewind-Organization` and `Rewind-Namespace`: the path every client takes, with the same
//!   authorization. What runs model-written code (`POST /v1/test`, `POST /v1/workflows`, a fork's
//!   suite) runs on this node, under its sandbox.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use rewind_agent_tools::{Api, ApiError as ToolError, McpApi, Organization};
use rewind_api::action_sets::{
    ActionSearchResults, ActionSet, ActionSetList, CreateActionSet, SearchQuery,
};
use rewind_api::common::Duration;
use rewind_api::events::HistoryEvent;
use rewind_api::headers::IdempotencyKey;
use rewind_api::namespaces::{NamespaceList, NamespaceListQuery, NamespaceName};
use rewind_api::runs::{
    CancelRun, ForkRun, Run, RunFilter, RunPage, SignalAccepted, StartRun, Transcript,
};
use rewind_api::scripts::{CheckRequest, CheckResult, Script, TestRequest};
use rewind_api::secrets::SecretList;
use rewind_api::testing::{Test, TestReport};
use rewind_api::workflows::{
    CreateWorkflow, RecordTests, RecordedTests, Workflow, WorkflowVersions,
};
use rewind_api::{Client, ProblemType, routes};
use rewind_mcp_server::{Connector, ConnectorError, http_service};
use serde_json::Value;

use crate::app::App;
use crate::auth::bearer;
use crate::connections::{self, McpCaller};
use crate::error::ApiError;
use crate::http::BODY_LIMIT;

/// What every request to `/mcp` shares.
struct McpState {
    app: Arc<App>,
    /// `/v1` on this server, over the loopback address.
    local: Client,
}

/// The routes of `/mcp`, served by a server listening on TCP at `local`.
pub(crate) fn router(app: Arc<App>, local: SocketAddr) -> Result<Router, String> {
    let loopback = SocketAddr::new(
        match local.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
            ip => ip,
        },
        local.port(),
    );
    let local = Client::remote(&format!("http://{loopback}"), None)
        .map_err(|error| format!("/mcp cannot reach this server's /v1: {error}"))?;
    let state = Arc::new(McpState { app, local });
    // No `Host` check: every request carries a Bearer and none reads a cookie, so a page that
    // rebinds a name to this server gains nothing.
    let service = http_service(
        |parts: &axum::http::request::Parts| parts.extensions.get::<ServerConnector>().cloned(),
        Vec::new(),
    );
    Ok(Router::new()
        .route_service(routes::MCP, service)
        .layer(from_fn(bounded))
        .layer(from_fn_with_state(state, authenticate)))
}

/// Reads the body up to `/v1`'s limit before `rmcp` does, whatever its encoding: `rmcp` reads a
/// body whole, without a bound of its own.
async fn bounded(request: Request, next: Next) -> Response {
    let (parts, body) = request.into_parts();
    match axum::body::to_bytes(body, BODY_LIMIT).await {
        Ok(bytes) => {
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
        Err(_) => ApiError::new(
            ProblemType::PayloadTooLarge,
            format!(
                "the body is above this server's limit of {} MiB, or could not be read",
                BODY_LIMIT / (1024 * 1024)
            ),
        )
        .into_response(),
    }
}

/// Authenticates a request to `/mcp`, and hands its caller to the MCP service.
async fn authenticate(
    State(state): State<Arc<McpState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let token = match bearer(request.headers()) {
        Ok(token) => token.to_owned(),
        Err(error) => return challenge(&state.app, error, false),
    };
    match connections::mcp_caller(&state.app, &token).await {
        Ok(caller) => match ServerConnector::new(&state, caller, &token) {
            Ok(connector) => {
                request.extensions_mut().insert(connector);
                next.run(request).await
            }
            Err(error) => error.into_response(),
        },
        Err(error) if error.0.status == ProblemType::Unauthorized.status() => {
            challenge(&state.app, error, true)
        }
        Err(error) => error.into_response(),
    }
}

/// The 401 of a request without a valid Bearer, with `WWW-Authenticate`: the resource's
/// metadata when this server is an OAuth authorization server, and `invalid_token` when a token
/// was sent (RFC 6750, RFC 9728).
fn challenge(app: &App, error: ApiError, token_sent: bool) -> Response {
    let mut value = String::from("Bearer");
    let mut parameters = Vec::new();
    if let Ok((oauth, _)) = app.oauth() {
        parameters.push(format!(
            "resource_metadata=\"{}{}\"",
            oauth.issuer,
            routes::PROTECTED_RESOURCE_MCP
        ));
    }
    if token_sent {
        parameters.push("error=\"invalid_token\"".to_owned());
    }
    if !parameters.is_empty() {
        value.push(' ');
        value.push_str(&parameters.join(", "));
    }
    let mut response = error.into_response();
    if let Ok(value) = HeaderValue::from_str(&value) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }
    response
}

/// The caller of one request to `/mcp`, as the MCP service sees it.
#[derive(Clone)]
pub(crate) struct ServerConnector {
    organizations: Arc<Vec<Organization>>,
    /// `/v1` on this server, with the caller's Bearer.
    local: Client,
}

impl ServerConnector {
    fn new(state: &McpState, caller: McpCaller, token: &str) -> Result<Self, ApiError> {
        let with_token = |client: &Client| {
            client.with_bearer(token).map_err(|error| {
                ApiError::new(
                    ProblemType::Unauthorized,
                    format!("this token cannot be sent again: {error}"),
                )
            })
        };
        let organizations = caller
            .organizations
            .into_iter()
            .map(|organization| Organization {
                id: organization.id,
                name: organization.name,
                permissions: organization.permissions,
            })
            .collect();
        Ok(Self {
            organizations: Arc::new(organizations),
            local: with_token(&state.local)?,
        })
    }
}

impl Connector for ServerConnector {
    type Api = ServerApi;

    async fn organizations(&self) -> Result<Vec<Organization>, ConnectorError> {
        Ok(self.organizations.as_ref().clone())
    }

    fn api(
        &self,
        organization: &Organization,
        namespace: Option<&NamespaceName>,
    ) -> Result<ServerApi, ConnectorError> {
        let bind = |client: &Client| -> Result<Client, ConnectorError> {
            let client = client
                .with_organization(&organization.id)
                .map_err(ConnectorError::new)?;
            Ok(match namespace {
                Some(namespace) => client.with_namespace(namespace.clone()),
                None => client,
            })
        };
        Ok(ServerApi {
            local: bind(&self.local)?,
        })
    }
}

/// `/v1` as one caller of `/mcp`, in one organization.
pub(crate) struct ServerApi {
    local: Client,
}

impl Api for ServerApi {
    async fn start_run(
        &self,
        start: &StartRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, ToolError> {
        Api::start_run(&self.local, start, key).await
    }

    async fn get_run(&self, run: &str, wait: Option<Duration>) -> Result<Run, ToolError> {
        Api::get_run(&self.local, run, wait).await
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<RunPage, ToolError> {
        Api::list_runs(&self.local, filter).await
    }

    async fn run_transcript(&self, run: &str) -> Result<Transcript, ToolError> {
        Api::run_transcript(&self.local, run).await
    }

    async fn run_event(&self, run: &str, idx: u64) -> Result<HistoryEvent, ToolError> {
        Api::run_event(&self.local, run, idx).await
    }

    async fn signal_run(
        &self,
        run: &str,
        name: &str,
        payload: &Value,
        key: Option<&IdempotencyKey>,
    ) -> Result<SignalAccepted, ToolError> {
        Api::signal_run(&self.local, run, name, payload, key).await
    }

    async fn cancel_run(&self, run: &str, cancel: &CancelRun) -> Result<Run, ToolError> {
        Api::cancel_run(&self.local, run, cancel).await
    }

    /// A fork of a run of a workflow runs its suite first: on a worker.
    async fn fork_run(
        &self,
        run: &str,
        fork: &ForkRun,
        key: Option<&IdempotencyKey>,
    ) -> Result<Run, ToolError> {
        Api::fork_run(&self.local, run, fork, key).await
    }

    async fn check(&self, check: &CheckRequest) -> Result<CheckResult, ToolError> {
        Api::check(&self.local, check).await
    }

    /// Tests run model-written code: on a worker.
    async fn test(&self, test: &TestRequest) -> Result<TestReport, ToolError> {
        Api::test(&self.local, test).await
    }

    async fn get_script(&self, digest: &str) -> Result<Script, ToolError> {
        Api::get_script(&self.local, digest).await
    }

    async fn get_payload(&self, digest: &str, run: &str) -> Result<Value, ToolError> {
        Api::get_payload(&self.local, digest, run).await
    }

    async fn get_test(&self, digest: &str) -> Result<Test, ToolError> {
        Api::get_test(&self.local, digest).await
    }

    async fn create_action_set(
        &self,
        action_set: &CreateActionSet,
    ) -> Result<ActionSet, ToolError> {
        Api::create_action_set(&self.local, action_set).await
    }

    async fn search_actions(
        &self,
        action_set: &str,
        query: &SearchQuery,
    ) -> Result<ActionSearchResults, ToolError> {
        Api::search_actions(&self.local, action_set, query).await
    }

    /// The gate runs the version's suite: on a worker.
    async fn create_workflow(&self, workflow: &CreateWorkflow) -> Result<Workflow, ToolError> {
        Api::create_workflow(&self.local, workflow).await
    }

    async fn workflow_versions(&self, name: &str) -> Result<WorkflowVersions, ToolError> {
        Api::workflow_versions(&self.local, name).await
    }

    async fn record_tests(
        &self,
        workflow: &str,
        record: &RecordTests,
    ) -> Result<RecordedTests, ToolError> {
        Api::record_tests(&self.local, workflow, record).await
    }

    async fn list_secrets(&self) -> Result<SecretList, ToolError> {
        Api::list_secrets(&self.local).await
    }

    async fn namespace_secrets(&self, namespace: &str) -> Result<SecretList, ToolError> {
        Api::namespace_secrets(&self.local, namespace).await
    }

    fn namespace(&self) -> Option<String> {
        Api::namespace(&self.local)
    }
}

impl McpApi for ServerApi {
    async fn list_namespaces(
        &self,
        query: &NamespaceListQuery,
    ) -> Result<NamespaceList, ToolError> {
        McpApi::list_namespaces(&self.local, query).await
    }

    async fn list_action_sets(&self) -> Result<ActionSetList, ToolError> {
        McpApi::list_action_sets(&self.local).await
    }
}
