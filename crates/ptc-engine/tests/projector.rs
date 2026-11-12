//! The projector: the feed's consumers fill the read model, export spans and logs, and send
//! lifecycle webhooks, idempotently (`archi/05-testing-and-observability.md`).

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::{BTreeMap, BTreeSet};

use ptc_core::{RunMode, RunStatus, Seq};
use ptc_engine::DriveOptions;
use ptc_engine::adapters::{MemoryReadModel, MemorySpanExporter, MemoryWebhooks};
use ptc_engine::ports::{
    Delivery, FeedConsumer, JournalStore, ReadModel, RunMetadata, RunRef, RunSearch, Secret,
    SpanId, SpanStatus, Subscription, TraceContext, TraceId, WebhookEventKind,
};
use ptc_engine::projector::{Projector, ProjectorConfig};
use ptc_engine::telemetry::{SpanKey, span_id};
use serde_json::json;
use support::*;

/// Calls `a`, logs, then waits for `go` and completes.
fn programs() -> FakeExecutor {
    FakeExecutor::new().with(
        "flow",
        |run| {
            run.call("a", json!({ "n": 1 }));
        },
        |run, delivery| match delivery {
            Delivery::ToolCallCompleted { .. } => {
                run.log("a answered");
                run.await_signal("go");
            }
            Delivery::SignalReceived { .. } => run.complete(json!({ "done": true })),
            _ => {}
        },
    )
}

struct Sinks {
    read_model: MemoryReadModel,
    exporter: MemorySpanExporter,
    webhooks: MemoryWebhooks,
}

impl Sinks {
    fn new() -> Self {
        Self {
            read_model: MemoryReadModel::new(),
            exporter: MemorySpanExporter::new(),
            webhooks: MemoryWebhooks::new(),
        }
    }

    fn projector<'a>(
        &'a self,
        engine: &'a TestEngine,
        config: ProjectorConfig,
    ) -> Projector<
        'a,
        ptc_engine::adapters::MemoryJournal,
        MemoryReadModel,
        MemorySpanExporter,
        MemoryWebhooks,
    > {
        Projector {
            journal: engine.journal(),
            read_model: &self.read_model,
            exporter: &self.exporter,
            webhooks: &self.webhooks,
            config,
        }
    }
}

fn now(engine: &TestEngine) -> ptc_core::Timestamp {
    ptc_engine::ports::Clock::now(engine.clock())
}

async fn drive(engine: &TestEngine) {
    let report = engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    assert!(report.failures.is_empty() && !report.interrupted);
}

async fn subscribe(engine: &TestEngine) {
    engine
        .journal()
        .put_subscription(&Subscription {
            tenant: ptc_engine::ports::TenantId::local(),
            id: "sub".into(),
            url: "https://harness.example.com/hooks".into(),
            secret: Secret::new("whsec_x"),
            events: BTreeSet::new(),
            labels: BTreeMap::new(),
        })
        .await
        .unwrap();
}

async fn start(engine: &TestEngine, metadata: RunMetadata) -> RunRef {
    let mut request = request("flow", json!({}));
    request.metadata = metadata;
    engine.start_run(request).await.unwrap().run
}

#[tokio::test]
async fn committed_entries_are_projected_once_whatever_the_deliveries() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    subscribe(&engine).await;
    let sinks = Sinks::new();
    let projector = sinks.projector(&engine, ProjectorConfig::default());
    let run = start(&engine, RunMetadata::default()).await;
    drive(&engine).await;

    // Suspended twice: waiting for the host tool `a`, then for the signal.
    let applied = projector.project_all(now(&engine)).await.unwrap();
    assert_eq!(applied, 6, "two activations, three consumers");
    let deliveries = sinks.webhooks.deliveries();
    let suspended: Vec<(&str, Vec<String>)> = deliveries
        .iter()
        .map(|(_, event)| match &event.kind {
            WebhookEventKind::RunSuspended { waiting_for } => {
                (event.id.as_str(), waiting_for.clone())
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        suspended,
        [
            (
                format!("{}/1", run.id).as_str(),
                vec!["tool a #1".to_owned()]
            ),
            (
                format!("{}/4", run.id).as_str(),
                vec!["signal \"go\"".to_owned()]
            )
        ]
    );

    engine.signal(&run, "go", json!(null), None).await.unwrap();
    drive(&engine).await;
    projector.project_all(now(&engine)).await.unwrap();

    // The read model.
    let summaries = sinks
        .read_model
        .search_runs(&RunSearch::new(run.tenant.clone(), 10))
        .await
        .unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].status, RunStatus::Completed);
    assert_eq!(summaries[0].tool_calls, 1);
    assert_eq!(summaries[0].history_len, 7);

    // The spans: the run, as a root of the run's own trace, and its tool call under it.
    let trace = TraceId(*run.id.as_uuid().as_bytes());
    let spans = sinks.exporter.spans();
    let names: BTreeSet<&str> = spans.iter().map(|span| span.name.as_str()).collect();
    assert_eq!(names, BTreeSet::from(["a", "run", "signal go"]));
    let root = spans.iter().find(|span| span.name == "run").unwrap();
    assert_eq!(root.span_id, span_id(run.id, SpanKey::Run));
    assert_eq!((root.trace_id, root.parent), (trace, None));
    assert_eq!(root.status, SpanStatus::Ok);
    let call = spans.iter().find(|span| span.name == "a").unwrap();
    assert_eq!(call.span_id, span_id(run.id, SpanKey::Command(Seq(1))));
    assert_eq!(call.parent, Some(root.span_id));
    assert!(call.attributes.contains_key("ptc.args.digest"));
    // The log, under the activation that committed it.
    let logs = sinks.exporter.logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "a answered");
    assert!(logs[0].span_id.is_some());

    // Lifecycle: run.completed, named by the entry.
    let deliveries = sinks.webhooks.deliveries();
    assert_eq!(deliveries.len(), 3);
    assert!(matches!(
        deliveries[2].1.kind,
        WebhookEventKind::RunCompleted { .. }
    ));
    assert_eq!(deliveries[2].1.id, format!("{}/6", run.id));

    // Every feed item is acknowledged; projecting the whole history again changes nothing.
    for consumer in FeedConsumer::ALL {
        let left = engine
            .journal()
            .claim_feed(
                consumer,
                "x",
                now(&engine),
                std::time::Duration::from_secs(1),
                10,
            )
            .await
            .unwrap();
        assert!(left.is_empty());
    }
    let history = engine.history(&run).await.unwrap();
    let record = engine.get_run(&run).await.unwrap().unwrap();
    sinks
        .read_model
        .apply(
            &ptc_engine::ports::ProjectedRun {
                run: run.clone(),
                script: record.script,
                toolset: record.toolset,
                labels: record.metadata.labels.clone(),
            },
            &history,
        )
        .await
        .unwrap();
    let again = sinks
        .read_model
        .search_runs(&RunSearch::new(run.tenant.clone(), 10))
        .await
        .unwrap();
    assert_eq!(again, summaries);
    assert_eq!(sinks.exporter.spans().len(), spans.len());
}

