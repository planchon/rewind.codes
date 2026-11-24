//! The nodes' local cache of blobs (`archi/06-storage-and-scale.md`, "Payloads").

use std::fmt;
use std::future::Future;
use std::sync::Arc;

use quick_cache::sync::Cache;
use quick_cache::{OptionsBuilder, Weighter};
use rewind_core::Digest;
use rewind_engine::ports::{BlobError, BlobOwner, BlobStore, RunRef};

/// A cached blob: its owner and its digest.
type Key = (BlobOwner, Digest);

type Bytes = Arc<Vec<u8>>;

/// What an entry costs beyond its bytes: its key and the cache's bookkeeping.
const ENTRY_OVERHEAD: u64 = 128;

/// The cache is split into this many independently locked shards. A blob larger than a shard,
/// an eighth of the capacity, is not cached.
const SHARDS: usize = 8;

/// The size the cache expects of an average blob, to size its tables: the blobs above the
/// table tier are larger than 64 KiB.
const TYPICAL_BLOB: u64 = 64 * 1024;

/// The default capacity of a [`CachedBlobs`]: 256 MiB.
pub const DEFAULT_CACHE_CAPACITY: u64 = 256 * 1024 * 1024;

/// A `BlobStore` with a local cache in front of it: what a node puts in front of S3.
///
/// The cache is in memory, bounded in bytes, and evicts the blobs read least recently and least
/// often (CLOCK-Pro, from `quick_cache`). Its entries are immutable: a digest always names the
/// same bytes, so an entry is never refreshed, and only bytes whose digest a store checked
/// enter it. A get that misses reads the inner store and keeps the blob; a put always writes
/// to the inner store, then keeps the blob; a delete drops the entry and deletes from the
/// inner store, and so does the deletion of a run's blobs for each of them. A copy is the inner
/// store's. Another node's cache may still serve a blob this one deleted: a cleanup only
/// deletes blobs that no run has referenced for a day, and an erased run is read no more.
pub struct CachedBlobs<S> {
    inner: S,
    cache: Cache<Key, Bytes, BlobWeight>,
    capacity: u64,
}

impl<S> CachedBlobs<S> {
    /// A cache of at most `capacity` bytes in front of `inner`; see [`DEFAULT_CACHE_CAPACITY`].
    pub fn new(inner: S, capacity: u64) -> Self {
        // At least 32 entries per shard, or quick_cache merges shards.
        let entries = usize::try_from(capacity / TYPICAL_BLOB)
            .unwrap_or(usize::MAX)
            .max(32 * SHARDS);
        let options = OptionsBuilder::new()
            .shards(SHARDS)
            .estimated_items_capacity(entries)
            .weight_capacity(capacity)
            .build()
            .expect("both capacities are set");
        Self {
            inner,
            cache: Cache::with_options(options, BlobWeight, Default::default(), Default::default()),
            capacity,
        }
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }

    /// The bound of the cache, in bytes.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The bytes the cache holds, the entries' overhead included.
    pub fn cached_bytes(&self) -> u64 {
        self.cache.weight()
    }

    /// The number of blobs the cache holds.
    pub fn cached_blobs(&self) -> usize {
        self.cache.len()
    }

    /// Whether the cache holds an owner's blob.
    pub fn is_cached(&self, owner: &BlobOwner, digest: Digest) -> bool {
        self.cache.peek(&(owner.clone(), digest)).is_some()
    }

    async fn read(
        &self,
        key: Key,
        inner: impl Future<Output = Result<Option<Vec<u8>>, BlobError>>,
    ) -> Result<Option<Vec<u8>>, BlobError> {
        if let Some(bytes) = self.cache.get(&key) {
            return Ok(Some(Vec::clone(&bytes)));
        }
        let bytes = inner.await?;
        if let Some(bytes) = &bytes {
            self.cache.insert(key, Arc::new(bytes.clone()));
        }
        Ok(bytes)
    }

    async fn remove(
        &self,
        key: Key,
        inner: impl Future<Output = Result<bool, BlobError>>,
    ) -> Result<bool, BlobError> {
        self.cache.remove(&key);
        let existed = inner.await?;
        // Also drops what a get that missed concurrently kept.
        self.cache.remove(&key);
        Ok(existed)
    }
}

impl<S: fmt::Debug> fmt::Debug for CachedBlobs<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedBlobs")
            .field("inner", &self.inner)
            .field("capacity", &self.capacity)
            .field("cached_bytes", &self.cached_bytes())
            .field("cached_blobs", &self.cached_blobs())
            .finish()
    }
}

impl<S: BlobStore> BlobStore for CachedBlobs<S> {
    async fn put(
        &self,
        owner: &BlobOwner,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        self.inner.put(owner, digest, bytes.clone()).await?;
        self.cache.insert((owner.clone(), digest), Arc::new(bytes));
        Ok(())
    }

    async fn get(&self, owner: &BlobOwner, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        self.read((owner.clone(), digest), self.inner.get(owner, digest))
            .await
    }

    async fn copy(
        &self,
        from: &BlobOwner,
        to: &BlobOwner,
        digest: Digest,
    ) -> Result<(), BlobError> {
        self.inner.copy(from, to, digest).await
    }

    async fn delete_run(&self, run: &RunRef) -> Result<(), BlobError> {
        let of_run = |(owner, _): &Key| matches!(owner, BlobOwner::Run(of) if of == run);
        self.cache.retain(|key, _| !of_run(key));
        self.inner.delete_run(run).await?;
        self.cache.retain(|key, _| !of_run(key));
        Ok(())
    }

    async fn delete(&self, owner: &BlobOwner, digest: Digest) -> Result<bool, BlobError> {
        self.remove((owner.clone(), digest), self.inner.delete(owner, digest))
            .await
    }
}

/// Weighs an entry by its bytes.
#[derive(Debug, Clone, Copy)]
struct BlobWeight;

impl Weighter<Key, Bytes> for BlobWeight {
    fn weight(&self, _key: &Key, bytes: &Bytes) -> u64 {
        bytes.len() as u64 + ENTRY_OVERHEAD
    }
}
