use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;

use rewind_core::{Digest, RunId};

use crate::ports::tenancy::TenantId;

/// A secret value. It is never printed: `Debug` redacts it.
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

/// The credentials of a toolset or of a run: what the dispatcher injects into the transport of
/// each MCP server (`archi/04-tools.md`, "Credentials").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credentials {
    /// By server name.
    pub servers: BTreeMap<String, ServerCredentials>,
}

/// The credentials of one MCP server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServerCredentials {
    /// HTTP headers of a streamable HTTP transport, such as `Authorization`.
    pub headers: BTreeMap<String, Secret>,
    /// Environment variables of a stdio transport.
    pub env: BTreeMap<String, Secret>,
}

/// Whose credentials: a toolset's, or a run's, which override them.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SecretOwner {
    Toolset(Digest),
    Run(RunId),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// A secret could not be encrypted or decrypted: a wrong master key, a corrupt ciphertext.
    #[error("secret store encryption error: {0}")]
    Crypto(String),
    #[error("secret store backend error: {0}")]
    Backend(String),
}

/// Credentials per toolset and per run, encrypted at rest (`archi/04-tools.md`, "Credentials";
/// `archi/07-code-architecture.md`, "The other ports").
///
/// The contract, checked by the conformance suite:
///
/// - `put` stores or replaces the credentials of an owner; `delete` removes them.
/// - `resolve` returns the run's credentials if it has some, else the toolset's, whole: the
///   two are never merged.
/// - Every method is scoped by the tenant: the credentials of one tenant are never visible to
///   another.
///
/// Envelope encryption: each tenant has a data key, itself encrypted by a master key from the
/// deployment's configuration. Secrets leave the store only toward the dispatcher; they never
/// enter the journal, the isolate, a cassette or OpenTelemetry.
pub trait SecretStore: Send + Sync {
    fn put(
        &self,
        tenant: &TenantId,
        owner: &SecretOwner,
        credentials: &Credentials,
    ) -> impl Future<Output = Result<(), SecretError>> + Send;

    fn resolve(
        &self,
        tenant: &TenantId,
        run: RunId,
        toolset: Digest,
    ) -> impl Future<Output = Result<Option<Credentials>, SecretError>> + Send;

    /// Returns whether the owner had credentials.
    fn delete(
        &self,
        tenant: &TenantId,
        owner: &SecretOwner,
    ) -> impl Future<Output = Result<bool, SecretError>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_never_printed() {
        let credentials = Credentials {
            servers: BTreeMap::from([(
                "github".to_owned(),
                ServerCredentials {
                    headers: BTreeMap::from([(
                        "Authorization".to_owned(),
                        Secret::new("Bearer ghp_123"),
                    )]),
                    env: BTreeMap::new(),
                },
            )]),
        };
        let printed = format!("{credentials:?}");
        assert!(!printed.contains("ghp_123"), "{printed}");
        assert!(printed.contains("Secret(<redacted>)"));
    }
}
