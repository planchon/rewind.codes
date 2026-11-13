//! Service telemetry: what the platform itself does, from the `tracing` facade, exported over
//! OTLP. `tracing` spans become OpenTelemetry spans, events become log records correlated with
//! the span they happen in, and measurements become metrics (see the module `metrics`).
//!
//! The engine and the adapters never name OpenTelemetry: a span joins a run's trace through
//! fields it records, at creation or later with `Span::record`, before it closes:
//!
//! | Field | Value | Effect |
//! | --- | --- | --- |
//! | `trace_id` | 32 hex digits | The span belongs to this trace: the run's. |
//! | `span_id` | 16 hex digits | The span's own id, when it is derived, such as an activation's `H(run, fence)`. |
//! | `parent_span_id` | 16 hex digits | The span's parent in that trace, such as the run span `H(run, "run")`. |
//! | `traceparent` | a W3C `traceparent` | The span is a child of that span, in that trace: a tool call's trace context. |
//!
//! A span without these fields belongs to the trace of the span it is nested in, under it; a
//! span nested in none starts a trace of its own. A span that names a trace but no parent sits
//! under the span it is nested in when that one is in the same trace, else at the trace's root. Ids are looked up when a span closes, so
//! the spans nested in an activation follow the identity it records once it has read its run.
//! Whether a trace is exported is decided from its id, as for run telemetry: every worker, the
//! projector included, agrees on a run.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use opentelemetry::logs::{AnyValue, LogRecord as _, Logger as _, LoggerProvider as _, Severity};
use opentelemetry::metrics::MeterProvider as _;
use opentelemetry::propagation::TextMapPropagator as _;
use opentelemetry::trace::{
    SpanContext, SpanKind, Status, TraceContextExt as _, TraceFlags, TraceState,
};
use opentelemetry::{Context, InstrumentationScope, Key, KeyValue, SpanId, TraceId, Value};
use opentelemetry_sdk::logs::{LogExporter, SdkLogRecord, SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::{
    BatchSpanProcessor, IdGenerator as _, RandomIdGenerator, SpanData, SpanEvents, SpanExporter,
    SpanLinks, SpanProcessor as _,
};
use rewind_engine::telemetry::{TelemetryConfig, sampled};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{EnvFilter, FilterExt as _, filter_fn};
use tracing_subscriber::layer::Context as LayerContext;
use tracing_subscriber::registry::{LookupSpan, SpanRef};

use crate::metrics::{self, Instruments, MetricsLayer};
use crate::otlp::{self, OnRuntime};
use crate::{OtelError, OtlpConfig, ServiceConfig};

/// The instrumentation scope of service spans.
pub const SERVICE_SCOPE: &str = "rewind";

/// The field that names a span's trace.
pub const TRACE_ID_FIELD: &str = "trace_id";
/// The field that gives a span its own id.
pub const SPAN_ID_FIELD: &str = "span_id";
/// The field that names a span's parent.
pub const PARENT_SPAN_ID_FIELD: &str = "parent_span_id";
/// The field that names a span's trace and parent as a W3C `traceparent`.
pub const TRACEPARENT_FIELD: &str = "traceparent";

/// What the export itself goes through: its spans and events are never exported, or each
/// export would cause another.
const UNEXPORTED_TARGETS: &[&str] = &[
    "opentelemetry",
    "h2",
    "hyper",
    "hyper_util",
    "reqwest",
    "rustls",
    "tonic",
    "tower",
];

/// What logs the messages it carries: `rmcp` writes the MCP messages of a session into its
/// events (a server's notifications at `info`, every request and result at `debug` and
/// `trace`), and those hold tool arguments and results. Payloads never reach OpenTelemetry
/// (`archi/05-testing-and-observability.md`, "Span model"); these stay on stderr only.
const PAYLOAD_TARGETS: &[&str] = &["rmcp"];

/// Service telemetry exported over OpenTelemetry: the [`layer`](Self::layer) to add to a
/// `tracing` subscriber, and the batch processors and the metric reader behind it. Spans and log
/// records are exported in batches, and metrics periodically (`OTEL_METRIC_EXPORT_INTERVAL`,
/// 60 s by default), from the SDK's threads.
#[derive(Clone)]
pub struct ServiceTelemetry {
    pipeline: Arc<Pipeline>,
    instruments: Arc<Instruments>,
}

struct Pipeline {
    spans: BatchSpanProcessor,
    logs: SdkLoggerProvider,
    logger: SdkLogger,
    meters: SdkMeterProvider,
    scope: InstrumentationScope,
    telemetry: TelemetryConfig,
    ids: RandomIdGenerator,
}

impl ServiceTelemetry {
    /// Exports over OTLP, as `otlp` and the `OTEL_EXPORTER_OTLP_*` variables say. Called within a
    /// Tokio runtime, on which the exports then run. `telemetry` is the projector's: a run's
    /// service spans are sampled as its projected spans.
    pub fn otlp(
        service: &ServiceConfig,
        otlp: &OtlpConfig,
        telemetry: TelemetryConfig,
    ) -> Result<Self, OtelError> {
        let runtime = otlp::runtime()?;
        Ok(Self::new(
            OnRuntime::new(otlp::span_exporter(otlp)?, runtime.clone()),
            OnRuntime::new(otlp::log_exporter(otlp)?, runtime.clone()),
            OnRuntime::new(otlp::metric_exporter(otlp)?, runtime),
            service,
            telemetry,
        ))
    }

    /// Exports through the given OpenTelemetry exporters, with `service` as their resource.
    pub fn new<S, L, M>(
        spans: S,
        logs: L,
        metrics: M,
        service: &ServiceConfig,
        telemetry: TelemetryConfig,
    ) -> Self
    where
        S: SpanExporter + 'static,
        L: LogExporter + 'static,
        M: PushMetricExporter,
    {
        let resource = service.resource();
        let mut processor = BatchSpanProcessor::builder(spans).build();
        processor.set_resource(&resource);
        let provider = SdkLoggerProvider::builder()
            .with_resource(resource.clone())
            .with_batch_exporter(logs)
            .build();
        let meters = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(PeriodicReader::builder(metrics).build())
            .build();
        let scope = InstrumentationScope::builder(SERVICE_SCOPE)
            .with_version(env!("CARGO_PKG_VERSION"))
            .build();
        let instruments = Arc::new(Instruments::new(meters.meter_with_scope(scope.clone())));
        Self {
            pipeline: Arc::new(Pipeline {
                spans: processor,
                logger: provider.logger_with_scope(scope.clone()),
                logs: provider,
                meters,
                scope,
                telemetry,
                ids: RandomIdGenerator::default(),
            }),
            instruments,
        }
    }

    /// The `tracing` layer that feeds this pipeline: spans, log records and metrics. It leaves
    /// out what the export itself goes through (the OpenTelemetry SDK, the HTTP and gRPC
    /// clients), and the MCP client's own logs, which hold tool arguments and results.
    pub fn layer<S>(&self) -> impl Layer<S> + Send + Sync
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        self.filtered_layer(None)
    }

    /// [`layer`](Self::layer), with the spans and log records it exports filtered by `filter`
    /// too. Measurements are never filtered out.
    pub(crate) fn filtered_layer<S>(&self, filter: Option<EnvFilter>) -> impl Layer<S> + Send + Sync
    where
        S: Subscriber + for<'a> LookupSpan<'a>,
    {
        let traces = ServiceLayer {
            pipeline: Arc::clone(&self.pipeline),
        }
        .with_filter(
            filter_fn(exported)
                .and(filter_fn(|metadata| !metrics::is_measurement(metadata)))
                .and(filter),
        );
        let measurements = MetricsLayer {
            instruments: Arc::clone(&self.instruments),
        }
        .with_filter(filter_fn(metrics::measures));
        traces.and_then(measurements)
    }

    /// Exports what is buffered now, metrics included. Blocks until the backend answered, or
    /// the SDK's timeout.
    pub fn flush(&self) -> Result<(), OtelError> {
        let spans = self.pipeline.spans.force_flush();
        let logs = self.pipeline.logs.force_flush();
        let meters = self.pipeline.meters.force_flush();
        spans
            .and(logs)
            .and(meters)
            .map_err(|error| OtelError::Flush(error.to_string()))
    }

    /// Exports what is buffered and stops the pipeline: what the layer sees afterwards is
    /// dropped. Blocks like [`flush`](Self::flush): call it outside the runtime, or from a
    /// multi-threaded one, since the exports themselves run on it.
    pub fn shutdown(&self) -> Result<(), OtelError> {
        let spans = self.pipeline.spans.shutdown();
        let logs = self.pipeline.logs.shutdown();
        let meters = self.pipeline.meters.shutdown();
        spans
            .and(logs)
            .and(meters)
            .map_err(|error| OtelError::Shutdown(error.to_string()))
    }
}

