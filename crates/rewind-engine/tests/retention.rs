//! Retention and deletion: archives, deletion, payload cleanup and the read model's retention
//! (`archi/06-storage-and-scale.md`, "Retention and deletion").

#![allow(clippy::unwrap_used)]

mod support;

use rewind_core::{Digest, JournalDocument, Payload, RunStatus, Timestamp};
use rewind_engine::adapters::{MemoryReadModel, MemorySpanExporter, MemoryWebhooks};
use rewind_engine::projector::{Projector, ProjectorConfig, run_snapshot};

use rewind_engine::ports::{
    BlobStore, Clock, JournalStore, ProjectedRun, ReadModel, RunOrder, RunQuery, RunRef,
    StoreError, VirtualTime,
};
use rewind_engine::retention::Housekeeper;
use rewind_engine::{DriveOptions, EngineConfig, PayloadTiers};
use serde_json::json;
use support::*;

/// Completes with its input, once `a` echoed it.
fn echo() -> FakeExecutor {
    FakeExecutor::new().with(
        "echo",
        |run| {
            let input = run.input.clone();
            run.call("a", input);
        },
        |run, delivery| run.complete(result_of(delivery).clone()),
    )
}

/// The archive's bytes in tests: what identifies the document, not the stored format.
fn encode(document: &JournalDocument) -> String {
    format!(
        "{} {} {} {}",
        document.run,
        document.script.digest(),
        document.toolset.digest(),
        document.entries.len()
    )
}

async fn finished_run(engine: &TestEngine, input: serde_json::Value) -> RunRef {
    let run = engine.start_run(request("echo", input)).await.unwrap().run;
    engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    assert_eq!(
        engine.get_run(&run).await.unwrap().unwrap().status,
        RunStatus::Completed
    );
    run
}

fn later(engine: &TestEngine, days: u64) {
    let at = Timestamp(engine.clock().now().0 + days * DAY);
    engine.clock().advance_to(at);
}

fn tiny_tiers() -> EngineConfig {
    // Every payload above 8 bytes goes to the blob store.
    EngineConfig {
        payloads: PayloadTiers {
            inline: 8,
            table: 8,
        },
        ..EngineConfig::default()
    }
}

#[tokio::test]
async fn finished_runs_are_archived_then_deleted() {
    let engine = engine_with(echo(), RecordingDispatcher::echo(), tiny_tiers());
    let read_model = MemoryReadModel::new();
    let housekeeper = Housekeeper {
        journal: engine.journal(),
        blobs: engine.blobs(),
        read_model: &read_model,
        secrets: engine.secrets(),
        batch: 100,
    };
    let run = finished_run(&engine, json!({ "text": "a payload of the run" })).await;
    let blobs_before = engine.blobs().len();

    // Not before its journal retention (30 days).
    later(&engine, 29);
    assert_eq!(
        housekeeper
            .archive(engine.clock().now(), encode)
            .await
            .unwrap(),
        0
    );
    later(&engine, 2);
    // The projector has not applied the run's entries yet: its history is still needed.
    assert_eq!(
        housekeeper
            .archive(engine.clock().now(), encode)
            .await
            .unwrap(),
        0
    );
    let projector = Projector {
        journal: engine.journal(),
        read_model: &read_model,
        exporter: &MemorySpanExporter::new(),
        webhooks: &MemoryWebhooks::new(),
        config: ProjectorConfig::default(),
    };
    assert!(projector.project_all(engine.clock().now()).await.unwrap() > 0);
    assert_eq!(
        housekeeper
            .archive(engine.clock().now(), encode)
            .await
            .unwrap(),
        1
    );
    let record = engine.get_run(&run).await.unwrap().unwrap();
    let archive = record.archive.unwrap();
    assert!(matches!(
        engine.history(&run).await,
        Err(rewind_engine::EngineError::Store(StoreError::Archived(_)))
    ));
    let bytes = housekeeper.load_archive(&run).await.unwrap().unwrap();
    assert_eq!(Digest::of(&bytes), archive.document);
    assert!(
        String::from_utf8(bytes)
            .unwrap()
            .starts_with(&run.id.to_string())
    );
    // The payloads stay: the archive references them.
    assert_eq!(engine.blobs().len(), blobs_before + 1);
    assert_eq!(
        housekeeper
            .clean_payloads(engine.clock().now())
            .await
            .unwrap(),
        0
    );

    // An archive is kept a year, then the run is deleted with it.
    later(&engine, 364);
    assert_eq!(
        housekeeper
            .delete_expired(engine.clock().now())
            .await
            .unwrap(),
        0
    );
    later(&engine, 2);
    assert_eq!(
        housekeeper
            .delete_expired(engine.clock().now())
            .await
            .unwrap(),
        1
    );
    assert_eq!(engine.get_run(&run).await.unwrap(), None);
    assert_eq!(
        engine
            .blobs()
            .get(&run.tenant, archive.document)
            .await
            .unwrap(),
        None
    );

    // Its payloads, now unreferenced, are deleted after their grace.
    assert_eq!(
        housekeeper
            .clean_payloads(engine.clock().now())
            .await
            .unwrap(),
        0
    );
    later(&engine, 1);
    assert_eq!(
        housekeeper
            .clean_payloads(engine.clock().now())
            .await
            .unwrap(),
        1
    );
    let digest = Payload::digest_of(&json!({ "text": "a payload of the run" }));
    assert_eq!(engine.blobs().get(&run.tenant, digest).await.unwrap(), None);
    assert_eq!(
        engine
            .journal()
            .get_payload(&run.tenant, digest)
            .await
            .unwrap(),
        None
    );
    assert_eq!(engine.blobs().len(), 0);
}

