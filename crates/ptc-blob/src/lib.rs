//! `BlobStore` adapters of ai-ptc (`archi/06-storage-and-scale.md`, "Payloads").
//!
//! The `BlobStore` port holds immutable bytes by tenant and by their SHA-256: payload values
//! above the table tier, and the journal documents of archived runs. This crate says where they
//! live:
//!
//! - [`FileBlobs`]: local files, for the embedded server (`.ptc/blobs/`). Writes are atomic: a
//!   staging file, synced, then renamed. A tenant's directory escapes its capital letters, so
//!   that tenants stay apart on a file system that ignores case.
//! - [`S3Blobs`]: S3 or any S3-compatible service, for production. Keys are
//!   `{tenant}/sha256/{digest}`.
//! - [`CachedBlobs`]: the workers' local cache, in memory and bounded in bytes, in front of
//!   either store.
//!
//! Every store checks the digest of the bytes it is given and of the bytes it reads, and passes
//! the conformance suite of the port. `delete` serves the cleanup of unreferenced payloads and
//! expired archives.

#![forbid(unsafe_code)]

mod cache;
mod files;
mod s3;

use std::io;
use std::path::PathBuf;

pub use cache::{CachedBlobs, DEFAULT_CACHE_CAPACITY};
pub use files::FileBlobs;
pub use s3::{S3Blobs, S3Config};

use ptc_core::Digest;
use ptc_engine::ports::BlobError;

/// A blob store could not be opened. Once open, a store reports the port's [`BlobError`].
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cannot open the blob directory {path}: {source}")]
    Directory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid S3 configuration: {0}")]
    S3Config(String),
}

/// Refuses bytes that do not hash to their digest.
fn verify(digest: Digest, bytes: &[u8], what: &str) -> Result<(), BlobError> {
    if Digest::of(bytes) == digest {
        Ok(())
    } else {
        Err(BlobError::Corrupt {
            digest,
            message: format!("the {what} do not hash to the digest"),
        })
    }
}
