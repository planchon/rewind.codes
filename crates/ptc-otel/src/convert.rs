//! The port's spans and log records as OpenTelemetry's, kept as the projector derived them:
//! ids, parents, times, statuses and attributes.

use std::borrow::Cow;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use opentelemetry::logs::{AnyValue, LogRecord as _, Severity};
use opentelemetry::trace::{SpanContext, Status, TraceFlags, TraceState};
use opentelemetry::{InstrumentationScope, Key, KeyValue, Value};
use opentelemetry_sdk::logs::SdkLogRecord;
use opentelemetry_sdk::trace::{SpanData, SpanEvents, SpanLinks};
use ptc_core::{LogLevel, Timestamp};
use ptc_engine::ports::{AttributeValue, LogRecord, Span, SpanId, SpanKind, SpanStatus, TraceId};

pub(crate) fn trace_id(id: TraceId) -> opentelemetry::TraceId {
    opentelemetry::TraceId::from_bytes(id.0)
}

pub(crate) fn span_id(id: SpanId) -> opentelemetry::SpanId {
    opentelemetry::SpanId::from_bytes(id.0)
}

pub(crate) fn time(at: Timestamp) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(at.as_millis())
}

fn value(value: &AttributeValue) -> Value {
    match value {
        AttributeValue::String(text) => Value::String(text.clone().into()),
        AttributeValue::Int(n) => Value::I64(*n),
        AttributeValue::Float(x) => Value::F64(*x),
        AttributeValue::Bool(b) => Value::Bool(*b),
    }
}

fn any_value(value: &AttributeValue) -> AnyValue {
    match value {
        AttributeValue::String(text) => AnyValue::String(text.clone().into()),
        AttributeValue::Int(n) => AnyValue::Int(*n),
        AttributeValue::Float(x) => AnyValue::Double(*x),
        AttributeValue::Bool(b) => AnyValue::Boolean(*b),
    }
}

pub(crate) fn severity(level: LogLevel) -> Severity {
    match level {
        LogLevel::Debug => Severity::Debug,
        LogLevel::Info => Severity::Info,
        LogLevel::Warn => Severity::Warn,
        LogLevel::Error => Severity::Error,
    }
}

/// A span of the run's trace, sampled: the projector only exports sampled runs.
pub(crate) fn span_data(span: Span, scope: &InstrumentationScope) -> SpanData {
    SpanData {
        span_context: SpanContext::new(
            trace_id(span.trace_id),
            span_id(span.span_id),
            TraceFlags::SAMPLED,
            false,
            TraceState::NONE,
        ),
        parent_span_id: span.parent.map_or(opentelemetry::SpanId::INVALID, span_id),
        parent_span_is_remote: false,
        span_kind: match span.kind {
            SpanKind::Internal => opentelemetry::trace::SpanKind::Internal,
            SpanKind::Client => opentelemetry::trace::SpanKind::Client,
        },
        name: Cow::Owned(span.name),
        start_time: time(span.start),
        end_time: time(span.end),
        attributes: span
            .attributes
            .iter()
            .map(|(key, v)| KeyValue::new(Key::new(key.clone()), value(v)))
            .collect(),
        dropped_attributes_count: 0,
        events: SpanEvents::default(),
        links: SpanLinks::default(),
        status: match span.status {
            SpanStatus::Unset => Status::Unset,
            SpanStatus::Ok => Status::Ok,
            SpanStatus::Error { message } => Status::Error {
                description: message.into(),
            },
        },
        instrumentation_scope: scope.clone(),
    }
}

/// Fills an empty OpenTelemetry log record. The time it was observed is the time the journal
/// recorded it: a record exported again is the same record.
pub(crate) fn fill_log_record(log: LogRecord, record: &mut SdkLogRecord) {
    record.set_timestamp(time(log.time));
    record.set_observed_timestamp(time(log.time));
    record.set_severity_number(severity(log.level));
    record.set_severity_text(log.level.as_str());
    record.set_body(AnyValue::String(log.message.into()));
    record.add_attributes(
        log.attributes
            .iter()
            .map(|(key, v)| (Key::new(key.clone()), any_value(v))),
    );
    record.set_trace_context(
        trace_id(log.trace_id),
        log.span_id.map_or(opentelemetry::SpanId::INVALID, span_id),
        Some(TraceFlags::SAMPLED),
    );
}

