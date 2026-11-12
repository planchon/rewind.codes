//! `OtelSpanExporter` and `ServiceTelemetry` against an OpenTelemetry Collector in Docker, over
//! gRPC and over HTTP: the conformance suite of `SpanExporter`, read back from what the
//! collector received, and service spans joined to a run's trace.
//!
//! The collector writes what it receives as OTLP JSON to files the test reads. Each exporter the
//! suite creates reads only what arrived after it was created. The container has a unique name
//! and local ports, and is removed when the test ends. Without Docker the test is skipped, and
//! says so.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ptc_core::{LogLevel, RunId, Timestamp};
use ptc_engine::conformance::spans::{self, Collected, Collector};
use ptc_engine::ports::{
    AttributeValue, ExportError, LogRecord, Span, SpanExporter, SpanId, SpanKind, SpanStatus,
    TraceId,
};
use ptc_engine::telemetry::{SpanKey, TelemetryConfig, span_id};
use ptc_otel::{
    OtelSpanExporter, OtlpConfig, OtlpProtocol, RUN_SCOPE, SERVICE_SCOPE, ServiceConfig,
    ServiceTelemetry,
};
use serde_json::Value;
use tracing_subscriber::layer::SubscriberExt as _;

const IMAGE: &str = "otel/opentelemetry-collector:0.165.0";

const CONFIG: &str = "\
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
      http:
        endpoint: 0.0.0.0:4318
exporters:
  file/traces:
    path: /out/traces.jsonl
    flush_interval: 50ms
  file/logs:
    path: /out/logs.jsonl
    flush_interval: 50ms
service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [file/traces]
    logs:
      receivers: [otlp]
      exporters: [file/logs]
";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn otlp_exports_reach_an_opentelemetry_collector() {
    let Some(collector) = OtelCollector::start() else {
        return;
    };
    for protocol in [OtlpProtocol::Grpc, OtlpProtocol::HttpProtobuf] {
        eprintln!("over {protocol:?}");
        let otlp = collector.otlp(protocol);
        spans::run_all(|| OverOtlp {
            exporter: OtelSpanExporter::otlp(&ServiceConfig::default(), &otlp).expect("exporter"),
            files: collector.files(),
        })
        .await;
        service_spans_join_the_run_s_trace(&collector, &otlp).await;
    }
}

/// Service spans and logs, through the subscriber's pipeline, in the run's trace.
async fn service_spans_join_the_run_s_trace(collector: &OtelCollector, otlp: &OtlpConfig) {
    let files = collector.files();
    let service = ServiceConfig {
        name: "ptc-collector-test".into(),
        ..ServiceConfig::default()
    };
    let telemetry =
        ServiceTelemetry::otlp(&service, otlp, TelemetryConfig::default()).expect("telemetry");
    let run = RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0077);
    let trace = TraceId(*run.as_uuid().as_bytes());
    let own = span_id(run, SpanKey::Activation(3));
    let parent = span_id(run, SpanKey::Run);
    let subscriber = tracing_subscriber::registry().with(telemetry.layer());
    tracing::subscriber::with_default(subscriber, || {
        let activation = tracing::info_span!(
            "activation",
            trace_id = %trace,
            span_id = %own,
            parent_span_id = %parent,
        );
        activation.in_scope(|| tracing::info!(fence = 3_u64, "instance rebuilt"));
    });
    telemetry.shutdown().expect("shutdown");

    let collected = files.read_until_stable().await;
    let activation = collected
        .spans
        .iter()
        .find(|span| span.item.name == "activation")
        .expect("the activation span");
    assert_eq!(activation.item.trace_id, trace);
    assert_eq!(activation.item.span_id, own);
    assert_eq!(activation.item.parent, Some(parent));
    assert_eq!(activation.scope, SERVICE_SCOPE);
    if std::env::var("OTEL_SERVICE_NAME").is_err() {
        assert_eq!(activation.service.as_deref(), Some("ptc-collector-test"));
    }
    let log = collected
        .logs
        .iter()
        .find(|log| log.item.message == "instance rebuilt")
        .expect("the log");
    assert_eq!((log.item.trace_id, log.item.span_id), (trace, Some(own)));
    assert_eq!(log.item.level, LogLevel::Info);
    assert_eq!(
        log.item.attributes.get("fence"),
        Some(&AttributeValue::Int(3))
    );
}

/// The exporter under test, and the collector's files as its backend.
struct OverOtlp {
    exporter: OtelSpanExporter,
    files: Files,
}

impl SpanExporter for OverOtlp {
    async fn export(&self, spans: Vec<Span>) -> Result<(), ExportError> {
        self.exporter.export(spans).await
    }

    async fn export_logs(&self, logs: Vec<LogRecord>) -> Result<(), ExportError> {
        self.exporter.export_logs(logs).await
    }
}

