//! The conformance suite of `SpanExporter`: what an exporter is given reaches its backend as the
//! projector derived it (ids, parents, names, kinds, times, statuses, attributes, logs and their
//! trace context), a span exported twice is the same span, and nothing else arrives.
//!
//! An exporter has no read side, so the suite reads the backend through a [`Collector`] that the
//! adapter's test provides: the memory exporter is its own collector; an OpenTelemetry exporter's
//! test reads an in-memory OpenTelemetry exporter or a collector. Delivery is at least once: the
//! backend may hold a span twice, but every copy must be the same span.

use std::collections::BTreeMap;
use std::future::Future;

use rewind_core::{LogLevel, RunId, Seq, Timestamp};

use super::{T0, check};
use crate::adapters::MemorySpanExporter;
use crate::ports::{
    AttributeValue, LogRecord, Span, SpanExporter, SpanId, SpanKind, SpanStatus, TraceId,
};
use crate::telemetry::{SpanKey, span_id};

/// What reached the backend behind an exporter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Collected {
    /// Every span received, a span exported twice possibly twice.
    pub spans: Vec<Span>,
    /// Every log record received, in any order.
    pub logs: Vec<LogRecord>,
}

/// The backend behind an exporter, as the suite reads it. Each new exporter the suite creates
/// starts with an empty backend, or one whose earlier content its collector leaves out.
pub trait Collector {
    /// What reached the backend so far, once every export that returned has arrived.
    fn collected(&self) -> impl Future<Output = Collected> + Send;
}

/// The memory exporter keeps what it was given: it is its own backend.
impl Collector for MemorySpanExporter {
    async fn collected(&self) -> Collected {
        Collected {
            spans: self.spans(),
            logs: self.logs(),
        }
    }
}

/// Runs every check, each on a new exporter.
pub async fn run_all<E: SpanExporter + Collector>(new_exporter: impl Fn() -> E) {
    check!("spans", new_exporter(), spans_reach_the_backend_as_exported);
    check!(
        "spans",
        new_exporter(),
        a_span_exported_twice_is_the_same_span
    );
    check!("spans", new_exporter(), traces_stay_apart);
    check!("spans", new_exporter(), attribute_values_keep_their_type);
    check!("spans", new_exporter(), statuses_kinds_and_times_are_kept);
    check!(
        "spans",
        new_exporter(),
        logs_reach_the_backend_with_their_trace_context
    );
    check!("spans", new_exporter(), every_log_level_is_kept);
    check!("spans", new_exporter(), empty_exports_send_nothing);
    check!("spans", new_exporter(), large_batches_arrive_whole);
}

fn run() -> RunId {
    RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0001)
}

/// The run's own trace: its id.
fn trace() -> TraceId {
    TraceId(*run().as_uuid().as_bytes())
}

/// A harness's trace and span, as a `traceparent` gives them.
fn harness() -> (TraceId, SpanId) {
    (
        TraceId([
            0x0a, 0xf7, 0x65, 0x19, 0x16, 0xcd, 0x43, 0xdd, 0x84, 0x48, 0xeb, 0x21, 0x1c, 0x80,
            0x31, 0x9c,
        ]),
        SpanId([0xb7, 0xad, 0x6b, 0x71, 0x69, 0x20, 0x33, 0x31]),
    )
}

fn at(ms: u64) -> Timestamp {
    Timestamp(T0.as_millis() + ms)
}

fn string(text: &str) -> AttributeValue {
    AttributeValue::String(text.to_owned())
}

fn attributes<const N: usize>(
    pairs: [(&str, AttributeValue); N],
) -> BTreeMap<String, AttributeValue> {
    let mut attributes: BTreeMap<String, AttributeValue> = pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    attributes.insert("rewind.run_id".into(), string(&run().to_string()));
    attributes
}

/// The run span, child of the harness's span.
fn run_span() -> Span {
    let (trace, parent) = harness();
    Span {
        trace_id: trace,
        span_id: span_id(run(), SpanKey::Run),
        parent: Some(parent),
        name: "run".into(),
        kind: SpanKind::Internal,
        start: at(0),
        end: at(9_000),
        status: SpanStatus::Ok,
        attributes: attributes([
            ("rewind.script", string(&"ab".repeat(32))),
            ("rewind.toolset", string(&"cd".repeat(32))),
            ("rewind.outcome", string("completed")),
            ("rewind.label.agent", string("triage")),
        ]),
    }
}

