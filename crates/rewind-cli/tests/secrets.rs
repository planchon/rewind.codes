//! `rewind secrets list|put|delete|usage` against a stand-in embedded server: the organization's
//! secrets, or a namespace's with `--namespace`; a value read from standard input, never from an
//! argument, and never printed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use rewind_api::admin::ServerInfo;
use rewind_cli::{Console, Exit};
use serde_json::{Value, json};

/// A request the stand-in saw: what, its body, and its namespace header.
#[derive(Debug, Clone)]
struct Seen {
    what: String,
    body: Value,
    namespace: Option<String>,
}

type Shared = Arc<Mutex<Vec<Seen>>>;

fn see(seen: &Shared, what: String, headers: &HeaderMap, body: Value) {
    seen.lock().unwrap().push(Seen {
        what,
        body,
        namespace: headers
            .get("rewind-namespace")
            .map(|value| value.to_str().unwrap().to_owned()),
    });
}

fn secret(reference: &str, namespace: Option<&str>, labels: Value) -> Value {
    let (scope, name) = reference.split_once('/').unwrap();
    let mut secret = json!({
        "name": name, "scope": scope, "reference": reference, "labels": labels,
        "createdBy": "key:ab", "createdAt": "2026-11-20T10:00:00Z",
        "updatedBy": "key:ab", "updatedAt": "2026-11-20T10:00:00Z",
        "reads": [{ "day": "2026-11-20", "reads": 3 }],
        "lastRead": { "run": "run-9", "namespace": "alice", "at": "2026-11-20T11:00:00Z" }
    });
    if let Some(namespace) = namespace {
        secret["namespace"] = json!(namespace);
    }
    secret
}

fn usage(reference: &str) -> Value {
    let version = format!("4be1c07a9d3e{}", "0".repeat(52));
    json!({
        "secret": secret(reference, None, json!({})),
        "activeRuns": ["run-1"], "activeTotal": 1, "waitingRuns": [], "waitingTotal": 0,
        "workflows": [{ "name": "triage", "digest": version, "servers": ["github"] }]
    })
}

/// A stand-in embedded server on `<dir>/.rewind/server.sock`.
struct Embedded {
    dir: tempfile::TempDir,
    seen: Shared,
}

