//! `migrate-runs` in the driver (`archi/08-decisions.md`, "Taken while writing the
//! object-storage spec": the migration): runs of the set-aside tables, recorded by a first plane,
//! moved into the partitions and the archive of a second one with several partitions, under
//! version 7 ids; then a node owns them, reads them back and lets the live one go on.

#![allow(clippy::unwrap_used)]

use crate::support;

use std::num::NonZeroU16;
use std::sync::Arc;

use rewind_core::{
    Command, Completion, Digest, Event, HistoryEntry, JsonValue, Payload, RunId, RunStatus,
    canonical_json,
};
use rewind_engine::adapters::{
    ControlPlaneChanges, MemoryArchive, MemoryBlobs, MemoryCatalog, MemoryCluster, MemoryLegacy,
    MemoryPartitions, MemorySecrets, MemorySpanExporter, MemoryWebhooks, SeededEntropy,
    VirtualClock,
};
use rewind_engine::driver::{Driver, PlaneClock};
use rewind_engine::edge::Edge;
use rewind_engine::migrate::{Migration, MovedAs};
use rewind_engine::plane::Wiring;
use rewind_engine::ports::legacy::{LegacyHistory, LegacyRun};
use rewind_engine::ports::{
    Attempt, BlobOwner, BlobStore, CatalogStore, Clock, Delivery, NamespaceName, NodeId,
    PayloadRecord, RunArchive, RunRef, TenantId, namespace_partition,
};
use serde_json::json;
use support::plane::{TestPlane, World, config};
use support::{FakeExecutor, HOUR, RecordingDispatcher, T0, request};

const BIG: usize = 70_000;

fn programs() -> FakeExecutor {
    FakeExecutor::new()
        .with(
            "ended",
            |run| {
                run.call("a", json!({ "text": "m".repeat(2_000) }));
            },
            |run, delivery| match delivery {
                Delivery::ToolCallCompleted { result, .. } => {
                    let size = result
                        .get("text")
                        .and_then(JsonValue::as_str)
                        .map_or(0, str::len);
                    run.complete(json!({ "size": size }));
                }
                _ => run.fail("Error", "unexpected"),
            },
        )
        .with(
            "waiting",
            |run| {
                run.await_signal("go");
                run.timer(HOUR);
            },
            |run, delivery| {
                if let Delivery::SignalReceived { payload, .. } = delivery {
                    run.complete(payload.clone());
                }
            },
        )
}

/// `a` answers with a value above 64 KiB.
fn dispatcher() -> RecordingDispatcher {
    RecordingDispatcher::new(|_| Ok(json!({ "text": "b".repeat(BIG) })))
}

fn payload(event: &Event) -> Option<Payload> {
    match event {
        Event::Command(Command::ScheduleToolCall { args, .. }) => Some(args.clone()),
        Event::Completion(Completion::ToolCallCompleted { result, .. }) => Some(result.clone()),
        _ => None,
    }
}

/// The digests of the payloads that left the entries.
fn digests(entries: &[HistoryEntry]) -> Vec<Digest> {
    entries
        .iter()
        .filter_map(|entry| match payload(&entry.event)? {
            Payload::Digest(digest) => Some(digest),
            Payload::Value(_) => None,
        })
        .collect()
}

/// A plane of `partitions` partitions without a node yet.
fn plane(partitions: u16) -> (Arc<TestPlane>, MemoryCluster) {
    let changes = ControlPlaneChanges::new();
    let cluster = MemoryCluster::with_changes(partitions, changes.clone());
    let blobs = MemoryBlobs::new();
    let plane = Arc::new(Wiring {
        catalog: MemoryCatalog::with_changes(changes.clone()),
        partitions: MemoryPartitions::new(),
        archive: MemoryArchive::with_blobs(blobs.clone()),
        blobs,
        executor: programs(),
        dispatcher: dispatcher(),
        secrets: MemorySecrets::with_changes(changes),
        spans: MemorySpanExporter::new(),
        webhooks: MemoryWebhooks::new(),
        clock: VirtualClock::new(T0),
        entropy: SeededEntropy::new(42),
    });
    (plane, cluster)
}

