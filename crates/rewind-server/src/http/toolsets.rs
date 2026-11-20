//! Toolsets: registering and pinning them, their declarations, search (`archi/04-tools.md`,
//! "Toolsets and code generation", "API tools", "Authenticating calls").
//!
//! Registering connects to the MCP servers and lists their tools, and reads the APIs' OpenAPI
//! documents; each server's and API's `auth` is pinned with it; the harness's policies, its
//! host tools and the `http` built-in (with its allow-list) are added, and so is the `webhook`
//! built-in, always: every script may call `webhook()`. The pinned toolset is content-addressed,
//! so registering the same toolset again gives the same id. It holds no secret.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::access::Permission;
use rewind_api::toolsets::{
    ApiSummary, Auth, CreateToolset, DeclarationFormat, Declarations, DeclarationsQuery,
    DeclarationsText, McpTransport as ApiTransport, OnInterruption, Route, SearchQuery,
    ServerSummary, Tool as ApiTool, ToolMatch, ToolPolicy as ApiPolicy, ToolSearchResults,
    Toolset as ApiToolset, ToolsetList, ToolsetSummary,
};
use rewind_core::{
    Digest, Interruption, McpServer, McpTransport, RunId, SecretRef, SecretScope, Tool, ToolAuth,
    ToolPolicy, ToolRoute, Toolset,
};
use rewind_engine::attachments::{AttachmentProblem, RefusedAttachments};
use rewind_engine::ports::{
    Day, JournalStore, NamespaceName, ReadModel, Secret, SecretId, SecretNamespace, SecretReader,
    SecretStore, StoredToolset, TenantId, ToolDeclarations,
};
use rewind_engine::text::{TOOLSET_TEXT, refuse_nul};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::{Body, Query};
use crate::http::workflows;
use crate::wiring::tools::DiscoveryError;