impl Embedded {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join(".rewind");
        std::fs::create_dir_all(&state).unwrap();
        let listener = tokio::net::UnixListener::bind(state.join("server.sock")).unwrap();
        let seen: Shared = Arc::default();
        let app = Router::new()
            .route(
                "/v1/server",
                get(|| async {
                    axum::Json(ServerInfo {
                        version: rewind_cli::embedded::VERSION.into(),
                        epochs: vec![1],
                        node: "test".into(),
                        partitions: 1,
                        nodes: 1,
                        embedded: true,
                        virtual_time: false,
                        mcp_url: None,
                        oauth: false,
                    })
                }),
            )
            .route(
                "/v1/secrets",
                get(|State(seen): State<Shared>, headers: HeaderMap| async move {
                    see(&seen, "list".into(), &headers, Value::Null);
                    axum::Json(json!({ "secrets": [secret("org/zendesk", None, json!({ "env": "prod" }))] }))
                }),
            )
            .route(
                "/v1/secrets/{name}",
                get(|| async { StatusCode::METHOD_NOT_ALLOWED })
                    .put(
                        |State(seen): State<Shared>,
                         Path(name): Path<String>,
                         headers: HeaderMap,
                         axum::Json(body): axum::Json<Value>| async move {
                            see(&seen, format!("put {name}"), &headers, body.clone());
                            let labels = body.get("labels").cloned().unwrap_or(json!({}));
                            (
                                StatusCode::CREATED,
                                axum::Json(secret(&format!("org/{name}"), None, labels)),
                            )
                        },
                    )
                    .delete(
                        |State(seen): State<Shared>,
                         Path(name): Path<String>,
                         headers: HeaderMap| async move {
                            see(&seen, format!("delete {name}"), &headers, Value::Null);
                            StatusCode::NO_CONTENT
                        },
                    ),
            )
            .route(
                "/v1/secrets/{name}/usage",
                get(
                    |State(seen): State<Shared>,
                     Path(name): Path<String>,
                     headers: HeaderMap| async move {
                        see(&seen, format!("usage {name}"), &headers, Value::Null);
                        axum::Json(usage(&format!("org/{name}")))
                    },
                ),
            )
            .route(
                "/v1/namespaces/{namespace}/secrets",
                get(
                    |State(seen): State<Shared>,
                     Path(namespace): Path<String>,
                     headers: HeaderMap| async move {
                        see(&seen, format!("list of {namespace}"), &headers, Value::Null);
                        axum::Json(
                            json!({ "secrets": [secret("ns/github", Some(&namespace), json!({}))] }),
                        )
                    },
                ),
            )
            .route(
                "/v1/namespaces/{namespace}/secrets/{name}",
                get(|| async { StatusCode::METHOD_NOT_ALLOWED }).put(
                    |State(seen): State<Shared>,
                     Path((namespace, name)): Path<(String, String)>,
                     headers: HeaderMap,
                     axum::Json(body): axum::Json<Value>| async move {
                        see(&seen, format!("put {namespace}/{name}"), &headers, body);
                        (
                            StatusCode::CREATED,
                            axum::Json(secret(&format!("ns/{name}"), Some(&namespace), json!({}))),
                        )
                            .into_response()
                    },
                ),
            )
            .with_state(seen.clone());
        tokio::spawn(axum::serve(listener, app).into_future());
        Self { dir, seen }
    }

    async fn rewind(&self, args: &[&str], input: &str) -> (Exit, String, String) {
        let state = self.dir.path().join(".rewind");
        let mut argv = vec!["--dir".to_owned(), state.display().to_string()];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        let (console, captured) = Console::captured();
        let mut console = console.with_input(std::io::Cursor::new(input.to_owned()));
        let exit = rewind_cli::run(argv, &mut console).await;
        (exit, captured.stdout(), captured.stderr())
    }

    fn seen(&self) -> Vec<Seen> {
        std::mem::take(&mut self.seen.lock().unwrap())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_organization_s_secrets_are_listed_written_and_deleted() {
    let server = Embedded::start().await;
    let (exit, out, err) = server.rewind(&["secrets", "list"], "").await;
    assert_eq!(exit, Exit::Success, "{err}");
    assert!(out.contains("SECRET"), "{out}");
    assert!(
        out.contains("org/zendesk") && out.contains("env=prod"),
        "{out}"
    );
    assert!(out.contains("run run-9"), "the last read: {out}");
    let seen = server.seen();
    assert_eq!(seen[0].what, "list");
    assert_eq!(
        seen[0].namespace, None,
        "the organization's span namespaces"
    );

    let (exit, out, err) = server
        .rewind(
            &["secrets", "put", "github", "--label", "env=prod"],
            "ghp_from_stdin\n",
        )
        .await;
    assert_eq!(exit, Exit::Success, "{err}");
    assert_eq!(out, "secret org/github (env=prod) written\n");
    assert!(!out.contains("ghp_from_stdin") && !err.contains("ghp_from_stdin"));
    let seen = server.seen();
    assert_eq!(seen[0].what, "put github");
    assert_eq!(
        seen[0].body,
        json!({ "value": "ghp_from_stdin", "labels": { "env": "prod" } }),
        "the value from standard input, its trailing newline dropped"
    );

    let (exit, _, err) = server.rewind(&["secrets", "put", "github"], "").await;
    assert_eq!(exit, Exit::Usage, "{err}");
    assert!(err.contains("no value on standard input"), "{err}");
    let (exit, _, _) = server
        .rewind(&["secrets", "put", "github", "ghp_as_an_argument"], "x")
        .await;
    assert_eq!(exit, Exit::Usage, "never a value as an argument");

    let (exit, out, err) = server.rewind(&["secrets", "usage", "github"], "").await;
    assert_eq!(exit, Exit::Success, "{err}");
    assert!(out.contains("active runs using it: 1 (run-1)"), "{out}");
    assert!(out.contains("triage@4be1c07a9d3e for github"), "{out}");

    let (exit, out, err) = server.rewind(&["secrets", "delete", "github"], "").await;
    assert_eq!(exit, Exit::Success, "{err}");
    assert!(out.starts_with("secret github deleted"), "{out}");
    let whats: Vec<String> = server.seen().into_iter().map(|seen| seen.what).collect();
    assert_eq!(whats, ["usage github", "delete github"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_namespace_s_secrets_with_namespace() {
    let server = Embedded::start().await;
    let (exit, out, err) = server
        .rewind(&["--namespace", "alice", "secrets", "list"], "")
        .await;
    assert_eq!(exit, Exit::Success, "{err}");
    assert!(out.contains("ns/github"), "{out}");
    let (exit, out, err) = server
        .rewind(
            &["--json", "--namespace", "alice", "secrets", "put", "github"],
            "ghp_alice",
        )
        .await;
    assert_eq!(exit, Exit::Success, "{err}");
    let written: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(written["reference"], "ns/github");
    assert_eq!(written["namespace"], "alice");
    let seen = server.seen();
    assert_eq!(
        seen.iter()
            .map(|seen| seen.what.as_str())
            .collect::<Vec<_>>(),
        ["list of alice", "put alice/github"]
    );
    assert_eq!(seen[1].body, json!({ "value": "ghp_alice" }));

    let (exit, _, err) = server
        .rewind(
            &[
                "--namespace",
                "alice",
                "secrets",
                "put",
                "github",
                "--label",
                "a=b",
            ],
            "x",
        )
        .await;
    assert_eq!(exit, Exit::Usage, "{err}");
    assert!(err.contains("a namespace's secret takes no label"), "{err}");
}
