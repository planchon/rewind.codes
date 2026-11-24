//! The commands that go through the API, against a stand-in server: on the embedded server's
//! socket (the launcher finds it running, of this CLI's version), and remote over TCP with an
//! API key. What the commands send, what they print, and their exit codes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rewind_api::action_sets::{ActionSet, CreateActionSet, Declarations};
use rewind_api::admin::ServerInfo;
use rewind_api::checks::{CheckRequest, CheckResult, Diagnostic, Location, Severity};
use rewind_api::common::ErrorInfo;
use rewind_api::common::{Callsite, Labels, Payload, RunMode, RunStatus, Timestamp};
use rewind_api::problem::{self, Problem, ProblemType};
use rewind_api::runs::{
    CancelRun, ForkRun, ForkSource, Run, RunPage, RunSummary, SignalAccepted, StartRun, Transcript,
    Waiting,
};
use rewind_cli::{Console, Exit};
use serde_json::{Value, json};

const ACTION_SET_ID: &str = "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4";

/// What the stand-in server saw and holds.
#[derive(Debug, Default)]
struct Stub {
    runs: BTreeMap<String, Run>,
    /// Reads without a wait left before a run completes; `u32::MAX`: it never ends by itself. A
    /// read with a wait is held until the run ends, or the wait elapses.
    reads_left: BTreeMap<String, u32>,
    /// Runs asked to cancel, with the reason: they end cancelled by the next held read.
    cancelling: BTreeMap<String, String>,
    requests: Vec<Seen>,
}

#[derive(Debug, Clone)]
struct Seen {
    what: String,
    query: BTreeMap<String, String>,
    idempotency_key: Option<String>,
    authorization: Option<String>,
    namespace: Option<String>,
    body: Value,
}

type Shared = Arc<Mutex<Stub>>;

fn at() -> Timestamp {
    "2026-11-09T10:02:11Z".parse().unwrap()
}

/// A run of the stub, whose handle is `local/script/<id>`.
fn base_run(id: &str, status: RunStatus) -> Run {
    Run {
        id: id.into(),
        handle: format!("local/script/{id}"),
        namespace: "local".parse().unwrap(),
        status,
        quarantine: None,
        code_digest: "aa".repeat(32),
        action_set: "bb".repeat(32),
        epoch: 1,
        mode: RunMode::Live,
        labels: Labels::new(),
        secrets: Default::default(),
        trace_id: "0".repeat(32),
        source: None,
        workflow: None,
        started_by: None,
        input: Some(Payload::of(json!({}), 1024)),
        output: None,
        error: None,
        abort: None,
        waiting_for: Vec::new(),
        logs: Vec::new(),
        deliveries: Vec::new(),
        history_len: 2,
        created_at: at(),
        updated_at: at(),
        ended_at: None,
        expires_at: at(),
    }
}

fn completed(mut run: Run, output: Value) -> Run {
    run.status = RunStatus::Completed;
    run.output = Some(Payload::of(output, 262_144));
    run.waiting_for.clear();
    run
}

fn problem(kind: ProblemType, detail: &str) -> Response {
    let problem = Problem::new(kind, detail);
    (
        StatusCode::from_u16(problem.status).unwrap(),
        [(header::CONTENT_TYPE, problem::CONTENT_TYPE)],
        serde_json::to_string(&problem).unwrap(),
    )
        .into_response()
}

fn see(
    stub: &Shared,
    what: impl Into<String>,
    headers: &HeaderMap,
    query: BTreeMap<String, String>,
    body: Value,
) {
    let header = |name: &str| {
        headers
            .get(name)
            .map(|value| value.to_str().unwrap().to_owned())
    };
    stub.lock().unwrap().requests.push(Seen {
        what: what.into(),
        query,
        idempotency_key: header("idempotency-key"),
        authorization: header("authorization"),
        namespace: header("rewind-namespace"),
        body,
    });
}