#[tokio::test]
async fn a_run_is_erased_at_any_time_with_its_read_model_rows() {
    let engine = engine_with(echo(), RecordingDispatcher::echo(), tiny_tiers());
    let read_model = MemoryReadModel::new();
    let housekeeper = Housekeeper {
        journal: engine.journal(),
        blobs: engine.blobs(),
        read_model: &read_model,
        secrets: engine.secrets(),
        batch: 100,
    };
    let shared = json!({ "text": "shared by two runs" });
    let erased = finished_run(&engine, shared.clone()).await;
    let kept = finished_run(&engine, shared.clone()).await;
    for run in [&erased, &kept] {
        let record = engine.get_run(run).await.unwrap().unwrap();
        let history = engine.history(run).await.unwrap();
        read_model
            .apply(
                &ProjectedRun {
                    run: run.clone(),
                    namespace: record.namespace.clone(),
                    script: record.script,
                    headline: String::new(),
                    toolset: record.toolset,
                    labels: record.metadata.labels,
                    parent: None,
                    workflow: None,
                    started_by: None,
                },
                &history,
                &run_snapshot(&history, None),
            )
            .await
            .unwrap();
    }

    assert!(
        housekeeper
            .delete_run(&erased, engine.clock().now())
            .await
            .unwrap()
    );
    assert!(
        !housekeeper
            .delete_run(&erased, engine.clock().now())
            .await
            .unwrap()
    );
    assert_eq!(engine.get_run(&erased).await.unwrap(), None);
    let left = read_model
        .search_runs(
            &RunQuery::new(erased.tenant.clone()),
            RunOrder::Started,
            None,
            10,
        )
        .await
        .unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].run, kept);

    // The payload is still referenced by the other run: it stays.
    later(&engine, 2);
    assert_eq!(
        housekeeper
            .clean_payloads(engine.clock().now())
            .await
            .unwrap(),
        0
    );
    assert!(
        engine
            .blobs()
            .get(&kept.tenant, Payload::digest_of(&shared))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn the_read_model_keeps_runs_for_its_retention() {
    let engine = engine(echo(), RecordingDispatcher::echo());
    let read_model = MemoryReadModel::new();
    let housekeeper = Housekeeper {
        journal: engine.journal(),
        blobs: engine.blobs(),
        read_model: &read_model,
        secrets: engine.secrets(),
        batch: 100,
    };
    let run = finished_run(&engine, json!(1)).await;
    let record = engine.get_run(&run).await.unwrap().unwrap();
    let history = engine.history(&run).await.unwrap();
    read_model
        .apply(
            &ProjectedRun {
                run: run.clone(),
                namespace: record.namespace.clone(),
                script: record.script,
                headline: String::new(),
                toolset: record.toolset,
                labels: record.metadata.labels,
                parent: None,
                workflow: None,
                started_by: None,
            },
            &history,
            &run_snapshot(&history, None),
        )
        .await
        .unwrap();
    later(&engine, 89);
    assert_eq!(
        housekeeper
            .purge_read_model(engine.clock().now())
            .await
            .unwrap(),
        0
    );
    later(&engine, 2);
    assert_eq!(
        housekeeper
            .purge_read_model(engine.clock().now())
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn unreferenced_tests_go_after_their_grace_and_recorded_ones_with_their_namespace() {
    use rewind_core::{Test, TestOrigin};
    use rewind_engine::ports::{NamespaceName, TEST_GRACE, Tenant, TenantId};

    let engine = engine(echo(), RecordingDispatcher::echo());
    let read_model = MemoryReadModel::new();
    let housekeeper = Housekeeper {
        journal: engine.journal(),
        blobs: engine.blobs(),
        read_model: &read_model,
        secrets: engine.secrets(),
        batch: 100,
    };
    let tenant = TenantId::local();
    let mut recorded = Test::new("recorded");
    recorded.origin = Some(TestOrigin {
        run: rewind_core::RunId::from_u128(9),
        version: Digest::of(b"v1"),
        namespace: "alice".into(),
    });
    let unreferenced = Test::new("never saved");
    let digests = engine
        .store_tests(&tenant, &[recorded, unreferenced], test_format())
        .await
        .unwrap();
    assert_eq!(engine.blobs().len(), 2);

    // Within their grace, both stay.
    assert_eq!(housekeeper.clean_tests(T0).await.unwrap(), 0);
    // After it, both go: no version references them.
    let after = T0.saturating_add(TEST_GRACE);
    assert_eq!(housekeeper.clean_tests(after).await.unwrap(), 2);
    assert_eq!(engine.blobs().len(), 0);
    assert!(
        engine
            .journal()
            .get_test(&tenant, digests[0])
            .await
            .unwrap()
            .is_none()
    );

    // A recorded test goes with its namespace.
    let digests = engine
        .store_tests(
            &tenant,
            &[{
                let mut test = Test::new("recorded again");
                test.origin = Some(TestOrigin {
                    run: rewind_core::RunId::from_u128(10),
                    version: Digest::of(b"v1"),
                    namespace: "alice".into(),
                });
                test
            }],
            test_format(),
        )
        .await
        .unwrap();
    engine
        .journal()
        .put_tenant(&Tenant::new(tenant.clone()))
        .await
        .unwrap();
    let alice = NamespaceName::new("alice").unwrap();
    engine
        .journal()
        .put_namespace(&tenant, &alice, None, T0)
        .await
        .unwrap();
    engine
        .journal()
        .delete_namespace(&tenant, &alice, T0)
        .await
        .unwrap();
    let erased = housekeeper.erase_namespaces(T0).await.unwrap();
    assert_eq!(erased.tests, 1);
    assert!(
        engine
            .journal()
            .get_test(&tenant, digests[0])
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(engine.blobs().len(), 0);
}
