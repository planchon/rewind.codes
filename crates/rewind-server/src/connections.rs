//! People's connections as callers (`archi/06-storage-and-scale.md`, "Connections"): the access
//! token `rewind_ct_…` of a connection, the [`Caller`] it is in the organization its request
//! names, and who calls `/mcp` ([`mcp_caller`]).
//!
//! A connection is a person's key, outside every organization. In the organization a request
//! names (`Rewind-Organization`), it uses the permissions of its grant's role that are among
//! those of its access ([`CONNECTION_PERMISSIONS`]) and that the person's own grant there gives
//! now, and sees the runs carrying the labels of both grants; without either grant, or with
//! labels that contradict each other, it has no access (403). Every request reads the token,
//! the connection, its grant and the person's grant again.
//!
//! Connections live in the world's time, as the dashboard's sign-in does: their tokens are an
//! OAuth client's, whatever the server's clock.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rewind_api::Problem;
use rewind_api::access::Permission;
use rewind_api::connections::Access as ApiAccess;
use rewind_api::headers::{
    API_KEY_PREFIX, CONNECTION_TOKEN_PREFIX, TOKEN_KEY_PREFIX, TOKEN_PREFIX,
};
use rewind_core::{Digest, Timestamp};
use rewind_engine::adapters::SystemClock;
use rewind_engine::ports::{
    Access, AccessStore, Clock, ConnectionTokenKind, IdentityProvider, MembershipStatus,
    OrganizationId, Principal, TenantId, UserId,
};

use crate::app::App;
use crate::auth::{Caller, TokenForm, role_permissions, tenant_credentials, unauthorized};
use crate::error::ApiError;

/// How long an access token lives.
pub(crate) const ACCESS_TOKEN_LIFETIME: Duration = Duration::from_secs(3_600);

/// How long a refresh token lives unused: each use replaces it.
pub(crate) const REFRESH_TOKEN_LIFETIME: Duration = Duration::from_secs(30 * 86_400);

/// A connection's last use is written at most this often: a request reads it, and writes only
/// when it is older.
const TOUCH_EVERY: Duration = Duration::from_secs(60);

/// The permissions a connection may ever use: those of `run`, which hold those of `read`.
pub(crate) const CONNECTION_PERMISSIONS: [Permission; 5] = [
    Permission::RunsRead,
    Permission::RunsStart,
    Permission::RunsControl,
    Permission::ToolsetsRead,
    Permission::NamespacesRead,
];

/// Now, in the world's time.
pub(crate) fn world_now() -> Timestamp {
    SystemClock.now()
}

/// `at` plus `duration`.
pub(crate) fn after(at: Timestamp, duration: Duration) -> Timestamp {
    let millis = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    Timestamp::from_millis(at.as_millis().saturating_add(millis))
}

/// The access of the API.
pub(crate) fn api_access(access: Access) -> ApiAccess {
    match access {
        Access::Read => ApiAccess::Read,
        Access::Run => ApiAccess::Run,
    }
}

/// The access of the store.
pub(crate) fn store_access(access: ApiAccess) -> Access {
    match access {
        ApiAccess::Read => Access::Read,
        ApiAccess::Run => Access::Run,
    }
}

/// The permissions an access gives, at most.
pub(crate) fn access_permissions(access: Access) -> BTreeSet<Permission> {
    api_access(access).permissions().iter().copied().collect()
}

/// A person's connection, as its access token presented it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignedConnection {
    /// `cn_…`.
    pub(crate) id: String,
    /// The person.
    pub(crate) user: UserId,
    pub(crate) client_id: String,
    pub(crate) client_name: String,
    /// The SHA-256 of the access token.
    pub(crate) token: Digest,
    /// When the access token expires, in the world's time: an event stream it opened closes
    /// then.
    pub(crate) expires_at: Timestamp,
}

impl SignedConnection {
    pub(crate) fn principal(&self) -> Principal {
        Principal::Connection(self.id.clone())
    }
}

/// The connection of an access token: live, of a connection not revoked. Its last use is
/// written when older than a minute.
pub(crate) async fn authenticate(app: &App, token: &str) -> Result<SignedConnection, ApiError> {
    let hash = Digest::of(token.as_bytes());
    let now = world_now();
    let found = app
        .store
        .connection_token(hash)
        .await?
        .filter(|found| found.kind == ConnectionTokenKind::Access && now < found.expires_at);
    let Some(found) = found else {
        return Err(unauthorized(
            "this connection's access token is unknown, revoked or expired: the client renews \
             it with its refresh token, or connects again",
        ));
    };
    let connection = app
        .store
        .connection(&found.connection)
        .await?
        .filter(|connection| connection.revoked_at.is_none());
    let Some(connection) = connection else {
        return Err(unauthorized(
            "this connection was revoked: connect the client to Rewind again",
        ));
    };
    let stale = connection
        .last_used_at
        .is_none_or(|used| now >= after(used, TOUCH_EVERY));
    if stale && let Err(error) = app.store.touch_connection(&connection.id, now).await {
        tracing::warn!(connection = %connection.id, %error, "the connection's last use was not written");
    }
    Ok(SignedConnection {
        id: connection.id,
        user: connection.user,
        client_id: connection.client_id,
        client_name: connection.client_name,
        token: hash,
        expires_at: found.expires_at,
    })
}

