//! `rewind namespaces list|show|quotas|delete` against a stand-in embedded server: what the
//! commands send, what they print, and that they span namespaces unless `--namespace` names one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rewind_api::admin::{ServerInfo, ServerRole};
use rewind_api::common::Timestamp;
use rewind_api::namespaces::{
    Namespace, NamespaceDefaults, NamespaceDetail, NamespaceList, PutNamespace,
    PutNamespaceDefaults, QuotaUsage, Quotas,
};
use rewind_cli::{Console, Exit};
use serde_json::{Value, json};

/// A request the stand-in server saw: what, its query or body, and its namespace header.
#[derive(Debug, Clone)]
struct Seen {
    what: String,
    query: BTreeMap<String, String>,
    namespace: Option<String>,
    body: Value,
}

#[derive(Debug, Default)]
struct Stub {
    exceptions: Quotas,
    defaults: Quotas,
    seen: Vec<Seen>,
}

type Shared = Arc<Mutex<Stub>>;

fn at() -> Timestamp {
    "2026-11-14T10:00:00Z".parse().unwrap()
}

fn see(
    stub: &Shared,
    what: &str,
    headers: &HeaderMap,
    query: BTreeMap<String, String>,
    body: Value,
) {
    stub.lock().unwrap().seen.push(Seen {
        what: what.to_owned(),
        query,
        namespace: headers
            .get("rewind-namespace")
            .map(|value| value.to_str().unwrap().to_owned()),
        body,
    });
}

fn namespace(name: &str, exceptions: &Quotas) -> Namespace {
    let defaults = Quotas {
        active_runs: Some(10),
        runs_per_month: Some(1_000),
        ..Quotas::default()
    };
    Namespace {
        name: name.parse().unwrap(),
        limits: Quotas {
            cpu_per_month: exceptions.cpu_per_month,
            active_runs: exceptions.active_runs.or(defaults.active_runs),
            runs_per_month: exceptions.runs_per_month.or(defaults.runs_per_month),
            scripts: exceptions.scripts,
            max_ttl: exceptions.max_ttl,
            max_script_lines: exceptions.max_script_lines,
        },
        exceptions: exceptions.clone(),
        usage: QuotaUsage {
            cpu: "12m".parse().unwrap(),
            active_runs: 2,
            runs_started: 840,
            scripts: 3,
        },
        created_at: at(),
        last_active_at: at(),
        deleting_since: None,
    }
}

fn detail(name: &str, exceptions: &Quotas) -> NamespaceDetail {
    NamespaceDetail {
        namespace: namespace(name, exceptions),
        month: "2026-11".into(),
        days: Vec::new(),
        alerts: Vec::new(),
        tokens: 2,
        tokens_revoked_before: None,
    }
}

async fn list(
    State(stub): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    see(&stub, "list", &headers, query, Value::Null);
    let exceptions = stub.lock().unwrap().exceptions.clone();
    axum::Json(NamespaceList {
        month: "2026-11".into(),
        namespaces: vec![namespace("alice", &exceptions)],
        total: 2,
        next: Some("840-alice".into()),
    })
    .into_response()
}

async fn get_one(
    State(stub): State<Shared>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    see(
        &stub,
        &format!("get {name}"),
        &headers,
        BTreeMap::new(),
        Value::Null,
    );
    if name != "alice" {
        let problem = rewind_api::Problem::new(rewind_api::ProblemType::NotFound, "no namespace");
        return (
            StatusCode::NOT_FOUND,
            [("content-type", rewind_api::problem::CONTENT_TYPE)],
            serde_json::to_string(&problem).unwrap(),
        )
            .into_response();
    }
    let exceptions = stub.lock().unwrap().exceptions.clone();
    axum::Json(detail(&name, &exceptions)).into_response()
}

async fn put_one(
    State(stub): State<Shared>,
    Path(name): Path<String>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    see(
        &stub,
        &format!("put {name}"),
        &headers,
        BTreeMap::new(),
        body.clone(),
    );
    let put: PutNamespace = serde_json::from_value(body).unwrap();
    stub.lock().unwrap().exceptions = put.exceptions.clone();
    axum::Json(detail(&name, &put.exceptions)).into_response()
}

