//! `BlobStore` on S3 or any S3-compatible service, for production.

use std::fmt;

use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::{ObjectStore as _, ObjectStoreExt as _, PutMode};
use ptc_core::Digest;
use ptc_engine::ports::{BlobError, BlobStore, TenantId};

use crate::{Error, verify};

/// Where an [`S3Blobs`] store keeps its blobs.
///
/// Without an access key, what the configuration leaves out comes from the standard AWS
/// environment (`AWS_REGION`, `AWS_ENDPOINT`, `AWS_ACCESS_KEY_ID`…), and credentials fall back to
/// the container, web-identity or instance role. With an access key, nothing comes from the
/// environment: a session token left there cannot pair with the wrong key.
#[derive(Clone, Default)]
pub struct S3Config {
    pub bucket: String,
    pub region: Option<String>,
    /// The endpoint of an S3-compatible service (MinIO, R2…). Requests use path-style URLs.
    pub endpoint: Option<String>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    /// Allows an `http://` endpoint. Only for a service on a trusted network.
    pub allow_http: bool,
}

impl S3Config {
    pub fn new(bucket: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            ..Self::default()
        }
    }
}

impl fmt::Debug for S3Config {
    /// Leaves the secrets out.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let redacted = |secret: &Option<String>| secret.as_ref().map(|_| "…");
        f.debug_struct("S3Config")
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &redacted(&self.secret_access_key))
            .field("session_token", &redacted(&self.session_token))
            .field("allow_http", &self.allow_http)
            .finish()
    }
}

/// `BlobStore` on S3: a tenant's blob is the object `{tenant}/sha256/{hex digest}` of the
/// bucket.
///
/// A put is conditional (`If-None-Match: *`), so a second put of a digest never rewrites its
/// object; the service must support conditional writes, as AWS S3, MinIO and R2 do. A get checks
/// the digest of the object's bytes. S3 does not say whether a deletion removed anything, so
/// `delete` asks first: two concurrent deletions of one blob may both report it existed.
/// Requests are retried with backoff by `object_store`. The store must be used inside a Tokio
/// runtime.
#[derive(Debug, Clone)]
pub struct S3Blobs {
    store: AmazonS3,
}

impl S3Blobs {
    /// Builds the client; no request is made.
    pub fn new(config: S3Config) -> Result<Self, Error> {
        let S3Config {
            bucket,
            region,
            endpoint,
            access_key_id,
            secret_access_key,
            session_token,
            allow_http,
        } = config;
        let builder = if access_key_id.is_some() {
            AmazonS3Builder::new()
        } else {
            AmazonS3Builder::from_env()
        };
        let mut builder = builder.with_bucket_name(bucket);
        if let Some(region) = region {
            builder = builder.with_region(region);
        }
        if let Some(endpoint) = endpoint {
            builder = builder
                .with_endpoint(endpoint)
                .with_virtual_hosted_style_request(false);
        }
        if let Some(key) = access_key_id {
            builder = builder.with_access_key_id(key);
        }
        if let Some(secret) = secret_access_key {
            builder = builder.with_secret_access_key(secret);
        }
        if let Some(token) = session_token {
            builder = builder.with_token(token);
        }
        if allow_http {
            builder = builder.with_allow_http(true);
        }
        let store = builder
            .build()
            .map_err(|error| Error::S3Config(error.to_string()))?;
        Ok(Self { store })
    }

    /// The key of a tenant's blob: `{tenant}/sha256/{hex digest}`.
    pub fn key(tenant: &TenantId, digest: Digest) -> String {
        format!("{tenant}/sha256/{}", digest.to_hex())
    }

    fn location(tenant: &TenantId, digest: Digest) -> Path {
        // Tenant ids and hex digests hold no character a path would encode.
        Path::from(Self::key(tenant, digest))
    }
}

impl BlobStore for S3Blobs {
    async fn put(
        &self,
        tenant: &TenantId,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        verify(digest, &bytes, "bytes")?;
        let location = Self::location(tenant, digest);
        match self
            .store
            .put_opts(&location, bytes.into(), PutMode::Create.into())
            .await
        {
            // Blobs are immutable: a second put changes nothing.
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
            Err(error) => Err(failed("write", &location, &error)),
        }
    }

    async fn get(&self, tenant: &TenantId, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        let location = Self::location(tenant, digest);
        let object = match self.store.get(&location).await {
            Ok(object) => object,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(failed("read", &location, &error)),
        };
        let bytes = object
            .bytes()
            .await
            .map_err(|error| failed("read", &location, &error))?;
        verify(digest, &bytes, "stored bytes")?;
        Ok(Some(bytes.into()))
    }

    async fn delete(&self, tenant: &TenantId, digest: Digest) -> Result<bool, BlobError> {
        let location = Self::location(tenant, digest);
        match self.store.head(&location).await {
            Ok(_) => {}
            Err(object_store::Error::NotFound { .. }) => return Ok(false),
            Err(error) => return Err(failed("delete", &location, &error)),
        }
        self.store
            .delete(&location)
            .await
            .map_err(|error| failed("delete", &location, &error))?;
        Ok(true)
    }
}

fn failed(action: &str, location: &Path, error: &object_store::Error) -> BlobError {
    BlobError::Backend(format!("cannot {action} s3 object {location}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_start_with_the_tenant() {
        let tenant = TenantId::new("acme").expect("tenant");
        let digest = Digest::of(b"{}");
        let key = S3Blobs::key(&tenant, digest);
        assert_eq!(key, format!("acme/sha256/{}", digest.to_hex()));
        assert_eq!(S3Blobs::location(&tenant, digest).as_ref(), key);
    }

    #[test]
    fn debug_leaves_the_secrets_out() {
        let config = S3Config {
            access_key_id: Some("AKIDEXAMPLE".into()),
            secret_access_key: Some("wJalrXUtnFEMI".into()),
            session_token: Some("token-secret".into()),
            ..S3Config::new("payloads")
        };
        let debug = format!("{config:?}");
        assert!(debug.contains("AKIDEXAMPLE"), "{debug}");
        assert!(!debug.contains("wJalrXUtnFEMI"), "{debug}");
        assert!(!debug.contains("token-secret"), "{debug}");
    }
}
