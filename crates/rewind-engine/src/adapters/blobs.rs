use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use rewind_core::Digest;

use crate::ports::{BlobError, BlobOwner, BlobStore, RunRef};

type Blobs = BTreeMap<(BlobOwner, Digest), Vec<u8>>;

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

    /// Number of blobs stored, across owners.
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

    /// Replaces the bytes of an owner's blob without any check: for tests of corruption
    /// detection.
    pub fn corrupt(&self, owner: &BlobOwner, digest: Digest, bytes: Vec<u8>) {
        if let Ok(mut blobs) = self.lock() {
            blobs.insert((owner.clone(), digest), bytes);
        }
    }

    fn insert(
        &self,
        key: (BlobOwner, Digest),
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        if Digest::of(&bytes) != digest {
            return Err(BlobError::Corrupt {
                digest,
                message: "the bytes do not hash to the digest".into(),
            });
        }
        self.lock()?.entry(key).or_insert(bytes);
        Ok(())
    }

    fn read(
        &self,
        key: &(BlobOwner, Digest),
        digest: Digest,
    ) -> Result<Option<Vec<u8>>, BlobError> {
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
            .retain(|(owner, _), _| !matches!(owner, BlobOwner::Run(of) if of == run));
        Ok(())
    }
}

impl BlobStore for MemoryBlobs {
    async fn put(
        &self,
        owner: &BlobOwner,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        self.insert((owner.clone(), digest), digest, bytes)
    }

    async fn get(&self, owner: &BlobOwner, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        self.read(&(owner.clone(), digest), digest)
    }

    async fn copy(
        &self,
        from: &BlobOwner,
        to: &BlobOwner,
        digest: Digest,
    ) -> Result<(), BlobError> {
        let mut blobs = self.lock()?;
        let bytes = blobs
            .get(&(from.clone(), digest))
            .cloned()
            .ok_or(BlobError::Missing(digest))?;
        blobs.entry((to.clone(), digest)).or_insert(bytes);
        Ok(())
    }

    async fn delete_run(&self, run: &RunRef) -> Result<(), BlobError> {
        self.remove_run(run)
    }

    async fn delete(&self, owner: &BlobOwner, digest: Digest) -> Result<bool, BlobError> {
        Ok(self.lock()?.remove(&(owner.clone(), digest)).is_some())
    }
}
