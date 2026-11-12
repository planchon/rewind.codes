//! The workers' local cache of blobs (`archi/06-storage-and-scale.md`, "Payloads").

use std::fmt;
use std::sync::Arc;

use ptc_core::Digest;
use ptc_engine::ports::{BlobError, BlobStore, TenantId};
use quick_cache::sync::Cache;
use quick_cache::{OptionsBuilder, Weighter};

type Key = (TenantId, Digest);
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

/// A `BlobStore` with a local cache in front of it: what a worker puts in front of S3.
///
/// The cache is in memory, bounded in bytes, and evicts the blobs read least recently and least
/// often (CLOCK-Pro, from `quick_cache`). Its entries are immutable: a digest always names the
/// same bytes, so an entry is never refreshed, and only bytes whose digest a store checked
/// enter it. A get that misses reads the inner store and keeps the blob; a put always writes
/// to the inner store, then keeps the blob; a delete drops the entry and deletes from the
/// inner store. Another worker's cache may still serve a blob this one deleted: a cleanup only
/// deletes blobs that no run has referenced for a day.
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

    /// Whether the cache holds a tenant's blob.
    pub fn is_cached(&self, tenant: &TenantId, digest: Digest) -> bool {
        self.cache.peek(&(tenant.clone(), digest)).is_some()
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
        tenant: &TenantId,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        // Always written through: the blob may have been cleaned up from the inner store since
        // this cache kept it.
        self.inner.put(tenant, digest, bytes.clone()).await?;
        self.cache.insert((tenant.clone(), digest), Arc::new(bytes));
        Ok(())
    }

    async fn get(&self, tenant: &TenantId, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        let key = (tenant.clone(), digest);
        if let Some(bytes) = self.cache.get(&key) {
            return Ok(Some(Vec::clone(&bytes)));
        }
        let bytes = self.inner.get(tenant, digest).await?;
        if let Some(bytes) = &bytes {
            self.cache.insert(key, Arc::new(bytes.clone()));
        }
        Ok(bytes)
    }

    async fn delete(&self, tenant: &TenantId, digest: Digest) -> Result<bool, BlobError> {
        let key = (tenant.clone(), digest);
        self.cache.remove(&key);
        let existed = self.inner.delete(tenant, digest).await?;
        // Also drops what a get that missed concurrently kept.
        self.cache.remove(&key);
        Ok(existed)
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
