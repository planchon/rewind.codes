//! Administration: API keys, who a key is, the server itself.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rewind_api::access::{ApiKey, CreateApiKey, Grant, fixed_permissions};
use rewind_api::admin::ServerInfo;
use rewind_api::common::{Labels, timestamp};
use rewind_api::organization::{Identity, Principal};
use rewind_core::Digest;
use rewind_engine::ports::{JournalStore, Tenant, TenantId};
use rewind_engine::text::refuse_nul_in_labels;

use crate::app::App;
use crate::auth::{Access, Caller, key_hash, new_key};
use crate::error::ApiError;
use crate::extract::Body;

/// `POST /v1/api-keys`: a key for a tenant, the caller's unless it names another, created with
/// its first key. The key itself is only in this answer.
pub(super) async fn create_key(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<CreateApiKey>,
) -> Result<Response, ApiError> {
    caller.allow(Access::Admin)?;
    let tenant = match request.tenant {
        Some(tenant) => {
            TenantId::new(tenant).map_err(|error| ApiError::invalid(error.to_string()))?
        }
        None => caller.tenant.clone(),
    };
    if fixed_permissions(&request.role).is_none() {
        return Err(ApiError::invalid(format!(
            "role: unknown role {:?}: expected owner, harness, worker, reader or operator",
            request.role
        )));
    }
    refuse_nul_in_labels(&request.allowed_labels.0)?;
    ensure_tenant(&app, &tenant).await?;
    let key = new_key()?;
    let hash = key_hash(&key);
    let created_at = app.now();
    app.store
        .put_api_key(&rewind_engine::ports::ApiKey {
            hash,
            tenant: tenant.clone(),
            role: request.role.clone(),
            allowed_labels: request.allowed_labels.0.clone(),
            created_at,
            revoked_at: None,
        })
        .await?;
    let key = ApiKey {
        id: hash.to_hex(),
        tenant: tenant.as_str().to_owned(),
        name: request.name,
        role: request.role,
        allowed_labels: request.allowed_labels,
        created_by: caller.key_id.as_ref().map(|id| format!("key:{id}")),
        created_at: timestamp(created_at),
        revoked_at: None,
        key: Some(key),
    };
    Ok((StatusCode::CREATED, Json(key)).into_response())
}

/// A tenant gets its record, with the deployment's quota and retention, when its first key is
/// created.
pub(crate) async fn ensure_tenant(app: &App, tenant: &TenantId) -> Result<(), ApiError> {
    if app.store.get_tenant(tenant).await?.is_none() {
        app.store
            .put_tenant(&Tenant {
                id: tenant.clone(),
                active_run_quota: app.settings.run_quota,
                retention: app.settings.retention,
            })
            .await?;
    }
    Ok(())
}

/// `DELETE /v1/api-keys/{id}`.
pub(super) async fn revoke_key(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.allow(Access::Admin)?;
    let missing = || ApiError::not_found(format!("no API key {id}"));
    let hash = Digest::from_hex(&id).map_err(|_| missing())?;
    if !app.store.revoke_api_key(hash, app.now()).await? {
        return Err(missing());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/whoami`.
pub(super) async fn whoami(caller: Caller) -> Json<Identity> {
    Json(Identity {
        tenant: Some(caller.tenant.as_str().to_owned()),
        principal: caller.key_id.clone().map(|id| Principal::Key { id }),
        permissions: caller.permissions().to_vec(),
        operator: caller.operator(),
        grant: Some(Grant {
            role: caller.role,
            allowed_labels: Labels::from(caller.labels),
            granted_by: None,
            granted_at: None,
        }),
        organization: None,
        organizations: Vec::new(),
    })
}

/// `GET /v1/server`.
pub(super) async fn server(State(app): State<Arc<App>>, _caller: Caller) -> Json<ServerInfo> {
    Json(ServerInfo {
        version: app.settings.version.to_owned(),
        epochs: app.engine.epochs().iter().map(|epoch| epoch.0).collect(),
        roles: app.settings.roles.clone(),
        embedded: app.settings.embedded,
        virtual_time: app.engine.clock().is_virtual(),
    })
}

/// `POST /v1/server/stop`: only the embedded server stops this way, once its work in flight is
/// committed.
pub(super) async fn stop(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<StatusCode, ApiError> {
    caller.allow(Access::Admin)?;
    if !app.settings.embedded {
        return Err(ApiError::forbidden(
            "only the embedded server stops through the API: stop this server as it was started",
        ));
    }
    tracing::info!("stop requested through the API");
    app.stop.send_replace(true);
    Ok(StatusCode::ACCEPTED)
}
