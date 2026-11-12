//! OpenTelemetry for ai-ptc (`archi/05-testing-and-observability.md`, "Observability"). The only
//! crate that depends on OpenTelemetry (`archi/07-code-architecture.md`, "Dependency rules").
//!
//! Two kinds of telemetry, both over OTLP (gRPC or HTTP with protobuf), in the same traces:
//!
//! - **Run telemetry**: what each script did. The projector derives spans and log records from
//!   committed entries, with deterministic ids, and hands them to [`OtelSpanExporter`], the
//!   `SpanExporter` port. They are exported as derived.
//! - **Service telemetry**: what the platform does, from the `tracing` facade of the engine and
//!   the adapters. [`install`] sets up the process's subscriber; [`ServiceTelemetry`] turns
//!   `tracing` spans into OpenTelemetry spans, joined to a run's trace by the fields they record
//!   (`trace_id`, `span_id`, `parent_span_id`, `traceparent`), events into log records, and
//!   measurements into metrics.
//!
//! Measurements are `tracing` events and spans whose fields name an instrument, as
//! `tracing-opentelemetry` does: `monotonic_counter.<name>`, `counter.<name>` (up-down),
//! `histogram.<name>` and `gauge.<name>` on an event record its value; `histogram.<name>` on a
//! span records the span's duration in seconds when it closes. Fields named `ptc.*` are their
//! attributes, and no other. A measurement is not a log, and `RUST_LOG` never filters it out.
//! `ptc_engine::metrics` lists what the engine measures.
//!
//! Composition roots build both within their Tokio runtime:
//!
//! ```no_run
//! # async fn wire() -> Result<(), ptc_otel::OtelError> {
//! use ptc_otel::{OtelSpanExporter, OtlpConfig, ServiceConfig, SubscriberConfig};
//!
//! let otlp = OtlpConfig::default(); // the `OTEL_EXPORTER_OTLP_*` variables
//! let service = ServiceConfig::default();
//! let _guard = ptc_otel::install(&SubscriberConfig {
//!     stderr: true,
//!     otlp: Some(otlp.clone()),
//!     service: service.clone(),
//!     ..SubscriberConfig::default()
//! })?;
//! let exporter = OtelSpanExporter::otlp(&service, &otlp)?; // for the projector
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod config;
mod convert;
mod exporter;
mod metrics;
mod otlp;
mod service;
mod subscriber;

pub use config::{OtlpConfig, OtlpProtocol, ServiceConfig};
pub use exporter::{OtelSpanExporter, RUN_SCOPE};
pub use service::{
    PARENT_SPAN_ID_FIELD, SERVICE_SCOPE, SPAN_ID_FIELD, ServiceTelemetry, TRACE_ID_FIELD,
    TRACEPARENT_FIELD,
};
pub use subscriber::{SubscriberConfig, TelemetryGuard, install};

/// What can go wrong when setting up or stopping telemetry. Export failures of run telemetry
/// are the port's `ExportError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OtelError {
    #[error("invalid telemetry configuration: {0}")]
    Config(String),
    #[error("the OTLP exporter could not be built: {0}")]
    Build(String),
    #[error("OTLP exports run on a Tokio runtime: build them within one")]
    NoRuntime,
    #[error("the tracing subscriber could not be installed: {0}")]
    Install(String),
    #[error("telemetry could not be flushed: {0}")]
    Flush(String),
    #[error("telemetry could not be shut down: {0}")]
    Shutdown(String),
}
