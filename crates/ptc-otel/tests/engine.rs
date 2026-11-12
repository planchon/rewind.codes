//! The engine's service telemetry through `ptc-otel`, on the SDK's in-memory exporters: an
//! activation is a child of its run span, a tool attempt a child of its tool call span, and
//! every instrument of `ptc_engine::metrics` is recorded, the projector's run metrics included.
//!
//! The engine runs with in-memory adapters, a virtual clock and a fake executor whose scripts
//! are Rust closures, on this thread, under the layer of `ServiceTelemetry`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use opentelemetry::KeyValue;
use opentelemetry_sdk::logs::InMemoryLogExporter;
use opentelemetry_sdk::metrics::InMemoryMetricExporter;
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use ptc_core::{
    Command, Epoch, JsonValue, McpServer, McpTransport, Payload, Script, Seq, Timestamp, Tool,
    ToolFailure, ToolFailureKind, ToolRoute, Toolset,
};
use ptc_engine::adapters::{
    MemoryBlobs, MemoryJournal, MemoryReadModel, MemorySecrets, MemorySpanExporter, MemoryWebhooks,
    SeededEntropy, VirtualClock,
};
use ptc_engine::metrics;
use ptc_engine::ports::{
    Clock as _, CompiledScript, Credentials, Delivery, Dispatched, Emitted, ExecutorError,
    InvalidScript, ScriptExecutor, ScriptInstance, StartScript, ToolCall, ToolDispatcher,
};
use ptc_engine::projector::{Projector, ProjectorConfig};
use ptc_engine::telemetry::{SpanKey, TelemetryConfig, span_id};
use ptc_engine::{ActivationOutcome, Adapters, DriveOptions, Engine, EngineConfig, StartRun};
use ptc_otel::{ServiceConfig, ServiceTelemetry};
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
        if call.attempt == 1 {
            Dispatched::Transient(ToolFailure {
                kind: ToolFailureKind::Unavailable,
                message: "try again".into(),
                data: None,
            })
        } else {
            Dispatched::Result(json!({ "title": "Flaky" }))
        }
    });
    let engine = engine(Programs::default(), flaky, EngineConfig::default());
    let run = engine
        .start_run(request("flow"))
        .await
        .expect("started")
        .run;
    engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .expect("driven");
    project(&engine).await;
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
                .contains(&KeyValue::new("ptc.run_id", run.id.to_string()))
        );
        assert!(
            activation
                .attributes
                .iter()
                .all(|kv| kv.key.as_str() != "run")
        );
    }
    let fences: Vec<String> = activations
        .iter()
        .map(|span| span.span_context.span_id().to_string())
        .collect();
    assert!(fences.contains(&span_id(run.id, SpanKey::Activation(1)).to_string()));
    // Each attempt is a child of the tool call span.
    let attempts = backend.spans("tool_attempt");
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
        vec![(vec![label("ptc.result", "hit")], 1)]
    );
    assert_eq!(
        backend.series(metrics::TOOL_ATTEMPT_DURATION),
        vec![
            (
                vec![label(metrics::OUTCOME, "result"), label("ptc.tool", ISSUE)],
                1
            ),
            (
                vec![
                    label(metrics::OUTCOME, "transient"),
                    label("ptc.tool", ISSUE)
                ],
                1
            ),
        ]
    );
    assert_eq!(
        backend.series(metrics::TOOL_ATTEMPT_RETRIES),
        vec![(vec![label("ptc.tool", ISSUE)], 1)]
    );
    // Run metrics, projected from the journal.
    assert_eq!(backend.series(metrics::RUNS_STARTED), vec![(vec![], 1)]);
    assert_eq!(
        backend.series(metrics::RUNS_ENDED),
        vec![(vec![label(metrics::OUTCOME, "completed")], 1)]
    );
    assert_eq!(
        backend.series(metrics::TOOL_CALL_DURATION),
        vec![(
            vec![
                label(metrics::OUTCOME, "completed"),
                label("ptc.tool", ISSUE)
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
    let mut config = EngineConfig::default();
    config.cache.max_instances = 0;
    let programs = Programs::default();
    let engine = engine(
        programs.clone(),
        Dispatcher::new(|_| Dispatched::Result(json!({ "title": "Stable" }))),
        config,
    );
    let run = engine
        .start_run(request("diverge"))
        .await
        .expect("started")
        .run;
    engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .expect("driven");
    // The script changes under the run: its next replay diverges.
    programs.changed.store(true, Ordering::SeqCst);
    engine
        .signal(&run, "go", json!(null), None)
        .await
        .expect("signalled");
    let report = engine.activate_next().await.expect("activated");
    assert!(matches!(
        report.map(|report| report.outcome),
        Some(ActivationOutcome::Quarantined { .. })
    ));
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
        vec![(vec![label("ptc.result", "miss")], 2)]
    );
    assert_eq!(
        backend.series(metrics::INSTANCE_CACHE_EVICTIONS),
        vec![(vec![], 2)]
    );
    assert_eq!(
        backend.series(metrics::RUNS_QUARANTINED),
        vec![(vec![label("ptc.reason", "non_determinism")], 1)]
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
// The engine

type TestEngine = Engine<
    MemoryJournal,
    MemoryBlobs,
    Programs,
    Dispatcher,
    VirtualClock,
    SeededEntropy,
    MemorySecrets,
    MemoryWebhooks,
>;

fn engine(programs: Programs, dispatcher: Dispatcher, config: EngineConfig) -> TestEngine {
    Engine::new(
        Adapters {
            journal: MemoryJournal::new(),
            blobs: MemoryBlobs::new(),
            executor: programs,
            dispatcher,
            clock: VirtualClock::new(T0),
            entropy: SeededEntropy::new(7),
            secrets: MemorySecrets::new(),
            webhooks: MemoryWebhooks::new(),
        },
        config,
    )
}

/// A run of the script `name`, with the MCP tool `github.get_issue`.
fn request(name: &str) -> StartRun {
    let server = McpServer::new(
        "github",
        McpTransport::Http {
            url: "https://mcp.example.com/github".into(),
        },
    );
    let issue = Tool::new(
        ISSUE,
        ToolRoute::Mcp {
            server: "github".into(),
        },
        json!({}),
    );
    let toolset = Toolset::new([server], [issue]).expect("a toolset");
    StartRun::new(Script::new(name), toolset, json!({}))
}

/// Applies every feed item: the run metrics are measured once acknowledged.
async fn project(engine: &TestEngine) {
    let read_model = MemoryReadModel::new();
    let exporter = MemorySpanExporter::new();
    let webhooks = MemoryWebhooks::new();
    let projector = Projector {
        journal: engine.journal(),
        read_model: &read_model,
        exporter: &exporter,
        webhooks: &webhooks,
        config: ProjectorConfig::default(),
    };
    projector
        .project_all(engine.clock().now())
        .await
        .expect("projected");
}

/// Answers each call with a function of it.
struct Dispatcher {
    answer: Box<dyn Fn(&ToolCall) -> Dispatched + Send + Sync>,
}

impl Dispatcher {
    fn new(answer: impl Fn(&ToolCall) -> Dispatched + Send + Sync + 'static) -> Self {
        Self {
            answer: Box::new(answer),
        }
    }
}

impl ToolDispatcher for Dispatcher {
    async fn dispatch(&self, call: ToolCall, _credentials: Option<Credentials>) -> Dispatched {
        (self.answer)(&call)
    }
}

/// Two scripts. `flow` calls `github.get_issue` and completes with its result. `diverge` calls
/// it with `{"n": 1}` (`{"n": 2}` once `changed` is set), then waits for the signal `go`.
#[derive(Clone, Default)]
struct Programs {
    changed: Arc<AtomicBool>,
}

struct Instance {
    script: String,
}

fn call(args: JsonValue) -> Emitted {
    Emitted::Command(Command::ScheduleToolCall {
        seq: Seq(1),
        tool: ISSUE.into(),
        args: Payload::Value(args),
        callsite: None,
    })
}

impl ScriptExecutor for Programs {
    type Instance = Instance;

    fn epochs(&self) -> &[Epoch] {
        &[Epoch(1)]
    }

    fn compile(
        &self,
        script: &Script,
        _toolset: &Toolset,
    ) -> Result<CompiledScript, InvalidScript> {
        Ok(CompiledScript {
            javascript: script.source().into(),
            source_map: String::new(),
        })
    }

    async fn start(&self, start: StartScript) -> Result<(Instance, Vec<Emitted>), ExecutorError> {
        let script = start.compiled.javascript;
        let args = match script.as_str() {
            "diverge" if self.changed.load(Ordering::SeqCst) => json!({ "n": 2 }),
            "diverge" | "flow" => json!({ "n": 1 }),
            other => return Err(ExecutorError::Internal(format!("no script {other}"))),
        };
        Ok((Instance { script }, vec![call(args)]))
    }
}

impl ScriptInstance for Instance {
    async fn deliver(
        &mut self,
        delivery: Delivery,
        _now: Timestamp,
    ) -> Result<Vec<Emitted>, ExecutorError> {
        Ok(match (self.script.as_str(), delivery) {
            ("flow", Delivery::ToolCallCompleted { result, .. }) => {
                vec![Emitted::Command(Command::CompleteRun {
                    output: Payload::Value(result),
                })]
            }
            ("diverge", Delivery::ToolCallCompleted { .. }) => {
                vec![Emitted::Annotation(ptc_core::Annotation::SignalAwaited {
                    name: "go".into(),
                    timeout: None,
                    callsite: None,
                })]
            }
            _ => Vec::new(),
        })
    }
}
