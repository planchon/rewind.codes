//! People's connections (`archi/07-code-architecture.md`, "HTTP API `/v1`", group Connections;
//! `archi/06-storage-and-scale.md`, "Connections"): the person's own (`/v1/connections`), those
//! that open the caller's organization (`/v1/organization/connections`), and the consent that
//! makes one (`/v1/oauth/consent`).
//!
//! Only its person changes or revokes a connection; whoever holds `members:manage` in an
//! organization removes it from a connection. No one gives more than they hold: opening an
//! organization with an access requires the person's grant there to hold its permissions.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use rewind_api::access::Permission;
use rewind_api::common::{Labels, timestamp};
use rewind_api::connections::{
    Access as ApiAccess, ClientKind as ApiClientKind, Connection as ConnectionView,
    ConnectionClient, ConnectionList, ConnectionOrganization, ConnectionPerson, Consent,
    ConsentAnswer, ConsentClient, ConsentDecision, ConsentOrganization, ConsentQuery,
    ConsentRedirect, UpdateConnection,
};
use rewind_api::oauth::REGISTERED_CLIENT_PREFIX;
use rewind_api::organization::Principal as CallerPrincipal;
use rewind_engine::ports::{
    self, Access, AccessStore, IdentityProvider, MembershipStatus, OAuthCode, OpenedOrganization,
    OrganizationId, Principal, TenantId, UserId,
};
use rewind_engine::text::refuse_nul_in_labels;

use crate::app::{App, SignIn};
use crate::auth::{Caller, Member, Who, listed, role_permissions};
use crate::connections::{
    SignedConnection, access_permissions, after, api_access, organization_names, store_access,
    world_now,
};
use crate::error::ApiError;
use crate::extract::{Body, Query};
use crate::grants::holds;
use crate::oauth::{
    CODE_LIFETIME, ClientError, PendingRequest, hash, loopback, plain_id, random_text,
    redirect_host,
};

// -------------------------------------------------------------------------------------------
// Views

/// A person's name, from their first and last names.
fn person_name(user: &ports::User) -> Option<String> {
    let name = [user.first_name.as_deref(), user.last_name.as_deref()]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!name.is_empty()).then_some(name)
}

/// The person, as the identity provider knows them: nothing without one, or when it cannot be
/// reached.
async fn person(app: &App, user: &UserId) -> ConnectionPerson {
    let found = match &app.sign_in {
        Some(sign_in) => sign_in.identity.user(user).await.ok().flatten(),
        None => None,
    };
    ConnectionPerson {
        id: user.to_string(),
        email: found.as_ref().map(|user| user.email.clone()),
        name: found.as_ref().and_then(person_name),
    }
}

fn client_kind(id: &str) -> ApiClientKind {
    if id.starts_with(REGISTERED_CLIENT_PREFIX) {
        ApiClientKind::Registered
    } else {
        ApiClientKind::MetadataDocument
    }
}

/// The client of a connection: its name when the connection was made, its site when Rewind
/// still keeps the client.
async fn client(app: &App, connection: &ports::Connection) -> Result<ConnectionClient, ApiError> {
    let kept = app.store.oauth_client(&connection.client_id).await?;
    Ok(ConnectionClient {
        id: connection.client_id.clone(),
        name: connection.client_name.clone(),
        uri: kept.and_then(|client| client.uri),
        kind: client_kind(&connection.client_id),
    })
}

/// The organizations a connection's grants open, named.
fn organizations(
    grants: &[ports::Grant],
    names: &HashMap<String, String>,
) -> Vec<ConnectionOrganization> {
    grants
        .iter()
        .map(|grant| ConnectionOrganization {
            id: grant.tenant.to_string(),
            name: names.get(grant.tenant.as_str()).cloned(),
            access: api_access(Access::of_role(&grant.role).unwrap_or(Access::Read)),
            allowed_labels: Labels::from(grant.allowed_labels.clone()),
        })
        .collect()
}

/// A connection as the API shows it, with the organizations of `grants`.
async fn view(
    app: &App,
    connection: &ports::Connection,
    person: ConnectionPerson,
    grants: &[ports::Grant],
    names: &HashMap<String, String>,
) -> Result<ConnectionView, ApiError> {
    Ok(ConnectionView {
        id: connection.id.clone(),
        person,
        client: client(app, connection).await?,
        organizations: organizations(grants, names),
        created_at: timestamp(connection.created_at),
        last_used_at: connection.last_used_at.map(timestamp),
    })
}

