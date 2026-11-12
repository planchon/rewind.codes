//! The OTLP exporters, over gRPC or HTTP, and the wrapper that runs their exports on Tokio.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, HeaderName, HeaderValue};
use opentelemetry::Context;
use opentelemetry::context::FutureExt as _;
use opentelemetry_otlp::tonic_types::metadata::MetadataMap;
use opentelemetry_otlp::{
    Protocol, WithExportConfig as _, WithHttpConfig as _, WithTonicConfig as _,
};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::metrics::Temporality;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use tokio::runtime::Handle;

use crate::{OtelError, OtlpConfig, OtlpProtocol};

/// The OTLP span exporter `config` describes. Called within a Tokio runtime: the gRPC client
/// starts its connection there.
pub(crate) fn span_exporter(
    config: &OtlpConfig,
) -> Result<opentelemetry_otlp::SpanExporter, OtelError> {
    runtime()?;
    let builder = opentelemetry_otlp::SpanExporter::builder();
    let built = match config.resolved_protocol()? {
        OtlpProtocol::Grpc => {
            let mut builder = builder.with_tonic();
            if let Some(endpoint) = &config.endpoint {
                builder = builder.with_endpoint(endpoint.clone());
            }
            if let Some(timeout) = config.timeout {
                builder = builder.with_timeout(timeout);
            }
            if !config.headers.is_empty() {
                builder = builder.with_metadata(metadata(config)?);
            }
            builder.build()
        }
        OtlpProtocol::HttpProtobuf => {
            let mut builder = builder.with_http().with_protocol(Protocol::HttpBinary);
            if let Some(endpoint) = config.http_endpoint("/v1/traces") {
                builder = builder.with_endpoint(endpoint);
            }
            if let Some(timeout) = config.timeout {
                builder = builder.with_timeout(timeout);
            }
            if !config.headers.is_empty() {
                builder = builder.with_headers(headers(config));
            }
            builder.build()
        }
    };
    built.map_err(|error| OtelError::Build(error.to_string()))
}

/// The OTLP log exporter `config` describes. Called within a Tokio runtime.
pub(crate) fn log_exporter(
    config: &OtlpConfig,
) -> Result<opentelemetry_otlp::LogExporter, OtelError> {
    runtime()?;
    let builder = opentelemetry_otlp::LogExporter::builder();
    let built = match config.resolved_protocol()? {
        OtlpProtocol::Grpc => {
            let mut builder = builder.with_tonic();
            if let Some(endpoint) = &config.endpoint {
                builder = builder.with_endpoint(endpoint.clone());
            }
            if let Some(timeout) = config.timeout {
                builder = builder.with_timeout(timeout);
            }
            if !config.headers.is_empty() {
                builder = builder.with_metadata(metadata(config)?);
            }
            builder.build()
        }
        OtlpProtocol::HttpProtobuf => {
            let mut builder = builder.with_http().with_protocol(Protocol::HttpBinary);
            if let Some(endpoint) = config.http_endpoint("/v1/logs") {
                builder = builder.with_endpoint(endpoint);
            }
            if let Some(timeout) = config.timeout {
                builder = builder.with_timeout(timeout);
            }
            if !config.headers.is_empty() {
                builder = builder.with_headers(headers(config));
            }
            builder.build()
        }
    };
    built.map_err(|error| OtelError::Build(error.to_string()))
}

/// The OTLP metric exporter `config` describes. Called within a Tokio runtime. Its temporality
/// is `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE`, else cumulative.
pub(crate) fn metric_exporter(
    config: &OtlpConfig,
) -> Result<opentelemetry_otlp::MetricExporter, OtelError> {
    runtime()?;
    let builder = opentelemetry_otlp::MetricExporter::builder();
    let built = match config.resolved_protocol()? {
        OtlpProtocol::Grpc => {
            let mut builder = builder.with_tonic();
            if let Some(endpoint) = &config.endpoint {
                builder = builder.with_endpoint(endpoint.clone());
            }
            if let Some(timeout) = config.timeout {
                builder = builder.with_timeout(timeout);
            }
            if !config.headers.is_empty() {
                builder = builder.with_metadata(metadata(config)?);
            }
            builder.build()
        }
        OtlpProtocol::HttpProtobuf => {
            let mut builder = builder.with_http().with_protocol(Protocol::HttpBinary);
            if let Some(endpoint) = config.http_endpoint("/v1/metrics") {
                builder = builder.with_endpoint(endpoint);
            }
            if let Some(timeout) = config.timeout {
                builder = builder.with_timeout(timeout);
            }
            if !config.headers.is_empty() {
                builder = builder.with_headers(headers(config));
            }
            builder.build()
        }
    };
    built.map_err(|error| OtelError::Build(error.to_string()))
}

