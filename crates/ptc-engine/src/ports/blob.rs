use std::future::Future;

use ptc_core::Digest;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    #[error("payload {0} is missing from the blob store")]
    Missing(Digest),
    #[error("payload {digest} is corrupt: {message}")]
    Corrupt { digest: Digest, message: String },
    #[error("blob store backend error: {0}")]
    Backend(String),
}

/// Payload values, by digest (`archi/06-storage-and-scale.md`, "Payloads").
///
/// The bytes are the canonical JSON (RFC 8785) of the value, so a blob always hashes to its
/// digest. Blobs are immutable and deduplicated: putting the same digest twice is a no-op.
/// Where they live (files, S3, memory) and under which key is the adapter's business.
pub trait BlobStore: Send + Sync {
    fn put(
        &self,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> impl Future<Output = Result<(), BlobError>> + Send;

    fn get(
        &self,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, BlobError>> + Send;
}
