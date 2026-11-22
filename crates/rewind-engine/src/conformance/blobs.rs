//! The conformance suite of `BlobStore`: content addressing, immutability, corruption
//! detection, deletion and tenant isolation; the blobs of an owner (a run, a tenant's tests),
//! their copy under another owner, and the deletion of a run's blobs.

use rewind_core::{Digest, PartitionId, RunId, Timestamp, canonical_json};
use serde_json::json;

use super::{T0, check, tenant};
use crate::ports::{BlobError, BlobOwner, BlobStore, RunRef, TenantId};

/// Runs every check, each on a new store.
pub async fn run_all<S: BlobStore>(new_store: impl Fn() -> S) {
    check!("blobs", new_store(), blobs_are_stored_by_tenant_and_digest);
    check!("blobs", new_store(), tenants_differing_in_case_are_distinct);
    check!("blobs", new_store(), blobs_are_immutable);
    check!("blobs", new_store(), bytes_must_hash_to_their_digest);
    check!("blobs", new_store(), blobs_are_deleted);
    check!("blobs", new_store(), large_blobs_round_trip);
    check!("blobs", new_store(), blobs_are_kept_apart_by_owner);
    check!("blobs", new_store(), owned_blobs_are_immutable_and_checked);
    check!("blobs", new_store(), copy_puts_a_blob_under_another_owner);
    check!(
        "blobs",
        new_store(),
        delete_run_deletes_every_blob_of_the_run
    );
    check!("blobs", new_store(), owned_blobs_are_deleted_one_at_a_time);
    check!("blobs", new_store(), owners_differing_in_case_are_distinct);
}

fn run_of(tenant_name: &str, n: u64) -> RunRef {
    RunRef::new(
        tenant(tenant_name),
        RunId::in_partition(PartitionId::new(1), Timestamp(T0.0 + n), n),
    )
}

fn run_owner(tenant_name: &str, n: u64) -> BlobOwner {
    BlobOwner::Run(run_of(tenant_name, n))
}

fn tests_of(tenant_name: &str) -> BlobOwner {
    BlobOwner::Tests(tenant(tenant_name))
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

pub async fn blobs_are_kept_apart_by_owner<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("owned");
    let run = run_owner("acme", 1);
    assert_eq!(store.get_in(&run, digest).await.expect("get_in"), None);
    store
        .put_in(&run, digest, bytes.clone())
        .await
        .expect("put_in");
    assert_eq!(
        store.get_in(&run, digest).await.expect("get_in"),
        Some(bytes.clone())
    );
    for elsewhere in [
        run_owner("acme", 2),
        tests_of("acme"),
        BlobOwner::Run(RunRef::new(tenant("other"), run_of("acme", 1).id)),
    ] {
        assert_eq!(
            store.get_in(&elsewhere, digest).await.expect("get_in"),
            None,
            "{elsewhere} does not see the blob of {run}"
        );
    }
    assert_eq!(
        store.get(&tenant("acme"), digest).await.expect("get"),
        None,
        "a blob of an owner is not a blob of the tenant alone"
    );
    let tests = tests_of("acme");
    store
        .put_in(&tests, digest, bytes.clone())
        .await
        .expect("put_in");
    assert!(store.delete_in(&tests, digest).await.expect("delete_in"));
    assert_eq!(
        store.get_in(&run, digest).await.expect("get_in"),
        Some(bytes),
        "deleting the tests' blob leaves the run's"
    );
}

pub async fn owned_blobs_are_immutable_and_checked<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("immutable");
    let (_, other_bytes) = blob("other");
    let run = run_owner("acme", 1);
    let refused = store.put_in(&run, digest, other_bytes.clone()).await;
    assert!(
        matches!(refused, Err(BlobError::Corrupt { .. })),
        "{refused:?}"
    );
    assert_eq!(store.get_in(&run, digest).await.expect("get_in"), None);
    store
        .put_in(&run, digest, bytes.clone())
        .await
        .expect("put_in");
    store
        .put_in(&run, digest, bytes.clone())
        .await
        .expect("a second put of a digest changes nothing");
    assert_eq!(
        store.get_in(&run, digest).await.expect("get_in"),
        Some(bytes)
    );
}