impl fmt::Debug for ServiceTelemetry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceTelemetry")
            .field("telemetry", &self.pipeline.telemetry)
            .finish_non_exhaustive()
    }
}

fn exported(metadata: &Metadata<'_>) -> bool {
    let target = metadata.target();
    !UNEXPORTED_TARGETS
        .iter()
        .chain(PAYLOAD_TARGETS)
        .any(|unexported| {
            target
                .strip_prefix(unexported)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
        })
}

/// The `tracing` layer of [`ServiceTelemetry`].
struct ServiceLayer {
    pipeline: Arc<Pipeline>,
}

/// What a span recorded, kept until it closes.
struct SpanState {
    start: SystemTime,
    /// The span's id unless it records one.
    random_span: SpanId,
    /// The trace it starts unless it belongs to another.
    random_trace: TraceId,
    identity: Identity,
    attributes: Vec<KeyValue>,
    status: Status,
}

/// The identity a span recorded through its fields.
#[derive(Debug, Clone, Copy, Default)]
struct Identity {
    trace: Option<TraceId>,
    span: Option<SpanId>,
    parent: Option<SpanId>,
    /// From a `traceparent`: the trace and the parent span.
    traceparent: Option<(TraceId, SpanId)>,
}

/// Where a span sits once its fields and its ancestors are known.
#[derive(Debug, Clone, Copy)]
struct Resolved {
    trace: TraceId,
    span: SpanId,
    parent: Option<SpanId>,
    /// The parent was named by a field: another process's, or a span the projector exports.
    remote_parent: bool,
    sampled: bool,
}

