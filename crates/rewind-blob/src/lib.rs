//! `BlobStore` and `RunArchive` adapters of Rewind (`archi/06-storage-and-scale.md`,
//! "Payloads", "The archive and the blobs").
//!
//! The archive: [`FileArchive`] and [`S3Archive`] keep the sealed attempts of ended runs beside
//! the blobs, `runs/{tenant}/{run}/{attempt}.json`, each the attempt's journal document
//! (`rewind-format`); erasing a run deletes its attempts and its blobs. [`CachedArchive`] is the
//! local cache every process puts in front of it: a sealed attempt never changes.
//!
//! The `BlobStore` port holds immutable bytes by their SHA-256: under a run, its payload values
//! above 64 KiB; under a tenant's tests, their documents. This crate says where they live:
//!
//! - [`FileBlobs`]: local files, for the embedded server (`.rewind/`). Writes are atomic: a
//!   staging file, synced, then renamed. A tenant's directory escapes its capital letters, so
//!   that tenants stay apart on a file system that ignores case.
//! - [`S3Blobs`]: S3 or any S3-compatible service, for production. Keys are
//!   `blobs/{tenant}/{run}/sha256/{digest}` and `tests/{tenant}/sha256/{digest}`.
//! - [`CachedBlobs`]: the workers' local cache, in memory and bounded in bytes, in front of
//!   either store.
//!
//! [`LegacyFiles`] and [`LegacyS3`] read the tenant-keyed blobs of the runs `rewind server
//! migrate-runs` moves: their payload values above 64 KiB and their archived journals.
//!
//! Every store checks the digest of the bytes it is given and of the bytes it reads, and passes
//! the conformance suite of the port. `copy` serves a fork, `delete_run` the erasure of a run,
//! `delete` the cleanup of tests.

#![forbid(unsafe_code)]

mod archive;
mod cache;
mod files;
mod legacy;
mod s3;

use std::io;
use std::path::PathBuf;

pub use archive::{CachedArchive, DEFAULT_ARCHIVE_CACHE_CAPACITY, FileArchive, S3Archive};
pub use cache::{CachedBlobs, DEFAULT_CACHE_CAPACITY};
pub use files::FileBlobs;
pub use legacy::{LegacyFiles, LegacyS3};
pub use s3::{S3Blobs, S3Config};

use rewind_core::Digest;
use rewind_engine::ports::BlobError;

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
