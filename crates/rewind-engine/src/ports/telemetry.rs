use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;

use rewind_core::{LogLevel, Timestamp};

/// A W3C / OpenTelemetry trace id: the harness's trace id when the run was started with a
/// `traceparent`, otherwise the run id (both are 128 bits).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TraceId(pub [u8; 16]);

/// A span id. Spans projected from the journal have deterministic ids, `H(run, …)`: a span
/// exported again after a crash is the same span.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SpanId(pub [u8; 8]);

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TraceId({self})")
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::Debug for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpanId({self})")
    }
}

/// A value of a span or log attribute.
#[derive(Clone, Debug, PartialEq)]
pub enum AttributeValue {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SpanKind {
    Internal,
    /// A call to the outside: a tool call.
    Client,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpanStatus {
    Unset,
    Ok,
    Error { message: String },
}

/// A span derived from committed entries (`archi/05-testing-and-observability.md`, "Span
/// model"): the run, its tool calls, timers and signal waits. Attributes carry payload digests,
/// sizes and previews, never values.
#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub trace_id: TraceId,
    pub span_id: SpanId,
    pub parent: Option<SpanId>,
    pub name: String,
    pub kind: SpanKind,
    pub start: Timestamp,
    pub end: Timestamp,
    pub status: SpanStatus,
    pub attributes: BTreeMap<String, AttributeValue>,
}

/// A `console.*` log of a script, as an OpenTelemetry log record correlated with the trace.
#[derive(Clone, Debug, PartialEq)]
pub struct LogRecord {
    pub trace_id: TraceId,
    /// The span the log belongs to: the activation that committed it.
    pub span_id: Option<SpanId>,
    pub time: Timestamp,
    pub level: LogLevel,
    pub message: String,
    pub attributes: BTreeMap<String, AttributeValue>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExportError {
    #[error("telemetry export failed: {0}")]
    Export(String),
}

/// Exports run telemetry, projected from committed entries (`archi/07-code-architecture.md`,
/// "The other ports"). Nothing about a script is exported from the isolate, the runtime or a
/// replay.
///
/// Spans have deterministic ids: exporting one twice gives the same span. The projector
/// exports each feed item at least once.
pub trait SpanExporter: Send + Sync {
    fn export(&self, spans: Vec<Span>) -> impl Future<Output = Result<(), ExportError>> + Send;

    fn export_logs(
        &self,
        logs: Vec<LogRecord>,
    ) -> impl Future<Output = Result<(), ExportError>> + Send;
}
