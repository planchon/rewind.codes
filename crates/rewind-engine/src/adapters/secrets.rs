use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
use rewind_core::Timestamp;

use crate::ports::{
    DailyReads, Day, LastRead, MasterKeyId, NamespaceName, NewSecret, PrivateKey, PublicKey,
    PutSecret, RETIRED_KEY_PUBLISHED, Secret, SecretError, SecretId, SecretMeta, SecretNamespace,
    SecretReader, SecretStore, SigningKey, TenantId,
};

/// A secret as kept in memory.
#[derive(Debug, Clone)]
struct Stored {
    value: Secret,
    labels: BTreeMap<String, String>,
    created_by: String,
    created_at: Timestamp,
    updated_by: String,
    updated_at: Timestamp,
    reads: BTreeMap<Day, u64>,
    last_read: Option<LastRead>,
}

#[derive(Debug, Default)]
struct State {
    secrets: BTreeMap<(TenantId, SecretId), Stored>,
    /// Each tenant's keys, oldest first. The last one signs unless it is retired.
    keys: BTreeMap<TenantId, Vec<SigningKey>>,
}

/// `SecretStore` in memory, for engine tests, test runs and the embedded server without a
/// database. Nothing is at rest, so nothing is encrypted and no master key is in use. It passes
/// the conformance suite. A secret put or deleted, and each secret a namespace's erasure
/// deletes, sends its `ControlPlaneChange::Secret` through the changes it was given; reads and
/// signing keys send nothing.
#[derive(Debug, Default)]
pub struct MemorySecrets {
    state: Mutex<State>,
    changes: Option<crate::adapters::ControlPlaneChanges>,
}

impl MemorySecrets {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store whose writes send their `ControlPlaneChange::Secret` through `changes`, as the
    /// stores of the control plane do in their transaction: the owners resume the calls parked
    /// for a secret put again.
    pub fn with_changes(changes: crate::adapters::ControlPlaneChanges) -> Self {
        Self {
            state: Mutex::default(),
            changes: Some(changes),
        }
    }

    fn changed(&self, tenant: &TenantId, secret: &SecretId) {
        if let Some(changes) = &self.changes {
            changes.send(crate::ports::ControlPlaneChange::Secret {
                tenant: tenant.clone(),
                secret: secret.clone(),
            });
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, SecretError> {
        self.state
            .lock()
            .map_err(|_| SecretError::Backend("the memory secret store's lock is poisoned".into()))
    }
}

/// A new Ed25519 key pair, from the operating system's CSPRNG.
fn new_signing_key(now: Timestamp) -> Result<SigningKey, SecretError> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|err| SecretError::Crypto(err.to_string()))?;
    let pair = Ed25519KeyPair::from_seed_unchecked(&seed)
        .map_err(|err| SecretError::Crypto(err.to_string()))?;
    let key: [u8; 32] = pair
        .public_key()
        .as_ref()
        .try_into()
        .map_err(|_| SecretError::Crypto("an Ed25519 public key has 32 bytes".into()))?;
    Ok(SigningKey {
        public: PublicKey {
            kid: PublicKey::kid_of(&key),
            key,
            created_at: now,
            retired_at: None,
        },
        private: PrivateKey::from_seed(seed),
    })
}

fn meta(id: &SecretId, stored: &Stored, since: Day) -> SecretMeta {
    SecretMeta {
        id: id.clone(),
        labels: stored.labels.clone(),
        created_by: stored.created_by.clone(),
        created_at: stored.created_at,
        updated_by: stored.updated_by.clone(),
        updated_at: stored.updated_at,
        reads: stored
            .reads
            .range(since..)
            .map(|(day, reads)| DailyReads {
                day: *day,
                reads: *reads,
            })
            .collect(),
        last_read: stored.last_read.clone(),
    }
}

/// The current key: the newest, unless it is retired.
fn current(keys: &[SigningKey]) -> Option<&SigningKey> {
    keys.last().filter(|key| key.public.retired_at.is_none())
}

