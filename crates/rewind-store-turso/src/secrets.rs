//! `SecretStore` on Turso, with envelope encryption (`archi/04-tools.md`, "Credentials").
//!
//! Each tenant has a data key: 256 random bits, created with its first secret, stored in
//! `data_keys` encrypted by the deployment's [`MasterKey`]. Each owner's credentials are
//! encrypted by the tenant's data key. Both use XChaCha20-Poly1305 with a random 192-bit nonce,
//! stored before the ciphertext. The associated data binds a data key to its tenant, and a
//! secret to its tenant and owner: a row copied under another key does not decrypt.

use std::collections::BTreeMap;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rewind_core::{Digest, RunId};
use rewind_engine::ports::{
    Credentials, NamespaceName, Secret, SecretError, SecretOwner, SecretStore, ServerCredentials,
    TenantId,
};
use serde_json::{Map, Value};
use turso::{Connection, Value as SqlValue};

use crate::sql::{self, Columns, tenant, text, transaction};
use crate::{Error, TursoStore};

const NONCE_LEN: usize = 24;

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

fn random_bytes<const N: usize>() -> Result<[u8; N], Error> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|err| Error::Random(err.to_string()))?;
    Ok(bytes)
}

/// `nonce ‖ ciphertext` of `plaintext` under `key`, bound to `aad`.
fn seal(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, SecretError> {
    let nonce: [u8; NONCE_LEN] = random_bytes()?;
    let cipher = XChaCha20Poly1305::new(&Key::from(*key));
    let ciphertext = cipher
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| SecretError::Crypto("encryption failed".into()))?;
    let mut sealed = nonce.to_vec();
    sealed.extend(ciphertext);
    Ok(sealed)
}

/// The plaintext of [`seal`]'s output; an error for a wrong key or a tampered value.
fn open(key: &[u8; 32], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, SecretError> {
    let invalid = || {
        SecretError::Crypto(
            "a secret does not decrypt: wrong master key, or a corrupt value".into(),
        )
    };
    if sealed.len() < NONCE_LEN {
        return Err(invalid());
    }
    let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| invalid())?;
    let cipher = XChaCha20Poly1305::new(&Key::from(*key));
    cipher
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| invalid())
}

fn data_key_aad(tenant_id: &TenantId) -> Vec<u8> {
    format!("rewind data key\0{tenant_id}").into_bytes()
}

fn secret_aad(tenant_id: &TenantId, owner: &str) -> Vec<u8> {
    format!("rewind secret\0{tenant_id}\0{owner}").into_bytes()
}

/// `toolset:<digest>`, `namespace:<name>/toolset:<digest>` or `run:<id>`. A namespace's name
/// holds no `/`.
fn owner_key(owner: &SecretOwner) -> String {
    match owner {
        SecretOwner::Toolset(digest) => format!("toolset:{digest}"),
        SecretOwner::Namespace { namespace, toolset } => {
            format!("{}toolset:{toolset}", namespace_prefix(namespace))
        }
        SecretOwner::Run(run) => format!("run:{run}"),
    }
}

/// What every owner key of a namespace starts with.
fn namespace_prefix(namespace: &NamespaceName) -> String {
    format!("namespace:{namespace}/")
}

/// The tenant's data key; created first if `create` and the tenant has none.
async fn data_key(
    connection: &Connection,
    master_key: &MasterKey,
    tenant_id: &TenantId,
    create: bool,
) -> Result<Option<[u8; 32]>, SecretError> {
    let aad = data_key_aad(tenant_id);
    let row = sql::query_row(
        connection,
        "SELECT wrapped_key FROM data_keys WHERE tenant = ?",
        vec![tenant(tenant_id)],
    )
    .await?;
    if let Some(row) = row {
        let key = open(&master_key.0, &aad, &row.blob(0)?)?;
        let key: [u8; 32] = key
            .try_into()
            .map_err(|_| SecretError::Crypto("a data key is not 256 bits".into()))?;
        return Ok(Some(key));
    }
    if !create {
        return Ok(None);
    }
    let key: [u8; 32] = random_bytes()?;
    let wrapped = seal(&master_key.0, &aad, &key)?;
    sql::execute(
        connection,
        "INSERT INTO data_keys (tenant, wrapped_key) VALUES (?, ?)",
        vec![tenant(tenant_id), SqlValue::Blob(wrapped)],
    )
    .await?;
    Ok(Some(key))
}

fn encode_credentials(credentials: &Credentials) -> Vec<u8> {
    let secrets = |map: &BTreeMap<String, Secret>| -> Value {
        Value::Object(
            map.iter()
                .map(|(name, secret)| (name.clone(), Value::String(secret.expose().to_owned())))
                .collect(),
        )
    };
    let servers: Map<String, Value> = credentials
        .servers
        .iter()
        .map(|(name, server)| {
            let mut object = Map::new();
            object.insert("headers".into(), secrets(&server.headers));
            object.insert("env".into(), secrets(&server.env));
            (name.clone(), Value::Object(object))
        })
        .collect();
    let mut document = Map::new();
    document.insert("servers".into(), Value::Object(servers));
    serde_json::to_vec(&Value::Object(document)).unwrap_or_default()
}