/// The run a code asks for: a code naming `fails`, `waits` or `refused` gets that.
async fn start(
    State(stub): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    see(&stub, "start", &headers, query, body.clone());
    let start: StartRun = serde_json::from_value(body).unwrap();
    if start.code.contains("refused") {
        let problem = Problem::new(ProblemType::InvalidCode, "the code does not type-check")
            .with_diagnostics(vec![Diagnostic {
                severity: Severity::Error,
                code: "TS2322".into(),
                message: "Type 'number' is not assignable to type 'string'.".into(),
                location: Some(Location {
                    line: 3,
                    column: 28,
                    end_line: None,
                    end_column: None,
                }),
                frame: None,
                hint: None,
            }]);
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            [(header::CONTENT_TYPE, problem::CONTENT_TYPE)],
            serde_json::to_string(&problem).unwrap(),
        )
            .into_response();
    }
    let mut stub = stub.lock().unwrap();
    let id = format!("run-{}", stub.runs.len() + 1);
    let mut run = base_run(&id, RunStatus::Active);
    run.labels = start.labels.clone();
    run.input = Some(Payload::of(start.input.clone(), 262_144));
    if start.code.contains("fails") {
        run.status = RunStatus::Failed;
        run.error = Some(ErrorInfo {
            name: "TypeError".into(),
            message: "issue is undefined".into(),
            stack: None,
            location: Some(Callsite { line: 5, column: 3 }),
            frame: Some("5 | issue.close()".into()),
            failed_call: None,
        });
    } else if start.code.contains("waits") {
        run.waiting_for = vec![Waiting::Signal {
            name: "approval".into(),
            timeout_at: None,
            callsite: Some(Callsite { line: 4, column: 9 }),
        }];
        // Never ends by itself.
        stub.reads_left.insert(id.clone(), u32::MAX);
    } else {
        // Ends after two reads.
        stub.reads_left.insert(id.clone(), 2);
    }
    stub.runs.insert(id, run.clone());
    axum::Json(run).into_response()
}

async fn get_run(
    State(stub): State<Shared>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    see(
        &stub,
        format!("get {id}"),
        &headers,
        query.clone(),
        Value::Null,
    );
    // A run's handle names it as its id does.
    let id = id
        .strip_prefix("local/script/")
        .map_or_else(|| id.clone(), str::to_owned);
    let wait = query
        .get("wait")
        .map(|wait| wait.parse::<rewind_api::common::Duration>().unwrap());
    let never_ends = stub.lock().unwrap().reads_left.get(&id) == Some(&u32::MAX);
    if let Some(wait) = wait
        && never_ends
        && !stub.lock().unwrap().cancelling.contains_key(&id)
    {
        // Held until the wait elapses: the run never ends by itself.
        tokio::time::sleep(std::time::Duration::from(wait).min(std::time::Duration::from_secs(2)))
            .await;
    }
    let mut stub = stub.lock().unwrap();
    let Some(run) = stub.runs.get(&id).cloned() else {
        return problem(ProblemType::NotFound, &format!("no run {id}"));
    };
    if wait.is_some()
        && let Some(reason) = stub.cancelling.remove(&id)
    {
        // Held until the cancelled run ends.
        let run = cancelled(run, reason);
        stub.runs.insert(id, run.clone());
        return axum::Json(run).into_response();
    }
    let left = stub.reads_left.entry(id.clone()).or_insert(0);
    // Held until the run ends: one that ends by itself is answered ended.
    if wait.is_some() && *left != u32::MAX {
        *left = 0;
    }
    let run = if *left == 0 && run.status == RunStatus::Active {
        let input = run
            .input
            .as_ref()
            .and_then(|input| input.value.clone())
            .unwrap_or(Value::Null);
        completed(run, input)
    } else {
        *left = left.saturating_sub(1);
        run
    };
    stub.runs.insert(id, run.clone());
    axum::Json(run).into_response()
}

