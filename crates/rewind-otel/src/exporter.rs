use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use opentelemetry::InstrumentationScope;
use opentelemetry::logs::{Logger as _, LoggerProvider as _};
use opentelemetry_sdk::logs::{LogBatch, LogExporter, SdkLogger, SdkLoggerProvider};
use opentelemetry_sdk::trace::SpanExporter as SdkSpanExporter;
use rewind_engine::ports::{ExportError, LogRecord, Span, SpanExporter};
use tokio::sync::Mutex;

use crate::convert::{fill_log_record, span_data};
use crate::{OtelError, OtlpConfig, ServiceConfig, otlp};

/// The instrumentation scope of run telemetry: what workflows did, projected from the journal.
pub const RUN_SCOPE: &str = "rewind.run";

/// `SpanExporter` on OpenTelemetry: the spans and log records the projector derived from the
/// journal, sent over OTLP as they were derived. Ids, parents, times, statuses and attributes
/// are kept: a span exported twice is the same span.
///
/// Each call exports right away, without batching, and returns the backend's answer: a failure
/// leaves the feed item claimed, and the projector applies it again. The projector batches
/// already: one call per feed item.
pub struct OtelSpanExporter<
    S = opentelemetry_otlp::SpanExporter,
    L = opentelemetry_otlp::LogExporter,
> {
    // OpenTelemetry's exporters are never called concurrently.
    spans: Mutex<S>,
    logs: Mutex<L>,
    scope: InstrumentationScope,
    /// Creates empty log records: OpenTelemetry has no other way.
    records: SdkLogger,
    shut_down: AtomicBool,
}

impl OtelSpanExporter {
    /// Exports over OTLP, as `otlp` and the `OTEL_EXPORTER_OTLP_*` variables say. Called within a
    /// Tokio runtime, on which the exports then run.
    pub fn otlp(service: &ServiceConfig, otlp: &OtlpConfig) -> Result<Self, OtelError> {
        Ok(Self::new(
            otlp::span_exporter(otlp)?,
            otlp::log_exporter(otlp)?,
            service,
        ))
    }
}

impl<S: SdkSpanExporter, L: LogExporter> OtelSpanExporter<S, L> {
    /// Exports through the given OpenTelemetry exporters, with `service` as their resource.
    pub fn new(mut spans: S, mut logs: L, service: &ServiceConfig) -> Self {
        let resource = service.resource();
        spans.set_resource(&resource);
        logs.set_resource(&resource);
        let scope = InstrumentationScope::builder(RUN_SCOPE)
            .with_version(env!("CARGO_PKG_VERSION"))
            .build();
        Self {
            spans: Mutex::new(spans),
            logs: Mutex::new(logs),
            records: SdkLoggerProvider::builder()
                .build()
                .logger_with_scope(scope.clone()),
            scope,
            shut_down: AtomicBool::new(false),
        }
    }

    /// Shuts the exporters down: later exports fail. Shutting down twice does nothing.
    pub async fn shutdown(&self) -> Result<(), OtelError> {
        if self.shut_down.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let spans = self.spans.lock().await.shutdown();
        let logs = self.logs.lock().await.shutdown();
        spans
            .and(logs)
            .map_err(|error| OtelError::Shutdown(error.to_string()))
    }
}

impl<S, L> fmt::Debug for OtelSpanExporter<S, L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OtelSpanExporter")
            .field("scope", &self.scope.name())
            .finish_non_exhaustive()
    }
}

impl<S, L> OtelSpanExporter<S, L> {
    fn open(&self) -> Result<(), ExportError> {
        if self.shut_down.load(Ordering::SeqCst) {
            return Err(ExportError::Export("the OTLP exporter is shut down".into()));
        }
        Ok(())
    }
}

impl<S: SdkSpanExporter, L: LogExporter> SpanExporter for OtelSpanExporter<S, L> {
    async fn export(&self, spans: Vec<Span>) -> Result<(), ExportError> {
        self.open()?;
        if spans.is_empty() {
            return Ok(());
        }
        let batch = spans
            .into_iter()
            .map(|span| span_data(span, &self.scope))
            .collect();
        self.spans
            .lock()
            .await
            .export(batch)
            .await
            .map_err(|error| ExportError::Export(format!("OTLP spans: {error}")))
    }

