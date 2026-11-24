//! Action sets: registering and pinning them, their declarations, search (`archi/04-tools.md`,
//! "Action sets and code generation", "API actions", "Authenticating calls").
//!
//! Registering connects to the MCP servers and lists their actions, and reads the APIs' OpenAPI
//! documents; each server's and API's `auth` is pinned with it; the harness's policies, its
//! harness actions and the `http` built-in (with its allow-list) are added, and so is the `webhook`
//! built-in, always: every code may call `signalUrl()`. The pinned action set is content-addressed,
//! so registering the same action set again gives the same id. It holds no secret.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::access::Permission;
use rewind_api::action_sets::{
    Action as ApiAction, ActionMatch, ActionPolicy as ApiPolicy, ActionSearchResults,
    ActionSet as ApiActionSet, ActionSetList, ActionSetSummary, Api as ApiDeclared, ApiSummary,
    Auth, CreateActionSet, DeclarationFormat, Declarations, DeclarationsQuery, DeclarationsText,
    McpTransport as ApiTransport, OpenApiDocument, Route, SearchQuery, ServerSummary,
    SkippedOperation,
};
use rewind_core::{
    Action, ActionAuth, ActionPolicy, ActionRoute, ActionSet, Api, Digest, McpServer, McpTransport,
    SecretRef, SecretScope,
};
use rewind_engine::ports::{
    ActionDeclarations, CatalogStore, Day, Secret, SecretId, SecretNamespace, SecretReader,
    SecretStore, StoredActionSet, TenantId,
};
use rewind_engine::secrets::{RefusedSecrets, SecretProblem};
use rewind_engine::text::{ACTION_SET_TEXT, refuse_nul};
use rewind_mcp::ListingAuth;
use rewind_openapi::{ReadOptions, read};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::{Body, Query};
use crate::http::workflows;
use crate::wiring::actions::DiscoveryError;

/// `POST /v1/action-sets`.
pub(super) async fn create(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(mut request): Body<CreateActionSet>,
) -> Result<Response, ApiError> {
    caller.require(Permission::ActionsWrite)?;
    let references = std::mem::take(&mut request.workflows);
    let (action_set, skipped) = match request.base.take() {
        Some(base) => {
            if !request.mcp.is_empty()
                || !request.apis.is_empty()
                || !request.harness.is_empty()
                || request.http.is_some()
            {
                return Err(invalid_action_set(
                    "an action set derived from a base keeps its servers and actions: give only \
                     workflows with base",
                ));
            }
            (
                stored(&app, &caller.tenant, &base).await?.action_set,
                Vec::new(),
            )
        }
        None => pin(&app, &caller, request).await?,
    };
    let action_set = workflows::pin(
        &app,
        &caller.tenant,
        &caller.labels,
        &references,
        action_set,
    )
    .await?;
    let names: Vec<String> = action_set
        .actions()
        .iter()
        .map(|action| action.name.clone())
        .collect();
    let observed = app
        .edge
        .shapes(
            &mut app.gateway(),
            &rewind_engine::ports::Scope::Tenant(caller.tenant.clone()),
            &names,
        )
        .await?;
    let generated = app.actions.declarations(&action_set, &observed);
    let declarations = ActionDeclarations {
        typescript: generated.typescript,
        prompt: generated.prompt,
    };
    refuse_nul(ACTION_SET_TEXT, &declarations.typescript)?;
    refuse_nul(ACTION_SET_TEXT, &declarations.prompt)?;
    let created = app
        .store
        .get_action_set(&caller.tenant, action_set.digest())
        .await?
        .is_none();
    app.store
        .put_action_set(&caller.tenant, &action_set, &declarations)
        .await?;
    let stored = app
        .store
        .get_action_set(&caller.tenant, action_set.digest())
        .await?
        .ok_or_else(|| ApiError::internal("the action set was not stored"))?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let mut answer = view(&stored);
    answer.skipped = skipped;
    Ok((status, Json(answer)).into_response())
}