async fn signal(
    State(stub): State<Shared>,
    UrlPath((id, name)): UrlPath<(String, String)>,
    headers: HeaderMap,
    axum::Json(payload): axum::Json<Value>,
) -> Response {
    see(
        &stub,
        format!("signal {id} {name}"),
        &headers,
        BTreeMap::new(),
        payload,
    );
    let stub = stub.lock().unwrap();
    match stub.runs.get(&id) {
        None => problem(ProblemType::NotFound, &format!("no run {id}")),
        Some(run) if run.status.is_terminal() => problem(
            ProblemType::RunEnded,
            &format!("run {id} has ended ({})", run.status),
        ),
        Some(_) => axum::Json(SignalAccepted {
            run_id: id,
            name,
            duplicate: headers.contains_key("idempotency-key"),
        })
        .into_response(),
    }
}

async fn cancel(
    State(stub): State<Shared>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    see(
        &stub,
        format!("cancel {id}"),
        &headers,
        BTreeMap::new(),
        body.clone(),
    );
    let request: CancelRun = serde_json::from_value(body).unwrap();
    let mut stub = stub.lock().unwrap();
    let Some(run) = stub.runs.get(&id).cloned() else {
        return problem(ProblemType::NotFound, &format!("no run {id}"));
    };
    let reason = request.reason.unwrap_or_default();
    if request.wait.is_some() {
        let run = cancelled(run, reason);
        stub.runs.insert(id, run.clone());
        return axum::Json(run).into_response();
    }
    // The run ends cancelled by the time a held read answers.
    stub.cancelling.insert(id, reason);
    axum::Json(run).into_response()
}

fn cancelled(mut run: Run, reason: String) -> Run {
    run.status = RunStatus::Cancelled;
    run.waiting_for.clear();
    run.error = Some(ErrorInfo {
        name: "CancelledError".into(),
        message: reason,
        stack: None,
        location: None,
        frame: None,
        failed_call: None,
    });
    run
}

async fn fork(
    State(stub): State<Shared>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    see(&stub, format!("fork {id}"), &headers, query, body.clone());
    let request: ForkRun = serde_json::from_value(body).unwrap();
    let mut stub = stub.lock().unwrap();
    if !stub.runs.contains_key(&id) {
        return problem(ProblemType::NotFound, &format!("no run {id}"));
    }
    let fork_id = format!("run-{}", stub.runs.len() + 1);
    let mut run = completed(
        base_run(&fork_id, RunStatus::Active),
        json!({"fixed": true}),
    );
    run.source = Some(ForkSource {
        handle: format!("local/script/{id}"),
        run: id,
        at: request.at,
    });
    stub.runs.insert(fork_id, run.clone());
    axum::Json(run).into_response()
}

async fn list(
    State(stub): State<Shared>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    see(&stub, "list", &headers, query, Value::Null);
    let stub = stub.lock().unwrap();
    let runs = stub
        .runs
        .values()
        .map(|run| RunSummary {
            id: run.id.clone(),
            handle: run.handle.clone(),
            namespace: run.namespace.clone(),
            status: run.status,
            quarantined: run.quarantine.is_some(),
            code_digest: run.code_digest.clone(),
            action_set: run.action_set.clone(),
            labels: run.labels.clone(),
            started_at: run.created_at,
            ended_at: run.ended_at,
            last_activity_at: run.updated_at,
            calls: 1,
            errors: 0,
            history_len: run.history_len,
            error: None,
            error_message: None,
            error_line: None,
            fingerprint: None,
            waiting_for: Vec::new(),
            wakes_at: None,
            source: None,
            workflow: None,
            started_by: None,
        })
        .collect();
    axum::Json(RunPage {
        runs,
        next_cursor: Some("page-2".into()),
    })
    .into_response()
}

async fn transcript(State(stub): State<Shared>, UrlPath(id): UrlPath<String>) -> Response {
    if !stub.lock().unwrap().runs.contains_key(&id) {
        return problem(ProblemType::NotFound, &format!("no run {id}"));
    }
    axum::Json(Transcript {
        run_id: id,
        text: "run completed\n#1 0ms workflow.ts:3 github.get_issue({number:4}) → {…}\n".into(),
    })
    .into_response()
}

