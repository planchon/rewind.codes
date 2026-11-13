//! The conformance suite of `BlobStore`: content addressing, immutability, corruption
//! detection, deletion and tenant isolation.

use rewind_core::{Digest, canonical_json};
use serde_json::json;

use super::{check, tenant};
use crate::ports::{BlobError, BlobStore};

/// Runs every check, each on a new store.
pub async fn run_all<S: BlobStore>(new_store: impl Fn() -> S) {
    check!("blobs", new_store(), blobs_are_stored_by_tenant_and_digest);
    check!("blobs", new_store(), tenants_differing_in_case_are_distinct);
    check!("blobs", new_store(), blobs_are_immutable);
    check!("blobs", new_store(), bytes_must_hash_to_their_digest);
    check!("blobs", new_store(), blobs_are_deleted);
    check!("blobs", new_store(), large_blobs_round_trip);
}

fn blob(text: &str) -> (Digest, Vec<u8>) {
    let bytes = canonical_json(&json!({ "text": text })).into_bytes();
    (Digest::of(&bytes), bytes)
}

pub async fn blobs_are_stored_by_tenant_and_digest<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("a");
    assert_eq!(store.get(&tenant("acme"), digest).await.expect("get"), None);
    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put");
    assert_eq!(
        store.get(&tenant("acme"), digest).await.expect("get"),
        Some(bytes.clone())
    );
    assert_eq!(
        store.get(&tenant("other"), digest).await.expect("get"),
        None,
        "another tenant does not see it"
    );
    store
        .put(&tenant("other"), digest, bytes.clone())
        .await
        .expect("put");
    assert!(
        store
            .delete(&tenant("other"), digest)
            .await
            .expect("delete")
    );
    assert_eq!(
        store.get(&tenant("acme"), digest).await.expect("get"),
        Some(bytes),
        "deleting a tenant's blob leaves the other's"
    );
}

/// Tenant ids are case-sensitive: `Acme` and `acme` are two tenants, whatever the backend
/// (a case-insensitive file system included).
pub async fn tenants_differing_in_case_are_distinct<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("case");
    store
        .put(&tenant("Acme"), digest, bytes.clone())
        .await
        .expect("put");
    assert_eq!(
        store.get(&tenant("acme"), digest).await.expect("get"),
        None,
        "acme does not see Acme's blob"
    );
    assert!(
        !store.delete(&tenant("acme"), digest).await.expect("delete"),
        "acme has no blob to delete"
    );
    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put");
    assert!(store.delete(&tenant("acme"), digest).await.expect("delete"));
    assert_eq!(
        store.get(&tenant("Acme"), digest).await.expect("get"),
        Some(bytes),
        "deleting acme's blob leaves Acme's"
    );
}

pub async fn blobs_are_immutable<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("b");
    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put");
    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("a second put of a digest changes nothing");
    assert_eq!(
        store.get(&tenant("acme"), digest).await.expect("get"),
        Some(bytes)
    );
}

pub async fn bytes_must_hash_to_their_digest<S: BlobStore>(store: &S) {
    let (digest, _) = blob("c");
    let (_, other_bytes) = blob("d");
    let refused = store.put(&tenant("acme"), digest, other_bytes).await;
    assert!(
        matches!(refused, Err(BlobError::Corrupt { .. })),
        "{refused:?}"
    );
    assert_eq!(store.get(&tenant("acme"), digest).await.expect("get"), None);
}

pub async fn blobs_are_deleted<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("e");
    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put");
    assert!(store.delete(&tenant("acme"), digest).await.expect("delete"));
    assert_eq!(store.get(&tenant("acme"), digest).await.expect("get"), None);
    assert!(
        !store
            .delete(&tenant("acme"), digest)
            .await
            .expect("deleting a missing blob is not an error")
    );
    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put again");
    assert_eq!(
        store.get(&tenant("acme"), digest).await.expect("get"),
        Some(bytes)
    );
}

pub async fn large_blobs_round_trip<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob(&"x".repeat(3 * 1024 * 1024));
    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put");
    assert_eq!(
        store.get(&tenant("acme"), digest).await.expect("get"),
        Some(bytes)
    );
}
