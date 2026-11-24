use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use rewind_core::{Digest, Timestamp};

use crate::adapters::ControlPlaneChanges;
use crate::ports::{
    AccessStore, ApiKey, ClientKind, CodeRedemption, Connection, ConnectionToken,
    ConnectionTokenKind, ControlPlaneChange, Grant, GrantClaim, GrantDeletion, GrantWrite,
    NamespaceName, NewConnection, NewToken, OAuthClient, OAuthCode, OpenedOrganization, Principal,
    Role, RoleDeletion, Rotation, StoreError, TenantId, Token, TokenKey, UserId,
};

/// `AccessStore` in memory, for engine and server tests. It passes the conformance suite: the
/// reference implementation of the contract. Every write that changes something sends its
/// `ControlPlaneChange` through the [`ControlPlaneChanges`] it was given, under the lock of the
/// write, as the stores of the control plane do in their transaction: a key, a token, a token
/// key, a grant, a role, a tenant's default role (`Tenant`), a connection. Usage stamps (a
/// connection's last use, an OAuth client's) and OAuth codes send nothing.
#[derive(Debug, Default)]
pub struct MemoryAccess {
    state: Mutex<State>,
    changes: ControlPlaneChanges,
}

fn grant_changed(tenant: &TenantId, principal: &Principal) -> ControlPlaneChange {
    ControlPlaneChange::Grant {
        tenant: tenant.clone(),
        principal: principal.clone(),
    }
}

fn connection_changed(id: &str) -> ControlPlaneChange {
    ControlPlaneChange::Connection { id: id.to_owned() }
}

#[derive(Debug, Default)]
struct State {
    keys: BTreeMap<Digest, ApiKey>,
    tokens: BTreeMap<Digest, Token>,
    token_keys: BTreeMap<String, TokenKey>,
    roles: BTreeMap<(TenantId, String), Role>,
    grants: BTreeMap<(TenantId, Principal), Grant>,
    default_roles: BTreeMap<TenantId, String>,
    clients: BTreeMap<String, OAuthClient>,
    codes: BTreeMap<Digest, OAuthCode>,
    connections: BTreeMap<String, Connection>,
    connection_tokens: BTreeMap<Digest, ConnectionToken>,
}

impl State {
    /// Whether taking `principal`'s grant away from the role `keep` would leave the tenant
    /// without a member holding it. Keys and invitations never count.
    fn holds_last(&self, tenant: &TenantId, principal: &Principal, keep: &str) -> bool {
        let member = |principal: &Principal| matches!(principal, Principal::Member(_));
        let holds = member(principal)
            && self
                .grants
                .get(&(tenant.clone(), principal.clone()))
                .is_some_and(|grant| grant.role == keep);
        holds
            && !self.tenant_grants(tenant).any(|grant| {
                grant.role == keep && member(&grant.principal) && grant.principal != *principal
            })
    }

    fn tenant_grants<'a>(&'a self, tenant: &'a TenantId) -> impl Iterator<Item = &'a Grant> {
        self.grants
            .iter()
            .filter(move |((of, _), _)| of == tenant)
            .map(|(_, grant)| grant)
    }

    /// Deletes the grants of `principal`, with the change of each.
    fn delete_grants(&mut self, principal: &Principal, changes: &mut Vec<ControlPlaneChange>) {
        self.grants.retain(|(tenant, of), _| {
            if of == principal {
                changes.push(grant_changed(tenant, principal));
                return false;
            }
            true
        });
    }

    /// Replaces the grants of `connection` with one per organization, given by its person.
    fn open(
        &mut self,
        connection: &Connection,
        organizations: &[OpenedOrganization],
        now: Timestamp,
        changes: &mut Vec<ControlPlaneChange>,
    ) {
        let principal = connection.principal();
        self.delete_grants(&principal, changes);
        changes.push(connection_changed(&connection.id));
        for opened in organizations {
            changes.push(grant_changed(&opened.tenant, &principal));
            self.grants.insert(
                (opened.tenant.clone(), principal.clone()),
                Grant {
                    tenant: opened.tenant.clone(),
                    principal: principal.clone(),
                    role: opened.access.role().to_owned(),
                    allowed_labels: opened.allowed_labels.clone(),
                    granted_by: Some(Principal::from(&connection.user)),
                    granted_at: now,
                },
            );
        }
    }

