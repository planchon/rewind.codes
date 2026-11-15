use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use rewind_core::{Digest, Timestamp};

use crate::ports::{
    AccessStore, ApiKey, Grant, GrantClaim, GrantDeletion, GrantWrite, NamespaceName, Principal,
    Role, RoleDeletion, StoreError, TenantId, Token, TokenKey,
};

/// `AccessStore` in memory, for engine and server tests. It passes the conformance suite: the
/// reference implementation of the contract.
#[derive(Debug, Default)]
pub struct MemoryAccess {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    keys: BTreeMap<Digest, ApiKey>,
    tokens: BTreeMap<Digest, Token>,
    token_keys: BTreeMap<String, TokenKey>,
    roles: BTreeMap<(TenantId, String), Role>,
    grants: BTreeMap<(TenantId, Principal), Grant>,
    default_roles: BTreeMap<TenantId, String>,
}

impl State {
    /// Whether taking `principal`'s grant away from the role `keep` would leave the tenant
    /// without a member holding it. Keys and invitations never count.
    fn holds_last(&self, tenant: &TenantId, principal: &Principal, keep: &str) -> bool {
        let member = |principal: &Principal| matches!(principal, Principal::User(_));
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
}

impl MemoryAccess {
    pub fn new() -> Self {
        Self::default()
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
        let mut revoked = 0;
        for token in state.tokens.values_mut() {
            if token.tenant == *tenant
                && token.namespace == *namespace
                && token.revoked_at.is_none()
            {
                token.revoked_at = Some(now);
                revoked += 1;
            }
        }
        Ok(revoked)
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
            state.tokens.remove(hash);
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
        let user_key = (tenant.clone(), user.clone());
        if let Some(existing) = state.grants.get(&user_key) {
            return Ok(GrantClaim::AlreadyGranted(existing.clone()));
        }
        let Some(mut grant) = pending else {
            return Ok(GrantClaim::NoGrant);
        };
        grant.principal = user.clone();
        state.grants.insert(user_key, grant.clone());
        Ok(GrantClaim::Claimed(grant))
    }

    async fn set_default_role(&self, tenant: &TenantId, role: &str) -> Result<(), StoreError> {
        self.lock()?
            .default_roles
            .insert(tenant.clone(), role.to_owned());
        Ok(())
    }

    async fn default_role(&self, tenant: &TenantId) -> Result<Option<String>, StoreError> {
        Ok(self.lock()?.default_roles.get(tenant).cloned())
    }
}
