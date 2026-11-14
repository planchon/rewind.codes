//! Administration: API keys, who calls, the catalog of permissions, the server itself.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rewind_api::access::{
    ApiKey, ApiKeyList, CreateApiKey, Grant, OPERATOR, Permission, PermissionList,
};
use rewind_api::admin::ServerInfo;
use rewind_api::common::{Labels, timestamp};
use rewind_api::organization::{Identity, Principal as CallerPrincipal};
use rewind_core::Digest;
use rewind_engine::ports::{self, AccessStore, JournalStore, Principal, Tenant, TenantId};
use rewind_engine::text::{refuse_nul, refuse_nul_in_labels};

use crate::app::App;
use crate::auth::{Caller, key_hash, listed, new_key, role_permissions};
use crate::error::ApiError;
use crate::extract::Body;

/// A key as the API shows it: with its grant's role and labels, and its secret only in the
/// answer to its creation. A key without a grant (revoked) has the role `""`.
fn key_view(key: &ports::ApiKey, grant: Option<&ports::Grant>, secret: Option<String>) -> ApiKey {
    ApiKey {
        id: key.hash.to_hex(),
        tenant: key.tenant.as_str().to_owned(),
        name: key.name.clone(),
        role: grant.map(|grant| grant.role.clone()).unwrap_or_default(),
        allowed_labels: grant
            .map(|grant| Labels::from(grant.allowed_labels.clone()))
            .unwrap_or_default(),
        created_by: key.created_by.as_ref().map(ToString::to_string),
        created_at: timestamp(key.created_at),
        revoked_at: key.revoked_at.map(timestamp),
        key: secret,
    }
}

