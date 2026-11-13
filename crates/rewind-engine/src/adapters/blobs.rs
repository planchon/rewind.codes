use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use rewind_core::Digest;

use crate::ports::{BlobError, BlobStore, TenantId};

type Blobs = HashMap<(TenantId, Digest), Vec<u8>>;

/// `BlobStore` in memory. It passes the conformance suite.
#[derive(Debug, Default)]
pub struct MemoryBlobs {
    blobs: Mutex<Blobs>,
}

impl MemoryBlobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of blobs stored, across tenants.
    pub fn len(&self) -> usize {
        self.lock().map_or(0, |blobs| blobs.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> Result<MutexGuard<'_, Blobs>, BlobError> {
        self.blobs
            .lock()
            .map_err(|_| BlobError::Backend("the memory blob store's lock is poisoned".into()))
    }

    /// Replaces a blob's bytes without any check: for tests of corruption detection.
    pub fn corrupt(&self, tenant: &TenantId, digest: Digest, bytes: Vec<u8>) {
        if let Ok(mut blobs) = self.lock() {
            blobs.insert((tenant.clone(), digest), bytes);
        }
    }
}

impl BlobStore for MemoryBlobs {
    async fn put(
        &self,
        tenant: &TenantId,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        if Digest::of(&bytes) != digest {
            return Err(BlobError::Corrupt {
                digest,
                message: "the bytes do not hash to the digest".into(),
            });
        }
        self.lock()?
            .entry((tenant.clone(), digest))
            .or_insert(bytes);
        Ok(())
    }

    async fn get(&self, tenant: &TenantId, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        let bytes = self.lock()?.get(&(tenant.clone(), digest)).cloned();
        match bytes {
            Some(bytes) if Digest::of(&bytes) != digest => Err(BlobError::Corrupt {
                digest,
                message: "the stored bytes do not hash to the digest".into(),
            }),
            other => Ok(other),
        }
    }

    async fn delete(&self, tenant: &TenantId, digest: Digest) -> Result<bool, BlobError> {
        Ok(self.lock()?.remove(&(tenant.clone(), digest)).is_some())
    }
}
