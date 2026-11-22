//! `SecretStore` on Postgres (`archi/04-tools.md`, "Secrets" and "Signed calls";
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
//! Every seal is XChaCha20-Poly1305 with a random 192-bit nonce stored before the ciphertext; the
//! associated data binds what is sealed to where it belongs, so a row copied elsewhere does not
//! decrypt. A tenant's data key is made with its first secret or signing key, under the current
//! master key. Replacing the master key encrypts the data keys again, one tenant per transaction
//! (`rewrap_data_keys`); the values and private keys are not touched.
//!
//! Two servers making a tenant's first data key at once keep the first one stored, and both
//! encrypt with it. A tenant's signing keys are written under a transaction-scoped advisory lock
//! of the tenant, and a partial unique index keeps one current key per tenant. A read takes the
//! secret's row lock while it counts: a deletion waits for it, and leaves no read behind.

use std::collections::BTreeMap;

use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rewind_core::{SecretName, Timestamp};
use rewind_engine::ports::{
    DailyReads, Day, LastRead, MasterKeyId, NamespaceName, NewSecret, PrivateKey, PublicKey,
    PutSecret, RETIRED_KEY_PUBLISHED, Secret, SecretError, SecretId, SecretMeta, SecretNamespace,
    SecretReader, SecretStore, SigningKey, TenantId,
};
use sqlx::PgConnection;
use sqlx::postgres::PgRow;

use crate::codec::{decode_labels, encode_labels};
use crate::sql::{Columns, Failure, ms, retrying, tenant, ts, uuid};
use crate::{Error, PostgresStore};

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

type Fail = Failure<SecretError>;

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
    conn: &mut PgConnection,
    tenant_id: &TenantId,
) -> Result<Option<(Vec<u8>, String)>, Error> {
    Ok(
        sqlx::query_as("SELECT wrapped_key, master_key FROM data_keys WHERE tenant = $1")
            .bind(tenant(tenant_id))
            .fetch_optional(&mut *conn)
            .await?,
    )
}

/// The tenant's data key; made first, under the current master key, if `create` and the tenant
/// has none.
async fn data_key(
    conn: &mut PgConnection,
    keys: &MasterKeys,
    tenant_id: &TenantId,
    create: bool,
) -> Result<Option<[u8; 32]>, Fail> {
    let (wrapped, master_key) = match stored_data_key(conn, tenant_id).await? {
        Some(stored) => stored,
        None if !create => return Ok(None),
        None => {
            let key: [u8; 32] = random_bytes()?;
            let wrapped = seal(&keys.current().0, &data_key_aad(tenant_id.as_str()), &key)?;
            // A key stored by a concurrent call first is kept, and used.
            sqlx::query(
                "INSERT INTO data_keys (tenant, wrapped_key, master_key) VALUES ($1, $2, $3) \
                 ON CONFLICT (tenant) DO NOTHING",
            )
            .bind(tenant(tenant_id))
            .bind(wrapped)
            .bind(keys.current_id().as_str())
            .execute(&mut *conn)
            .await?;
            stored_data_key(conn, tenant_id).await?.ok_or_else(|| {
                SecretError::Backend(format!("the data key of {tenant_id} was not stored"))
            })?
        }
    };
    Ok(Some(open_data_key(
        keys,
        tenant_id.as_str(),
        &wrapped,
        &master_key,
    )?))
}

/// The tenant's data key, which must exist: something was sealed with it.
async fn existing_data_key(
    conn: &mut PgConnection,
    keys: &MasterKeys,
    tenant_id: &TenantId,
) -> Result<[u8; 32], Fail> {
    Ok(data_key(conn, keys, tenant_id, false)
        .await?
        .ok_or_else(|| SecretError::Crypto(format!("tenant {tenant_id} has no data key")))?)
}

// -------------------------------------------------------------------------------------------
// Secrets

