//! Authentication and access (`archi/06-storage-and-scale.md`, "Tenancy, identity and access").
//!
//! Every request to `/v1` carries a Bearer token, and nothing else authenticates it:
//!
//! - An API key, `rewind_sk_…`. The access store keeps the SHA-256 of keys, never a key. A key
//!   belongs to a tenant, which scopes everything it sees; its principal is `key:<hash>`.
//! - On a server with a sign-in (WorkOS), a member's access token, checked by the
//!   identity provider ([`Member`]). Its organization is the tenant, whose record is created
//!   when missing; its principal is `user:<id>`. A token without organization reaches only the
//!   onboarding endpoints, which take [`Who`]: every other one answers 403.
//!
//! What a principal may do is its grant: a role, and optionally allowed labels. A built-in
//! role's permissions and the operator's are fixed (`rewind_api::access`); a role of the tenant
//! is read from the store; any other role gives none, as does a principal without a grant.
//! Every request reads the grant and the role: a change applies from the next request.
//!
//! Every endpoint requires one permission of the catalog ([`Caller::require`]), answering 403 with
//! the missing permission. A grant limited to labels sees only the runs carrying all of them, and
//! cannot administer: the permissions of [`ADMINISTRATION`] are refused to it.
//!
//! The operator (role `operator`) holds every permission in its own tenant, reaches the keys of
//! every tenant, and stops the embedded server. The embedded server's socket takes no key: its
//! one caller is the operator of the tenant `local`.
//!
//! An end user's token (`archi/06-storage-and-scale.md`, "Namespaces", "Who calls") holds no
//! grant: its permissions are its own, among [`Permission::TOKEN`], its labels work as a grant's,
//! and it is bound to its namespace. It is opaque (`rewind_nt_…`, found by its SHA-256 in the
//! access store) or a JWT the customer signed with a registered key (`kid` `rk_…`). Every
//! request reads its state again: revoked, expired, its key deleted, signed before its
//! namespace's `tokens_revoked_before`, or its namespace being erased, it is refused (401).
//!
//! The namespace of a request ([`Caller::namespace`]) is the token's; for a key or a member, the
//! header `Rewind-Namespace`, which narrows what it reads and names where it creates. A token's
//! request naming another namespace is refused (403).
//!
//! A person's connection (`rewind_ct_…`, `archi/06-storage-and-scale.md`, "Connections") names
//! no tenant: its request names one with `Rewind-Organization` ([`crate::connections`]), and
//! without it reaches only `GET /v1/whoami` (400 elsewhere). Every request reads the token, the
//! connection, its grant and its person's grant again; both roles check it against the access
//! store alone. Any other caller may repeat its own tenant in `Rewind-Organization`; another is
//! refused (403).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::header;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::access::{OPERATOR, Permission, fixed_permissions};
use rewind_api::headers::{
    API_KEY_PREFIX, CONNECTION_TOKEN_PREFIX, NAMESPACE, ORGANIZATION, TOKEN_KEY_PREFIX,
    TOKEN_PREFIX,
};
use rewind_api::namespaces::{SignedTokenClaims, TOKEN_AUDIENCE, TOKEN_MAX_TTL};
use rewind_core::{Digest, Timestamp};
use rewind_engine::adapters::SystemClock;
use rewind_engine::ports::{
    AccessStore, CatalogStore, Clock, IdentityError, IdentityProvider, NamespaceName,
    NamespaceWrite, OrganizationId, Principal, RunHead, SessionId, StoreError, TenantId, UserId,
};

use crate::app::App;
use crate::connections::{self, SignedConnection};
use crate::error::ApiError;
use crate::tokens::PublicKey;

/// The clock skew allowed on a signed token's expiry, as on a member's access token.
const LEEWAY_SECONDS: u64 = 30;

/// The permissions a grant limited to labels never uses: members, roles, keys and the
/// organization carry no labels.
pub(crate) const ADMINISTRATION: [Permission; 6] = [
    Permission::MembersInvite,
    Permission::MembersManage,
    Permission::RolesWrite,
    Permission::ApiKeysRead,
    Permission::ApiKeysWrite,
    Permission::OrganizationWrite,
];

/// Who calls, in which tenant, and what its grant lets it do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Caller {
    pub(crate) tenant: TenantId,
    /// The principal of the caller's grant: an API key. `None` over the embedded server's
    /// socket.
    pub(crate) principal: Option<Principal>,
    /// The role of the caller's grant: a built-in role, `operator`, or a role of the tenant.
    /// `None` for a principal without a grant, which holds no permission.
    pub(crate) role: Option<String>,
    /// Not empty: the caller only sees the runs carrying all of these labels.
    pub(crate) labels: BTreeMap<String, String>,
    /// Who gave the grant; `None` for the grants the server gives.
    pub(crate) granted_by: Option<Principal>,
    pub(crate) granted_at: Option<Timestamp>,
    /// What the role gives, or what the token holds.
    pub(crate) permissions: BTreeSet<Permission>,
    /// The namespace the request is bound to: a token's own; for a key or a member, the one
    /// `Rewind-Namespace` names, if any ([`Caller::bind`]).
    pub(crate) namespace: Option<NamespaceName>,
    /// The caller is an end user's token.
    pub(crate) token: Option<EndUserToken>,
    /// The caller is a person's connection, in the organization its request names.
    pub(crate) connection: Option<SignedConnection>,
}