    async fn export_logs(&self, logs: Vec<LogRecord>) -> Result<(), ExportError> {
        self.open()?;
        if logs.is_empty() {
            return Ok(());
        }
        let records: Vec<_> = logs
            .into_iter()
            .map(|log| {
                let mut record = self.records.create_log_record();
                fill_log_record(log, &mut record);
                record
            })
            .collect();
        let batch: Vec<_> = records.iter().map(|record| (record, &self.scope)).collect();
        self.logs
            .lock()
            .await
            .export(LogBatch::new(&batch))
            .await
            .map_err(|error| ExportError::Export(format!("OTLP logs: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry_sdk::logs::InMemoryLogExporter;
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use rewind_engine::conformance::spans::{self, Collected, Collector};

    use super::*;
    use crate::convert::back;

    /// The exporter, with the in-memory OpenTelemetry exporters behind it as its backend.
    struct InMemory {
        exporter: OtelSpanExporter<InMemorySpanExporter, InMemoryLogExporter>,
        spans: InMemorySpanExporter,
        logs: InMemoryLogExporter,
    }

    impl InMemory {
        fn new() -> Self {
            let spans = InMemorySpanExporter::default();
            let logs = InMemoryLogExporter::default();
            Self {
                exporter: OtelSpanExporter::new(
                    spans.clone(),
                    logs.clone(),
                    &ServiceConfig::default(),
                ),
                spans,
                logs,
            }
        }
    }

    impl SpanExporter for InMemory {
        async fn export(&self, spans: Vec<Span>) -> Result<(), ExportError> {
            self.exporter.export(spans).await
        }

        async fn export_logs(&self, logs: Vec<LogRecord>) -> Result<(), ExportError> {
            self.exporter.export_logs(logs).await
        }
    }

    impl Collector for InMemory {
        async fn collected(&self) -> Collected {
            let spans = self.spans.get_finished_spans().expect("spans");
            let logs = self.logs.get_emitted_logs().expect("logs");
            for span in &spans {
                assert_eq!(span.instrumentation_scope.name(), RUN_SCOPE);
                assert!(span.span_context.is_sampled());
            }
            for log in &logs {
                assert_eq!(log.instrumentation.name(), RUN_SCOPE);
            }
            Collected {
                spans: spans.iter().map(back::span).collect(),
                logs: logs.iter().map(|log| back::log(&log.record)).collect(),
            }
        }
    }

    #[tokio::test]
    async fn the_exporter_passes_the_conformance_suite() {
        spans::run_all(InMemory::new).await;
    }

    #[tokio::test]
    async fn the_resource_is_the_service_s() {
        let backend = InMemory::new();
        backend
            .export_logs(vec![LogRecord {
                trace_id: rewind_engine::ports::TraceId([1; 16]),
                span_id: None,
                time: rewind_core::Timestamp(1),
                level: rewind_core::LogLevel::Info,
                message: "hello".into(),
                attributes: Default::default(),
            }])
            .await
            .expect("export");
        let logs = backend.logs.get_emitted_logs().expect("logs");
        let name = logs[0].resource.get(&"service.name".into());
        if std::env::var("OTEL_SERVICE_NAME").is_err() {
            assert_eq!(name.map(|name| name.to_string()).as_deref(), Some("rewind"));
        }
    }

    #[tokio::test]
    async fn an_exporter_shut_down_refuses_exports() {
        let backend = InMemory::new();
        backend.exporter.shutdown().await.expect("shutdown");
        assert!(backend.spans.is_shutdown_called());
        let failed = backend
            .exporter
            .export(vec![Span {
                trace_id: rewind_engine::ports::TraceId([1; 16]),
                span_id: rewind_engine::ports::SpanId([2; 8]),
                parent: None,
                name: "run".into(),
                kind: rewind_engine::ports::SpanKind::Internal,
                start: rewind_core::Timestamp(1),
                end: rewind_core::Timestamp(2),
                status: rewind_engine::ports::SpanStatus::Unset,
                attributes: Default::default(),
            }])
            .await;
        assert!(matches!(failed, Err(ExportError::Export(_))), "{failed:?}");
    }
}
