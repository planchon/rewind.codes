//! Administration: API keys, who a key is, the server itself.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ptc_api::admin::{ApiKey, CreateApiKey, Identity, ServerInfo};
use ptc_api::common::{Labels, timestamp};
use ptc_core::Digest;
use ptc_engine::ports::{JournalStore, Retention, Tenant, TenantId};

use crate::app::App;
use crate::auth::{Access, Caller, key_hash, new_key};
use crate::error::ApiError;
use crate::extract::Body;

/// `POST /v1/api-keys`: a key for a tenant, created with its first key. The key itself is only
/// in this answer.
pub(super) async fn create_key(
    State(app): State<Arc<App>>,
    caller: Caller,
    Body(request): Body<CreateApiKey>,
) -> Result<Response, ApiError> {
    caller.allow(Access::Admin)?;
    let tenant =
        TenantId::new(request.tenant).map_err(|error| ApiError::invalid(error.to_string()))?;
    ensure_tenant(&app, &tenant).await?;
    let key = new_key()?;
    let hash = key_hash(&key);
    let created_at = app.now();
    app.store
        .put_api_key(&ptc_engine::ports::ApiKey {
            hash,
            tenant: tenant.clone(),
            role: request.role.as_str().to_owned(),
            allowed_labels: request.allowed_labels.0.clone(),
            created_at,
            revoked_at: None,
        })
        .await?;
    let key = ApiKey {
        id: hash.to_hex(),
        tenant: tenant.as_str().to_owned(),
        role: request.role,
        allowed_labels: request.allowed_labels,
        name: request.name,
        created_at: timestamp(created_at),
        key: Some(key),
    };
    Ok((StatusCode::CREATED, Json(key)).into_response())
}

/// A tenant gets its record, with the deployment's quota, when its first key is created.
pub(crate) async fn ensure_tenant(app: &App, tenant: &TenantId) -> Result<(), ApiError> {
    if app.store.get_tenant(tenant).await?.is_none() {
        app.store
            .put_tenant(&Tenant {
                id: tenant.clone(),
                active_run_quota: app.settings.run_quota,
                retention: Retention::default(),
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
        tenant: caller.tenant.as_str().to_owned(),
        role: caller.role,
        allowed_labels: Labels::from(caller.labels),
        key_id: caller.key_id,
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
