//! Secrets and signing keys (`archi/04-tools.md`, "Secrets" and "Signed calls";
//! `archi/07-code-architecture.md`, "HTTP API /v1").
//!
//! | Endpoint | Permission | Who may |
//! | --- | --- | --- |
//! | `GET /v1/secrets`, `…/{name}/usage` | `secrets:read` | the organization's secrets carrying the caller's labels; not an end user's token |
//! | `PUT`, `DELETE /v1/secrets/{name}` | `secrets:write` | not an end user's token; a grant limited to labels writes the secrets carrying them, and gives its labels |
//! | `GET /v1/namespaces/{name}/secrets`, `…/{secret}/usage` | `secrets:read` | a token, its own namespace |
//! | `PUT`, `DELETE /v1/namespaces/{name}/secrets/{secret}` | `secrets:write` | a token, its own namespace; not a grant or a token limited to labels |
//! | `GET /v1/organization/signing-keys` | `secrets:read` | not an end user's token |
//! | `POST /v1/organization/signing-keys` | `organization:write` | |
//! | `GET /organizations/{organization}/jwks.json` | none | `Cache-Control: max-age=300` |
//!
//! A value is written and never answered. Putting a secret again makes ready the calls parked by
//! a refusal or a missing secret in the runs that use it: the write's change of the control
//! plane reaches the owners, which resume them.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use rewind_api::access::Permission;
use rewind_api::common::{Labels, timestamp};
use rewind_api::problem::ProblemType;
use rewind_api::secrets::{
    DailyReads, Jwk, Jwks, LastRead, PutSecret, Secret as SecretView, SecretList, SecretScope,
    SecretUsage, SigningKey, SigningKeyList, WorkflowSecrets,
};
use rewind_core::RunStatus;
use rewind_core::{MAX_SECRET_VALUE_BYTES, SecretName};
use rewind_engine::ports::{
    CallClaims, CatalogStore, Day, NamespaceWrite, NewSecret, Page, Predicate, PublicKey,
    PutSecret as Put, RunOrder, RunRef, SCAN_BUDGET, Scope, Secret, SecretId, SecretMeta,
    SecretNamespace, SecretStore, TenantId, Term, WaitingFor, WorkflowFilter,
};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::Body;
use crate::http::namespaces::{deleting, name as namespace_name, reach};

/// How many days of reads a secret is listed with.
const READS_DAYS: u32 = 30;

/// The most runs a usage lists, of each kind.
const USAGE_RUNS: usize = 100;

/// How long a JWKS may be cached (`archi/04-tools.md`, "Signed calls").
const JWKS_MAX_AGE: &str = "max-age=300";

/// `GET /v1/secrets`.
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<SecretList>, ApiError> {
    caller.require(Permission::SecretsRead)?;
    organization_only(&caller)?;
    let listed = app
        .store
        .list(
            &caller.tenant,
            &SecretNamespace::Organization,
            &caller.labels,
            since(&app),
        )
        .await?;
    Ok(Json(SecretList {
        secrets: shown_all(&app, &caller, &listed).await?,
    }))
}

/// `PUT /v1/secrets/{name}`: 201 when created, 200 when replaced, with the secret's metadata.
pub(super) async fn put(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(name): Path<String>,
    Body(secret): Body<PutSecret>,
) -> Result<Response, ApiError> {
    caller.require(Permission::SecretsWrite)?;
    organization_only(&caller)?;
    let id = SecretId::organization(secret_name(&name)?);
    let labels = caller.within(secret.labels.0.clone())?;
    rewind_engine::text::refuse_nul_in_labels(&labels)?;
    // A key limited to labels writes only the secrets carrying them: it does not see the others.
    if let Some(found) = app
        .store
        .meta(&caller.tenant, &id, Day::of(app.now()))
        .await?
        && !carries(&found.labels, &caller.labels)
    {
        return Err(ApiError::forbidden(format!(
            "{} writes only the organization's secrets carrying its labels, and the secret {name} \
             exists without them",
            caller.who()
        )));
    }
    write(&app, &caller, id, secret, labels).await
}

