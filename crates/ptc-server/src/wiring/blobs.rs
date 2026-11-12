//! The blob store: large payloads and archived journals (`archi/06-storage-and-scale.md`,
//! "Payloads"):
//!
//! | `--blobs` | Backend |
//! | --- | --- |
//! | a directory | `ptc-blob`'s files (the embedded server: `.ptc/blobs`) |
//! | `s3://<bucket>` | `ptc-blob`'s S3, behind the workers' cache |

use std::path::PathBuf;

use ptc_blob::{CachedBlobs, DEFAULT_CACHE_CAPACITY, FileBlobs, S3Blobs, S3Config};
use ptc_core::Digest;
use ptc_engine::ports::{BlobError, BlobStore, TenantId};

use crate::config::ConfigError;

/// Where blobs live.
#[derive(Debug, Clone)]
pub(crate) enum BlobLocation {
    Files(PathBuf),
    S3(S3Config),
}

impl BlobLocation {
    /// Reads `--blobs`: `s3://<bucket>`, or a directory. The S3 endpoint, region and keys come
    /// from `--s3-*` or the standard AWS environment.
    pub(crate) fn parse(text: &str, s3: S3Options) -> Result<Self, ConfigError> {
        match text.strip_prefix("s3://") {
            Some(bucket) => {
                let bucket = bucket.trim_end_matches('/');
                if bucket.is_empty() || bucket.contains('/') {
                    return Err(ConfigError::new(format!(
                        "--blobs {text:?}: expected s3://<bucket>"
                    )));
                }
                let mut config = S3Config::new(bucket);
                config.endpoint = s3.endpoint;
                config.region = s3.region;
                config.allow_http = s3.allow_http;
                config.access_key_id = s3.access_key_id;
                config.secret_access_key = s3.secret_access_key;
                Ok(Self::S3(config))
            }
            None => Ok(Self::Files(PathBuf::from(text))),
        }
    }
}

/// What `--s3-*` give.
#[derive(Debug, Clone, Default)]
pub(crate) struct S3Options {
    pub(crate) endpoint: Option<String>,
    pub(crate) region: Option<String>,
    pub(crate) allow_http: bool,
    pub(crate) access_key_id: Option<String>,
    pub(crate) secret_access_key: Option<String>,
}

/// The blob store of this server.
#[derive(Debug, Clone)]
pub(crate) enum Blobs {
    Files(FileBlobs),
    S3(std::sync::Arc<CachedBlobs<S3Blobs>>),
}

impl Blobs {
    pub(crate) fn open(location: &BlobLocation) -> Result<Self, ConfigError> {
        match location {
            BlobLocation::Files(root) => {
                FileBlobs::open(root.clone())
                    .map(Blobs::Files)
                    .map_err(|error| {
                        ConfigError::new(format!("cannot open {}: {error}", root.display()))
                    })
            }
            BlobLocation::S3(config) => S3Blobs::new(config.clone())
                .map(|s3| {
                    Blobs::S3(std::sync::Arc::new(CachedBlobs::new(
                        s3,
                        DEFAULT_CACHE_CAPACITY,
                    )))
                })
                .map_err(|error| ConfigError::new(format!("S3 blob store: {error}"))),
        }
    }
}

impl BlobStore for Blobs {
    async fn put(
        &self,
        tenant: &TenantId,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        match self {
            Blobs::Files(files) => files.put(tenant, digest, bytes).await,
            Blobs::S3(s3) => s3.put(tenant, digest, bytes).await,
        }
    }

    async fn get(&self, tenant: &TenantId, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        match self {
            Blobs::Files(files) => files.get(tenant, digest).await,
            Blobs::S3(s3) => s3.get(tenant, digest).await,
        }
    }

    async fn delete(&self, tenant: &TenantId, digest: Digest) -> Result<bool, BlobError> {
        match self {
            Blobs::Files(files) => files.delete(tenant, digest).await,
            Blobs::S3(s3) => s3.delete(tenant, digest).await,
        }
    }
}