impl Collector for OverOtlp {
    async fn collected(&self) -> Collected {
        let read = self.files.read_until_stable().await;
        for span in &read.spans {
            assert_eq!(span.scope, RUN_SCOPE);
        }
        for log in &read.logs {
            assert_eq!(log.scope, RUN_SCOPE);
        }
        Collected {
            spans: read.spans.into_iter().map(|span| span.item).collect(),
            logs: read.logs.into_iter().map(|log| log.item).collect(),
        }
    }
}

/// The collector's output files, from where they ended when this was created.
struct Files {
    traces: PathBuf,
    logs: PathBuf,
    from: (usize, usize),
}

#[derive(Debug, Default, PartialEq)]
struct Read {
    spans: Vec<Received<Span>>,
    logs: Vec<Received<LogRecord>>,
}

/// An item the collector received, with its resource's service name and its scope.
#[derive(Debug, PartialEq)]
struct Received<T> {
    service: Option<String>,
    scope: String,
    item: T,
}

impl Files {
    fn read(&self) -> Read {
        let traces = tail(&self.traces, self.from.0);
        let logs = tail(&self.logs, self.from.1);
        Read {
            spans: lines(&traces)
                .flat_map(|line| received(&line, "resourceSpans", "scopeSpans", "spans"))
                .map(|(service, scope, span)| Received {
                    service,
                    scope,
                    item: parse_span(&span),
                })
                .collect(),
            logs: lines(&logs)
                .flat_map(|line| received(&line, "resourceLogs", "scopeLogs", "logRecords"))
                .map(|(service, scope, log)| Received {
                    service,
                    scope,
                    item: parse_log(&log),
                })
                .collect(),
        }
    }

    /// What arrived, once the collector has written everything the exports sent.
    async fn read_until_stable(&self) -> Read {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut last = None;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let read = self.read();
            if last.as_ref() == Some(&read) {
                return read;
            }
            assert!(Instant::now() < deadline, "the collector never settled");
            last = Some(read);
        }
    }
}

fn tail(path: &Path, from: usize) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    text.get(from..).unwrap_or_default().to_owned()
}

/// The complete lines: the collector may be writing the last one.
fn lines(text: &str) -> impl Iterator<Item = Value> + '_ {
    text.split_inclusive('\n')
        .filter(|line| line.ends_with('\n') && !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a line of OTLP JSON"))
}

/// The items of one OTLP JSON document, with their resource's service name and their scope.
fn received(
    document: &Value,
    resources: &str,
    scopes: &str,
    items: &str,
) -> Vec<(Option<String>, String, Value)> {
    let mut received = Vec::new();
    for resource in document[resources].as_array().into_iter().flatten() {
        let service = attributes(&resource["resource"]["attributes"])
            .remove("service.name")
            .and_then(|value| match value {
                AttributeValue::String(name) => Some(name),
                _ => None,
            });
        for scope in resource[scopes].as_array().into_iter().flatten() {
            let name = scope["scope"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            for item in scope[items].as_array().into_iter().flatten() {
                received.push((service.clone(), name.clone(), item.clone()));
            }
        }
    }
    received
}

fn hex<const N: usize>(value: &Value) -> Option<[u8; N]> {
    let text = value.as_str().unwrap_or_default();
    if text.is_empty() {
        return None;
    }
    let mut bytes = [0u8; N];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).expect("hex");
    }
    (bytes != [0; N]).then_some(bytes)
}

fn millis(value: &Value) -> Timestamp {
    let nanos: u64 = match value {
        Value::String(text) => text.parse().expect("nanoseconds"),
        other => other.as_u64().expect("nanoseconds"),
    };
    // Run telemetry has whole milliseconds; service spans are timed by the process.
    Timestamp(nanos / 1_000_000)
}

fn any_value(value: &Value) -> AttributeValue {
    if let Some(text) = value.get("stringValue") {
        AttributeValue::String(text.as_str().expect("a string").to_owned())
    } else if let Some(n) = value.get("intValue") {
        AttributeValue::Int(match n {
            Value::String(text) => text.parse().expect("an integer"),
            other => other.as_i64().expect("an integer"),
        })
    } else if let Some(x) = value.get("doubleValue") {
        AttributeValue::Float(x.as_f64().expect("a double"))
    } else if let Some(b) = value.get("boolValue") {
        AttributeValue::Bool(b.as_bool().expect("a boolean"))
    } else {
        panic!("an attribute value the projector does not export: {value}")
    }
}

fn attributes(value: &Value) -> BTreeMap<String, AttributeValue> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|kv| {
            (
                kv["key"].as_str().expect("a key").to_owned(),
                any_value(&kv["value"]),
            )
        })
        .collect()
}

