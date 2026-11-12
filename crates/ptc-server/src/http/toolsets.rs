//! Toolsets: registering and pinning them, their declarations, search, credentials
//! (`archi/04-tools.md`, "Toolsets and code generation").
//!
//! Registering connects to the MCP servers and lists their tools; the harness's policies, its
//! host tools and the `http` built-in (with its allow-list) are added, and so is the `webhook`
//! built-in, always: every script may call `webhook()`. The pinned toolset is content-addressed,
//! so registering the same toolset again gives the same id.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ptc_api::ProblemType;
use ptc_api::toolsets::{
    CreateToolset, Credentials, DeclarationFormat, Declarations, DeclarationsQuery,
    DeclarationsText, McpTransport as ApiTransport, OnInterruption, Route, SearchQuery,
    ServerSummary, Tool as ApiTool, ToolMatch, ToolPolicy as ApiPolicy, ToolSearchResults,
    Toolset as ApiToolset, ToolsetList, ToolsetSummary,
};
use ptc_core::{
    Digest, Interruption, McpServer, McpTransport, Tool, ToolPolicy, ToolRoute, Toolset,
};
use ptc_engine::ports::{
    JournalStore, ReadModel, Secret, ServerCredentials, StoredToolset, TenantId, ToolDeclarations,
};

use crate::app::App;
use crate::auth::{Access, Caller};
use crate::error::ApiError;
use crate::extract::{Body, Query};
use crate::wiring::tools::DiscoveryError;

/// `POST /v1/toolsets`.
pub(super) async fn create(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<CreateToolset>,
) -> Result<Response, ApiError> {
    caller.allow(Access::Write)?;
    let credentials = request.credentials.clone();
    let toolset = pin(&app, &caller.tenant, request).await?;
    let names: Vec<String> = toolset
        .tools()
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    let observed = app.store.observed_shapes(&caller.tenant, &names).await?;
    let generated = app.tools.declarations(&toolset, &observed);
    let declarations = ToolDeclarations {
        typescript: generated.typescript,
        prompt: generated.prompt,
    };
    let created = app
        .store
        .get_toolset(&caller.tenant, toolset.digest())
        .await?
        .is_none();
    app.store
        .put_toolset(&caller.tenant, &toolset, &declarations)
        .await?;
    if let Some(credentials) = credentials {
        app.engine
            .set_toolset_credentials(&caller.tenant, toolset.digest(), &secrets(credentials))
            .await?;
    }
    let stored = app
        .store
        .get_toolset(&caller.tenant, toolset.digest())
        .await?
        .ok_or_else(|| ApiError::internal("the toolset was not stored"))?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(view(&stored))).into_response())
}

/// `GET /v1/toolsets`.
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<ToolsetList>, ApiError> {
    caller.allow(Access::Read)?;
    let toolsets = app
        .store
        .list_toolsets(&caller.tenant)
        .await?
        .iter()
        .map(|stored| ToolsetSummary {
            id: stored.toolset.digest().to_hex(),
            servers: servers(&stored.toolset),
            tools: stored
                .toolset
                .tools()
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
        })
        .collect();
    Ok(Json(ToolsetList { toolsets }))
}

/// `GET /v1/toolsets/{id}/declarations`.
pub(super) async fn declarations(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<DeclarationsQuery>,
) -> Result<Json<DeclarationsText>, ApiError> {
    caller.allow(Access::Read)?;
    let stored = stored(&app, &caller.tenant, &id).await?;
    let format = query.format.unwrap_or_default();
    let text = match format {
        DeclarationFormat::Dts => stored.declarations.typescript,
        DeclarationFormat::Prompt => stored.declarations.prompt,
    };
    Ok(Json(DeclarationsText { format, text }))
}

