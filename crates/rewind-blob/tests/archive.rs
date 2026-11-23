//! `FileArchive`: the conformance suite of `RunArchive`, alone and behind the local cache; the
//! layout on disk beside the blobs; corruption detection; the cache's bound.

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};

use rewind_blob::{CachedArchive, DEFAULT_ARCHIVE_CACHE_CAPACITY, FileArchive, FileBlobs};
use rewind_core::{
    Digest, Epoch, Event, HistoryEntry, Lifecycle, RunMode, RunStarted, Script, Seed, Timestamp,
    Toolset, canonical_json,
};
use rewind_engine::conformance::{archive, tenant};
use rewind_engine::ports::{
    ArchiveError, Attempt, BlobOwner, BlobStore, RunArchive, RunRef, SealedAttempt,
};
use serde_json::json;

fn run(tenant_name: &str) -> RunRef {
    RunRef::new(
        tenant(tenant_name),
        "01930000-0000-7000-8000-000000000001"
            .parse()
            .expect("run id"),
    )
}

fn sealed(run: &RunRef, attempt: Attempt, input: &str) -> SealedAttempt {
    let script = Script::new("export default async (input) => input");
    let toolset = Toolset::from_host_tools(["github.close_issue"]).expect("toolset");
    SealedAttempt {
        run: run.clone(),
        attempt,
        entries: vec![HistoryEntry {
            index: 0,
            recorded_at: Timestamp(1),
            event: Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
                script: script.digest(),
                toolset: toolset.digest(),
                input: json!({ "text": input }).into(),
                seed: Seed::from_bytes([1; 32]),
                epoch: Epoch(1),
                expires_at: Timestamp(2),
                mode: RunMode::Live,
            })),
        }],
        script,
        toolset,
    }
}

#[tokio::test]
async fn file_archives_pass_the_conformance_suite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let roots = AtomicUsize::new(0);
    let open = || {
        let n = roots.fetch_add(1, Ordering::Relaxed);
        FileBlobs::open(dir.path().join(format!("rewind-{n}"))).expect("open")
    };
    archive::run_all(|| {
        let blobs = open();
        (FileArchive::new(blobs.clone()), blobs)
    })
    .await;
    archive::run_all(|| {
        let blobs = open();
        (
            CachedArchive::new(
                FileArchive::new(blobs.clone()),
                DEFAULT_ARCHIVE_CACHE_CAPACITY,
            ),
            blobs,
        )
    })
    .await;
}

#[tokio::test]
async fn a_sealed_attempt_is_a_journal_document_beside_the_run_s_blobs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join(".rewind");
    let blobs = FileBlobs::open(&root).expect("open");
    let archive = FileArchive::new(blobs.clone());
    let run = run("Acme");
    let first = sealed(&run, Attempt::FIRST, "first");
    archive.put(&first).await.expect("put");

    let path = root
        .join("runs")
        .join("+acme")
        .join("01930000-0000-7000-8000-000000000001")
        .join("1.json");
    assert_eq!(archive.path(&run, Attempt::FIRST), path);
    let document = rewind_format::decode_journal(&fs::read_to_string(&path).expect("read"))
        .expect("a journal document v1");
    assert_eq!(document, first.document());
    assert_eq!(
        fs::read_dir(root.join(".staging"))
            .expect("staging")
            .count(),
        0,
        "the staging file was renamed"
    );

    // The run's blobs live under it, the first two hex digits a directory of their own.
    let bytes = canonical_json(&json!({ "large": "x" })).into_bytes();
    let digest = Digest::of(&bytes);
    let owner = BlobOwner::Run(run.clone());
    blobs.put_in(&owner, digest, bytes).await.expect("put_in");
    let hex = digest.to_hex();
    let blob = root
        .join("blobs")
        .join("+acme")
        .join("01930000-0000-7000-8000-000000000001")
        .join("sha256")
        .join(&hex[..2])
        .join(&hex);
    assert_eq!(blobs.path_in(&owner, digest), blob);
    assert!(blob.exists());

    archive.erase(&run).await.expect("erase");
    assert!(
        !path.exists() && !blob.exists(),
        "the run's objects are gone"
    );
    assert!(
        !root
            .join("runs")
            .join("+acme")
            .join(run.id.to_string())
            .exists()
    );
}

#[tokio::test]
async fn a_corrupt_or_foreign_attempt_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let archive = FileArchive::new(FileBlobs::open(dir.path()).expect("open"));
    let run = run("acme");
    archive
        .put(&sealed(&run, Attempt::FIRST, "x"))
        .await
        .expect("put");
    let path = archive.path(&run, Attempt::FIRST);

    fs::write(&path, b"{\"v\":1}").expect("tamper");
    let read = archive.get(&run, Attempt::FIRST).await;
    assert!(matches!(read, Err(ArchiveError::Corrupt(_))), "{read:?}");

    let other = RunRef::new(
        tenant("acme"),
        "01930000-0000-7000-8000-000000000002"
            .parse()
            .expect("run id"),
    );
    fs::write(
        &path,
        rewind_format::encode_journal(&sealed(&other, Attempt::FIRST, "x").document()),
    )
    .expect("swap");
    let read = archive.get(&run, Attempt::FIRST).await;
    assert!(
        matches!(&read, Err(ArchiveError::Corrupt(message)) if message.contains("journal of run")),
        "another run's journal: {read:?}"
    );
}

#[tokio::test]
async fn the_cache_serves_what_it_read_and_forgets_an_erased_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let files = FileArchive::new(FileBlobs::open(dir.path()).expect("open"));
    let cached = CachedArchive::new(files.clone(), DEFAULT_ARCHIVE_CACHE_CAPACITY);
    let run = run("acme");
    let first = sealed(&run, Attempt::FIRST, "first");
    cached.put(&first).await.expect("put");
    assert_eq!(cached.weight(), 0, "a put is not kept: the first put stays");
    assert_eq!(
        cached.get(&run, Attempt::FIRST).await.expect("get"),
        Some(first.clone())
    );
    assert!(cached.weight() > 0);

    // Read from the cache, not the files.
    fs::remove_file(files.path(&run, Attempt::FIRST)).expect("remove");
    assert_eq!(
        cached.get(&run, Attempt::FIRST).await.expect("get"),
        Some(first)
    );
    cached.erase(&run).await.expect("erase");
    assert_eq!(cached.get(&run, Attempt::FIRST).await.expect("get"), None);

    // An attempt larger than a shard is served but not kept.
    let small = CachedArchive::new(files.clone(), 8 * 1024);
    let large = sealed(&run, Attempt::FIRST, &"x".repeat(64 * 1024));
    small.put(&large).await.expect("put");
    assert_eq!(
        small.get(&run, Attempt::FIRST).await.expect("get"),
        Some(large)
    );
    assert_eq!(small.weight(), 0);
}