/// A tool call span under the run span.
fn call_span(seq: u32) -> Span {
    let (trace, _) = harness();
    Span {
        trace_id: trace,
        span_id: span_id(run(), SpanKey::Command(Seq(seq))),
        parent: Some(span_id(run(), SpanKey::Run)),
        name: "github.list_issues".into(),
        kind: SpanKind::Client,
        start: at(10 * u64::from(seq)),
        end: at(10 * u64::from(seq) + 310),
        status: SpanStatus::Ok,
        attributes: attributes([
            ("rewind.seq", AttributeValue::Int(i64::from(seq))),
            ("rewind.tool", string("github.list_issues")),
            ("rewind.callsite", string("script.ts:5")),
            ("rewind.outcome", string("completed")),
            ("rewind.args.digest", string(&"ef".repeat(32))),
            ("rewind.args.size", AttributeValue::Int(34)),
            (
                "rewind.args.preview",
                string("{\"repo\":\"acme/api\",\"state\":\"open\"}"),
            ),
        ]),
    }
}

/// A failed tool call span.
fn failed_span() -> Span {
    let mut span = call_span(2);
    span.status = SpanStatus::Error {
        message: "rate limited: retry in 30 s".into(),
    };
    span.attributes
        .insert("rewind.outcome".into(), string("failed"));
    span.attributes
        .insert("rewind.error.kind".into(), string("transient"));
    span
}

/// A timer span: no status, internal.
fn timer_span() -> Span {
    let (trace, _) = harness();
    Span {
        trace_id: trace,
        span_id: span_id(run(), SpanKey::Command(Seq(3))),
        parent: Some(span_id(run(), SpanKey::Run)),
        name: "timer".into(),
        kind: SpanKind::Internal,
        start: at(400),
        end: at(3_400),
        status: SpanStatus::Unset,
        attributes: attributes([
            ("rewind.seq", AttributeValue::Int(3)),
            ("rewind.duration_ms", AttributeValue::Int(3_000)),
            ("rewind.outcome", string("fired")),
        ]),
    }
}

fn log(level: LogLevel, message: &str, span: Option<SpanId>, ms: u64) -> LogRecord {
    let (trace, _) = harness();
    LogRecord {
        trace_id: trace,
        span_id: span,
        time: at(ms),
        level,
        message: message.to_owned(),
        attributes: attributes([
            (
                "rewind.index",
                AttributeValue::Int(i64::try_from(ms).unwrap_or(0)),
            ),
            ("rewind.callsite", string("script.ts:7")),
        ]),
    }
}

/// The spans received, one per (trace, span id). Every copy of a span must be the same.
fn distinct(spans: Vec<Span>) -> BTreeMap<(TraceId, SpanId), Span> {
    let mut by_id: BTreeMap<(TraceId, SpanId), Span> = BTreeMap::new();
    for span in spans {
        let key = (span.trace_id, span.span_id);
        if let Some(seen) = by_id.get(&key) {
            assert_eq!(
                seen, &span,
                "two different spans arrived with the same trace and span id"
            );
        }
        by_id.insert(key, span);
    }
    by_id
}

fn by_id(spans: impl IntoIterator<Item = Span>) -> BTreeMap<(TraceId, SpanId), Span> {
    spans
        .into_iter()
        .map(|span| ((span.trace_id, span.span_id), span))
        .collect()
}

/// Log records in a stable order: the backend may reorder them.
fn sorted(mut logs: Vec<LogRecord>) -> Vec<LogRecord> {
    logs.sort_by(|a, b| {
        (a.time, &a.message, a.level.as_str(), a.span_id).cmp(&(
            b.time,
            &b.message,
            b.level.as_str(),
            b.span_id,
        ))
    });
    logs
}

pub async fn spans_reach_the_backend_as_exported<E: SpanExporter + Collector>(exporter: &E) {
    let spans = vec![call_span(1), failed_span(), timer_span(), run_span()];
    exporter.export(spans.clone()).await.expect("export");
    let collected = exporter.collected().await;
    assert_eq!(distinct(collected.spans), by_id(spans));
    assert!(collected.logs.is_empty(), "spans are not logs");
}

pub async fn a_span_exported_twice_is_the_same_span<E: SpanExporter + Collector>(exporter: &E) {
    // A feed item applied twice, then a later one that ends the run.
    exporter
        .export(vec![call_span(1), timer_span()])
        .await
        .expect("export");
    exporter
        .export(vec![call_span(1), timer_span()])
        .await
        .expect("export again");
    exporter
        .export(vec![timer_span(), run_span()])
        .await
        .expect("export the end");
    let collected = exporter.collected().await;
    assert_eq!(
        distinct(collected.spans),
        by_id([call_span(1), timer_span(), run_span()])
    );
}

pub async fn traces_stay_apart<E: SpanExporter + Collector>(exporter: &E) {
    // The same span id in two traces: two runs started under two harness traces would not
    // share ids, but a backend must keep spans apart by trace all the same.
    let first = call_span(1);
    let mut second = call_span(1);
    second.trace_id = trace();
    second.name = "slack.post_message".into();
    let mut root = run_span();
    root.trace_id = trace();
    root.parent = None;
    let spans = vec![first, second, root];
    exporter.export(spans.clone()).await.expect("export");
    assert_eq!(distinct(exporter.collected().await.spans), by_id(spans));
}

