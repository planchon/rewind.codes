//! The engine's service telemetry through `rewind-otel`, on the SDK's in-memory exporters: an
//! activation is a child of its run span, a tool attempt a child of its call span, and
//! every instrument of `rewind_engine::metrics` is recorded, the run metrics of the outbox's
//! projections included.
//!
//! The run plane runs in the engine's driver: one node of one partition over the memory
//! adapters, a virtual clock and a fake executor whose workflows are Rust closures, on this
//! thread, under the layer of `ServiceTelemetry`.

use std::num::NonZeroU16;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use opentelemetry::KeyValue;
use opentelemetry_sdk::logs::InMemoryLogExporter;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use rewind_core::{
    Action, ActionFailure, ActionFailureKind, ActionRoute, ActionSet, Code, Command, Epoch,
    JsonValue, McpServer, McpTransport, Payload, Seq, Timestamp,
};
use rewind_engine::adapters::{
    ControlPlaneChanges, MemoryArchive, MemoryBlobs, MemoryCatalog, MemoryCluster,
    MemoryPartitions, MemorySecrets, MemorySpanExporter, MemoryWebhooks, SeededEntropy,
    VirtualClock,
};
use rewind_engine::driver::{Driver, PlaneClock};
use rewind_engine::edge::Edge;
use rewind_engine::metrics;
use rewind_engine::plane::{NodeConfig, Wiring};
use rewind_engine::ports::{
    ActionDispatcher, Call, CallAuth, CodeExecutor, CodeInstance, CompiledCode, Delivery,
    Dispatched, Emitted, ExecutorError, InvalidCode, NodeId, QuarantineKind, RunRef, StartCode,
};
use rewind_engine::telemetry::{SpanKey, TelemetryConfig, span_id};
use rewind_engine::{CacheConfig, StartRun};
use rewind_otel::{ServiceConfig, ServiceTelemetry};
use serde_json::json;
use tracing_subscriber::layer::SubscriberExt as _;

const ISSUE: &str = "github.get_issue";

/// 2026-01-01T00:00:00Z
const T0: Timestamp = Timestamp(1_767_225_600_000);

