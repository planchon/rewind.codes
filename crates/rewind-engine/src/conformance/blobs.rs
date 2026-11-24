//! The conformance suite of `BlobStore`: the blobs of an owner (a run, a tenant's tests), content
//! addressing, immutability, corruption detection, deletion, isolation between owners and
//! tenants, the copy of a blob under another owner, and the deletion of a run's blobs.

use rewind_core::{Digest, PartitionId, RunId, Timestamp, canonical_json};
use serde_json::json;

use super::{T0, check, tenant};
use crate::ports::{BlobError, BlobOwner, BlobStore, RunRef, TenantId};

/// Runs every check, each on a new store.
pub async fn run_all<S: BlobStore>(new_store: impl Fn() -> S) {
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

pub async fn large_blobs_round_trip<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob(&"x".repeat(3 * 1024 * 1024));
    let run = run_owner("acme", 1);
    store.put(&run, digest, bytes.clone()).await.expect("put");
    assert_eq!(store.get(&run, digest).await.expect("get"), Some(bytes));
}

pub async fn blobs_are_kept_apart_by_owner<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("owned");
    let run = run_owner("acme", 1);
    assert_eq!(store.get(&run, digest).await.expect("get"), None);
    store.put(&run, digest, bytes.clone()).await.expect("put");
    assert_eq!(
        store.get(&run, digest).await.expect("get"),
        Some(bytes.clone())
    );
    for elsewhere in [
        run_owner("acme", 2),
        tests_of("acme"),
        BlobOwner::Run(RunRef::new(tenant("other"), run_of("acme", 1).id)),
    ] {
        assert_eq!(
            store.get(&elsewhere, digest).await.expect("get"),
            None,
            "{elsewhere} does not see the blob of {run}"
        );
    }
    let tests = tests_of("acme");
    store.put(&tests, digest, bytes.clone()).await.expect("put");
    assert!(store.delete(&tests, digest).await.expect("delete"));
    assert_eq!(
        store.get(&run, digest).await.expect("get"),
        Some(bytes),
        "deleting the tests' blob leaves the run's"
    );
}

pub async fn owned_blobs_are_immutable_and_checked<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("immutable");
    let (_, other_bytes) = blob("other");
    let run = run_owner("acme", 1);
    let refused = store.put(&run, digest, other_bytes.clone()).await;
    assert!(
        matches!(refused, Err(BlobError::Corrupt { .. })),
        "{refused:?}"
    );
    assert_eq!(store.get(&run, digest).await.expect("get"), None);
    store.put(&run, digest, bytes.clone()).await.expect("put");
    store
        .put(&run, digest, bytes.clone())
        .await
        .expect("a second put of a digest changes nothing");
    assert_eq!(store.get(&run, digest).await.expect("get"), Some(bytes));
}

pub async fn copy_puts_a_blob_under_another_owner<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("forked");
    let (parent, fork) = (run_owner("acme", 1), run_owner("acme", 2));
    let missing = store.copy(&parent, &fork, digest).await;
    assert_eq!(missing, Err(BlobError::Missing(digest)));
    assert_eq!(store.get(&fork, digest).await.expect("get"), None);
    store
        .put(&parent, digest, bytes.clone())
        .await
        .expect("put");
    store.copy(&parent, &fork, digest).await.expect("copy");
    assert_eq!(
        store.get(&fork, digest).await.expect("get"),
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
        store.get(&fork, digest).await.expect("get"),
        Some(bytes.clone()),
        "the fork's copy outlives its source's blob"
    );
    let tests = tests_of("acme");
    store.copy(&fork, &tests, digest).await.expect("copy");
    assert_eq!(store.get(&tests, digest).await.expect("get"), Some(bytes));
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
        store.put(owner, digest, bytes.clone()).await.expect("put");
    }
    store
        .delete_run(&run_of("acme", 1))
        .await
        .expect("delete_run");
    for digest in [first, second] {
        assert_eq!(store.get(&erased, digest).await.expect("get"), None);
    }
    for owner in [&kept, &tests] {
        assert_eq!(
            store.get(owner, first).await.expect("get"),
            Some(first_bytes.clone()),
            "the blobs of {owner} stay"
        );
    }
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
            .delete(&tests, digest)
            .await
            .expect("deleting a missing blob is not an error")
    );
    store.put(&tests, digest, bytes.clone()).await.expect("put");
    assert!(store.delete(&tests, digest).await.expect("delete"));
    assert_eq!(store.get(&tests, digest).await.expect("get"), None);
    assert!(!store.delete(&tests, digest).await.expect("delete"));
    store
        .put(&tests, digest, bytes.clone())
        .await
        .expect("put again");
    assert_eq!(store.get(&tests, digest).await.expect("get"), Some(bytes));
}

/// Tenant ids are case-sensitive under an owner too, whatever the backend.
pub async fn owners_differing_in_case_are_distinct<S: BlobStore>(store: &S) {
    let (digest, bytes) = blob("cased");
    let id = run_of("acme", 1).id;
    let upper = BlobOwner::Run(RunRef::new(tenant("Acme"), id));
    let lower = BlobOwner::Run(RunRef::new(tenant("acme"), id));
    store.put(&upper, digest, bytes.clone()).await.expect("put");
    assert_eq!(store.get(&lower, digest).await.expect("get"), None);
    store.put(&lower, digest, bytes.clone()).await.expect("put");
    store
        .delete_run(&RunRef::new(tenant("acme"), id))
        .await
        .expect("delete_run");
    assert_eq!(
        store.get(&upper, digest).await.expect("get"),
        Some(bytes.clone()),
        "deleting acme's run leaves Acme's"
    );
    let (upper_tests, lower_tests): (TenantId, TenantId) = (tenant("Acme"), tenant("acme"));
    store
        .put(&BlobOwner::Tests(upper_tests), digest, bytes)
        .await
        .expect("put");
    assert_eq!(
        store
            .get(&BlobOwner::Tests(lower_tests), digest)
            .await
            .expect("get"),
        None
    );
}