/// `DELETE /v1/secrets/{name}`.
pub(super) async fn delete(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<Response, ApiError> {
    caller.require(Permission::SecretsWrite)?;
    organization_only(&caller)?;
    let id = SecretId::organization(secret_name(&name)?);
    seen(&app, &caller, &id).await?;
    erase(&app, &caller, &id).await
}

/// `GET /v1/secrets/{name}/usage`.
pub(super) async fn usage(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<Json<SecretUsage>, ApiError> {
    caller.require(Permission::SecretsRead)?;
    organization_only(&caller)?;
    let id = SecretId::organization(secret_name(&name)?);
    let meta = seen(&app, &caller, &id).await?;
    Ok(Json(usage_of(&app, &caller, &id, &meta).await?))
}

/// `GET /v1/namespaces/{name}/secrets`.
pub(super) async fn list_of_namespace(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(namespace): Path<String>,
) -> Result<Json<SecretList>, ApiError> {
    caller.require(Permission::SecretsRead)?;
    let namespace = namespace_name(&namespace)?;
    reach(&caller, &namespace)?;
    let listed = app
        .store
        .list(
            &caller.tenant,
            &SecretNamespace::Namespace(namespace),
            &BTreeMap::new(),
            since(&app),
        )
        .await?;
    Ok(Json(SecretList {
        secrets: shown_all(&app, &caller, &listed).await?,
    }))
}

/// `PUT /v1/namespaces/{name}/secrets/{secret}`.
pub(super) async fn put_of_namespace(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path((namespace, name)): Path<(String, String)>,
    Body(secret): Body<PutSecret>,
) -> Result<Response, ApiError> {
    caller.require(Permission::SecretsWrite)?;
    let namespace = namespace_name(&namespace)?;
    reach(&caller, &namespace)?;
    let id = SecretId::of_namespace(namespace.clone(), secret_name(&name)?);
    writes_namespaces(&caller)?;
    if !secret.labels.is_empty() {
        return Err(ApiError::invalid(
            "a namespace's secret takes no label: its namespace is its scope. Labels are the \
             organization's secrets' (PUT /v1/secrets/{name})",
        ));
    }
    if app
        .store
        .put_namespace(
            &caller.tenant,
            &namespace,
            None,
            app.config.partitions,
            app.now(),
        )
        .await?
        == NamespaceWrite::Deleting
    {
        return Err(deleting());
    }
    write(&app, &caller, id, secret, BTreeMap::new()).await
}

/// `DELETE /v1/namespaces/{name}/secrets/{secret}`.
pub(super) async fn delete_of_namespace(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    caller.require(Permission::SecretsWrite)?;
    let namespace = namespace_name(&namespace)?;
    reach(&caller, &namespace)?;
    let id = SecretId::of_namespace(namespace, secret_name(&name)?);
    writes_namespaces(&caller)?;
    erase(&app, &caller, &id).await
}

/// `GET /v1/namespaces/{name}/secrets/{secret}/usage`.
pub(super) async fn usage_of_namespace(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path((namespace, name)): Path<(String, String)>,
) -> Result<Json<SecretUsage>, ApiError> {
    caller.require(Permission::SecretsRead)?;
    let namespace = namespace_name(&namespace)?;
    reach(&caller, &namespace)?;
    let id = SecretId::of_namespace(namespace, secret_name(&name)?);
    let meta = seen(&app, &caller, &id).await?;
    Ok(Json(usage_of(&app, &caller, &id, &meta).await?))
}

/// `GET /v1/organization/signing-keys`.
pub(super) async fn signing_keys(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<SigningKeyList>, ApiError> {
    caller.require(Permission::SecretsRead)?;
    organization_only(&caller)?;
    let issuer = issuer(&app, &caller.tenant)?;
    let keys = app.store.public_keys(&caller.tenant, app.now()).await?;
    Ok(Json(SigningKeyList {
        keys: keys.iter().map(signing_key).collect(),
        jwks_url: format!("{issuer}/jwks.json"),
        issuer,
    }))
}

/// `POST /v1/organization/signing-keys`.
pub(super) async fn rotate_signing_key(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<SigningKey>, ApiError> {
    caller.require(Permission::OrganizationWrite)?;
    let key = app
        .store
        .rotate_signing_key(&caller.tenant, app.now())
        .await?;
    tracing::info!(tenant = %caller.tenant, kid = %key.public.kid, "signing key rotated");
    Ok(Json(signing_key(&key.public)))
}

/// `GET /organizations/{organization}/jwks.json`, without authentication: the current key and those
/// retired less than 24 hours ago. A tenant without a key, or that does not exist, has none.
pub(super) async fn jwks(
    State(app): State<Arc<App>>,
    Path(tenant): Path<String>,
) -> Result<Response, ApiError> {
    let keys = match TenantId::new(tenant) {
        Ok(tenant) => app.store.public_keys(&tenant, app.now()).await?,
        Err(_) => Vec::new(),
    };
    let jwks = Jwks {
        keys: keys
            .iter()
            .map(|key| {
                Jwk::ed25519(
                    key.kid.clone(),
                    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.key),
                )
            })
            .collect(),
    };
    let mut response = Json(jwks).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(JWKS_MAX_AGE),
    );
    Ok(response)
}

// -------------------------------------------------------------------------------------------
// Helpers

/// A secret's name from a path: 400 when it is not one.
fn secret_name(text: &str) -> Result<SecretName, ApiError> {
    SecretName::new(text).map_err(|error| ApiError::invalid(error.to_string()))
}

/// Refuses (403) an end user's token on the organization's secrets and keys: its
/// `secrets:read` and `secrets:write` cover its namespace's secrets only.
fn organization_only(caller: &Caller) -> Result<(), ApiError> {
    if caller.token.is_some() {
        return Err(ApiError::forbidden(
            "an end user's token reaches its own namespace's secrets only \
             (/v1/namespaces/{name}/secrets), never the organization's",
        ));
    }
    Ok(())
}

/// Refuses (403) a grant or a token limited to labels writing a namespace's secret: a
/// namespace's secret serves the end user's runs whatever their labels.
fn writes_namespaces(caller: &Caller) -> Result<(), ApiError> {
    if caller.scoped() {
        return Err(ApiError::forbidden(format!(
            "{} is limited to labels, and a namespace's secret serves the end user's runs \
             whatever their labels: write it with a key or a token without labels",
            caller.who()
        )));
    }
    Ok(())
}

/// The secret, if the caller sees it: an organization's carrying the caller's labels, or a
/// namespace's. 404 otherwise.
async fn seen(app: &App, caller: &Caller, id: &SecretId) -> Result<SecretMeta, ApiError> {
    let meta = app
        .store
        .meta(&caller.tenant, id, since(app))
        .await?
        .filter(|meta| {
            id.namespace != SecretNamespace::Organization || carries(&meta.labels, &caller.labels)
        });
    meta.ok_or_else(|| not_found(id))
}

fn not_found(id: &SecretId) -> ApiError {
    let reference = id.to_ref();
    ApiError::not_found(match &id.namespace {
        SecretNamespace::Organization => format!(
            "no secret {reference} for this caller: GET /v1/secrets lists the organization's"
        ),
        SecretNamespace::Namespace(namespace) => format!(
            "no secret {} in the namespace {namespace}: GET /v1/namespaces/{namespace}/secrets \
             lists its secrets",
            id.name
        ),
    })
}

/// Creates or replaces a secret, then records the tasks it made ready again: 201 or 200, with
/// its metadata.
async fn write(
    app: &App,
    caller: &Caller,
    id: SecretId,
    secret: PutSecret,
    labels: BTreeMap<String, String>,
) -> Result<Response, ApiError> {
    if secret.value.len() > MAX_SECRET_VALUE_BYTES {
        return Err(ApiError::new(
            ProblemType::PayloadTooLarge,
            format!(
                "the value is {} bytes: a secret holds at most {MAX_SECRET_VALUE_BYTES} bytes",
                secret.value.len()
            ),
        ));
    }
    let new = NewSecret {
        id: id.clone(),
        value: Secret::new(secret.value),
        labels,
        by: caller.writer(),
    };
    let put = app.store.put(&caller.tenant, &new, app.now()).await?;
    tracing::info!(tenant = %caller.tenant, secret = %id, "secret written");
    let meta = app
        .store
        .meta(&caller.tenant, &id, since(app))
        .await?
        .ok_or_else(|| ApiError::internal("the secret was not stored"))?;
    let status = match put {
        Put::Created => StatusCode::CREATED,
        Put::Replaced => StatusCode::OK,
    };
    Ok((status, Json(seen_by(app, caller, &meta).await?)).into_response())
}

/// Deletes a secret: 204, or 404 when it does not exist. The next call of a run that uses
/// it is parked.
async fn erase(app: &App, caller: &Caller, id: &SecretId) -> Result<Response, ApiError> {
    if !app.store.delete(&caller.tenant, id).await? {
        return Err(not_found(id));
    }
    tracing::info!(tenant = %caller.tenant, secret = %id, "secret deleted");
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// What reads a secret: the active runs that use it and those of them waiting on a call it
/// serves, which the caller sees, and the workflows whose latest version uses it.
async fn usage_of(
    app: &App,
    caller: &Caller,
    id: &SecretId,
    meta: &SecretMeta,
) -> Result<SecretUsage, ApiError> {
    let usage = users(app, caller, id).await?;
    let shown = |runs: &[RunRef]| -> Vec<String> {
        runs.iter()
            .take(USAGE_RUNS)
            .map(|run| run.id.to_string())
            .collect()
    };
    let mut workflows = Vec::new();
    let latest = app
        .catalog()
        .latest_workflows(&WorkflowFilter {
            tenant: caller.tenant.clone(),
            labels: caller.labels.clone(),
            after: None,
            limit: Some(1_000),
        })
        .await?;
    for stored in latest {
        let servers: Vec<String> = stored
            .workflow
            .secrets()
            .iter()
            .filter(|(_, secret)| {
                matches!(&id.namespace, SecretNamespace::Organization)
                    && secret.scope == rewind_core::SecretScope::Organization
                    && secret.name == id.name
            })
            .map(|(server, _)| server.to_owned())
            .collect();
        if !servers.is_empty() {
            workflows.push(WorkflowSecrets {
                name: stored.workflow.name().to_owned(),
                digest: stored.workflow.version().to_hex(),
                servers,
            });
        }
    }
    Ok(SecretUsage {
        secret: seen_by(app, caller, meta).await?,
        active_runs: shown(&usage.active),
        active_total: usage.active.len() as u64,
        waiting_runs: shown(&usage.waiting),
        waiting_total: usage.waiting.len() as u64,
        workflows,
    })
}

/// The active runs the caller sees that use a secret, and those of them parked on a call.
struct SecretUsers {
    active: Vec<RunRef>,
    waiting: Vec<RunRef>,
}

/// The runs that use a secret, among the active runs of its scope the caller sees: the
/// newest [`SCAN_BUDGET`] of the index, each run's head read for what it uses.
async fn users(app: &App, caller: &Caller, id: &SecretId) -> Result<SecretUsers, ApiError> {
    let mut query = crate::http::search::run_query(caller, None, None, app.now())?;
    query.terms.push(Term {
        negated: false,
        predicate: Predicate::Status([RunStatus::Active].into()),
    });
    let scope = match &id.namespace {
        SecretNamespace::Namespace(namespace) => Scope::Namespace {
            tenant: caller.tenant.clone(),
            namespace: namespace.clone(),
        },
        SecretNamespace::Organization => crate::http::search::scope_of(&query),
    };
    let found = app
        .edge
        .search(
            &mut app.gateway(),
            &scope,
            &query,
            RunOrder::Started,
            &Page {
                cursor: None,
                limit: 1_000,
                budget: SCAN_BUDGET,
            },
        )
        .await?;
    let mut users = SecretUsers {
        active: Vec::new(),
        waiting: Vec::new(),
    };
    for summary in found.summaries {
        let Some(view) = app.read_run(&summary.run).await? else {
            continue;
        };
        let used = view
            .head
            .metadata
            .secrets
            .iter()
            .any(|(_, secret)| SecretId::resolve(secret, &view.head.namespace) == *id);
        if !used {
            continue;
        }
        let parked = summary
            .waits
            .iter()
            .any(|wait| wait.waiting_for() == WaitingFor::Parked);
        if parked {
            users.waiting.push(summary.run.clone());
        }
        users.active.push(summary.run);
    }
    Ok(users)
}

/// The first day of the reads a secret is listed with: 30 days, today included.
fn since(app: &App) -> Day {
    let today = Day::of(app.now());
    Day(today.0.saturating_sub(READS_DAYS - 1))
}

/// Whether `labels` holds every one of `wanted`.
fn carries(labels: &BTreeMap<String, String>, wanted: &BTreeMap<String, String>) -> bool {
    wanted
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value))
}

/// A secret as `caller` sees it: its last read left out when it was by a run the caller does not
/// see (another label, another end user), which would otherwise name that run and its namespace;
/// with the handle of that run while it is kept. A registration's read names neither: every
/// caller that sees the secret sees it.
async fn seen_by(app: &App, caller: &Caller, meta: &SecretMeta) -> Result<SecretView, ApiError> {
    let mut view = view(meta);
    let restricted = !caller.labels.is_empty() || caller.namespace.is_some();
    let last_run = meta
        .last_read
        .as_ref()
        .and_then(|last| last.by.run())
        .map(|(run, _)| run);
    if let Some(last) = last_run {
        let run = RunRef::new(caller.tenant.clone(), last);
        let found = app.read_run(&run).await?;
        let sees = found
            .as_ref()
            .is_some_and(|found| caller.sees_run(&found.head));
        if restricted && !sees {
            view.last_read = None;
        } else if let (Some(read), Some(found)) = (view.last_read.as_mut(), found) {
            read.run_handle = Some(found.head.handle().to_string());
        }
    }
    Ok(view)
}

async fn shown_all(
    app: &App,
    caller: &Caller,
    listed: &[SecretMeta],
) -> Result<Vec<SecretView>, ApiError> {
    let mut all = Vec::with_capacity(listed.len());
    for meta in listed {
        all.push(seen_by(app, caller, meta).await?);
    }
    Ok(all)
}

/// A secret, as the API lists it.
pub(crate) fn view(meta: &SecretMeta) -> SecretView {
    let (scope, namespace) = match &meta.id.namespace {
        SecretNamespace::Organization => (SecretScope::Org, None),
        SecretNamespace::Namespace(namespace) => (
            SecretScope::Ns,
            rewind_api::namespaces::NamespaceName::new(namespace.as_str()).ok(),
        ),
    };
    SecretView {
        name: meta.id.name.to_string(),
        scope,
        namespace,
        reference: meta.id.to_ref().to_string(),
        labels: Labels(meta.labels.clone()),
        created_by: meta.created_by.clone(),
        created_at: timestamp(meta.created_at),
        updated_by: meta.updated_by.clone(),
        updated_at: timestamp(meta.updated_at),
        reads: meta
            .reads
            .iter()
            .map(|day| DailyReads {
                day: day.day.to_string(),
                reads: day.reads,
            })
            .collect(),
        last_read: meta.last_read.as_ref().map(|last| {
            let (run, namespace) = match last.by.run() {
                Some((run, namespace)) => (
                    Some(run.to_string()),
                    rewind_api::namespaces::NamespaceName::new(namespace.as_str()).ok(),
                ),
                None => (None, None),
            };
            LastRead {
                run,
                // The run's record names it: `seen_by` reads it.
                run_handle: None,
                namespace,
                at: timestamp(last.at),
            }
        }),
    }
}

fn signing_key(key: &PublicKey) -> SigningKey {
    SigningKey {
        kid: key.kid.clone(),
        created_at: timestamp(key.created_at),
        retired_at: key.retired_at.map(timestamp),
    }
}

/// The issuer of the tenant's signed calls: 409 on a server without a public URL, whose keys
/// no server can fetch.
fn issuer(app: &App, tenant: &TenantId) -> Result<String, ApiError> {
    let public_url = app.settings.public_url.as_deref().ok_or_else(|| {
        ApiError::unavailable(
            "this server has no public URL (--public-url): the servers that check its signed \
             calls cannot fetch its public keys, so it signs none",
        )
    })?;
    Ok(CallClaims::issuer(public_url, tenant))
}
