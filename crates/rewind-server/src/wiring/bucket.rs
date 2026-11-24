//! The bucket (`archi/06-storage-and-scale.md`, "Backends", "The archive and the blobs"): the
//! partitions of the run plane, the archive of sealed attempts and the blobs, under one root:
//!
//! | `--bucket` | Partitions | Archive and blobs |
//! | --- | --- | --- |
//! | a directory | SlateDB on local files under `partitions/` (`rewind-store-slatedb`) | files under `runs/`, `blobs/`, `tests/` (`rewind-blob`) |
//! | `s3://<bucket>` | SlateDB on AWS S3 or any S3-compatible service | the same bucket (`rewind-blob`'s S3), behind this process's cache |
//!
//! Every object of the archive and the blobs is immutable: each process caches what it reads
//! without asking again.

use std::path::PathBuf;

use rewind_blob::{
    CachedArchive, CachedBlobs, DEFAULT_ARCHIVE_CACHE_CAPACITY, DEFAULT_CACHE_CAPACITY,
    FileArchive, FileBlobs, LegacyFiles, LegacyS3, S3Archive, S3Blobs, S3Config,
};
use rewind_core::{Digest, HistoryEntry};
use rewind_engine::ports::legacy::{LegacyBlobs, LegacyError};
use rewind_engine::ports::partition::Attempt;
use rewind_engine::ports::{
    ArchiveError, BlobError, BlobOwner, BlobStore, RunArchive, RunRef, SealedAttempt, TenantId,
};
use rewind_store_slatedb::{Bucket, Options, S3Bucket, SlatePartitions};

use crate::config::ConfigError;

/// Where the bucket is.
#[derive(Debug, Clone)]
pub(crate) enum BucketLocation {
    Files(PathBuf),
    S3(S3Options),
}

impl BucketLocation {
    /// Reads `--bucket`: `s3://<bucket>`, or a directory. The S3 endpoint, region and keys come
    /// from `--s3-*` or the standard AWS environment.
    pub(crate) fn parse(text: &str, mut s3: S3Options) -> Result<Self, ConfigError> {
        match text.strip_prefix("s3://") {
            Some(bucket) => {
                let bucket = bucket.trim_end_matches('/');
                if bucket.is_empty() || bucket.contains('/') {
                    return Err(ConfigError::new(format!(
                        "--bucket {text:?}: expected s3://<bucket>"
                    )));
                }
                s3.bucket = bucket.to_owned();
                Ok(Self::S3(s3))
            }
            None => Ok(Self::Files(PathBuf::from(text))),
        }
    }
}

/// What `--s3-*` give, and the bucket's name.
#[derive(Debug, Clone, Default)]
pub(crate) struct S3Options {
    pub(crate) bucket: String,
    pub(crate) endpoint: Option<String>,
    pub(crate) region: Option<String>,
    pub(crate) allow_http: bool,
    pub(crate) access_key_id: Option<String>,
    pub(crate) secret_access_key: Option<String>,
}

impl S3Options {
    fn blobs(&self) -> S3Config {
        let mut config = S3Config::new(self.bucket.clone());
        config.endpoint.clone_from(&self.endpoint);
        config.region.clone_from(&self.region);
        config.allow_http = self.allow_http;
        config.access_key_id.clone_from(&self.access_key_id);
        config.secret_access_key.clone_from(&self.secret_access_key);
        config
    }

    fn partitions(&self) -> S3Bucket {
        S3Bucket {
            bucket: self.bucket.clone(),
            region: self.region.clone(),
            endpoint: self.endpoint.clone(),
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            allow_http: self.allow_http,
            ..S3Bucket::default()
        }
    }
}

/// What the bucket holds, opened.
pub(crate) struct Opened {
    pub(crate) partitions: SlatePartitions,
    pub(crate) archive: Archive,
    pub(crate) blobs: Blobs,
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened").finish_non_exhaustive()
    }
}

/// Opens the partitions, the archive and the blobs of the bucket.
pub(crate) fn open(location: &BucketLocation) -> Result<Opened, ConfigError> {
    match location {
        BucketLocation::Files(root) => {
            let files = FileBlobs::open(root.clone()).map_err(|error| {
                ConfigError::new(format!("cannot open {}: {error}", root.display()))
            })?;
            let partitions = SlatePartitions::new(&Bucket::Local(root.clone()), Options::default())
                .map_err(|error| {
                    ConfigError::new(format!(
                        "cannot open the partitions under {}: {error}",
                        root.display()
                    ))
                })?;
            Ok(Opened {
                partitions,
                archive: CachedArchive::new(
                    Archives::Files(FileArchive::new(files.clone())),
                    DEFAULT_ARCHIVE_CACHE_CAPACITY,
                ),
                blobs: Blobs::Files(files),
            })
        }
        BucketLocation::S3(s3) => {
            let blobs = S3Blobs::new(s3.blobs())
                .map_err(|error| ConfigError::new(format!("S3 bucket: {error}")))?;
            let partitions = SlatePartitions::new(&Bucket::S3(s3.partitions()), Options::default())
                .map_err(|error| ConfigError::new(format!("S3 partitions: {error}")))?;
            Ok(Opened {
                partitions,
                archive: CachedArchive::new(
                    Archives::S3(S3Archive::new(blobs.clone())),
                    DEFAULT_ARCHIVE_CACHE_CAPACITY,
                ),
                blobs: Blobs::S3(std::sync::Arc::new(CachedBlobs::new(
                    blobs,
                    DEFAULT_CACHE_CAPACITY,
                ))),
            })
        }
    }
}

