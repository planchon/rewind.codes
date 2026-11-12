use std::io::IsTerminal as _;

use ptc_engine::telemetry::TelemetryConfig;
use tracing_subscriber::Layer as _;
use tracing_subscriber::filter::{EnvFilter, FilterExt as _, filter_fn};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

use crate::{OtelError, OtlpConfig, ServiceConfig, ServiceTelemetry, metrics};

/// The `tracing` subscriber a composition root installs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubscriberConfig {
    /// Which spans and logs are kept, in `RUST_LOG` syntax (`info,ptc_engine=debug`). Unset:
    /// `RUST_LOG`, else `info`. Measurements are always kept.
    pub filter: Option<String>,
    /// Human-readable logs on stderr.
    pub stderr: bool,
    /// Service telemetry over OTLP. Unset: nothing is exported.
    pub otlp: Option<OtlpConfig>,
    /// The resource of what is exported.
    pub service: ServiceConfig,
    /// The projector's configuration: a run's service spans are sampled as its projected spans.
    pub telemetry: TelemetryConfig,
}

/// Installs the process's `tracing` subscriber: logs on stderr, and service telemetry over OTLP
/// ([`ServiceTelemetry`]). Records of the `log` crate go through it too. The filter applies to
/// spans and logs; measurements are always recorded, and never written to stderr. Called once,
/// within a Tokio runtime when OTLP is configured.
///
/// Keep the guard for the life of the process: dropping it exports what is buffered and stops
/// the export.
pub fn install(config: &SubscriberConfig) -> Result<TelemetryGuard, OtelError> {
    let directives = match &config.filter {
        Some(directives) => directives.clone(),
        None => std::env::var("RUST_LOG")
            .ok()
            .filter(|directives| !directives.trim().is_empty())
            .unwrap_or_else(|| "info".into()),
    };
    // Each layer filters on its own: one filter per layer.
    let filter = || {
        EnvFilter::try_new(&directives)
            .map_err(|error| OtelError::Config(format!("the filter {directives:?}: {error}")))
    };
    let stderr_filter = filter()?;
    let service = config
        .otlp
        .as_ref()
        .map(|otlp| ServiceTelemetry::otlp(&config.service, otlp, config.telemetry.clone()))
        .transpose()?;
    let service_layer = service
        .as_ref()
        .map(|service| Ok::<_, OtelError>(service.filtered_layer(Some(filter()?))))
        .transpose()?;
    let stderr = config.stderr.then(|| {
        tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_ansi(std::io::stderr().is_terminal())
            .with_filter(
                stderr_filter.and(filter_fn(|metadata| !metrics::is_measurement(metadata))),
            )
    });
    tracing_subscriber::registry()
        .with(stderr)
        .with(service_layer)
        .try_init()
        .map_err(|error| OtelError::Install(error.to_string()))?;
    Ok(TelemetryGuard { service })
}

/// Keeps service telemetry running. Dropping it exports what is buffered and stops the export,
/// blocking like [`TelemetryGuard::shutdown`].
#[derive(Debug)]
#[must_use = "dropping the guard stops the export of service telemetry"]
pub struct TelemetryGuard {
    service: Option<ServiceTelemetry>,
}

impl TelemetryGuard {
    /// Exports what is buffered now.
    pub fn flush(&self) -> Result<(), OtelError> {
        self.service
            .as_ref()
            .map_or(Ok(()), ServiceTelemetry::flush)
    }

    /// Exports what is buffered and stops the export. Blocks until the backend answered: call it
    /// outside the runtime, or from a multi-threaded one, since the exports run on it.
    pub fn shutdown(mut self) -> Result<(), OtelError> {
        self.service
            .take()
            .map_or(Ok(()), |service| service.shutdown())
    }
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(service) = self.service.take()
            && let Err(error) = service.shutdown()
        {
            eprintln!("ai-ptc: service telemetry was not all exported: {error}");
        }
    }
}