/// `GET /v1/toolsets/{id}/search`: the tools whose name or description holds the most words
/// of the query, best first.
pub(super) async fn search(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<ToolSearchResults>, ApiError> {
    caller.allow(Access::Read)?;
    let stored = stored(&app, &caller.tenant, &id).await?;
    let limit = usize::try_from(query.limit.unwrap_or(10).clamp(1, 100)).unwrap_or(10);
    let words: Vec<String> = query
        .query
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect();
    if words.is_empty() {
        return Err(ApiError::invalid(
            "query: give at least one word to look for",
        ));
    }
    let names: Vec<String> = stored
        .toolset
        .tools()
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    let observed = app.store.observed_shapes(&caller.tenant, &names).await?;
    let generated = app.tools.declarations(&stored.toolset, &observed);
    let declared: BTreeMap<&str, _> = generated
        .tools
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect();
    let mut scored: Vec<(usize, &Tool)> = stored
        .toolset
        .tools()
        .iter()
        .map(|tool| {
            let name = tool.name.to_lowercase();
            let description = tool
                .description
                .as_deref()
                .unwrap_or_default()
                .to_lowercase();
            let score = words
                .iter()
                .map(|word| {
                    usize::from(name.contains(word.as_str())) * 2
                        + usize::from(description.contains(word.as_str()))
                })
                .sum();
            (score, tool)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by(|(a, tool_a), (b, tool_b)| b.cmp(a).then_with(|| tool_a.name.cmp(&tool_b.name)));
    let tools = scored
        .into_iter()
        .take(limit)
        .map(|(_, tool)| {
            let declared = declared.get(tool.name.as_str());
            ToolMatch {
                name: tool.name.clone(),
                description: tool.description.clone().unwrap_or_default(),
                prompt: declared.map(|d| d.prompt.clone()).unwrap_or_default(),
                dts: declared.map(|d| d.typescript.clone()).unwrap_or_default(),
            }
        })
        .collect();
    Ok(Json(ToolSearchResults { tools }))
}

/// `PUT /v1/toolsets/{id}/credentials`.
pub(super) async fn credentials(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Body(credentials): Body<Credentials>,
) -> Result<StatusCode, ApiError> {
    caller.allow(Access::Write)?;
    let digest = parse_digest(&id)?;
    app.engine
        .set_toolset_credentials(&caller.tenant, digest, &secrets(credentials))
        .await?;
    app.wake.bump();
    Ok(StatusCode::NO_CONTENT)
}

fn server_secrets(credentials: ptc_api::toolsets::ServerCredentials) -> ServerCredentials {
    let secret = |values: BTreeMap<String, String>| -> BTreeMap<String, Secret> {
        values
            .into_iter()
            .map(|(name, value)| (name, Secret::new(value)))
            .collect()
    };
    ServerCredentials {
        headers: secret(credentials.headers),
        env: secret(credentials.env),
    }
}

/// The credentials of the API, as the secret store keeps them.
pub(crate) fn secrets(credentials: Credentials) -> ptc_engine::ports::Credentials {
    let secret = |values: BTreeMap<String, String>| -> BTreeMap<String, Secret> {
        values
            .into_iter()
            .map(|(name, value)| (name, Secret::new(value)))
            .collect()
    };
    ptc_engine::ports::Credentials {
        servers: credentials
            .servers
            .into_iter()
            .map(|(server, credentials)| {
                (
                    server,
                    ServerCredentials {
                        headers: secret(credentials.headers),
                        env: secret(credentials.env),
                    },
                )
            })
            .collect(),
    }
}

pub(crate) fn parse_digest(id: &str) -> Result<Digest, ApiError> {
    Digest::from_hex(id).map_err(|_| {
        ApiError::not_found(format!(
            "no toolset {id:?}: a toolset's id is the digest POST /v1/toolsets returned"
        ))
    })
}

/// A stored toolset of the tenant, or 404.
pub(crate) async fn stored(
    app: &App,
    tenant: &TenantId,
    id: &str,
) -> Result<StoredToolset, ApiError> {
    let digest = parse_digest(id)?;
    app.store.get_toolset(tenant, digest).await?.ok_or_else(|| {
        ApiError::not_found(format!(
            "no toolset {id} for this tenant: register it with POST /v1/toolsets"
        ))
    })
}

fn invalid_toolset(detail: impl Into<String>) -> ApiError {
    ApiError::new(ProblemType::InvalidToolset, detail)
}

/// Pins a toolset: discovery of its MCP servers, the harness's policies, host tools and
/// built-ins.
async fn pin(app: &App, tenant: &TenantId, request: CreateToolset) -> Result<Toolset, ApiError> {
    let mut servers = Vec::new();
    let mut tools = Vec::new();
    let mut names = BTreeSet::new();
    for server in &request.mcp {
        if !names.insert(server.name.clone()) {
            return Err(invalid_toolset(format!(
                "the MCP server {:?} is declared twice",
                server.name
            )));
        }
        let transport = match (&server.url, &server.command) {
            (Some(url), None) => {
                if !(url.starts_with("https://") || url.starts_with("http://")) {
                    return Err(invalid_toolset(format!(
                        "MCP server {}: url must be http:// or https://",
                        server.name
                    )));
                }
                McpTransport::Http { url: url.clone() }
            }
            (None, Some(command)) => {
                if !app.settings.allow_stdio {
                    return Err(invalid_toolset(format!(
                        "MCP server {}: this server does not start stdio servers (command); give \
                         the url of a streamable HTTP server",
                        server.name
                    )));
                }
                McpTransport::Stdio {
                    command: command.clone(),
                    args: server.args.clone(),
                    env: server.env.clone(),
                }
            }
            _ => {
                return Err(invalid_toolset(format!(
                    "MCP server {}: give exactly one of url and command",
                    server.name
                )));
            }
        };
        let pinned = McpServer::new(server.name.clone(), transport);
        let credentials = request
            .credentials
            .as_ref()
            .and_then(|credentials| credentials.servers.get(&server.name))
            .map(|credentials| server_secrets(credentials.clone()));
        let listed = app
            .tools
            .discover(tenant, &pinned, credentials.as_ref())
            .await
            .map_err(|DiscoveryError(message)| {
                ApiError::new(ProblemType::ToolsetUnavailable, message)
            })?;
        if let Some(unknown) = server
            .policies
            .keys()
            .find(|name| !listed.iter().any(|tool| tool.local_name() == name.as_str()))
        {
            return Err(invalid_toolset(format!(
                "MCP server {}: a policy names {unknown:?}, which the server does not have",
                server.name
            )));
        }
        for mut tool in listed {
            if let Some(policy) = server.policies.get(tool.local_name()) {
                tool.policy = policy_of(&tool.route, *policy, &tool.name)?;
            }
            tools.push(tool);
        }
        servers.push(pinned);
    }
    for host in request.host {
        if !host.input_schema.is_object() {
            return Err(invalid_toolset(format!(
                "host tool {}: inputSchema must be a JSON Schema object",
                host.name
            )));
        }
        let group = host.name.split_once('.').map(|(group, _)| group.to_owned());
        let route = ToolRoute::Host { group };
        let mut tool = Tool::new(host.name.clone(), route.clone(), host.input_schema);
        tool.description = Some(host.description);
        tool.output_schema = host.output_schema;
        if let Some(policy) = host.policy {
            tool.policy = policy_of(&route, policy, &host.name)?;
        }
        tools.push(tool);
    }
    if let Some(http) = request.http {
        if http.allow.is_empty() {
            return Err(invalid_toolset(
                "http: allow needs at least one host; without one there is no http tool",
            ));
        }
        let mut tool = app.tools.http_tool(http.allow);
        if let Some(policy) = http.policy {
            tool.policy = policy_of(&tool.route.clone(), policy, "http")?;
        }
        tools.push(tool);
    }
    tools.push(app.tools.webhook_tool());
    Toolset::new(servers, tools).map_err(|error| invalid_toolset(error.to_string()))
}

/// A policy of the API, with the route's defaults for what it leaves out.
fn policy_of(route: &ToolRoute, policy: ApiPolicy, tool: &str) -> Result<ToolPolicy, ApiError> {
    let mut pinned = ToolPolicy::default_for(route);
    if let Some(on) = policy.on_interruption {
        pinned.on_interruption = match on {
            OnInterruption::Retry => Interruption::Retry,
            OnInterruption::AtMostOnce => Interruption::AtMostOnce,
        };
    }
    if let Some(approval) = policy.approval {
        pinned.approval = approval;
    }
    if let Some(timeout) = policy.timeout {
        if timeout.as_millis() == 0 {
            return Err(invalid_toolset(format!("{tool}: timeout must be positive")));
        }
        pinned.execution_timeout = Some(timeout.get());
    }
    if let Some(heartbeat) = policy.heartbeat_timeout {
        if heartbeat.get() < Duration::from_secs(1) {
            return Err(invalid_toolset(format!(
                "{tool}: heartbeatTimeout must be at least 1s"
            )));
        }
        pinned.heartbeat_timeout = heartbeat.get();
    }
    if let Some(concurrency) = policy.concurrency {
        if concurrency == 0 {
            return Err(invalid_toolset(format!(
                "{tool}: concurrency must be at least 1"
            )));
        }
        pinned.concurrency = concurrency;
    }
    Ok(pinned)
}

fn servers(toolset: &Toolset) -> Vec<ServerSummary> {
    toolset
        .servers()
        .iter()
        .map(|server| ServerSummary {
            name: server.name.clone(),
            transport: match server.transport {
                McpTransport::Http { .. } => ApiTransport::Http,
                McpTransport::Stdio { .. } => ApiTransport::Stdio,
            },
            tools: u32::try_from(
                toolset
                    .tools()
                    .iter()
                    .filter(|tool| matches!(&tool.route, ToolRoute::Mcp { server: of } if *of == server.name))
                    .count(),
            )
            .unwrap_or(u32::MAX),
        })
        .collect()
}

/// A stored toolset, as the API shows it.
pub(crate) fn view(stored: &StoredToolset) -> ApiToolset {
    let toolset = &stored.toolset;
    ApiToolset {
        id: toolset.digest().to_hex(),
        servers: servers(toolset),
        tools: toolset
            .tools()
            .iter()
            .map(|tool| ApiTool {
                name: tool.name.clone(),
                route: match tool.route {
                    ToolRoute::Mcp { .. } => Route::Mcp,
                    ToolRoute::Host { .. } => Route::Host,
                    ToolRoute::Builtin(_) => Route::Builtin,
                },
                server: match &tool.route {
                    ToolRoute::Mcp { server } => Some(server.clone()),
                    _ => None,
                },
                description: tool.description.clone().unwrap_or_default(),
                input_schema: tool.input_schema.clone(),
                output_schema: tool.output_schema.clone(),
                policy: ApiPolicy {
                    on_interruption: Some(match tool.policy.on_interruption {
                        Interruption::Retry => OnInterruption::Retry,
                        Interruption::AtMostOnce => OnInterruption::AtMostOnce,
                    }),
                    approval: Some(tool.policy.approval),
                    timeout: tool.policy.execution_timeout.map(Into::into),
                    heartbeat_timeout: Some(tool.policy.heartbeat_timeout.into()),
                    concurrency: Some(tool.policy.concurrency),
                },
            })
            .collect(),
        declarations: Declarations {
            dts: stored.declarations.typescript.clone(),
            prompt: stored.declarations.prompt.clone(),
        },
    }
}
