//! The tenant-keyed blobs of the runs `rewind server migrate-runs` moves
//! (`archi/08-decisions.md`, "Taken while writing the object-storage spec": the migration): the
//! `LegacyBlobs` of the engine.
//!
//! The set-aside runs keep their payload values above 64 KiB, and the journal documents their
//! retention archived, as blobs of their tenant, keyed by the SHA-256 of their bytes:
//! `{tenant}/sha256/{ab}/{hex}` under a directory (the embedded server's `.rewind/blobs`, a
//! server's `--blobs <dir>`), the tenant's capital letters escaped as [`crate::FileBlobs`]
//! escapes them, or `{tenant}/sha256/{hex}` in a bucket. [`LegacyFiles`] and [`LegacyS3`] read
//! them, check their digests and decode the journals; they write nothing.

use std::path::PathBuf;

use object_store::path::Path;
use rewind_core::{Digest, HistoryEntry};
use rewind_engine::ports::legacy::{LegacyBlobs, LegacyError};
use rewind_engine::ports::{BlobError, TenantId};

use crate::files::{blocking, read, tenant_directory};
use crate::{Error, S3Blobs, S3Config};

fn unavailable(error: BlobError) -> LegacyError {
    match error {
        BlobError::Corrupt { digest, message } => {
            LegacyError::Corrupt(format!("blob {digest}: {message}"))
        }
        other => LegacyError::Unavailable(other.to_string()),
    }
}

/// The entries of an archived journal document.
fn entries(digest: Digest, bytes: &[u8]) -> Result<Vec<HistoryEntry>, LegacyError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| LegacyError::Corrupt(format!("the journal {digest} is not UTF-8")))?;
    rewind_format::decode_journal(text)
        .map(|document| document.entries)
        .map_err(|error| LegacyError::Corrupt(format!("the journal {digest}: {error}")))
}

/// The tenant-keyed blobs under a directory.
#[derive(Debug, Clone)]
pub struct LegacyFiles {
    root: PathBuf,
}

impl LegacyFiles {
    /// The blobs under `root`: `.rewind/blobs`, or a server's `--blobs` directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `{root}/{tenant}/sha256/{ab}/{hex}`.
    pub fn path(&self, tenant: &TenantId, digest: Digest) -> PathBuf {
        let hex = digest.to_hex();
        self.root
            .join(tenant_directory(tenant))
            .join("sha256")
            .join(&hex[..2])
            .join(hex)
    }

    /// Removes every tenant-keyed blob under the root (each `{tenant}/sha256/`), once the runs
    /// that referenced them moved. The blobs of runs under the same root stay.
    pub fn remove(&self) -> Result<(), Error> {
        let failed = |source| Error::Directory {
            path: self.root.clone(),
            source,
        };
        let tenants = match std::fs::read_dir(&self.root) {
            Ok(tenants) => tenants,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(failed(error)),
        };
        for tenant in tenants {
            let blobs = tenant.map_err(failed)?.path().join("sha256");
            if blobs.is_dir() {
                std::fs::remove_dir_all(&blobs).map_err(failed)?;
            }
        }
        Ok(())
    }

    async fn bytes(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<Vec<u8>>, LegacyError> {
        let path = self.path(tenant, digest);
        blocking(move || read(digest, &path))
            .await
            .map_err(unavailable)
    }
}

impl LegacyBlobs for LegacyFiles {
    async fn payload(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<Vec<u8>>, LegacyError> {
        self.bytes(tenant, digest).await
    }

    async fn journal(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<Vec<HistoryEntry>>, LegacyError> {
        match self.bytes(tenant, digest).await? {
            Some(bytes) => entries(digest, &bytes).map(Some),
            None => Ok(None),
        }
    }
}

/// The tenant-keyed blobs of a bucket.
#[derive(Debug, Clone)]
pub struct LegacyS3 {
    bucket: S3Blobs,
}

impl LegacyS3 {
    pub fn new(config: S3Config) -> Result<Self, Error> {
        Ok(Self {
            bucket: S3Blobs::new(config)?,
        })
    }

    /// `{tenant}/sha256/{hex}`.
    pub fn key(tenant: &TenantId, digest: Digest) -> String {
        format!("{tenant}/sha256/{}", digest.to_hex())
    }

    async fn bytes(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<Vec<u8>>, LegacyError> {
        // Tenant ids and hex digests hold no character a path would encode.
        let location = Path::from(Self::key(tenant, digest));
        self.bucket
            .read(&location, digest)
            .await
            .map_err(unavailable)
    }
}

impl LegacyBlobs for LegacyS3 {
    async fn payload(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<Vec<u8>>, LegacyError> {
        self.bytes(tenant, digest).await
    }

    async fn journal(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<Vec<HistoryEntry>>, LegacyError> {
        match self.bytes(tenant, digest).await? {
            Some(bytes) => entries(digest, &bytes).map(Some),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_tenants_blob_is_read_under_its_escaped_directory_then_removed() {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let tenant = TenantId::new("Acme").expect("tenant");
        let bytes = br#"{"text":"b"}"#.to_vec();
        let digest = Digest::of(&bytes);
        let legacy = LegacyFiles::new(dir.path());
        let path = legacy.path(&tenant, digest);
        assert!(path.starts_with(dir.path().join("+acme").join("sha256")));
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("mkdir");
        std::fs::write(&path, &bytes).expect("write");
        let run = dir.path().join("blobs");
        std::fs::create_dir_all(&run).expect("mkdir");
        assert_eq!(legacy.payload(&tenant, digest).await, Ok(Some(bytes)));
        assert_eq!(
            legacy.payload(&tenant, Digest::of(b"other")).await,
            Ok(None)
        );
        assert!(matches!(
            legacy.journal(&tenant, digest).await,
            Err(LegacyError::Corrupt(_))
        ));
        legacy.remove().expect("remove");
        assert!(!dir.path().join("+acme").join("sha256").exists());
        assert!(run.exists(), "what is not a tenant's blob stays");
    }
}