/// An end user's token, as the request presented it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EndUserToken {
    pub(crate) form: TokenForm,
    /// When it expires: in the server's time for an opaque token, in the world's for a signed
    /// one.
    pub(crate) expires_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenForm {
    /// `rewind_nt_…`, by the SHA-256 of its secret: its id.
    Opaque { hash: Digest },
    /// A JWT the customer signed with the key `kid`.
    Signed {
        kid: String,
        jti: Option<String>,
        /// Its `iat`, in milliseconds.
        issued_at: Timestamp,
    },
}

impl Caller {
    /// The one caller of the embedded server's socket: the operator of the tenant `local`.
    pub(crate) fn local() -> Self {
        Self {
            tenant: TenantId::local(),
            principal: None,
            role: Some(OPERATOR.to_owned()),
            labels: BTreeMap::new(),
            granted_by: None,
            granted_at: None,
            permissions: Permission::ALL.into_iter().collect(),
            namespace: None,
            token: None,
            connection: None,
        }
    }

    /// `principal` in `tenant`, as its grant says now.
    pub(crate) async fn of(
        store: &impl AccessStore,
        tenant: TenantId,
        principal: Principal,
    ) -> Result<Self, StoreError> {
        let Some(grant) = store.grant(&tenant, &principal).await? else {
            return Ok(Self {
                tenant,
                principal: Some(principal),
                role: None,
                labels: BTreeMap::new(),
                granted_by: None,
                granted_at: None,
                permissions: BTreeSet::new(),
                namespace: None,
                token: None,
                connection: None,
            });
        };
        let permissions = role_permissions(store, &tenant, &grant.role)
            .await?
            .unwrap_or_default();
        Ok(Self {
            tenant,
            principal: Some(principal),
            role: Some(grant.role),
            labels: grant.allowed_labels,
            granted_by: grant.granted_by,
            granted_at: Some(grant.granted_at),
            permissions,
            namespace: None,
            token: None,
            connection: None,
        })
    }

    /// Whether the caller is the deployment's operator.
    pub(crate) fn operator(&self) -> bool {
        self.role.as_deref() == Some(OPERATOR)
    }