    /// Revokes a live connection: its grants and its tokens go.
    fn revoke(&mut self, id: &str, now: Timestamp, changes: &mut Vec<ControlPlaneChange>) -> bool {
        let Some(connection) = self
            .connections
            .get_mut(id)
            .filter(|connection| connection.revoked_at.is_none())
        else {
            return false;
        };
        connection.revoked_at = Some(now);
        self.forget(id, changes);
        true
    }

    /// Deletes the grants and the tokens of a connection.
    fn forget(&mut self, id: &str, changes: &mut Vec<ControlPlaneChange>) {
        changes.push(connection_changed(id));
        self.delete_grants(&Principal::Connection(id.to_owned()), changes);
        self.connection_tokens
            .retain(|_, token| token.connection != id);
    }

    /// Stores new tokens of a connection; a hash already stored is a `Conflict`, and nothing is
    /// stored.
    fn issue(
        &mut self,
        connection: &str,
        tokens: [(&NewToken, ConnectionTokenKind); 2],
        now: Timestamp,
    ) -> Result<(), StoreError> {
        if tokens
            .iter()
            .any(|(token, _)| self.connection_tokens.contains_key(&token.hash))
            || tokens[0].0.hash == tokens[1].0.hash
        {
            return Err(StoreError::Conflict(
                "a connection's token with this hash already exists".into(),
            ));
        }
        for (token, kind) in tokens {
            self.connection_tokens.insert(
                token.hash,
                ConnectionToken {
                    hash: token.hash,
                    connection: connection.to_owned(),
                    kind,
                    expires_at: token.expires_at,
                    created_at: now,
                    used_at: None,
                },
            );
        }
        Ok(())
    }
}

impl MemoryAccess {
    /// A store whose changes reach nobody.
    pub fn new() -> Self {
        Self::default()
    }

    /// A store that sends its changes through `changes`: a clone of a
    /// [`MemoryCluster`](crate::adapters::MemoryCluster)'s, for its members to receive them.
    pub fn with_changes(changes: ControlPlaneChanges) -> Self {
        Self {
            state: Mutex::default(),
            changes,
        }
    }