/// `GET /v1/api-keys`: the tenant's keys, revoked ones included, oldest first, each with its
/// grant.
pub(super) async fn list_keys(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<ApiKeyList>, ApiError> {
    caller.require(Permission::ApiKeysRead)?;
    let keys = app.store.list_keys(&caller.tenant).await?;
    let grants: HashMap<Principal, ports::Grant> = app
        .store
        .list_grants(&caller.tenant)
        .await?
        .into_iter()
        .filter(|grant| matches!(grant.principal, Principal::Key(_)))
        .map(|grant| (grant.principal.clone(), grant))
        .collect();
    let keys = keys
        .iter()
        .map(|key| key_view(key, grants.get(&key.principal()), None))
        .collect();
    Ok(Json(ApiKeyList { keys }))
}

/// `POST /v1/api-keys`: a key and its grant, in the caller's tenant unless the operator names
/// another, created with its first key. No one gives more than they hold: the caller holds every
/// permission of the role given, and only the operator gives `operator`. The key itself is only
/// in this answer.
pub(super) async fn create_key(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<CreateApiKey>,
) -> Result<Response, ApiError> {
    caller.require(Permission::ApiKeysWrite)?;
    let tenant = match request.tenant {
        Some(tenant) => {
            TenantId::new(tenant).map_err(|error| ApiError::invalid(error.to_string()))?
        }
        None => caller.tenant.clone(),
    };
    if tenant != caller.tenant && !caller.operator() {
        return Err(ApiError::forbidden(format!(
            "only the deployment's operator creates keys in another tenant: leave `tenant` out \
             to create a key of {}",
            caller.tenant
        )));
    }
    let name = request.name.trim();
    if name.is_empty() {
        return Err(ApiError::invalid(
            "name: a key needs a name that says what it is for",
        ));
    }
    refuse_nul("the name", name)?;
    refuse_nul_in_labels(&request.allowed_labels.0)?;
    if request.role == OPERATOR && !caller.operator() {
        return Err(ApiError::forbidden(
            "only the deployment's operator creates a key with the role operator",
        ));
    }
    let Some(permissions) = role_permissions(&app.store, &tenant, &request.role).await? else {
        return Err(ApiError::invalid(format!(
            "role: no role {:?} in the tenant {tenant}: expected owner, harness, worker, reader \
             or a role of the tenant",
            request.role
        )));
    };
    let lacking = caller.lacks(&permissions);
    if !lacking.is_empty() {
        return Err(ApiError::forbidden(format!(
            "the role {} gives {}, which the caller does not hold: no one gives more than they \
             hold, so choose a role whose permissions the caller holds",
            request.role,
            listed(&lacking)
        )));
    }
    ensure_tenant(&app, &tenant).await?;
    let secret = new_key()?;
    let hash = key_hash(&secret);
    let now = app.now();
    let key = ports::ApiKey {
        hash,
        tenant: tenant.clone(),
        name: name.to_owned(),
        created_by: caller.principal.clone(),
        created_at: now,
        revoked_at: None,
    };
    app.store.put_key(&key).await?;
    let grant = ports::Grant {
        tenant,
        principal: key.principal(),
        role: request.role,
        allowed_labels: request.allowed_labels.0,
        granted_by: caller.principal.clone(),
        granted_at: now,
    };
    if let Err(error) = app.store.put_grant(&grant, None).await {
        // A key without its grant would hold nothing: it is revoked, and the caller retries.
        if let Err(revoking) = app.store.revoke_key(hash, now).await {
            tracing::warn!(key = %hash.short(), error = %revoking, "a key without a grant was not revoked");
        }
        return Err(error.into());
    }
    let view = key_view(&key, Some(&grant), Some(secret));
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

/// A tenant gets its record, with the deployment's quota and retention, when its first key is
/// created.
pub(crate) async fn ensure_tenant(app: &App, tenant: &TenantId) -> Result<(), ApiError> {
    if app.store.get_tenant(tenant).await?.is_none() {
        app.store
            .put_tenant(&Tenant {
                id: tenant.clone(),
                quotas: rewind_engine::ports::Quotas {
                    active_runs: app.settings.run_quota,
                    ..Default::default()
                },
                namespace_defaults: rewind_engine::ports::Quotas::default(),
                retention: app.settings.retention,
            })
            .await?;
    }
    Ok(())
}

/// `DELETE /v1/api-keys/{id}`: revokes a key of the caller's tenant (the operator's: of any
/// tenant) and deletes its grant. Taking a grant away needs every permission of its role, and
/// only the operator revokes an operator key.
pub(super) async fn revoke_key(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::ApiKeysWrite)?;
    let missing = || {
        ApiError::not_found(format!(
            "no API key {id} in this tenant, or it is already revoked: GET /v1/api-keys lists \
             the tenant's keys"
        ))
    };
    let hash = Digest::from_hex(&id).map_err(|_| missing())?;
    let key = app
        .store
        .find_key(hash)
        .await?
        .filter(|key| key.revoked_at.is_none())
        .filter(|key| key.tenant == caller.tenant || caller.operator())
        .ok_or_else(missing)?;
    let principal = key.principal();
    if let Some(grant) = app.store.grant(&key.tenant, &principal).await? {
        if grant.role == OPERATOR && !caller.operator() {
            return Err(ApiError::forbidden(
                "this key has the role operator: only the deployment's operator revokes it",
            ));
        }
        let permissions = role_permissions(&app.store, &key.tenant, &grant.role)
            .await?
            .unwrap_or_default();
        let lacking = caller.lacks(&permissions);
        if !lacking.is_empty() {
            return Err(ApiError::forbidden(format!(
                "revoking this key takes away its grant of the role {}, which gives {}: the \
                 caller does not hold them, and no one takes away more than they hold",
                grant.role,
                listed(&lacking)
            )));
        }
    }
    if !app.store.revoke_key(hash, app.now()).await? {
        return Err(missing());
    }
    app.store
        .delete_grant(&key.tenant, &principal, None)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/whoami`: any caller.
pub(super) async fn whoami(caller: Caller) -> Json<Identity> {
    let principal = match &caller.principal {
        Some(Principal::Key(hash)) => Some(CallerPrincipal::Key { id: hash.to_hex() }),
        _ => None,
    };
    let operator = caller.operator();
    let permissions = Permission::ALL
        .into_iter()
        .filter(|permission| caller.permissions.contains(permission))
        .collect();
    let grant = caller.role.map(|role| Grant {
        role,
        allowed_labels: Labels::from(caller.labels),
        granted_by: caller.granted_by.as_ref().map(ToString::to_string),
        granted_at: caller.granted_at.map(timestamp),
    });
    Json(Identity {
        tenant: Some(caller.tenant.as_str().to_owned()),
        principal,
        grant,
        permissions,
        operator,
        organization: None,
        organizations: Vec::new(),
    })
}

/// `GET /v1/permissions`: the catalog, to any caller.
pub(super) async fn permissions(_caller: Caller) -> Json<PermissionList> {
    Json(PermissionList::catalog())
}

/// `GET /v1/server`: any caller.
pub(super) async fn server(State(app): State<Arc<App>>, _caller: Caller) -> Json<ServerInfo> {
    Json(ServerInfo {
        version: app.settings.version.to_owned(),
        epochs: app.engine.epochs().iter().map(|epoch| epoch.0).collect(),
        roles: app.settings.roles.clone(),
        embedded: app.settings.embedded,
        virtual_time: app.engine.clock().is_virtual(),
    })
}

/// `POST /v1/server/stop`: the operator. Only the embedded server stops this way, once its work
/// in flight is committed.
pub(super) async fn stop(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<StatusCode, ApiError> {
    caller.require_operator("stop the server")?;
    if !app.settings.embedded {
        return Err(ApiError::forbidden(
            "only the embedded server stops through the API: stop this server as it was started",
        ));
    }
    tracing::info!("stop requested through the API");
    app.stop.send_replace(true);
    Ok(StatusCode::ACCEPTED)
}