#[tokio::test]
async fn an_engine_s_service_telemetry_joins_its_runs_and_measures_it() {
    let backend = Backend::new();
    let subscriber = tracing_subscriber::registry().with(backend.telemetry.layer());
    let _default = tracing::subscriber::set_default(subscriber);

    // A run whose call fails once, transiently: two attempts, a sticky second activation.
    let flaky = Dispatcher::new(|call| {
        if call.delivery == 1 {
            Dispatched::Transient(ActionFailure {
                kind: ActionFailureKind::Unavailable,
                message: "try again".into(),
                data: None,
            })
        } else {
            Dispatched::Result(json!({ "title": "Flaky" }))
        }
    });
    let mut world = World::new(Programs::default(), flaky, CacheConfig::default()).await;
    let run = world.start(request("flow")).await;
    world.driver.run_until_idle_virtual().await.expect("driven");
    drop(_default);
    backend.telemetry.flush().expect("flush");

    // Every activation is a child of the run span, in the run's trace, with its own id.
    let trace = opentelemetry::trace::TraceId::from_bytes(*run.id.as_uuid().as_bytes());
    let activations = backend.spans("activation");
    assert_eq!(activations.len(), 2, "the start, then the call's result");
    for activation in &activations {
        assert_eq!(activation.span_context.trace_id(), trace);
        assert_eq!(
            activation.parent_span_id.to_string(),
            span_id(run.id, SpanKey::Run).to_string()
        );
        assert!(activation.parent_span_is_remote);
        assert!(
            activation
                .attributes
                .contains(&KeyValue::new(metrics::OUTCOME, "committed"))
        );
        // The run is the attribute 05 names, not the tenant and the run in one string.
        assert!(
            activation
                .attributes
                .contains(&KeyValue::new("rewind.run_id", run.id.to_string()))
        );
        assert!(
            activation
                .attributes
                .iter()
                .all(|kv| kv.key.as_str() != "run")
        );
    }
    // An activation's id is `H(run, "activation", index of its first event)`: the start's
    // `run_started` and `schedule_call`, then the call's result at 2.
    let mut ids: Vec<String> = activations
        .iter()
        .map(|span| span.span_context.span_id().to_string())
        .collect();
    ids.sort();
    let mut expected = vec![
        span_id(run.id, SpanKey::Activation(0)).to_string(),
        span_id(run.id, SpanKey::Activation(2)).to_string(),
    ];
    expected.sort();
    assert_eq!(ids, expected);
    // Each attempt is a child of the call span.
    let attempts = backend.spans("delivery");
    assert_eq!(attempts.len(), 2);
    for attempt in &attempts {
        assert_eq!(attempt.span_context.trace_id(), trace);
        assert_eq!(
            attempt.parent_span_id.to_string(),
            span_id(run.id, SpanKey::Command(Seq(1))).to_string()
        );
    }

    let committed = vec![label(metrics::OUTCOME, "committed")];
    assert_eq!(
        backend.series(metrics::ACTIVATION_DURATION),
        vec![(committed, 2)]
    );
    assert_eq!(backend.series(metrics::ACTIVATION_INBOX), vec![(vec![], 2)]);
    // The second activation found the instance the first one kept.
    assert_eq!(
        backend.series(metrics::INSTANCE_CACHE_LOOKUPS),
        vec![(vec![label("rewind.result", "hit")], 1)]
    );
    assert_eq!(
        backend.series(metrics::DELIVERY_DURATION),
        vec![
            (
                vec![
                    label("rewind.action", ISSUE),
                    label(metrics::OUTCOME, "result")
                ],
                1
            ),
            (
                vec![
                    label("rewind.action", ISSUE),
                    label(metrics::OUTCOME, "transient")
                ],
                1
            ),
        ]
    );
    assert_eq!(
        backend.series(metrics::REDELIVERIES),
        vec![(vec![label("rewind.action", ISSUE)], 1)]
    );
    // Run metrics, from the outbox's projections of the committed events.
    assert_eq!(backend.series(metrics::RUNS_STARTED), vec![(vec![], 1)]);
    assert_eq!(
        backend.series(metrics::RUNS_ENDED),
        vec![(vec![label(metrics::OUTCOME, "completed")], 1)]
    );
    assert_eq!(
        backend.series(metrics::CALL_DURATION),
        vec![(
            vec![
                label("rewind.action", ISSUE),
                label(metrics::OUTCOME, "completed")
            ],
            1
        )]
    );
    // Nothing replayed, nothing evicted, nothing quarantined.
    for absent in [
        metrics::REPLAY_DURATION,
        metrics::INSTANCE_CACHE_EVICTIONS,
        metrics::RUNS_QUARANTINED,
    ] {
        assert!(!backend.has_metric(absent), "{absent}");
    }
}

#[tokio::test]
async fn replays_cache_misses_and_quarantines_are_measured() {
    let backend = Backend::new();
    let subscriber = tracing_subscriber::registry().with(backend.telemetry.layer());
    let _default = tracing::subscriber::set_default(subscriber);

    // No instance is kept: every activation replays the history.
    let programs = Programs::default();
    let mut world = World::new(
        programs.clone(),
        Dispatcher::new(|_| Dispatched::Result(json!({ "title": "Stable" }))),
        CacheConfig {
            max_instances: 0,
            ..CacheConfig::default()
        },
    )
    .await;
    let run = world.start(request("diverge")).await;
    world.driver.run_until_idle_virtual().await.expect("driven");
    // The code changes under the run: its next replay diverges.
    programs.changed.store(true, Ordering::SeqCst);
    {
        let World {
            edge, driver, at, ..
        } = &mut world;
        let mut gateway = driver.gateway(at);
        edge.signal(&mut gateway, &run, "go", json!(null), None)
            .await
            .expect("signalled");
    }
    world.driver.run_until_idle().await.expect("driven");
    let view = {
        let World {
            edge, driver, at, ..
        } = &mut world;
        let mut gateway = driver.gateway(at);
        edge.get_run(&mut gateway, &run)
            .await
            .expect("read")
            .expect("a run")
    };
    assert_eq!(
        view.head.quarantine.map(|quarantine| quarantine.kind),
        Some(QuarantineKind::NonDeterminism)
    );
    drop(_default);
    backend.telemetry.flush().expect("flush");

    assert_eq!(
        backend.series(metrics::ACTIVATION_DURATION),
        vec![
            (vec![label(metrics::OUTCOME, "committed")], 2),
            (vec![label(metrics::OUTCOME, "quarantined")], 1),
        ]
    );
    assert_eq!(
        backend.series(metrics::REPLAY_DURATION),
        vec![
            (vec![label(metrics::OUTCOME, "end_of_history")], 1),
            (vec![label(metrics::OUTCOME, "non_determinism")], 1),
        ]
    );
    assert_eq!(
        backend.series(metrics::INSTANCE_CACHE_LOOKUPS),
        vec![(vec![label("rewind.result", "miss")], 2)]
    );
    assert_eq!(
        backend.series(metrics::INSTANCE_CACHE_EVICTIONS),
        vec![(vec![], 2)]
    );
    assert_eq!(
        backend.series(metrics::RUNS_QUARANTINED),
        vec![(vec![label("rewind.reason", "non_determinism")], 1)]
    );
    // The replay is nested in its activation.
    let replays = backend.spans("replay");
    let activations = backend.spans("activation");
    for replay in &replays {
        assert!(
            activations
                .iter()
                .any(|activation| activation.span_context.span_id() == replay.parent_span_id)
        );
    }
}