impl SecretStore for MemorySecrets {
    async fn put(
        &self,
        tenant: &TenantId,
        secret: &NewSecret,
        now: Timestamp,
    ) -> Result<PutSecret, SecretError> {
        if matches!(secret.id.namespace, SecretNamespace::Namespace(_)) && !secret.labels.is_empty()
        {
            return Err(SecretError::Invalid(
                "a namespace's secret carries no label: the namespace is its scope".into(),
            ));
        }
        let mut state = self.lock()?;
        let key = (tenant.clone(), secret.id.clone());
        self.changed(tenant, &secret.id);
        Ok(match state.secrets.get_mut(&key) {
            Some(stored) => {
                stored.value = secret.value.clone();
                stored.labels = secret.labels.clone();
                stored.updated_by = secret.by.clone();
                stored.updated_at = now;
                PutSecret::Replaced
            }
            None => {
                state.secrets.insert(
                    key,
                    Stored {
                        value: secret.value.clone(),
                        labels: secret.labels.clone(),
                        created_by: secret.by.clone(),
                        created_at: now,
                        updated_by: secret.by.clone(),
                        updated_at: now,
                        reads: BTreeMap::new(),
                        last_read: None,
                    },
                );
                PutSecret::Created
            }
        })
    }

    async fn list(
        &self,
        tenant: &TenantId,
        namespace: &SecretNamespace,
        labels: &BTreeMap<String, String>,
        since: Day,
    ) -> Result<Vec<SecretMeta>, SecretError> {
        let state = self.lock()?;
        Ok(state
            .secrets
            .iter()
            .filter(|((of, id), stored)| {
                of == tenant
                    && &id.namespace == namespace
                    && labels
                        .iter()
                        .all(|(key, value)| stored.labels.get(key) == Some(value))
            })
            .map(|((_, id), stored)| meta(id, stored, since))
            .collect())
    }

    async fn meta(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
        since: Day,
    ) -> Result<Option<SecretMeta>, SecretError> {
        let state = self.lock()?;
        Ok(state
            .secrets
            .get(&(tenant.clone(), secret.clone()))
            .map(|stored| meta(secret, stored, since)))
    }

    async fn read(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
        reader: &SecretReader,
        now: Timestamp,
    ) -> Result<Option<Secret>, SecretError> {
        let mut state = self.lock()?;
        let Some(stored) = state.secrets.get_mut(&(tenant.clone(), secret.clone())) else {
            return Ok(None);
        };
        *stored.reads.entry(Day::of(now)).or_default() += 1;
        stored.last_read = Some(LastRead {
            by: reader.clone(),
            at: now,
        });
        Ok(Some(stored.value.clone()))
    }

    async fn delete(&self, tenant: &TenantId, secret: &SecretId) -> Result<bool, SecretError> {
        let mut state = self.lock()?;
        let existed = state
            .secrets
            .remove(&(tenant.clone(), secret.clone()))
            .is_some();
        if existed {
            self.changed(tenant, secret);
        }
        Ok(existed)
    }

    async fn delete_namespace(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
    ) -> Result<usize, SecretError> {
        let mut state = self.lock()?;
        let mut deleted = Vec::new();
        state.secrets.retain(|(of, id), _| {
            let erased = of == tenant
                && matches!(&id.namespace, SecretNamespace::Namespace(name) if name == namespace);
            if erased {
                deleted.push(id.clone());
            }
            !erased
        });
        for secret in &deleted {
            self.changed(tenant, secret);
        }
        Ok(deleted.len())
    }

    async fn signing_key(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, SecretError> {
        let mut state = self.lock()?;
        let keys = state.keys.entry(tenant.clone()).or_default();
        if let Some(key) = current(keys) {
            return Ok(key.clone());
        }
        let key = new_signing_key(now)?;
        keys.push(key.clone());
        Ok(key)
    }

    async fn rotate_signing_key(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, SecretError> {
        let key = new_signing_key(now)?;
        let mut state = self.lock()?;
        let keys = state.keys.entry(tenant.clone()).or_default();
        if let Some(last) = keys.last_mut()
            && last.public.retired_at.is_none()
        {
            last.public.retired_at = Some(now);
        }
        keys.push(key.clone());
        Ok(key)
    }

    async fn public_keys(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> Result<Vec<PublicKey>, SecretError> {
        let state = self.lock()?;
        Ok(state
            .keys
            .get(tenant)
            .into_iter()
            .flatten()
            .rev()
            .filter(|key| {
                key.public
                    .retired_at
                    .is_none_or(|retired| now < retired.saturating_add(RETIRED_KEY_PUBLISHED))
            })
            .map(|key| key.public.clone())
            .collect())
    }

    async fn master_keys_in_use(&self) -> Result<Vec<MasterKeyId>, SecretError> {
        Ok(Vec::new())
    }

    async fn rewrap_data_keys(&self) -> Result<usize, SecretError> {
        Ok(0)
    }
}
