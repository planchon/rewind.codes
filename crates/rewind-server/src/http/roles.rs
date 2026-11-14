//! Roles (`archi/07-code-architecture.md`, "HTTP API `/v1`", group Access): the four built-in
//! roles, which never change, and the tenant's own, composed from the catalog. Writing a role
//! needs every permission it gives, and every permission it gave: a change applies at once to
//! every principal holding it. A role a grant points to, or the organization's default role,
//! stays.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::access::{
    BuiltInRole, OPERATOR, Permission, PutRole, Role, RoleList, validate_role_name,
};
use rewind_api::common::timestamp;
use rewind_engine::ports::{self, AccessStore, RoleDeletion};
use rewind_engine::text::refuse_nul;

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::Body;
use crate::grants::holds;

/// The permissions a stored role gives: the names the catalog knows.
fn permissions_of(role: &ports::Role) -> BTreeSet<Permission> {
    role.permissions
        .iter()
        .filter_map(|name| Permission::parse(name))
        .collect()
}

fn role_view(role: &ports::Role) -> Role {
    Role {
        name: role.name.clone(),
        description: role.description.clone(),
        permissions: permissions_of(role).into_iter().collect(),
        built_in: false,
        created_at: Some(timestamp(role.created_at)),
        updated_at: Some(timestamp(role.updated_at)),
    }
}

fn in_use(detail: String) -> ApiError {
    ApiError::new(ProblemType::RoleInUse, detail)
}

/// `GET /v1/roles`: the built-in roles, then the tenant's own by name.
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<RoleList>, ApiError> {
    caller.require(Permission::RolesRead)?;
    let mut roles: Vec<Role> = BuiltInRole::ALL.into_iter().map(Role::built_in).collect();
    roles.extend(
        app.store
            .list_roles(&caller.tenant)
            .await?
            .iter()
            .map(role_view),
    );
    Ok(Json(RoleList { roles }))
}

/// `PUT /v1/roles/{name}`: creates (201) or changes (200) a role of the tenant.
pub(super) async fn put(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(name): Path<String>,
    Body(request): Body<PutRole>,
) -> Result<Response, ApiError> {
    caller.require(Permission::RolesWrite)?;
    validate_role_name(&name).map_err(|error| ApiError::invalid(error.to_string()))?;
    refuse_nul("the description", &request.description)?;
    let permissions: BTreeSet<Permission> = request.permissions.iter().copied().collect();
    holds(&caller, &name, &permissions, "writing")?;
    let existing = app.store.get_role(&caller.tenant, &name).await?;
    if let Some(existing) = &existing {
        holds(&caller, &name, &permissions_of(existing), "changing")?;
    }
    let now = app.now();
    let role = ports::Role {
        tenant: caller.tenant.clone(),
        name: name.clone(),
        description: request.description.trim().to_owned(),
        permissions: permissions
            .iter()
            .map(|permission| permission.as_str().to_owned())
            .collect(),
        created_at: existing
            .as_ref()
            .map_or(now, |existing| existing.created_at),
        updated_at: now,
    };
    app.store.put_role(&role).await?;
    let stored = app
        .store
        .get_role(&caller.tenant, &name)
        .await?
        .unwrap_or(role);
    let status = if existing.is_some() {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };
    Ok((status, Json(role_view(&stored))).into_response())
}

/// `DELETE /v1/roles/{name}`: deletes a role of the tenant that no grant points to and that is
/// not the default role (409 `role-in-use`).
pub(super) async fn delete(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::RolesWrite)?;
    if name == OPERATOR || BuiltInRole::parse(&name).is_some() {
        return Err(ApiError::invalid(format!(
            "{name} is a built-in role or the operator's: it never changes nor goes"
        )));
    }
    if app.store.default_role(&caller.tenant).await?.as_deref() == Some(name.as_str()) {
        return Err(in_use(format!(
            "the role {name} is the organization's default role: set another with PATCH \
             /v1/organization (defaultRole) first"
        )));
    }
    match app.store.delete_role(&caller.tenant, &name).await? {
        RoleDeletion::Deleted => Ok(StatusCode::NO_CONTENT),
        RoleDeletion::NotFound => Err(ApiError::not_found(format!(
            "no role {name} in this organization: GET /v1/roles lists them"
        ))),
        RoleDeletion::InUse => Err(in_use(format!(
            "the role {name} is held by a member, a key or an invitation: give them another \
             role first (GET /v1/organization/members, GET /v1/api-keys, GET \
             /v1/organization/invitations)"
        ))),
    }
}
