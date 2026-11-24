//! Named secrets and the tenants' signing keys (`archi/04-tools.md`, "Secrets" and "Signed
//! calls"; `archi/07-code-architecture.md`, "The other ports").
//!
//! ```text
//! tenant ─┬─ secrets of the organization   (namespace `*`)  name → value, labels, writes, reads
//!         ├─ secrets of each namespace      (namespace n)    name → value, writes, reads
//!         ├─ signing keys                   kid → Ed25519 key; the current one signs, the
//!         │                                 retired ones stay published 24 hours
//!         └─ data key, encrypted by a master key (id: the first 8 bytes of its SHA-256, hex)
//! ```
//!
//! A run reads a secret only through the dispatcher, and only one it attached at start
//! ([`Attachments`](rewind_core::Attachments)): the engine resolves the attachment
//! ([`SecretId::resolve`]) in the run's namespace and calls [`SecretStore::read`]. Values and
//! private keys never enter the history, the isolate, a cassette, OpenTelemetry nor an answer of
//! the API: their types redact `Debug` and implement no serialization.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::time::Duration;

use rewind_core::{Digest, RunId, SecretName, SecretRef, SecretScope, Timestamp};

use crate::ports::namespaces::{Day, NamespaceName};
use crate::ports::tenancy::TenantId;

/// A secret's value: a string of at most
/// [`MAX_SECRET_VALUE_BYTES`](rewind_core::MAX_SECRET_VALUE_BYTES). It is never printed: `Debug`
/// redacts it, and it has no serialization.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value, for the transport that uses it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Whose secrets: the organization's, kept under the namespace `*`, or one namespace's.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SecretNamespace {
    /// The tenant's own: `org/<name>` in an attachment.
    Organization,
    /// An end user's: `ns/<name>` in an attachment of a run of this namespace.
    Namespace(NamespaceName),
}

impl SecretNamespace {
    /// The namespace `secrets` rows of the organization are kept under.
    pub const ORGANIZATION: &'static str = "*";

    /// `*` for the organization, else the namespace's name: the form the store keys rows by.
    pub fn as_str(&self) -> &str {
        match self {
            SecretNamespace::Organization => Self::ORGANIZATION,
            SecretNamespace::Namespace(namespace) => namespace.as_str(),
        }
    }
}

/// A stored secret, in a tenant: its namespace and its name. What an attachment
/// ([`SecretRef`]) names once resolved in the run's namespace.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretId {
    pub namespace: SecretNamespace,
    pub name: SecretName,
}

impl SecretId {
    pub fn organization(name: SecretName) -> Self {
        Self {
            namespace: SecretNamespace::Organization,
            name,
        }
    }

    pub fn of_namespace(namespace: NamespaceName, name: SecretName) -> Self {
        Self {
            namespace: SecretNamespace::Namespace(namespace),
            name,
        }
    }

    /// The secret an attachment names for a run of `namespace`: `org/<name>` is the
    /// organization's, `ns/<name>` the run's own namespace's, never another's.
    pub fn resolve(secret: &SecretRef, namespace: &NamespaceName) -> Self {
        Self {
            namespace: match secret.scope {
                SecretScope::Organization => SecretNamespace::Organization,
                SecretScope::Namespace => SecretNamespace::Namespace(namespace.clone()),
            },
            name: secret.name.clone(),
        }
    }

    /// How an attachment names it: `org/<name>` or `ns/<name>`.
    pub fn to_ref(&self) -> SecretRef {
        match self.namespace {
            SecretNamespace::Organization => SecretRef::organization(self.name.clone()),
            SecretNamespace::Namespace(_) => SecretRef::namespace(self.name.clone()),
        }
    }
}

impl fmt::Display for SecretId {
    /// `org/zendesk`, `ns/alice/github`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.namespace {
            SecretNamespace::Organization => write!(f, "org/{}", self.name),
            SecretNamespace::Namespace(namespace) => write!(f, "ns/{namespace}/{}", self.name),
        }
    }
}