    /// The caller, as a message names it.
    pub(crate) fn who(&self) -> &'static str {
        if self.token.is_some() {
            return "this token";
        }
        match self.principal {
            Some(Principal::Key(_)) => "this key",
            Some(Principal::User(_)) => "this member",
            Some(Principal::Connection(_)) => "this connection",
            Some(Principal::Invitation(_)) | None => "this caller",
        }
    }

    /// Who writes, as what it writes names it (a secret, a code's title): a principal
    /// (`user:…`, `key:…`, `connection:…`), `token:<namespace>` for an end user's token, `local`
    /// over the embedded server's socket.
    pub(crate) fn writer(&self) -> String {
        if self.token.is_some()
            && let Some(namespace) = &self.namespace
        {
            return format!("token:{namespace}");
        }
        self.principal
            .as_ref()
            .map_or_else(|| "local".to_owned(), ToString::to_string)
    }

    /// Refuses the request (403) unless the caller's grant gives `permission`, or its token holds
    /// it; a grant limited to labels never administers.
    pub(crate) fn require(&self, permission: Permission) -> Result<(), ApiError> {
        let who = self.who();
        if !self.permissions.contains(&permission) {
            if self.token.is_some() {
                let held: Vec<Permission> = self.permissions.iter().copied().collect();
                return Err(ApiError::forbidden(format!(
                    "this request needs the permission `{permission}`, which this end user's \
                     token does not hold: it holds {}, within its namespace. The customer mints \
                     tokens with the permissions they need",
                    if held.is_empty() {
                        "none".to_owned()
                    } else {
                        listed(&held)
                    }
                )));
            }
            if self.connection.is_some() {
                let held: Vec<Permission> = self.permissions.iter().copied().collect();
                return Err(ApiError::forbidden(format!(
                    "this request needs the permission `{permission}`, which this connection \
                     does not use in the organization {}: it uses {} there, the permissions of \
                     its access (read or run) that its person's own grant gives now. A \
                     connection never administers; its person changes its access from the \
                     dashboard",
                    self.tenant,
                    if held.is_empty() {
                        "none".to_owned()
                    } else {
                        listed(&held)
                    }
                )));
            }
            return Err(ApiError::forbidden(match &self.role {
                None => format!(
                    "{who} has no grant in the tenant {}, so no permission: this request needs \
                     the permission `{permission}`. A grant gives a role to a key or a member",
                    self.tenant
                ),
                Some(role) => format!(
                    "this request needs the permission `{permission}`, which the role {role} of \
                     {who} does not give: use a key whose role gives it (GET /v1/whoami lists \
                     the permissions of this one)"
                ),
            }));
        }
        if self.scoped() && ADMINISTRATION.contains(&permission) {
            return Err(ApiError::forbidden(format!(
                "{who} is limited to labels, and a grant limited to labels cannot administer \
                 the organization: `{permission}` needs a grant without allowed labels"
            )));
        }
        Ok(())
    }

    /// Refuses the request (403) unless the caller is the deployment's operator.
    pub(crate) fn require_operator(&self, what: &str) -> Result<(), ApiError> {
        if self.operator() {
            return Ok(());
        }
        Err(ApiError::forbidden(format!(
            "only the deployment's operator may {what}: use the key given with --admin-key"
        )))
    }

    /// The permissions of `permissions` the caller does not hold, in the catalog's order.
    pub(crate) fn lacks(&self, permissions: &BTreeSet<Permission>) -> Vec<Permission> {
        permissions.difference(&self.permissions).copied().collect()
    }

    /// Whether the caller sees a run carrying `labels`.
    pub(crate) fn sees(&self, labels: &BTreeMap<String, String>) -> bool {
        self.labels
            .iter()
            .all(|(key, value)| labels.get(key) == Some(value))
    }

    /// Whether the caller sees the run: it carries the caller's labels, and it is in the
    /// namespace the request is bound to, if any.
    pub(crate) fn sees_run(&self, head: &RunHead) -> bool {
        self.sees(&head.metadata.labels) && self.reaches(&head.namespace)
    }

    /// Whether the request reaches `namespace`: any, unless it is bound to one.
    pub(crate) fn reaches(&self, namespace: &NamespaceName) -> bool {
        self.namespace
            .as_ref()
            .is_none_or(|bound| bound == namespace)
    }

    /// The namespace the request acts for, required to create in one: a token's, or the
    /// header's (400 without it).
    pub(crate) fn namespace_required(&self, what: &str) -> Result<NamespaceName, ApiError> {
        self.namespace.clone().ok_or_else(|| {
            ApiError::invalid(format!(
                "the header Rewind-Namespace is required to {what}: it names the end user (the \
                 namespace) the request acts for. There is no default namespace"
            ))
        })
    }

    /// Binds the request to the namespace `Rewind-Namespace` names: a key's or a member's acts
    /// for it; a token's may repeat its own, and naming another is refused (403).
    pub(crate) fn bind(&mut self, header: Option<NamespaceName>) -> Result<(), ApiError> {
        let Some(header) = header else {
            return Ok(());
        };
        if self.token.is_some() {
            if self.namespace.as_ref() != Some(&header) {
                return Err(ApiError::forbidden(
                    "this end user's token reaches its own namespace only: Rewind-Namespace may \
                     repeat it or be left out, and names no other",
                ));
            }
            return Ok(());
        }
        self.namespace = Some(header);
        Ok(())
    }

    /// Whether the caller's grant is limited to labels.
    pub(crate) fn scoped(&self) -> bool {
        !self.labels.is_empty()
    }

    /// What the caller may use, in the catalog's order: its role's permissions, without the
    /// administration when its grant is limited to labels.
    pub(crate) fn effective_permissions(&self) -> Vec<Permission> {
        Permission::ALL
            .into_iter()
            .filter(|permission| self.permissions.contains(permission))
            .filter(|permission| !(self.scoped() && ADMINISTRATION.contains(permission)))
            .collect()
    }

    /// The labels of something the caller creates (a run, a subscription): `labels`, with the
    /// grant's allowed labels added. A label that contradicts the grant's is refused.
    pub(crate) fn within(
        &self,
        mut labels: BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, ApiError> {
        for (key, value) in &self.labels {
            match labels.get(key) {
                Some(given) if given != value => {
                    return Err(ApiError::forbidden(format!(
                        "{} only sees runs labelled {key}:{value}, not {key}:{given}",
                        self.who()
                    )));
                }
                Some(_) => {}
                None => {
                    labels.insert(key.clone(), value.clone());
                }
            }
        }
        Ok(labels)
    }
}

/// The permissions of `role` in `tenant`: a built-in role's and the operator's are fixed, a
/// role of the tenant's are read from the store, ignoring a name the catalog does not know.
/// `None` for a role that does not exist.
pub(crate) async fn role_permissions(
    store: &impl AccessStore,
    tenant: &TenantId,
    role: &str,
) -> Result<Option<BTreeSet<Permission>>, StoreError> {
    if let Some(fixed) = fixed_permissions(role) {
        return Ok(Some(fixed.iter().copied().collect()));
    }
    Ok(store.get_role(tenant, role).await?.map(|role| {
        role.permissions
            .iter()
            .filter_map(|name| Permission::parse(name))
            .collect()
    }))
}