#[tokio::test]
async fn a_run_started_with_a_traceparent_joins_the_harness_s_trace() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    let sinks = Sinks::new();
    let mut config = ProjectorConfig::default();
    config.telemetry.exported_labels = BTreeSet::from(["agent".to_owned()]);
    let projector = sinks.projector(&engine, config);
    let run = start(
        &engine,
        RunMetadata {
            labels: BTreeMap::from([
                ("agent".to_owned(), "triage".to_owned()),
                ("user".to_owned(), "alice@example.com".to_owned()),
            ]),
            trace_context: Some(TraceContext {
                traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
                tracestate: None,
            }),
            parent: None,
        },
    )
    .await;
    drive(&engine).await;
    engine.signal(&run, "go", json!(null), None).await.unwrap();
    drive(&engine).await;
    projector.project_all(now(&engine)).await.unwrap();

    let spans = sinks.exporter.spans();
    let root = spans.iter().find(|span| span.name == "run").unwrap();
    assert_eq!(
        root.trace_id.to_string(),
        "0af7651916cd43dd8448eb211c80319c"
    );
    assert_eq!(
        root.parent,
        Some(SpanId([0xb7, 0xad, 0x6b, 0x71, 0x69, 0x20, 0x33, 0x31]))
    );
    // Only allow-listed labels are exported.
    assert!(root.attributes.contains_key("ptc.label.agent"));
    assert!(!root.attributes.contains_key("ptc.label.user"));
}

#[tokio::test]
async fn unsampled_and_mock_runs_export_nothing_but_fill_the_read_model() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    let sinks = Sinks::new();
    let mut config = ProjectorConfig::default();
    config.telemetry.sampled_per_million = 0;
    let projector = sinks.projector(&engine, config);
    let run = start(&engine, RunMetadata::default()).await;
    let mut mock = request("flow", json!({}));
    mock.mode = RunMode::Mock;
    engine.start_run(mock).await.unwrap();
    drive(&engine).await;
    engine.signal(&run, "go", json!(null), None).await.unwrap();
    drive(&engine).await;
    projector.project_all(now(&engine)).await.unwrap();
    assert!(sinks.exporter.spans().is_empty());
    assert!(sinks.exporter.logs().is_empty());
    let summaries = sinks
        .read_model
        .search_runs(&RunSearch::new(run.tenant.clone(), 10))
        .await
        .unwrap();
    assert_eq!(summaries.len(), 2);

    // Mock runs are not exported even when every run is sampled.
    let sinks = Sinks::new();
    let engine = support::engine(programs(), RecordingDispatcher::echo());
    let projector = sinks.projector(&engine, ProjectorConfig::default());
    let mut mock = request("flow", json!({}));
    mock.mode = RunMode::Mock;
    engine.start_run(mock).await.unwrap();
    drive(&engine).await;
    projector.project_all(now(&engine)).await.unwrap();
    assert!(sinks.exporter.spans().is_empty());
}

#[tokio::test]
async fn a_quarantined_run_is_reported_to_the_harness() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    subscribe(&engine).await;
    let sinks = Sinks::new();
    let projector = sinks.projector(&engine, ProjectorConfig::default());
    let run = start(&engine, RunMetadata::default()).await;
    let lease = engine
        .journal()
        .acquire_run(&run, "w", now(&engine), std::time::Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    engine
        .journal()
        .quarantine(&lease, "history entry 1 is …".into(), now(&engine))
        .await
        .unwrap();
    projector.notify_quarantined(&run).await.unwrap();
    let deliveries = sinks.webhooks.deliveries();
    assert_eq!(deliveries.len(), 1);
    assert!(matches!(
        &deliveries[0].1.kind,
        WebhookEventKind::RunQuarantined { reason } if reason == "history entry 1 is …"
    ));
}
