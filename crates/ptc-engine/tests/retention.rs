//! Retention and deletion: archives, deletion, payload cleanup and the read model's retention
//! (`archi/06-storage-and-scale.md`, "Retention and deletion").

#![allow(clippy::unwrap_used)]

mod support;

use ptc_core::{Digest, JournalDocument, Payload, RunStatus, Timestamp};
use ptc_engine::adapters::{MemoryReadModel, MemorySpanExporter, MemoryWebhooks};
use ptc_engine::projector::{Projector, ProjectorConfig};
use std::collections::BTreeMap;

use ptc_engine::ports::{
    BlobStore, Clock, Credentials, JournalStore, ProjectedRun, ReadModel, RunRef, RunSearch,
    Secret, SecretOwner, SecretStore, ServerCredentials, StoreError, VirtualTime,
};
use ptc_engine::retention::Housekeeper;
use ptc_engine::{DriveOptions, EngineConfig, PayloadTiers};
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
        Err(ptc_engine::EngineError::Store(StoreError::Archived(_)))
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

fn credentials(token: &str) -> Credentials {
    Credentials {
        servers: BTreeMap::from([(
            "github".to_owned(),
            ServerCredentials {
                headers: BTreeMap::from([("Authorization".to_owned(), Secret::new(token))]),
                env: BTreeMap::new(),
            },
        )]),
    }
}

#[tokio::test]
async fn a_run_is_erased_at_any_time_with_its_read_model_rows_and_credentials() {
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
        engine
            .secrets()
            .put(
                &run.tenant,
                &SecretOwner::Run(run.id),
                &credentials(&run.id.to_string()),
            )
            .await
            .unwrap();
        let history = engine.history(run).await.unwrap();
        read_model
            .apply(
                &ProjectedRun {
                    run: run.clone(),
                    script: record.script,
                    toolset: record.toolset,
                    labels: record.metadata.labels,
                },
                &history,
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
    let toolset = engine.get_run(&kept).await.unwrap().unwrap().toolset;
    let secrets = engine.secrets();
    assert_eq!(
        secrets
            .resolve(&erased.tenant, erased.id, toolset)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        secrets
            .resolve(&kept.tenant, kept.id, toolset)
            .await
            .unwrap(),
        Some(credentials(&kept.id.to_string()))
    );
    let left = read_model
        .search_runs(&RunSearch::new(erased.tenant.clone(), 10))
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
    read_model
        .apply(
            &ProjectedRun {
                run: run.clone(),
                script: record.script,
                toolset: record.toolset,
                labels: record.metadata.labels,
            },
            &engine.history(&run).await.unwrap(),
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
