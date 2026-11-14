use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use rewind_core::{Digest, RunId};

use crate::ports::{Credentials, NamespaceName, SecretError, SecretOwner, SecretStore, TenantId};

type Secrets = BTreeMap<(TenantId, SecretOwner), Credentials>;

/// `SecretStore` in memory, for engine tests and mock mode. Nothing is at rest, so nothing is
/// encrypted. It passes the conformance suite.
#[derive(Debug, Default)]
pub struct MemorySecrets {
    secrets: Mutex<Secrets>,
}

impl MemorySecrets {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, Secrets>, SecretError> {
        self.secrets
            .lock()
            .map_err(|_| SecretError::Backend("the memory secret store's lock is poisoned".into()))
    }
}

impl SecretStore for MemorySecrets {
    async fn put(
        &self,
        tenant: &TenantId,
        owner: &SecretOwner,
        credentials: &Credentials,
    ) -> Result<(), SecretError> {
        self.lock()?
            .insert((tenant.clone(), owner.clone()), credentials.clone());
        Ok(())
    }

    async fn resolve(
        &self,
        tenant: &TenantId,
        run: RunId,
        namespace: &NamespaceName,
        toolset: Digest,
    ) -> Result<Option<Credentials>, SecretError> {
        let secrets = self.lock()?;
        let of_namespace = SecretOwner::Namespace {
            namespace: namespace.clone(),
            toolset,
        };
        Ok(secrets
            .get(&(tenant.clone(), SecretOwner::Run(run)))
            .or_else(|| secrets.get(&(tenant.clone(), of_namespace)))
            .or_else(|| secrets.get(&(tenant.clone(), SecretOwner::Toolset(toolset))))
            .cloned())
    }

    async fn delete_namespace(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
    ) -> Result<usize, SecretError> {
        let mut secrets = self.lock()?;
        let before = secrets.len();
        secrets.retain(|(owner_tenant, owner), _| {
            !(owner_tenant == tenant
                && matches!(owner, SecretOwner::Namespace { namespace: of, .. } if of == namespace))
        });
        Ok(before - secrets.len())
    }

    async fn delete(&self, tenant: &TenantId, owner: &SecretOwner) -> Result<bool, SecretError> {
        Ok(self
            .lock()?
            .remove(&(tenant.clone(), owner.clone()))
            .is_some())
    }
}
