use std::collections::BTreeMap;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::resource::{EnvResourceDetector, TelemetryResourceDetector};

use crate::OtelError;

/// The service that emits telemetry: the OpenTelemetry resource of every span and log record.
///
/// `OTEL_SERVICE_NAME` and `OTEL_RESOURCE_ATTRIBUTES` take precedence over these values, as
/// OpenTelemetry specifies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// `service.name`: `rewind` by default.
    pub name: String,
    /// `service.version`: the version of the `rewind` binary.
    pub version: Option<String>,
    /// Other resource attributes: `service.instance.id`, `deployment.environment.name`…
    pub attributes: BTreeMap<String, String>,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            name: "rewind".into(),
            version: None,
            attributes: BTreeMap::new(),
        }
    }
}

impl ServiceConfig {
    pub(crate) fn resource(&self) -> Resource {
        let mut attributes = vec![KeyValue::new("service.name", self.name.clone())];
        if let Some(version) = &self.version {
            attributes.push(KeyValue::new("service.version", version.clone()));
        }
        attributes.extend(
            self.attributes
                .iter()
                .map(|(key, value)| KeyValue::new(key.clone(), value.clone())),
        );
        // A detected resource merged later wins: the SDK's own attributes first, then ours, then
        // the environment's.
        let mut builder = Resource::builder_empty()
            .with_detectors(&[Box::new(TelemetryResourceDetector)])
            .with_attributes(attributes)
            .with_detectors(&[Box::new(EnvResourceDetector::new())]);
        if let Some(name) = std::env::var("OTEL_SERVICE_NAME")
            .ok()
            .filter(|name| !name.trim().is_empty())
        {
            builder = builder.with_service_name(name);
        }
        builder.build()
    }
}

/// The OTLP transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpProtocol {
    /// OTLP over gRPC, on port 4317 by default.
    Grpc,
    /// OTLP over HTTP with protobuf bodies, on port 4318 by default.
    HttpProtobuf,
}

/// Where telemetry is sent over OTLP. What is left unset follows the standard
/// `OTEL_EXPORTER_OTLP_*` environment variables, then OpenTelemetry's defaults. Its `Debug`
/// output names the headers, not their values.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct OtlpConfig {
    /// The transport. Unset: `OTEL_EXPORTER_OTLP_PROTOCOL` (`grpc` or `http/protobuf`), else
    /// HTTP with protobuf, OpenTelemetry's default.
    pub protocol: Option<OtlpProtocol>,
    /// The collector's base URL, such as `http://localhost:4317`. Over HTTP, `/v1/traces` and
    /// `/v1/logs` are appended. Unset: the `OTEL_EXPORTER_OTLP_*ENDPOINT` variables, else the
    /// local collector.
    pub endpoint: Option<String>,
    /// Headers of every export (gRPC metadata), such as an API key. They add to those of
    /// `OTEL_EXPORTER_OTLP_*HEADERS`.
    pub headers: BTreeMap<String, String>,
    /// How long one export may take. Unset: `OTEL_EXPORTER_OTLP_*TIMEOUT`, else 10 s.
    pub timeout: Option<Duration>,
}

impl std::fmt::Debug for OtlpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OtlpConfig")
            .field("protocol", &self.protocol)
            .field("endpoint", &self.endpoint)
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl OtlpConfig {
    /// The transport this configuration resolves to.
    pub fn resolved_protocol(&self) -> Result<OtlpProtocol, OtelError> {
        if let Some(protocol) = self.protocol {
            return Ok(protocol);
        }
        match std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL") {
            Err(_) => Ok(OtlpProtocol::HttpProtobuf),
            Ok(value) => match value.trim() {
                "" | "http/protobuf" => Ok(OtlpProtocol::HttpProtobuf),
                "grpc" => Ok(OtlpProtocol::Grpc),
                other => Err(OtelError::Config(format!(
                    "OTEL_EXPORTER_OTLP_PROTOCOL is {other:?}: Rewind speaks `grpc` and \
                     `http/protobuf`"
                ))),
            },
        }
    }

    /// The endpoint of one signal over HTTP: the base URL and the signal's path.
    pub(crate) fn http_endpoint(&self, path: &str) -> Option<String> {
        self.endpoint
            .as_deref()
            .map(|base| format!("{}{path}", base.trim_end_matches('/')))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_resource_names_the_service() {
        let service = ServiceConfig {
            name: "rewind-worker".into(),
            version: Some("0.1.0".into()),
            attributes: BTreeMap::from([("deployment.environment.name".into(), "test".into())]),
        };
        let resource = service.resource();
        let get = |key: &'static str| resource.get(&key.into()).map(|value| value.to_string());
        if std::env::var("OTEL_SERVICE_NAME").is_err() {
            assert_eq!(get("service.name").as_deref(), Some("rewind-worker"));
        }
        assert_eq!(get("service.version").as_deref(), Some("0.1.0"));
        assert_eq!(get("deployment.environment.name").as_deref(), Some("test"));
        assert_eq!(get("telemetry.sdk.language").as_deref(), Some("rust"));
    }

    #[test]
    fn debug_output_names_headers_but_not_their_values() {
        let config = OtlpConfig {
            headers: BTreeMap::from([("x-api-key".into(), "secret-1234".into())]),
            ..OtlpConfig::default()
        };
        let debug = format!("{config:?}");
        assert!(debug.contains("x-api-key"), "{debug}");
        assert!(!debug.contains("secret-1234"), "{debug}");
    }

    #[test]
    fn http_endpoints_append_the_signal_s_path() {
        let config = OtlpConfig {
            endpoint: Some("https://otel.example.com:4318/".into()),
            ..OtlpConfig::default()
        };
        assert_eq!(
            config.http_endpoint("/v1/traces").as_deref(),
            Some("https://otel.example.com:4318/v1/traces")
        );
        assert_eq!(OtlpConfig::default().http_endpoint("/v1/logs"), None);
        let grpc = OtlpConfig {
            protocol: Some(OtlpProtocol::Grpc),
            ..OtlpConfig::default()
        };
        assert_eq!(grpc.resolved_protocol().unwrap(), OtlpProtocol::Grpc);
    }
}