/// Permissions as a message lists them: `runs:start`, `runs:delete`.
pub(crate) fn listed(permissions: &[Permission]) -> String {
    permissions
        .iter()
        .map(|permission| format!("`{permission}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A member signed in on the dashboard: what their access token says, checked by the identity
/// provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub(crate) user: UserId,
    pub(crate) session: SessionId,
    /// The token's organization, the tenant; `None` reaches only the onboarding endpoints.
    pub(crate) organization: Option<OrganizationId>,
    /// When the token expires: an event stream it opened is closed then.
    pub(crate) expires_at: Timestamp,
}

/// The onboarding endpoints' caller: a key or the socket's operator (`caller` only), a member of
/// an organization (both), a member without organization (`member` only), or a person's
/// connection (`connection`, with `caller` when its request names an organization).
#[derive(Debug, Clone)]
pub(crate) struct Who {
    pub(crate) caller: Option<Caller>,
    pub(crate) member: Option<Member>,
    pub(crate) connection: Option<SignedConnection>,
}

/// The expiry of the caller's access token, a member's or a connection's, in the world's time;
/// `None` for a key, an end user's token and over the socket.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TokenExpiry(pub(crate) Option<Timestamp>);

/// What a member without organization is told everywhere but the onboarding endpoints.
fn no_organization() -> ApiError {
    ApiError::forbidden(
        "this access token has no organization, so it reaches only GET /v1/whoami, POST \
         /v1/organizations, GET /v1/invitations and POST /v1/invitations/{id}/accept: create an \
         organization or accept an invitation, then choose it with POST /auth/session and its \
         organizationId, which gives an access token for it",
    )
}

fn unauthenticated() -> ApiError {
    ApiError::new(ProblemType::Unauthorized, "no API key nor access token")
}

pub(crate) fn unauthorized(detail: impl Into<String>) -> ApiError {
    ApiError::new(ProblemType::Unauthorized, detail)
}

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        if let Some(caller) = parts.extensions.get::<Caller>() {
            let mut caller = caller.clone();
            repeat_organization(&caller, &parts.headers)?;
            caller.bind(namespace_header(&parts.headers)?)?;
            return Ok(caller);
        }
        if parts.extensions.get::<SignedConnection>().is_some() {
            return Err(connections::organization_required());
        }
        if parts.extensions.get::<Member>().is_some() {
            return Err(no_organization());
        }
        Err(unauthenticated())
    }
}

/// The organization `Rewind-Organization` names, if any; a header that is not a tenant's id is
/// refused (400).
pub(crate) fn organization_header(
    headers: &axum::http::HeaderMap,
) -> Result<Option<TenantId>, ApiError> {
    let Some(value) = headers.get(ORGANIZATION) else {
        return Ok(None);
    };
    let invalid = || {
        ApiError::invalid(
            "Rewind-Organization: expected an organization's id, org_… (GET /v1/whoami lists \
             the organizations a connection opens)",
        )
    };
    let text = value.to_str().map_err(|_| invalid())?.trim();
    TenantId::new(text).map(Some).map_err(|_| invalid())
}

/// Refuses (403) a request whose `Rewind-Organization` names another tenant than the caller's:
/// a key, a member or a token may only repeat their own.
fn repeat_organization(caller: &Caller, headers: &axum::http::HeaderMap) -> Result<(), ApiError> {
    match organization_header(headers)? {
        Some(named) if named != caller.tenant => Err(ApiError::forbidden(format!(
            "{} acts in the organization {} only: Rewind-Organization may repeat it or be left \
             out, and names no other",
            caller.who(),
            caller.tenant
        ))),
        _ => Ok(()),
    }
}

/// The namespace `Rewind-Namespace` names, if any; a header that is not a namespace's name is
/// refused (400).
pub(crate) fn namespace_header(
    headers: &axum::http::HeaderMap,
) -> Result<Option<NamespaceName>, ApiError> {
    let Some(value) = headers.get(NAMESPACE) else {
        return Ok(None);
    };
    let invalid = || {
        ApiError::invalid(
            "Rewind-Namespace: expected 1 to 128 ASCII letters, digits, -, _, ., : and @, not starting with rewind:",
        )
    };
    let text = value.to_str().map_err(|_| invalid())?;
    NamespaceName::new(text).map(Some).map_err(|_| invalid())
}

impl<S: Send + Sync> FromRequestParts<S> for Who {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        let who = Who {
            caller: parts.extensions.get::<Caller>().cloned(),
            member: parts.extensions.get::<Member>().cloned(),
            connection: parts.extensions.get::<SignedConnection>().cloned(),
        };
        if who.caller.is_none() && who.member.is_none() && who.connection.is_none() {
            return Err(unauthenticated());
        }
        if let Some(caller) = &who.caller {
            repeat_organization(caller, &parts.headers)?;
        }
        Ok(who)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for TokenExpiry {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        Ok(TokenExpiry(
            parts
                .extensions
                .get::<Member>()
                .map(|member| member.expires_at)
                .or_else(|| {
                    parts
                        .extensions
                        .get::<SignedConnection>()
                        .map(|connection| connection.expires_at)
                }),
        ))
    }
}

/// The SHA-256 of a key: how the store finds it.
pub(crate) fn key_hash(key: &str) -> Digest {
    Digest::of(key.as_bytes())
}

/// A new key: `rewind_sk_` and 256 bits from the operating system's CSPRNG, in base 62.
pub(crate) fn new_key() -> Result<String, ApiError> {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut bytes = [0u8; 43];
    getrandom::fill(&mut bytes).map_err(ApiError::internal)?;
    // 43 characters of base 62 hold 256 bits; the bias of `% 62` on a byte is below 2 %.
    let key: String = bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect();
    Ok(format!("{API_KEY_PREFIX}{key}"))
}

/// The middleware of the TCP listener: reads the Bearer token and puts its [`Caller`] (in a
/// tenant), its [`Member`] (a member's access token) and its [`SignedConnection`] (a
/// connection's access token) in the request's extensions, or answers 401.
pub(crate) async fn authenticate(
    State(app): State<Arc<App>>,
    mut request: Request,
    next: Next,
) -> Response {
    match credentials(&app, request.headers()).await {
        Ok(found) => {
            if let Some(caller) = found.caller {
                request.extensions_mut().insert(caller);
            }
            if let Some(member) = found.member {
                request.extensions_mut().insert(member);
            }
            if let Some(connection) = found.connection {
                request.extensions_mut().insert(connection);
            }
            next.run(request).await
        }
        Err(error) => error.into_response(),
    }
}

