use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use rewind_core::Digest;

use crate::ports::{BlobError, BlobOwner, BlobStore, RunRef, TenantId};

/// Where a blob is kept: under a tenant alone, or under its owner.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Tenant(TenantId, Digest),
    Owned(BlobOwner, Digest),
}

type Blobs = BTreeMap<Key, Vec<u8>>;

/// `BlobStore` in memory. It passes the conformance suite. Its clones share their blobs: a
/// [`MemoryArchive`](crate::adapters::MemoryArchive) erases a run's blobs through one.
#[derive(Debug, Default, Clone)]
pub struct MemoryBlobs {
    blobs: Arc<Mutex<Blobs>>,
}

impl MemoryBlobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of blobs stored, across tenants and owners.
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
            blobs.insert(Key::Tenant(tenant.clone(), digest), bytes);
        }
    }

    /// Replaces the bytes of an owner's blob without any check: for tests of corruption
    /// detection.
    pub fn corrupt_in(&self, owner: &BlobOwner, digest: Digest, bytes: Vec<u8>) {
        if let Ok(mut blobs) = self.lock() {
            blobs.insert(Key::Owned(owner.clone(), digest), bytes);
        }
    }

    fn insert(&self, key: Key, digest: Digest, bytes: Vec<u8>) -> Result<(), BlobError> {
        if Digest::of(&bytes) != digest {
            return Err(BlobError::Corrupt {
                digest,
                message: "the bytes do not hash to the digest".into(),
            });
        }
        self.lock()?.entry(key).or_insert(bytes);
        Ok(())
    }

    fn read(&self, key: &Key, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        let bytes = self.lock()?.get(key).cloned();
        match bytes {
            Some(bytes) if Digest::of(&bytes) != digest => Err(BlobError::Corrupt {
                digest,
                message: "the stored bytes do not hash to the digest".into(),
            }),
            other => Ok(other),
        }
    }

    /// Deletes every blob of a run: what [`BlobStore::delete_run`] does.
    pub(crate) fn remove_run(&self, run: &RunRef) -> Result<(), BlobError> {
        self.lock()?
            .retain(|key, _| !matches!(key, Key::Owned(BlobOwner::Run(of), _) if of == run));
        Ok(())
    }
}

impl BlobStore for MemoryBlobs {
    async fn put(
        &self,
        tenant: &TenantId,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        self.insert(Key::Tenant(tenant.clone(), digest), digest, bytes)
    }

    async fn get(&self, tenant: &TenantId, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        self.read(&Key::Tenant(tenant.clone(), digest), digest)
    }

    async fn delete(&self, tenant: &TenantId, digest: Digest) -> Result<bool, BlobError> {
        Ok(self
            .lock()?
            .remove(&Key::Tenant(tenant.clone(), digest))
            .is_some())
    }

    async fn put_in(
        &self,
        owner: &BlobOwner,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        self.insert(Key::Owned(owner.clone(), digest), digest, bytes)
    }

    async fn get_in(
        &self,
        owner: &BlobOwner,
        digest: Digest,
    ) -> Result<Option<Vec<u8>>, BlobError> {
        self.read(&Key::Owned(owner.clone(), digest), digest)
    }

    async fn copy(
        &self,
        from: &BlobOwner,
        to: &BlobOwner,
        digest: Digest,
    ) -> Result<(), BlobError> {
        let mut blobs = self.lock()?;
        let bytes = blobs
            .get(&Key::Owned(from.clone(), digest))
            .cloned()
            .ok_or(BlobError::Missing(digest))?;
        blobs.entry(Key::Owned(to.clone(), digest)).or_insert(bytes);
        Ok(())
    }

    async fn delete_run(&self, run: &RunRef) -> Result<(), BlobError> {
        self.remove_run(run)
    }

    async fn delete_in(&self, owner: &BlobOwner, digest: Digest) -> Result<bool, BlobError> {
        Ok(self
            .lock()?
            .remove(&Key::Owned(owner.clone(), digest))
            .is_some())
    }
}
