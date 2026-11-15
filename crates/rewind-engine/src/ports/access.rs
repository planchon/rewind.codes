//! The `AccessStore` port: who may do what (`archi/06-storage-and-scale.md`, "What the caller
//! may do"; `archi/07-code-architecture.md`, "The other ports").
//!
//! A principal (a member, an API key, an invitation) holds at most one grant per tenant; the
//! grant gives a role and optionally allowed labels; a role is a set of permissions. The store
//! keeps the API keys, the tenants' own roles, the grants and each tenant's default role. It
//! never interprets a permission: permissions are strings the server reads, and the built-in
//! roles are not stored, the server knows them. The engine never reads this store.
//!
//! It also keeps end users' tokens: opaque tokens, each bound to one namespace with its own
//! permissions, and the public keys a tenant signs its end users' tokens with
//! (`archi/06-storage-and-scale.md`, "Who calls").

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::str::FromStr;

use rewind_core::{Digest, Timestamp};

use super::journal::StoreError;
use super::namespaces::NamespaceName;
use super::tenancy::TenantId;

/// Who holds a grant: a member, an API key, or an invitation not yet accepted. Written
/// `user:<id>`, `key:<sha256 hex>` and `invitation:<id>`, the form the store keys grants by.
///
/// Principals sort as their text form does.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Principal {
    /// A member: the identity provider's user id (`user_01J…`).
    User(String),
    /// An API key, by the SHA-256 of its secret.
    Key(Digest),
    /// An invitation not yet accepted (`invitation_01J…`): its grant goes to the user who
    /// accepts it.
    Invitation(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid principal {0:?}: expected user:<id> or invitation:<id> (1 to 128 ASCII letters, \
     digits, - and _), or key:<64 hex digits>"
)]
pub struct InvalidPrincipal(pub String);

impl Principal {
    /// The prefix of the text form: `user`, `key` or `invitation`.
    pub fn kind(&self) -> &'static str {
        match self {
            Principal::User(_) => "user",
            Principal::Key(_) => "key",
            Principal::Invitation(_) => "invitation",
        }
    }

    /// Reads `user:<id>`, `key:<hex>` or `invitation:<id>`.
    pub fn parse(text: &str) -> Result<Self, InvalidPrincipal> {
        let invalid = || InvalidPrincipal(text.to_owned());
        let (kind, id) = text.split_once(':').ok_or_else(invalid)?;
        let valid_id = (1..=128).contains(&id.len())
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
        match kind {
            "user" if valid_id => Ok(Principal::User(id.to_owned())),
            "invitation" if valid_id => Ok(Principal::Invitation(id.to_owned())),
            "key" => Digest::from_hex(id)
                .map(Principal::Key)
                .map_err(|_| invalid()),
            _ => Err(invalid()),
        }
    }

    /// The order of the text forms' prefixes: `invitation` < `key` < `user`.
    fn rank(&self) -> u8 {
        match self {
            Principal::Invitation(_) => 0,
            Principal::Key(_) => 1,
            Principal::User(_) => 2,
        }
    }
}

impl fmt::Display for Principal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Principal::User(id) | Principal::Invitation(id) => write!(f, "{}:{id}", self.kind()),
            Principal::Key(hash) => write!(f, "key:{}", hash.to_hex()),
        }
    }
}

impl FromStr for Principal {
    type Err = InvalidPrincipal;

    fn from_str(text: &str) -> Result<Self, InvalidPrincipal> {
        Self::parse(text)
    }
}

impl Ord for Principal {
    /// As the text forms sort: the prefixes differ from their first letter, the ids compare
    /// byte by byte, and a digest's bytes in the order of its lowercase hex.
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Principal::User(a), Principal::User(b))
            | (Principal::Invitation(a), Principal::Invitation(b)) => a.cmp(b),
            (Principal::Key(a), Principal::Key(b)) => a.cmp(b),
            _ => self.rank().cmp(&other.rank()),
        }
    }
}

