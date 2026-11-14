//! A server started in the test's process, on Turso in a temporary directory, its API over
//! HTTP with an admin key.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use base64::Engine as _;
use clap::Parser;
use rewind_api::Client;
use rewind_api::access::{BuiltInRole, CreateApiKey};
use rewind_api::common::Labels;
use rewind_server::{Running, ServerArgs};

pub(crate) const ADMIN_KEY: &str = "rewind_sk_admin_of_the_tests_0123456789abcdefghij";

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

    /// A server on `store`, with its blobs in `blobs`, whose files the caller keeps: another
    /// server may start on them once this one stopped.
    pub(crate) async fn start_in(
        dir: &std::path::Path,
        store: &str,
        blobs: &std::path::Path,
        extra: &[&str],
    ) -> Self {
        let scratch = tempfile::tempdir_in(dir).unwrap();
        Self::start_on(scratch, store, blobs, extra).await
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
            "rewind server",
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
        let running = rewind_server::start(args).await.unwrap();
        let url = running.url().unwrap();
        let admin = Client::remote(&url, Some(ADMIN_KEY))
            .unwrap()
            .with_namespace(test_namespace());
        Self {
            running: Some(running),
            url,
            admin,
            dir,
        }
    }

    /// A key of the tenant `acme` with this role.
    pub(crate) async fn key(&self, role: BuiltInRole, labels: &[(&str, &str)]) -> Client {
        self.key_of("acme", role, labels).await
    }

    pub(crate) async fn key_of(
        &self,
        tenant: &str,
        role: BuiltInRole,
        labels: &[(&str, &str)],
    ) -> Client {
        self.key_with(tenant, role.as_str(), labels).await
    }

    /// A key of `tenant` with `role`, built in or the tenant's, made by the operator.
    pub(crate) async fn key_with(
        &self,
        tenant: &str,
        role: &str,
        labels: &[(&str, &str)],
    ) -> Client {
        let key = self
            .admin
            .create_api_key(&CreateApiKey {
                tenant: Some(tenant.into()),
                name: format!("a {role} key of the tests"),
                role: role.into(),
                allowed_labels: labels.iter().copied().collect::<Labels>(),
            })
            .await
            .unwrap();
        self.client(key.key.as_deref())
    }

    /// A client of this server with `key`.
    pub(crate) fn client(&self, key: Option<&str>) -> Client {
        Client::remote(&self.url, key)
            .unwrap()
            .with_namespace(test_namespace())
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

pub(crate) fn wait(text: &str) -> Option<rewind_api::common::Duration> {
    Some(text.parse().unwrap())
}

/// Lookups whose key holds a NUL character (U+0000) match nothing, whatever the store: an
/// unknown token or id is not found, and a filter is refused with 400. Postgres refuses the
/// character in a query, and its error was answered 503 "the store cannot be reached".
pub(crate) async fn nul_lookups_match_nothing(server: &TestServer) {
    use rewind_api::ProblemType;
    use rewind_api::runs::RunFilter;
    use rewind_api::scripts::ScriptFilter;
    use rewind_api::tasks::{CompleteTask, DeferTask, FailTask, RejectTask, TaskQuery};
    use serde_json::json;

    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let worker = server.key(BuiltInRole::Worker, &[]).await;
    let answer = |error: rewind_api::Error| {
        let problem = error
            .problem()
            .cloned()
            .unwrap_or_else(|| panic!("not a problem: {error}"));
        (problem.kind(), problem.status, problem.detail)
    };
    let not_found = |error: rewind_api::Error| {
        let (kind, status, detail) = answer(error);
        assert_eq!(
            (kind, status),
            (Some(ProblemType::NotFound), 404),
            "{detail}"
        );
    };
    let gone = |error: rewind_api::Error| {
        let (kind, status, detail) = answer(error);
        assert_eq!(
            (kind, status),
            (Some(ProblemType::TaskGone), 410),
            "{detail}"
        );
    };
    let refused = |error: rewind_api::Error| {
        let (kind, status, detail) = answer(error);
        assert_eq!(
            (kind, status),
            (Some(ProblemType::InvalidRequest), 400),
            "{detail}"
        );
        assert!(detail.contains("NUL character"), "{detail}");
    };

    // Tokens: a webhook's URL, a host-tool task's claim.
    not_found(
        harness
            .post_hook("hook\u{0}token", &json!({ "ok": true }), None)
            .await
            .unwrap_err(),
    );
    let token = "task\u{0}token";
    gone(worker.heartbeat_task(token).await.unwrap_err());
    gone(
        worker
            .defer_task(token, &DeferTask::default())
            .await
            .unwrap_err(),
    );
    gone(
        worker
            .complete_task(token, &CompleteTask { result: json!(1) })
            .await
            .unwrap_err(),
    );
    gone(
        worker
            .fail_task(
                token,
                &FailTask {
                    message: "no".into(),
                    data: None,
                    retryable: false,
                },
            )
            .await
            .unwrap_err(),
    );

    // Ids: a task's, a subscription's, a run's.
    not_found(harness.approve_task("run\u{0}.1").await.unwrap_err());
    not_found(
        harness
            .reject_task("run\u{0}.1", &RejectTask::default())
            .await
            .unwrap_err(),
    );
    not_found(harness.delete_subscription("sub\u{0}").await.unwrap_err());
    not_found(harness.get_run("run\u{0}", None).await.unwrap_err());

    // Filters: labels and tools.
    for label in ["agent:tri\u{0}age", "ag\u{0}ent:triage"] {
        let runs = RunFilter {
            labels: vec![label.into()],
            ..RunFilter::default()
        };
        refused(harness.list_runs(&runs).await.unwrap_err());
        let scripts = ScriptFilter {
            labels: vec![label.into()],
            ..ScriptFilter::default()
        };
        refused(harness.list_scripts(&scripts).await.unwrap_err());
    }
    let runs = RunFilter {
        tool: Some("ask\u{0}".into()),
        ..RunFilter::default()
    };
    refused(harness.list_runs(&runs).await.unwrap_err());
    let scripts = ScriptFilter {
        tool: Some("ask\u{0}".into()),
        ..ScriptFilter::default()
    };
    refused(harness.list_scripts(&scripts).await.unwrap_err());
    let claim = TaskQuery {
        tools: vec!["ask_user".into(), "ask\u{0}".into()],
        wait: Some("1s".parse().unwrap()),
        ..TaskQuery::default()
    };
    refused(worker.claim_tasks(&claim).await.unwrap_err());
}

/// The namespace the tests' clients act for: starting a run names one.
pub(crate) fn test_namespace() -> rewind_api::namespaces::NamespaceName {
    "tests".parse().unwrap()
}