/// `GET /v1/action-sets`.
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<ActionSetList>, ApiError> {
    caller.require(Permission::ActionsRead)?;
    let action_sets = app
        .store
        .list_action_sets(&caller.tenant)
        .await?
        .iter()
        .map(|stored| ActionSetSummary {
            id: stored.action_set.digest().to_hex(),
            servers: servers(&stored.action_set),
            apis: apis(&stored.action_set),
            actions: stored
                .action_set
                .actions()
                .iter()
                .map(|action| action.name.clone())
                .collect(),
            workflows: stored
                .action_set
                .workflows()
                .iter()
                .map(|workflow| workflow.name.clone())
                .collect(),
        })
        .collect();
    Ok(Json(ActionSetList { action_sets }))
}

/// `GET /v1/action-sets/{id}/declarations`.
pub(super) async fn declarations(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<DeclarationsQuery>,
) -> Result<Json<DeclarationsText>, ApiError> {
    caller.require(Permission::ActionsRead)?;
    let stored = stored(&app, &caller.tenant, &id).await?;
    let format = query.format.unwrap_or_default();
    let text = match format {
        DeclarationFormat::Dts => stored.declarations.typescript,
        DeclarationFormat::Prompt => stored.declarations.prompt,
    };
    Ok(Json(DeclarationsText { format, text }))
}