impl PartialOrd for Principal {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// An API key, as the store keeps it: hashed (`rewind_sk_…` is never stored), with its tenant,
/// its name and who created it. What it may do is its grant, held by [`Principal::Key`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKey {
    /// SHA-256 of the key. Keys are looked up by it.
    pub hash: Digest,
    pub tenant: TenantId,
    /// What the key is for.
    pub name: String,
    /// `None` for the operator's key given at start (`--admin-key`).
    pub created_by: Option<Principal>,
    pub created_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

impl ApiKey {
    /// The principal of the key's grant.
    pub fn principal(&self) -> Principal {
        Principal::Key(self.hash)
    }
}

/// A role of a tenant: a named set of permissions, composed by the organization from the
/// catalog. The built-in roles (`owner`, `harness`, `worker`, `reader`) are not stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Role {
    pub tenant: TenantId,
    pub name: String,
    pub description: String,
    /// Permissions of the catalog, as the server names them (`runs:read`). The store keeps
    /// them without reading them.
    pub permissions: BTreeSet<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A grant: one role, and optionally allowed labels, given to one principal in one tenant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub tenant: TenantId,
    pub principal: Principal,
    /// A built-in role, `operator`, or a role of the tenant. A grant whose role no longer
    /// exists gives no permission.
    pub role: String,
    /// When not empty, the principal only sees the runs carrying all of these labels.
    pub allowed_labels: BTreeMap<String, String>,
    /// `None` for the grants the server writes itself: the creator's `owner`, a member's
    /// default role, the operator's key.
    pub granted_by: Option<Principal>,
    pub granted_at: Timestamp,
}

/// An end user's opaque token, as the store keeps it: hashed (`rewind_nt_…` is never stored),
/// bound to one namespace, with the permissions it was given. It holds no grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// SHA-256 of the token: its id. Tokens are looked up by it.
    pub hash: Digest,
    pub tenant: TenantId,
    pub namespace: NamespaceName,
    /// Permissions of the catalog, as the server names them, among those a token may hold.
    pub permissions: BTreeSet<String>,
    /// When not empty, the token only reaches the runs carrying all of these labels.
    pub labels: BTreeMap<String, String>,
    pub expires_at: Timestamp,
    /// The key or member that minted it.
    pub created_by: Principal,
    pub created_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

impl Token {
    /// Whether the token authenticates at `now`: neither revoked nor expired.
    pub fn is_live(&self, now: Timestamp) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

/// A public key a tenant signs its end users' tokens with: a JWK, named `rk_…` (the `kid` of
/// the tokens it signs), with the permissions its tokens may carry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenKey {
    /// `rk_…`, unique across tenants.
    pub kid: String,
    pub tenant: TenantId,
    /// What the key is for.
    pub name: String,
    /// The public key, as the JSON text of a JWK. The store keeps it without reading it.
    pub jwk: String,
    /// The permissions its tokens may carry, as the server names them.
    pub permissions: BTreeSet<String>,
    pub created_by: Principal,
    pub created_at: Timestamp,
}

/// What [`AccessStore::delete_role`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleDeletion {
    Deleted,
    NotFound,
    /// A grant of the tenant points to the role: nothing was deleted.
    InUse,
}

/// What [`AccessStore::put_grant`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantWrite {
    Written,
    /// The principal holds the last grant of the kept role in the tenant, and the write would
    /// take it away: nothing changed.
    LastHolder,
}

/// What [`AccessStore::delete_grant`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantDeletion {
    Deleted,
    NotFound,
    /// The grant is the last of the kept role in the tenant: nothing was deleted.
    LastHolder,
}

/// What [`AccessStore::claim_grant`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantClaim {
    /// The invitation's grant is now the user's, as it was given.
    Claimed(Grant),
    /// The user already had a grant in the tenant: it stays, and the invitation's grant, if
    /// any, is deleted.
    AlreadyGranted(Grant),
    /// Neither the invitation nor the user has a grant in the tenant.
    NoGrant,
}