    /// Sends what a write changed; called under the lock of the write.
    fn send(&self, changes: impl IntoIterator<Item = ControlPlaneChange>) {
        for change in changes {
            self.changes.send(change);
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, StoreError> {
        self.state
            .lock()
            .map_err(|_| StoreError::Backend("the memory access store's lock is poisoned".into()))
    }
}

impl AccessStore for MemoryAccess {
    async fn put_key(&self, key: &ApiKey) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        if state.keys.contains_key(&key.hash) {
            return Err(StoreError::Conflict(format!(
                "an API key with hash {} already exists",
                key.hash.short()
            )));
        }
        state.keys.insert(key.hash, key.clone());
        self.send([ControlPlaneChange::Key {
            tenant: key.tenant.clone(),
            hash: key.hash,
        }]);
        Ok(())
    }

    async fn find_key(&self, hash: Digest) -> Result<Option<ApiKey>, StoreError> {
        Ok(self.lock()?.keys.get(&hash).cloned())
    }

    async fn list_keys(&self, tenant: &TenantId) -> Result<Vec<ApiKey>, StoreError> {
        let mut keys: Vec<ApiKey> = self
            .lock()?
            .keys
            .values()
            .filter(|key| key.tenant == *tenant)
            .cloned()
            .collect();
        keys.sort_by_key(|key| (key.created_at, key.hash));
        Ok(keys)
    }

    async fn revoke_key(&self, hash: Digest, now: Timestamp) -> Result<bool, StoreError> {
        let mut state = self.lock()?;
        match state.keys.get_mut(&hash) {
            Some(key) if key.revoked_at.is_none() => {
                key.revoked_at = Some(now);
                self.send([ControlPlaneChange::Key {
                    tenant: key.tenant.clone(),
                    hash,
                }]);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn put_token(&self, token: &Token) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        if state.tokens.contains_key(&token.hash) {
            return Err(StoreError::Conflict(format!(
                "a token with hash {} already exists",
                token.hash.short()
            )));
        }
        state.tokens.insert(token.hash, token.clone());
        self.send([ControlPlaneChange::Token {
            tenant: token.tenant.clone(),
            hash: token.hash,
        }]);
        Ok(())
    }

    async fn find_token(&self, hash: Digest) -> Result<Option<Token>, StoreError> {
        Ok(self.lock()?.tokens.get(&hash).cloned())
    }

    async fn revoke_token(
        &self,
        tenant: &TenantId,
        hash: Digest,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        let mut state = self.lock()?;
        match state.tokens.get_mut(&hash) {
            Some(token) if token.tenant == *tenant && token.revoked_at.is_none() => {
                token.revoked_at = Some(now);
                self.send([ControlPlaneChange::Token {
                    tenant: tenant.clone(),
                    hash,
                }]);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn revoke_tokens(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        now: Timestamp,
    ) -> Result<u64, StoreError> {
        let mut state = self.lock()?;
        let mut revoked = Vec::new();
        for token in state.tokens.values_mut() {
            if token.tenant == *tenant
                && token.namespace == *namespace
                && token.revoked_at.is_none()
            {
                token.revoked_at = Some(now);
                revoked.push(ControlPlaneChange::Token {
                    tenant: tenant.clone(),
                    hash: token.hash,
                });
            }
        }
        let count = revoked.len() as u64;
        self.send(revoked);
        Ok(count)
    }

    async fn count_tokens(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        now: Timestamp,
    ) -> Result<u64, StoreError> {
        Ok(self
            .lock()?
            .tokens
            .values()
            .filter(|token| {
                token.tenant == *tenant && token.namespace == *namespace && token.is_live(now)
            })
            .count() as u64)
    }

    async fn purge_tokens(&self, before: Timestamp, limit: usize) -> Result<u64, StoreError> {
        let mut state = self.lock()?;
        let ended: Vec<Digest> = state
            .tokens
            .values()
            .filter(|token| {
                token.expires_at < before || token.revoked_at.is_some_and(|at| at < before)
            })
            .map(|token| token.hash)
            .take(limit)
            .collect();
        for hash in &ended {
            if let Some(token) = state.tokens.remove(hash) {
                self.send([ControlPlaneChange::Token {
                    tenant: token.tenant,
                    hash: *hash,
                }]);
            }
        }
        Ok(ended.len() as u64)
    }

    async fn put_token_key(&self, key: &TokenKey) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        if state.token_keys.contains_key(&key.kid) {
            return Err(StoreError::Conflict(format!(
                "a token key {} already exists",
                key.kid
            )));
        }
        state.token_keys.insert(key.kid.clone(), key.clone());
        self.send([ControlPlaneChange::TokenKey {
            tenant: key.tenant.clone(),
            kid: key.kid.clone(),
        }]);
        Ok(())
    }

    async fn find_token_key(&self, kid: &str) -> Result<Option<TokenKey>, StoreError> {
        Ok(self.lock()?.token_keys.get(kid).cloned())
    }

    async fn list_token_keys(&self, tenant: &TenantId) -> Result<Vec<TokenKey>, StoreError> {
        let mut keys: Vec<TokenKey> = self
            .lock()?
            .token_keys
            .values()
            .filter(|key| key.tenant == *tenant)
            .cloned()
            .collect();
        keys.sort_by(|a, b| (a.created_at, &a.kid).cmp(&(b.created_at, &b.kid)));
        Ok(keys)
    }

    async fn delete_token_key(&self, tenant: &TenantId, kid: &str) -> Result<bool, StoreError> {
        let mut state = self.lock()?;
        if state
            .token_keys
            .get(kid)
            .is_some_and(|key| key.tenant == *tenant)
        {
            state.token_keys.remove(kid);
            self.send([ControlPlaneChange::TokenKey {
                tenant: tenant.clone(),
                kid: kid.to_owned(),
            }]);
            return Ok(true);
        }
        Ok(false)
    }

    async fn put_role(&self, role: &Role) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        let key = (role.tenant.clone(), role.name.clone());
        let mut role = role.clone();
        if let Some(existing) = state.roles.get(&key) {
            role.created_at = existing.created_at;
        }
        self.send([ControlPlaneChange::Role {
            tenant: role.tenant.clone(),
            name: role.name.clone(),
        }]);
        state.roles.insert(key, role);
        Ok(())
    }

    async fn get_role(&self, tenant: &TenantId, name: &str) -> Result<Option<Role>, StoreError> {
        Ok(self
            .lock()?
            .roles
            .get(&(tenant.clone(), name.to_owned()))
            .cloned())
    }

    async fn list_roles(&self, tenant: &TenantId) -> Result<Vec<Role>, StoreError> {
        Ok(self
            .lock()?
            .roles
            .iter()
            .filter(|((of, _), _)| of == tenant)
            .map(|(_, role)| role.clone())
            .collect())
    }

    async fn delete_role(&self, tenant: &TenantId, name: &str) -> Result<RoleDeletion, StoreError> {
        let mut state = self.lock()?;
        let key = (tenant.clone(), name.to_owned());
        if !state.roles.contains_key(&key) {
            return Ok(RoleDeletion::NotFound);
        }
        if state.tenant_grants(tenant).any(|grant| grant.role == name) {
            return Ok(RoleDeletion::InUse);
        }
        state.roles.remove(&key);
        self.send([ControlPlaneChange::Role {
            tenant: tenant.clone(),
            name: name.to_owned(),
        }]);
        Ok(RoleDeletion::Deleted)
    }

    async fn put_grant(&self, grant: &Grant, keep: Option<&str>) -> Result<GrantWrite, StoreError> {
        let mut state = self.lock()?;
        if let Some(keep) = keep
            && grant.role != keep
            && state.holds_last(&grant.tenant, &grant.principal, keep)
        {
            return Ok(GrantWrite::LastHolder);
        }
        state.grants.insert(
            (grant.tenant.clone(), grant.principal.clone()),
            grant.clone(),
        );
        self.send([grant_changed(&grant.tenant, &grant.principal)]);
        Ok(GrantWrite::Written)
    }

    async fn grant(
        &self,
        tenant: &TenantId,
        principal: &Principal,
    ) -> Result<Option<Grant>, StoreError> {
        Ok(self
            .lock()?
            .grants
            .get(&(tenant.clone(), principal.clone()))
            .cloned())
    }

    async fn list_grants(&self, tenant: &TenantId) -> Result<Vec<Grant>, StoreError> {
        Ok(self.lock()?.tenant_grants(tenant).cloned().collect())
    }

    async fn delete_grant(
        &self,
        tenant: &TenantId,
        principal: &Principal,
        keep: Option<&str>,
    ) -> Result<GrantDeletion, StoreError> {
        let mut state = self.lock()?;
        let key = (tenant.clone(), principal.clone());
        if !state.grants.contains_key(&key) {
            return Ok(GrantDeletion::NotFound);
        }
        if keep.is_some_and(|keep| state.holds_last(tenant, principal, keep)) {
            return Ok(GrantDeletion::LastHolder);
        }
        state.grants.remove(&key);
        self.send([grant_changed(tenant, principal)]);
        Ok(GrantDeletion::Deleted)
    }

    async fn claim_grant(
        &self,
        tenant: &TenantId,
        invitation: &Principal,
        user: &Principal,
    ) -> Result<GrantClaim, StoreError> {
        let mut state = self.lock()?;
        let pending = state.grants.remove(&(tenant.clone(), invitation.clone()));
        if pending.is_some() {
            self.send([grant_changed(tenant, invitation)]);
        }
        let user_key = (tenant.clone(), user.clone());
        if let Some(existing) = state.grants.get(&user_key) {
            return Ok(GrantClaim::AlreadyGranted(existing.clone()));
        }
        let Some(mut grant) = pending else {
            return Ok(GrantClaim::NoGrant);
        };
        grant.principal = user.clone();
        state.grants.insert(user_key, grant.clone());
        self.send([grant_changed(tenant, user)]);
        Ok(GrantClaim::Claimed(grant))
    }

    async fn set_default_role(&self, tenant: &TenantId, role: &str) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        state.default_roles.insert(tenant.clone(), role.to_owned());
        // The default role is a setting of the tenant.
        self.send([ControlPlaneChange::Tenant {
            tenant: tenant.clone(),
        }]);
        Ok(())
    }

    async fn default_role(&self, tenant: &TenantId) -> Result<Option<String>, StoreError> {
        Ok(self.lock()?.default_roles.get(tenant).cloned())
    }

    async fn put_oauth_client(&self, client: &OAuthClient) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        let mut client = client.clone();
        if let Some(existing) = state.clients.get(&client.id) {
            client.created_at = existing.created_at;
            client.last_used_at = existing.last_used_at;
        }
        state.clients.insert(client.id.clone(), client);
        Ok(())
    }

    async fn oauth_client(&self, id: &str) -> Result<Option<OAuthClient>, StoreError> {
        Ok(self.lock()?.clients.get(id).cloned())
    }

    async fn count_oauth_clients(&self, kind: ClientKind) -> Result<u64, StoreError> {
        Ok(self
            .lock()?
            .clients
            .values()
            .filter(|client| client.kind == kind)
            .count() as u64)
    }

    async fn purge_oauth_clients(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, StoreError> {
        let mut state = self.lock()?;
        let unused: Vec<String> = state
            .clients
            .values()
            .filter(|client| {
                client
                    .last_used_at
                    .or(client.fetched_at)
                    .unwrap_or(client.created_at)
                    < before
            })
            .filter(|client| {
                !state
                    .connections
                    .values()
                    .any(|connection| connection.client_id == client.id)
            })
            .map(|client| client.id.clone())
            .take(limit)
            .collect();
        for id in &unused {
            state.clients.remove(id);
        }
        Ok(unused.len() as u64)
    }

    async fn put_oauth_code(&self, code: &OAuthCode) -> Result<(), StoreError> {
        let mut state = self.lock()?;
        if state.codes.contains_key(&code.hash) {
            return Err(StoreError::Conflict(format!(
                "an authorization code with hash {} already exists",
                code.hash.short()
            )));
        }
        state.codes.insert(code.hash, code.clone());
        Ok(())
    }

    async fn oauth_code(&self, hash: Digest) -> Result<Option<OAuthCode>, StoreError> {
        Ok(self.lock()?.codes.get(&hash).cloned())
    }

    async fn redeem_oauth_code(
        &self,
        hash: Digest,
        new: &NewConnection,
        now: Timestamp,
    ) -> Result<CodeRedemption, StoreError> {
        let mut state = self.lock()?;
        let Some(code) = state.codes.get(&hash).cloned() else {
            return Ok(CodeRedemption::Unknown);
        };
        if code.used_at.is_some() {
            let mut changes = Vec::new();
            if let Some(id) = &code.connection {
                state.revoke(id, now, &mut changes);
            }
            self.send(changes);
            return Ok(CodeRedemption::Used);
        }
        let kept = state
            .connections
            .values()
            .find(|connection| {
                connection.user == new.connection.user
                    && connection.client_id == new.connection.client_id
                    && connection.revoked_at.is_none()
            })
            .cloned();
        let connection = match kept {
            Some(kept) => Connection {
                expires_at: new.connection.expires_at,
                ..kept
            },
            None => {
                if state.connections.contains_key(&new.connection.id) {
                    return Err(StoreError::Conflict(format!(
                        "a connection {} already exists",
                        new.connection.id
                    )));
                }
                new.connection.clone()
            }
        };
        state.issue(
            &connection.id,
            [
                (&new.access, ConnectionTokenKind::Access),
                (&new.refresh, ConnectionTokenKind::Refresh),
            ],
            now,
        )?;
        state
            .connections
            .insert(connection.id.clone(), connection.clone());
        let mut changes = Vec::new();
        state.open(&connection, &new.organizations, now, &mut changes);
        self.send(changes);
        if let Some(code) = state.codes.get_mut(&hash) {
            code.used_at = Some(now);
            code.connection = Some(connection.id.clone());
        }
        if let Some(client) = state.clients.get_mut(&connection.client_id) {
            client.last_used_at = Some(now);
        }
        Ok(CodeRedemption::Redeemed(connection))
    }

    async fn purge_oauth_codes(&self, before: Timestamp, limit: usize) -> Result<u64, StoreError> {
        let mut state = self.lock()?;
        let expired: Vec<Digest> = state
            .codes
            .values()
            .filter(|code| code.expires_at < before)
            .map(|code| code.hash)
            .take(limit)
            .collect();
        for hash in &expired {
            state.codes.remove(hash);
        }
        Ok(expired.len() as u64)
    }

    async fn connection(&self, id: &str) -> Result<Option<Connection>, StoreError> {
        Ok(self.lock()?.connections.get(id).cloned())
    }

    async fn connections_of(&self, user: &UserId) -> Result<Vec<Connection>, StoreError> {
        let mut connections: Vec<Connection> = self
            .lock()?
            .connections
            .values()
            .filter(|connection| connection.user == *user && connection.revoked_at.is_none())
            .cloned()
            .collect();
        connections.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(connections)
    }

    async fn connection_grants(&self, id: &str) -> Result<Vec<Grant>, StoreError> {
        let principal = Principal::Connection(id.to_owned());
        Ok(self
            .lock()?
            .grants
            .values()
            .filter(|grant| grant.principal == principal)
            .cloned()
            .collect())
    }

    async fn set_connection_organizations(
        &self,
        id: &str,
        organizations: &[OpenedOrganization],
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        let mut state = self.lock()?;
        let Some(connection) = state
            .connections
            .get(id)
            .filter(|connection| connection.revoked_at.is_none())
            .cloned()
        else {
            return Ok(false);
        };
        let mut changes = Vec::new();
        state.open(&connection, organizations, now, &mut changes);
        self.send(changes);
        Ok(true)
    }

    async fn touch_connection(&self, id: &str, now: Timestamp) -> Result<(), StoreError> {
        if let Some(connection) = self.lock()?.connections.get_mut(id) {
            connection.last_used_at = Some(now);
        }
        Ok(())
    }

    async fn revoke_connection(&self, id: &str, now: Timestamp) -> Result<bool, StoreError> {
        let mut state = self.lock()?;
        let mut changes = Vec::new();
        let revoked = state.revoke(id, now, &mut changes);
        self.send(changes);
        Ok(revoked)
    }

    async fn purge_connections(
        &self,
        lapsed_before: Timestamp,
        revoked_before: Timestamp,
        limit: usize,
    ) -> Result<u64, StoreError> {
        let mut state = self.lock()?;
        let ended: Vec<String> = state
            .connections
            .values()
            .filter(|connection| {
                connection.expires_at < lapsed_before
                    || connection.revoked_at.is_some_and(|at| at < revoked_before)
            })
            .map(|connection| connection.id.clone())
            .take(limit)
            .collect();
        let mut changes = Vec::new();
        for id in &ended {
            state.connections.remove(id);
            state.forget(id, &mut changes);
        }
        self.send(changes);
        Ok(ended.len() as u64)
    }

    async fn connection_token(&self, hash: Digest) -> Result<Option<ConnectionToken>, StoreError> {
        Ok(self.lock()?.connection_tokens.get(&hash).cloned())
    }

    async fn rotate_refresh_token(
        &self,
        hash: Digest,
        access: &NewToken,
        refresh: &NewToken,
        now: Timestamp,
    ) -> Result<Rotation, StoreError> {
        let mut state = self.lock()?;
        let Some(token) = state
            .connection_tokens
            .get(&hash)
            .filter(|token| token.kind == ConnectionTokenKind::Refresh)
            .cloned()
        else {
            return Ok(Rotation::Unknown);
        };
        let Some(connection) = state
            .connections
            .get(&token.connection)
            .filter(|connection| connection.revoked_at.is_none())
            .cloned()
        else {
            return Ok(Rotation::Unknown);
        };
        if token.used_at.is_some() {
            let mut changes = Vec::new();
            state.revoke(&connection.id, now, &mut changes);
            self.send(changes);
            return Ok(Rotation::Used);
        }
        if token.expires_at <= now {
            return Ok(Rotation::Expired);
        }
        state.issue(
            &connection.id,
            [
                (access, ConnectionTokenKind::Access),
                (refresh, ConnectionTokenKind::Refresh),
            ],
            now,
        )?;
        if let Some(used) = state.connection_tokens.get_mut(&hash) {
            used.used_at = Some(now);
        }
        let connection = Connection {
            expires_at: refresh.expires_at,
            ..connection
        };
        state
            .connections
            .insert(connection.id.clone(), connection.clone());
        self.send([connection_changed(&connection.id)]);
        Ok(Rotation::Rotated(connection))
    }

    async fn purge_connection_tokens(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<u64, StoreError> {
        let mut state = self.lock()?;
        let ended: Vec<Digest> = state
            .connection_tokens
            .values()
            .filter(|token| token.expires_at < before)
            .map(|token| token.hash)
            .take(limit)
            .collect();
        let mut connections = std::collections::BTreeSet::new();
        for hash in &ended {
            if let Some(token) = state.connection_tokens.remove(hash) {
                connections.insert(token.connection);
            }
        }
        self.send(connections.iter().map(|id| connection_changed(id)));
        Ok(ended.len() as u64)
    }
}
