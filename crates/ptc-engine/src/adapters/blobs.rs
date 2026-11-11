use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use ptc_core::Digest;

use crate::ports::{BlobError, BlobStore};

/// `BlobStore` in memory.
#[derive(Debug, Default)]
pub struct MemoryBlobs {
    blobs: Mutex<HashMap<Digest, Vec<u8>>>,
}

impl MemoryBlobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of blobs stored.
    pub fn len(&self) -> usize {
        self.lock().map_or(0, |blobs| blobs.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<Digest, Vec<u8>>>, BlobError> {
        self.blobs
            .lock()
            .map_err(|_| BlobError::Backend("the memory blob store's lock is poisoned".into()))
    }
}

impl BlobStore for MemoryBlobs {
    async fn put(&self, digest: Digest, bytes: Vec<u8>) -> Result<(), BlobError> {
        self.lock()?.entry(digest).or_insert(bytes);
        Ok(())
    }

    async fn get(&self, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        Ok(self.lock()?.get(&digest).cloned())
    }
}