/// The Tokio runtime the exporters run on.
pub(crate) fn runtime() -> Result<Handle, OtelError> {
    Handle::try_current().map_err(|_| OtelError::NoRuntime)
}

fn headers(config: &OtlpConfig) -> HashMap<String, String> {
    config
        .headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn metadata(config: &OtlpConfig) -> Result<MetadataMap, OtelError> {
    let mut headers = HeaderMap::new();
    for (name, value) in &config.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| OtelError::Config(format!("{name:?} is not a valid header name")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| OtelError::Config(format!("the value of {name:?} is not valid")))?;
        headers.insert(name, value);
    }
    Ok(MetadataMap::from_headers(headers))
}

/// Runs an exporter's exports as tasks on a Tokio runtime, with telemetry suppressed.
///
/// The SDK's batch processors export from their own threads, where the async HTTP and gRPC
/// clients have no runtime. Suppression keeps what the export itself logs from being exported.
pub(crate) struct OnRuntime<E> {
    exporter: Arc<E>,
    runtime: Handle,
}

impl<E> OnRuntime<E> {
    pub(crate) fn new(exporter: E, runtime: Handle) -> Self {
        Self {
            exporter: Arc::new(exporter),
            runtime,
        }
    }

    async fn run<F>(&self, export: impl FnOnce(Arc<E>) -> F) -> OTelSdkResult
    where
        F: Future<Output = OTelSdkResult> + Send + 'static,
    {
        let exporter = Arc::clone(&self.exporter);
        let suppressed = Context::current().with_telemetry_suppressed();
        self.runtime
            .spawn(export(exporter).with_context(suppressed))
            .await
            .map_err(|error| OTelSdkError::InternalFailure(format!("the export task: {error}")))?
    }
}

impl<E> fmt::Debug for OnRuntime<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OnRuntime").finish_non_exhaustive()
    }
}

impl<E: SpanExporter + 'static> SpanExporter for OnRuntime<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.run(move |exporter| async move { exporter.export(batch).await })
            .await
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.exporter.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.exporter.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        if let Some(exporter) = Arc::get_mut(&mut self.exporter) {
            exporter.set_resource(resource);
        }
    }
}

impl<E: LogExporter + 'static> LogExporter for OnRuntime<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        // The batch borrows the processor's records: the task gets its own copy.
        let records: Vec<_> = batch
            .iter()
            .map(|(record, scope)| (record.clone(), scope.clone()))
            .collect();
        self.run(move |exporter| async move {
            let borrowed: Vec<_> = records
                .iter()
                .map(|(record, scope)| (record, scope))
                .collect();
            exporter.export(LogBatch::new(&borrowed)).await
        })
        .await
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.exporter.shutdown_with_timeout(timeout)
    }

    fn set_resource(&mut self, resource: &Resource) {
        if let Some(exporter) = Arc::get_mut(&mut self.exporter) {
            exporter.set_resource(resource);
        }
    }
}

/// The SDK's periodic reader exports from its own thread and lends the batch for the export's
/// duration: the export cannot become a task of its own, so this thread drives it on the runtime
/// until it ends.
impl<E: PushMetricExporter> PushMetricExporter for OnRuntime<E> {
    async fn export(&self, metrics: &ResourceMetrics) -> OTelSdkResult {
        let suppressed = Context::current().with_telemetry_suppressed();
        if Handle::try_current().is_ok() {
            return Err(OTelSdkError::InternalFailure(
                "metrics are exported from the reader's thread, outside the runtime".into(),
            ));
        }
        self.runtime
            .block_on(self.exporter.export(metrics).with_context(suppressed))
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.exporter.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.exporter.shutdown_with_timeout(timeout)
    }

    fn temporality(&self) -> Temporality {
        self.exporter.temporality()
    }
}