/// The columns of a secret's metadata and of one day of its reads, read by [`metas_from_rows`].
macro_rules! meta_columns {
    () => {
        "s.namespace, s.name, s.labels::text, s.created_by, s.created_at, s.updated_by, \
         s.updated_at, s.last_read_at, s.last_read_run, s.last_read_namespace, r.day, r.reads"
    };
}

/// A secret's rows joined to its reads from `$4` on, one row per day (a single row with no day
/// for a secret without reads), by name then day.
macro_rules! metas_from {
    ($filter:literal) => {
        concat!(
            "SELECT ",
            meta_columns!(),
            " FROM secrets s LEFT JOIN secret_reads r \
             ON r.tenant = s.tenant AND r.namespace = s.namespace AND r.name = s.name \
             AND r.day >= $3 \
             WHERE s.tenant = $1 AND s.namespace = $2 AND ",
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
fn metas_from_rows(rows: &[PgRow]) -> Result<Vec<SecretMeta>, Error> {
    let mut metas: Vec<SecretMeta> = Vec::new();
    for row in rows {
        let name = row.text(1)?;
        let reads = match row.opt_i64(10)? {
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

fn day(day: Day) -> i64 {
    i64::from(day.0)
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

fn public_key_from_row(row: &PgRow) -> Result<PublicKey, Error> {
    let kid = row.text(0)?;
    let key: [u8; 32] = row
        .bytes(1)?
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

/// The tenant's current signing key, sealed: its public part and private ciphertext.
async fn current_key(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
) -> Result<Option<(PublicKey, Vec<u8>)>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        key_columns!(),
        ", private_ciphertext FROM signing_keys WHERE tenant = $1 AND retired_at IS NULL"
    ))
    .bind(tenant(tenant_id))
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| Ok((public_key_from_row(&row)?, row.bytes(4)?)))
        .transpose()
}

/// Serializes the writes of a tenant's signing keys until the transaction ends.
async fn lock_signing_keys(conn: &mut PgConnection, tenant_id: &TenantId) -> Result<(), Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("rewind signing keys\u{1f}{tenant_id}"))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Opens a signing key's private part with the tenant's data key.
async fn open_signing_key(
    conn: &mut PgConnection,
    keys: &MasterKeys,
    tenant_id: &TenantId,
    public: PublicKey,
    sealed: &[u8],
) -> Result<SigningKey, Fail> {
    let key = existing_data_key(conn, keys, tenant_id).await?;
    let seed: [u8; 32] = open(
        &key,
        &private_key_aad(tenant_id, &public.kid),
        sealed,
        &format!("the signing key {}", public.kid),
    )?
    .try_into()
    .map_err(|_| SecretError::Crypto("an Ed25519 seed has 32 bytes".into()))?;
    Ok(SigningKey {
        public,
        private: PrivateKey::from_seed(seed),
    })
}

/// Makes the tenant a new current signing key; the caller holds the tenant's lock and retired
/// the current one.
async fn make_signing_key(
    conn: &mut PgConnection,
    keys: &MasterKeys,
    tenant_id: &TenantId,
    now: Timestamp,
) -> Result<SigningKey, Fail> {
    let data_key = data_key(conn, keys, tenant_id, true)
        .await?
        .ok_or_else(|| SecretError::Crypto("no data key".into()))?;
    let made = new_signing_key(now)?;
    let sealed = seal(
        &data_key,
        &private_key_aad(tenant_id, &made.public.kid),
        made.private.expose(),
    )?;
    sqlx::query(
        "INSERT INTO signing_keys (tenant, kid, public_key, private_ciphertext, created_at) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(tenant(tenant_id))
    .bind(made.public.kid.as_str())
    .bind(made.public.key.to_vec())
    .bind(sealed)
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    Ok(made)
}

/// The time before which a retired key is no longer published at `now`.
fn published_since(now: Timestamp) -> i64 {
    ts(now).saturating_sub(ms(RETIRED_KEY_PUBLISHED))
}

// -------------------------------------------------------------------------------------------
// One attempt of each method that runs a transaction.

impl PostgresStore {
    async fn put_once(
        &self,
        tenant_id: &TenantId,
        secret: &NewSecret,
        now: Timestamp,
    ) -> Result<PutSecret, Fail> {
        let mut tx = self.pool.begin().await?;
        let key = data_key(&mut tx, &self.master_keys, tenant_id, true)
            .await?
            .ok_or_else(|| SecretError::Crypto("no data key".into()))?;
        let sealed = seal(
            &key,
            &secret_aad(tenant_id, &secret.id),
            secret.value.expose().as_bytes(),
        )?;
        // A row just inserted has no `xmax`; a row the conflict updated has this transaction's.
        let created: bool = sqlx::query_scalar(
            "INSERT INTO secrets (tenant, namespace, name, ciphertext, labels, created_by, \
                 created_at, updated_by, updated_at) \
             VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, $6, $7) \
             ON CONFLICT (tenant, namespace, name) DO UPDATE SET \
                 ciphertext = EXCLUDED.ciphertext, labels = EXCLUDED.labels, \
                 updated_by = EXCLUDED.updated_by, updated_at = EXCLUDED.updated_at \
             RETURNING xmax = 0",
        )
        .bind(tenant(tenant_id))
        .bind(secret.id.namespace.as_str())
        .bind(secret.id.name.as_str())
        .bind(sealed)
        .bind(encode_labels(&secret.labels))
        .bind(secret.by.as_str())
        .bind(ts(now))
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(if created {
            PutSecret::Created
        } else {
            PutSecret::Replaced
        })
    }

    async fn read_once(
        &self,
        tenant_id: &TenantId,
        secret: &SecretId,
        reader: &SecretReader,
        now: Timestamp,
    ) -> Result<Option<Secret>, Fail> {
        let mut tx = self.pool.begin().await?;
        // The row's lock is held until the read is counted: a deletion waits for it. The last
        // read stays the latest one when reads commit out of order.
        let sealed: Option<Vec<u8>> = sqlx::query_scalar(
            "UPDATE secrets SET \
                 last_read_run = CASE WHEN last_read_at IS NULL OR last_read_at <= $4 \
                     THEN $5 ELSE last_read_run END, \
                 last_read_namespace = CASE WHEN last_read_at IS NULL OR last_read_at <= $4 \
                     THEN $6 ELSE last_read_namespace END, \
                 last_read_at = GREATEST(last_read_at, $4) \
             WHERE tenant = $1 AND namespace = $2 AND name = $3 RETURNING ciphertext",
        )
        .bind(tenant(tenant_id))
        .bind(secret.namespace.as_str())
        .bind(secret.name.as_str())
        .bind(ts(now))
        .bind(reader.run().map(|(run, _)| uuid(run)))
        .bind(reader.run().map(|(_, namespace)| namespace.as_str()))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(sealed) = sealed else {
            return Ok(None);
        };
        sqlx::query(
            "INSERT INTO secret_reads (tenant, namespace, name, day, reads) \
             VALUES ($1, $2, $3, $4, 1) \
             ON CONFLICT (tenant, namespace, name, day) DO UPDATE SET \
                 reads = secret_reads.reads + 1",
        )
        .bind(tenant(tenant_id))
        .bind(secret.namespace.as_str())
        .bind(secret.name.as_str())
        .bind(day(Day::of(now)))
        .execute(&mut *tx)
        .await?;
        let key = existing_data_key(&mut tx, &self.master_keys, tenant_id).await?;
        let value = open(
            &key,
            &secret_aad(tenant_id, secret),
            &sealed,
            &format!("the secret {secret}"),
        )?;
        let value = String::from_utf8(value)
            .map_err(|_| SecretError::Crypto(format!("the secret {secret} is not UTF-8")))?;
        tx.commit().await?;
        Ok(Some(Secret::new(value)))
    }

    async fn delete_once(
        &self,
        tenant_id: &TenantId,
        namespace: &str,
        name: Option<&str>,
    ) -> Result<u64, Fail> {
        let mut tx = self.pool.begin().await?;
        let deleted = sqlx::query(
            "DELETE FROM secrets WHERE tenant = $1 AND namespace = $2 \
             AND ($3::text IS NULL OR name = $3)",
        )
        .bind(tenant(tenant_id))
        .bind(namespace)
        .bind(name)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM secret_reads WHERE tenant = $1 AND namespace = $2 \
             AND ($3::text IS NULL OR name = $3)",
        )
        .bind(tenant(tenant_id))
        .bind(namespace)
        .bind(name)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(deleted.rows_affected())
    }

    async fn signing_key_once(
        &self,
        tenant_id: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, Fail> {
        let mut conn = self.pool.acquire().await?;
        if let Some((public, sealed)) = current_key(&mut conn, tenant_id).await? {
            return open_signing_key(&mut conn, &self.master_keys, tenant_id, public, &sealed)
                .await;
        }
        drop(conn);
        let mut tx = self.pool.begin().await?;
        lock_signing_keys(&mut tx, tenant_id).await?;
        // Another call may have made it while this one waited for the lock.
        let key = match current_key(&mut tx, tenant_id).await? {
            Some((public, sealed)) => {
                open_signing_key(&mut tx, &self.master_keys, tenant_id, public, &sealed).await?
            }
            None => make_signing_key(&mut tx, &self.master_keys, tenant_id, now).await?,
        };
        tx.commit().await?;
        Ok(key)
    }

    async fn rotate_signing_key_once(
        &self,
        tenant_id: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, Fail> {
        let mut tx = self.pool.begin().await?;
        lock_signing_keys(&mut tx, tenant_id).await?;
        // The keys no longer published can no longer be checked against: they go.
        sqlx::query("DELETE FROM signing_keys WHERE tenant = $1 AND retired_at <= $2")
            .bind(tenant(tenant_id))
            .bind(published_since(now))
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE signing_keys SET retired_at = $2 WHERE tenant = $1 AND retired_at IS NULL",
        )
        .bind(tenant(tenant_id))
        .bind(ts(now))
        .execute(&mut *tx)
        .await?;
        let key = make_signing_key(&mut tx, &self.master_keys, tenant_id, now).await?;
        tx.commit().await?;
        Ok(key)
    }

    /// Encrypts one tenant's data key with the current master key, unless another server did
    /// it first. Returns whether it changed it.
    async fn rewrap_once(&self, tenant_id: &str) -> Result<bool, Fail> {
        let mut tx = self.pool.begin().await?;
        let stored: Option<(Vec<u8>, String)> = sqlx::query_as(
            "SELECT wrapped_key, master_key FROM data_keys WHERE tenant = $1 FOR UPDATE",
        )
        .bind(tenant_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((wrapped, master_key)) = stored else {
            return Ok(false);
        };
        let current = self.master_keys.current_id();
        if master_key == current.as_str() {
            return Ok(false);
        }
        let key = open_data_key(&self.master_keys, tenant_id, &wrapped, &master_key)?;
        let rewrapped = seal(
            &self.master_keys.current().0,
            &data_key_aad(tenant_id),
            &key,
        )?;
        sqlx::query("UPDATE data_keys SET wrapped_key = $2, master_key = $3 WHERE tenant = $1")
            .bind(tenant_id)
            .bind(rewrapped)
            .bind(current.as_str())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }
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

impl SecretStore for PostgresStore {
    async fn put(
        &self,
        tenant_id: &TenantId,
        secret: &NewSecret,
        now: Timestamp,
    ) -> Result<PutSecret, SecretError> {
        check_labels(secret)?;
        Ok(retrying(|| self.put_once(tenant_id, secret, now)).await?)
    }

    async fn list(
        &self,
        tenant_id: &TenantId,
        namespace: &SecretNamespace,
        labels: &BTreeMap<String, String>,
        since: Day,
    ) -> Result<Vec<SecretMeta>, SecretError> {
        let rows = sqlx::query(metas_from!("s.labels @> $4::jsonb"))
            .bind(tenant(tenant_id))
            .bind(namespace.as_str())
            .bind(day(since))
            .bind(encode_labels(labels))
            .fetch_all(&self.pool)
            .await
            .map_err(Error::from)?;
        Ok(metas_from_rows(&rows)?)
    }

    async fn meta(
        &self,
        tenant_id: &TenantId,
        secret: &SecretId,
        since: Day,
    ) -> Result<Option<SecretMeta>, SecretError> {
        let rows = sqlx::query(metas_from!("s.name = $4"))
            .bind(tenant(tenant_id))
            .bind(secret.namespace.as_str())
            .bind(day(since))
            .bind(secret.name.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(Error::from)?;
        Ok(metas_from_rows(&rows)?.into_iter().next())
    }

    async fn read(
        &self,
        tenant_id: &TenantId,
        secret: &SecretId,
        reader: &SecretReader,
        now: Timestamp,
    ) -> Result<Option<Secret>, SecretError> {
        Ok(retrying(|| self.read_once(tenant_id, secret, reader, now)).await?)
    }

    async fn delete(&self, tenant_id: &TenantId, secret: &SecretId) -> Result<bool, SecretError> {
        let (namespace, name) = (secret.namespace.as_str(), Some(secret.name.as_str()));
        let deleted = retrying(|| self.delete_once(tenant_id, namespace, name)).await?;
        Ok(deleted > 0)
    }

    async fn delete_namespace(
        &self,
        tenant_id: &TenantId,
        namespace: &NamespaceName,
    ) -> Result<usize, SecretError> {
        let deleted = retrying(|| self.delete_once(tenant_id, namespace.as_str(), None)).await?;
        Ok(usize::try_from(deleted).unwrap_or(usize::MAX))
    }

    async fn signing_key(
        &self,
        tenant_id: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, SecretError> {
        Ok(retrying(|| self.signing_key_once(tenant_id, now)).await?)
    }

    async fn rotate_signing_key(
        &self,
        tenant_id: &TenantId,
        now: Timestamp,
    ) -> Result<SigningKey, SecretError> {
        Ok(retrying(|| self.rotate_signing_key_once(tenant_id, now)).await?)
    }

    async fn public_keys(
        &self,
        tenant_id: &TenantId,
        now: Timestamp,
    ) -> Result<Vec<PublicKey>, SecretError> {
        // The current key first, then the retired ones still published, newest first.
        let rows = sqlx::query(concat!(
            "SELECT ",
            key_columns!(),
            " FROM signing_keys WHERE tenant = $1 AND (retired_at IS NULL OR retired_at > $2) \
             ORDER BY retired_at IS NOT NULL, created_at DESC, retired_at DESC, kid"
        ))
        .bind(tenant(tenant_id))
        .bind(published_since(now))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(public_key_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn master_keys_in_use(&self) -> Result<Vec<MasterKeyId>, SecretError> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT master_key FROM data_keys ORDER BY master_key")
                .fetch_all(&self.pool)
                .await
                .map_err(Error::from)?;
        ids.into_iter()
            .map(|id| {
                MasterKeyId::parse(&id).ok_or_else(|| {
                    SecretError::Crypto(format!("a data key names no master key: {id:?}"))
                })
            })
            .collect()
    }

    async fn rewrap_data_keys(&self) -> Result<usize, SecretError> {
        let tenants: Vec<String> = sqlx::query_scalar(
            "SELECT tenant FROM data_keys WHERE master_key <> $1 ORDER BY tenant",
        )
        .bind(self.master_keys.current_id().as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        let mut changed = 0;
        for tenant_id in &tenants {
            if retrying(|| self.rewrap_once(tenant_id)).await? {
                changed += 1;
            }
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
