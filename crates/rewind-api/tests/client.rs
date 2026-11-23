//! The client against a stand-in server, over TCP and over a unix socket: what it sends
//! (paths, API key, idempotency keys, query strings, bodies) and how it reads answers
//! (JSON, problem+json, empty bodies, server-sent events).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use rewind_api::common::{Labels, Payload, RunMode, RunStatus, Timestamp};
use rewind_api::events::{EventsQuery, StreamEvent};
use rewind_api::headers::IdempotencyKey;
use rewind_api::problem::{self, Problem, ProblemType};
use rewind_api::runs::{Run, RunFilter, StartRun};
use rewind_api::tasks::CompleteTask;
use rewind_api::{Client, Error, routes};
use serde_json::{Value, json};

/// What the stand-in server saw of the last request.
#[derive(Debug, Default, Clone)]
struct Seen {
    method: String,
    path: String,
    query: Option<String>,
    authorization: Option<String>,
    idempotency_key: Option<String>,
    namespace: Option<String>,
    organization: Option<String>,
    body: Value,
}

type Log = Arc<Mutex<Seen>>;

async fn record(State(log): State<Log>, request: Request) -> Seen {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .map(|value| value.to_str().unwrap().to_owned())
    };
    let seen = Seen {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        query: parts.uri.query().map(str::to_owned),
        authorization: header("authorization"),
        idempotency_key: header("idempotency-key"),
        namespace: header("rewind-namespace"),
        organization: header("rewind-organization"),
        body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    };
    *log.lock().unwrap() = seen.clone();
    seen
}

fn run(id: &str) -> Run {
    let at = Timestamp::from_second(1_762_682_531).unwrap();
    Run {
        id: id.into(),
        handle: "alice/script/06bqx3k9f2mt".into(),
        namespace: "alice".parse().unwrap(),
        status: RunStatus::Completed,
        quarantine: None,
        script: "aa".repeat(32),
        script_title: None,
        toolset: "bb".repeat(32),
        epoch: 1,
        mode: RunMode::Live,
        labels: Labels::from_iter([("agent", "triage")]),
        secrets: Default::default(),
        trace_id: "0".repeat(32),
        parent: None,
        workflow: None,
        started_by: None,
        input: Some(Payload::of(json!({}), 1024)),
        output: Some(Payload::of(json!({ "closed": 3 }), 1024)),
        error: None,
        abort: None,
        waiting_for: Vec::new(),
        logs: Vec::new(),
        attempts: Vec::new(),
        history_len: 4,
        created_at: at,
        updated_at: at,
        ended_at: Some(at),
        expires_at: at,
    }
}

fn problem_response(problem: Problem) -> Response {
    let status = StatusCode::from_u16(problem.status).unwrap();
    (
        status,
        [(header::CONTENT_TYPE, problem::CONTENT_TYPE)],
        serde_json::to_string(&problem).unwrap(),
    )
        .into_response()
}

