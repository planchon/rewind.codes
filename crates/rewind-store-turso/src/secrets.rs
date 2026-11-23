//! `SecretStore` on Turso (`archi/04-tools.md`, "Secrets" and "Signed calls";
//! `archi/06-storage-and-scale.md`, "Tables"): named secrets of the organization (namespace `*`)
//! and of each namespace, their reads per day, and each tenant's Ed25519 signing keys, with
//! envelope encryption.
//!
//! ```text
//! master keys (configuration)   current ─ encrypts          previous ─ only decrypt
//!        │ id: first 8 bytes of the key's SHA-256, kept with each data key
//!        ▼
//! data_keys (tenant, wrapped_key, master_key)     one per tenant, 256 random bits
//!        │
//!        ├─▶ secrets.ciphertext            bound to tenant, namespace, name
//!        └─▶ signing_keys.private_ciphertext   bound to tenant, kid
//! ```
//!
//! The same scheme as the production store's (`rewind-store-postgres`), so that a value reads the
//! same whatever the backend: XChaCha20-Poly1305 with a random 192-bit nonce stored before the
//! ciphertext, the associated data binding what is sealed to where it belongs. A tenant's data
//! key is made with its first secret or signing key, under the current master key. Replacing the
//! master key encrypts the data keys again, one tenant per transaction (`rewrap_data_keys`); the
//! values and private keys are not touched. Every call runs alone on the store's one connection.
//!
//! `put` and the deletions send each secret they wrote or deleted as a change once they
//! committed: the owners resume the calls parked for it. A read only counts, and the signing keys
//! have no change: they send nothing.

use std::collections::BTreeMap;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rewind_core::{SecretName, Timestamp};
use rewind_engine::ports::{
    ControlPlaneChange, DailyReads, Day, LastRead, MasterKeyId, NamespaceName, NewSecret,
    PrivateKey, PublicKey, PutSecret, RETIRED_KEY_PUBLISHED, Secret, SecretError, SecretId,
    SecretMeta, SecretNamespace, SecretReader, SecretStore, SigningKey, TenantId,
};
use turso::{Connection, Row, Value};

use crate::codec::{decode_labels, encode_labels};
use crate::sql::{self, Columns, duration_ms, millis, opt_text, tenant, text, transaction, ts};
use crate::{Error, TursoStore};

const NONCE_LEN: usize = 24;

/// A key that encrypts the tenants' data keys: 256 bits from the deployment's configuration. It
/// never enters the database; without it, the secrets of the data keys it holds cannot be read.
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

    /// The key's id, which each data key records: the first 8 bytes of its SHA-256, in hex.
    pub fn id(&self) -> MasterKeyId {
        MasterKeyId::of(&self.0)
    }
}

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

/// The master keys a store holds (`archi/04-tools.md`, "Secrets": the master key can be
/// replaced): the current one, which encrypts the data keys it makes or rewraps, and the previous
/// ones, which only decrypt the data keys still held under them.
#[derive(Clone)]
pub struct MasterKeys {
    current: (MasterKeyId, MasterKey),
    /// No id twice, and never the current one's.
    previous: Vec<(MasterKeyId, MasterKey)>,
}

impl MasterKeys {
    pub fn new(current: MasterKey) -> Self {
        Self {
            current: (current.id(), current),
            previous: Vec::new(),
        }
    }

    /// Adds keys that only decrypt. A key already held is left out.
    pub fn with_previous(mut self, previous: impl IntoIterator<Item = MasterKey>) -> Self {
        for key in previous {
            let id = key.id();
            if !self.ids().any(|held| *held == id) {
                self.previous.push((id, key));
            }
        }
        self
    }

    /// The id of the current key.
    pub fn current_id(&self) -> &MasterKeyId {
        &self.current.0
    }

    /// The ids of every key held, the current one first.
    pub fn ids(&self) -> impl Iterator<Item = &MasterKeyId> {
        std::iter::once(&self.current.0).chain(self.previous.iter().map(|(id, _)| id))
    }

    fn current(&self) -> &MasterKey {
        &self.current.1
    }

    /// The key whose id is `id`, current or previous.
    fn get(&self, id: &MasterKeyId) -> Option<&MasterKey> {
        std::iter::once(&self.current)
            .chain(&self.previous)
            .find(|(held, _)| held == id)
            .map(|(_, key)| key)
    }
}

