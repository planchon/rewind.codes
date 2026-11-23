//! A server started in the test's process, on Turso in a temporary directory, its API over
//! HTTP with an admin key.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use clap::Parser;
use rewind_api::Client;
use rewind_api::access::{BuiltInRole, CreateApiKey};
use rewind_api::auth::AuthState;
use rewind_api::common::Labels;
use rewind_core::Timestamp;
use rewind_engine::adapters::{MemoryIdentity, SystemClock};
use rewind_engine::ports::Clock;
use rewind_server::{Running, ServerArgs};

pub(crate) const ADMIN_KEY: &str = "rewind_sk_admin_of_the_tests_0123456789abcdefghij";

/// The four `WORKOS_*` of a server whose sign-in the tests drive: the client id and the API key
/// are not used with the fake identity provider.
pub(crate) const WORKOS: [&str; 8] = [
    "--workos-client-id",
    "client_of_the_tests",
    "--workos-api-key",
    "sk_test_of_the_tests",
    "--workos-cookie-password",
    "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    "--workos-redirect-uri",
    "https://app.rewind.test/auth/callback",
];

/// The fake identity provider's clock: real time, moved by an offset the test sets, so that its
/// tokens expire when the test needs while the server reads real time.
#[derive(Debug, Default)]
pub(crate) struct SkewedClock {
    offset_ms: AtomicI64,
}

impl SkewedClock {
    /// The clock runs `offset` milliseconds ahead (behind when negative) of real time.
    pub(crate) fn set(&self, offset_ms: i64) {
        self.offset_ms.store(offset_ms, Ordering::SeqCst);
    }
}

impl Clock for SkewedClock {
    fn now(&self) -> Timestamp {
        let now = SystemClock.now().as_millis();
        let offset = self.offset_ms.load(Ordering::SeqCst);
        Timestamp::from_millis(now.saturating_add_signed(offset))
    }
}

/// A fake identity provider, and the handle on its clock.
pub(crate) fn identity() -> (MemoryIdentity, Arc<SkewedClock>) {
    let clock = Arc::new(SkewedClock::default());
    (MemoryIdentity::new(clock.clone()), clock)
}

/// A cookie as the server set it.
#[derive(Debug, Clone)]
pub(crate) struct Cookie {
    pub(crate) value: String,
    /// What follows the value: `Path=/auth; Max-Age=…; HttpOnly; …`.
    pub(crate) attributes: String,
}

/// A browser of the dashboard: a cookie jar over HTTP that follows no redirect and sends the
/// cookies of `Path=/auth` to `/auth` only.
pub(crate) struct Browser {
    http: reqwest::Client,
    url: String,
    pub(crate) cookies: BTreeMap<String, Cookie>,
}