fn app(log: Log) -> Router {
    Router::new()
        .route(
            routes::RUNS,
            post(|state: State<Log>, request: Request| async move {
                let seen = record(state, request).await;
                let id = format!("run-{}", seen.idempotency_key.as_deref().unwrap_or("new"));
                (StatusCode::CREATED, axum::Json(run(&id)))
            })
            .get(|state: State<Log>, request: Request| async move {
                record(state, request).await;
                axum::Json(json!({ "runs": [] }))
            }),
        )
        .route(
            routes::RUN,
            get(
                |Path(id): Path<String>, RawQuery(query): RawQuery| async move {
                    if id == "missing" {
                        return problem_response(
                            Problem::new(ProblemType::NotFound, format!("no run {id}"))
                                .with_instance(format!("/v1/runs/{id}")),
                        );
                    }
                    if id == "proxy" {
                        return (StatusCode::BAD_GATEWAY, "<html>bad gateway</html>").into_response();
                    }
                    let mut run = run(&id);
                    run.labels.insert("query".into(), query.unwrap_or_default());
                    axum::Json(run).into_response()
                },
            ),
        )
        .route(
            routes::RUN_SIGNAL,
            post(|state: State<Log>, request: Request| async move {
                let seen = record(state, request).await;
                if seen.path.contains("ended") {
                    return problem_response(Problem::new(
                        ProblemType::RunEnded,
                        "run ended has ended (completed)",
                    ));
                }
                (
                    StatusCode::ACCEPTED,
                    axum::Json(json!({ "runId": "r1", "name": "approval", "duplicate": false })),
                )
                    .into_response()
            }),
        )
        .route(
            routes::TASK_COMPLETE,
            post(|state: State<Log>, request: Request| async move {
                record(state, request).await;
                StatusCode::NO_CONTENT
            }),
        )
        .route(
            routes::RUN_EVENTS,
            get(|headers: HeaderMap, RawQuery(query): RawQuery| async move {
                assert_eq!(headers.get(header::ACCEPT).unwrap(), "text/event-stream");
                assert_eq!(query.as_deref(), Some("follow=true&from=1"));
                let entry = json!({
                    "type": "entry", "idx": 1, "recordedAt": "2026-11-09T10:02:11Z",
                    "event": { "type": "timer_fired", "seq": 2 }
                });
                let body = format!(
                    ": keep-alive\n\nevent: entry\nid: 1\ndata: {entry}\n\nevent: end\ndata: {}\n\n",
                    json!({ "type": "end", "status": "completed" })
                );
                ([(header::CONTENT_TYPE, "text/event-stream")], Body::from(body))
            }),
        )
        .with_state(log)
}

async fn serve_tcp() -> (Client, Log) {
    let log = Log::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = app(log.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = Client::remote(&format!("http://{address}/"), Some("rewind_sk_test")).unwrap();
    (client, log)
}

#[tokio::test]
async fn a_remote_client_sends_its_key_and_idempotency_keys() {
    let (client, log) = serve_tcp().await;
    let mut start = StartRun::new("export default async () => 1;");
    start.wait = Some("30s".parse().unwrap());
    let key = IdempotencyKey::new("toolu_01").unwrap();
    let run = client.start_run(&start, Some(&key)).await.unwrap();
    assert_eq!(run.id, "run-toolu_01");
    assert_eq!(run.output.unwrap().value, Some(json!({ "closed": 3 })));

    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.path, "/v1/runs");
    assert_eq!(seen.authorization.as_deref(), Some("Bearer rewind_sk_test"));
    assert_eq!(seen.idempotency_key.as_deref(), Some("toolu_01"));
    assert_eq!(seen.body["wait"], "30s");
    assert_eq!(seen.body["input"], json!({}));
}

#[tokio::test]
async fn a_connection_names_its_organization_and_namespace_on_every_request() {
    let (client, log) = serve_tcp().await;
    client.list_runs(&RunFilter::default()).await.unwrap();
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.organization, None);
    assert_eq!(seen.namespace, None);

    let connection = Client::remote(client.base_url().as_str(), Some("rewind_ct_abc"))
        .unwrap()
        .with_organization("org_01JBX")
        .unwrap()
        .with_namespace("alice".parse().unwrap());
    assert_eq!(connection.organization(), Some("org_01JBX"));
    connection.list_runs(&RunFilter::default()).await.unwrap();
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.authorization.as_deref(), Some("Bearer rewind_ct_abc"));
    assert_eq!(seen.organization.as_deref(), Some("org_01JBX"));
    assert_eq!(seen.namespace.as_deref(), Some("alice"));
    connection
        .start_run(&StartRun::new("x"), None)
        .await
        .unwrap();
    assert_eq!(
        log.lock().unwrap().organization.as_deref(),
        Some("org_01JBX")
    );

    assert!(client.with_organization("org\nX").is_err());
}

#[tokio::test]
async fn query_strings_are_written_as_the_server_reads_them() {
    let (client, log) = serve_tcp().await;
    let filter = RunFilter {
        status: Some(RunStatus::Failed),
        labels: Labels::from_iter([("agent", "triage"), ("conversation", "c 42")]).to_filters(),
        ..RunFilter::default()
    };
    client.list_runs(&filter).await.unwrap();
    let query = log.lock().unwrap().query.clone().unwrap();
    assert_eq!(
        query,
        "label=agent%3Atriage&label=conversation%3Ac+42&status=failed"
    );

    let run = client
        .get_run("r1", Some("1h30m".parse().unwrap()))
        .await
        .unwrap();
    assert_eq!(run.labels["query"], "wait=1h30m");
}

