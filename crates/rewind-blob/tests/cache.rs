//! `CachedBlobs`: the conformance suite of `BlobStore`, hits and misses, the bound, writes
//! through and deletion.

use std::sync::atomic::{AtomicUsize, Ordering};

use rewind_blob::{CachedBlobs, DEFAULT_CACHE_CAPACITY};
use rewind_core::{Digest, canonical_json};
use rewind_engine::adapters::MemoryBlobs;
use rewind_engine::conformance::{blobs, tenant};
use rewind_engine::ports::{BlobError, BlobOwner, BlobStore, RunRef, TenantId};
use serde_json::json;

fn blob(text: &str) -> (Digest, Vec<u8>) {
    let bytes = canonical_json(&json!({ "text": text })).into_bytes();
    (Digest::of(&bytes), bytes)
}

/// A memory store that counts the gets it serves.
#[derive(Debug, Default)]
struct Counting {
    blobs: MemoryBlobs,
    gets: AtomicUsize,
}

impl Counting {
    fn gets(&self) -> usize {
        self.gets.load(Ordering::Relaxed)
    }
}

impl BlobStore for Counting {
    async fn put(
        &self,
        tenant: &TenantId,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        self.blobs.put(tenant, digest, bytes).await
    }

    async fn get(&self, tenant: &TenantId, digest: Digest) -> Result<Option<Vec<u8>>, BlobError> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.blobs.get(tenant, digest).await
    }

    async fn delete(&self, tenant: &TenantId, digest: Digest) -> Result<bool, BlobError> {
        self.blobs.delete(tenant, digest).await
    }

    async fn put_in(
        &self,
        owner: &BlobOwner,
        digest: Digest,
        bytes: Vec<u8>,
    ) -> Result<(), BlobError> {
        self.blobs.put_in(owner, digest, bytes).await
    }

    async fn get_in(
        &self,
        owner: &BlobOwner,
        digest: Digest,
    ) -> Result<Option<Vec<u8>>, BlobError> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.blobs.get_in(owner, digest).await
    }

    async fn copy(
        &self,
        from: &BlobOwner,
        to: &BlobOwner,
        digest: Digest,
    ) -> Result<(), BlobError> {
        self.blobs.copy(from, to, digest).await
    }

    async fn delete_run(&self, run: &RunRef) -> Result<(), BlobError> {
        self.blobs.delete_run(run).await
    }

    async fn delete_in(&self, owner: &BlobOwner, digest: Digest) -> Result<bool, BlobError> {
        self.blobs.delete_in(owner, digest).await
    }
}

fn cached(capacity: u64) -> CachedBlobs<Counting> {
    CachedBlobs::new(Counting::default(), capacity)
}

#[tokio::test]
async fn cached_blobs_pass_the_conformance_suite() {
    blobs::run_all(|| CachedBlobs::new(MemoryBlobs::new(), DEFAULT_CACHE_CAPACITY)).await;
}

#[tokio::test]
async fn a_miss_reads_the_store_once_and_keeps_the_blob() {
    let store = cached(DEFAULT_CACHE_CAPACITY);
    let (digest, bytes) = blob("miss");
    store
        .inner()
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put behind the cache");
    assert!(!store.is_cached(&tenant("acme"), digest));

    for _ in 0..3 {
        assert_eq!(
            store.get(&tenant("acme"), digest).await.expect("get"),
            Some(bytes.clone())
        );
    }

    assert_eq!(
        store.inner().gets(),
        1,
        "only the first get reached the store"
    );
    assert!(store.is_cached(&tenant("acme"), digest));
    assert!(
        !store.is_cached(&tenant("other"), digest),
        "entries are per tenant"
    );
}

#[tokio::test]
async fn a_put_is_written_through_and_kept() {
    let store = cached(DEFAULT_CACHE_CAPACITY);
    let (digest, bytes) = blob("put");
    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put");
    assert!(store.is_cached(&tenant("acme"), digest));

    // Cleaned up from the store behind the cache's back (by another worker), then put again.
    store
        .inner()
        .delete(&tenant("acme"), digest)
        .await
        .expect("delete behind the cache");
    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put again");

    assert_eq!(
        store
            .inner()
            .get(&tenant("acme"), digest)
            .await
            .expect("get"),
        Some(bytes),
        "the second put reached the store"
    );
}

#[tokio::test]
async fn a_refused_put_keeps_nothing() {
    let store = cached(DEFAULT_CACHE_CAPACITY);
    let (digest, _) = blob("refused");
    let (_, other_bytes) = blob("other");

    let refused = store.put(&tenant("acme"), digest, other_bytes).await;

    assert!(
        matches!(refused, Err(BlobError::Corrupt { .. })),
        "{refused:?}"
    );
    assert!(!store.is_cached(&tenant("acme"), digest));
}

#[tokio::test]
async fn a_delete_drops_the_entry() {
    let store = cached(DEFAULT_CACHE_CAPACITY);
    let (digest, bytes) = blob("delete");
    store
        .put(&tenant("acme"), digest, bytes)
        .await
        .expect("put");

    assert!(store.delete(&tenant("acme"), digest).await.expect("delete"));

    assert!(!store.is_cached(&tenant("acme"), digest));
    assert_eq!(store.get(&tenant("acme"), digest).await.expect("get"), None);
}

#[tokio::test]
async fn the_cache_stays_within_its_capacity() {
    let capacity = 512 * 1024;
    let store = cached(capacity);
    let mut stored = Vec::new();
    for n in 0..200 {
        let (digest, bytes) = blob(&format!("{n:04}").repeat(2_500));
        store
            .put(&tenant("acme"), digest, bytes.clone())
            .await
            .expect("put");
        stored.push((digest, bytes));
    }

    assert!(
        store.cached_bytes() <= capacity,
        "{} bytes cached",
        store.cached_bytes()
    );
    assert!(store.cached_blobs() < stored.len());
    for (digest, bytes) in stored {
        assert_eq!(
            store.get(&tenant("acme"), digest).await.expect("get"),
            Some(bytes),
            "an evicted blob is read from the store"
        );
    }
}

#[tokio::test]
async fn a_blob_larger_than_a_shard_is_not_cached() {
    let capacity = 512 * 1024;
    let store = cached(capacity);
    let (digest, bytes) = blob(&"x".repeat(usize::try_from(capacity / 4).expect("size")));

    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put");

    assert!(!store.is_cached(&tenant("acme"), digest));
    assert_eq!(
        store.get(&tenant("acme"), digest).await.expect("get"),
        Some(bytes)
    );
}