async fn release(State(stub): State<Shared>, UrlPath(id): UrlPath<String>) -> Response {
    match stub.lock().unwrap().runs.get(&id) {
        None => problem(ProblemType::NotFound, &format!("no run {id}")),
        Some(_) => problem(
            ProblemType::NotQuarantined,
            &format!("run {id} is not quarantined"),
        ),
    }
}

/// The history of a run: a committed history of the e2e corpus, as a server exports it.
async fn export(
    UrlPath(_id): UrlPath<String>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    let case = corpus("epoch-1/blue/waiting_for_a_signal");
    if query.get("as").map(String::as_str) == Some("cassette") {
        let export = rewind_format::decode_run_export(
            &std::fs::read_to_string(case.join("history.v1.json")).unwrap(),
        )
        .unwrap();
        let cassette =
            rewind_core::Cassette::from_history(&export.events, rewind_core::Matching::Sequential)
                .unwrap();
        let text = rewind_format::encode_cassette(&cassette);
        return axum::Json(serde_json::from_str::<Value>(&text).unwrap()).into_response();
    }
    let text = std::fs::read_to_string(case.join("history.v1.json")).unwrap();
    axum::Json(serde_json::from_str::<Value>(&text).unwrap()).into_response()
}

async fn create_action_set(
    State(stub): State<Shared>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    see(&stub, "action_set", &headers, BTreeMap::new(), body.clone());
    let _: CreateActionSet = serde_json::from_value(body).unwrap();
    axum::Json(ActionSet {
        id: ACTION_SET_ID.into(),
        servers: Vec::new(),
        apis: Vec::new(),
        actions: Vec::new(),
        workflows: Vec::new(),
        declarations: Declarations {
            dts: "declare module \"rewind:actions\" {}\n".into(),
            prompt: String::new(),
        },
        skipped: Vec::new(),
    })
    .into_response()
}

async fn check(
    State(stub): State<Shared>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    see(&stub, "check", &headers, BTreeMap::new(), body.clone());
    let request: CheckRequest = serde_json::from_value(body).unwrap();
    assert_eq!(request.action_set.as_deref(), Some(ACTION_SET_ID));
    axum::Json(CheckResult {
        ok: true,
        diagnostics: Vec::new(),
        workflow: None,
    })
    .into_response()
}

fn app(stub: Shared) -> Router {
    Router::new()
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
        .route("/v1/runs", post(start).get(list))
        .route("/v1/runs/{id}", get(get_run))
        .route("/v1/runs/{id}/signals/{name}", post(signal))
        .route("/v1/runs/{id}/cancel", post(cancel))
        .route("/v1/runs/{id}/fork", post(fork))
        .route("/v1/runs/{id}/transcript", get(transcript))
        .route("/v1/runs/{id}/release", post(release))
        .route("/v1/runs/{id}/export", get(export))
        .route("/v1/action-sets", post(create_action_set))
        .route("/v1/check", post(check))
        .with_state(stub)
}

fn corpus(case: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rewind-e2e/corpus")
        .join(case)
}

/// A stand-in embedded server on `<dir>/.rewind/server.sock`.
struct Embedded {
    dir: tempfile::TempDir,
    stub: Shared,
}

impl Embedded {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join(".rewind");
        std::fs::create_dir_all(&state).unwrap();
        let listener = tokio::net::UnixListener::bind(state.join("server.sock")).unwrap();
        let stub = Shared::default();
        tokio::spawn(axum::serve(listener, app(stub.clone())).into_future());
        Self { dir, stub }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    /// Runs `rewind --dir <dir>/.rewind <args…>` in this process.
    async fn rewind(&self, args: &[&str]) -> (Exit, String, String) {
        let state = self.dir.path().join(".rewind");
        let mut argv = vec!["--dir".to_owned(), state.display().to_string()];
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        let (mut console, captured) = Console::captured();
        let exit = rewind_cli::run(argv, &mut console).await;
        (exit, captured.stdout(), captured.stderr())
    }

