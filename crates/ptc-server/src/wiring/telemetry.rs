//! Telemetry: the `tracing` subscriber of the service telemetry, and the `SpanExporter` of run
//! telemetry (`archi/05-testing-and-observability.md`, "Observability").
//!
//! Service telemetry is written as log lines on standard error, filtered by `--log` (the
//! `RUST_LOG` syntax). Run telemetry has no exporter configured: the projector applies the
//! `otel` feed and exports nothing.

use ptc_engine::ports::{ExportError, LogRecord, Span, SpanExporter};
use tracing_subscriber::EnvFilter;

/// Installs the log subscriber of the process. A second call does nothing.
pub(crate) fn install(filter: &str) {
    let filter = EnvFilter::try_new(filter).unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// The exporter of run telemetry when none is configured: spans and logs are dropped.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct NoExport;

impl SpanExporter for NoExport {
    async fn export(&self, _spans: Vec<Span>) -> Result<(), ExportError> {
        Ok(())
    }

    async fn export_logs(&self, _logs: Vec<LogRecord>) -> Result<(), ExportError> {
        Ok(())
    }
}

/// The exporter of this server.
pub(crate) type Exporter = NoExport;
