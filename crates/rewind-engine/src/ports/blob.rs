use std::future::Future;

use rewind_core::Digest;

use crate::ports::tenancy::TenantId;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    #[error("blob {0} is missing from the blob store")]
    Missing(Digest),
    /// The bytes do not hash to their digest: refused by `put`, detected by `get`.
    #[error("blob {digest} is corrupt: {message}")]
    Corrupt { digest: Digest, message: String },
    #[error("blob store backend error: {0}")]
    Backend(String),
}

/// Immutable bytes, by tenant and by the SHA-256 of the bytes (`archi/06-storage-and-scale.md`,
/// "Payloads"; "Retention and deletion").
///
/// It holds the payload values above the table tier, as their canonical JSON (RFC 8785), so a
/// payload's blob is keyed by the payload's digest; and the journal documents of archived runs.
/// Where blobs live (`{tenant}/sha256/{digest}` on S3, files, memory) is the adapter's
/// business.
///
/// The contract, checked by the conformance suite:
///
/// - `put` refuses bytes that do not hash to `digest`. Blobs are immutable: a second `put` of a
///   digest changes nothing.
/// - `get` checks the digest of what it reads, and reports a mismatch as `Corrupt`.
/// - `delete` removes a blob, for the cleanup of unreferenced payloads and expired archives;
///   deleting a missing blob is not an error.
/// - Blobs of one tenant are never visible to another.
pub trait BlobStore: Send + Sync {
    fn put(
        &self,
        tenant: &TenantId,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> impl Future<Output = Result<(), BlobError>> + Send;

    fn get(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, BlobError>> + Send;

    /// Returns whether the blob existed.
    fn delete(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> impl Future<Output = Result<bool, BlobError>> + Send;
}
