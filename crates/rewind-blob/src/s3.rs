//! `BlobStore` on S3 or any S3-compatible service, for production.

use std::fmt;

use futures_util::StreamExt as _;
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path;
use object_store::{ObjectStore as _, ObjectStoreExt as _, PutMode};
use rewind_core::Digest;
use rewind_engine::ports::{BlobError, BlobOwner, BlobStore, RunRef};

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
/// bucket; a run's, `blobs/{tenant}/{run}/sha256/{hex digest}`; a tenant's test,
/// `tests/{tenant}/sha256/{hex digest}` (`archi/06-storage-and-scale.md`, "The archive and the
/// blobs").
///
/// A put is conditional (`If-None-Match: *`), so a second put of a digest never rewrites its
/// object; the service must support conditional writes, as AWS S3, MinIO and R2 do. A get checks
/// the digest of the object's bytes. A copy is the service's own copy, the bytes staying in the
/// bucket; it may rewrite a target already there with the same bytes. S3 does not say whether a
/// deletion removed anything, so `delete` asks first: two concurrent deletions of one blob may
/// both report it existed. `delete_run` lists the run's prefix and deletes what it holds.
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

    /// The bucket's client, which the archive shares.
    pub(crate) fn store(&self) -> &AmazonS3 {
        &self.store
    }

    /// The key of an owner's blob: `blobs/{tenant}/{run}/sha256/{hex digest}` or
    /// `tests/{tenant}/sha256/{hex digest}`.
    pub fn key(owner: &BlobOwner, digest: Digest) -> String {
        match owner {
            BlobOwner::Run(run) => format!("{}/sha256/{}", Self::run_prefix(run), digest.to_hex()),
            BlobOwner::Tests(tenant) => format!("tests/{tenant}/sha256/{}", digest.to_hex()),
        }
    }

    /// The prefix of a run's blobs: `blobs/{tenant}/{run}`.
    pub fn run_prefix(run: &RunRef) -> String {
        format!("blobs/{}/{}", run.tenant, run.id)
    }

    fn location(owner: &BlobOwner, digest: Digest) -> Path {
        // Tenant ids, run ids and hex digests hold no character a path would encode.
        Path::from(Self::key(owner, digest))
    }

    pub(crate) async fn read(
        &self,
        location: &Path,
        digest: Digest,
    ) -> Result<Option<Vec<u8>>, BlobError> {
        let object = match self.store.get(location).await {
            Ok(object) => object,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(failed("read", location, &error)),
        };
        let bytes = object
            .bytes()
            .await
            .map_err(|error| failed("read", location, &error))?;
        verify(digest, &bytes, "stored bytes")?;
        Ok(Some(bytes.into()))
    }

    async fn write(
        &self,
        location: &Path,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        verify(digest, &bytes, "bytes")?;
        match self
            .store
            .put_opts(location, bytes.into(), PutMode::Create.into())
            .await
        {
            // Blobs are immutable: a second put changes nothing.
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => Ok(()),
            Err(error) => Err(failed("write", location, &error)),
        }
    }

    async fn remove(&self, location: &Path) -> Result<bool, BlobError> {
        match self.store.head(location).await {
            Ok(_) => {}
            Err(object_store::Error::NotFound { .. }) => return Ok(false),
            Err(error) => return Err(failed("delete", location, &error)),
        }
        self.store
            .delete(location)
            .await
            .map_err(|error| failed("delete", location, &error))?;
        Ok(true)
    }
}

impl BlobStore for S3Blobs {
    async fn put(
        &self,
        owner: &BlobOwner,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        self.write(&Self::location(owner, digest), digest, bytes)
            .await
    }

    async fn get(&self, owner: &BlobOwner, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        self.read(&Self::location(owner, digest), digest).await
    }

    async fn copy(
        &self,
        from: &BlobOwner,
        to: &BlobOwner,
        digest: Digest,
    ) -> Result<(), BlobError> {
        let (source, target) = (Self::location(from, digest), Self::location(to, digest));
        match self.store.copy(&source, &target).await {
            Ok(()) => Ok(()),
            Err(object_store::Error::NotFound { .. }) => Err(BlobError::Missing(digest)),
            Err(error) => Err(failed("copy", &target, &error)),
        }
    }

    async fn delete_run(&self, run: &RunRef) -> Result<(), BlobError> {
        let prefix = Path::from(Self::run_prefix(run));
        let mut listed = self.store.list(Some(&prefix));
        while let Some(object) = listed.next().await {
            let object = object.map_err(|error| failed("list", &prefix, &error))?;
            match self.store.delete(&object.location).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(error) => return Err(failed("delete", &object.location, &error)),
            }
        }
        Ok(())
    }

    async fn delete(&self, owner: &BlobOwner, digest: Digest) -> Result<bool, BlobError> {
        self.remove(&Self::location(owner, digest)).await
    }
}

fn failed(action: &str, location: &Path, error: &object_store::Error) -> BlobError {
    BlobError::Backend(format!("cannot {action} s3 object {location}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rewind_engine::ports::TenantId;

    #[test]
    fn owned_keys_are_the_buckets_layout() {
        let tenant = TenantId::new("Acme").expect("tenant");
        let run = RunRef::new(
            tenant.clone(),
            "01930000-0000-7000-8000-000000000001"
                .parse()
                .expect("run id"),
        );
        let digest = Digest::of(b"{}");
        let hex = digest.to_hex();
        assert_eq!(
            S3Blobs::key(&BlobOwner::Run(run.clone()), digest),
            format!("blobs/Acme/01930000-0000-7000-8000-000000000001/sha256/{hex}")
        );
        assert_eq!(
            S3Blobs::key(&BlobOwner::Tests(tenant), digest),
            format!("tests/Acme/sha256/{hex}")
        );
        assert_eq!(
            S3Blobs::run_prefix(&run),
            "blobs/Acme/01930000-0000-7000-8000-000000000001"
        );
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