/// What the store knows of a secret, never its value (`archi/04-tools.md`, "Secrets": a secret
/// is listed with its metadata).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretMeta {
    pub id: SecretId,
    /// An organization's secret's own labels: a grant limited to labels sees, writes and
    /// attaches only the secrets carrying all of its labels. Always empty for a namespace's.
    pub labels: BTreeMap<String, String>,
    /// Who wrote it first, and when.
    pub created_by: String,
    pub created_at: Timestamp,
    /// Who wrote it last, and when: the creation until it is put again.
    pub updated_by: String,
    pub updated_at: Timestamp,
    /// Its reads per day from the `since` the listing gave, oldest first; a day without a read
    /// is left out.
    pub reads: Vec<DailyReads>,
    /// Its last read, if it was ever read.
    pub last_read: Option<LastRead>,
}

/// The reads of a secret in one day (UTC).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DailyReads {
    pub day: Day,
    pub reads: u64,
}

/// The last read of a secret: by whom, when.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastRead {
    pub by: SecretReader,
    pub at: Timestamp,
}

/// The secret to write and who writes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSecret {
    pub id: SecretId,
    pub value: Secret,
    /// An organization's secret's labels; must be empty for a namespace's
    /// ([`SecretError::Invalid`] otherwise).
    pub labels: BTreeMap<String, String>,
    /// The caller, in the form the API shows who wrote a secret: a principal (`user:<id>`,
    /// `key:<sha-256>`, `connection:<id>`) or `token:<namespace>` for an end user's token.
    pub by: String,
}

/// What `put` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutSecret {
    Created,
    /// A secret of that name existed: its value and labels are replaced, its creation, reads
    /// and last read kept.
    Replaced,
}

/// Who reads a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretReader {
    /// The run whose call the dispatcher prepares, in its namespace: an organization's secret is
    /// read by the runs of every namespace.
    Run {
        run: RunId,
        namespace: NamespaceName,
    },
    /// An action set's registration, listing the actions of an MCP server the secret is attached
    /// for (`POST /v1/action-sets`): no run exists yet.
    Registration,
}

impl SecretReader {
    /// The run that reads, and its namespace; none for a registration.
    pub fn run(&self) -> Option<(RunId, &NamespaceName)> {
        match self {
            Self::Run { run, namespace } => Some((*run, namespace)),
            Self::Registration => None,
        }
    }
}

/// The id of a master key: the first 8 bytes of its SHA-256, in lowercase hex
/// (`archi/04-tools.md`, "Secrets": the master key can be replaced).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MasterKeyId(String);

impl MasterKeyId {
    /// The id of the master key whose bytes are `key`.
    pub fn of(key: &[u8]) -> Self {
        Self(Digest::of(key).to_hex()[..16].to_owned())
    }