impl Browser {
    pub(crate) fn new(url: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            url: url.to_owned(),
            cookies: BTreeMap::new(),
        }
    }

    fn cookie_header(&self, path: &str) -> Option<String> {
        let sent: Vec<String> = self
            .cookies
            .iter()
            .filter(|_| path.starts_with("/auth"))
            .map(|(name, cookie)| format!("{name}={}", cookie.value))
            .collect();
        (!sent.is_empty()).then(|| sent.join("; "))
    }

    /// Keeps what `Set-Cookie` says: a cookie set, or cleared (`Max-Age=0`).
    fn keep(&mut self, response: &reqwest::Response) {
        for header in response.headers().get_all(reqwest::header::SET_COOKIE) {
            let text = header.to_str().unwrap();
            let (pair, attributes) = text.split_once(';').unwrap_or((text, ""));
            let (name, value) = pair.split_once('=').unwrap();
            if attributes.contains("Max-Age=0") {
                self.cookies.remove(name.trim());
            } else {
                self.cookies.insert(
                    name.trim().to_owned(),
                    Cookie {
                        value: value.trim().to_owned(),
                        attributes: attributes.trim().to_owned(),
                    },
                );
            }
        }
    }

    pub(crate) async fn get(&mut self, path: &str) -> reqwest::Response {
        let mut request = self.http.get(format!("{}{path}", self.url));
        if let Some(cookies) = self.cookie_header(path) {
            request = request.header(reqwest::header::COOKIE, cookies);
        }
        let response = request.send().await.unwrap();
        self.keep(&response);
        response
    }

    /// A `POST` of `body` with `content_type`.
    pub(crate) async fn post_raw(
        &mut self,
        path: &str,
        content_type: Option<&str>,
        body: &str,
    ) -> reqwest::Response {
        let mut request = self
            .http
            .post(format!("{}{path}", self.url))
            .body(body.to_owned());
        if let Some(content_type) = content_type {
            request = request.header(reqwest::header::CONTENT_TYPE, content_type);
        }
        if let Some(cookies) = self.cookie_header(path) {
            request = request.header(reqwest::header::COOKIE, cookies);
        }
        let response = request.send().await.unwrap();
        self.keep(&response);
        response
    }

    /// A `POST` of JSON, as the dashboard sends it.
    pub(crate) async fn post(&mut self, path: &str, body: serde_json::Value) -> reqwest::Response {
        self.post_raw(path, Some("application/json"), &body.to_string())
            .await
    }

    /// A step that answers an `AuthState` (200), or the problem's status and type.
    pub(crate) async fn step(
        &mut self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<AuthState, (u16, String)> {
        let response = self.post(path, body).await;
        answer(response).await
    }

    /// `POST /auth/session`.
    pub(crate) async fn session(&mut self) -> Result<AuthState, (u16, String)> {
        self.step("/auth/session", serde_json::json!({})).await
    }

    /// `POST /auth/session` for `organization`.
    pub(crate) async fn switch(&mut self, organization: &str) -> Result<AuthState, (u16, String)> {
        self.step(
            "/auth/session",
            serde_json::json!({ "organizationId": organization }),
        )
        .await
    }

    /// Signs in with the code the fake provider sent to `email`; the step `POST /auth/magic-auth`
    /// answers when it sends none.
    pub(crate) async fn sign_in(
        &mut self,
        identity: &MemoryIdentity,
        email: &str,
        invitation_token: Option<&str>,
    ) -> Result<AuthState, (u16, String)> {
        let mut request = serde_json::json!({ "email": email });
        if let Some(token) = invitation_token {
            request["invitationToken"] = token.into();
        }
        let sent = self.post("/auth/magic-auth", request.clone()).await;
        if sent.status() == 200 {
            return answer(sent).await;
        }
        assert_eq!(sent.status(), 202);
        request["code"] = identity.last_code(email).unwrap().into();
        self.step("/auth/magic-auth/verify", request).await
    }
}

/// An `AuthState`, or the problem's status and `type`.
pub(crate) async fn answer(response: reqwest::Response) -> Result<AuthState, (u16, String)> {
    let status = response.status().as_u16();
    if status == 200 {
        return Ok(response.json().await.unwrap());
    }
    let problem: serde_json::Value = response.json().await.unwrap_or_default();
    Err((
        status,
        problem["type"]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches("urn:rewind:problem:")
            .to_owned(),
    ))
}

