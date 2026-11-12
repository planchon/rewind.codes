//! `install` sets the process's subscriber once. One test: the subscriber is global.

use ptc_otel::{OtelError, OtlpConfig, SubscriberConfig, install};

#[test]
fn the_subscriber_is_installed_once() {
    let invalid = install(&SubscriberConfig {
        filter: Some("ptc_engine=loud".into()),
        ..SubscriberConfig::default()
    });
    assert!(matches!(invalid, Err(OtelError::Config(_))), "{invalid:?}");

    // OTLP exports run on the Tokio runtime the subscriber is installed in.
    let outside = install(&SubscriberConfig {
        otlp: Some(OtlpConfig::default()),
        ..SubscriberConfig::default()
    });
    assert!(matches!(outside, Err(OtelError::NoRuntime)), "{outside:?}");

    let guard = install(&SubscriberConfig {
        filter: Some("info".into()),
        stderr: true,
        ..SubscriberConfig::default()
    })
    .expect("installed");
    tracing::info!("the subscriber is installed");
    let again = install(&SubscriberConfig::default());
    assert!(matches!(again, Err(OtelError::Install(_))), "{again:?}");
    guard.flush().expect("flush");
    guard.shutdown().expect("shutdown");
}