/// A version 7 id, as the journal store drew them.
fn v7(n: u128) -> RunId {
    RunId::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0000 | n)
}

#[tokio::test]
async fn ended_and_live_runs_move_into_their_partitions_once_and_go_on() {
    // The runs, as the tables kept them.
    let mut source = World::new(programs(), dispatcher()).await;
    let ended = source.run(request("ended", json!({}))).await;
    let waiting = source.run(request("waiting", json!({}))).await;
    let legacy = MemoryLegacy::new();
    let tenant = TenantId::local();
    let mut moved = Vec::new();
    let mut tables = Vec::new();
    for (run, id, namespace) in [(&ended, v7(1), "alice"), (&waiting, v7(2), "bob")] {
        let mut gateway = source.driver.gateway(&source.at);
        let digests = source
            .edge
            .history(&mut gateway, run, 0, false)
            .await
            .unwrap()
            .unwrap();
        let values = source.history(run).await;
        let view = source.view(run).await;
        let mut records = Vec::new();
        for digest in self::digests(&digests) {
            let bytes = source
                .plane
                .blobs
                .get(&BlobOwner::Run(run.clone()), digest)
                .await
                .unwrap()
                .unwrap_or_else(|| {
                    let value = values
                        .iter()
                        .zip(&digests)
                        .find_map(|(value, entry)| {
                            match (payload(&value.event)?, payload(&entry.event)?) {
                                (Payload::Value(value), Payload::Digest(found))
                                    if found == digest =>
                                {
                                    Some(value)
                                }
                                _ => None,
                            }
                        })
                        .unwrap();
                    canonical_json(&value).into_bytes()
                });
            let value: JsonValue = serde_json::from_slice(&bytes).unwrap();
            let table = bytes.len() <= 64 * 1024;
            records.push(PayloadRecord {
                digest,
                size: bytes.len() as u64,
                preview: "{\"text\":".into(),
                value: table.then_some(value),
            });
            if !table {
                legacy.put_payload(&tenant, digest, bytes);
            }
        }
        let head = view.head;
        let timers = digests
            .iter()
            .filter_map(|entry| match &entry.event {
                Event::Command(Command::StartTimer { seq, fire_at, .. }) => Some((*seq, *fire_at)),
                _ => None,
            })
            .collect();
        let record = LegacyRun {
            run: RunRef::new(tenant.clone(), id),
            namespace: NamespaceName::new(namespace).unwrap(),
            origin: "triage".into(),
            suffix_len: 13,
            script: head.script,
            toolset: head.toolset,
            epoch: head.epoch,
            expires_at: head.expires_at,
            expiry_enqueued: false,
            quarantine: None,
            failures: 0,
            attempts: 0,
            metadata: head.metadata.clone(),
            idempotency_key: Some(format!("start-{namespace}")),
            created_at: head.created_at,
            updated_at: head.updated_at,
            ended_at: head.ended_at,
            history: LegacyHistory::Entries(digests),
            inbox: Vec::new(),
            signal_keys: if head.ended_at.is_none() {
                vec!["key-1".into()]
            } else {
                Vec::new()
            },
            timers,
            tasks: Vec::new(),
            webhooks: Vec::new(),
            watchers: Vec::new(),
            payloads: records,
        };
        legacy.put_run(record.clone());
        tables.push(record);
        moved.push((RunRef::new(tenant.clone(), id), values));
    }

    // Moved into a plane of four partitions, its scripts and toolsets in its catalog.
    let (plane, cluster) = plane(4);
    for run in [&ended, &waiting] {
        let head = source.view(run).await.head;
        let script = source
            .plane
            .catalog
            .get_script(&tenant, head.script)
            .await
            .unwrap()
            .unwrap();
        plane
            .catalog
            .put_script(&tenant, &script.script, &script.compiled)
            .await
            .unwrap();
        let toolset = source
            .plane
            .catalog
            .get_toolset(&tenant, head.toolset)
            .await
            .unwrap()
            .unwrap();
        plane
            .catalog
            .put_toolset(&tenant, &toolset.toolset, &toolset.declarations)
            .await
            .unwrap();
    }
    let partitions = NonZeroU16::new(4).unwrap();
    let migration = Migration {
        catalog: &plane.catalog,
        partitions: &plane.partitions,
        archive: &plane.archive,
        blobs: &plane.blobs,
        legacy: &legacy,
        legacy_blobs: &legacy,
        partition_count: partitions,
        node: NodeId::new("migrate").unwrap(),
        now: plane.clock.now(),
    };
    let report = migration.run().await.unwrap();
    let how: Vec<_> = report.moved.iter().map(|moved| moved.how).collect();
    assert_eq!(how, [MovedAs::Ended, MovedAs::Live]);
    assert!(report.failed.is_empty() && report.skipped.is_empty());
    assert!(report.dropped && legacy.is_dropped());
    for moved in &report.moved {
        let namespace = if moved.run.id == v7(1) {
            "alice"
        } else {
            "bob"
        };
        let partition =
            namespace_partition(&tenant, &NamespaceName::new(namespace).unwrap(), partitions);
        assert_eq!(moved.partition, partition);
        assert_eq!(
            plane.catalog.legacy_partition(moved.run.id).await.unwrap(),
            Some(partition),
            "the router finds a version 7 id"
        );
    }

    // The ended run's attempt is sealed: its medium payload inlined, its big one a blob.
    let ended_ref = &moved[0].0;
    let sealed = plane
        .archive
        .get(ended_ref, Attempt::FIRST)
        .await
        .unwrap()
        .unwrap();
    let big = digests(&sealed.entries);
    assert_eq!(big.len(), 1, "only the value above 64 KiB stays a digest");
    assert!(
        plane
            .blobs
            .get(&BlobOwner::Run(ended_ref.clone()), big[0])
            .await
            .unwrap()
            .is_some(),
        "the blob is under the run"
    );

    // Run again on what is left of the tables, or on tables still holding the moved runs:
    // nothing moves twice.
    assert_eq!(migration.run().await.unwrap().moved, []);
    let again = MemoryLegacy::new();
    for record in tables {
        again.put_run(record);
    }
    let report = Migration {
        legacy: &again,
        legacy_blobs: &legacy,
        ..migration
    }
    .run()
    .await
    .unwrap();
    assert_eq!(report.moved, []);
    assert_eq!(report.skipped.len(), 2, "{report:?}");
    assert!(report.dropped);

    // A node owns the partitions: the runs read back the same, and the live one goes on.
    let mut driver = Driver::new(cluster, Arc::new(PlaneClock(Arc::clone(&plane))));
    let node = config("node-b", 4);
    driver
        .add_node(Arc::clone(&plane), node.clone())
        .await
        .unwrap();
    driver.run_until_idle().await.unwrap();
    let edge = Edge::new(Arc::clone(&plane), node.clone());
    for (run, values) in &moved {
        let mut gateway = driver.gateway(&node.node);
        let history = edge
            .history(&mut gateway, run, 0, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&history, values, "{run} reads back the same");
    }
    let mut gateway = driver.gateway(&node.node);
    let view = edge
        .get_run(&mut gateway, ended_ref)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(view.head.status, RunStatus::Completed);
    let waiting_ref = &moved[1].0;
    let mut gateway = driver.gateway(&node.node);
    assert_eq!(
        edge.signal(
            &mut gateway,
            waiting_ref,
            "go",
            json!({ "again": true }),
            Some("key-1".into())
        )
        .await
        .unwrap(),
        rewind_engine::ports::Enqueued::Duplicate,
        "its signal keys moved with it"
    );
    let mut gateway = driver.gateway(&node.node);
    edge.signal(
        &mut gateway,
        waiting_ref,
        "go",
        json!({ "done": true }),
        None,
    )
    .await
    .unwrap();
    driver.run_until_idle().await.unwrap();
    let mut gateway = driver.gateway(&node.node);
    let view = edge
        .get_run(&mut gateway, waiting_ref)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(view.head.status, RunStatus::Completed);
}