/// What a Bearer token says.
#[derive(Debug, Default)]
pub(crate) struct Credentials {
    /// The caller in a tenant.
    pub(crate) caller: Option<Caller>,
    /// A member signed in on the dashboard.
    pub(crate) member: Option<Member>,
    /// A person's connection.
    pub(crate) connection: Option<SignedConnection>,
}

impl Credentials {
    fn caller(caller: Caller) -> Self {
        Self {
            caller: Some(caller),
            ..Self::default()
        }
    }
}

/// The Bearer token of `Authorization`, refused (401) when absent or not `Bearer <token>`.
pub(crate) fn bearer(headers: &axum::http::HeaderMap) -> Result<&str, ApiError> {
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err(unauthorized(
            "no token: send an API key as `Authorization: Bearer rewind_sk_…`, an end user's \
             token, a connection's access token, or a member's access token from POST \
             /auth/session",
        ));
    };
    // The scheme is case-insensitive (RFC 9110, section 11.1).
    value
        .to_str()
        .ok()
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| {
            unauthorized(
                "the Authorization header must be `Bearer <token>`: an API key rewind_sk_… or a \
                 member's access token",
            )
        })
}

async fn credentials(app: &App, headers: &axum::http::HeaderMap) -> Result<Credentials, ApiError> {
    let token = bearer(headers)?;
    if token.starts_with(CONNECTION_TOKEN_PREFIX) {
        let connection = connections::authenticate(app, token).await?;
        let caller = match organization_header(headers)? {
            Some(tenant) => Some(Caller::of_connection(&app.store, &connection, tenant).await?),
            None => None,
        };
        return Ok(Credentials {
            caller,
            member: None,
            connection: Some(connection),
        });
    }
    tenant_credentials(app, token).await
}

/// What a Bearer token other than a connection's says: a key, an end user's token, or a
/// member's access token.
pub(crate) async fn tenant_credentials(app: &App, token: &str) -> Result<Credentials, ApiError> {
    if token.starts_with(API_KEY_PREFIX) {
        let stored = app.store.find_key(key_hash(token)).await?;
        let Some(stored) = stored.filter(|stored| stored.revoked_at.is_none()) else {
            return Err(unauthorized("this API key is unknown or revoked"));
        };
        let principal = stored.principal();
        return Ok(Credentials::caller(
            Caller::of(&app.store, stored.tenant, principal).await?,
        ));
    }
    if token.starts_with(TOKEN_PREFIX) {
        return Ok(Credentials::caller(opaque_token(app, token).await?));
    }
    if let Some(header) = jsonwebtoken::decode_header(token).ok().filter(|header| {
        header
            .kid
            .as_deref()
            .is_some_and(|kid| kid.starts_with(TOKEN_KEY_PREFIX))
    }) {
        return Ok(Credentials::caller(signed_token(app, token, header).await?));
    }
    // A worker, and a server without WorkOS, accept only keys and end users' tokens.
    let Some(sign_in) = &app.sign_in else {
        return Err(unauthorized(
            "this server accepts only API keys (`Authorization: Bearer rewind_sk_…`) and end \
             users' tokens: it has no sign-in, so no member's access token is valid here",
        ));
    };
    let claims =
        sign_in
            .identity
            .verify_access_token(token)
            .await
            .map_err(|error| match error {
                IdentityError::Unavailable(_) => ApiError::from(error),
                _ => unauthorized(
                    "the access token is invalid or expired: POST /auth/session gives a valid one",
                ),
            })?;
    let member = Member {
        user: claims.user,
        session: claims.session,
        organization: claims.organization,
        expires_at: claims.expires_at,
    };
    let Some(organization) = &member.organization else {
        return Ok(Credentials {
            member: Some(member),
            ..Credentials::default()
        });
    };
    let tenant = organization.tenant().map_err(|_| {
        unauthorized("the access token's organization is not a valid tenant id: sign in again")
    })?;
    app.ensure_tenant(&tenant).await?;
    let caller = Caller::of(&app.store, tenant, Principal::from(&member.user)).await?;
    Ok(Credentials {
        caller: Some(caller),
        member: Some(member),
        connection: None,
    })
}

/// The permissions a token holds, as the store or its claims name them: those a token may
/// hold, others ignored.
fn token_permissions<'a>(names: impl IntoIterator<Item = &'a String>) -> BTreeSet<Permission> {
    names
        .into_iter()
        .filter_map(|name| Permission::parse(name))
        .filter(|permission| permission.in_token())
        .collect()
}