/// API keys, the tenants' own roles, grants and each tenant's default role
/// (`archi/07-code-architecture.md`, "The other ports").
///
/// The contract, checked by the conformance suite (feature `conformance`, module
/// `conformance::access`):
///
/// - **Tenants.** Roles, grants and default roles are keyed by tenant: one tenant never sees
///   another's. Keys are found by their hash, which is unique across tenants, and each key
///   names its tenant.
/// - **One grant per principal.** A principal has at most one grant per tenant: `put_grant`
///   replaces it.
/// - **A kept role keeps a holder.** `put_grant` and `delete_grant` take `keep`, a role (the
///   server passes `owner`): a write that would take that role away from the last member
///   (`Principal::User`) holding it in the tenant changes nothing and answers `LastHolder`.
///   Keys and pending invitations never count as holders. The check and the write are atomic.
/// - **A role in use stays.** `delete_role` deletes nothing while a grant of the tenant points
///   to the role, decided in the same transaction.
/// - **Claims.** `claim_grant` moves an invitation's grant to the user who accepted it, unless
///   the user already has one, atomically.
/// - **Tokens.** Found by their hash, unique across tenants; `revoke_tokens` revokes every live
///   token of one namespace of one tenant, and `count_tokens` counts the live ones;
///   `purge_tokens` deletes, in every tenant, the tokens expired or revoked before a time. Token
///   keys are found by their `kid`, unique across tenants; deleting one is scoped by its tenant.
/// - **Time.** Stores never read a clock: every method that depends on time takes it.
///
/// Lists are ordered: keys by creation time then hash, roles by name, grants by principal
/// (as their text forms sort).
pub trait AccessStore: Send + Sync {
    // ---------------------------------------------------------------------------------------
    // API keys

    /// Stores an API key. A hash already stored is a `Conflict`.
    fn put_key(&self, key: &ApiKey) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// The key with this hash, revoked or not: the caller refuses a revoked key.
    fn find_key(
        &self,
        hash: Digest,
    ) -> impl Future<Output = Result<Option<ApiKey>, StoreError>> + Send;

    /// Every key of the tenant, revoked ones included.
    fn list_keys(
        &self,
        tenant: &TenantId,
    ) -> impl Future<Output = Result<Vec<ApiKey>, StoreError>> + Send;