/// What a connection's request without `Rewind-Organization` is told, but by `GET /v1/whoami`.
pub(crate) fn organization_required() -> ApiError {
    ApiError::invalid(
        "a connection's request names the organization it acts in with the header \
         Rewind-Organization: org_…. GET /v1/whoami lists the organizations this connection \
         opens",
    )
}

/// The labels of both grants; `None` when they contradict each other.
fn both(
    first: &BTreeMap<String, String>,
    second: &BTreeMap<String, String>,
) -> Option<BTreeMap<String, String>> {
    let mut labels = first.clone();
    for (key, value) in second {
        match labels.get(key) {
            Some(held) if held != value => return None,
            Some(_) => {}
            None => {
                labels.insert(key.clone(), value.clone());
            }
        }
    }
    Some(labels)
}

impl Caller {
    /// The connection in `tenant`, as its grant and its person's grant say now: the
    /// permissions of its grant's role among [`CONNECTION_PERMISSIONS`] and within its
    /// access, that the person's grant gives; the labels of both grants. 403 without either
    /// grant, or with labels that contradict each other.
    pub(crate) async fn of_connection(
        store: &impl AccessStore,
        connection: &SignedConnection,
        tenant: TenantId,
    ) -> Result<Self, ApiError> {
        let principal = connection.principal();
        let Some(grant) = store.grant(&tenant, &principal).await? else {
            return Err(ApiError::forbidden(format!(
                "this connection does not open the organization {tenant}: GET /v1/whoami lists \
                 those it opens, and its person opens others from the dashboard (Account › \
                 Connections)"
            )));
        };
        let person = store
            .grant(&tenant, &Principal::from(&connection.user))
            .await?;
        let Some(person) = person else {
            return Err(ApiError::forbidden(format!(
                "the person of this connection has no grant in the organization {tenant}, so \
                 the connection has no access there: no one gives more than they hold"
            )));
        };
        let Some(labels) = both(&grant.allowed_labels, &person.allowed_labels) else {
            return Err(ApiError::forbidden(format!(
                "the labels of this connection's grant contradict those of its person's grant \
                 in the organization {tenant}, so it sees no run there: its person changes them \
                 from the dashboard"
            )));
        };
        let ceiling = Access::of_role(&grant.role)
            .map(access_permissions)
            .unwrap_or_default();
        let own = role_permissions(store, &tenant, &grant.role)
            .await?
            .unwrap_or_default();
        let held = role_permissions(store, &tenant, &person.role)
            .await?
            .unwrap_or_default();
        let permissions = CONNECTION_PERMISSIONS
            .into_iter()
            .filter(|permission| {
                ceiling.contains(permission)
                    && own.contains(permission)
                    && held.contains(permission)
            })
            .collect();
        Ok(Self {
            tenant,
            principal: Some(principal),
            role: Some(grant.role),
            labels,
            granted_by: grant.granted_by,
            granted_at: Some(grant.granted_at),
            permissions,
            namespace: None,
            token: None,
            connection: Some(connection.clone()),
        })
    }
}

/// Whether a connection still authenticates in `tenant`, as when its request opened an event
/// stream: its access token neither expired nor gone, the connection not revoked, its grant
/// there kept.
pub(crate) async fn still_valid(
    app: &App,
    connection: &SignedConnection,
    tenant: &TenantId,
) -> bool {
    if world_now() >= connection.expires_at {
        return false;
    }
    let token = matches!(
        app.store.connection_token(connection.token).await,
        Ok(Some(_))
    );
    let live = matches!(
        app.store.connection(&connection.id).await,
        Ok(Some(found)) if found.revoked_at.is_none()
    );
    let granted = matches!(
        app.store.grant(tenant, &connection.principal()).await,
        Ok(Some(_))
    );
    token && live && granted
}

// -------------------------------------------------------------------------------------------
// Who calls `/mcp`

/// Who calls `/mcp`, from its Bearer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpPrincipal {
    /// A person's connection.
    Connection {
        /// `cn_…`.
        id: String,
        /// The person, `user_…`.
        person: String,
        /// The client's id: a metadata document's URL, or `rc_…`.
        client: String,
    },
    /// An API key, by its id (`DELETE /v1/api-keys/{id}`).
    Key { id: String },
    /// An end user's token: opaque (its id) or signed (its key).
    Token {
        id: Option<String>,
        kid: Option<String>,
    },
}

/// An organization the caller of `/mcp` reaches, with what it may do there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOrganization {
    /// The tenant, `org_…` (or a key's tenant id).
    pub id: String,
    /// Its name, as the identity provider gives it; none without one.
    pub name: Option<String>,
    /// The permissions the caller may use there, in the catalog's order.
    pub permissions: Vec<Permission>,
    /// For an end user's token: its namespace, the only one it reaches.
    pub namespace: Option<String>,
    /// The labels the caller sees runs by, and gives to what it starts.
    pub labels: BTreeMap<String, String>,
}