    /// An id as stored: 16 lowercase hex digits.
    pub fn parse(id: &str) -> Option<Self> {
        (id.len() == 16
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
        .then(|| Self(id.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MasterKeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The private part of a tenant's Ed25519 signing key: its 32-byte seed (RFC 8032). It is never
/// printed: `Debug` redacts it, and it has no serialization.
#[derive(Clone, PartialEq, Eq)]
pub struct PrivateKey([u8; 32]);

impl PrivateKey {
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(seed)
    }

    /// The seed, for the signer that uses it.
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrivateKey(<redacted>)")
    }
}

/// The public part of a signing key, as `/organizations/{organization}/jwks.json` publishes it: an
/// Ed25519 JWK (`kty` `OKP`, `crv` `Ed25519`, `alg` `EdDSA`) whose `x` is the key in base64url.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    /// The first 8 bytes of the SHA-256 of `key`, in lowercase hex.
    pub kid: String,
    /// The 32 bytes of the Ed25519 public key (RFC 8032).
    pub key: [u8; 32],
    pub created_at: Timestamp,
    /// When it stopped signing: it stays published [`RETIRED_KEY_PUBLISHED`] longer.
    pub retired_at: Option<Timestamp>,
}

impl PublicKey {
    /// The `kid` of a public key: the first 8 bytes of its SHA-256, in lowercase hex.
    pub fn kid_of(key: &[u8; 32]) -> String {
        Digest::of(key).to_hex()[..16].to_owned()
    }
}

/// The key a tenant signs its calls with: its public part, published, and its private part,
/// which leaves the store only toward the dispatcher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigningKey {
    pub public: PublicKey,
    pub private: PrivateKey,
}

/// How long a retired signing key stays published, so that the servers that cached the JWKS
/// still accept the calls it signed (`archi/04-tools.md`, "Signed calls").
pub const RETIRED_KEY_PUBLISHED: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// A value or a key could not be encrypted or decrypted: a wrong master key, a corrupt
    /// ciphertext, a row moved under another key.
    #[error("secret store encryption error: {0}")]
    Crypto(String),
    /// A data key is held under a master key the store was not given: the server refuses to
    /// start (exit 70) until the operator gives it (`--previous-master-keys`).
    #[error(
        "a data key is held under the master key {0}, which this server was not given: give it in \
         --previous-master-keys, then run rewind server --rewrap-data-keys"
    )]
    UnknownMasterKey(MasterKeyId),
    /// What the caller asked breaks the contract: labels on a namespace's secret.
    #[error("invalid secret: {0}")]
    Invalid(String),
    #[error("secret store backend error: {0}")]
    Backend(String),
}

/// Named secrets of the organization and of each namespace, their metadata and reads, and each
/// tenant's signing keys; values and private keys encrypted at rest (`archi/07-code-architecture.md`,
/// "The other ports").
///
/// The contract, checked by the conformance suite:
///
/// - Secrets are keyed by tenant, namespace (`*` for the organization's) and name. `put` creates
///   or replaces one: a replacement changes its value, labels, `updated_by` and `updated_at`, and
///   keeps its creation, reads and last read. A namespace's secret carries no label.
/// - `list` answers the secrets of one namespace, or of the organization, carrying all of
///   `labels` (none: every one), sorted by name; `meta` answers one. Both give the reads per day
///   from `since` and the last read, never a value.
/// - `read` answers the value and counts the read: one more read on `now`'s day, and the last
///   read becomes this run's, at `now`. A secret that does not exist is `None`, and nothing is
///   counted. Only the dispatcher's path calls it.
/// - `delete` removes a secret with its reads; `delete_namespace` every secret of a namespace
///   with their reads, for an erased end user. The organization's are not touched.
/// - `signing_key` answers the tenant's current signing key, made at the first call; it is the
///   same key until `rotate_signing_key`, which retires it at `now` and makes a new one.
///   `public_keys` answers the current key and those retired less than
///   [`RETIRED_KEY_PUBLISHED`] before `now`, newest first. A tenant has a key only once
///   `signing_key` or `rotate_signing_key` was called for it.
/// - Every method is scoped by the tenant: one tenant's secrets and keys are never visible to
///   another.
///
/// Envelope encryption (the stores; the memory adapter keeps nothing at rest): each tenant has a
/// data key, itself encrypted by a master key from the deployment's configuration, and records
/// that key's [`MasterKeyId`]. A value is encrypted bound to its tenant, namespace and name, a
/// private key to its tenant and `kid`: a row copied elsewhere does not decrypt. The adapter
/// holds the current master key and the previous ones, which only decrypt;
/// `master_keys_in_use` answers the ids its data keys are held under, and `rewrap_data_keys`
/// encrypts those held under a previous key with the current one, one tenant at a time, and
/// answers how many it changed. Values and private keys are not encrypted again.
pub trait SecretStore: Send + Sync {
    fn put(
        &self,
        tenant: &TenantId,
        secret: &NewSecret,
        now: Timestamp,
    ) -> impl Future<Output = Result<PutSecret, SecretError>> + Send;