/// `POST /v1/toolsets`.
pub(super) async fn create(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(mut request): Body<CreateToolset>,
) -> Result<Response, ApiError> {
    caller.require(Permission::ToolsetsWrite)?;
    let references = std::mem::take(&mut request.workflows);
    let toolset = match request.base.take() {
        Some(base) => {
            if !request.mcp.is_empty()
                || !request.apis.is_empty()
                || !request.host.is_empty()
                || request.http.is_some()
            {
                return Err(invalid_toolset(
                    "a toolset derived from a base keeps its servers and tools: give only \
                     workflows with base",
                ));
            }
            stored(&app, &caller.tenant, &base).await?.toolset
        }
        None => pin(&app, &caller, request).await?,
    };
    let toolset =
        workflows::pin(&app, &caller.tenant, &caller.labels, &references, toolset).await?;
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
    refuse_nul(TOOLSET_TEXT, &declarations.typescript)?;
    refuse_nul(TOOLSET_TEXT, &declarations.prompt)?;
    let created = app
        .store
        .get_toolset(&caller.tenant, toolset.digest())
        .await?
        .is_none();
    app.store
        .put_toolset(&caller.tenant, &toolset, &declarations)
        .await?;
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
    caller.require(Permission::ToolsetsRead)?;
    let toolsets = app
        .store
        .list_toolsets(&caller.tenant)
        .await?
        .iter()
        .map(|stored| ToolsetSummary {
            id: stored.toolset.digest().to_hex(),
            servers: servers(&stored.toolset),
            apis: apis(&stored.toolset),
            tools: stored
                .toolset
                .tools()
                .iter()
                .map(|tool| tool.name.clone())
                .collect(),
            workflows: stored
                .toolset
                .workflows()
                .iter()
                .map(|workflow| workflow.name.clone())
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
    caller.require(Permission::ToolsetsRead)?;
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
    caller.require(Permission::ToolsetsRead)?;
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
    // The tools, and the workflows as `workflows.<name>`, with their descriptions.
    let candidates: Vec<(String, String)> = stored
        .toolset
        .tools()
        .iter()
        .map(|tool| {
            (
                tool.name.clone(),
                tool.description.clone().unwrap_or_default(),
            )
        })
        .chain(stored.toolset.workflows().iter().map(|workflow| {
            (
                format!("{}.{}", rewind_core::WORKFLOWS_NAMESPACE, workflow.name),
                workflow.description.clone().unwrap_or_default(),
            )
        }))
        .collect();
    let mut scored: Vec<(usize, &(String, String))> = candidates
        .iter()
        .map(|candidate| {
            let name = candidate.0.to_lowercase();
            let description = candidate.1.to_lowercase();
            let score = words
                .iter()
                .map(|word| {
                    usize::from(name.contains(word.as_str())) * 2
                        + usize::from(description.contains(word.as_str()))
                })
                .sum();
            (score, candidate)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by(|(a, tool_a), (b, tool_b)| b.cmp(a).then_with(|| tool_a.0.cmp(&tool_b.0)));
    let tools = scored
        .into_iter()
        .take(limit)
        .map(|(_, (name, description))| {
            let declared = declared.get(name.as_str());
            ToolMatch {
                name: name.clone(),
                description: description.clone(),
                prompt: declared.map(|d| d.prompt.clone()).unwrap_or_default(),
                dts: declared.map(|d| d.typescript.clone()).unwrap_or_default(),
            }
        })
        .collect();
    Ok(Json(ToolSearchResults { tools }))
}

/// How the API's `auth` pins (`archi/04-tools.md`, "Authenticating calls"); none when absent.
fn auth_of(auth: Option<&Auth>, of: &str) -> Result<ToolAuth, ApiError> {
    auth.map_or(Ok(ToolAuth::None), |auth| {
        auth.to_core()
            .map_err(|message| invalid_toolset(format!("{of}: {message}")))
    })
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

/// Pins a toolset: discovery of its MCP servers, its APIs' operations, the harness's policies,
/// host tools and built-ins.
async fn pin(app: &App, caller: &Caller, request: CreateToolset) -> Result<Toolset, ApiError> {
    let tenant = &caller.tenant;
    let listing = if request.secrets.is_empty() {
        BTreeMap::new()
    } else {
        attached_for_listing(app, caller, &request).await?
    };
    if !request.apis.is_empty() {
        read_apis(&request);
    }
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
        let mut pinned = McpServer::new(server.name.clone(), transport);
        pinned.auth = auth_of(server.auth.as_ref(), &format!("MCP server {}", server.name))?;
        if pinned.auth != ToolAuth::None {
            listing_auth(&pinned, listing.get(&server.name));
        }
        let listed = app.tools.discover(tenant, &pinned, None).await.map_err(
            |DiscoveryError(message)| ApiError::new(ProblemType::ToolsetUnavailable, message),
        )?;
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
        // The harness writes these schemas: one that cannot be compiled is refused here, rather
        // than refusing every call of the tool later (an MCP server's own schemas are taken as
        // listed, `archi/08-decisions.md`).
        let schemas = [
            Some(("inputSchema", &host.input_schema)),
            host.output_schema
                .as_ref()
                .map(|schema| ("outputSchema", schema)),
        ];
        for (field, schema) in schemas.into_iter().flatten() {
            if let Some(error) = rewind_engine::schema::Validator::compile(schema).unusable() {
                return Err(invalid_toolset(format!(
                    "host tool {}: {field} is not a usable JSON Schema: {error}",
                    host.name
                )));
            }
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

/// The organization's secrets `secrets` attaches for the listings, read, by server
/// (`archi/04-tools.md`, "Secrets": `POST /v1/toolsets` attaches an organization's secrets only,
/// to list the tools of an MCP server that needs one; no run keeps them). The caller holds
/// `secrets:use`, and each secret is an organization's it sees (its labels), attached for an
/// MCP server of the request whose `auth` takes a secret. A read is counted as the run
/// [`LISTING_RUN`]'s, in the caller's namespace (`local` without one).
async fn attached_for_listing(
    app: &App,
    caller: &Caller,
    request: &CreateToolset,
) -> Result<BTreeMap<String, Secret>, ApiError> {
    caller.require(Permission::SecretsUse)?;
    let mut problems = Vec::new();
    let mut attached = Vec::new();
    for (server, text) in &request.secrets {
        let secret = SecretRef::parse(text)
            .map_err(|error| ApiError::invalid(format!("secrets.{server}: {error}")))?;
        if secret.scope != SecretScope::Organization {
            return Err(ApiError::invalid(format!(
                "secrets.{server}: {secret} is a namespace's secret, and a registration has no \
                 namespace: attach an organization's secret (org/<name>) to list the tools"
            )));
        }
        let takes_secret = request
            .mcp
            .iter()
            .find(|mcp| mcp.name == *server)
            .map(|mcp| auth_of(mcp.auth.as_ref(), &format!("MCP server {server}")))
            .transpose()?
            .is_some_and(|auth| auth.takes_secret());
        if !takes_secret {
            problems.push(AttachmentProblem::TakesNone {
                server: server.clone(),
            });
            continue;
        }
        attached.push((server.clone(), secret));
    }
    let today = Day::of(app.now());
    let mut read = BTreeMap::new();
    let reader = SecretReader {
        run: LISTING_RUN,
        namespace: caller.namespace.clone().unwrap_or_else(|| {
            NamespaceName::new(NamespaceName::LOCAL).unwrap_or_else(|_| NamespaceName::migrated())
        }),
    };
    for (server, secret) in attached {
        let id = SecretId::organization(secret.name.clone());
        let seen = app
            .engine
            .secrets()
            .meta(&caller.tenant, &id, today)
            .await?
            .is_some_and(|meta| {
                caller
                    .labels
                    .iter()
                    .all(|(key, value)| meta.labels.get(key) == Some(value))
            });
        let value = if seen {
            app.engine
                .secrets()
                .read(&caller.tenant, &id, &reader, app.now())
                .await?
        } else {
            None
        };
        match value {
            Some(value) => {
                read.insert(server, value);
            }
            None => problems.push(AttachmentProblem::Unknown { server, secret }),
        }
    }
    if problems.is_empty() {
        return Ok(read);
    }
    problems.sort_by(|a, b| a.server().cmp(b.server()));
    let listed = app
        .engine
        .secrets()
        .list(
            &caller.tenant,
            &SecretNamespace::Organization,
            &caller.labels,
            today,
        )
        .await?;
    let mut attachable: Vec<SecretRef> = listed.iter().map(|meta| meta.id.to_ref()).collect();
    attachable.truncate(rewind_engine::attachments::MAX_ATTACHABLE);
    let refused = RefusedAttachments {
        problems,
        attachable,
    };
    Err(ApiError::secret_required(refused.to_string(), &refused))
}

/// The run a listing's read of a secret is counted for: none, the nil id.
pub(crate) const LISTING_RUN: RunId = RunId::from_u128(0);

/// How the listing of a server's tools authenticates at registration: the attached
/// organization's secret, or a JWT without the claims of a call (`archi/04-tools.md`, "Signed
/// calls": `iss`, `aud`, `jti`, `htm`, `htu`, `body`, `iat`, `exp`).
#[expect(
    clippy::todo,
    reason = "the outbound workstream authenticates MCP listings"
)]
fn listing_auth(server: &McpServer, secret: Option<&Secret>) {
    let _ = (server, secret);
    todo!(
        "outbound: list the tools of a server whose auth is a secret (the one `secrets` \
         attaches) or rewind (a signed listing), through rewind-outbound"
    )
}

/// Reads each API's OpenAPI document into its tools (`archi/04-tools.md`, "API tools"), with
/// its `auth` or the document's security requirement, its policies, and the operations it
/// leaves out (`skipped`).
#[expect(
    clippy::todo,
    reason = "the outbound workstream reads OpenAPI documents"
)]
fn read_apis(request: &CreateToolset) {
    let _ = request;
    todo!(
        "outbound: read the OpenAPI documents (rewind-openapi), fetched through the egress \
         checks, into Api and its tools; Toolset::from_parts"
    )
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

fn apis(toolset: &Toolset) -> Vec<ApiSummary> {
    toolset
        .apis()
        .iter()
        .map(|api| ApiSummary {
            name: api.name.clone(),
            base_url: api.base_url.clone(),
            auth: Auth::of_core(&api.auth),
            tools: u32::try_from(
                toolset
                    .tools()
                    .iter()
                    .filter(
                        |tool| matches!(&tool.route, ToolRoute::Api { api: of } if *of == api.name),
                    )
                    .count(),
            )
            .unwrap_or(u32::MAX),
        })
        .collect()
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
            auth: Auth::of_core(&server.auth),
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
        apis: apis(toolset),
        tools: toolset
            .tools()
            .iter()
            .map(|tool| ApiTool {
                name: tool.name.clone(),
                route: match tool.route {
                    ToolRoute::Mcp { .. } => Route::Mcp,
                    ToolRoute::Api { .. } => Route::Api,
                    ToolRoute::Host { .. } => Route::Host,
                    ToolRoute::Builtin(_) => Route::Builtin,
                },
                server: match &tool.route {
                    ToolRoute::Mcp { server } => Some(server.clone()),
                    _ => None,
                },
                api: match &tool.route {
                    ToolRoute::Api { api } => Some(api.clone()),
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
        workflows: toolset
            .workflows()
            .iter()
            .map(|workflow| rewind_api::workflows::PinnedWorkflow {
                name: workflow.name.clone(),
                version: workflow.version.to_hex(),
                description: workflow.description.clone(),
            })
            .collect(),
        declarations: Declarations {
            dts: stored.declarations.typescript.clone(),
            prompt: stored.declarations.prompt.clone(),
        },
        skipped: Vec::new(),
    }
}