fn parse_span(span: &Value) -> Span {
    Span {
        trace_id: TraceId(hex(&span["traceId"]).expect("a trace id")),
        span_id: SpanId(hex(&span["spanId"]).expect("a span id")),
        parent: hex(&span["parentSpanId"]).map(SpanId),
        name: span["name"].as_str().expect("a name").to_owned(),
        kind: match span["kind"].as_i64() {
            Some(1) => SpanKind::Internal,
            Some(3) => SpanKind::Client,
            other => panic!("a span kind the projector does not export: {other:?}"),
        },
        start: millis(&span["startTimeUnixNano"]),
        end: millis(&span["endTimeUnixNano"]),
        status: match span["status"]["code"].as_i64() {
            None | Some(0) => SpanStatus::Unset,
            Some(1) => SpanStatus::Ok,
            Some(2) => SpanStatus::Error {
                message: span["status"]["message"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            },
            other => panic!("an unknown status code: {other:?}"),
        },
        attributes: attributes(&span["attributes"]),
    }
}

fn parse_log(log: &Value) -> LogRecord {
    let level = match log["severityNumber"].as_i64() {
        Some(1..=8) => LogLevel::Debug,
        Some(9..=12) => LogLevel::Info,
        Some(13..=16) => LogLevel::Warn,
        Some(17..=24) => LogLevel::Error,
        other => panic!("a log without severity: {other:?}"),
    };
    let message = match log.get("body") {
        None => String::new(),
        Some(body) => body["stringValue"].as_str().unwrap_or_default().to_owned(),
    };
    LogRecord {
        trace_id: TraceId(hex(&log["traceId"]).expect("a trace id")),
        span_id: hex(&log["spanId"]).map(SpanId),
        time: millis(&log["timeUnixNano"]),
        level,
        message,
        attributes: attributes(&log["attributes"]),
    }
}

/// An OpenTelemetry Collector in Docker, removed on drop.
struct OtelCollector {
    name: String,
    /// Holds the configuration and the output files.
    dir: tempfile::TempDir,
    grpc: String,
    http: String,
}

impl OtelCollector {
    /// Starts the collector; `None` when Docker is absent.
    fn start() -> Option<Self> {
        let version = Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .output();
        if !matches!(&version, Ok(output) if output.status.success()) {
            eprintln!(
                "SKIPPED: the OTLP test needs Docker to run an OpenTelemetry Collector, and \
                 Docker is absent or its daemon is not running"
            );
            return None;
        }

        let dir = tempfile::tempdir().expect("a temporary directory");
        let out = dir.path().join("out");
        fs::create_dir(&out).expect("the output directory");
        // The collector writes as its own user.
        let chmod = Command::new("chmod")
            .arg("0777")
            .arg(&out)
            .status()
            .expect("chmod");
        assert!(chmod.success());
        fs::write(dir.path().join("config.yaml"), CONFIG).expect("the configuration");

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let name = format!("ptc-otel-collector-{}-{nanos}", std::process::id());
        docker(&[
            "run",
            "--detach",
            "--rm",
            "--name",
            &name,
            "--publish",
            "127.0.0.1::4317",
            "--publish",
            "127.0.0.1::4318",
            "--volume",
            &format!(
                "{}:/etc/otelcol/config.yaml:ro",
                dir.path().join("config.yaml").display()
            ),
            "--volume",
            &format!("{}:/out", out.display()),
            IMAGE,
            "--config",
            "/etc/otelcol/config.yaml",
        ]);
        // From here on, dropping the collector removes the container.
        let mut collector = OtelCollector {
            name,
            dir,
            grpc: String::new(),
            http: String::new(),
        };
        collector.grpc = format!("http://{}", collector.port("4317/tcp"));
        collector.http = format!("http://{}", collector.port("4318/tcp"));
        collector.wait_until_ready();
        Some(collector)
    }

    fn port(&self, port: &str) -> String {
        let output = docker(&["port", &self.name, port]);
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .expect("a published port")
            .trim()
            .to_owned()
    }

    fn wait_until_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let logs = Command::new("docker")
                .args(["logs", &self.name])
                .output()
                .expect("docker logs");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&logs.stdout),
                String::from_utf8_lossy(&logs.stderr)
            );
            if text.contains("Everything is ready") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the collector did not start:\n{text}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn otlp(&self, protocol: OtlpProtocol) -> OtlpConfig {
        OtlpConfig {
            protocol: Some(protocol),
            endpoint: Some(match protocol {
                OtlpProtocol::Grpc => self.grpc.clone(),
                OtlpProtocol::HttpProtobuf => self.http.clone(),
            }),
            headers: BTreeMap::from([("x-ptc-test".to_owned(), "1".to_owned())]),
            timeout: Some(Duration::from_secs(10)),
        }
    }

    /// The output files, from where they end once the collector has written all it received.
    fn files(&self) -> Files {
        let out = self.dir.path().join("out");
        let traces = out.join("traces.jsonl");
        let logs = out.join("logs.jsonl");
        let len = |path: &Path| fs::metadata(path).map_or(0, |meta| meta.len() as usize);
        let mut from = (len(&traces), len(&logs));
        loop {
            std::thread::sleep(Duration::from_millis(200));
            let now = (len(&traces), len(&logs));
            if now == from {
                break;
            }
            from = now;
        }
        Files { traces, logs, from }
    }
}

impl Drop for OtelCollector {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .output();
    }
}

fn docker(args: &[&str]) -> Output {
    let output = Command::new("docker").args(args).output().expect("docker");
    assert!(
        output.status.success(),
        "docker {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
