//! Authentication and access (`archi/06-storage-and-scale.md`, "Tenancy, identity and access").
//!
//! Every request to `/v1` carries an API key as a Bearer token, `rewind_sk_…`. The access store
//! keeps the SHA-256 of keys, never a key. A key belongs to a tenant, which scopes everything it
//! sees; what it may do is its grant, held by the principal `key:<hash>`: a role, and optionally
//! allowed labels. A built-in role's permissions and the operator's are fixed
//! (`rewind_api::access`); a role of the tenant is read from the store; any other role gives
//! none, as does a principal without a grant. Every request reads the grant and the role: a
//! change applies from the next request.
//!
//! Every endpoint requires one permission of the catalog ([`Caller::require`]), answering 403 with
//! the missing permission. A grant limited to labels sees only the runs carrying all of them, and
//! cannot administer: the permissions of [`ADMINISTRATION`] are refused to it.
//!
//! The operator (role `operator`) holds every permission in its own tenant, reaches the keys of
//! every tenant, and stops the embedded server. The embedded server's socket takes no key: its
//! one caller is the operator of the tenant `local`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::{FromRequestParts, Request, State};
use axum::http::header;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::access::{OPERATOR, Permission, fixed_permissions};
use rewind_api::headers::API_KEY_PREFIX;
use rewind_core::{Digest, Timestamp};
use rewind_engine::ports::{AccessStore, Principal, StoreError, TenantId};

use crate::app::App;
use crate::error::ApiError;

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
    /// What the role gives.
    pub(crate) permissions: BTreeSet<Permission>,
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
        })
    }

    /// Whether the caller is the deployment's operator.
    pub(crate) fn operator(&self) -> bool {
        self.role.as_deref() == Some(OPERATOR)
    }

    /// The caller, as a message names it.
    fn who(&self) -> &'static str {
        match self.principal {
            Some(Principal::Key(_)) => "this key",
            Some(Principal::User(_)) => "this member",
            Some(Principal::Invitation(_)) | None => "this caller",
        }
    }

    /// Refuses the request (403) unless the caller's grant gives `permission`; a grant limited to
    /// labels never administers.
    pub(crate) fn require(&self, permission: Permission) -> Result<(), ApiError> {
        let who = self.who();
        if !self.permissions.contains(&permission) {
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

    /// Whether the caller's grant is limited to labels.
    pub(crate) fn scoped(&self) -> bool {
        !self.labels.is_empty()
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

impl<S: Send + Sync> FromRequestParts<S> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, ApiError> {
        parts
            .extensions
            .get::<Caller>()
            .cloned()
            .ok_or_else(|| ApiError::new(ProblemType::Unauthorized, "no API key"))
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

/// The middleware of the TCP listener: reads the Bearer key and puts its [`Caller`] in the
/// request's extensions, or answers 401.
pub(crate) async fn authenticate(
    State(app): State<Arc<App>>,
    mut request: Request,
    next: Next,
) -> Response {
    match caller(&app, request.headers()).await {
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(error) => error.into_response(),
    }
}

async fn caller(app: &App, headers: &axum::http::HeaderMap) -> Result<Caller, ApiError> {
    let unauthorized = |detail: &str| ApiError::new(ProblemType::Unauthorized, detail);
    let Some(value) = headers.get(header::AUTHORIZATION) else {
        return Err(unauthorized(
            "no API key: send it as `Authorization: Bearer rewind_sk_…`",
        ));
    };
    // The scheme is case-insensitive (RFC 9110, section 11.1).
    let key = value
        .to_str()
        .ok()
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, key)| key.trim())
        .filter(|key| key.starts_with(API_KEY_PREFIX))
        .ok_or_else(|| unauthorized("the Authorization header must be `Bearer rewind_sk_…`"))?;
    let stored = app.store.find_key(key_hash(key)).await?;
    let Some(stored) = stored.filter(|stored| stored.revoked_at.is_none()) else {
        return Err(unauthorized("this API key is unknown or revoked"));
    };
    let principal = stored.principal();
    Ok(Caller::of(&app.store, stored.tenant, principal).await?)
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
        for permission in ADMINISTRATION {
            let refused = detail(scoped.require(permission));
            assert!(
                refused.contains("limited to labels"),
                "{permission}: {refused}"
            );
        }
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