pub async fn copy_puts_a_blob_under_another_owner<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("forked");
    let (parent, fork) = (run_owner("acme", 1), run_owner("acme", 2));
    let missing = store.copy(&parent, &fork, digest).await;
    assert_eq!(missing, Err(BlobError::Missing(digest)));
    assert_eq!(store.get_in(&fork, digest).await.expect("get_in"), None);
    store
        .put_in(&parent, digest, bytes.clone())
        .await
        .expect("put_in");
    store.copy(&parent, &fork, digest).await.expect("copy");
    assert_eq!(
        store.get_in(&fork, digest).await.expect("get_in"),
        Some(bytes.clone())
    );
    store
        .copy(&parent, &fork, digest)
        .await
        .expect("a second copy changes nothing");
    store
        .delete_run(&run_of("acme", 1))
        .await
        .expect("delete_run");
    assert_eq!(
        store.get_in(&fork, digest).await.expect("get_in"),
        Some(bytes.clone()),
        "the fork's copy outlives its parent's blob"
    );
    let tests = tests_of("acme");
    store.copy(&fork, &tests, digest).await.expect("copy");
    assert_eq!(
        store.get_in(&tests, digest).await.expect("get_in"),
        Some(bytes)
    );
}

pub async fn delete_run_deletes_every_blob_of_the_run<S: BlobStore>(store: &S) {
    let (first, first_bytes) = blob("first");
    let (second, second_bytes) = blob("second");
    let (erased, kept, tests) = (run_owner("acme", 1), run_owner("acme", 2), tests_of("acme"));
    for (owner, digest, bytes) in [
        (&erased, first, &first_bytes),
        (&erased, second, &second_bytes),
        (&kept, first, &first_bytes),
        (&tests, first, &first_bytes),
    ] {
        store
            .put_in(owner, digest, bytes.clone())
            .await
            .expect("put_in");
    }
    store
        .put(&tenant("acme"), first, first_bytes.clone())
        .await
        .expect("put");
    store
        .delete_run(&run_of("acme", 1))
        .await
        .expect("delete_run");
    for digest in [first, second] {
        assert_eq!(store.get_in(&erased, digest).await.expect("get_in"), None);
    }
    for owner in [&kept, &tests] {
        assert_eq!(
            store.get_in(owner, first).await.expect("get_in"),
            Some(first_bytes.clone()),
            "the blobs of {owner} stay"
        );
    }
    assert_eq!(
        store.get(&tenant("acme"), first).await.expect("get"),
        Some(first_bytes)
    );
    store
        .delete_run(&run_of("acme", 1))
        .await
        .expect("deleting the blobs of a run twice is not an error");
    store
        .delete_run(&run_of("acme", 9))
        .await
        .expect("a run without blobs");
}

pub async fn owned_blobs_are_deleted_one_at_a_time<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("deleted");
    let tests = tests_of("acme");
    assert!(
        !store
            .delete_in(&tests, digest)
            .await
            .expect("deleting a missing blob is not an error")
    );
    store
        .put_in(&tests, digest, bytes.clone())
        .await
        .expect("put_in");
    assert!(store.delete_in(&tests, digest).await.expect("delete_in"));
    assert_eq!(store.get_in(&tests, digest).await.expect("get_in"), None);
    assert!(!store.delete_in(&tests, digest).await.expect("delete_in"));
    store
        .put_in(&tests, digest, bytes.clone())
        .await
        .expect("put again");
    assert_eq!(
        store.get_in(&tests, digest).await.expect("get_in"),
        Some(bytes)
    );
}

/// Tenant ids are case-sensitive under an owner too, whatever the backend.
pub async fn owners_differing_in_case_are_distinct<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("cased");
    let id = run_of("acme", 1).id;
    let upper = BlobOwner::Run(RunRef::new(tenant("Acme"), id));
    let lower = BlobOwner::Run(RunRef::new(tenant("acme"), id));
    store
        .put_in(&upper, digest, bytes.clone())
        .await
        .expect("put_in");
    assert_eq!(store.get_in(&lower, digest).await.expect("get_in"), None);
    store
        .put_in(&lower, digest, bytes.clone())
        .await
        .expect("put_in");
    store
        .delete_run(&RunRef::new(tenant("acme"), id))
        .await
        .expect("delete_run");
    assert_eq!(
        store.get_in(&upper, digest).await.expect("get_in"),
        Some(bytes.clone()),
        "deleting acme's run leaves Acme's"
    );
    let (upper_tests, lower_tests): (TenantId, TenantId) = (tenant("Acme"), tenant("acme"));
    store
        .put_in(&BlobOwner::Tests(upper_tests), digest, bytes)
        .await
        .expect("put_in");
    assert_eq!(
        store
            .get_in(&BlobOwner::Tests(lower_tests), digest)
            .await
            .expect("get_in"),
        None
    );
}
