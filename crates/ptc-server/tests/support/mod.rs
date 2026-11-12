//! A server started in the test's process, on Turso in a temporary directory, its API over
//! HTTP with an admin key.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use base64::Engine as _;
use clap::Parser;
use ptc_api::Client;
use ptc_api::admin::{CreateApiKey, Role};
use ptc_api::common::Labels;
use ptc_server::{Running, ServerArgs};

pub(crate) const ADMIN_KEY: &str = "ptc_sk_admin_of_the_tests_0123456789abcdefghij";

pub(crate) struct TestServer {
    pub(crate) running: Option<Running>,
    pub(crate) url: String,
    pub(crate) admin: Client,
    pub(crate) dir: tempfile::TempDir,
}

impl TestServer {
    /// A server with both roles and these extra options, on Turso.
    pub(crate) async fn start(extra: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = format!("turso:{}", dir.path().join("journal.db").display());
        let blobs = dir.path().join("blobs");
        Self::start_on(dir, &store, &blobs, extra).await
    }

    /// A server on `store`, with its blobs in `blobs`.
    pub(crate) async fn start_on(
        dir: tempfile::TempDir,
        store: &str,
        blobs: &std::path::Path,
        extra: &[&str],
    ) -> Self {
        let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        let blobs = blobs.display().to_string();
        let mut args = vec![
            "ptc server",
            "--store",
            store,
            "--blobs",
            &blobs,
            "--master-key",
            &key,
            "--admin-key",
            ADMIN_KEY,
            "--admin-tenant",
            "acme",
            "--listen",
            "127.0.0.1:0",
            "--activations",
            "2",
            "--dispatches",
            "4",
        ];
        args.extend_from_slice(extra);
        let args = ServerArgs::try_parse_from(args).unwrap();
        let running = ptc_server::start(args).await.unwrap();
        let url = running.url().unwrap();
        let admin = Client::remote(&url, Some(ADMIN_KEY)).unwrap();
        Self {
            running: Some(running),
            url,
            admin,
            dir,
        }
    }

    /// A key of the tenant `acme` with this role.
    pub(crate) async fn key(&self, role: Role, labels: &[(&str, &str)]) -> Client {
        self.key_of("acme", role, labels).await
    }

    pub(crate) async fn key_of(&self, tenant: &str, role: Role, labels: &[(&str, &str)]) -> Client {
        let key = self
            .admin
            .create_api_key(&CreateApiKey {
                tenant: tenant.into(),
                role,
                allowed_labels: labels.iter().copied().collect::<Labels>(),
                name: Some("test".into()),
            })
            .await
            .unwrap();
        Client::remote(&self.url, key.key.as_deref()).unwrap()
    }

    pub(crate) async fn stop(mut self) {
        if let Some(running) = self.running.take() {
            running.stop().await;
        }
    }
}

/// Polls `check` until it returns `Some`, for up to 30 seconds.
pub(crate) async fn eventually<T, F, Fut>(mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the condition did not hold within 30 s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub(crate) fn wait(text: &str) -> Option<ptc_api::common::Duration> {
    Some(text.parse().unwrap())
}