/// The caller of an opaque token: live, of a namespace that is not being erased.
async fn opaque_token(app: &App, token: &str) -> Result<Caller, ApiError> {
    let hash = key_hash(token);
    let stored = app.store.find_token(hash).await?;
    let Some(stored) = stored.filter(|stored| stored.is_live(app.now())) else {
        return Err(unauthorized(
            "this end user's token is unknown, revoked or expired: ask the service that gave it \
             for a new one",
        ));
    };
    refuse_erased(app, &stored.tenant, &stored.namespace).await?;
    Ok(Caller {
        tenant: stored.tenant,
        principal: None,
        role: None,
        labels: stored.labels,
        granted_by: None,
        granted_at: None,
        permissions: token_permissions(&stored.permissions),
        namespace: Some(stored.namespace),
        token: Some(EndUserToken {
            form: TokenForm::Opaque { hash },
            expires_at: stored.expires_at,
        }),
        connection: None,
    })
}

/// Refuses (401) a token whose namespace is being erased: it takes no token any more.
async fn refuse_erased(
    app: &App,
    tenant: &TenantId,
    namespace: &NamespaceName,
) -> Result<(), ApiError> {
    let erased = app
        .store
        .get_namespace(tenant, namespace)
        .await?
        .is_some_and(|namespace| namespace.deleting_since.is_some());
    if erased {
        return Err(unauthorized(
            "the end user of this token is being erased: its tokens are refused",
        ));
    }
    Ok(())
}

/// The caller of a token the customer signed with a registered key: a valid signature by a key
/// that still exists, the audience `rewind`, an expiry at most 30 days after its issue, not
/// past, permissions the key may give, issued after its namespace's `tokens_revoked_before`. Its
/// namespace is created at its first use.
async fn signed_token(
    app: &App,
    token: &str,
    header: jsonwebtoken::Header,
) -> Result<Caller, ApiError> {
    let kid = header.kid.unwrap_or_default();
    // A `kid` holding a NUL character is no key's, and never reaches a store.
    let key = if kid.contains('\0') {
        None
    } else {
        app.store.find_token_key(&kid).await?
    };
    let Some(key) = key else {
        return Err(unauthorized(format!(
            "the key {kid} that signed this token is unknown or was deleted: its tokens are \
             refused"
        )));
    };
    let public = PublicKey::stored(&key.jwk)
        .ok_or_else(|| ApiError::internal(format!("the stored key {kid} cannot be read")))?;
    let algorithm = public.jwt_algorithm();
    if header.alg != algorithm {
        return Err(unauthorized(format!(
            "this token is signed with {:?}, but its key {kid} signs with {algorithm:?}",
            header.alg
        )));
    }
    let decoding = public
        .decoding_key()
        .ok_or_else(|| ApiError::internal(format!("the stored key {kid} cannot be read")))?;
    let mut validation = jsonwebtoken::Validation::new(algorithm);
    validation.set_required_spec_claims(&["exp", "iat", "sub", "aud"]);
    validation.set_audience(&[TOKEN_AUDIENCE]);
    validation.leeway = LEEWAY_SECONDS;
    validation.validate_exp = true;
    validation.validate_nbf = true;
    let claims = jsonwebtoken::decode::<SignedTokenClaims>(token, &decoding, &validation)
        .map_err(|error| {
            unauthorized(format!(
                "this signed token is refused ({error}): it must be signed by its key, for the \
                 audience {TOKEN_AUDIENCE:?}, name its namespace in sub, and not be expired"
            ))
        })?
        .claims;
    if rewind_engine::text::refuse_nul_in_labels(&claims.labels.0).is_err() {
        return Err(unauthorized(
            "this signed token's labels hold a NUL character (U+0000), which no label holds",
        ));
    }
    let lifetime = claims.exp.saturating_sub(claims.iat);
    let max = i64::try_from(TOKEN_MAX_TTL.as_secs()).unwrap_or(i64::MAX);
    if lifetime <= 0 || lifetime > max {
        return Err(unauthorized(
            "this signed token's exp must come after its iat, by 30 days at most",
        ));
    }
    let given = token_permissions(&key.permissions);
    let permissions = match &claims.permissions {
        Some(asked) => {
            let asked: BTreeSet<Permission> = asked.iter().copied().collect();
            if let Some(refused) = asked.iter().find(|permission| !given.contains(permission)) {
                return Err(unauthorized(format!(
                    "this signed token asks for the permission `{refused}`, which its key {kid} \
                     may not give"
                )));
            }
            asked
        }
        None => Permission::TOKEN_DEFAULT
            .into_iter()
            .filter(|permission| given.contains(permission))
            .collect(),
    };
    let issued_at = Timestamp::from_millis(u64::try_from(claims.iat).unwrap_or(0) * 1000);
    let name = crate::views::engine_namespace(&claims.sub);
    let namespace = match app.store.get_namespace(&key.tenant, &name).await? {
        Some(namespace) => namespace,
        None => match app
            .store
            .put_namespace(&key.tenant, &name, None, app.config.partitions, app.now())
            .await?
        {
            NamespaceWrite::Written(namespace) => namespace,
            NamespaceWrite::Deleting => {
                return Err(unauthorized(
                    "the end user of this token is being erased: its tokens are refused",
                ));
            }
        },
    };
    if namespace.deleting_since.is_some() {
        return Err(unauthorized(
            "the end user of this token is being erased: its tokens are refused",
        ));
    }
    if namespace
        .tokens_revoked_before
        .is_some_and(|revoked| issued_at < revoked)
    {
        return Err(unauthorized(
            "this signed token was issued before its namespace's tokens were revoked: sign a new \
             one",
        ));
    }
    Ok(Caller {
        tenant: key.tenant,
        principal: None,
        role: None,
        labels: claims.labels.0,
        granted_by: None,
        granted_at: None,
        permissions,
        namespace: Some(name),
        token: Some(EndUserToken {
            form: TokenForm::Signed {
                kid,
                jti: claims.jti,
                issued_at,
            },
            expires_at: Timestamp::from_millis(u64::try_from(claims.exp).unwrap_or(0) * 1000),
        }),
        connection: None,
    })
}