// -------------------------------------------------------------------------------------------
// The backend

struct Backend {
    telemetry: ServiceTelemetry,
    spans: InMemorySpanExporter,
    metrics: InMemoryMetricExporter,
}

impl Backend {
    fn new() -> Self {
        let spans = InMemorySpanExporter::default();
        let metrics = InMemoryMetricExporter::default();
        Self {
            telemetry: ServiceTelemetry::new(
                spans.clone(),
                InMemoryLogExporter::default(),
                metrics.clone(),
                &ServiceConfig::default(),
                TelemetryConfig::default(),
            ),
            spans,
            metrics,
        }
    }

    fn spans(&self, name: &str) -> Vec<SpanData> {
        let spans = self.spans.get_finished_spans().expect("spans");
        spans.into_iter().filter(|span| span.name == name).collect()
    }

    fn has_metric(&self, name: &str) -> bool {
        let exports = self.metrics.get_finished_metrics().expect("metrics");
        exports
            .iter()
            .flat_map(|export| export.scope_metrics())
            .any(|scope| scope.metrics().any(|metric| metric.name() == name))
    }

    /// The metric's series as last exported (cumulative): attributes sorted, and the value of
    /// a counter or the count of a histogram.
    fn series(&self, name: &str) -> Vec<(Vec<(String, String)>, u64)> {
        let exports = self.metrics.get_finished_metrics().expect("metrics");
        let metric = exports
            .iter()
            .rev()
            .flat_map(|export| export.scope_metrics())
            .find_map(|scope| scope.metrics().find(|metric| metric.name() == name))
            .unwrap_or_else(|| panic!("no metric {name}"));
        let mut series: Vec<_> = match metric.data() {
            AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                .data_points()
                .map(|point| (labels(point.attributes()), point.value()))
                .collect(),
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
                .data_points()
                .map(|point| (labels(point.attributes()), point.count()))
                .collect(),
            other => panic!("{name}: unexpected data {other:?}"),
        };
        series.sort();
        series
    }
}

fn labels<'a>(attributes: impl Iterator<Item = &'a KeyValue>) -> Vec<(String, String)> {
    let mut labels: Vec<_> = attributes
        .map(|kv| (kv.key.to_string(), kv.value.to_string()))
        .collect();
    labels.sort();
    labels
}

fn label(key: &str, value: &str) -> (String, String) {
    (key.to_owned(), value.to_owned())
}

// -------------------------------------------------------------------------------------------
// The run plane

type TestPlane = Wiring<
    MemoryCatalog,
    MemoryPartitions,
    MemoryArchive,
    MemoryBlobs,
    Programs,
    Dispatcher,
    MemorySecrets,
    MemorySpanExporter,
    MemoryWebhooks,
    VirtualClock,
    SeededEntropy,
>;

/// One node of one partition, its edge, and the driver that steps it.
struct World {
    driver: Driver<TestPlane, MemoryCluster>,
    edge: Edge<TestPlane>,
    at: NodeId,
}