    fn list(
        &self,
        tenant: &TenantId,
        namespace: &SecretNamespace,
        labels: &BTreeMap<String, String>,
        since: Day,
    ) -> impl Future<Output = Result<Vec<SecretMeta>, SecretError>> + Send;

    fn meta(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
        since: Day,
    ) -> impl Future<Output = Result<Option<SecretMeta>, SecretError>> + Send;

    fn read(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
        reader: &SecretReader,
        now: Timestamp,
    ) -> impl Future<Output = Result<Option<Secret>, SecretError>> + Send;

    /// Returns whether the secret existed.
    fn delete(
        &self,
        tenant: &TenantId,
        secret: &SecretId,
    ) -> impl Future<Output = Result<bool, SecretError>> + Send;

    /// Removes every secret of the namespace with their reads: an end user erased. Returns how
    /// many.
    fn delete_namespace(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
    ) -> impl Future<Output = Result<usize, SecretError>> + Send;

    fn signing_key(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> impl Future<Output = Result<SigningKey, SecretError>> + Send;

    /// Retires the current key, if any, at `now`, and makes a new one, which it answers.
    fn rotate_signing_key(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> impl Future<Output = Result<SigningKey, SecretError>> + Send;

    fn public_keys(
        &self,
        tenant: &TenantId,
        now: Timestamp,
    ) -> impl Future<Output = Result<Vec<PublicKey>, SecretError>> + Send;

    /// The ids of the master keys the data keys are held under, sorted.
    fn master_keys_in_use(
        &self,
    ) -> impl Future<Output = Result<Vec<MasterKeyId>, SecretError>> + Send;

    /// Encrypts with the current master key every data key held under a previous one. Returns
    /// how many it changed.
    fn rewrap_data_keys(&self) -> impl Future<Output = Result<usize, SecretError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_and_private_keys_are_never_printed() {
        let secret = Secret::new("ghp_123");
        let printed = format!(
            "{:?}",
            NewSecret {
                id: SecretId::organization(SecretName::new("github").unwrap()),
                value: secret,
                labels: BTreeMap::new(),
                by: "user:u".into(),
            }
        );
        assert!(!printed.contains("ghp_123"), "{printed}");
        assert!(printed.contains("Secret(<redacted>)"));
        let key = PrivateKey::from_seed([7; 32]);
        assert_eq!(format!("{key:?}"), "PrivateKey(<redacted>)");
    }

    #[test]
    fn an_attachment_resolves_in_the_run_s_namespace() {
        let alice = NamespaceName::new("alice").unwrap();
        let github = SecretRef::parse("ns/github").unwrap();
        let id = SecretId::resolve(&github, &alice);
        assert_eq!(id.namespace, SecretNamespace::Namespace(alice.clone()));
        assert_eq!(id.to_string(), "ns/alice/github");
        assert_eq!(id.to_ref(), github);
        let zendesk = SecretRef::parse("org/zendesk").unwrap();
        let id = SecretId::resolve(&zendesk, &alice);
        assert_eq!(id.namespace, SecretNamespace::Organization);
        assert_eq!(id.namespace.as_str(), "*");
        assert_eq!(id.to_string(), "org/zendesk");
    }

    #[test]
    fn key_ids_are_the_start_of_a_sha_256() {
        let id = MasterKeyId::of(&[0; 32]);
        assert_eq!(id.as_str().len(), 16);
        assert_eq!(MasterKeyId::parse(id.as_str()), Some(id.clone()));
        assert_eq!(MasterKeyId::parse("not hex at all!!"), None);
        assert_eq!(
            id.as_str(),
            &Digest::of(&[0; 32]).to_hex()[..16],
            "the first 8 bytes"
        );
        assert_eq!(PublicKey::kid_of(&[0; 32]), id.as_str());
    }
}