/// Whether the caller's end user's token or connection still authenticates, as it did when its
/// request opened an event stream: a revocation, an expiry, a deleted key or an erasure closes
/// the stream; for a connection, its revocation or its grant removed too. Always for any other
/// caller.
pub(crate) async fn still_valid(app: &App, caller: &Caller) -> bool {
    if let Some(connection) = &caller.connection {
        return connections::still_valid(app, connection, &caller.tenant).await;
    }
    let (Some(token), Some(name)) = (&caller.token, &caller.namespace) else {
        return true;
    };
    let namespace = match app.store.get_namespace(&caller.tenant, name).await {
        Ok(namespace) => namespace,
        Err(_) => return false,
    };
    if namespace
        .as_ref()
        .is_some_and(|namespace| namespace.deleting_since.is_some())
    {
        return false;
    }
    match &token.form {
        TokenForm::Opaque { hash } => match app.store.find_token(*hash).await {
            Ok(Some(stored)) => stored.is_live(app.now()),
            _ => false,
        },
        TokenForm::Signed { kid, issued_at, .. } => {
            let key = app.store.find_token_key(kid).await;
            let key_kept = matches!(key, Ok(Some(key)) if key.tenant == caller.tenant);
            let not_revoked = namespace
                .and_then(|namespace| namespace.tokens_revoked_before)
                .is_none_or(|revoked| *issued_at >= revoked);
            key_kept && not_revoked && SystemClock.now() < token.expires_at
        }
    }
}

/// Whether the caller that opened a stream may still read what it reads: an end user's token
/// or a connection that still authenticates ([`still_valid`]); for a key, a member or a
/// connection, a grant as the store holds it now that still gives `permission`, with the same
/// labels. A key revoked or deleted, a grant removed, changed to other labels, or whose role no
/// longer gives the permission: false; a store that cannot be read too. The socket's caller
/// always may.
pub(crate) async fn still_holds(app: &App, caller: &Caller, permission: Permission) -> bool {
    if !still_valid(app, caller).await {
        return false;
    }
    // An end user's token holds what it was minted with: its revocation is its only change.
    if caller.token.is_some() {
        return true;
    }
    let now = if let Some(connection) = &caller.connection {
        match Caller::of_connection(&app.store, connection, caller.tenant.clone()).await {
            Ok(now) => now,
            Err(_) => return false,
        }
    } else {
        let Some(principal) = &caller.principal else {
            return true;
        };
        if let Principal::Key(hash) = principal {
            let kept = matches!(
                app.store.find_key(*hash).await,
                Ok(Some(key)) if key.revoked_at.is_none() && key.tenant == caller.tenant
            );
            if !kept {
                return false;
            }
        }
        match Caller::of(&app.store, caller.tenant.clone(), principal.clone()).await {
            Ok(now) => now,
            Err(_) => return false,
        }
    };
    now.permissions.contains(&permission) && now.labels == caller.labels
}

#[cfg(test)]
mod tests {
    use rewind_api::access::BuiltInRole;
    use rewind_engine::adapters::MemoryAccess;
    use rewind_engine::ports::{Grant, Role};

    use super::*;

    #[test]
    fn keys_are_long_and_prefixed() {
        let key = new_key().unwrap_or_default();
        assert!(key.starts_with("rewind_sk_"));
        assert_eq!(key.len(), "rewind_sk_".len() + 43);
        assert_ne!(new_key().unwrap_or_default(), key);
    }

    fn acme() -> TenantId {
        TenantId::new("acme").unwrap_or_else(|_| TenantId::local())
    }

    fn key(secret: &str) -> Principal {
        Principal::Key(Digest::of(secret.as_bytes()))
    }

    async fn grant(
        store: &MemoryAccess,
        principal: &Principal,
        role: &str,
        labels: &[(&str, &str)],
    ) {
        let grant = Grant {
            tenant: acme(),
            principal: principal.clone(),
            role: role.to_owned(),
            allowed_labels: labels
                .iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
            granted_by: None,
            granted_at: Timestamp(0),
        };
        assert!(store.put_grant(&grant, None).await.is_ok());
    }

    async fn caller(store: &MemoryAccess, principal: &Principal) -> Caller {
        Caller::of(store, acme(), principal.clone())
            .await
            .unwrap_or_else(|_| Caller::local())
    }

    fn detail(result: Result<(), ApiError>) -> String {
        result.err().map(|error| error.0.detail).unwrap_or_default()
    }