    /// Revokes a key at `now`. Returns `false` for an unknown or already revoked key.
    fn revoke_key(
        &self,
        hash: Digest,
        now: Timestamp,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;

    // ---------------------------------------------------------------------------------------
    // End users' tokens

    /// Stores an opaque token. A hash already stored is a `Conflict`.
    fn put_token(&self, token: &Token) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// The token with this hash, revoked or expired or not: the caller refuses those.
    fn find_token(
        &self,
        hash: Digest,
    ) -> impl Future<Output = Result<Option<Token>, StoreError>> + Send;

    /// Revokes a token of the tenant at `now`. Returns `false` for a token of another tenant,
    /// an unknown one, or one already revoked.
    fn revoke_token(
        &self,
        tenant: &TenantId,
        hash: Digest,
        now: Timestamp,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;

    /// Revokes at `now` every token of the namespace not yet revoked. Returns how many.
    fn revoke_tokens(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        now: Timestamp,
    ) -> impl Future<Output = Result<u64, StoreError>> + Send;

    /// The tokens of the namespace live at `now`: neither revoked nor expired.
    fn count_tokens(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        now: Timestamp,
    ) -> impl Future<Output = Result<u64, StoreError>> + Send;

    /// Deletes at most `limit` tokens, of any tenant, whose `expires_at` or `revoked_at` is
    /// before `before`. Returns how many: fewer than `limit` once none is left.
    fn purge_tokens(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> impl Future<Output = Result<u64, StoreError>> + Send;

    /// Stores a token key. A `kid` already stored is a `Conflict`.
    fn put_token_key(&self, key: &TokenKey) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// The key with this `kid`, in any tenant: the token's tenant is the key's.
    fn find_token_key(
        &self,
        kid: &str,
    ) -> impl Future<Output = Result<Option<TokenKey>, StoreError>> + Send;

    /// The tenant's token keys, by creation time then `kid`.
    fn list_token_keys(
        &self,
        tenant: &TenantId,
    ) -> impl Future<Output = Result<Vec<TokenKey>, StoreError>> + Send;

    /// Deletes a key of the tenant. Returns `false` for a key of another tenant or an unknown
    /// one.
    fn delete_token_key(
        &self,
        tenant: &TenantId,
        kid: &str,
    ) -> impl Future<Output = Result<bool, StoreError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Roles

    /// Stores a role of its tenant, or replaces the role of the same name; a replaced role
    /// keeps its first `created_at`.
    fn put_role(&self, role: &Role) -> impl Future<Output = Result<(), StoreError>> + Send;

    fn get_role(
        &self,
        tenant: &TenantId,
        name: &str,
    ) -> impl Future<Output = Result<Option<Role>, StoreError>> + Send;

    fn list_roles(
        &self,
        tenant: &TenantId,
    ) -> impl Future<Output = Result<Vec<Role>, StoreError>> + Send;

    /// Deletes a role of the tenant, unless a grant of the tenant points to it (`InUse`).
    fn delete_role(
        &self,
        tenant: &TenantId,
        name: &str,
    ) -> impl Future<Output = Result<RoleDeletion, StoreError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Grants

    /// Stores the principal's grant in its tenant, replacing the one it had. With `keep`, a
    /// write that would take `keep` away from the last member holding it in the tenant
    /// answers `LastHolder` and changes nothing.
    fn put_grant(
        &self,
        grant: &Grant,
        keep: Option<&str>,
    ) -> impl Future<Output = Result<GrantWrite, StoreError>> + Send;

    /// The principal's grant in the tenant.
    fn grant(
        &self,
        tenant: &TenantId,
        principal: &Principal,
    ) -> impl Future<Output = Result<Option<Grant>, StoreError>> + Send;

    fn list_grants(
        &self,
        tenant: &TenantId,
    ) -> impl Future<Output = Result<Vec<Grant>, StoreError>> + Send;

    /// Deletes the principal's grant in the tenant. With `keep`, the grant of the last member
    /// holding `keep` in the tenant stays (`LastHolder`).
    fn delete_grant(
        &self,
        tenant: &TenantId,
        principal: &Principal,
        keep: Option<&str>,
    ) -> impl Future<Output = Result<GrantDeletion, StoreError>> + Send;

    /// Moves the grant of `invitation` to `user`, with its role, labels, author and date: the
    /// user accepted the invitation. If `user` already has a grant in the tenant, it stays and
    /// the invitation's is deleted.
    fn claim_grant(
        &self,
        tenant: &TenantId,
        invitation: &Principal,
        user: &Principal,
    ) -> impl Future<Output = Result<GrantClaim, StoreError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Default role

    /// The role a member gets at their first session in the organization, without an
    /// invitation's grant.
    fn set_default_role(
        &self,
        tenant: &TenantId,
        role: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

    /// `None` when never set: the server's default applies (`reader`).
    fn default_role(
        &self,
        tenant: &TenantId,
    ) -> impl Future<Output = Result<Option<String>, StoreError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principals_round_trip_through_their_text() {
        let key = Principal::Key(Digest::of(b"rewind_sk_secret"));
        for principal in [
            Principal::User("user_01JABC".into()),
            key.clone(),
            Principal::Invitation("invitation_01JABC".into()),
        ] {
            assert_eq!(Principal::parse(&principal.to_string()), Ok(principal));
        }
        assert!(key.to_string().starts_with("key:"));
        for invalid in [
            "",
            "user",
            "user:",
            "member:user_1",
            "key:abc",
            "user:a b",
            "invitation:é",
            &format!("user:{}", "x".repeat(129)),
        ] {
            assert_eq!(
                Principal::parse(invalid),
                Err(InvalidPrincipal(invalid.to_owned())),
                "{invalid}"
            );
        }
    }

    #[test]
    fn principals_sort_as_their_text() {
        let mut principals = [
            Principal::User("user_b".into()),
            Principal::Key(Digest::of(b"one")),
            Principal::User("user_a".into()),
            Principal::Invitation("invitation_z".into()),
            Principal::Key(Digest::of(b"two")),
        ];
        let mut texts: Vec<String> = principals.iter().map(ToString::to_string).collect();
        principals.sort();
        texts.sort();
        assert_eq!(
            principals
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            texts
        );
    }
}