impl Pipeline {
    fn sampled(&self, trace: TraceId) -> bool {
        sampled(
            rewind_engine::ports::TraceId(trace.to_bytes()),
            &self.telemetry,
        )
    }

    /// The span's trace, id and parent: what its fields say, else what its ancestors say.
    fn resolve<S>(&self, span: &SpanRef<'_, S>) -> Option<Resolved>
    where
        S: for<'a> LookupSpan<'a>,
    {
        let (identity, random_trace, random_span) = {
            let extensions = span.extensions();
            let state = extensions.get::<SpanState>()?;
            (state.identity, state.random_trace, state.random_span)
        };
        let own = identity.span.unwrap_or(random_span);
        let outer = || span.parent().and_then(|outer| self.resolve(&outer));
        let (trace, parent, remote_parent) = if let Some(trace) = identity.trace {
            let named = identity.parent.or(identity
                .traceparent
                .filter(|(of, _)| *of == trace)
                .map(|(_, parent)| parent));
            match named {
                Some(parent) => (trace, Some(parent), true),
                // Under the span it is nested in, if that one is in the same trace.
                None => {
                    let outer = outer().filter(|outer| outer.trace == trace);
                    (trace, outer.map(|outer| outer.span), false)
                }
            }
        } else if let Some((trace, parent)) = identity.traceparent {
            (trace, Some(identity.parent.unwrap_or(parent)), true)
        } else if let Some(outer) = outer() {
            (
                outer.trace,
                Some(identity.parent.unwrap_or(outer.span)),
                identity.parent.is_some(),
            )
        } else {
            (random_trace, identity.parent, identity.parent.is_some())
        };
        Some(Resolved {
            trace,
            span: own,
            parent,
            remote_parent,
            sampled: self.sampled(trace),
        })
    }
}

impl<S> Layer<S> for ServiceLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: LayerContext<'_, S>) {
        if Context::is_current_telemetry_suppressed() {
            return;
        }
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut state = SpanState {
            start: SystemTime::now(),
            random_span: self.pipeline.ids.new_span_id(),
            random_trace: self.pipeline.ids.new_trace_id(),
            identity: Identity::default(),
            attributes: Vec::with_capacity(attrs.fields().len()),
            status: Status::Unset,
        };
        attrs.record(&mut SpanFields { state: &mut state });
        span.extensions_mut().insert(state);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: LayerContext<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let mut extensions = span.extensions_mut();
        if let Some(state) = extensions.get_mut::<SpanState>() {
            values.record(&mut SpanFields { state });
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: LayerContext<'_, S>) {
        if Context::is_current_telemetry_suppressed() {
            return;
        }
        let metadata = event.metadata();
        let level = *metadata.level();
        let mut record = self.pipeline.logger.create_log_record();
        let now = SystemTime::now();
        record.set_timestamp(now);
        record.set_observed_timestamp(now);
        record.set_target(metadata.target());
        record.set_severity_number(severity(level));
        record.set_severity_text(level.as_str());
        let mut fields = EventFields {
            record: &mut record,
            message: None,
        };
        event.record(&mut fields);
        let message = fields.message.take();
        if let Some(message) = &message {
            record.set_body(AnyValue::String(message.clone().into()));
        }
        if let Some(span) = ctx.event_span(event) {
            if let Some(resolved) = self.pipeline.resolve(&span) {
                let flags = TraceFlags::default().with_sampled(resolved.sampled);
                record.set_trace_context(resolved.trace, resolved.span, Some(flags));
            }
            // An error marks the span it happens in.
            if level == Level::ERROR
                && let Some(state) = span.extensions_mut().get_mut::<SpanState>()
            {
                state.status = Status::error(message.unwrap_or_default());
            }
        }
        self.pipeline.logger.emit(record);
    }