    #[tokio::test]
    async fn a_grant_gives_its_role_s_permissions() {
        let store = MemoryAccess::new();
        let reader = key("reader");
        grant(&store, &reader, BuiltInRole::Reader.as_str(), &[]).await;
        let reader = caller(&store, &reader).await;
        assert!(reader.require(Permission::RunsRead).is_ok());
        let refused = detail(reader.require(Permission::RunsStart));
        assert!(refused.contains("`runs:start`"), "{refused}");
        assert!(refused.contains("reader"), "{refused}");

        let triage = Role {
            tenant: acme(),
            name: "triage".into(),
            description: String::new(),
            permissions: ["runs:read", "runs:control", "runs:everything"]
                .map(String::from)
                .into(),
            created_at: Timestamp(0),
            updated_at: Timestamp(0),
        };
        assert!(store.put_role(&triage).await.is_ok());
        let triager = key("triager");
        grant(&store, &triager, "triage", &[]).await;
        let triager = caller(&store, &triager).await;
        assert_eq!(
            triager.permissions,
            BTreeSet::from([Permission::RunsRead, Permission::RunsControl]),
            "a name the catalog does not know gives nothing"
        );

        let gone = key("gone");
        grant(&store, &gone, "deleted-role", &[]).await;
        let gone = caller(&store, &gone).await;
        assert!(gone.permissions.is_empty(), "an unknown role gives nothing");
        assert!(gone.require(Permission::RunsRead).is_err());

        let stranger = caller(&store, &key("stranger")).await;
        assert_eq!(stranger.role, None);
        let refused = detail(stranger.require(Permission::RunsRead));
        assert!(refused.contains("no grant"), "{refused}");

        let operator = key("operator");
        grant(&store, &operator, OPERATOR, &[]).await;
        let operator = caller(&store, &operator).await;
        assert!(operator.operator());
        assert_eq!(operator.permissions.len(), Permission::ALL.len());
        assert!(operator.require_operator("stop").is_ok());
        assert!(reader.require_operator("stop").is_err());
        assert!(Caller::local().operator());
        assert_eq!(Caller::local().permissions, operator.permissions);
    }

    #[tokio::test]
    async fn a_grant_limited_to_labels_does_not_administer() {
        let store = MemoryAccess::new();
        let scoped = key("scoped");
        grant(
            &store,
            &scoped,
            BuiltInRole::Owner.as_str(),
            &[("team", "a")],
        )
        .await;
        let scoped = caller(&store, &scoped).await;
        assert!(scoped.require(Permission::RunsStart).is_ok());
        assert!(scoped.require(Permission::MembersRead).is_ok());
        let effective = scoped.effective_permissions();
        assert_eq!(
            effective.len(),
            Permission::ALL.len() - ADMINISTRATION.len()
        );
        assert!(
            ADMINISTRATION
                .iter()
                .all(|admin| !effective.contains(admin))
        );
        assert_eq!(
            Caller::local().effective_permissions(),
            Permission::ALL,
            "without labels, the role's permissions"
        );
        for permission in ADMINISTRATION {
            let refused = detail(scoped.require(permission));
            assert!(
                refused.contains("limited to labels"),
                "{permission}: {refused}"
            );
        }
    }

    #[test]
    fn a_request_is_bound_to_its_token_s_namespace_or_its_header_s() {
        let alice = NamespaceName::new("alice").unwrap_or_else(|_| NamespaceName::migrated());
        let bob = NamespaceName::new("bob").unwrap_or_else(|_| NamespaceName::migrated());
        let mut key = Caller::local();
        assert!(key.reaches(&bob), "without the header, every namespace");
        assert!(key.namespace_required("start a run").is_err());
        assert!(key.bind(Some(alice.clone())).is_ok());
        assert!(key.reaches(&alice) && !key.reaches(&bob));
        assert_eq!(
            key.namespace_required("start a run").ok(),
            Some(alice.clone())
        );

        let token = Caller {
            role: None,
            permissions: Permission::TOKEN_DEFAULT.into_iter().collect(),
            namespace: Some(alice.clone()),
            token: Some(EndUserToken {
                form: TokenForm::Opaque {
                    hash: Digest::of(b"rewind_nt_alice"),
                },
                expires_at: Timestamp(0),
            }),
            connection: None,
            ..Caller::local()
        };
        let mut repeated = token.clone();
        assert!(repeated.bind(Some(alice.clone())).is_ok());
        assert!(repeated.bind(None).is_ok());
        assert_eq!(repeated.namespace, Some(alice.clone()));
        let mut astray = token.clone();
        let refused = detail(astray.bind(Some(bob.clone())));
        assert!(refused.contains("its own namespace only"), "{refused}");
        let refused = detail(token.require(Permission::RunsDelete));
        assert!(refused.contains("`runs:delete`"), "{refused}");
        assert!(refused.contains("token"), "{refused}");
        assert_eq!(token.who(), "this token");
    }

    #[test]
    fn labels_scope_what_a_key_creates() {
        let scoped = Caller {
            labels: BTreeMap::from([("conversation".into(), "c-1".into())]),
            ..Caller::local()
        };
        let labels = scoped
            .within(BTreeMap::from([("agent".into(), "triage".into())]))
            .unwrap_or_default();
        assert_eq!(labels.len(), 2);
        assert!(scoped.sees(&labels));
        assert!(
            scoped
                .within(BTreeMap::from([("conversation".into(), "c-2".into())]))
                .is_err()
        );
        assert!(!scoped.sees(&BTreeMap::new()));
    }
}