    fn requests(&self) -> Vec<Seen> {
        self.stub.lock().unwrap().requests.clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn run_starts_a_run_and_waits_for_it() {
    let server = Embedded::start().await;
    let code = server.write("hello.ts", "export default async () => 1;\n");
    let (exit, out, err) = server
        .rewind(&[
            "run",
            code.to_str().unwrap(),
            "--input",
            r#"{"repo":"acme/api"}"#,
            "--label",
            "agent=triage",
            "--key",
            "start-1",
            "--wait",
            "2m",
            "--secret",
            "github=ns/github",
            "--secret",
            "zendesk=org/zendesk",
        ])
        .await;
    assert_eq!(exit, Exit::Success, "{out}{err}");
    assert!(out.contains("run local/script/run-1 completed"), "{out}");
    assert!(out.contains(r#"output: {"repo":"acme/api"}"#), "{out}");
    assert!(err.contains("started run local/script/run-1"), "{err}");

    let requests = server.requests();
    let start = &requests[0];
    assert_eq!(start.what, "start");
    assert_eq!(start.idempotency_key.as_deref(), Some("start-1"));
    assert_eq!(start.authorization, None, "the socket takes no API key");
    assert_eq!(
        start.namespace.as_deref(),
        Some("local"),
        "against its own embedded server, the namespace local"
    );
    assert_eq!(start.body["labels"], json!({"agent": "triage"}));
    assert_eq!(
        start.body["secrets"],
        json!({ "github": "ns/github", "zendesk": "org/zendesk" }),
        "--secret names a secret by server, names only"
    );
    // The start answers at once; the wait is one request, which the server holds.
    assert_eq!(start.body.get("wait"), None);
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(requests[1].what, "get run-1");
    let wait: rewind_api::common::Duration = requests[1].query["wait"].parse().unwrap();
    assert!(
        (119_000..=120_000).contains(&wait.as_millis()),
        "the whole wait: {wait}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn run_exits_with_the_runs_outcome() {
    let server = Embedded::start().await;
    let fails = server.write("fails.ts", "// fails\n");
    let (exit, out, _) = server.rewind(&["run", fails.to_str().unwrap()]).await;
    assert_eq!(exit, Exit::Failed);
    assert!(
        out.contains("TypeError: issue is undefined\n  at workflow.ts:5:3"),
        "{out}"
    );

    let waits = server.write("waits.ts", "// waits\n");
    let (exit, out, _) = server
        .rewind(&["run", waits.to_str().unwrap(), "--wait", "1500ms"])
        .await;
    assert_eq!(exit, Exit::Waiting);
    assert!(
        out.contains("waiting for:\n  - signal \"approval\" (workflow.ts:4:9)"),
        "{out}"
    );

    let (exit, out, _) = server
        .rewind(&["--json", "run", waits.to_str().unwrap(), "--wait", "0"])
        .await;
    assert_eq!(exit, Exit::Waiting);
    let run: Run = serde_json::from_str(&out).unwrap();
    assert_eq!(run.status, RunStatus::Active);
    let reads = server
        .requests()
        .into_iter()
        .filter(|seen| seen.what.starts_with("get"))
        .count();
    let (_, _, _) = server
        .rewind(&["run", waits.to_str().unwrap(), "--wait", "0"])
        .await;
    let after = server
        .requests()
        .into_iter()
        .filter(|seen| seen.what.starts_with("get"))
        .count();
    assert_eq!(after, reads, "--wait 0 does not wait");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_script_exits_65_with_its_diagnostics() {
    let server = Embedded::start().await;
    let code = server.write("refused.ts", "// refused\n");
    let (exit, out, err) = server.rewind(&["run", code.to_str().unwrap()]).await;
    assert_eq!(exit, Exit::CodeRefused);
    assert!(out.is_empty());
    assert!(
        err.contains("error: The code was refused: the code does not type-check"),
        "{err}"
    );
    assert_eq!(
        err.matches("workflow.ts:3:28 error TS2322").count(),
        1,
        "each diagnostic is printed once: {err}"
    );

    let (exit, _, err) = server
        .rewind(&["--json", "run", code.to_str().unwrap()])
        .await;
    assert_eq!(exit, Exit::CodeRefused);
    let problem: Problem = serde_json::from_str(err.trim()).unwrap();
    assert!(problem.is(ProblemType::InvalidCode));
    assert_eq!(problem.diagnostics.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_set_file_is_registered_before_the_run() {
    let server = Embedded::start().await;
    let code = server.write("hello.ts", "export default async () => 1;\n");
    let action_set = server.write(
        "action_set.json",
        r#"{"harness": [{"name": "ask_user", "description": "Ask.", "inputSchema": {"type": "object"}}]}"#,
    );
    let (exit, _, _) = server
        .rewind(&[
            "run",
            code.to_str().unwrap(),
            "--action-set",
            action_set.to_str().unwrap(),
        ])
        .await;
    assert_eq!(exit, Exit::Success);
    let requests = server.requests();
    assert_eq!(requests[0].what, "action_set");
    assert_eq!(requests[1].what, "start");
    assert_eq!(requests[1].body["actionSet"], ACTION_SET_ID);

    let (exit, _, err) = server
        .rewind(&[
            "run",
            code.to_str().unwrap(),
            "--action-set",
            "missing.json",
        ])
        .await;
    assert_eq!(exit, Exit::Usage, "{err}");

    // A registered action set lives on the server: the server checks the code against it.
    let (exit, out, _) = server
        .rewind(&[
            "check",
            code.to_str().unwrap(),
            "--action-set",
            ACTION_SET_ID,
        ])
        .await;
    assert_eq!(exit, Exit::Success);
    assert!(out.contains("no problems"), "{out}");
    assert_eq!(server.requests().last().unwrap().what, "check");
}

#[tokio::test(flavor = "multi_thread")]
async fn signals_and_cancellations() {
    let server = Embedded::start().await;
    let waits = server.write("waits.ts", "// waits\n");
    server
        .rewind(&["run", waits.to_str().unwrap(), "--wait", "0"])
        .await;

    let (exit, out, _) = server
        .rewind(&["signal", "run-1", "approval", r#"{"close": true}"#])
        .await;
    assert_eq!(exit, Exit::Success);
    assert_eq!(out, "signal \"approval\" sent to run run-1\n");
    let signal = server.requests().last().unwrap().clone();
    assert_eq!(signal.what, "signal run-1 approval");
    assert_eq!(signal.body, json!({"close": true}));

    let (exit, out, _) = server
        .rewind(&["signal", "run-1", "approval", "--key", "delivery-7"])
        .await;
    assert_eq!(exit, Exit::Success);
    assert!(out.contains("not sent again"), "{out}");
    let signal = server.requests().last().unwrap().clone();
    assert_eq!(signal.body, Value::Null, "no payload is null");
    assert_eq!(signal.idempotency_key.as_deref(), Some("delivery-7"));

    let (exit, _, err) = server.rewind(&["signal", "nobody", "approval"]).await;
    assert_eq!(exit, Exit::Refused, "{err}");

    let (exit, out, _) = server.rewind(&["cancel", "run-1"]).await;
    assert_eq!(
        exit,
        Exit::Success,
        "a cancellation without --wait succeeds: {out}"
    );
    let (exit, out, _) = server
        .rewind(&["cancel", "run-1", "--reason", "stop", "--wait", "10s"])
        .await;
    assert_eq!(exit, Exit::Cancelled, "{out}");
    assert!(out.contains("CancelledError: stop"), "{out}");

    let (exit, _, err) = server.rewind(&["signal", "run-1", "approval"]).await;
    assert_eq!(
        exit,
        Exit::Refused,
        "a signal to a run that has ended is refused"
    );
    assert!(err.contains("has ended"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn status_runs_transcript_fork_and_release() {
    let server = Embedded::start().await;
    let code = server.write("hello.ts", "export default async () => 1;\n");
    server
        .rewind(&["run", code.to_str().unwrap(), "--wait", "0"])
        .await;

    let (exit, out, _) = server.rewind(&["status", "run-1"]).await;
    assert_eq!(exit, Exit::Waiting, "{out}");
    let (exit, out, _) = server.rewind(&["status", "run-1", "--wait", "1m"]).await;
    assert_eq!(exit, Exit::Success, "{out}");
    // By its handle: one segment of the path, percent-encoded.
    let (exit, out, _) = server.rewind(&["status", "local/script/run-1"]).await;
    assert_eq!(exit, Exit::Success, "{out}");
    assert!(out.starts_with("run local/script/run-1 completed"), "{out}");
    assert_eq!(
        server.requests().last().unwrap().what,
        "get local/script/run-1"
    );

    let (exit, out, err) = server
        .rewind(&["runs", "--label", "agent=triage", "--status", "completed"])
        .await;
    assert_eq!(exit, Exit::Success);
    assert!(out.starts_with("RUN                 STATUS"), "{out}");
    assert!(out.contains("local/script/run-1  completed"), "{out}");
    assert!(err.contains("--cursor page-2"), "{err}");
    let list = server.requests().last().unwrap().clone();
    assert_eq!(
        list.query.get("label").map(String::as_str),
        Some("agent:triage")
    );
    assert_eq!(
        list.query.get("status").map(String::as_str),
        Some("completed")
    );

    let (exit, out, _) = server.rewind(&["--json", "runs"]).await;
    assert_eq!(exit, Exit::Success);
    let page: RunPage = serde_json::from_str(&out).unwrap();
    assert_eq!(page.runs.len(), 1);

    let (exit, _, _) = server
        .rewind(&["runs", "@agent:triage -status:failed", "--sort", "wakes"])
        .await;
    assert_eq!(exit, Exit::Success);
    let list = server.requests().last().unwrap().clone();
    assert_eq!(
        list.query.get("q").map(String::as_str),
        Some("@agent:triage -status:failed")
    );
    assert_eq!(list.query.get("sort").map(String::as_str), Some("wakes"));

    let (exit, out, _) = server.rewind(&["transcript", "run-1"]).await;
    assert_eq!(exit, Exit::Success);
    assert!(out.contains("github.get_issue"), "{out}");
    let (exit, _, _) = server.rewind(&["transcript", "nobody"]).await;
    assert_eq!(exit, Exit::Refused);

    let fixed = server.write("fixed.ts", "export default async () => 2;\n");
    let (exit, out, _) = server
        .rewind(&[
            "fork",
            "run-1",
            "--code",
            fixed.to_str().unwrap(),
            "--at",
            "3",
            "--key",
            "fork-1",
        ])
        .await;
    assert_eq!(exit, Exit::Success, "{out}");
    assert!(
        out.contains("run local/script/run-2 completed (fork of local/script/run-1 at event 3)"),
        "{out}"
    );
    let fork = server
        .requests()
        .into_iter()
        .find(|seen| seen.what == "fork run-1")
        .unwrap();
    assert_eq!(fork.body["at"], 3);
    assert_eq!(fork.idempotency_key.as_deref(), Some("fork-1"));
    assert_eq!(
        fork.body.get("secrets"),
        None,
        "the source's when none is given"
    );
    let (exit, _, err) = server
        .rewind(&[
            "fork",
            "run-1",
            "--code",
            fixed.to_str().unwrap(),
            "--secret",
            "github=org/github",
        ])
        .await;
    assert_eq!(exit, Exit::Success, "{err}");
    let fork = server
        .requests()
        .into_iter()
        .rfind(|seen| seen.what == "fork run-1")
        .unwrap();
    assert_eq!(fork.body["secrets"], json!({ "github": "org/github" }));
    let (exit, _, err) = server
        .rewind(&[
            "fork",
            "run-1",
            "--code",
            fixed.to_str().unwrap(),
            "--secret",
            "github",
        ])
        .await;
    assert_eq!(exit, Exit::Usage, "{err}");
    assert!(err.contains("expected <server>=<scope>/<name>"), "{err}");

    let (exit, _, err) = server.rewind(&["release", "run-1"]).await;
    assert_eq!(exit, Exit::Refused, "{err}");
    assert!(err.contains("not quarantined"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_is_exported_and_replayed_here() {
    let server = Embedded::start().await;
    let output = server.path("run.json");
    let (exit, _, err) = server
        .rewind(&["export", "run-1", "--output", output.to_str().unwrap()])
        .await;
    assert_eq!(exit, Exit::Success, "{err}");
    // Written in the stored format, byte for byte as the corpus has it.
    let committed =
        std::fs::read_to_string(corpus("epoch-1/blue/waiting_for_a_signal/history.v1.json"))
            .unwrap();
    assert_eq!(std::fs::read_to_string(&output).unwrap(), committed);

    let (exit, out, _) = server
        .rewind(&["export", "run-1", "--as", "cassette"])
        .await;
    assert_eq!(exit, Exit::Success);
    assert!(rewind_format::decode_cassette(&out).is_ok(), "{out}");

    // `rewind replay <run>` exports the history and replays it in this process.
    let (exit, out, err) = server.rewind(&["replay", "run-1"]).await;
    assert_eq!(exit, Exit::Success, "{out}{err}");
    assert!(out.contains("the code replays the 4 events"), "{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_server_gets_the_api_key() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let stub = Shared::default();
    tokio::spawn(axum::serve(listener, app(stub.clone())).into_future());
    let dir = tempfile::tempdir().unwrap();
    let code = dir.path().join("hello.ts");
    std::fs::write(&code, "export default async () => 1;\n").unwrap();

    let state = dir.path().join(".rewind");
    let rewind = |key: &'static str, namespace: Option<&'static str>| {
        let mut argv = vec![
            "--url".to_owned(),
            url.clone(),
            "--api-key".to_owned(),
            key.to_owned(),
            "--dir".to_owned(),
            state.display().to_string(),
        ];
        if let Some(namespace) = namespace {
            argv.extend(["--namespace".to_owned(), namespace.to_owned()]);
        }
        argv.extend(["run".to_owned(), code.display().to_string()]);
        async move {
            let (mut console, captured) = Console::captured();
            let exit = rewind_cli::run(argv, &mut console).await;
            (exit, captured.stderr())
        }
    };

    // A key names the end user the run acts for: a usage error without it, before any request.
    let (exit, err) = rewind("rewind_sk_secret", None).await;
    assert_eq!(exit, Exit::Usage, "{err}");
    assert!(err.contains("--namespace"), "{err}");
    assert!(stub.lock().unwrap().requests.is_empty());

    let (exit, err) = rewind("rewind_sk_secret", Some("alice")).await;
    assert_eq!(exit, Exit::Success, "{err}");
    let requests = stub.lock().unwrap().requests.clone();
    assert!(
        requests
            .iter()
            .all(|seen| seen.authorization.as_deref() == Some("Bearer rewind_sk_secret"))
    );
    assert!(
        requests
            .iter()
            .all(|seen| seen.namespace.as_deref() == Some("alice"))
    );
    assert!(!state.exists(), "a remote server needs no embedded one");

    // An end user's token names its own namespace.
    stub.lock().unwrap().requests.clear();
    let (exit, err) = rewind("rewind_nt_token_of_alice", None).await;
    assert_eq!(exit, Exit::Success, "{err}");
    let requests = stub.lock().unwrap().requests.clone();
    assert!(requests.iter().all(|seen| seen.namespace.is_none()));
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_errors_exit_64() {
    let (mut console, captured) = Console::captured();
    assert_eq!(rewind_cli::run(["launch"], &mut console).await, Exit::Usage);
    assert!(captured.stderr().contains("unrecognized subcommand"));
    let (mut console, captured) = Console::captured();
    assert_eq!(
        rewind_cli::run(["--help"], &mut console).await,
        Exit::Success
    );
    assert!(captured.stdout().contains("Exit codes: 0 completed"));
    let (mut console, _) = Console::captured();
    assert_eq!(
        rewind_cli::run(["run", "/nonexistent/workflow.ts"], &mut console).await,
        Exit::Usage
    );
    let (mut console, captured) = Console::captured();
    assert_eq!(
        rewind_cli::run(["signal", "r1", "go", "{not json"], &mut console).await,
        Exit::Usage
    );
    assert!(captured.stderr().contains("the payload is not JSON"));
}
