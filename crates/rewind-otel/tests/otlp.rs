//! OTLP exports to a collector that is not there fail, over gRPC and over HTTP: the projector
//! then leaves the feed item claimed, and applies it again.

use std::net::TcpListener;
use std::time::Duration;

use rewind_core::{LogLevel, Timestamp};
use rewind_engine::ports::{
    ExportError, LogRecord, Span, SpanExporter, SpanId, SpanKind, SpanStatus, TraceId,
};
use rewind_otel::{OtelError, OtelSpanExporter, OtlpConfig, OtlpProtocol, ServiceConfig};

/// A local address nothing listens on.
fn closed_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
    let address = listener.local_addr().expect("its address");
    drop(listener);
    format!("http://{address}")
}

fn span() -> Span {
    Span {
        trace_id: TraceId([7; 16]),
        span_id: SpanId([8; 8]),
        parent: None,
        name: "run".into(),
        kind: SpanKind::Internal,
        start: Timestamp(1_767_225_600_000),
        end: Timestamp(1_767_225_601_000),
        status: SpanStatus::Ok,
        attributes: Default::default(),
    }
}

fn log() -> LogRecord {
    LogRecord {
        trace_id: TraceId([7; 16]),
        span_id: None,
        time: Timestamp(1_767_225_600_000),
        level: LogLevel::Info,
        message: "hello".into(),
        attributes: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exports_to_an_absent_collector_fail() {
    for protocol in [OtlpProtocol::Grpc, OtlpProtocol::HttpProtobuf] {
        let otlp = OtlpConfig {
            protocol: Some(protocol),
            endpoint: Some(closed_address()),
            timeout: Some(Duration::from_secs(1)),
            ..OtlpConfig::default()
        };
        let exporter = OtelSpanExporter::otlp(&ServiceConfig::default(), &otlp).expect("built");
        let spans = exporter.export(vec![span()]).await;
        assert!(
            matches!(spans, Err(ExportError::Export(_))),
            "{protocol:?}: {spans:?}"
        );
        let logs = exporter.export_logs(vec![log()]).await;
        assert!(
            matches!(logs, Err(ExportError::Export(_))),
            "{protocol:?}: {logs:?}"
        );
    }
}

#[test]
fn exporters_need_a_runtime() {
    let built = OtelSpanExporter::otlp(&ServiceConfig::default(), &OtlpConfig::default());
    assert_eq!(built.err(), Some(OtelError::NoRuntime));
}