fn decode_credentials(bytes: &[u8]) -> Result<Credentials, SecretError> {
    let invalid = || SecretError::Crypto("decrypted credentials are not readable".into());
    let document: Value = serde_json::from_slice(bytes).map_err(|_| invalid())?;
    let secrets = |value: Option<&Value>| -> Result<BTreeMap<String, Secret>, SecretError> {
        value
            .and_then(Value::as_object)
            .ok_or_else(invalid)?
            .iter()
            .map(|(name, secret)| {
                let secret = secret.as_str().ok_or_else(invalid)?;
                Ok((name.clone(), Secret::new(secret)))
            })
            .collect()
    };
    let servers = document
        .get("servers")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?
        .iter()
        .map(|(name, server)| {
            let credentials = ServerCredentials {
                headers: secrets(server.get("headers"))?,
                env: secrets(server.get("env"))?,
            };
            Ok((name.clone(), credentials))
        })
        .collect::<Result<_, SecretError>>()?;
    Ok(Credentials { servers })
}

impl SecretStore for TursoStore {
    async fn put(
        &self,
        tenant_id: &TenantId,
        owner: &SecretOwner,
        credentials: &Credentials,
    ) -> Result<(), SecretError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let master_key = &self.inner.master_key;
        transaction(connection, async {
            let key = data_key(connection, master_key, tenant_id, true)
                .await?
                .ok_or_else(|| SecretError::Crypto("no data key".into()))?;
            let owner = owner_key(owner);
            let sealed = seal(
                &key,
                &secret_aad(tenant_id, &owner),
                &encode_credentials(credentials),
            )?;
            sql::execute(
                connection,
                "INSERT OR REPLACE INTO secrets (tenant, owner, ciphertext) VALUES (?, ?, ?)",
                vec![tenant(tenant_id), text(owner), SqlValue::Blob(sealed)],
            )
            .await?;
            Ok(())
        })
        .await
    }

    async fn resolve(
        &self,
        tenant_id: &TenantId,
        run: RunId,
        namespace: &NamespaceName,
        toolset: Digest,
    ) -> Result<Option<Credentials>, SecretError> {
        let connection = self.connection().await?;
        // The run's credentials, whole, else its namespace's for the toolset, else the
        // toolset's: never merged.
        let of_namespace = SecretOwner::Namespace {
            namespace: namespace.clone(),
            toolset,
        };
        for owner in [
            SecretOwner::Run(run),
            of_namespace,
            SecretOwner::Toolset(toolset),
        ] {
            let owner = owner_key(&owner);
            let row = sql::query_row(
                &connection,
                "SELECT ciphertext FROM secrets WHERE tenant = ? AND owner = ?",
                vec![tenant(tenant_id), text(owner.as_str())],
            )
            .await?;
            let Some(row) = row else {
                continue;
            };
            let key = data_key(&connection, &self.inner.master_key, tenant_id, false)
                .await?
                .ok_or_else(|| {
                    SecretError::Crypto(format!("tenant {tenant_id} has no data key"))
                })?;
            let plaintext = open(&key, &secret_aad(tenant_id, &owner), &row.blob(0)?)?;
            return decode_credentials(&plaintext).map(Some);
        }
        Ok(None)
    }

    async fn delete(&self, tenant_id: &TenantId, owner: &SecretOwner) -> Result<bool, SecretError> {
        let connection = self.connection().await?;
        let deleted = sql::execute(
            &connection,
            "DELETE FROM secrets WHERE tenant = ? AND owner = ?",
            vec![tenant(tenant_id), text(owner_key(owner))],
        )
        .await?;
        Ok(deleted > 0)
    }

    async fn delete_namespace(
        &self,
        tenant_id: &TenantId,
        namespace: &NamespaceName,
    ) -> Result<usize, SecretError> {
        let connection = self.connection().await?;
        let prefix = namespace_prefix(namespace);
        let deleted = sql::execute(
            &connection,
            "DELETE FROM secrets WHERE tenant = ? AND substr(owner, 1, ?) = ?",
            vec![
                tenant(tenant_id),
                SqlValue::Integer(i64::try_from(prefix.len()).unwrap_or(i64::MAX)),
                text(prefix),
            ],
        )
        .await?;
        Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_values_open_only_with_their_key_and_binding() {
        let key = [7u8; 32];
        let sealed = seal(&key, b"acme", b"secret").expect("seal");
        assert_eq!(open(&key, b"acme", &sealed).expect("open"), b"secret");
        assert!(open(&[8u8; 32], b"acme", &sealed).is_err(), "another key");
        assert!(open(&key, b"other", &sealed).is_err(), "another binding");
        let mut tampered = sealed.clone();
        *tampered.last_mut().expect("not empty") ^= 1;
        assert!(open(&key, b"acme", &tampered).is_err(), "tampered");
        assert_ne!(
            seal(&key, b"acme", b"secret").expect("seal"),
            sealed,
            "random nonces"
        );
    }

    #[test]
    fn credentials_round_trip() {
        let credentials = Credentials {
            servers: BTreeMap::from([(
                "github".to_owned(),
                ServerCredentials {
                    headers: BTreeMap::from([(
                        "Authorization".to_owned(),
                        Secret::new("Bearer x"),
                    )]),
                    env: BTreeMap::from([("TOKEN".to_owned(), Secret::new("y"))]),
                },
            )]),
        };
        assert_eq!(
            decode_credentials(&encode_credentials(&credentials)).expect("decode"),
            credentials
        );
    }

    #[test]
    fn master_keys_are_never_printed() {
        let key = MasterKey::from_bytes([42; 32]);
        assert_eq!(format!("{key:?}"), "MasterKey(<redacted>)");
    }
}