/// The caller of `GET /v1/whoami` when it is a connection: its id, person, client, and the
/// organizations it opens with their access and labels.
pub(super) async fn principal(
    app: &App,
    signed: &SignedConnection,
) -> Result<CallerPrincipal, ApiError> {
    let connection = app.store.connection(&signed.id).await?.ok_or_else(|| {
        ApiError::new(
            rewind_api::ProblemType::Unauthorized,
            "this connection no longer exists: connect the client again",
        )
    })?;
    let grants = app.store.connection_grants(&connection.id).await?;
    let names = organization_names(app, &connection.user).await;
    Ok(CallerPrincipal::Connection {
        id: connection.id.clone(),
        person: person(app, &connection.user).await,
        client: client(app, &connection).await?,
        organizations: organizations(&grants, &names),
    })
}

// -------------------------------------------------------------------------------------------
// The person's connections

/// The member behind a request about their own connections: keys, tokens and connections are
/// refused (403).
fn member(who: &Who, what: &str) -> Result<Member, ApiError> {
    who.member.clone().ok_or_else(|| {
        ApiError::forbidden(format!(
            "only a member signed in on the dashboard may {what}: a key, a token or a \
             connection has no connections of its own"
        ))
    })
}

/// The person's live connection `id`, or 404: another person's is answered as missing.
async fn own(app: &App, member: &Member, id: &str) -> Result<ports::Connection, ApiError> {
    let found = if plain_id(id) {
        app.store.connection(id).await?
    } else {
        None
    };
    found
        .filter(|connection| connection.user == member.user && connection.revoked_at.is_none())
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "no connection {id} of yours: GET /v1/connections lists them"
            ))
        })
}

/// `GET /v1/connections`: the caller's connections, in every organization.
pub(super) async fn list(
    State(app): State<Arc<App>>,
    who: Who,
) -> Result<Json<ConnectionList>, ApiError> {
    let member = member(&who, "list their connections")?;
    app.sign_in()?;
    let names = organization_names(&app, &member.user).await;
    let person = person(&app, &member.user).await;
    let mut connections = Vec::new();
    for connection in app.store.connections_of(&member.user).await? {
        let grants = app.store.connection_grants(&connection.id).await?;
        connections.push(view(&app, &connection, person.clone(), &grants, &names).await?);
    }
    Ok(Json(ConnectionList { connections }))
}

/// `PATCH /v1/connections/{id}`: the organizations the connection opens from now on, each where
/// the person's grant gives the access chosen.
pub(super) async fn update(
    State(app): State<Arc<App>>,
    who: Who,
    Path(id): Path<String>,
    Body(request): Body<UpdateConnection>,
) -> Result<Json<ConnectionView>, ApiError> {
    let member = member(&who, "change their connections")?;
    let sign_in = app.sign_in()?;
    let connection = own(&app, &member, &id).await?;
    let mut opened = Vec::new();
    let openable = openable(&app, sign_in, &member.user).await?;
    let mut seen = BTreeSet::new();
    for organization in &request.organizations {
        let tenant = tenant(&organization.id)?;
        if !seen.insert(tenant.clone()) {
            return Err(ApiError::invalid(format!(
                "organizations: {} is given twice",
                organization.id
            )));
        }
        let access = store_access(organization.access);
        may_open(&openable, &tenant, access)?;
        refuse_nul_in_labels(&organization.allowed_labels.0)?;
        opened.push(OpenedOrganization {
            tenant,
            access,
            allowed_labels: organization.allowed_labels.0.clone(),
        });
    }
    if !app
        .store
        .set_connection_organizations(&connection.id, &opened, world_now())
        .await?
    {
        return Err(ApiError::not_found(format!(
            "no connection {id} of yours: GET /v1/connections lists them"
        )));
    }
    let grants = app.store.connection_grants(&connection.id).await?;
    let names = organization_names(&app, &member.user).await;
    let person = person(&app, &member.user).await;
    Ok(Json(
        view(&app, &connection, person, &grants, &names).await?,
    ))
}