async fn delete_one(
    State(stub): State<Shared>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    see(
        &stub,
        &format!("delete {name}"),
        &headers,
        BTreeMap::new(),
        Value::Null,
    );
    StatusCode::ACCEPTED.into_response()
}

fn defaults_of(defaults: &Quotas) -> NamespaceDefaults {
    NamespaceDefaults {
        defaults: defaults.clone(),
        ceiling: Quotas {
            active_runs: Some(10_000),
            ..Quotas::default()
        },
        month: "2026-11".into(),
        usage: QuotaUsage::default(),
    }
}

async fn get_defaults(State(stub): State<Shared>, headers: HeaderMap) -> Response {
    see(
        &stub,
        "get defaults",
        &headers,
        BTreeMap::new(),
        Value::Null,
    );
    let defaults = stub.lock().unwrap().defaults.clone();
    axum::Json(defaults_of(&defaults)).into_response()
}

async fn put_defaults(
    State(stub): State<Shared>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    see(
        &stub,
        "put defaults",
        &headers,
        BTreeMap::new(),
        body.clone(),
    );
    let put: PutNamespaceDefaults = serde_json::from_value(body).unwrap();
    stub.lock().unwrap().defaults = put.defaults.clone();
    axum::Json(defaults_of(&put.defaults)).into_response()
}

/// A stand-in embedded server on `<dir>/.rewind/server.sock`.
struct Embedded {
    dir: tempfile::TempDir,
    stub: Shared,
}