/// The reverse conversions, to read what an in-memory OpenTelemetry exporter received.
#[cfg(test)]
pub(crate) mod back {
    use std::collections::BTreeMap;

    use super::*;

    pub(crate) fn timestamp(at: SystemTime) -> Timestamp {
        let since = at.duration_since(UNIX_EPOCH).expect("after the epoch");
        assert_eq!(since.subsec_nanos() % 1_000_000, 0, "whole milliseconds");
        Timestamp(u64::try_from(since.as_millis()).expect("in range"))
    }

    fn attribute(value: &Value) -> AttributeValue {
        match value {
            Value::String(text) => AttributeValue::String(text.as_str().to_owned()),
            Value::I64(n) => AttributeValue::Int(*n),
            Value::F64(x) => AttributeValue::Float(*x),
            Value::Bool(b) => AttributeValue::Bool(*b),
            other => panic!("the projector exports no array: {other:?}"),
        }
    }

    fn any_attribute(value: &AnyValue) -> AttributeValue {
        match value {
            AnyValue::String(text) => AttributeValue::String(text.as_str().to_owned()),
            AnyValue::Int(n) => AttributeValue::Int(*n),
            AnyValue::Double(x) => AttributeValue::Float(*x),
            AnyValue::Boolean(b) => AttributeValue::Bool(*b),
            other => panic!("the projector exports scalar attributes only: {other:?}"),
        }
    }

    pub(crate) fn span(data: &SpanData) -> Span {
        let parent = (data.parent_span_id != opentelemetry::SpanId::INVALID)
            .then(|| SpanId(data.parent_span_id.to_bytes()));
        Span {
            trace_id: TraceId(data.span_context.trace_id().to_bytes()),
            span_id: SpanId(data.span_context.span_id().to_bytes()),
            parent,
            name: data.name.to_string(),
            kind: match data.span_kind {
                opentelemetry::trace::SpanKind::Internal => SpanKind::Internal,
                opentelemetry::trace::SpanKind::Client => SpanKind::Client,
                ref other => panic!("the projector exports no {other:?} span"),
            },
            start: timestamp(data.start_time),
            end: timestamp(data.end_time),
            status: match &data.status {
                Status::Unset => SpanStatus::Unset,
                Status::Ok => SpanStatus::Ok,
                Status::Error { description } => SpanStatus::Error {
                    message: description.to_string(),
                },
            },
            attributes: data
                .attributes
                .iter()
                .map(|kv| (kv.key.as_str().to_owned(), attribute(&kv.value)))
                .collect(),
        }
    }

    pub(crate) fn log(record: &SdkLogRecord) -> LogRecord {
        let context = record.trace_context().expect("a trace context");
        let level = match record.severity_number().expect("a severity") as i32 {
            1..=8 => LogLevel::Debug,
            9..=12 => LogLevel::Info,
            13..=16 => LogLevel::Warn,
            _ => LogLevel::Error,
        };
        assert_eq!(record.severity_text(), Some(level.as_str()));
        assert_eq!(record.observed_timestamp(), record.timestamp());
        let message = match record.body() {
            Some(AnyValue::String(text)) => text.as_str().to_owned(),
            other => panic!("a log's body is its message: {other:?}"),
        };
        LogRecord {
            trace_id: TraceId(context.trace_id.to_bytes()),
            span_id: (context.span_id != opentelemetry::SpanId::INVALID)
                .then(|| SpanId(context.span_id.to_bytes())),
            time: timestamp(record.timestamp().expect("a time")),
            level,
            message,
            attributes: record
                .attributes_iter()
                .map(|(key, value)| (key.as_str().to_owned(), any_attribute(value)))
                .collect::<BTreeMap<_, _>>(),
        }
    }
}
