//! `SecretStore` on Postgres (`archi/04-tools.md`, "Secrets" and "Signed calls";
//! `archi/06-storage-and-scale.md`, "Tables"): named secrets of the organization (namespace `*`)
//! and of each namespace, their reads per day, and each tenant's Ed25519 signing keys, with
//! envelope encryption: a data key per tenant, encrypted by a master key whose id the tenant row
//! records.
//!
//! The contract is frozen in `rewind-engine` (its conformance suite `conformance::secrets`, which
//! the memory adapter passes); its implementation here, with the migration of `secrets`,
//! `secret_reads`, `signing_keys` and `tenants.master_key`, and the previous master keys, is the
//! stores workstream's (`archi/09-roadmap.md`). The credentials per toolset, per namespace and
//! per run it replaces were sealed with XChaCha20-Poly1305 (`chacha20poly1305`), bound to their
//! tenant and owner.

use std::collections::BTreeMap;

use rewind_core::Timestamp;
use rewind_engine::ports::{
    Day, MasterKeyId, NamespaceName, NewSecret, PublicKey, PutSecret, Secret, SecretError,
    SecretId, SecretMeta, SecretNamespace, SecretReader, SecretStore, SigningKey, TenantId,
};

use crate::sql::Failure;
use crate::{Error, PostgresStore};

/// The key that encrypts every tenant's data key: 256 bits from the deployment's
/// configuration. It never enters the database; without it, secrets cannot be read.
#[derive(Clone, PartialEq, Eq)]
pub struct MasterKey([u8; 32]);

impl MasterKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// A new key from the operating system's CSPRNG, for a deployment that has none yet.
    pub fn generate() -> Result<Self, Error> {
        random_bytes().map(Self)
    }

    /// The key's bytes, for the configuration that keeps it.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

impl From<Error> for SecretError {
    fn from(err: Error) -> Self {
        SecretError::Backend(err.to_string())
    }
}

impl From<SecretError> for Failure<SecretError> {
    fn from(err: SecretError) -> Self {
        Failure::Port(err)
    }
}

impl From<Failure<SecretError>> for SecretError {
    fn from(failure: Failure<SecretError>) -> Self {
        match failure {
            Failure::Port(err) => err,
            Failure::Db(err) => err.into(),
        }
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|err| Error::Random(err.to_string()))?;
    Ok(bytes)
}

#[expect(
    clippy::todo,
    reason = "the stores workstream builds the SecretStore of named secrets"
)]
impl SecretStore for PostgresStore {
    async fn put(
        &self,
        tenant: &TenantId,
        secret: &NewSecret,
        now: Timestamp,
    ) -> Result<PutSecret, SecretError> {
        let _ = (tenant, secret, now);
        todo!("stores: create or replace a secret in `secrets`, sealed with the tenant's data key")
    }

    async fn list(
        &self,
        tenant: &TenantId,
        namespace: &SecretNamespace,
        labels: &BTreeMap<String, String>,
        since: Day,
    ) -> Result<Vec<SecretMeta>, SecretError> {
        let _ = (tenant, namespace, labels, since);
        todo!("stores: the metadata of a namespace's secrets, with secret_reads from `since`")
    }

    async fn meta(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
        since: Day,
    ) -> Result<Option<SecretMeta>, SecretError> {
        let _ = (tenant, secret, since);
        todo!("stores: the metadata of one secret, with secret_reads from `since`")
    }

    async fn read(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
        reader: &SecretReader,
        now: Timestamp,
    ) -> Result<Option<Secret>, SecretError> {
        let _ = (tenant, secret, reader, now);
        todo!("stores: open a secret's value and count the read in secret_reads and last_read_*")
    }

    async fn delete(&self, tenant: &TenantId, secret: &SecretId) -> Result<bool, SecretError> {
        let _ = (tenant, secret);
        todo!("stores: delete a secret and its secret_reads")
    }

    async fn delete_namespace(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
    ) -> Result<usize, SecretError> {
        let _ = (tenant, namespace);
        todo!("stores: delete a namespace's secrets and their secret_reads")
    }

    async fn signing_key(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, SecretError> {
        let _ = (tenant, now);
        todo!("stores: the tenant's current key of signing_keys, made at the first call")
    }

    async fn rotate_signing_key(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, SecretError> {
        let _ = (tenant, now);
        todo!("stores: retire the current signing key and make a new one")
    }

    async fn public_keys(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> Result<Vec<PublicKey>, SecretError> {
        let _ = (tenant, now);
        todo!("stores: the current signing key and those retired less than 24 hours ago")
    }

    async fn master_keys_in_use(&self) -> Result<Vec<MasterKeyId>, SecretError> {
        todo!("stores: the ids of tenants.master_key")
    }

    async fn rewrap_data_keys(&self) -> Result<usize, SecretError> {
        todo!("stores: encrypt with the current master key the data keys held under a previous one")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_keys_are_never_printed() {
        let key = MasterKey::from_bytes([42; 32]);
        assert_eq!(format!("{key:?}"), "MasterKey(<redacted>)");
    }
}