/// The archive of this server, behind its cache.
pub(crate) type Archive = CachedArchive<Archives>;

/// Where sealed attempts are written.
#[derive(Debug, Clone)]
pub(crate) enum Archives {
    Files(FileArchive),
    S3(S3Archive),
}

impl RunArchive for Archives {
    async fn put(&self, sealed: &SealedAttempt) -> Result<(), ArchiveError> {
        match self {
            Archives::Files(files) => files.put(sealed).await,
            Archives::S3(s3) => s3.put(sealed).await,
        }
    }

    async fn get(
        &self,
        run: &RunRef,
        attempt: Attempt,
    ) -> Result<Option<SealedAttempt>, ArchiveError> {
        match self {
            Archives::Files(files) => files.get(run, attempt).await,
            Archives::S3(s3) => s3.get(run, attempt).await,
        }
    }

    async fn erase(&self, run: &RunRef) -> Result<(), ArchiveError> {
        match self {
            Archives::Files(files) => files.erase(run).await,
            Archives::S3(s3) => s3.erase(run).await,
        }
    }
}

/// The blob store of this server.
#[derive(Debug, Clone)]
pub(crate) enum Blobs {
    Files(FileBlobs),
    S3(std::sync::Arc<CachedBlobs<S3Blobs>>),
}

impl BlobStore for Blobs {
    async fn put(
        &self,
        owner: &BlobOwner,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        match self {
            Blobs::Files(files) => files.put(owner, digest, bytes).await,
            Blobs::S3(s3) => s3.put(owner, digest, bytes).await,
        }
    }

    async fn get(&self, owner: &BlobOwner, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        match self {
            Blobs::Files(files) => files.get(owner, digest).await,
            Blobs::S3(s3) => s3.get(owner, digest).await,
        }
    }

    async fn copy(
        &self,
        from: &BlobOwner,
        to: &BlobOwner,
        digest: Digest,
    ) -> Result<(), BlobError> {
        match self {
            Blobs::Files(files) => files.copy(from, to, digest).await,
            Blobs::S3(s3) => s3.copy(from, to, digest).await,
        }
    }

    async fn delete_run(&self, run: &RunRef) -> Result<(), BlobError> {
        match self {
            Blobs::Files(files) => files.delete_run(run).await,
            Blobs::S3(s3) => s3.delete_run(run).await,
        }
    }

    async fn delete(&self, owner: &BlobOwner, digest: Digest) -> Result<bool, BlobError> {
        match self {
            Blobs::Files(files) => files.delete(owner, digest).await,
            Blobs::S3(s3) => s3.delete(owner, digest).await,
        }
    }
}

/// The tenant-keyed blobs the runs `migrate-runs` moves reference: under a directory (the
/// embedded server's `.rewind/blobs`, a server's `--blobs` directory) or in a bucket.
#[derive(Debug, Clone)]
pub(crate) enum LegacyBlobStore {
    Files(LegacyFiles),
    S3(LegacyS3),
}

impl LegacyBlobStore {
    pub(crate) fn open(location: &BucketLocation) -> Result<Self, ConfigError> {
        match location {
            BucketLocation::Files(root) => Ok(Self::Files(LegacyFiles::new(root.clone()))),
            BucketLocation::S3(s3) => LegacyS3::new(s3.blobs())
                .map(Self::S3)
                .map_err(|error| ConfigError::new(format!("--blobs: {error}"))),
        }
    }
}

impl LegacyBlobs for LegacyBlobStore {
    async fn payload(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<Vec<u8>>, LegacyError> {
        match self {
            Self::Files(files) => files.payload(tenant, digest).await,
            Self::S3(s3) => s3.payload(tenant, digest).await,
        }
    }

    async fn export(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<Vec<HistoryEntry>>, LegacyError> {
        match self {
            Self::Files(files) => files.export(tenant, digest).await,
            Self::S3(s3) => s3.export(tenant, digest).await,
        }
    }
}