impl From<MasterKey> for MasterKeys {
    fn from(current: MasterKey) -> Self {
        Self::new(current)
    }
}

impl std::fmt::Debug for MasterKeys {
    /// The ids only, which are not secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MasterKeys")
            .field("current", &self.current.0)
            .field(
                "previous",
                &self.previous.iter().map(|(id, _)| id).collect::<Vec<_>>(),
            )
            .finish()
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

// -------------------------------------------------------------------------------------------
// Sealing

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

/// The plaintext of [`seal`]'s output; an error for a wrong key, another binding or a tampered
/// value.
fn open(key: &[u8; 32], aad: &[u8], sealed: &[u8], what: &str) -> Result<Vec<u8>, SecretError> {
    let invalid = || SecretError::Crypto(format!("{what} does not decrypt: a corrupt value"));
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

/// What a data key is bound to: its tenant. The credentials' data keys used the same binding.
fn data_key_aad(tenant_id: &str) -> Vec<u8> {
    format!("rewind data key\0{tenant_id}").into_bytes()
}

/// What a secret's value is bound to: its tenant, namespace (`*` for the organization) and name.
fn secret_aad(tenant_id: &TenantId, secret: &SecretId) -> Vec<u8> {
    format!(
        "rewind secret\0{tenant_id}\0{}\0{}",
        secret.namespace.as_str(),
        secret.name
    )
    .into_bytes()
}

/// What a signing key's private part is bound to: its tenant and `kid`.
fn private_key_aad(tenant_id: &TenantId, kid: &str) -> Vec<u8> {
    format!("rewind signing key\0{tenant_id}\0{kid}").into_bytes()
}

/// A data key's plaintext, opened with the master key whose id it records.
fn open_data_key(
    keys: &MasterKeys,
    tenant_id: &str,
    wrapped: &[u8],
    master_key: &str,
) -> Result<[u8; 32], SecretError> {
    let id = MasterKeyId::parse(master_key).ok_or_else(|| {
        SecretError::Crypto(format!(
            "the data key of {tenant_id} names no master key: {master_key:?}"
        ))
    })?;
    let master = keys
        .get(&id)
        .ok_or_else(|| SecretError::UnknownMasterKey(id.clone()))?;
    let key = open(
        &master.0,
        &data_key_aad(tenant_id),
        wrapped,
        &format!("the data key of {tenant_id}"),
    )?;
    key.try_into()
        .map_err(|_| SecretError::Crypto("a data key is not 256 bits".into()))
}

async fn stored_data_key(
    connection: &Connection,
    tenant_id: &str,
) -> Result<Option<(Vec<u8>, String)>, Error> {
    let row = sql::query_row(
        connection,
        "SELECT wrapped_key, master_key FROM data_keys WHERE tenant = ?",
        vec![text(tenant_id)],
    )
    .await?;
    row.map(|row| Ok((row.blob(0)?, row.text(1)?))).transpose()
}

/// The tenant's data key; made first, under the current master key, if `create` and the tenant
/// has none.
async fn data_key(
    connection: &Connection,
    keys: &MasterKeys,
    tenant_id: &TenantId,
    create: bool,
) -> Result<Option<[u8; 32]>, SecretError> {
    if let Some((wrapped, master_key)) = stored_data_key(connection, tenant_id.as_str()).await? {
        return open_data_key(keys, tenant_id.as_str(), &wrapped, &master_key).map(Some);
    }
    if !create {
        return Ok(None);
    }
    let key: [u8; 32] = random_bytes()?;
    let wrapped = seal(&keys.current().0, &data_key_aad(tenant_id.as_str()), &key)?;
    sql::execute(
        connection,
        "INSERT INTO data_keys (tenant, wrapped_key, master_key) VALUES (?, ?, ?)",
        vec![
            tenant(tenant_id),
            Value::Blob(wrapped),
            text(keys.current_id().as_str()),
        ],
    )
    .await?;
    Ok(Some(key))
}

/// The tenant's data key, which must exist: something was sealed with it.
async fn existing_data_key(
    connection: &Connection,
    keys: &MasterKeys,
    tenant_id: &TenantId,
) -> Result<[u8; 32], SecretError> {
    data_key(connection, keys, tenant_id, false)
        .await?
        .ok_or_else(|| SecretError::Crypto(format!("tenant {tenant_id} has no data key")))
}

// -------------------------------------------------------------------------------------------
// Secrets

/// A secret's rows joined to its reads from the first parameter on, one row per day (a single row
/// with no day for a secret without reads), by name then day; read by [`metas_from_rows`].
macro_rules! metas_from {
    ($filter:literal) => {
        concat!(
            "SELECT s.namespace, s.name, s.labels, s.created_by, s.created_at, s.updated_by, \
             s.updated_at, s.last_read_at, s.last_read_run, s.last_read_namespace, r.day, r.reads \
             FROM secrets s LEFT JOIN secret_reads r \
             ON r.tenant = s.tenant AND r.namespace = s.namespace AND r.name = s.name \
             AND r.day >= ? \
             WHERE s.tenant = ? AND s.namespace = ?",
            $filter,
            " ORDER BY s.name, r.day"
        )
    };
}

fn secret_namespace(name: &str) -> Result<SecretNamespace, Error> {
    if name == SecretNamespace::ORGANIZATION {
        return Ok(SecretNamespace::Organization);
    }
    NamespaceName::new(name)
        .map(SecretNamespace::Namespace)
        .map_err(|_| Error::Corrupt(format!("invalid namespace of a secret {name:?}")))
}

fn namespace_name(name: String) -> Result<NamespaceName, Error> {
    NamespaceName::new(name.clone())
        .map_err(|_| Error::Corrupt(format!("invalid namespace {name:?}")))
}

/// The secrets of rows from [`metas_from`], in their order.
fn metas_from_rows(rows: &[Row]) -> Result<Vec<SecretMeta>, Error> {
    let mut metas: Vec<SecretMeta> = Vec::new();
    for row in rows {
        let name = row.text(1)?;
        let reads = match row.opt_int(10)? {
            None => None,
            Some(day) => Some(DailyReads {
                day: Day(u32::try_from(day).map_err(|_| Error::Corrupt(format!("day {day}")))?),
                reads: row.u64(11)?,
            }),
        };
        if let Some(last) = metas.last_mut()
            && last.id.name.as_str() == name
        {
            last.reads.extend(reads);
            continue;
        }
        let last_read = match row.opt_ts(7)? {
            None => None,
            Some(at) => Some(LastRead {
                by: match (row.opt_run_id(8)?, row.opt_text(9)?) {
                    (Some(run), Some(namespace)) => SecretReader::Run {
                        run,
                        namespace: namespace_name(namespace)?,
                    },
                    _ => SecretReader::Registration,
                },
                at,
            }),
        };
        metas.push(SecretMeta {
            id: SecretId {
                namespace: secret_namespace(&row.text(0)?)?,
                name: SecretName::new(name.clone())
                    .map_err(|_| Error::Corrupt(format!("invalid secret name {name:?}")))?,
            },
            labels: decode_labels(&row.text(2)?)?,
            created_by: row.text(3)?,
            created_at: row.ts(4)?,
            updated_by: row.text(5)?,
            updated_at: row.ts(6)?,
            reads: reads.into_iter().collect(),
            last_read,
        });
    }
    Ok(metas)
}

fn day(day: Day) -> Value {
    Value::Integer(i64::from(day.0))
}

/// The key of a secret's rows: tenant, namespace, name.
fn secret_key(tenant_id: &TenantId, secret: &SecretId) -> Vec<Value> {
    vec![
        tenant(tenant_id),
        text(secret.namespace.as_str()),
        text(secret.name.as_str()),
    ]
}

/// Refuses labels on a namespace's secret.
fn check_labels(secret: &NewSecret) -> Result<(), SecretError> {
    if matches!(secret.id.namespace, SecretNamespace::Namespace(_)) && !secret.labels.is_empty() {
        return Err(SecretError::Invalid(
            "a namespace's secret carries no label: the namespace is its scope".into(),
        ));
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------
// Signing keys

/// A new Ed25519 key pair, from the operating system's CSPRNG.
fn new_signing_key(now: Timestamp) -> Result<SigningKey, SecretError> {
    let seed: [u8; 32] = random_bytes()?;
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

/// The columns of a signing key, read by [`public_key_from_row`].
macro_rules! key_columns {
    () => {
        "kid, public_key, created_at, retired_at"
    };
}

fn public_key_from_row(row: &Row) -> Result<PublicKey, Error> {
    let kid = row.text(0)?;
    let key: [u8; 32] = row
        .blob(1)?
        .try_into()
        .map_err(|_| Error::Corrupt(format!("the public key {kid} is not 32 bytes")))?;
    if PublicKey::kid_of(&key) != kid {
        return Err(Error::Corrupt(format!(
            "the signing key {kid} is not its public key's"
        )));
    }
    Ok(PublicKey {
        kid,
        key,
        created_at: row.ts(2)?,
        retired_at: row.opt_ts(3)?,
    })
}

/// The tenant's current signing key, opened with its data key.
async fn current_key(
    connection: &Connection,
    keys: &MasterKeys,
    tenant_id: &TenantId,
) -> Result<Option<SigningKey>, SecretError> {
    let row = sql::query_row(
        connection,
        concat!(
            "SELECT ",
            key_columns!(),
            ", private_ciphertext FROM signing_keys WHERE tenant = ? AND retired_at IS NULL"
        ),
        vec![tenant(tenant_id)],
    )
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let public = public_key_from_row(&row)?;
    let data_key = existing_data_key(connection, keys, tenant_id).await?;
    let seed: [u8; 32] = open(
        &data_key,
        &private_key_aad(tenant_id, &public.kid),
        &row.blob(4)?,
        &format!("the signing key {}", public.kid),
    )?
    .try_into()
    .map_err(|_| SecretError::Crypto("an Ed25519 seed has 32 bytes".into()))?;
    Ok(Some(SigningKey {
        public,
        private: PrivateKey::from_seed(seed),
    }))
}

/// Makes the tenant a new current signing key; the caller retired the current one.
async fn make_signing_key(
    connection: &Connection,
    keys: &MasterKeys,
    tenant_id: &TenantId,
    now: Timestamp,
) -> Result<SigningKey, SecretError> {
    let data_key = data_key(connection, keys, tenant_id, true)
        .await?
        .ok_or_else(|| SecretError::Crypto("no data key".into()))?;
    let made = new_signing_key(now)?;
    let sealed = seal(
        &data_key,
        &private_key_aad(tenant_id, &made.public.kid),
        made.private.expose(),
    )?;
    sql::execute(
        connection,
        "INSERT INTO signing_keys (tenant, kid, public_key, private_ciphertext, created_at) \
         VALUES (?, ?, ?, ?, ?)",
        vec![
            tenant(tenant_id),
            text(made.public.kid.as_str()),
            Value::Blob(made.public.key.to_vec()),
            Value::Blob(sealed),
            ts(now),
        ],
    )
    .await?;
    Ok(made)
}

/// The time before which a retired key is no longer published at `now`.
fn published_since(now: Timestamp) -> Value {
    Value::Integer(millis(now).saturating_sub(duration_ms(RETIRED_KEY_PUBLISHED)))
}

impl SecretStore for TursoStore {
    async fn put(
        &self,
        tenant_id: &TenantId,
        secret: &NewSecret,
        now: Timestamp,
    ) -> Result<PutSecret, SecretError> {
        check_labels(secret)?;
        let connection = self.connection().await?;
        let connection = &*connection;
        let keys = &self.inner.master_keys;
        let put = transaction(connection, async {
            let key = data_key(connection, keys, tenant_id, true)
                .await?
                .ok_or_else(|| SecretError::Crypto("no data key".into()))?;
            let sealed = seal(
                &key,
                &secret_aad(tenant_id, &secret.id),
                secret.value.expose().as_bytes(),
            )?;
            let exists = sql::query_row(
                connection,
                "SELECT 1 FROM secrets WHERE tenant = ? AND namespace = ? AND name = ?",
                secret_key(tenant_id, &secret.id),
            )
            .await?
            .is_some();
            let labels = encode_labels(&secret.labels);
            if exists {
                let mut params = vec![
                    Value::Blob(sealed),
                    text(labels),
                    text(secret.by.as_str()),
                    ts(now),
                ];
                params.extend(secret_key(tenant_id, &secret.id));
                sql::execute(
                    connection,
                    "UPDATE secrets SET ciphertext = ?, labels = ?, updated_by = ?, updated_at = ? \
                     WHERE tenant = ? AND namespace = ? AND name = ?",
                    params,
                )
                .await?;
                return Ok(PutSecret::Replaced);
            }
            let mut params = secret_key(tenant_id, &secret.id);
            params.extend([
                Value::Blob(sealed),
                text(labels),
                text(secret.by.as_str()),
                ts(now),
                text(secret.by.as_str()),
                ts(now),
            ]);
            sql::execute(
                connection,
                "INSERT INTO secrets (tenant, namespace, name, ciphertext, labels, created_by, \
                     created_at, updated_by, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params,
            )
            .await?;
            Ok::<_, SecretError>(PutSecret::Created)
        })
        .await?;
        self.send([ControlPlaneChange::Secret {
            tenant: tenant_id.clone(),
            secret: secret.id.clone(),
        }]);
        Ok(put)
    }

    async fn list(
        &self,
        tenant_id: &TenantId,
        namespace: &SecretNamespace,
        labels: &BTreeMap<String, String>,
        since: Day,
    ) -> Result<Vec<SecretMeta>, SecretError> {
        let connection = self.connection().await?;
        let rows = sql::query(
            &connection,
            metas_from!(""),
            vec![day(since), tenant(tenant_id), text(namespace.as_str())],
        )
        .await?;
        // A tenant has few secrets: the labels are matched here.
        Ok(metas_from_rows(&rows)?
            .into_iter()
            .filter(|meta| {
                labels
                    .iter()
                    .all(|(key, value)| meta.labels.get(key) == Some(value))
            })
            .collect())
    }

    async fn meta(
        &self,
        tenant_id: &TenantId,
        secret: &SecretId,
        since: Day,
    ) -> Result<Option<SecretMeta>, SecretError> {
        let connection = self.connection().await?;
        let mut params = vec![day(since)];
        params.extend(secret_key(tenant_id, secret));
        let rows = sql::query(&connection, metas_from!(" AND s.name = ?"), params).await?;
        Ok(metas_from_rows(&rows)?.into_iter().next())
    }

    async fn read(
        &self,
        tenant_id: &TenantId,
        secret: &SecretId,
        reader: &SecretReader,
        now: Timestamp,
    ) -> Result<Option<Secret>, SecretError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let keys = &self.inner.master_keys;
        transaction(connection, async {
            let row = sql::query_row(
                connection,
                "SELECT ciphertext, last_read_at FROM secrets \
                 WHERE tenant = ? AND namespace = ? AND name = ?",
                secret_key(tenant_id, secret),
            )
            .await?;
            let Some(row) = row else {
                return Ok(None);
            };
            let sealed = row.blob(0)?;
            // The last read stays the latest one.
            if row.opt_ts(1)?.is_none_or(|last| last <= now) {
                let mut params = vec![
                    ts(now),
                    opt_text(reader.run().map(|(run, _)| run.to_string())),
                    opt_text(reader.run().map(|(_, namespace)| namespace.as_str())),
                ];
                params.extend(secret_key(tenant_id, secret));
                sql::execute(
                    connection,
                    "UPDATE secrets SET last_read_at = ?, last_read_run = ?, \
                         last_read_namespace = ? \
                     WHERE tenant = ? AND namespace = ? AND name = ?",
                    params,
                )
                .await?;
            }
            let mut params = secret_key(tenant_id, secret);
            params.push(day(Day::of(now)));
            sql::execute(
                connection,
                "INSERT INTO secret_reads (tenant, namespace, name, day, reads) \
                 VALUES (?, ?, ?, ?, 1) \
                 ON CONFLICT (tenant, namespace, name, day) DO UPDATE SET reads = reads + 1",
                params,
            )
            .await?;
            let key = existing_data_key(connection, keys, tenant_id).await?;
            let value = open(
                &key,
                &secret_aad(tenant_id, secret),
                &sealed,
                &format!("the secret {secret}"),
            )?;
            let value = String::from_utf8(value)
                .map_err(|_| SecretError::Crypto(format!("the secret {secret} is not UTF-8")))?;
            Ok(Some(Secret::new(value)))
        })
        .await
    }

    async fn delete(&self, tenant_id: &TenantId, secret: &SecretId) -> Result<bool, SecretError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let deleted = transaction(connection, async {
            let deleted = sql::execute(
                connection,
                "DELETE FROM secrets WHERE tenant = ? AND namespace = ? AND name = ?",
                secret_key(tenant_id, secret),
            )
            .await?;
            sql::execute(
                connection,
                "DELETE FROM secret_reads WHERE tenant = ? AND namespace = ? AND name = ?",
                secret_key(tenant_id, secret),
            )
            .await?;
            Ok::<_, SecretError>(deleted > 0)
        })
        .await?;
        if deleted {
            self.send([ControlPlaneChange::Secret {
                tenant: tenant_id.clone(),
                secret: secret.clone(),
            }]);
        }
        Ok(deleted)
    }

    async fn delete_namespace(
        &self,
        tenant_id: &TenantId,
        namespace: &NamespaceName,
    ) -> Result<usize, SecretError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let deleted = transaction(connection, async {
            let params = || vec![tenant(tenant_id), text(namespace.as_str())];
            let names = sql::query(
                connection,
                "SELECT name FROM secrets WHERE tenant = ? AND namespace = ?",
                params(),
            )
            .await?
            .iter()
            .map(|row| SecretName::new(row.text(0)?).map_err(|err| Error::Corrupt(err.to_string())))
            .collect::<Result<Vec<_>, Error>>()?;
            sql::execute(
                connection,
                "DELETE FROM secrets WHERE tenant = ? AND namespace = ?",
                params(),
            )
            .await?;
            sql::execute(
                connection,
                "DELETE FROM secret_reads WHERE tenant = ? AND namespace = ?",
                params(),
            )
            .await?;
            Ok::<_, SecretError>(names)
        })
        .await?;
        self.send(deleted.iter().map(|name| ControlPlaneChange::Secret {
            tenant: tenant_id.clone(),
            secret: SecretId::of_namespace(namespace.clone(), name.clone()),
        }));
        Ok(deleted.len())
    }

    async fn signing_key(
        &self,
        tenant_id: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, SecretError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let keys = &self.inner.master_keys;
        if let Some(key) = current_key(connection, keys, tenant_id).await? {
            return Ok(key);
        }
        transaction(
            connection,
            make_signing_key(connection, keys, tenant_id, now),
        )
        .await
    }

    async fn rotate_signing_key(
        &self,
        tenant_id: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, SecretError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let keys = &self.inner.master_keys;
        transaction(connection, async {
            // The keys no longer published can no longer be checked against: they go.
            sql::execute(
                connection,
                "DELETE FROM signing_keys WHERE tenant = ? AND retired_at <= ?",
                vec![tenant(tenant_id), published_since(now)],
            )
            .await?;
            sql::execute(
                connection,
                "UPDATE signing_keys SET retired_at = ? WHERE tenant = ? AND retired_at IS NULL",
                vec![ts(now), tenant(tenant_id)],
            )
            .await?;
            make_signing_key(connection, keys, tenant_id, now).await
        })
        .await
    }

    async fn public_keys(
        &self,
        tenant_id: &TenantId,
        now: Timestamp,
    ) -> Result<Vec<PublicKey>, SecretError> {
        let connection = self.connection().await?;
        // The current key first, then the retired ones still published, newest first.
        let rows = sql::query(
            &connection,
            concat!(
                "SELECT ",
                key_columns!(),
                " FROM signing_keys WHERE tenant = ? AND (retired_at IS NULL OR retired_at > ?) \
                 ORDER BY retired_at IS NOT NULL, created_at DESC, retired_at DESC, kid"
            ),
            vec![tenant(tenant_id), published_since(now)],
        )
        .await?;
        Ok(rows
            .iter()
            .map(public_key_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn master_keys_in_use(&self) -> Result<Vec<MasterKeyId>, SecretError> {
        let connection = self.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT DISTINCT master_key FROM data_keys ORDER BY master_key",
            Vec::new(),
        )
        .await?;
        rows.iter()
            .map(|row| {
                let id = row.text(0)?;
                MasterKeyId::parse(&id).ok_or_else(|| {
                    SecretError::Crypto(format!("a data key names no master key: {id:?}"))
                })
            })
            .collect()
    }

    async fn rewrap_data_keys(&self) -> Result<usize, SecretError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let keys = &self.inner.master_keys;
        let current = keys.current_id().as_str();
        let tenants = sql::query(
            connection,
            "SELECT tenant FROM data_keys WHERE master_key <> ? ORDER BY tenant",
            vec![text(current)],
        )
        .await?
        .iter()
        .map(|row| row.text(0))
        .collect::<Result<Vec<_>, Error>>()?;
        let mut changed = 0;
        for tenant_id in &tenants {
            let rewrapped = transaction(connection, async {
                let Some((wrapped, master_key)) = stored_data_key(connection, tenant_id).await?
                else {
                    return Ok::<_, SecretError>(false);
                };
                if master_key == current {
                    return Ok(false);
                }
                let key = open_data_key(keys, tenant_id, &wrapped, &master_key)?;
                let rewrapped = seal(&keys.current().0, &data_key_aad(tenant_id), &key)?;
                sql::execute(
                    connection,
                    "UPDATE data_keys SET wrapped_key = ?, master_key = ? WHERE tenant = ?",
                    vec![
                        Value::Blob(rewrapped),
                        text(current),
                        text(tenant_id.as_str()),
                    ],
                )
                .await?;
                Ok(true)
            })
            .await?;
            changed += usize::from(rewrapped);
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_keys_are_never_printed() {
        let key = MasterKey::from_bytes([42; 32]);
        assert_eq!(format!("{key:?}"), "MasterKey(<redacted>)");
        let keys = MasterKeys::new(key.clone()).with_previous([MasterKey::from_bytes([7; 32])]);
        let printed = format!("{keys:?}");
        assert!(printed.contains(key.id().as_str()), "{printed}");
        assert!(!printed.contains("42"), "{printed}");
    }

    #[test]
    fn a_ring_holds_each_key_once_the_current_one_first() {
        let (current, old) = (
            MasterKey::from_bytes([1; 32]),
            MasterKey::from_bytes([2; 32]),
        );
        let keys = MasterKeys::new(current.clone()).with_previous([
            old.clone(),
            current.clone(),
            old.clone(),
        ]);
        assert_eq!(
            keys.ids().cloned().collect::<Vec<_>>(),
            [current.id(), old.id()]
        );
        assert_eq!(keys.current_id(), &current.id());
        assert_eq!(keys.get(&old.id()), Some(&old));
        assert_eq!(keys.get(&MasterKey::from_bytes([3; 32]).id()), None);
    }

    #[test]
    fn sealed_values_open_only_with_their_key_and_binding() {
        let key = [7u8; 32];
        let sealed = seal(&key, b"acme", b"secret").expect("seal");
        assert_eq!(open(&key, b"acme", &sealed, "it").expect("open"), b"secret");
        assert!(
            open(&[8u8; 32], b"acme", &sealed, "it").is_err(),
            "another key"
        );
        assert!(
            open(&key, b"other", &sealed, "it").is_err(),
            "another binding"
        );
        let mut tampered = sealed.clone();
        *tampered.last_mut().expect("not empty") ^= 1;
        assert!(open(&key, b"acme", &tampered, "it").is_err(), "tampered");
        assert_ne!(
            seal(&key, b"acme", b"secret").expect("seal"),
            sealed,
            "random nonces"
        );
    }

    #[test]
    fn a_data_key_opens_only_under_the_master_key_it_names() {
        let (current, old) = (
            MasterKey::from_bytes([1; 32]),
            MasterKey::from_bytes([2; 32]),
        );
        let data_key = [9u8; 32];
        let wrapped = seal(&old.0, &data_key_aad("acme"), &data_key).expect("seal");
        let ring = MasterKeys::new(current.clone()).with_previous([old.clone()]);
        assert_eq!(
            open_data_key(&ring, "acme", &wrapped, old.id().as_str()),
            Ok(data_key)
        );
        assert_eq!(
            open_data_key(
                &MasterKeys::new(current),
                "acme",
                &wrapped,
                old.id().as_str()
            ),
            Err(SecretError::UnknownMasterKey(old.id())),
            "a key the store was not given"
        );
        assert!(
            open_data_key(&ring, "globex", &wrapped, old.id().as_str()).is_err(),
            "another tenant's"
        );
    }
}
