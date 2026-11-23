//! `SlatePartitions` passes the conformance suite of `PartitionStore`, on SlateDB over the
//! in-memory object store and over local files.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;

use rewind_engine::conformance::partition;
use rewind_store_slatedb::{Bucket, Options, SlatePartitions};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slatedb_in_memory_passes_the_partition_suite() {
    partition::run_all(|| SlatePartitions::new(&Bucket::Memory, Options::default()).unwrap()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slatedb_on_local_files_passes_the_partition_suite() {
    let directories = Mutex::new(Vec::new());
    partition::run_all(|| {
        let directory = tempfile::tempdir().unwrap();
        let store =
            SlatePartitions::new(&Bucket::Local(directory.path().into()), Options::default())
                .unwrap();
        directories.lock().unwrap().push(directory);
        store
    })
    .await;
}
