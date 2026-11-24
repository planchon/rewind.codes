//! Telemetry, with `rewind-otel` (`archi/05-testing-and-observability.md`, "Observability"):
//!
//! - service telemetry: the process's `tracing` subscriber, logs on standard error filtered by
//!   `--log`, and spans and logs over OTLP when an endpoint is configured;
//! - run telemetry: the `SpanExporter` the outbox hands the spans and log records the owners
//!   derive from committed events, over OTLP when an endpoint is configured, dropped otherwise.
//!
//! OTLP is on when `--otlp-endpoint` is given, or when the standard
//! `OTEL_EXPORTER_OTLP_ENDPOINT` (or `…_TRACES_ENDPOINT`) is set; the other `OTEL_*` variables
//! apply as OpenTelemetry specifies.

use rewind_engine::ports::{ExportError, LogRecord, Span, SpanExporter};
use rewind_otel::{
    OtelSpanExporter, OtlpConfig, OtlpProtocol, ServiceConfig, SubscriberConfig, TelemetryGuard,
};

use crate::config::ServerArgs;

/// Where OTLP goes, if anywhere.
pub(crate) fn otlp(args: &ServerArgs) -> Option<OtlpConfig> {
    let configured = args.otlp_endpoint.is_some()
        || [
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
        ]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()));
    configured.then(|| OtlpConfig {
        protocol: args.otlp_protocol.map(|protocol| match protocol {
            crate::config::OtlpArg::Grpc => OtlpProtocol::Grpc,
            crate::config::OtlpArg::Http => OtlpProtocol::HttpProtobuf,
        }),
        endpoint: args.otlp_endpoint.clone(),
        ..OtlpConfig::default()
    })
}

/// The resource of what this process exports.
pub(crate) fn service(instance: &str) -> ServiceConfig {
    ServiceConfig {
        version: Some(crate::serve::VERSION.to_owned()),
        attributes: [("service.instance.id".to_owned(), instance.to_owned())]
            .into_iter()
            .collect(),
        ..ServiceConfig::default()
    }
}

/// Installs the process's subscriber. Called once, within the Tokio runtime; the guard lives as
/// long as the process.
pub(crate) fn install(args: &ServerArgs) -> Result<TelemetryGuard, String> {
    let instance = args
        .node_id
        .clone()
        .unwrap_or_else(|| crate::config::default_node_id().to_string());
    rewind_otel::install(&SubscriberConfig {
        filter: Some(args.log.clone()),
        stderr: true,
        otlp: otlp(args),
        service: service(&instance),
        ..SubscriberConfig::default()
    })
    .map_err(|error| error.to_string())
}

/// The exporter of run telemetry.
#[derive(Debug)]
pub(crate) enum Exporter {
    /// No OTLP endpoint: spans and log records are dropped.
    Off,
    Otlp(Box<OtelSpanExporter>),
}

impl Exporter {
    /// Built within the Tokio runtime, on which the exports run.
    pub(crate) fn new(otlp: Option<&OtlpConfig>, service: &ServiceConfig) -> Result<Self, String> {
        match otlp {
            None => Ok(Exporter::Off),
            Some(otlp) => OtelSpanExporter::otlp(service, otlp)
                .map(|exporter| Exporter::Otlp(Box::new(exporter)))
                .map_err(|error| error.to_string()),
        }
    }

    /// Sends what is buffered and stops the exports.
    pub(crate) async fn shutdown(&self) {
        if let Exporter::Otlp(exporter) = self
            && let Err(error) = exporter.shutdown().await
        {
            tracing::warn!(%error, "run telemetry was not all exported");
        }
    }
}

impl SpanExporter for Exporter {
    async fn export(&self, spans: Vec<Span>) -> Result<(), ExportError> {
        match self {
            Exporter::Off => Ok(()),
            Exporter::Otlp(exporter) => exporter.export(spans).await,
        }
    }

    async fn export_logs(&self, logs: Vec<LogRecord>) -> Result<(), ExportError> {
        match self {
            Exporter::Off => Ok(()),
            Exporter::Otlp(exporter) => exporter.export_logs(logs).await,
        }
    }
}