/// `DELETE /v1/connections/{id}`: revokes it, its tokens and its grants at once.
pub(super) async fn revoke(
    State(app): State<Arc<App>>,
    who: Who,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let member = member(&who, "revoke their connections")?;
    app.sign_in()?;
    let connection = own(&app, &member, &id).await?;
    app.store
        .revoke_connection(&connection.id, world_now())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// -------------------------------------------------------------------------------------------
// The organization's connections

/// `GET /v1/organization/connections`: the connections that open the organization, each with
/// this organization only.
pub(super) async fn organization_list(
    State(app): State<Arc<App>>,
    caller: Caller,
) -> Result<Json<ConnectionList>, ApiError> {
    caller.require(Permission::MembersRead)?;
    let name = match &app.sign_in {
        Some(sign_in) => sign_in
            .identity
            .organization(&OrganizationId::new(caller.tenant.as_str()))
            .await?
            .map(|organization| organization.name),
        None => None,
    };
    let names: HashMap<String, String> = name
        .map(|name| (caller.tenant.to_string(), name))
        .into_iter()
        .collect();
    let mut connections = Vec::new();
    for grant in app.store.list_grants(&caller.tenant).await? {
        let Principal::Connection(id) = &grant.principal else {
            continue;
        };
        let Some(connection) = app
            .store
            .connection(id)
            .await?
            .filter(|connection| connection.revoked_at.is_none())
        else {
            continue;
        };
        let person = person(&app, &connection.user).await;
        connections.push(view(&app, &connection, person, &[grant], &names).await?);
    }
    connections.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    Ok(Json(ConnectionList { connections }))
}

/// `DELETE /v1/organization/connections/{id}`: the connection no longer opens the organization.
/// The caller holds the permissions of the connection's access here.
pub(super) async fn organization_remove(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::MembersManage)?;
    let principal = Principal::Connection(id.clone());
    let missing = || {
        ApiError::not_found(format!(
            "no connection {id} opens this organization: GET /v1/organization/connections lists \
             them"
        ))
    };
    if !plain_id(&id) {
        return Err(missing());
    }
    let Some(grant) = app.store.grant(&caller.tenant, &principal).await? else {
        return Err(missing());
    };
    let access = Access::of_role(&grant.role).unwrap_or(Access::Run);
    holds(
        &caller,
        &grant.role,
        &access_permissions(access),
        "taking away",
    )?;
    app.store
        .delete_grant(&caller.tenant, &principal, None)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes, in `tenant`, the grants of the connections of `user`: removing a member removes
/// their connections' grants there.
pub(super) async fn close_for(app: &App, tenant: &TenantId, user: &UserId) -> Result<(), ApiError> {
    for connection in app.store.connections_of(user).await? {
        app.store
            .delete_grant(tenant, &connection.principal(), None)
            .await?;
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------
// What a person may open

/// An organization the person may open to a connection: their role there and its permissions.
#[derive(Debug, Clone)]
struct Openable {
    name: String,
    role: Option<String>,
    permissions: BTreeSet<Permission>,
}

/// The person's active organizations, each with their grant's role and its permissions.
async fn openable(
    app: &App,
    sign_in: &SignIn,
    user: &UserId,
) -> Result<BTreeMap<TenantId, Openable>, ApiError> {
    let mut found = BTreeMap::new();
    for membership in sign_in.identity.memberships_of_user(user).await? {
        if membership.status != MembershipStatus::Active {
            continue;
        }
        let Ok(tenant) = membership.organization.id.tenant() else {
            continue;
        };
        let grant = app.store.grant(&tenant, &Principal::from(user)).await?;
        let permissions = match &grant {
            Some(grant) => role_permissions(&app.store, &tenant, &grant.role)
                .await?
                .unwrap_or_default(),
            None => BTreeSet::new(),
        };
        found.insert(
            tenant,
            Openable {
                name: membership.organization.name.clone(),
                role: grant.map(|grant| grant.role),
                permissions,
            },
        );
    }
    Ok(found)
}

/// The accesses the person may give in an organization: those whose permissions their grant
/// holds.
fn accesses(openable: &Openable) -> Vec<ApiAccess> {
    Access::ALL
        .into_iter()
        .filter(|access| access_permissions(*access).is_subset(&openable.permissions))
        .map(api_access)
        .collect()
}

/// Refuses (403) opening `tenant` with `access` unless the person is a member there whose grant
/// holds the access's permissions: no one gives more than they hold.
fn may_open(
    openable: &BTreeMap<TenantId, Openable>,
    tenant: &TenantId,
    access: Access,
) -> Result<(), ApiError> {
    let Some(found) = openable.get(tenant) else {
        return Err(ApiError::forbidden(format!(
            "you are not a member of the organization {tenant}: a connection opens only \
             organizations where its person holds a grant"
        )));
    };
    let Some(role) = &found.role else {
        return Err(ApiError::forbidden(format!(
            "you hold no grant in the organization {tenant}, so you cannot open it to a \
             connection: no one gives more than they hold"
        )));
    };
    let lacking: Vec<Permission> = access_permissions(access)
        .difference(&found.permissions)
        .copied()
        .collect();
    if !lacking.is_empty() {
        return Err(ApiError::forbidden(format!(
            "the access {access} in the organization {tenant} gives {}, which your role {role} \
             there does not hold: no one gives more than they hold",
            listed(&lacking)
        )));
    }
    Ok(())
}

fn tenant(id: &str) -> Result<TenantId, ApiError> {
    TenantId::new(id.trim()).map_err(|_| {
        ApiError::invalid(format!(
            "organizations: {id:?} is not an organization's id (org_…)"
        ))
    })
}

// -------------------------------------------------------------------------------------------
// The consent

/// The client of a pending request, or 400 when it can no longer be used.
async fn consent_client(
    app: &App,
    request: &PendingRequest,
) -> Result<ports::OAuthClient, ApiError> {
    let (oauth, _) = app.oauth()?;
    oauth
        .client(&app.store, &request.client_id)
        .await
        .map_err(|error| match error {
            ClientError::Unknown(reason) => ApiError::invalid(reason),
            ClientError::Store(error) => ApiError::from(error),
        })
}

/// The labels a connection's grants carry unless the person changes them: what Claude starts
/// carries `via: claude`.
fn default_labels() -> Labels {
    Labels::from(BTreeMap::from([("via".to_owned(), "claude".to_owned())]))
}

/// `GET /v1/oauth/consent?request=…`: the client, where it returns, and the person's
/// organizations with the accesses they may give in each.
pub(super) async fn consent(
    State(app): State<Arc<App>>,
    who: Who,
    Query(query): Query<ConsentQuery>,
) -> Result<Json<Consent>, ApiError> {
    let member = member(&who, "answer an authorization request")?;
    let (_, sign_in) = app.oauth()?;
    let request = PendingRequest::unseal(sign_in, &query.request)?;
    let client = consent_client(&app, &request).await?;
    let openable = openable(&app, sign_in, &member.user).await?;
    let current = member
        .organization
        .as_ref()
        .and_then(|organization| organization.tenant().ok());
    let organizations = openable
        .iter()
        .map(|(tenant, found)| ConsentOrganization {
            id: tenant.to_string(),
            name: Some(found.name.clone()),
            role: found.role.clone(),
            accesses: accesses(found),
            current: current.as_ref() == Some(tenant),
        })
        .collect();
    Ok(Json(Consent {
        client: ConsentClient {
            kind: client_kind(&client.id),
            id: client.id,
            name: client.name,
            uri: client.uri,
            logo_uri: client.logo_uri,
        },
        redirect_host: redirect_host(&request.redirect_uri),
        loopback: loopback(&request.redirect_uri),
        organizations,
        allowed_labels: default_labels(),
        expires_at: timestamp(request.expires_at()),
    }))
}

/// `POST /v1/oauth/consent`: allows the request (a code for the organizations chosen, 5
/// minutes) or denies it; answers where to send the browser.
pub(super) async fn answer(
    State(app): State<Arc<App>>,
    who: Who,
    Body(answer): Body<ConsentAnswer>,
) -> Result<Json<ConsentRedirect>, ApiError> {
    let member = member(&who, "answer an authorization request")?;
    let (_, sign_in) = app.oauth()?;
    let request = PendingRequest::unseal(sign_in, &answer.request)?;
    consent_client(&app, &request).await?;
    if answer.decision == ConsentDecision::Deny {
        return Ok(Json(ConsentRedirect {
            redirect_to: request.answer(&[
                ("error", "access_denied"),
                ("error_description", "the person denied the request"),
            ]),
        }));
    }
    if answer.organizations.is_empty() {
        return Err(ApiError::invalid(
            "organizations: allowing a connection needs at least one organization to open",
        ));
    }
    let openable = openable(&app, sign_in, &member.user).await?;
    let mut organizations = BTreeMap::new();
    for choice in &answer.organizations {
        let tenant = tenant(&choice.id)?;
        let access = store_access(choice.access);
        may_open(&openable, &tenant, access)?;
        if organizations.insert(tenant, access).is_some() {
            return Err(ApiError::invalid(format!(
                "organizations: {} is given twice",
                choice.id
            )));
        }
    }
    refuse_nul_in_labels(&answer.allowed_labels.0)?;
    let code = random_text(43)?;
    let now = world_now();
    app.store
        .put_oauth_code(&OAuthCode {
            hash: hash(&code),
            client_id: request.client_id.clone(),
            user: member.user.clone(),
            redirect_uri: request.redirect_uri.clone(),
            code_challenge: request.code_challenge.clone(),
            resource: request.resource.clone(),
            organizations,
            labels: answer.allowed_labels.0.clone(),
            expires_at: after(now, CODE_LIFETIME),
            used_at: None,
            connection: None,
        })
        .await?;
    Ok(Json(ConsentRedirect {
        redirect_to: request.answer(&[("code", code.as_str())]),
    }))
}