impl Embedded {
    async fn start(stub: Stub) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join(".rewind");
        std::fs::create_dir_all(&state).unwrap();
        let listener = tokio::net::UnixListener::bind(state.join("server.sock")).unwrap();
        let stub = Arc::new(Mutex::new(stub));
        let app = Router::new()
            .route(
                "/v1/server",
                get(|| async {
                    axum::Json(ServerInfo {
                        version: rewind_cli::embedded::VERSION.into(),
                        epochs: vec![1],
                        roles: vec![ServerRole::Api, ServerRole::Worker],
                        embedded: true,
                        virtual_time: false,
                    })
                }),
            )
            .route("/v1/namespaces", get(list))
            .route(
                "/v1/namespaces/{name}",
                get(get_one).put(put_one).delete(delete_one),
            )
            .route(
                "/v1/organization/namespace-defaults",
                get(get_defaults).put(put_defaults),
            )
            .with_state(stub.clone());
        tokio::spawn(axum::serve(listener, app).into_future());
        Self { dir, stub }
    }

    async fn rewind(&self, args: &[&str]) -> (Exit, String, String) {
        let state = self.dir.path().join(".rewind");
        let mut argv = vec!["--dir".to_owned(), state.display().to_string()];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        let (mut console, captured) = Console::captured();
        let exit = rewind_cli::run(argv, &mut console).await;
        (exit, captured.stdout(), captured.stderr())
    }

    fn seen(&self) -> Vec<Seen> {
        std::mem::take(&mut self.stub.lock().unwrap().seen)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn namespaces_are_listed_and_shown_across_namespaces() {
    let server = Embedded::start(Stub::default()).await;
    let (exit, out, err) = server
        .rewind(&[
            "namespaces",
            "list",
            "--above",
            "0.8",
            "--order",
            "runs",
            "--limit",
            "1",
        ])
        .await;
    assert_eq!(exit, Exit::Success, "{err}");
    assert!(out.contains("NAME"), "{out}");
    assert!(out.contains("alice"), "{out}");
    assert!(
        out.contains("840/1000"),
        "the runs of the month against the limit: {out}"
    );
    assert!(out.contains("2/10"), "{out}");
    assert!(err.contains("--after 840-alice"), "{err}");
    let seen = server.seen();
    assert_eq!(seen[0].what, "list");
    assert_eq!(seen[0].query.get("above").map(String::as_str), Some("0.8"));
    assert_eq!(seen[0].query.get("order").map(String::as_str), Some("runs"));
    assert_eq!(
        seen[0].namespace, None,
        "a command across namespaces is bound to none by default, not to local"
    );

    let (exit, out, err) = server.rewind(&["namespaces", "show", "alice"]).await;
    assert_eq!(exit, Exit::Success, "{err}");
    assert!(out.contains("namespace alice"), "{out}");
    assert!(out.contains("runs_per_month"), "{out}");
    assert!(out.contains("tokens in use: 2"), "{out}");

    let (exit, out, _) = server
        .rewind(&[
            "--json",
            "--namespace",
            "alice",
            "namespaces",
            "show",
            "alice",
        ])
        .await;
    assert_eq!(exit, Exit::Success);
    let shown: NamespaceDetail = serde_json::from_str(&out).unwrap();
    assert_eq!(shown.tokens, 2);
    assert_eq!(server.seen()[1].namespace.as_deref(), Some("alice"));

    let (exit, _, err) = server.rewind(&["namespaces", "show", "bob"]).await;
    assert_eq!(exit, Exit::Refused, "{err}");
    let (exit, _, _) = server.rewind(&["namespaces", "show", "not a name"]).await;
    assert_eq!(exit, Exit::Usage);
}

#[tokio::test(flavor = "multi_thread")]
async fn quotas_change_only_the_limits_given() {
    let server = Embedded::start(Stub {
        exceptions: Quotas {
            scripts: Some(9),
            max_ttl: Some("1h".parse().unwrap()),
            ..Quotas::default()
        },
        defaults: Quotas {
            max_script_lines: Some(300),
            ..Quotas::default()
        },
        seen: Vec::new(),
    })
    .await;

    // Without a limit, the namespace's limits are printed.
    let (exit, out, err) = server.rewind(&["namespaces", "quotas", "alice"]).await;
    assert_eq!(exit, Exit::Success, "{err}");
    assert!(out.contains("EXCEPTION"), "{out}");
    assert_eq!(server.seen().len(), 1);

    let (exit, out, err) = server
        .rewind(&[
            "namespaces",
            "quotas",
            "alice",
            "--active-runs",
            "5",
            "--max-ttl",
            "none",
        ])
        .await;
    assert_eq!(exit, Exit::Success, "{err}");
    assert!(out.contains("active_runs"), "{out}");
    let seen = server.seen();
    assert_eq!(seen[0].what, "get alice");
    assert_eq!(seen[1].what, "put alice");
    assert_eq!(
        seen[1].body,
        json!({ "exceptions": { "activeRuns": 5, "scripts": 9 } }),
        "the exceptions given change, the others stay, none removes one"
    );

    // A namespace not created yet starts without exceptions.
    let (exit, _, err) = server
        .rewind(&["namespaces", "quotas", "bob", "--scripts", "3"])
        .await;
    assert_eq!(exit, Exit::Success, "{err}");
    let seen = server.seen();
    assert_eq!(seen[1].body, json!({ "exceptions": { "scripts": 3 } }));

    // The defaults.
    let (exit, out, err) = server
        .rewind(&[
            "namespaces",
            "quotas",
            "--defaults",
            "--cpu-per-month",
            "2h",
        ])
        .await;
    assert_eq!(exit, Exit::Success, "{err}");
    assert!(out.contains("CEILING"), "{out}");
    let seen = server.seen();
    assert_eq!(seen[0].what, "get defaults");
    assert_eq!(
        seen[1].body,
        json!({ "defaults": { "cpuPerMonth": "2h", "maxScriptLines": 300 } })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_namespace_is_erased() {
    let server = Embedded::start(Stub::default()).await;
    let (exit, out, err) = server.rewind(&["namespaces", "delete", "alice"]).await;
    assert_eq!(exit, Exit::Success, "{err}");
    assert!(out.contains("namespace alice is being erased"), "{out}");
    assert_eq!(server.seen()[0].what, "delete alice");
    let (exit, out, _) = server
        .rewind(&["--json", "namespaces", "delete", "alice"])
        .await;
    assert_eq!(exit, Exit::Success);
    let answer: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(answer, json!({ "namespace": "alice", "erasing": true }));
}