/// Who calls `/mcp`, and the organizations it reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCaller {
    pub principal: McpPrincipal,
    /// For a connection, the organizations it opens where it has access now; for a key or a
    /// token, its tenant.
    pub organizations: Vec<McpOrganization>,
}

/// Who calls `/mcp`, from its Bearer (`archi/07-code-architecture.md`, "MCP and OAuth"): a
/// connection's access token, an API key or an end user's token. A member's access token is
/// refused (401), as anything else.
pub(crate) async fn mcp_caller(app: &App, bearer: &str) -> Result<McpCaller, ApiError> {
    if bearer.starts_with(CONNECTION_TOKEN_PREFIX) {
        let connection = authenticate(app, bearer).await?;
        let names = organization_names(app, &connection.user).await;
        let mut organizations = Vec::new();
        for grant in app.store.connection_grants(&connection.id).await? {
            // An organization where it has no access now is not reachable.
            let Ok(caller) = Caller::of_connection(&app.store, &connection, grant.tenant).await
            else {
                continue;
            };
            organizations.push(McpOrganization {
                id: caller.tenant.to_string(),
                name: names.get(caller.tenant.as_str()).cloned(),
                permissions: caller.effective_permissions(),
                namespace: None,
                labels: caller.labels,
            });
        }
        return Ok(McpCaller {
            principal: McpPrincipal::Connection {
                id: connection.id,
                person: connection.user.to_string(),
                client: connection.client_id,
            },
            organizations,
        });
    }
    let tenant_bearer = bearer.starts_with(API_KEY_PREFIX)
        || bearer.starts_with(TOKEN_PREFIX)
        || jsonwebtoken::decode_header(bearer).is_ok_and(|header| {
            header
                .kid
                .as_deref()
                .is_some_and(|kid| kid.starts_with(TOKEN_KEY_PREFIX))
        });
    if !tenant_bearer {
        return Err(unauthorized(
            "/mcp takes a connection's access token (rewind_ct_…, from the OAuth flow), an API \
             key or an end user's token: not a member's access token",
        ));
    }
    let Some(caller) = tenant_credentials(app, bearer).await?.caller else {
        return Err(unauthorized("this token names no organization"));
    };
    let name = match &app.sign_in {
        Some(sign_in) => sign_in
            .identity
            .organization(&OrganizationId::new(caller.tenant.as_str()))
            .await
            .ok()
            .flatten()
            .map(|organization| organization.name),
        None => None,
    };
    let principal = match (&caller.token, &caller.principal) {
        (Some(token), _) => match &token.form {
            TokenForm::Opaque { hash } => McpPrincipal::Token {
                id: Some(hash.to_hex()),
                kid: None,
            },
            TokenForm::Signed { kid, .. } => McpPrincipal::Token {
                id: None,
                kid: Some(kid.clone()),
            },
        },
        (None, Some(Principal::Key(hash))) => McpPrincipal::Key { id: hash.to_hex() },
        _ => return Err(unauthorized("this token names no caller")),
    };
    Ok(McpCaller {
        principal,
        organizations: vec![McpOrganization {
            id: caller.tenant.to_string(),
            name,
            permissions: caller.effective_permissions(),
            namespace: caller.namespace.as_ref().map(ToString::to_string),
            labels: caller.labels,
        }],
    })
}

/// The names of the person's organizations, by id: none without the identity provider, or
/// when it cannot be reached.
pub(crate) async fn organization_names(
    app: &App,
    user: &UserId,
) -> std::collections::HashMap<String, String> {
    let Some(sign_in) = &app.sign_in else {
        return std::collections::HashMap::new();
    };
    match sign_in.identity.memberships_of_user(user).await {
        Ok(memberships) => memberships
            .into_iter()
            .filter(|membership| membership.status == MembershipStatus::Active)
            .map(|membership| {
                (
                    membership.organization.id.as_str().to_owned(),
                    membership.organization.name,
                )
            })
            .collect(),
        Err(error) => {
            tracing::warn!(%error, "the person's organizations could not be named");
            std::collections::HashMap::new()
        }
    }
}

/// [`mcp_caller`], its refusal as a problem.
pub(crate) async fn mcp_caller_or_problem(
    app: &App,
    bearer: &str,
) -> Result<McpCaller, Box<Problem>> {
    mcp_caller(app, bearer).await.map_err(|error| error.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contradicting_labels_give_no_access() {
        let via = BTreeMap::from([("via".to_owned(), "claude".to_owned())]);
        let team = BTreeMap::from([("team".to_owned(), "a".to_owned())]);
        assert_eq!(both(&via, &team).map(|labels| labels.len()), Some(2));
        assert_eq!(both(&via, &via), Some(via.clone()));
        let other = BTreeMap::from([("via".to_owned(), "cursor".to_owned())]);
        assert_eq!(both(&via, &other), None);
    }
}
