//! What the conformance suite cannot see from one store: fencing between two processes that
//! share a bucket (SlateDB's writer epoch, not the store's count of opens), commits racing on one
//! log, and a partition read back from its files by a new process.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use object_store::ObjectStore;
use object_store::memory::InMemory;
use rewind_core::{
    Digest, Epoch, PartitionId, RunId, RunMode, RunStarted, RunStatus, Seed, Timestamp,
};
use rewind_engine::ports::partition::CreateRun;
use rewind_engine::ports::{
    Attempt, Batch, Change, InboxItem, IndexedSummary, NamespaceName, NodeId, PartitionError,
    PartitionLog, PartitionStore, RunHead, RunMetadata, RunRef, RunSummary, TenantId, Trigger,
};
use rewind_store_slatedb::{Bucket, Options, SlatePartitions};
use serde_json::json;

const PARTITION: PartitionId = PartitionId::new(5);
const T0: Timestamp = Timestamp(1_767_225_600_000);

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn run(n: u64) -> RunRef {
    RunRef::new(
        TenantId::new("acme").unwrap(),
        RunId::in_partition(PARTITION, Timestamp(T0.0 + n), n),
    )
}

fn create(run: &RunRef) -> Change {
    let namespace = NamespaceName::new("alice").unwrap();
    let head = RunHead {
        run: run.clone(),
        namespace: namespace.clone(),
        origin: "script".into(),
        suffix_len: 12,
        code: Digest::of(b"script"),
        action_set: Digest::of(b"action_set"),
        status: RunStatus::Pending,
        epoch: Epoch(1),
        attempt: Attempt::FIRST,
        sealed: None,
        failures: 0,
        attempts: 0,
        quarantine: None,
        waits: Vec::new(),
        expires_at: Timestamp(T0.0 + 86_400_000),
        history_len: 0,
        metadata: RunMetadata::default(),
        idempotency_key: None,
        created_at: T0,
        updated_at: T0,
        ended_at: None,
    };
    let summary = IndexedSummary::new(RunSummary {
        run: run.clone(),
        namespace,
        origin: "script".into(),
        suffix_len: 12,
        code: head.code,
        headline: "export default async () => {".into(),
        action_set: head.action_set,
        status: RunStatus::Pending,
        mode: Some(RunMode::Mock),
        labels: BTreeMap::new(),
        parent: None,
        workflow: None,
        started_by: None,
        started_at: None,
        ended_at: None,
        last_activity_at: T0,
        calls: 0,
        errors: 0,
        history_len: 0,
        waits: Vec::new(),
        wakes_at: None,
        ending: None,
        quarantine: None,
    });
    CreateRun {
        run: run.clone(),
        head,
        start_key: None,
        events: Vec::new(),
        payloads: Vec::new(),
        summary,
    }
    .into()
}

fn started(id: u64) -> InboxItem {
    InboxItem {
        id,
        trigger: Trigger::RunStarted(RunStarted {
            code: Digest::of(b"script"),
            action_set: Digest::of(b"action_set"),
            input: json!({ "n": id }).into(),
            seed: Seed::from_bytes([1; 32]),
            epoch: Epoch(1),
            expires_at: Timestamp(T0.0 + 86_400_000),
            mode: RunMode::Mock,
        }),
        received_at: T0,
    }
}