/// The access token of a signed-in state, and its organization.
pub(crate) fn token(state: &AuthState) -> (String, Option<String>) {
    match state {
        AuthState::Authenticated {
            access_token,
            organization_id,
            ..
        } => (access_token.clone(), organization_id.clone()),
        other => panic!("not signed in: {other:?}"),
    }
}

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
        let store = format!("turso:{}", dir.path().join("control.db").display());
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
        Self::start_with(dir, store, blobs, extra, None).await
    }

    /// A server with both roles on Turso, whose sign-in's identity provider is `identity`.
    pub(crate) async fn signed_in(identity: &MemoryIdentity, extra: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = format!("turso:{}", dir.path().join("control.db").display());
        let blobs = dir.path().join("blobs");
        Self::start_with(dir, &store, &blobs, extra, Some(identity.clone())).await
    }

    /// A server with both roles on the engine's in-memory adapters of the store's ports, its
    /// blobs in a temporary directory, with these extra options.
    pub(crate) async fn in_memory(extra: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        let blobs = dir.path().join("blobs").display().to_string();
        let mut args = vec![
            "rewind server",
            "--bucket",
            &blobs,
            "--master-key",
            &key,
            "--admin-key",
            ADMIN_KEY,
            "--admin-tenant",
            "acme",
            "--listen",
            "127.0.0.1:0",
            "--executors",
            "2",
            "--calls",
            "4",
        ];
        args.extend_from_slice(extra);
        let args = ServerArgs::try_parse_from(args).unwrap();
        let running = rewind_server::start_in_memory(args, None).await.unwrap();
        let url = running.url().unwrap();
        let admin = Client::remote(&url, Some(ADMIN_KEY)).unwrap();
        Self {
            running: Some(running),
            url,
            admin,
            dir,
        }
    }

    /// A server on `store`, with its blobs in `blobs`; with `identity`, the dashboard's sign-in
    /// (the four `WORKOS_*`) on that identity provider.
    pub(crate) async fn start_with(
        dir: tempfile::TempDir,
        store: &str,
        blobs: &std::path::Path,
        extra: &[&str],
        identity: Option<MemoryIdentity>,
    ) -> Self {
        let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        let blobs = blobs.display().to_string();
        let mut args = vec![
            "rewind server",
            "--store",
            store,
            "--bucket",
            &blobs,
            "--master-key",
            &key,
            "--admin-key",
            ADMIN_KEY,
            "--admin-tenant",
            "acme",
            "--listen",
            "127.0.0.1:0",
            "--executors",
            "2",
            "--calls",
            "4",
        ];
        if store.starts_with("postgres") {
            // A node on Postgres links to the other nodes of its deployment.
            args.extend_from_slice(&[
                "--cluster-secret",
                "the-cluster-secret-of-the-tests-0123456789",
                "--link-listen",
                "127.0.0.1:0",
            ]);
        }
        args.extend_from_slice(extra);
        if identity.is_some() {
            args.extend_from_slice(&WORKOS);
        }
        let args = ServerArgs::try_parse_from(args).unwrap();
        let running = match identity {
            Some(identity) => rewind_server::start_with_identity(args, identity)
                .await
                .unwrap(),
            None => rewind_server::start(args).await.unwrap(),
        };
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

    /// A key of `tenant` with `role` whose requests name no namespace: they read every
    /// namespace of the tenant.
    pub(crate) async fn tenant_key(&self, tenant: &str, role: &str) -> Client {
        let key = self
            .admin
            .create_api_key(&CreateApiKey {
                tenant: Some(tenant.into()),
                name: format!("a {role} key of the tests"),
                role: role.into(),
                allowed_labels: Labels::new(),
            })
            .await
            .unwrap();
        Client::remote(&self.url, key.key.as_deref()).unwrap()
    }

    /// A client of this server with `key`, whose requests name no namespace: an end user's
    /// token names its own.
    pub(crate) fn bare(&self, key: &str) -> Client {
        Client::remote(&self.url, Some(key)).unwrap()
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
    use rewind_api::tasks::{CompleteTask, DeferTask, FailTask, RejectTask, TaskStreamQuery};
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

    // Tokens: a webhook's URL, a host-tool task's hand-out.
    not_found(
        harness
            .post_hook("hook\u{0}token", &json!({ "ok": true }), None)
            .await
            .unwrap_err(),
    );
    let token = "task\u{0}token";
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
    not_found(harness.revoke_token("ab\u{0}").await.unwrap_err());
    not_found(harness.delete_token_key("rk_\u{0}").await.unwrap_err());
    not_found(harness.get_run("run\u{0}", None).await.unwrap_err());

    // Filters: labels and tools.
    for label in ["agent:tri\u{0}age", "ag\u{0}ent:triage"] {
        let runs = RunFilter {
            labels: vec![label.into()],
            ..RunFilter::default()
        };
        refused(harness.list_runs(&runs).await.unwrap_err());
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
    let stream = TaskStreamQuery {
        tools: vec!["ask_user".into(), "ask\u{0}".into()],
        ..TaskStreamQuery::default()
    };
    refused(worker.task_stream(&stream).await.unwrap_err());
}

/// A worker's task stream, as the SDK's worker holds it open.
pub(crate) struct TaskWorker {
    stream: rewind_api::TaskStream,
}

impl TaskWorker {
    /// Opens a stream for the tasks `query` names.
    pub(crate) async fn open(client: &Client, query: &rewind_api::tasks::TaskStreamQuery) -> Self {
        Self {
            stream: client.task_stream(query).await.unwrap(),
        }
    }

    /// The next event of the stream, `None` when none comes within `wait` or the stream ends.
    pub(crate) async fn event(
        &mut self,
        wait: Duration,
    ) -> Option<rewind_api::tasks::TaskStreamEvent> {
        use futures_util::StreamExt as _;
        match tokio::time::timeout(wait, self.stream.next()).await {
            Ok(Some(event)) => Some(event.unwrap()),
            _ => None,
        }
    }

    /// The next task handed, within 30 s; `gone` events before it are skipped.
    pub(crate) async fn task(&mut self) -> rewind_api::tasks::HandedTask {
        loop {
            match self.event(Duration::from_secs(30)).await {
                Some(rewind_api::tasks::TaskStreamEvent::Task(task)) => return *task,
                Some(rewind_api::tasks::TaskStreamEvent::Gone(_)) => {}
                None => panic!("no task was handed within 30 s"),
            }
        }
    }

    /// Whether no task is handed within `wait`.
    pub(crate) async fn none_within(&mut self, wait: Duration) -> bool {
        loop {
            match self.event(wait).await {
                Some(rewind_api::tasks::TaskStreamEvent::Task(_)) => return false,
                Some(rewind_api::tasks::TaskStreamEvent::Gone(_)) => {}
                None => return true,
            }
        }
    }
}

/// The namespace the tests' clients act for: starting a run names one.
pub(crate) fn test_namespace() -> rewind_api::namespaces::NamespaceName {
    "tests".parse().unwrap()
}

/// A Postgres container, removed on drop.
pub(crate) struct Postgres {
    name: String,
    address: String,
}

impl Postgres {
    pub(crate) fn start() -> Option<Self> {
        let docker = Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .output();
        if !matches!(&docker, Ok(output) if output.status.success()) {
            eprintln!("SKIPPED: the Postgres tests of rewind-server need Docker, which is absent");
            assert!(
                std::env::var_os("REWIND_REQUIRE_DOCKER").is_none(),
                "REWIND_REQUIRE_DOCKER is set: Docker must be running"
            );
            return None;
        }
        // Tests run in parallel within one process, and the clock may only have microseconds:
        // a counter keeps two containers of the same process from sharing a name.
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!(
            "rewind-server-postgres-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        run(&[
            "run",
            "--detach",
            "--rm",
            "--name",
            &name,
            "--publish",
            "127.0.0.1::5432",
            "--tmpfs",
            "/var/lib/postgresql/data",
            "--env",
            "POSTGRES_USER=rewind",
            "--env",
            "POSTGRES_PASSWORD=rewind-secret",
            "postgres:17",
        ]);
        let mut postgres = Postgres {
            name,
            address: String::new(),
        };
        postgres.address = run(&["port", &postgres.name, "5432/tcp"])
            .lines()
            .next()
            .unwrap()
            .trim()
            .to_owned();
        let deadline = Instant::now() + Duration::from_secs(60);
        while !Command::new("docker")
            .args([
                "exec",
                &postgres.name,
                "psql",
                "-h",
                "127.0.0.1",
                "-U",
                "rewind",
                "-d",
                "postgres",
                "-tAc",
                "SELECT 1",
            ])
            .output()
            .is_ok_and(|output| output.status.success())
        {
            assert!(Instant::now() < deadline, "Postgres did not start in time");
            std::thread::sleep(Duration::from_millis(200));
        }
        Some(postgres)
    }

    pub(crate) fn url(&self) -> String {
        format!("postgres://rewind:rewind-secret@{}/postgres", self.address)
    }

    /// The database `postgres`, as `pg_dump` writes it in plain SQL.
    pub(crate) fn pg_dump(&self) -> String {
        run(&[
            "exec",
            &self.name,
            "pg_dump",
            "-U",
            "rewind",
            "--no-owner",
            "--no-privileges",
            "postgres",
        ])
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "--force", &self.name])
            .output();
    }
}

fn run(args: &[&str]) -> String {
    let output = Command::new("docker").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "docker {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}