    fn on_close(&self, id: Id, ctx: LayerContext<'_, S>) {
        let Some(span) = ctx.span(&id) else {
            return;
        };
        let Some(resolved) = self.pipeline.resolve(&span) else {
            return;
        };
        let Some(state) = span.extensions_mut().remove::<SpanState>() else {
            return;
        };
        if !resolved.sampled {
            return;
        }
        self.pipeline.spans.on_end(SpanData {
            span_context: SpanContext::new(
                resolved.trace,
                resolved.span,
                TraceFlags::SAMPLED,
                false,
                TraceState::NONE,
            ),
            parent_span_id: resolved.parent.unwrap_or(SpanId::INVALID),
            parent_span_is_remote: resolved.remote_parent,
            span_kind: SpanKind::Internal,
            name: Cow::Borrowed(span.metadata().name()),
            start_time: state.start,
            end_time: SystemTime::now(),
            attributes: state.attributes,
            dropped_attributes_count: 0,
            events: SpanEvents::default(),
            links: SpanLinks::default(),
            status: state.status,
            instrumentation_scope: self.pipeline.scope.clone(),
        });
    }
}

fn severity(level: Level) -> Severity {
    match level {
        Level::TRACE => Severity::Trace,
        Level::DEBUG => Severity::Debug,
        Level::INFO => Severity::Info,
        Level::WARN => Severity::Warn,
        Level::ERROR => Severity::Error,
    }
}

/// A span's fields: its identity, and its attributes.
struct SpanFields<'a> {
    state: &'a mut SpanState,
}

impl SpanFields<'_> {
    fn attribute(&mut self, field: &Field, value: Value) {
        let key = Key::from_static_str(field.name());
        let attributes = &mut self.state.attributes;
        match attributes.iter_mut().find(|kv| kv.key == key) {
            Some(kv) => kv.value = value,
            None => attributes.push(KeyValue::new(key, value)),
        }
    }

    fn text(&mut self, field: &Field, text: &str) {
        let identity = &mut self.state.identity;
        let recognized = match field.name() {
            TRACE_ID_FIELD => trace_id(text).map(|id| identity.trace = Some(id)),
            SPAN_ID_FIELD => span_id(text).map(|id| identity.span = Some(id)),
            PARENT_SPAN_ID_FIELD => span_id(text).map(|id| identity.parent = Some(id)),
            TRACEPARENT_FIELD => traceparent(text).map(|ids| identity.traceparent = Some(ids)),
            _ => None,
        };
        // A value that is not an id stays visible as an attribute.
        if recognized.is_none() {
            self.attribute(field, Value::String(text.to_owned().into()));
        }
    }
}