impl World {
    async fn new(programs: Programs, dispatcher: Dispatcher, cache: CacheConfig) -> Self {
        let changes = ControlPlaneChanges::new();
        let cluster = MemoryCluster::with_changes(1, changes.clone());
        let blobs = MemoryBlobs::new();
        let plane = Arc::new(Wiring {
            catalog: MemoryCatalog::with_changes(changes.clone()),
            partitions: MemoryPartitions::new(),
            archive: MemoryArchive::with_blobs(blobs.clone()),
            blobs,
            executor: programs,
            dispatcher,
            secrets: MemorySecrets::with_changes(changes),
            spans: MemorySpanExporter::new(),
            webhooks: MemoryWebhooks::new(),
            clock: VirtualClock::new(T0),
            entropy: SeededEntropy::new(7),
        });
        let node = NodeId::new("otel").expect("a node id");
        let mut config = NodeConfig::new(node, NonZeroU16::MIN);
        config.horizon = std::time::Duration::from_secs(100 * 365 * 86_400);
        config.cache = cache;
        let mut driver = Driver::new(cluster, Arc::new(PlaneClock(Arc::clone(&plane))));
        let at = driver
            .add_node(Arc::clone(&plane), config.clone())
            .await
            .expect("a node");
        driver.run_until_idle().await.expect("started");
        let edge = Edge::new(plane, config);
        Self { driver, edge, at }
    }

    async fn start(&mut self, request: StartRun) -> RunRef {
        let mut gateway = self.driver.gateway(&self.at);
        self.edge
            .start_run(&mut gateway, request)
            .await
            .expect("started")
            .run
    }
}

/// A run of the code `name`, with the MCP action `github.get_issue`.
fn request(name: &str) -> StartRun {
    let server = McpServer::new(
        "github",
        McpTransport::Http {
            url: "https://mcp.example.com/github".into(),
        },
    );
    let issue = Action::new(
        ISSUE,
        ActionRoute::Mcp {
            server: "github".into(),
        },
        json!({}),
    );
    let action_set = ActionSet::new([server], [issue]).expect("an action set");
    StartRun::new(Code::new(name), action_set, json!({}))
}

/// Answers each call with a function of it.
struct Dispatcher {
    answer: Box<dyn Fn(&Call) -> Dispatched + Send + Sync>,
}

impl Dispatcher {
    fn new(answer: impl Fn(&Call) -> Dispatched + Send + Sync + 'static) -> Self {
        Self {
            answer: Box::new(answer),
        }
    }
}

impl ActionDispatcher for Dispatcher {
    async fn dispatch(&self, call: Call, _auth: CallAuth) -> Dispatched {
        (self.answer)(&call)
    }
}

/// Two workflows. `flow` calls `github.get_issue` and completes with its result. `diverge` calls
/// it with `{"n": 1}` (`{"n": 2}` once `changed` is set), then waits for the signal `go`.
#[derive(Clone, Default)]
struct Programs {
    changed: Arc<AtomicBool>,
}

struct Instance {
    code: String,
}

fn call(args: JsonValue) -> Emitted {
    Emitted::Command(Command::ScheduleCall {
        seq: Seq(1),
        action: ISSUE.into(),
        args: Payload::Value(args),
        callsite: None,
    })
}

impl CodeExecutor for Programs {
    type Instance = Instance;

    fn epochs(&self) -> &[Epoch] {
        &[Epoch(1)]
    }

    fn compile(&self, code: &Code, _action_set: &ActionSet) -> Result<CompiledCode, InvalidCode> {
        Ok(CompiledCode {
            javascript: code.source().into(),
            source_map: String::new(),
        })
    }

    async fn start(&self, start: StartCode) -> Result<(Instance, Vec<Emitted>), ExecutorError> {
        let code = start.compiled.javascript;
        let args = match code.as_str() {
            "diverge" if self.changed.load(Ordering::SeqCst) => json!({ "n": 2 }),
            "diverge" | "flow" => json!({ "n": 1 }),
            other => return Err(ExecutorError::Internal(format!("no code {other}"))),
        };
        Ok((Instance { code }, vec![call(args)]))
    }
}

impl CodeInstance for Instance {
    async fn deliver(
        &mut self,
        delivery: Delivery,
        _now: Timestamp,
    ) -> Result<Vec<Emitted>, ExecutorError> {
        Ok(match (self.code.as_str(), delivery) {
            ("flow", Delivery::CallCompleted { result, .. }) => {
                vec![Emitted::Command(Command::CompleteRun {
                    output: Payload::Value(result),
                })]
            }
            ("diverge", Delivery::CallCompleted { .. }) => {
                vec![Emitted::Annotation(
                    rewind_core::Annotation::SignalAwaited {
                        name: "go".into(),
                        timeout: None,
                        callsite: None,
                    },
                )]
            }
            _ => Vec::new(),
        })
    }
}
