//! The conformance suite of `RunArchive`: sealed attempts read back as they were put, a second
//! put that changes nothing, the erasure of every attempt and blob of a run, tenant isolation.
//!
//! The suite also reads the blob store the archive erases a run's blobs from: the factory
//! returns the archive and that store, over the same backend.

use rewind_core::{
    ActionSet, Annotation, Digest, Epoch, Event, HistoryEntry, Lifecycle, LogLevel, PartitionId,
    RunId, RunMode, RunStarted, Script, Seed, Timestamp, canonical_json,
};
use serde_json::json;

use super::{T0, tenant};
use crate::ports::{Attempt, BlobOwner, BlobStore, RunArchive, RunRef, SealedAttempt};

/// Runs every check, each on a new archive and its blob store.
pub async fn run_all<A: RunArchive, B: BlobStore>(new_archive: impl Fn() -> (A, B)) {
    macro_rules! archive_check {
        ($check:ident) => {{
            eprintln!(concat!("archive conformance: ", stringify!($check)));
            let (archive, blobs) = new_archive();
            $check(&archive, &blobs).await;
        }};
    }
    archive_check!(sealed_attempts_are_read_back_as_they_were_put);
    archive_check!(a_second_put_of_an_attempt_changes_nothing);
    archive_check!(erase_deletes_every_attempt_and_blob_of_the_run);
    archive_check!(runs_of_another_tenant_are_invisible);
}

fn run(tenant_name: &str, n: u64) -> RunRef {
    RunRef::new(
        tenant(tenant_name),
        RunId::in_partition(PartitionId::new(2), Timestamp(T0.0 + n), n),
    )
}

fn script() -> Script {
    Script::new("export default async (input) => input")
}

fn action_set() -> ActionSet {
    ActionSet::from_harness_actions(["github.close_issue"]).expect("action set")
}

fn sealed(run: &RunRef, attempt: Attempt, logs: u64) -> SealedAttempt {
    let started = RunStarted {
        script: script().digest(),
        action_set: action_set().digest(),
        input: json!({ "issue": 7, "body": "x".repeat(2_000) }).into(),
        seed: Seed::from_bytes([3; 32]),
        epoch: Epoch(1),
        expires_at: Timestamp(T0.0 + 86_400_000),
        mode: RunMode::Live,
    };
    let mut entries = vec![HistoryEntry {
        index: 0,
        recorded_at: T0,
        event: Event::Lifecycle(Lifecycle::RunStarted(started)),
    }];
    for index in 1..=logs {
        entries.push(HistoryEntry {
            index,
            recorded_at: Timestamp(T0.0 + index),
            event: Event::Annotation(Annotation::Log {
                level: LogLevel::Info,
                message: format!("attempt {attempt}, entry {index}"),
                callsite: None,
            }),
        });
    }
    SealedAttempt {
        run: run.clone(),
        attempt,
        script: script(),
        action_set: action_set(),
        entries,
    }
}

fn blob(text: &str) -> (Digest, Vec<u8>) {
    let bytes = canonical_json(&json!({ "large": text })).into_bytes();
    (Digest::of(&bytes), bytes)
}

pub async fn sealed_attempts_are_read_back_as_they_were_put<A: RunArchive, B: BlobStore>(
    archive: &A,
    _blobs: &B,
) {
    let run = run("acme", 1);
    assert_eq!(archive.get(&run, Attempt::FIRST).await.expect("get"), None);
    let first = sealed(&run, Attempt::FIRST, 3);
    archive.put(&first).await.expect("put");
    assert_eq!(
        archive.get(&run, Attempt::FIRST).await.expect("get"),
        Some(first.clone())
    );
    assert_eq!(
        archive.get(&run, Attempt::FIRST.next()).await.expect("get"),
        None,
        "another attempt of the run"
    );
    let second = sealed(&run, Attempt::FIRST.next(), 1);
    archive.put(&second).await.expect("put");
    assert_eq!(
        archive.get(&run, Attempt::FIRST.next()).await.expect("get"),
        Some(second)
    );
    assert_eq!(
        archive.get(&run, Attempt::FIRST).await.expect("get"),
        Some(first),
        "each attempt is sealed apart"
    );
}

pub async fn a_second_put_of_an_attempt_changes_nothing<A: RunArchive, B: BlobStore>(
    archive: &A,
    _blobs: &B,
) {
    let run = run("acme", 1);
    let first = sealed(&run, Attempt::FIRST, 2);
    archive.put(&first).await.expect("put");
    archive
        .put(&first)
        .await
        .expect("a second put of the same attempt is not an error");
    archive
        .put(&sealed(&run, Attempt::FIRST, 5))
        .await
        .expect("a second put of the attempt is not an error");
    assert_eq!(
        archive.get(&run, Attempt::FIRST).await.expect("get"),
        Some(first),
        "the first put stays"
    );
}

pub async fn erase_deletes_every_attempt_and_blob_of_the_run<A: RunArchive, B: BlobStore>(
    archive: &A,
    blobs: &B,
) {
    let (erased, kept) = (run("acme", 1), run("acme", 2));
    for attempt in [Attempt::FIRST, Attempt::FIRST.next()] {
        archive
            .put(&sealed(&erased, attempt, 1))
            .await
            .expect("put");
    }
    archive
        .put(&sealed(&kept, Attempt::FIRST, 1))
        .await
        .expect("put");
    let (digest, bytes) = blob("payload above 64 KiB");
    let tests = BlobOwner::Tests(tenant("acme"));
    for owner in [
        BlobOwner::Run(erased.clone()),
        BlobOwner::Run(kept.clone()),
        tests.clone(),
    ] {
        blobs.put(&owner, digest, bytes.clone()).await.expect("put");
    }

    archive.erase(&erased).await.expect("erase");
    for attempt in [Attempt::FIRST, Attempt::FIRST.next()] {
        assert_eq!(archive.get(&erased, attempt).await.expect("get"), None);
    }
    assert_eq!(
        blobs
            .get(&BlobOwner::Run(erased.clone()), digest)
            .await
            .expect("get"),
        None,
        "the run's blobs go with it"
    );
    assert!(
        archive
            .get(&kept, Attempt::FIRST)
            .await
            .expect("get")
            .is_some(),
        "another run stays"
    );
    for owner in [BlobOwner::Run(kept.clone()), tests] {
        assert_eq!(
            blobs.get(&owner, digest).await.expect("get"),
            Some(bytes.clone()),
            "the blobs of {owner} stay"
        );
    }
    archive
        .erase(&erased)
        .await
        .expect("erasing a run twice is not an error");
    archive
        .erase(&run("acme", 9))
        .await
        .expect("erasing a run with nothing archived is not an error");
}

pub async fn runs_of_another_tenant_are_invisible<A: RunArchive, B: BlobStore>(
    archive: &A,
    _blobs: &B,
) {
    let run = run("acme", 1);
    let other = RunRef::new(tenant("other"), run.id);
    archive
        .put(&sealed(&run, Attempt::FIRST, 1))
        .await
        .expect("put");
    assert_eq!(
        archive.get(&other, Attempt::FIRST).await.expect("get"),
        None
    );
    archive.erase(&other).await.expect("erase");
    assert!(
        archive
            .get(&run, Attempt::FIRST)
            .await
            .expect("get")
            .is_some(),
        "erasing another tenant's run leaves this one"
    );
}