impl Visit for SpanFields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.text(field, value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.text(field, &format!("{value:?}"));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.attribute(field, Value::String(value.to_string().into()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.attribute(field, Value::I64(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.attribute(field, Value::I64(i64::try_from(value).unwrap_or(i64::MAX)));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.attribute(field, Value::F64(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.attribute(field, Value::Bool(value));
    }
}

fn trace_id(text: &str) -> Option<TraceId> {
    let text = text.trim();
    (text.len() == 32)
        .then(|| TraceId::from_hex(text).ok())
        .flatten()
        .filter(|id| *id != TraceId::INVALID)
}

fn span_id(text: &str) -> Option<SpanId> {
    let text = text.trim();
    (text.len() == 16)
        .then(|| SpanId::from_hex(text).ok())
        .flatten()
        .filter(|id| *id != SpanId::INVALID)
}

/// A W3C `traceparent`, read by OpenTelemetry's propagator.
fn traceparent(text: &str) -> Option<(TraceId, SpanId)> {
    let carrier = HashMap::from([("traceparent".to_owned(), text.trim().to_owned())]);
    let context = TraceContextPropagator::new().extract(&carrier);
    let span = context.span();
    let span_context = span.span_context();
    span_context
        .is_valid()
        .then(|| (span_context.trace_id(), span_context.span_id()))
}

/// An event's fields: `message` is the body, the others are attributes.
struct EventFields<'a> {
    record: &'a mut SdkLogRecord,
    message: Option<String>,
}

impl EventFields<'_> {
    fn attribute(&mut self, field: &Field, value: AnyValue) {
        self.record
            .add_attribute(Key::from_static_str(field.name()), value);
    }
}

impl Visit for EventFields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_owned());
        } else {
            self.attribute(field, AnyValue::String(value.to_owned().into()));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(text);
        } else {
            self.attribute(field, AnyValue::String(text.into()));
        }
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.attribute(field, AnyValue::String(value.to_string().into()));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.attribute(field, AnyValue::Int(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.attribute(
            field,
            AnyValue::Int(i64::try_from(value).unwrap_or(i64::MAX)),
        );
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.attribute(field, AnyValue::Double(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.attribute(field, AnyValue::Boolean(value));
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry_sdk::logs::InMemoryLogExporter;
    use opentelemetry_sdk::metrics::InMemoryMetricExporter;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, Metric, MetricData};
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use rewind_core::RunId;
    use rewind_engine::telemetry::{SpanKey, span_id as derived};
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    struct Backend {
        telemetry: ServiceTelemetry,
        spans: InMemorySpanExporter,
        logs: InMemoryLogExporter,
        metrics: InMemoryMetricExporter,
    }

    /// A series of a metric: its attributes, sorted, and its value (a histogram's count).
    type Series = Vec<(Vec<(String, String)>, f64)>;

    impl Backend {
        fn new(telemetry: TelemetryConfig) -> Self {
            let spans = InMemorySpanExporter::default();
            let logs = InMemoryLogExporter::default();
            let metrics = InMemoryMetricExporter::default();
            Self {
                telemetry: ServiceTelemetry::new(
                    spans.clone(),
                    logs.clone(),
                    metrics.clone(),
                    &ServiceConfig::default(),
                    telemetry,
                ),
                spans,
                logs,
                metrics,
            }
        }

        /// Runs `f` with this backend's layer as the thread's subscriber.
        fn trace(&self, f: impl FnOnce()) {
            let subscriber = tracing_subscriber::registry().with(self.telemetry.layer());
            tracing::subscriber::with_default(subscriber, f);
            self.telemetry.flush().expect("flush");
        }

        /// Runs `f` with this backend's layer filtered by `directives`, as `install` does.
        fn trace_filtered(&self, directives: &str, f: impl FnOnce()) {
            let filter = EnvFilter::try_new(directives).expect("directives");
            let subscriber =
                tracing_subscriber::registry().with(self.telemetry.filtered_layer(Some(filter)));
            tracing::subscriber::with_default(subscriber, f);
            self.telemetry.flush().expect("flush");
        }

        /// The metric as last exported: cumulative, so it holds every measurement.
        fn metric<T>(&self, name: &str, read: impl Fn(&Metric) -> T) -> T {
            let exports = self.metrics.get_finished_metrics().expect("metrics");
            let metric = exports
                .iter()
                .rev()
                .flat_map(|export| export.scope_metrics())
                .find_map(|scope| scope.metrics().find(|metric| metric.name() == name))
                .unwrap_or_else(|| panic!("no metric {name}"));
            read(metric)
        }

        fn has_metric(&self, name: &str) -> bool {
            let exports = self.metrics.get_finished_metrics().expect("metrics");
            exports
                .iter()
                .flat_map(|export| export.scope_metrics())
                .any(|scope| scope.metrics().any(|metric| metric.name() == name))
        }

        fn series(&self, name: &str) -> Series {
            self.metric(name, |metric| {
                let mut series: Series = match metric.data() {
                    AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                        .data_points()
                        .map(|point| (labels(point.attributes()), point.value() as f64))
                        .collect(),
                    AggregatedMetrics::I64(MetricData::Sum(sum)) => sum
                        .data_points()
                        .map(|point| (labels(point.attributes()), point.value() as f64))
                        .collect(),
                    AggregatedMetrics::F64(MetricData::Gauge(gauge)) => gauge
                        .data_points()
                        .map(|point| (labels(point.attributes()), point.value()))
                        .collect(),
                    AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
                        .data_points()
                        .map(|point| (labels(point.attributes()), point.count() as f64))
                        .collect(),
                    other => panic!("{name}: unexpected data {other:?}"),
                };
                series.sort_by(|a, b| a.0.cmp(&b.0));
                series
            })
        }

        fn span(&self, name: &str) -> SpanData {
            let spans = self.spans.get_finished_spans().expect("spans");
            let mut named = spans.into_iter().filter(|span| span.name == name);
            let span = named.next().unwrap_or_else(|| panic!("no span {name}"));
            assert!(named.next().is_none(), "one span {name}");
            span
        }

        fn log(&self, message: &str) -> SdkLogRecord {
            let logs = self.logs.get_emitted_logs().expect("logs");
            logs.into_iter()
                .map(|log| log.record)
                .find(|record| {
                    matches!(record.body(), Some(AnyValue::String(body)) if body.as_str() == message)
                })
                .unwrap_or_else(|| panic!("no log {message}"))
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

    fn run() -> RunId {
        RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0042)
    }

    fn hex(id: rewind_engine::ports::SpanId) -> String {
        id.to_string()
    }

    #[test]
    fn an_activation_joins_its_run_s_trace_with_its_own_id() {
        let backend = Backend::new(TelemetryConfig::default());
        let trace = TraceId::from_bytes(*run().as_uuid().as_bytes());
        let own = derived(run(), SpanKey::Activation(7));
        let run_span = derived(run(), SpanKey::Run);
        backend.trace(|| {
            // As the engine does: the identity is recorded once the activation read its run.
            let activation = tracing::info_span!(
                "activation",
                fence = 7,
                trace_id = tracing::field::Empty,
                span_id = tracing::field::Empty,
                parent_span_id = tracing::field::Empty,
            );
            let _entered = activation.enter();
            let early = tracing::info_span!("take_inbox");
            activation.record("trace_id", tracing::field::display(trace));
            activation.record("span_id", tracing::field::display(own));
            activation.record("parent_span_id", tracing::field::display(run_span));
            drop(early);
            tracing::info_span!("replay", entries = 12_u64).in_scope(|| {
                tracing::info!(cached = false, "instance rebuilt");
            });
            tracing::info_span!("same_trace", trace_id = %trace).in_scope(|| {});
        });

        let activation = backend.span("activation");
        assert_eq!(activation.span_context.trace_id(), trace);
        assert_eq!(activation.span_context.span_id().to_string(), hex(own));
        assert_eq!(activation.parent_span_id.to_string(), hex(run_span));
        assert!(activation.parent_span_is_remote);
        assert_eq!(activation.instrumentation_scope.name(), SERVICE_SCOPE);
        assert!(
            activation
                .attributes
                .contains(&KeyValue::new("fence", Value::I64(7)))
        );
        assert!(
            !activation
                .attributes
                .iter()
                .any(|kv| kv.key.as_str().ends_with("_id")),
            "ids are the span's identity, not attributes"
        );
        // Nested spans follow the identity, even those created before it was recorded.
        for name in ["take_inbox", "replay"] {
            let nested = backend.span(name);
            assert_eq!(nested.span_context.trace_id(), trace, "{name}");
            assert_eq!(nested.parent_span_id.to_string(), hex(own), "{name}");
            assert!(!nested.parent_span_is_remote);
        }
        // A span naming the trace it is nested in stays under the span it is nested in.
        let same = backend.span("same_trace");
        assert_eq!(same.span_context.trace_id(), trace);
        assert_eq!(same.parent_span_id.to_string(), hex(own));
        assert!(!same.parent_span_is_remote);
        let replay = backend.span("replay");
        // The log is correlated with the span it happened in.
        let log = backend.log("instance rebuilt");
        let context = log.trace_context().expect("trace context");
        assert_eq!(context.trace_id, trace);
        assert_eq!(context.span_id, replay.span_context.span_id());
        assert_eq!(context.trace_flags, Some(TraceFlags::SAMPLED));
        assert_eq!(log.severity_number(), Some(Severity::Info));
        assert!(
            log.attributes_iter()
                .any(|(key, value)| key.as_str() == "cached" && *value == AnyValue::Boolean(false))
        );
    }

    #[test]
    fn a_traceparent_makes_a_span_the_child_of_a_tool_call() {
        let backend = Backend::new(TelemetryConfig::default());
        let call = derived(run(), SpanKey::Command(rewind_core::Seq(3)));
        let traceparent = format!("00-0af7651916cd43dd8448eb211c80319c-{call}-01");
        backend.trace(|| {
            tracing::info_span!("attempt", traceparent = %traceparent, attempt = 2_u64)
                .in_scope(|| tracing::info_span!("http").in_scope(|| {}));
        });
        let attempt = backend.span("attempt");
        assert_eq!(
            attempt.span_context.trace_id().to_string(),
            "0af7651916cd43dd8448eb211c80319c"
        );
        assert_eq!(attempt.parent_span_id.to_string(), hex(call));
        assert!(attempt.parent_span_is_remote);
        let http = backend.span("http");
        assert_eq!(
            http.span_context.trace_id(),
            attempt.span_context.trace_id()
        );
        assert_eq!(http.parent_span_id, attempt.span_context.span_id());
    }

    #[test]
    fn spans_without_identity_start_their_own_trace() {
        let backend = Backend::new(TelemetryConfig::default());
        backend.trace(|| {
            tracing::info_span!("worker").in_scope(|| {
                tracing::info_span!("acquire_runnable").in_scope(|| {});
                // A value that is not an id stays an attribute.
                tracing::info_span!("odd", trace_id = "not-hex").in_scope(|| {});
            });
            tracing::info_span!("scheduler").in_scope(|| {});
        });
        let worker = backend.span("worker");
        assert_eq!(worker.parent_span_id, SpanId::INVALID);
        let acquire = backend.span("acquire_runnable");
        assert_eq!(
            acquire.span_context.trace_id(),
            worker.span_context.trace_id()
        );
        assert_eq!(acquire.parent_span_id, worker.span_context.span_id());
        let odd = backend.span("odd");
        assert_eq!(odd.span_context.trace_id(), worker.span_context.trace_id());
        assert!(
            odd.attributes
                .contains(&KeyValue::new("trace_id", Value::String("not-hex".into())))
        );
        let scheduler = backend.span("scheduler");
        assert_ne!(
            scheduler.span_context.trace_id(),
            worker.span_context.trace_id()
        );
    }

    #[test]
    fn unsampled_traces_export_their_logs_but_not_their_spans() {
        let backend = Backend::new(TelemetryConfig {
            sampled_per_million: 0,
            ..TelemetryConfig::default()
        });
        backend.trace(|| {
            tracing::info_span!("activation").in_scope(|| tracing::warn!("lease lost"));
        });
        assert!(
            backend
                .spans
                .get_finished_spans()
                .expect("spans")
                .is_empty()
        );
        let log = backend.log("lease lost");
        let context = log.trace_context().expect("trace context");
        assert_eq!(context.trace_flags, Some(TraceFlags::NOT_SAMPLED));
        assert_eq!(log.severity_number(), Some(Severity::Warn));
    }

    #[test]
    fn an_error_event_marks_its_span() {
        let backend = Backend::new(TelemetryConfig::default());
        backend.trace(|| {
            tracing::info_span!("commit").in_scope(|| tracing::error!("the store refused"));
            tracing::error!("outside any span");
        });
        assert_eq!(
            backend.span("commit").status,
            Status::error("the store refused")
        );
        assert!(backend.log("outside any span").trace_context().is_none());
    }

    #[test]
    fn the_export_path_is_never_exported() {
        let backend = Backend::new(TelemetryConfig::default());
        backend.trace(|| {
            tracing::info_span!(target: "h2::codec", "frame").in_scope(|| {
                tracing::info!(target: "hyper_util::client", "connected");
            });
            tracing::info!(target: "h2_like", "kept");
            tracing::info_span!(target: "tonic", "call").in_scope(|| {});
        });
        assert!(
            backend
                .spans
                .get_finished_spans()
                .expect("spans")
                .is_empty()
        );
        let logs = backend.logs.get_emitted_logs().expect("logs");
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].record.target().map(|t| t.as_ref()), Some("h2_like"));
    }

    #[test]
    fn the_messages_of_mcp_sessions_are_never_exported() {
        let backend = Backend::new(TelemetryConfig::default());
        backend.trace(|| {
            // What `rmcp` logs of a session: a server's notification, a call and its result.
            tracing::info!(
                target: "rmcp::service",
                notification = "{\"method\":\"notifications/message\",\"params\":{\"data\":\"card 4242\"}}",
                "received notification"
            );
            tracing::debug!(target: "rmcp::service", request = "{\"name\":\"charge\"}", "received request");
            tracing::info_span!(target: "rmcp::service", "serve_inner").in_scope(|| {});
            tracing::info!(target: "rmcp_like", "kept");
        });
        assert!(
            backend
                .spans
                .get_finished_spans()
                .expect("spans")
                .is_empty()
        );
        let logs = backend.logs.get_emitted_logs().expect("logs");
        assert_eq!(logs.len(), 1);
        assert_eq!(
            logs[0].record.target().map(|t| t.as_ref()),
            Some("rmcp_like")
        );
    }

    #[test]
    fn suppressed_telemetry_is_not_exported() {
        let backend = Backend::new(TelemetryConfig::default());
        backend.trace(|| {
            let _suppressed = Context::enter_telemetry_suppressed_scope();
            tracing::info_span!("export").in_scope(|| tracing::info!("sent"));
            tracing::info!(monotonic_counter.rewind.exports = 1_u64);
            tracing::info_span!(
                "timed",
                histogram.rewind.export.duration = tracing::field::Empty
            )
            .in_scope(|| {});
        });
        assert!(!backend.has_metric("rewind.exports"));
        assert!(!backend.has_metric("rewind.export.duration"));
        assert!(
            backend
                .spans
                .get_finished_spans()
                .expect("spans")
                .is_empty()
        );
        assert!(backend.logs.get_emitted_logs().expect("logs").is_empty());
    }

    #[test]
    fn measurement_events_become_metrics_and_not_logs() {
        let backend = Backend::new(TelemetryConfig::default());
        backend.trace(|| {
            tracing::info_span!("activation", run = "r1").in_scope(|| {
                for result in ["hit", "hit", "miss"] {
                    tracing::info!(
                        monotonic_counter.rewind.lookups = 1_u64,
                        rewind.result = result,
                        run = "r1",
                        task = 7_u64,
                    );
                }
                tracing::info!(histogram.rewind.inbox = 3_u64);
                tracing::info!(histogram.rewind.inbox = 1_i64);
                tracing::info!(counter.rewind.queued = 2_i64);
                tracing::info!(counter.rewind.queued = -1_i64);
                tracing::info!(gauge.rewind.heap = 0.5, rewind.worker = true);
                // Not an integer: a counter ignores it.
                tracing::info!(monotonic_counter.rewind.lookups = 0.5);
                tracing::info!("a log");
            });
        });
        // Only `rewind.*` fields are attributes.
        assert_eq!(
            backend.series("rewind.lookups"),
            vec![
                (vec![label("rewind.result", "hit")], 2.0),
                (vec![label("rewind.result", "miss")], 1.0),
            ]
        );
        assert_eq!(backend.series("rewind.inbox"), vec![(vec![], 2.0)]);
        assert_eq!(backend.series("rewind.queued"), vec![(vec![], 1.0)]);
        assert_eq!(
            backend.series("rewind.heap"),
            vec![(vec![label("rewind.worker", "true")], 0.5)]
        );
        let (unit, bounds, sum) = backend.metric("rewind.inbox", |metric| match metric.data() {
            AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                let point = histogram.data_points().next().expect("a point");
                (
                    metric.unit().to_owned(),
                    point.bounds().collect::<Vec<_>>(),
                    point.sum(),
                )
            }
            other => panic!("{other:?}"),
        });
        assert_eq!(unit, "");
        assert_eq!(bounds.get(..4), Some(&[0.0, 1.0, 2.0, 4.0][..]));
        assert!((sum - 4.0).abs() < f64::EPSILON);
        let scope = backend.metrics.get_finished_metrics().expect("metrics")[0]
            .scope_metrics()
            .next()
            .expect("a scope")
            .scope()
            .name()
            .to_owned();
        assert_eq!(scope, SERVICE_SCOPE);
        // Measurements are not logs; the log is.
        let logs = backend.logs.get_emitted_logs().expect("logs");
        assert_eq!(logs.len(), 1, "{logs:?}");
        backend.log("a log");
    }

    #[test]
    fn a_span_measures_its_duration_with_the_attributes_it_records() {
        let backend = Backend::new(TelemetryConfig::default());
        backend.trace(|| {
            for outcome in ["committed", "failed", "committed"] {
                let span = tracing::info_span!(
                    "activation",
                    fence = 3_u64,
                    histogram.rewind.activation.duration = tracing::field::Empty,
                    rewind.outcome = tracing::field::Empty,
                );
                span.in_scope(|| std::thread::sleep(std::time::Duration::from_millis(2)));
                span.record("rewind.outcome", outcome);
            }
        });
        assert_eq!(
            backend.series("rewind.activation.duration"),
            vec![
                (vec![label("rewind.outcome", "committed")], 2.0),
                (vec![label("rewind.outcome", "failed")], 1.0),
            ]
        );
        let (unit, sum, bounds) =
            backend.metric("rewind.activation.duration", |metric| match metric.data() {
                AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                    let point = histogram.data_points().next().expect("a point");
                    (
                        metric.unit().to_owned(),
                        point.sum(),
                        point.bounds().collect::<Vec<_>>(),
                    )
                }
                other => panic!("{other:?}"),
            });
        assert_eq!(unit, "s");
        assert!(sum >= 0.002, "{sum}");
        assert_eq!(bounds.first(), Some(&0.001));
        assert_eq!(bounds.last(), Some(&86_400.0));
        // The span is still a span, with its outcome as an attribute.
        let spans = backend.spans.get_finished_spans().expect("spans");
        assert_eq!(spans.len(), 3);
        assert!(
            spans[1]
                .attributes
                .contains(&KeyValue::new("rewind.outcome", "failed"))
        );
    }

    #[test]
    fn the_filter_keeps_measurements() {
        let backend = Backend::new(TelemetryConfig::default());
        backend.trace_filtered("warn", || {
            let span = tracing::info_span!(
                "replay",
                histogram.rewind.replay.duration = tracing::field::Empty,
            );
            span.in_scope(|| {
                tracing::info!(monotonic_counter.rewind.runs.started = 1_u64);
                tracing::info!("filtered out");
                tracing::warn!("kept");
            });
        });
        assert_eq!(
            backend.series("rewind.replay.duration"),
            vec![(vec![], 1.0)]
        );
        assert_eq!(backend.series("rewind.runs.started"), vec![(vec![], 1.0)]);
        // The span and the info log are filtered out; the warning is not.
        assert!(
            backend
                .spans
                .get_finished_spans()
                .expect("spans")
                .is_empty()
        );
        let logs = backend.logs.get_emitted_logs().expect("logs");
        assert_eq!(logs.len(), 1);
        backend.log("kept");
        assert!(!backend.has_metric("rewind.unknown"));
    }
}
