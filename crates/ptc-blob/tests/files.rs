//! `FileBlobs`: the conformance suite of `BlobStore`, the layout on disk, atomic writes and
//! corruption detection.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ptc_blob::{Error, FileBlobs};
use ptc_core::{Digest, canonical_json};
use ptc_engine::conformance::{blobs, tenant};
use ptc_engine::ports::{BlobError, BlobStore};
use serde_json::json;

fn blob(text: &str) -> (Digest, Vec<u8>) {
    let bytes = canonical_json(&json!({ "text": text })).into_bytes();
    (Digest::of(&bytes), bytes)
}

#[tokio::test]
async fn file_blobs_pass_the_conformance_suite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stores = AtomicUsize::new(0);
    blobs::run_all(|| {
        let n = stores.fetch_add(1, Ordering::Relaxed);
        FileBlobs::open(dir.path().join(format!("blobs-{n}"))).expect("open")
    })
    .await;
}

#[tokio::test]
async fn a_blob_lives_under_its_tenant_and_digest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("blobs");
    let store = FileBlobs::open(&root).expect("open");
    let (digest, bytes) = blob("layout");
    let hex = digest.to_hex();

    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put");

    let path = root.join("acme").join("sha256").join(&hex[..2]).join(&hex);
    assert_eq!(store.path(&tenant("acme"), digest), path);
    assert_eq!(fs::read(&path).expect("read"), bytes);
    let staged = fs::read_dir(root.join(".staging"))
        .expect("staging")
        .count();
    assert_eq!(staged, 0, "the staging file was renamed");
}

#[tokio::test]
async fn a_corrupt_file_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FileBlobs::open(dir.path()).expect("open");
    let (digest, bytes) = blob("corrupt");
    store
        .put(&tenant("acme"), digest, bytes)
        .await
        .expect("put");

    fs::write(
        store.path(&tenant("acme"), digest),
        b"{\"text\":\"tampered\"}",
    )
    .expect("tamper");

    let read = store.get(&tenant("acme"), digest).await;
    assert!(
        matches!(read, Err(BlobError::Corrupt { digest: d, .. }) if d == digest),
        "{read:?}"
    );
}

#[tokio::test]
async fn a_second_put_leaves_the_file_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FileBlobs::open(dir.path()).expect("open");
    let (digest, bytes) = blob("immutable");
    store
        .put(&tenant("acme"), digest, bytes.clone())
        .await
        .expect("put");
    let path = store.path(&tenant("acme"), digest);
    let written = fs::metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime");

    store
        .put(&tenant("acme"), digest, bytes)
        .await
        .expect("put again");

    let after = fs::metadata(&path)
        .expect("metadata")
        .modified()
        .expect("mtime");
    assert_eq!(written, after, "the file was not rewritten");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_puts_of_one_blob_all_succeed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Arc::new(FileBlobs::open(dir.path()).expect("open"));
    let (digest, bytes) = blob(&"concurrent".repeat(10_000));

    let puts: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            let bytes = bytes.clone();
            tokio::spawn(async move { store.put(&tenant("acme"), digest, bytes).await })
        })
        .collect();
    for put in puts {
        put.await.expect("join").expect("put");
    }

    assert_eq!(
        store.get(&tenant("acme"), digest).await.expect("get"),
        Some(bytes)
    );
    let staged = fs::read_dir(dir.path().join(".staging"))
        .expect("staging")
        .count();
    assert_eq!(staged, 0, "the losers' staging files were removed");
}

#[test]
fn a_root_that_is_a_file_cannot_be_opened() {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("not-a-directory");
    fs::write(&file, b"").expect("write");

    let opened = FileBlobs::open(&file);

    assert!(
        matches!(&opened, Err(Error::Directory { path, .. }) if *path == file),
        "{opened:?}"
    );
}
