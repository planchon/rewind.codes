use std::fmt;
use std::future::Future;

use rewind_core::Digest;

use crate::ports::tenancy::{RunRef, TenantId};

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

/// What a blob belongs to (`archi/06-storage-and-scale.md`, "The archive and the blobs"): a run,
/// for its payload values above 64 KiB, or a tenant's tests, for their documents.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BlobOwner {
    /// `blobs/{tenant}/{run}/sha256/{digest}`.
    Run(RunRef),
    /// `tests/{tenant}/sha256/{digest}`.
    Tests(TenantId),
}

impl BlobOwner {
    pub fn tenant(&self) -> &TenantId {
        match self {
            BlobOwner::Run(run) => &run.tenant,
            BlobOwner::Tests(tenant) => tenant,
        }
    }
}

impl fmt::Display for BlobOwner {
    /// `run {tenant}/{run}` or `tests of {tenant}`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlobOwner::Run(run) => write!(f, "run {run}"),
            BlobOwner::Tests(tenant) => write!(f, "tests of {tenant}"),
        }
    }
}

/// Immutable bytes keyed by the SHA-256 of the bytes (`archi/06-storage-and-scale.md`,
/// "Payloads"; "The archive and the blobs"; "Retention and deletion").
///
/// A blob belongs to a [`BlobOwner`]: a run, whose payload values above 64 KiB are blobs of
/// their canonical JSON (RFC 8785), so a payload's blob is keyed by the payload's digest; or a
/// tenant's tests, whose documents are blobs. A payload belongs to its run: the same value
/// carried by two runs is stored twice, and a fork copies the blobs it references under its own
/// run. Blobs keyed by a tenant alone serve the payload values and archived journals of the
/// journal store. Where blobs live (S3 keys, files, memory) is the adapter's business.
///
/// The contract, checked by the conformance suite:
///
/// - `put` and `put_in` refuse bytes that do not hash to `digest`. Blobs are immutable: a second
///   put of a digest changes nothing.
/// - `get` and `get_in` check the digest of what they read, and report a mismatch as `Corrupt`.
/// - `copy` puts a blob of one owner under another, without the bytes leaving the backend when
///   it can (a fork). A missing source is [`BlobError::Missing`]; a target already there is
///   kept.
/// - `delete_run` deletes every blob of a run (its erasure); `delete` and `delete_in` remove one
///   blob (the cleanup of tests, of unreferenced payloads, of expired archives). Deleting a
///   missing blob is not an error.
/// - Blobs of one tenant are never visible to another, nor blobs of one owner to another.
pub trait BlobStore: Send + Sync {
    /// Stores a tenant's blob.
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

    /// Stores a blob of `owner`.
    fn put_in(
        &self,
        owner: &BlobOwner,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> impl Future<Output = Result<(), BlobError>> + Send;

    fn get_in(
        &self,
        owner: &BlobOwner,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, BlobError>> + Send;

    /// Puts the blob `digest` of `from` under `to`.
    fn copy(
        &self,
        from: &BlobOwner,
        to: &BlobOwner,
        digest: Digest,
    ) -> impl Future<Output = Result<(), BlobError>> + Send;

    /// Deletes every blob of the run.
    fn delete_run(&self, run: &RunRef) -> impl Future<Output = Result<(), BlobError>> + Send;

    /// Deletes a blob of `owner`; returns whether it existed.
    fn delete_in(
        &self,
        owner: &BlobOwner,
        digest: Digest,
    ) -> impl Future<Output = Result<bool, BlobError>> + Send;
}