pub async fn attribute_values_keep_their_type<E: SpanExporter + Collector>(exporter: &E) {
    let mut span = call_span(4);
    let long: String = "é✓🚀".repeat(40);
    span.attributes.extend([
        ("test.int.max".to_owned(), AttributeValue::Int(i64::MAX)),
        ("test.int.min".to_owned(), AttributeValue::Int(i64::MIN)),
        ("test.int.zero".to_owned(), AttributeValue::Int(0)),
        ("test.float".to_owned(), AttributeValue::Float(0.25)),
        (
            "test.float.large".to_owned(),
            AttributeValue::Float(-1.5e300),
        ),
        ("test.float.small".to_owned(), AttributeValue::Float(5e-324)),
        ("test.true".to_owned(), AttributeValue::Bool(true)),
        ("test.false".to_owned(), AttributeValue::Bool(false)),
        ("test.empty".to_owned(), string("")),
        ("test.unicode".to_owned(), AttributeValue::String(long)),
        ("test.digits".to_owned(), string("12")),
    ]);
    exporter.export(vec![span.clone()]).await.expect("export");
    assert_eq!(distinct(exporter.collected().await.spans), by_id([span]));
}

pub async fn statuses_kinds_and_times_are_kept<E: SpanExporter + Collector>(exporter: &E) {
    // A span that starts and ends at the same millisecond, a root of the run's own trace, and
    // one per status.
    let mut instant = timer_span();
    instant.span_id = span_id(run(), SpanKey::Command(Seq(5)));
    instant.start = at(1_234);
    instant.end = at(1_234);
    let mut root = run_span();
    root.trace_id = trace();
    root.parent = None;
    root.status = SpanStatus::Error {
        message: "TypeError: x is not a function".into(),
    };
    let mut wait = timer_span();
    wait.span_id = span_id(run(), SpanKey::Entry(9));
    wait.trace_id = trace();
    wait.name = "signal approval".into();
    wait.start = at(86_400_000 * 3);
    wait.end = at(86_400_000 * 3 + 1);
    let spans = vec![instant, root, wait, call_span(6), failed_span()];
    exporter.export(spans.clone()).await.expect("export");
    assert_eq!(distinct(exporter.collected().await.spans), by_id(spans));
}

pub async fn logs_reach_the_backend_with_their_trace_context<E: SpanExporter + Collector>(
    exporter: &E,
) {
    let activation = span_id(run(), SpanKey::Activation(1));
    let mut own_trace = log(LogLevel::Info, "second run", Some(activation), 20);
    own_trace.trace_id = trace();
    let logs = vec![
        log(LogLevel::Info, "3 stale issues", Some(activation), 12),
        log(LogLevel::Info, "3 stale issues", Some(activation), 13),
        log(LogLevel::Warn, "no span", None, 14),
        log(LogLevel::Info, "", Some(activation), 15),
        own_trace,
    ];
    exporter.export_logs(logs.clone()).await.expect("export");
    let collected = exporter.collected().await;
    assert_eq!(sorted(collected.logs), sorted(logs));
    assert!(collected.spans.is_empty(), "logs are not spans");
}

pub async fn every_log_level_is_kept<E: SpanExporter + Collector>(exporter: &E) {
    let activation = Some(span_id(run(), SpanKey::Activation(2)));
    let logs = vec![
        log(LogLevel::Debug, "debug", activation, 1),
        log(LogLevel::Info, "info", activation, 2),
        log(LogLevel::Warn, "warn", activation, 3),
        log(LogLevel::Error, "error", activation, 4),
    ];
    exporter.export_logs(logs.clone()).await.expect("export");
    assert_eq!(sorted(exporter.collected().await.logs), sorted(logs));
}

pub async fn empty_exports_send_nothing<E: SpanExporter + Collector>(exporter: &E) {
    exporter.export(Vec::new()).await.expect("export no span");
    exporter
        .export_logs(Vec::new())
        .await
        .expect("export no log");
    assert_eq!(exporter.collected().await, Collected::default());
}

pub async fn large_batches_arrive_whole<E: SpanExporter + Collector>(exporter: &E) {
    let spans: Vec<Span> = (1..=2_000).map(call_span).collect();
    let logs: Vec<LogRecord> = (0..1_000)
        .map(|n| log(LogLevel::Info, &format!("item {n}"), None, n))
        .collect();
    exporter.export(spans.clone()).await.expect("export");
    exporter
        .export_logs(logs.clone())
        .await
        .expect("export logs");
    let collected = exporter.collected().await;
    assert_eq!(distinct(collected.spans), by_id(spans));
    assert_eq!(sorted(collected.logs), sorted(logs));
}