fn store_on(objects: &Arc<dyn ObjectStore>) -> SlatePartitions {
    SlatePartitions::on(Arc::clone(objects), String::new(), Options::default())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_writer_of_another_process_is_fenced_by_the_writer_epoch() {
    let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    // Two stores: two processes sharing the bucket, each with its own count of opens.
    let (first_process, second_process) = (store_on(&objects), store_on(&objects));
    let first = first_process.open(PARTITION, &node("n1")).await.unwrap();
    first
        .commit(Batch::new().with(create(&run(1))))
        .await
        .unwrap();

    let second = second_process.open(PARTITION, &node("n2")).await.unwrap();
    assert!(
        second.run(&run(1)).await.unwrap().is_some(),
        "what was durable"
    );
    let fenced = first.commit(Batch::new().with(create(&run(2)))).await;
    assert_eq!(
        fenced,
        Err(PartitionError::Fenced {
            partition: PARTITION
        }),
        "SlateDB refuses the old writer's WAL"
    );
    assert_eq!(
        first.commit(Batch::new()).await,
        Err(PartitionError::Fenced {
            partition: PARTITION
        }),
        "and the log commits nothing more"
    );
    assert_eq!(
        second.run(&run(2)).await.unwrap(),
        None,
        "nothing of the fenced batch"
    );
    second
        .commit(Batch::new().with(create(&run(3))))
        .await
        .unwrap();
    let third = store_on(&objects)
        .open(PARTITION, &node("n3"))
        .await
        .unwrap();
    assert!(third.run(&run(3)).await.unwrap().is_some());
    assert_eq!(third.run(&run(2)).await.unwrap(), None);
}

/// A node whose open was under way when the partition moved to another node: its open ends
/// after the new owner's, and must fence neither the new writer nor its compactor. The new
/// owner opens at moments spread over the old node's whole open (its writer's claim, its WAL's
/// fence, its compactor's claim): whatever the old open did by then, the new owner goes on
/// committing once the old open ended, and the old open, if it ended at all, cannot commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_under_way_when_another_ends_fences_neither_the_new_writer_nor_its_compactor() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    use std::time::Duration;

    for offset in [150, 300, 450, 600, 750, 900, 1_050, 1_200, 1_400, 1_700] {
        let objects: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let before = store_on(&objects)
            .open(PARTITION, &node("n0"))
            .await
            .unwrap();
        before
            .commit(Batch::new().with(create(&run(1))))
            .await
            .unwrap();
        before.close().await.unwrap();

        // The old node reaches the bucket slowly: each of its calls waits 100 ms.
        let wait = Duration::from_millis(100);
        let slow: Arc<dyn ObjectStore> = Arc::new(ThrottledStore::new(
            Arc::clone(&objects),
            ThrottleConfig {
                wait_get_per_call: wait,
                wait_put_per_call: wait,
                wait_list_per_call: wait,
                ..ThrottleConfig::default()
            },
        ));
        let old_node = store_on(&slow);
        let late = tokio::spawn(async move { old_node.open(PARTITION, &node("n1")).await });
        tokio::time::sleep(Duration::from_millis(offset)).await;
        let owner = store_on(&objects)
            .open(PARTITION, &node("n2"))
            .await
            .unwrap();
        owner
            .commit(Batch::new().with(create(&run(2))))
            .await
            .unwrap();

        let late = late.await.unwrap();
        // What the old open's background tasks do once it ended has had time to happen.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let commit = owner.commit(Batch::new().with(create(&run(3)))).await;
        assert_eq!(
            commit,
            Ok(()),
            "the new owner, open {offset} ms into the old open, commits"
        );
        assert!(owner.run(&run(3)).await.unwrap().is_some());
        if let Ok(late) = late {
            assert!(
                late.commit(Batch::new().with(create(&run(4))))
                    .await
                    .is_err(),
                "the old open, {offset} ms ahead, commits nothing"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commits_racing_on_one_log_each_apply_whole() {
    let store = SlatePartitions::new(&Bucket::Memory, Options::default()).unwrap();
    let log = Arc::new(store.open(PARTITION, &node("n1")).await.unwrap());
    let runs: Vec<RunRef> = (1..=8).map(run).collect();
    log.commit(Batch {
        changes: runs.iter().map(create).collect(),
    })
    .await
    .unwrap();
    let mut racing = Vec::new();
    for item in 1..=16_u64 {
        for run in &runs {
            let (log, run) = (Arc::clone(&log), run.clone());
            racing.push(tokio::spawn(async move {
                log.commit(Batch::new().with(Change::Enqueue {
                    run,
                    item: started(item),
                    signal_key: Some(format!("key-{}", item % 4)),
                }))
                .await
            }));
        }
    }
    for commit in racing {
        commit.await.unwrap().unwrap();
    }
    for run in &runs {
        let inbox = log.inbox(run).await.unwrap();
        assert_eq!(inbox.len(), 4, "one item per signal key: {inbox:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_process_reads_the_partition_from_its_files() {
    let directory = tempfile::tempdir().unwrap();
    let bucket = Bucket::Local(directory.path().into());
    {
        let store = SlatePartitions::new(&bucket, Options::default()).unwrap();
        let log = store.open(PARTITION, &node("n1")).await.unwrap();
        log.commit(Batch::new().with(create(&run(1))).with(Change::Enqueue {
            run: run(1),
            item: started(1),
            signal_key: None,
        }))
        .await
        .unwrap();
        // The process dies: nothing is closed.
        std::mem::forget(log);
    }
    let store = SlatePartitions::new(&bucket, Options::default()).unwrap();
    let log = store.open(PARTITION, &node("n2")).await.unwrap();
    assert_eq!(log.inbox(&run(1)).await.unwrap(), [started(1)]);
    let recovery = log.recover(T0).await.unwrap();
    assert_eq!(recovery.runnable.len(), 1);
    assert!(
        directory.path().join("partitions/5").is_dir(),
        "the partition lives at partitions/{{id}}/"
    );
    log.close().await.unwrap();
}