#[tokio::test]
async fn errors_are_problems_whatever_the_body() {
    let (client, _) = serve_tcp().await;
    let error = client.get_run("missing", None).await.unwrap_err();
    let problem = error.problem().unwrap();
    assert!(problem.is(ProblemType::NotFound));
    assert_eq!(problem.instance.as_deref(), Some("/v1/runs/missing"));

    let error = client.get_run("proxy", None).await.unwrap_err();
    let problem = error.problem().unwrap();
    assert_eq!(problem.status, 502);
    assert_eq!(problem.kind(), None);

    let error = client
        .signal_run("ended", "approval", &json!(true), None)
        .await
        .unwrap_err();
    assert!(error.problem().unwrap().is(ProblemType::RunEnded));
}

#[tokio::test]
async fn path_parameters_are_one_segment_each() {
    let (client, log) = serve_tcp().await;
    let accepted = client
        .signal_run("r1", "pr merged/1", &json!({ "close": true }), None)
        .await
        .unwrap();
    assert!(!accepted.duplicate);
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.path, "/v1/runs/r1/signals/pr%20merged%2F1");
    assert_eq!(seen.body, json!({ "close": true }));

    // A run's handle holds `/`: it is one segment too, which the server reads back whole.
    let run = client
        .get_run("dana/triage/06bqx3k9f2mt", None)
        .await
        .unwrap();
    assert_eq!(run.id, "dana/triage/06bqx3k9f2mt");
}

#[tokio::test]
async fn the_event_stream_skips_keep_alives_and_reads_each_event() {
    let (client, _) = serve_tcp().await;
    let query = EventsQuery {
        follow: Some(true),
        from: Some(1),
    };
    let events: Vec<StreamEvent> = client
        .run_events("r1", &query)
        .await
        .unwrap()
        .map(Result::unwrap)
        .collect()
        .await;
    assert_eq!(events.len(), 2);
    match &events[0] {
        StreamEvent::Entry(entry) => {
            assert_eq!(entry.idx, 1);
            assert_eq!(entry.event.name(), "timer_fired");
        }
        other => panic!("expected an entry, got {other:?}"),
    }
    assert_eq!(
        events[1],
        StreamEvent::End {
            status: RunStatus::Completed
        }
    );
}

#[tokio::test]
async fn a_server_that_is_not_there_is_a_transport_error() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let client = Client::remote(&format!("http://{address}"), None).unwrap();
    let error = client.get_run("r1", None).await.unwrap_err();
    assert!(matches!(error, Error::Transport { .. }), "{error:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn the_embedded_client_talks_over_the_socket_without_a_key() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("server.sock");
    let log = Log::default();
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let router = app(log.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let client = Client::embedded(&socket).unwrap();
    client
        .complete_task(
            "tok-1",
            &CompleteTask {
                result: json!([1, 2]),
            },
        )
        .await
        .unwrap();
    let seen = log.lock().unwrap().clone();
    assert_eq!(seen.path, "/v1/tasks/tok-1/complete");
    assert_eq!(seen.authorization, None);
    assert_eq!(seen.body, json!({ "result": [1, 2] }));

    let run = client.start_run(&StartRun::new("x"), None).await.unwrap();
    assert_eq!(run.id, "run-new");
}

/// Every path of the API registers in one axum router: parameters at the same place share
/// their position, as the router requires.
#[test]
fn every_route_fits_in_one_router() {
    let mut router: Router = Router::new();
    let mut paths: Vec<&str> = routes::OPERATIONS.iter().map(|(_, path)| *path).collect();
    paths.sort_unstable();
    paths.dedup();
    for path in paths {
        router = router.route(path, axum::routing::any(|| async { StatusCode::OK }));
    }
    drop(router);
}