/// `GET /v1/action-sets/{id}/search`: the actions whose name or description holds the most words
/// of the query, best first.
pub(super) async fn search(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<ActionSearchResults>, ApiError> {
    caller.require(Permission::ActionsRead)?;
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
        .action_set
        .actions()
        .iter()
        .map(|action| action.name.clone())
        .collect();
    let observed = app
        .edge
        .shapes(
            &mut app.gateway(),
            &rewind_engine::ports::Scope::Tenant(caller.tenant.clone()),
            &names,
        )
        .await?;
    let generated = app.actions.declarations(&stored.action_set, &observed);
    let declared: BTreeMap<&str, _> = generated
        .actions
        .iter()
        .map(|action| (action.name.as_str(), action))
        .collect();
    // The actions, and the workflows as `workflows.<name>`, with their descriptions.
    let candidates: Vec<(String, String)> = stored
        .action_set
        .actions()
        .iter()
        .map(|action| {
            (
                action.name.clone(),
                action.description.clone().unwrap_or_default(),
            )
        })
        .chain(stored.action_set.workflows().iter().map(|workflow| {
            (
                format!("{}.{}", rewind_core::WORKFLOWS_GROUP, workflow.name),
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
    scored
        .sort_by(|(a, action_a), (b, action_b)| b.cmp(a).then_with(|| action_a.0.cmp(&action_b.0)));
    let actions = scored
        .into_iter()
        .take(limit)
        .map(|(_, (name, description))| {
            let declared = declared.get(name.as_str());
            ActionMatch {
                name: name.clone(),
                description: description.clone(),
                prompt: declared.map(|d| d.prompt.clone()).unwrap_or_default(),
                dts: declared.map(|d| d.typescript.clone()).unwrap_or_default(),
            }
        })
        .collect();
    Ok(Json(ActionSearchResults { actions }))
}

/// How the API's `auth` pins (`archi/04-tools.md`, "Authenticating calls"); none when absent.
fn auth_of(auth: Option<&Auth>, of: &str) -> Result<ActionAuth, ApiError> {
    auth.map_or(Ok(ActionAuth::None), |auth| {
        auth.to_core()
            .map_err(|message| invalid_action_set(format!("{of}: {message}")))
    })
}

pub(crate) fn parse_digest(id: &str) -> Result<Digest, ApiError> {
    Digest::from_hex(id).map_err(|_| {
        ApiError::not_found(format!(
            "no action set {id:?}: an action set's id is the digest POST /v1/action-sets returned"
        ))
    })
}

/// A stored action set of the tenant, or 404.
pub(crate) async fn stored(
    app: &App,
    tenant: &TenantId,
    id: &str,
) -> Result<StoredActionSet, ApiError> {
    let digest = parse_digest(id)?;
    app.store
        .get_action_set(tenant, digest)
        .await?
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no action set {id} for this tenant: register it with POST /v1/action-sets"
            ))
        })
}

fn invalid_action_set(detail: impl Into<String>) -> ApiError {
    ApiError::new(ProblemType::InvalidActionSet, detail)
}

/// Pins an action set: discovery of its MCP servers, its APIs' operations, the harness's policies,
/// harness actions and built-ins. The operations of its APIs left out are answered beside it.
async fn pin(
    app: &App,
    caller: &Caller,
    request: CreateActionSet,
) -> Result<(ActionSet, Vec<SkippedOperation>), ApiError> {
    let tenant = &caller.tenant;
    let mut listing = if request.secrets.is_empty() {
        BTreeMap::new()
    } else {
        secrets_for_listing(app, caller, &request).await?
    };
    let (apis, mut actions, skipped) = read_apis(app, &request).await?;
    let mut servers = Vec::new();
    let mut names = BTreeSet::new();
    for server in &request.mcp {
        if !names.insert(server.name.clone()) {
            return Err(invalid_action_set(format!(
                "the MCP server {:?} is declared twice",
                server.name
            )));
        }
        let transport = match (&server.url, &server.command) {
            (Some(url), None) => {
                if !(url.starts_with("https://") || url.starts_with("http://")) {
                    return Err(invalid_action_set(format!(
                        "MCP server {}: url must be http:// or https://",
                        server.name
                    )));
                }
                McpTransport::Http { url: url.clone() }
            }
            (None, Some(command)) => {
                if !app.settings.allow_stdio {
                    return Err(invalid_action_set(format!(
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
                return Err(invalid_action_set(format!(
                    "MCP server {}: give exactly one of url and command",
                    server.name
                )));
            }
        };
        let mut pinned = McpServer::new(server.name.clone(), transport);
        pinned.auth = auth_of(server.auth.as_ref(), &format!("MCP server {}", server.name))?;
        let secret = listing.remove(&server.name);
        let named = secret.is_some();
        let auth = listing_auth(app, tenant, &pinned, secret).await?;
        let listed = app.actions.discover(tenant, &pinned, auth).await.map_err(
            |DiscoveryError(message)| {
                let hint = if pinned.auth.takes_secret() && !named {
                    format!(
                        "{message} (the listing carried no secret: use an organization's \
                         secret for {} in secrets)",
                        pinned.name
                    )
                } else {
                    message
                };
                ApiError::new(ProblemType::ActionSetUnavailable, hint)
            },
        )?;
        if let Some(unknown) = server.policies.keys().find(|name| {
            !listed
                .iter()
                .any(|action| action.local_name() == name.as_str())
        }) {
            return Err(invalid_action_set(format!(
                "MCP server {}: a policy names {unknown:?}, which the server does not have",
                server.name
            )));
        }
        for mut action in listed {
            if let Some(policy) = server.policies.get(action.local_name()) {
                action.policy = policy_of(&action.route, *policy, &action.name)?;
            }
            actions.push(action);
        }
        servers.push(pinned);
    }
    for harness in request.harness {
        if !harness.input_schema.is_object() {
            return Err(invalid_action_set(format!(
                "harness action {}: inputSchema must be a JSON Schema object",
                harness.name
            )));
        }
        // The harness writes these schemas: one that cannot be compiled is refused here, rather
        // than refusing every call of the action later (an MCP server's own schemas are taken as
        // listed, `archi/08-decisions.md`).
        let schemas = [
            Some(("inputSchema", &harness.input_schema)),
            harness
                .output_schema
                .as_ref()
                .map(|schema| ("outputSchema", schema)),
        ];
        for (field, schema) in schemas.into_iter().flatten() {
            if let Some(error) = rewind_engine::schema::Validator::compile(schema).unusable() {
                return Err(invalid_action_set(format!(
                    "harness action {}: {field} is not a usable JSON Schema: {error}",
                    harness.name
                )));
            }
        }
        let group = harness
            .name
            .split_once('.')
            .map(|(group, _)| group.to_owned());
        let route = ActionRoute::Harness { group };
        let mut action = Action::new(harness.name.clone(), route.clone(), harness.input_schema);
        action.description = Some(harness.description);
        action.output_schema = harness.output_schema;
        if let Some(policy) = harness.policy {
            action.policy = policy_of(&route, policy, &harness.name)?;
        }
        actions.push(action);
    }
    if let Some(http) = request.http {
        if http.allow.is_empty() {
            return Err(invalid_action_set(
                "http: allow needs at least one host; without one there is no http action",
            ));
        }
        let mut action = app.actions.http_action(http.allow);
        if let Some(policy) = http.policy {
            action.policy = policy_of(&action.route.clone(), policy, "http")?;
        }
        actions.push(action);
    }
    actions.push(app.actions.signal_url_action());
    let action_set = ActionSet::from_parts(servers, apis, actions)
        .map_err(|error| invalid_action_set(error.to_string()))?;
    Ok((action_set, skipped))
}

/// The organization's secrets `secrets` uses for the listings, read, by server
/// (`archi/04-tools.md`, "Secrets": `POST /v1/action-sets` uses an organization's secrets only,
/// to list the actions of an MCP server that needs one; no run keeps them). The caller holds
/// `secrets:use`, and each secret is an organization's it sees (its labels), used for an
/// MCP server of the request whose `auth` takes a secret. A read is counted as a registration's,
/// which names no run.
async fn secrets_for_listing(
    app: &App,
    caller: &Caller,
    request: &CreateActionSet,
) -> Result<BTreeMap<String, Secret>, ApiError> {
    caller.require(Permission::SecretsUse)?;
    let mut problems = Vec::new();
    let mut named = Vec::new();
    for (server, text) in &request.secrets {
        let secret = SecretRef::parse(text)
            .map_err(|error| ApiError::invalid(format!("secrets.{server}: {error}")))?;
        if secret.scope != SecretScope::Organization {
            return Err(ApiError::invalid(format!(
                "secrets.{server}: {secret} is a namespace's secret, and a registration has no \
                 namespace: use an organization's secret (org/<name>) to list the actions"
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
            problems.push(SecretProblem::TakesNone {
                server: server.clone(),
            });
            continue;
        }
        named.push((server.clone(), secret));
    }
    let today = Day::of(app.now());
    let mut read = BTreeMap::new();
    let reader = SecretReader::Registration;
    for (server, secret) in named {
        let id = SecretId::organization(secret.name.clone());
        let seen = app
            .store
            .meta(&caller.tenant, &id, today)
            .await?
            .is_some_and(|meta| {
                caller
                    .labels
                    .iter()
                    .all(|(key, value)| meta.labels.get(key) == Some(value))
            });
        let value = if seen {
            app.store
                .read(&caller.tenant, &id, &reader, app.now())
                .await?
        } else {
            None
        };
        match value {
            Some(value) => {
                read.insert(server, value);
            }
            None => problems.push(SecretProblem::Unknown { server, secret }),
        }
    }
    if problems.is_empty() {
        return Ok(read);
    }
    problems.sort_by(|a, b| a.server().cmp(b.server()));
    let listed = app
        .store
        .list(
            &caller.tenant,
            &SecretNamespace::Organization,
            &caller.labels,
            today,
        )
        .await?;
    let mut usable: Vec<SecretRef> = listed.iter().map(|meta| meta.id.to_ref()).collect();
    usable.truncate(rewind_engine::secrets::MAX_USABLE);
    let refused = RefusedSecrets { problems, usable };
    Err(ApiError::secret_required(refused.to_string(), &refused))
}

/// How the listing of a server's actions authenticates at registration (`archi/04-tools.md`,
/// "Authenticating calls"): the organization's secret used for it, if any; or a JWT signed
/// with the tenant's key, made with the first action set that needs it, without the claims of a
/// call (`iss`, `aud`, `jti`, `htm`, `htu`, `body`, `iat`, `exp`).
async fn listing_auth(
    app: &App,
    tenant: &TenantId,
    server: &McpServer,
    secret: Option<Secret>,
) -> Result<ListingAuth, ApiError> {
    Ok(match (&server.auth, secret) {
        (ActionAuth::Secret(_), Some(secret)) => ListingAuth::Secret(secret),
        (ActionAuth::Rewind, _) => ListingAuth::Signed {
            key: app.store.signing_key(tenant, app.now()).await?,
            issuer: issuer(app, tenant, &format!("MCP server {}", server.name))?,
        },
        _ => ListingAuth::None,
    })
}

/// The issuer of the tenant's JWTs: `{public-url}/organizations/{organization}`, whose keys are at
/// `{iss}/jwks.json`. A server without a public URL cannot sign: no one could fetch its keys.
fn issuer(app: &App, tenant: &TenantId, of: &str) -> Result<String, ApiError> {
    match &app.settings.public_url {
        Some(public_url) => Ok(format!("{public_url}/organizations/{}", tenant.as_str())),
        None => Err(ApiError::new(
            ProblemType::ActionSetUnavailable,
            format!(
                "{of} checks Rewind's signature, and this server has no public URL \
                 (--public-url) where its keys are published: the issuer of its calls is \
                 {{public-url}}/organizations/{}",
                tenant.as_str()
            ),
        )),
    }
}

/// Reads each API's OpenAPI document into its actions (`archi/04-tools.md`, "API actions"), with
/// its `auth` or the document's security requirement, its policies, and the operations it
/// leaves out (`skipped`). A document given by URL is read once, through the egress checks;
/// what an internal server answered is not quoted.
async fn read_apis(
    app: &App,
    request: &CreateActionSet,
) -> Result<(Vec<Api>, Vec<Action>, Vec<SkippedOperation>), ApiError> {
    let mut apis = Vec::new();
    let mut actions = Vec::new();
    let mut skipped = Vec::new();
    for declared in &request.apis {
        let (read_api, internal) = read_api(app, declared).await?;
        let read_api = read_api.map_err(|error| {
            invalid_action_set(if internal {
                format!(
                    "API {}: its OpenAPI document, read from an internal address, cannot be \
                     read as an API; the answer of an internal server is not quoted",
                    declared.name
                )
            } else {
                error.to_string()
            })
        })?;
        if let Some(unknown) = declared.policies.keys().find(|name| {
            !read_api
                .actions
                .iter()
                .any(|action| action.local_name() == name.as_str())
        }) {
            return Err(invalid_action_set(format!(
                "API {}: a policy names {unknown:?}, which it does not have",
                declared.name
            )));
        }
        for mut action in read_api.actions {
            if let Some(policy) = declared.policies.get(action.local_name()) {
                action.policy = policy_over(action.policy, *policy, &action.name)?;
            }
            actions.push(action);
        }
        skipped.extend(
            read_api
                .skipped
                .into_iter()
                .map(|operation| SkippedOperation {
                    api: declared.name.clone(),
                    method: operation.method,
                    path: operation.path,
                    operation_id: operation.operation_id,
                    reason: operation.reason,
                }),
        );
        apis.push(read_api.api);
    }
    Ok((apis, actions, skipped))
}

/// One API read from its document: what was read, and whether the document came from an
/// internal address.
async fn read_api(
    app: &App,
    declared: &ApiDeclared,
) -> Result<
    (
        Result<rewind_openapi::ReadApi, rewind_openapi::ReadError>,
        bool,
    ),
    ApiError,
> {
    let auth = declared
        .auth
        .as_ref()
        .map(|auth| {
            auth.to_core()
                .map_err(|message| invalid_action_set(format!("API {}: {message}", declared.name)))
        })
        .transpose()?;
    let mut options = ReadOptions::new(declared.name.clone());
    options.base_url = declared.base_url.clone();
    options.auth = auth;
    options.operations = declared.operations.clone();
    options.concurrency = declared.concurrency;
    options.allow_http = app.settings.embedded;
    Ok(match &declared.openapi {
        OpenApiDocument::Document(document) => (read(document, &options), false),
        OpenApiDocument::Url(url) => {
            let fetched = app.actions.fetch_document(url).await.map_err(|error| {
                ApiError::new(
                    ProblemType::ActionSetUnavailable,
                    format!("API {}: {error}", declared.name),
                )
            })?;
            options.document_url = Some(fetched.url.clone());
            (read(&fetched.document, &options), fetched.internal)
        }
    })
}

/// A policy of the API, with the route's defaults for what it leaves out.
fn policy_of(
    route: &ActionRoute,
    policy: ApiPolicy,
    action: &str,
) -> Result<ActionPolicy, ApiError> {
    policy_over(ActionPolicy::default_for(route), policy, action)
}

/// A policy of the API over `pinned`, the action's default: its route's, or for an API's
/// operation its method's.
fn policy_over(
    mut pinned: ActionPolicy,
    policy: ApiPolicy,
    action: &str,
) -> Result<ActionPolicy, ApiError> {
    if let Some(idempotent) = policy.idempotent {
        pinned.idempotent = idempotent;
    }
    if let Some(approval) = policy.approval {
        pinned.approval = approval;
    }
    if let Some(timeout) = policy.timeout {
        if timeout.as_millis() == 0 {
            return Err(invalid_action_set(format!(
                "{action}: timeout must be positive"
            )));
        }
        pinned.execution_timeout = Some(timeout.get());
    }
    if let Some(reconnect) = policy.reconnect_timeout {
        if reconnect.get() < Duration::from_secs(1) {
            return Err(invalid_action_set(format!(
                "{action}: reconnectTimeout must be at least 1s"
            )));
        }
        pinned.reconnect_timeout = reconnect.get();
    }
    if let Some(concurrency) = policy.concurrency {
        if concurrency == 0 {
            return Err(invalid_action_set(format!(
                "{action}: concurrency must be at least 1"
            )));
        }
        pinned.concurrency = concurrency;
    }
    Ok(pinned)
}

fn apis(action_set: &ActionSet) -> Vec<ApiSummary> {
    action_set
        .apis()
        .iter()
        .map(|api| ApiSummary {
            name: api.name.clone(),
            base_url: api.base_url.clone(),
            auth: Auth::of_core(&api.auth),
            actions: u32::try_from(
                action_set
                    .actions()
                    .iter()
                    .filter(
                        |action| matches!(&action.route, ActionRoute::Api { api: of } if *of == api.name),
                    )
                    .count(),
            )
            .unwrap_or(u32::MAX),
        })
        .collect()
}

fn servers(action_set: &ActionSet) -> Vec<ServerSummary> {
    action_set
        .servers()
        .iter()
        .map(|server| ServerSummary {
            name: server.name.clone(),
            transport: match server.transport {
                McpTransport::Http { .. } => ApiTransport::Http,
                McpTransport::Stdio { .. } => ApiTransport::Stdio,
            },
            auth: Auth::of_core(&server.auth),
            actions: u32::try_from(
                action_set
                    .actions()
                    .iter()
                    .filter(|action| matches!(&action.route, ActionRoute::Mcp { server: of } if *of == server.name))
                    .count(),
            )
            .unwrap_or(u32::MAX),
        })
        .collect()
}

/// A stored action set, as the API shows it.
pub(crate) fn view(stored: &StoredActionSet) -> ApiActionSet {
    let action_set = &stored.action_set;
    ApiActionSet {
        id: action_set.digest().to_hex(),
        servers: servers(action_set),
        apis: apis(action_set),
        actions: action_set
            .actions()
            .iter()
            .map(|action| ApiAction {
                name: action.name.clone(),
                route: match action.route {
                    ActionRoute::Mcp { .. } => Route::Mcp,
                    ActionRoute::Api { .. } => Route::Api,
                    ActionRoute::Harness { .. } => Route::Harness,
                    ActionRoute::Builtin(_) => Route::Builtin,
                },
                server: match &action.route {
                    ActionRoute::Mcp { server } => Some(server.clone()),
                    _ => None,
                },
                api: match &action.route {
                    ActionRoute::Api { api } => Some(api.clone()),
                    _ => None,
                },
                description: action.description.clone().unwrap_or_default(),
                input_schema: action.input_schema.clone(),
                output_schema: action.output_schema.clone(),
                policy: ApiPolicy {
                    idempotent: Some(action.policy.idempotent),
                    approval: Some(action.policy.approval),
                    timeout: action.policy.execution_timeout.map(Into::into),
                    reconnect_timeout: Some(action.policy.reconnect_timeout.into()),
                    concurrency: Some(action.policy.concurrency),
                },
            })
            .collect(),
        workflows: action_set
            .workflows()
            .iter()
            .map(|workflow| rewind_api::workflows::PinnedWorkflow {
                name: workflow.name.clone(),
                digest: workflow.version.to_hex(),
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
