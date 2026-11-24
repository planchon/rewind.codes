//! The API `/v1` end to end over HTTP, on Turso: runs, idempotency, type-checking at start,
//! exports, harness calls, approvals, signals, inbound and outbound webhooks, the event
//! stream, quotas, keys, roles and label scopes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::HeaderMap;
use futures_util::StreamExt;
use rewind_api::access::BuiltInRole;
use rewind_api::action_sets::{ActionPolicy, CreateActionSet, HarnessAction};
use rewind_api::calls::{
    CallState, CallStreamQuery, CompleteCall, DeferCall, FailCall, RejectCall,
};
use rewind_api::common::{Labels, RunStatus};
use rewind_api::events::{Event, EventsQuery, StreamEvent};
use rewind_api::headers::IdempotencyKey;
use rewind_api::runs::{
    CancelRun, ExportAs, FailureQuery, ForkRun, RunFilter, RunSort, RunStatsQuery, StartRun,
    WaitKind, Waiting,
};
use rewind_api::scripts::{CheckRequest, ScriptFilter};
use rewind_api::webhooks::{
    CreateSubscription, SignedHeaders, WebhookEvent, WebhookEventType, WebhookSecret,
};
use rewind_api::{Problem, ProblemType};
use serde_json::json;
use support::{TaskWorker, TestServer, eventually, wait};

const SLEEP: &str = r#"import { sleep } from "rewind";
export default async function main(input: { ms: number }) {
  await sleep(input.ms);
  console.log("slept", input.ms);
  return { slept: input.ms };
}
"#;

const SIGNAL: &str = r#"import { signal } from "rewind";
export default async function main() {
  const decision = await signal<{ ok: boolean }>("approval");
  return { decision };
}
"#;

const ASK: &str = r#"import { ask_user } from "rewind:actions";
export default async function main() {
  const answer = await ask_user({ question: "ready?" });
  return { answer };
}
"#;

const HOOK: &str = r#"import { signal, webhook } from "rewind";
export default async function main() {
  const hook = await webhook("pr_merged");
  console.log(hook.url);
  await signal("go");
  const first = await signal("pr_merged");
  const second = await signal("pr_merged", { timeout: "50ms" });
  return { first, second: second ?? null };
}
"#;

fn problem(error: rewind_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

fn start(code: &str) -> StartRun {
    StartRun::new(code)
}

fn ask_action_set(policy: ActionPolicy) -> CreateActionSet {
    CreateActionSet {
        harness: vec![HarnessAction {
            name: "ask_user".into(),
            description: "Ask the end user a question and return their answer.".into(),
            input_schema: json!({
                "type": "object",
                "properties": { "question": { "type": "string" } },
                "required": ["question"]
            }),
            output_schema: None,
            policy: Some(policy),
        }],
        ..CreateActionSet::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_starts_waits_and_is_read_back() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let mut request = start(SLEEP);
    request.input = json!({ "ms": 50 });
    request.wait = wait("20s");
    request.labels = Labels::from_iter([("agent", "triage")]);
    let key = IdempotencyKey::new("tool-call-1").unwrap();
    let run = harness.start_run(&request, Some(&key)).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(
        run.output.as_ref().unwrap().value,
        Some(json!({ "slept": 50 }))
    );
    assert_eq!(run.input.as_ref().unwrap().value, Some(json!({ "ms": 50 })));
    assert_eq!(run.logs[0].message, "slept 50");

    // The same key: the same run, not a second one.
    let again = harness.start_run(&request, Some(&key)).await.unwrap();
    assert_eq!(again.id, run.id);

    let read = harness.get_run(&run.id, None).await.unwrap();
    assert_eq!(read.history_len, run.history_len);
    let trace = harness.run_transcript(&run.id).await.unwrap();
    assert!(trace.text.contains("completed"), "{}", trace.text);
    let entry = harness.run_event(&run.id, 0).await.unwrap();
    assert!(matches!(entry.event, Event::RunStarted { .. }));
    assert_eq!(
        problem(harness.run_event(&run.id, 99).await.unwrap_err()).kind(),
        Some(ProblemType::NotFound)
    );

    let export = harness.export_run(&run.id, ExportAs::Run).await.unwrap();
    let document = rewind_format::decode_run_export(&export.to_string()).unwrap();
    assert_eq!(document.run.to_string(), run.id);
    assert_eq!(document.events.len() as u64, run.history_len);
    let cassette = harness
        .export_run(&run.id, ExportAs::Cassette)
        .await
        .unwrap();
    assert_eq!(cassette["v"], 1);

    // The run is listed: its summary is in the index of its partition.
    let listed = eventually(|| async {
        let page = harness
            .list_runs(&RunFilter {
                labels: vec!["agent:triage".into()],
                ..RunFilter::default()
            })
            .await
            .unwrap();
        page.runs.into_iter().find(|summary| summary.id == run.id)
    })
    .await;
    assert_eq!(listed.status, RunStatus::Completed);
    assert_eq!(listed.headline, "import { sleep } from \"rewind\";");
    let scripts = harness
        .list_scripts(&ScriptFilter {
            digest: Some(run.code_digest.clone()),
            ..ScriptFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(scripts.scripts.len(), 1);
    assert_eq!(scripts.scripts[0].runs.completed, 1);
    let code = harness.get_script(&run.code_digest).await.unwrap();
    assert_eq!(code.source, SLEEP);
    assert_eq!(code.runs.completed, 1);

    harness.delete_run(&run.id).await.unwrap();
    assert_eq!(
        problem(harness.get_run(&run.id, None).await.unwrap_err()).kind(),
        Some(ProblemType::NotFound)
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_script_that_does_not_type_check_is_refused_unless_the_check_is_skipped() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let code = "export default function main() {\n  const n: string = 42;\n  return n;\n}\n";
    let refused = problem(harness.start_run(&start(code), None).await.unwrap_err());
    assert_eq!(refused.kind(), Some(ProblemType::InvalidCode));
    assert_eq!(refused.status, 422);
    let diagnostic = &refused.diagnostics[0];
    assert_eq!(diagnostic.code, "TS2322");
    assert_eq!(diagnostic.location.unwrap().line, 2);

    let checked = harness
        .check(&CheckRequest {
            code: code.into(),
            action_set: None,
        })
        .await
        .unwrap();
    assert!(!checked.ok);

    let mut skipped = start(code);
    skipped.check = Some(false);
    skipped.wait = wait("20s");
    let run = harness.start_run(&skipped, None).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    assert_eq!(run.output.unwrap().value, Some(json!(42)));

    let invalid = problem(
        harness
            .start_run(
                &start("import fs from \"fs\";\nexport default () => 1;"),
                None,
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(invalid.kind(), Some(ProblemType::InvalidCode));
    assert_eq!(invalid.diagnostics[0].code, "rewind/invalid-code");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn harness_actions_are_handed_on_a_stream_and_answered() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let worker = server.key(BuiltInRole::Worker, &[]).await;
    let action_set = harness
        .create_action_set(&ask_action_set(ActionPolicy {
            reconnect_timeout: Some("5s".parse().unwrap()),
            ..ActionPolicy::default()
        }))
        .await
        .unwrap();
    assert!(
        action_set
            .actions
            .iter()
            .any(|action| action.name == "webhook")
    );
    assert_eq!(
        harness.list_action_sets().await.unwrap().action_sets.len(),
        1
    );

    assert!(
        action_set
            .declarations
            .dts
            .contains("export function ask_user"),
        "{}",
        action_set.declarations.dts
    );
    let mut wrong = start(&ASK.replace("\"ready?\"", "42"));
    wrong.action_set = Some(action_set.id.clone());
    let refused = problem(harness.start_run(&wrong, None).await.unwrap_err());
    assert_eq!(refused.kind(), Some(ProblemType::InvalidCode));
    assert_eq!(refused.diagnostics[0].code, "TS2322", "{refused}");

    let mut request = start(ASK);
    request.action_set = Some(action_set.id.clone());
    let run = harness.start_run(&request, None).await.unwrap();

    let query = CallStreamQuery {
        action_set: Some(action_set.id.clone()),
        actions: vec!["ask_user".into()],
        slots: Some(1),
        worker: Some("worker-1".into()),
        ..CallStreamQuery::default()
    };
    let mut stream = TaskWorker::open(&worker, &query).await;
    let task = &stream.task().await;
    assert_eq!(task.run_id, run.id);
    assert_eq!(task.args, json!({ "question": "ready?" }));
    assert_eq!(task.idempotency_key, format!("{}/1", run.id));
    assert_eq!(
        task.timeout_ms, None,
        "a harness action has no execution timeout"
    );

    // Deferred, then answered later through the same token.
    worker
        .defer_call(&task.token, &DeferCall::default())
        .await
        .unwrap();
    let state = harness.get_run(&run.id, None).await.unwrap();
    assert!(matches!(
        &state.waiting_for[0],
        Waiting::Call {
            state: CallState::Deferred,
            ..
        }
    ));
    worker
        .complete_call(
            &task.token,
            &CompleteCall {
                result: json!("yes"),
            },
        )
        .await
        .unwrap();
    let ended = harness.get_run(&run.id, wait("20s")).await.unwrap();
    assert_eq!(ended.status, RunStatus::Completed);
    assert_eq!(
        ended.output.unwrap().value,
        Some(json!({ "answer": "yes" }))
    );
    assert_eq!(
        problem(
            worker
                .complete_call(
                    &task.token,
                    &CompleteCall {
                        result: json!("again")
                    }
                )
                .await
                .unwrap_err()
        )
        .kind(),
        Some(ProblemType::CallGone)
    );

    // A retryable failure is delivered again: the next hand-out is delivery 2.
    let run = harness.start_run(&request, None).await.unwrap();
    let first = stream.task().await;
    worker
        .fail_call(
            &first.token,
            &FailCall {
                message: "the user is away".into(),
                data: None,
                retryable: true,
            },
        )
        .await
        .unwrap();
    let second = stream.task().await;
    assert_eq!(
        (second.run_id.as_str(), second.delivery),
        (run.id.as_str(), 2)
    );
    let attempts = harness.get_run(&run.id, None).await.unwrap().deliveries;
    assert_eq!((attempts[0].seq, attempts[0].deliveries), (1, 2));
    worker
        .complete_call(
            &second.token,
            &CompleteCall {
                result: json!("back"),
            },
        )
        .await
        .unwrap();
    let ended = harness.get_run(&run.id, wait("20s")).await.unwrap();
    assert_eq!(ended.status, RunStatus::Completed);
    server.stop().await;
}

/// A run that fails on an `ActionError` it did not catch says which call failed: the runtime's
/// message (`ask_user failed: …`) is not the action's, so the call is found through its recorded
/// failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_failed_by_an_uncaught_action_error_names_the_call() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let worker = server.key(BuiltInRole::Worker, &[]).await;
    let action_set = harness
        .create_action_set(&ask_action_set(ActionPolicy::default()))
        .await
        .unwrap();
    // The first failure is caught; the second is not.
    let code = r#"import { ask_user } from "rewind:actions";
export default async function main() {
  try {
    await ask_user({ question: "first?" });
  } catch {}
  const answer = await ask_user({ question: "second?" });
  return { answer };
}
"#;
    let mut request = start(code);
    request.action_set = Some(action_set.id.clone());
    let run = harness.start_run(&request, None).await.unwrap();
    let query = CallStreamQuery {
        action_set: Some(action_set.id.clone()),
        actions: vec!["ask_user".into()],
        ..CallStreamQuery::default()
    };
    let mut stream = TaskWorker::open(&worker, &query).await;
    for message in ["the user is away", "the user left"] {
        let task = stream.task().await;
        worker
            .fail_call(
                &task.token,
                &FailCall {
                    message: message.into(),
                    data: Some(json!({ "retryAfter": "1h" })),
                    retryable: false,
                },
            )
            .await
            .unwrap();
    }
    let ended = harness.get_run(&run.id, wait("20s")).await.unwrap();
    assert_eq!(ended.status, RunStatus::Failed, "{ended:?}");
    let error = ended.error.unwrap();
    assert_eq!(error.name, "ActionError");
    assert_eq!(error.message, "ask_user failed: the user left");
    let call = error.failed_call.expect("the call that failed");
    assert_eq!((call.seq, call.action.as_str()), (2, "ask_user"));
    assert_eq!(call.args.value, Some(json!({ "question": "second?" })));
    assert_eq!(call.error.message, "the user left");
    assert_eq!(call.error.data, Some(json!({ "retryAfter": "1h" })));
    assert_eq!(call.callsite.map(|callsite| callsite.line), Some(6));
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_under_approval_waits_for_the_harness_with_its_arguments() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let action_set = harness
        .create_action_set(&ask_action_set(ActionPolicy {
            approval: Some(true),
            ..ActionPolicy::default()
        }))
        .await
        .unwrap();
    let mut request = start(ASK);
    request.action_set = Some(action_set.id.clone());

    // Rejected: the call fails with an ActionError, and the run fails.
    let run = harness.start_run(&request, None).await.unwrap();
    let (call_id, args) = eventually(|| async {
        let state = harness.get_run(&run.id, None).await.unwrap();
        state
            .waiting_for
            .into_iter()
            .find_map(|waiting| match waiting {
                Waiting::Call {
                    call_id,
                    state: CallState::Approval,
                    args,
                    ..
                } => Some((call_id, args)),
                _ => None,
            })
    })
    .await;
    assert_eq!(args.value, Some(json!({ "question": "ready?" })));
    harness
        .reject_call(
            &call_id,
            &RejectCall {
                message: Some("not now".into()),
            },
        )
        .await
        .unwrap();
    let ended = harness.get_run(&run.id, wait("20s")).await.unwrap();
    assert_eq!(ended.status, RunStatus::Failed);
    let error = ended.error.unwrap();
    assert_eq!(error.name, "ActionError");
    assert!(error.message.contains("not now"), "{}", error.message);

    // Approved: the worker gets it.
    let run = harness.start_run(&request, None).await.unwrap();
    let call_id = eventually(|| async {
        let state = harness.get_run(&run.id, None).await.unwrap();
        state
            .waiting_for
            .into_iter()
            .find_map(|waiting| match waiting {
                Waiting::Call { call_id, .. } => Some(call_id),
                _ => None,
            })
    })
    .await;
    assert_eq!(
        problem(harness.approve_call("nope.1").await.unwrap_err()).kind(),
        Some(ProblemType::NotFound)
    );
    harness.approve_call(&call_id).await.unwrap();
    assert_eq!(
        problem(harness.approve_call(&call_id).await.unwrap_err()).kind(),
        Some(ProblemType::CallNotWaiting)
    );
    let task = TaskWorker::open(&harness, &CallStreamQuery::default())
        .await
        .task()
        .await;
    harness
        .complete_call(
            &task.token,
            &CompleteCall {
                result: json!("ok"),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        harness.get_run(&run.id, wait("20s")).await.unwrap().status,
        RunStatus::Completed
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn signals_are_delivered_once_per_key_and_refused_after_the_end() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let run = harness.start_run(&start(SIGNAL), None).await.unwrap();
    let key = IdempotencyKey::new("approval-1").unwrap();
    let first = harness
        .signal_run(&run.id, "approval", &json!({ "ok": true }), Some(&key))
        .await
        .unwrap();
    assert!(!first.duplicate);
    let again = harness
        .signal_run(&run.id, "approval", &json!({ "ok": false }), Some(&key))
        .await
        .unwrap();
    assert!(again.duplicate);
    let ended = harness.get_run(&run.id, wait("20s")).await.unwrap();
    assert_eq!(
        ended.output.unwrap().value,
        Some(json!({ "decision": { "ok": true } }))
    );
    let refused = problem(
        harness
            .signal_run(&run.id, "approval", &json!(null), None)
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::RunEnded));

    // A cancelled run ends cancelled; a second cancellation is refused.
    let run = harness.start_run(&start(SIGNAL), None).await.unwrap();
    let cancelled = harness
        .cancel_run(
            &run.id,
            &CancelRun {
                reason: Some("enough".into()),
                wait: wait("20s"),
            },
        )
        .await
        .unwrap();
    assert_eq!(cancelled.status, RunStatus::Cancelled);
    assert_eq!(
        problem(
            harness
                .cancel_run(&run.id, &CancelRun::default())
                .await
                .unwrap_err()
        )
        .kind(),
        Some(ProblemType::RunEnded)
    );
    assert_eq!(
        problem(harness.release_run(&run.id).await.unwrap_err()).kind(),
        Some(ProblemType::NotQuarantined)
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_script_gets_a_webhook_url_and_whatever_is_posted_there_is_a_signal() {
    let server = TestServer::start(&["--public-url", "https://rewind.example.com"]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let run = harness.start_run(&start(HOOK), None).await.unwrap();
    let url = eventually(|| async {
        let state = harness.get_run(&run.id, None).await.unwrap();
        state.logs.first().map(|line| line.message.clone())
    })
    .await;
    let token = url
        .strip_prefix("https://rewind.example.com/hooks/")
        .unwrap_or_else(|| panic!("unexpected webhook URL {url}"));
    // Anyone may post to the URL: no key.
    let anonymous = rewind_api::Client::remote(&server.url, None).unwrap();
    let delivery = IdempotencyKey::new("delivery-1").unwrap();
    anonymous
        .post_hook(token, &json!({ "merged": true }), Some(&delivery))
        .await
        .unwrap();
    anonymous
        .post_hook(token, &json!({ "merged": false }), Some(&delivery))
        .await
        .unwrap();
    harness
        .signal_run(&run.id, "go", &json!(null), None)
        .await
        .unwrap();
    let ended = harness.get_run(&run.id, wait("20s")).await.unwrap();
    assert_eq!(
        ended.output.unwrap().value,
        Some(json!({ "first": { "merged": true }, "second": null })),
        "the delivery id made the second post a duplicate"
    );
    assert_eq!(
        problem(
            anonymous
                .post_hook(token, &json!(1), None)
                .await
                .unwrap_err()
        )
        .kind(),
        Some(ProblemType::NotFound),
        "the token of a run that ended no longer resolves"
    );
    server.stop().await;
}

/// What a receiver of webhooks got: the body, verified against the secret.
#[derive(Default, Clone)]
struct Received(Arc<Mutex<Vec<(HeaderMap, Bytes)>>>);

async fn receiver() -> (String, Received) {
    let received = Received::default();
    let recorded = received.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |headers: HeaderMap, body: Bytes| {
            let recorded = recorded.clone();
            async move {
                recorded.0.lock().unwrap().push((headers, body));
                axum::http::StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}/hook"), received)
}

/// A receiver that accepts connections and never answers; returns its URL and how many
/// connections it accepted.
async fn silent_receiver() -> (String, Arc<Mutex<usize>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(Mutex::new(0));
    let counted = accepted.clone();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            *counted.lock().unwrap() += 1;
            held.push(socket);
        }
    });
    (format!("http://{addr}/hook"), accepted)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_that_never_answers_holds_up_no_other_run() {
    // The receivers listen on 127.0.0.1.
    let server = TestServer::start(&["--allow-private-webhooks"]).await;
    let stranger = server.key_of("other", BuiltInRole::Harness, &[]).await;
    let (url, accepted) = silent_receiver().await;
    stranger
        .create_subscription(&CreateSubscription {
            url,
            events: Vec::new(),
            labels: Labels::new(),
        })
        .await
        .unwrap();
    let mut quick = start(SLEEP);
    quick.input = json!({ "ms": 10 });
    quick.wait = wait("20s");
    stranger.start_run(&quick, None).await.unwrap();
    eventually(|| async { (*accepted.lock().unwrap() > 0).then_some(()) }).await;

    // While its lifecycle event waits for the silent receiver, the runs of another tenant go
    // on, and are listed.
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let run = harness.start_run(&quick, None).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let page = harness.list_runs(&RunFilter::default()).await.unwrap();
        if page.runs.iter().any(|listed| listed.id == run.id) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run was not listed within 10 s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let stopping = tokio::time::Instant::now();
    server.stop().await;
    assert!(
        stopping.elapsed() < Duration::from_secs(10),
        "a delivery in progress does not hold up the stop: it is sent again later"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_webhooks_are_signed_and_filtered_by_labels() {
    // The receivers listen on 127.0.0.1.
    let server = TestServer::start(&["--allow-private-webhooks"]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let (url, received) = receiver().await;
    let subscription = harness
        .create_subscription(&CreateSubscription {
            url,
            events: vec![
                WebhookEventType::RunCompleted,
                WebhookEventType::RunCancelled,
            ],
            labels: Labels::from_iter([("conversation", "c-1")]),
        })
        .await
        .unwrap();
    let secret = WebhookSecret::parse(subscription.secret.as_deref().unwrap()).unwrap();
    assert!(
        harness.list_subscriptions().await.unwrap().subscriptions[0]
            .secret
            .is_none()
    );

    let mut mine = start(SLEEP);
    mine.input = json!({ "ms": 10 });
    mine.labels = Labels::from_iter([("conversation", "c-1")]);
    mine.wait = wait("20s");
    let run = harness.start_run(&mine, None).await.unwrap();
    let mut other = mine.clone();
    other.labels = Labels::from_iter([("conversation", "c-2")]);
    harness.start_run(&other, None).await.unwrap();

    eventually(|| async { (!received.0.lock().unwrap().is_empty()).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let deliveries = received.0.lock().unwrap().clone();
    assert_eq!(deliveries.len(), 1, "only the run of conversation c-1");
    let (headers, body) = &deliveries[0];
    let reqwest_headers: reqwest::header::HeaderMap = headers
        .iter()
        .map(|(name, value)| {
            (
                reqwest::header::HeaderName::from_bytes(name.as_str().as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
            )
        })
        .collect();
    let signed = SignedHeaders::from_headers(&reqwest_headers).unwrap();
    let event = secret
        .verify(&signed, body, jiff::Timestamp::now())
        .unwrap();
    let WebhookEvent::RunCompleted { data, .. } = event else {
        panic!("expected run.completed");
    };
    assert_eq!(data.run.id, run.id);
    assert_eq!(data.run.output.unwrap().value, Some(json!({ "slept": 10 })));
    assert_eq!(signed.id, format!("{}/{}", run.id, run.history_len - 1));
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_event_stream_follows_a_run_until_it_ends() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Reader, &[]).await;
    let starter = server.key(BuiltInRole::Harness, &[]).await;
    let mut request = start(SLEEP);
    request.input = json!({ "ms": 300 });
    let run = starter.start_run(&request, None).await.unwrap();
    let mut stream = harness
        .run_events(
            &run.id,
            &EventsQuery {
                follow: Some(true),
                from: None,
            },
        )
        .await
        .unwrap();
    let mut names = Vec::new();
    let mut end = None;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(20), stream.next())
        .await
        .unwrap()
    {
        match event.unwrap() {
            StreamEvent::Event(entry) => names.push(entry.event.name()),
            StreamEvent::End { status } => end = Some(status),
        }
    }
    assert_eq!(end, Some(RunStatus::Completed));
    assert_eq!(
        names,
        [
            "run_started",
            "start_timer",
            "timer_fired",
            "log",
            "complete_run"
        ]
    );
    // From an index, without following: the rest, then the end.
    let mut rest = harness
        .run_events(
            &run.id,
            &EventsQuery {
                follow: None,
                from: Some(3),
            },
        )
        .await
        .unwrap();
    let mut count = 0;
    while let Some(event) = rest.next().await {
        if let StreamEvent::Event(entry) = event.unwrap() {
            assert!(entry.idx >= 3);
            count += 1;
        }
    }
    assert_eq!(count, 2);

    // A client resumes after the last event it saw, whatever index it sends.
    let resumed = reqwest::Client::new()
        .get(format!("{}/v1/runs/{}/events", server.url, run.id))
        .bearer_auth(support::ADMIN_KEY)
        .header("Last-Event-ID", u64::MAX.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resumed.status(), 200);
    let text = resumed.text().await.unwrap();
    assert!(text.contains("event: end"), "{text}");
    assert!(!text.contains("event: event"), "{text}");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_above_its_quota_is_refused_with_429() {
    let server = TestServer::start(&["--quota-active-runs", "1"]).await;
    let harness = server.key_of("small", BuiltInRole::Harness, &[]).await;
    let key = IdempotencyKey::new("first").unwrap();
    let first = harness.start_run(&start(SIGNAL), Some(&key)).await.unwrap();
    let refused = problem(harness.start_run(&start(SIGNAL), None).await.unwrap_err());
    assert_eq!(refused.kind(), Some(ProblemType::QuotaExceeded));
    assert_eq!(refused.status, 429);
    assert_eq!(
        harness
            .start_run(&start(SIGNAL), Some(&key))
            .await
            .unwrap()
            .id,
        first.id,
        "a retried request still gets its run"
    );
    harness
        .cancel_run(
            &first.id,
            &CancelRun {
                reason: None,
                wait: wait("20s"),
            },
        )
        .await
        .unwrap();
    harness.start_run(&start(SIGNAL), None).await.unwrap();
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keys_have_a_tenant_a_role_and_label_scopes() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let reader = server.key(BuiltInRole::Reader, &[]).await;
    let worker = server.key(BuiltInRole::Worker, &[]).await;
    let scoped = server
        .key(BuiltInRole::Harness, &[("conversation", "c-1")])
        .await;
    let stranger = server.key_of("other", BuiltInRole::Harness, &[]).await;

    let unknown = rewind_api::Client::remote(&server.url, Some("rewind_sk_unknown")).unwrap();
    assert_eq!(
        problem(unknown.list_runs(&RunFilter::default()).await.unwrap_err()).kind(),
        Some(ProblemType::Unauthorized)
    );
    // The scheme is case-insensitive (RFC 9110).
    let lowercase = reqwest::Client::new()
        .get(format!("{}/v1/whoami", server.url))
        .header("authorization", format!("bearer {}", support::ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(lowercase.status(), 200);
    let anonymous = rewind_api::Client::remote(&server.url, None).unwrap();
    assert_eq!(
        problem(anonymous.server_info().await.unwrap_err()).kind(),
        Some(ProblemType::Unauthorized)
    );
    assert_eq!(
        problem(reader.start_run(&start(SIGNAL), None).await.unwrap_err()).kind(),
        Some(ProblemType::Forbidden)
    );
    assert_eq!(
        problem(worker.list_runs(&RunFilter::default()).await.unwrap_err()).kind(),
        Some(ProblemType::Forbidden)
    );
    let me = scoped.whoami().await.unwrap();
    let grant = me.grant.unwrap();
    assert_eq!(
        (me.tenant.as_deref(), grant.role.as_str()),
        (Some("acme"), "harness")
    );
    assert_eq!(
        grant.allowed_labels,
        Labels::from_iter([("conversation", "c-1")])
    );

    // The scoped key's runs carry its labels; it sees nothing else.
    let theirs = harness.start_run(&start(SIGNAL), None).await.unwrap();
    let mine = scoped.start_run(&start(SIGNAL), None).await.unwrap();
    assert_eq!(mine.labels, Labels::from_iter([("conversation", "c-1")]));
    assert_eq!(
        problem(scoped.get_run(&theirs.id, None).await.unwrap_err()).kind(),
        Some(ProblemType::NotFound)
    );
    assert_eq!(
        problem(stranger.get_run(&mine.id, None).await.unwrap_err()).kind(),
        Some(ProblemType::NotFound),
        "another tenant does not see it"
    );
    let mut outside = start(SIGNAL);
    outside.labels = Labels::from_iter([("conversation", "c-2")]);
    assert_eq!(
        problem(scoped.start_run(&outside, None).await.unwrap_err()).kind(),
        Some(ProblemType::Forbidden)
    );
    reader.get_run(&mine.id, None).await.unwrap();

    // A fork without labels keeps its source's: the scoped key sees it.
    harness
        .signal_run(&mine.id, "approval", &json!({ "ok": true }), None)
        .await
        .unwrap();
    harness.get_run(&mine.id, wait("20s")).await.unwrap();
    let mut fork = ForkRun::new(SIGNAL);
    fork.at = Some(1);
    let forked = scoped.fork_run(&mine.id, &fork, None).await.unwrap();
    assert_eq!(forked.labels, Labels::from_iter([("conversation", "c-1")]));
    assert_eq!(forked.source.unwrap().run, mine.id);

    // A revoked key is refused.
    let made = server
        .admin
        .create_api_key(&rewind_api::access::CreateApiKey {
            tenant: Some("acme".into()),
            name: "revoked".into(),
            role: BuiltInRole::Reader.as_str().into(),
            allowed_labels: Labels::new(),
        })
        .await
        .unwrap();
    server.admin.revoke_api_key(&made.id).await.unwrap();
    let revoked = rewind_api::Client::remote(&server.url, made.key.as_deref()).unwrap();
    assert_eq!(
        problem(revoked.whoami().await.unwrap_err()).kind(),
        Some(ProblemType::Unauthorized)
    );
    assert_eq!(
        problem(harness.revoke_api_key(&made.id).await.unwrap_err()).kind(),
        Some(ProblemType::Forbidden)
    );
    let _ = BTreeMap::<String, String>::new();
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_server_serves_its_dashboard_and_says_what_it_is() {
    let server = TestServer::start(&[]).await;
    let info = server.admin.server_info().await.unwrap();
    assert_eq!(info.version, rewind_server::VERSION);
    assert!(!info.embedded);
    let page = reqwest::get(format!("{}/", server.url)).await.unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(page.headers()["rewind-version"], rewind_server::VERSION);
    assert!(page.text().await.unwrap().contains("<div id=\"root\">"));
    let missing = reqwest::get(format!("{}/v1/nothing", server.url))
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);

    // A route of the dashboard is a path: the app's page, which reads the route from the URL.
    for route in [
        "/runs/abc",
        "/namespaces/alice",
        "/namespaces/team.alpha",
        "/connect/authorize?request=x",
    ] {
        let page = reqwest::get(format!("{}{route}", server.url))
            .await
            .unwrap();
        assert_eq!(page.status(), 200, "{route}");
        assert_eq!(page.headers()["cache-control"], "no-cache", "{route}");
        assert_eq!(
            page.headers()["content-type"],
            "text/html; charset=utf-8",
            "{route}"
        );
        assert!(page.text().await.unwrap().contains("<div id=\"root\">"));
    }
    let head = reqwest::Client::new()
        .head(format!("{}/overview", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    for path in [
        "/v1/nope",
        "/auth/nope",
        "/oauth/nope",
        "/.well-known/nope",
        "/hooks",
        "/assets/missing.js",
        "/favicon.ico",
    ] {
        let missing = reqwest::get(format!("{}{path}", server.url)).await.unwrap();
        assert_eq!(missing.status(), 404, "{path}");
        assert_eq!(
            missing.headers()["content-type"],
            "application/problem+json",
            "{path}"
        );
    }
    let posted = reqwest::Client::new()
        .post(format!("{}/runs/abc", server.url))
        .send()
        .await
        .unwrap();
    assert_eq!(posted.status(), 404, "a route is read, never posted to");

    assert_eq!(
        problem(server.admin.stop_server().await.unwrap_err()).kind(),
        Some(ProblemType::Forbidden),
        "only the embedded server stops through the API"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_http_action_is_pinned_with_its_allow_list_only() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let action_set = harness
        .create_action_set(&CreateActionSet {
            http: Some(rewind_api::action_sets::HttpAction {
                allow: vec!["api.example.com".into()],
                policy: None,
            }),
            ..CreateActionSet::default()
        })
        .await
        .unwrap();
    let names: Vec<&str> = action_set
        .actions
        .iter()
        .map(|action| action.name.as_str())
        .collect();
    assert_eq!(names, ["http", "webhook"]);
    assert!(
        action_set
            .declarations
            .dts
            .starts_with(rewind_check::HTTP_ACTION_REFERENCE),
        "the declarations bring fetch to the type-checker"
    );
    let checked = harness
        .check(&CheckRequest {
            code: "export default async function main() {\n  const response = await fetch(\"https://api.example.com/x\");\n  return response.status;\n}\n".into(),
            action_set: Some(action_set.id.clone()),
        })
        .await
        .unwrap();
    assert!(checked.ok, "{:?}", checked.diagnostics);
    let without = harness
        .check(&CheckRequest {
            code: "export default async function main() {\n  const response = await fetch(\"https://api.example.com/x\");\n  return response.status;\n}\n".into(),
            action_set: None,
        })
        .await
        .unwrap();
    assert!(!without.ok, "no http action, no fetch");
    let empty = problem(
        harness
            .create_action_set(&CreateActionSet {
                http: Some(rewind_api::action_sets::HttpAction {
                    allow: Vec::new(),
                    policy: None,
                }),
                ..CreateActionSet::default()
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(empty.kind(), Some(ProblemType::InvalidActionSet));
    server.stop().await;
}

/// A code that waits for a signal, its input unused: a run that stays active.
fn waiting_run(marker: &str, text: &str) -> StartRun {
    let mut request = start(&format!("// {marker}\n{SIGNAL}"));
    request.input = json!({ "text": text });
    request
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_limited_to_labels_reads_only_what_its_runs_reference() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let reader = server.key(BuiltInRole::Reader, &[]).await;
    let scoped = server
        .key(BuiltInRole::Harness, &[("conversation", "c-1")])
        .await;

    // Inputs above 1 KiB are held by their digest, in the payloads table.
    let mut theirs = waiting_run("theirs", &"t".repeat(2048));
    theirs.labels = Labels::from_iter([("conversation", "c-2")]);
    let theirs = harness.start_run(&theirs, None).await.unwrap();
    let mine = scoped
        .start_run(&waiting_run("mine", &"m".repeat(2048)), None)
        .await
        .unwrap();
    let input = |client: &rewind_api::Client, run: String| {
        let client = client.clone();
        async move {
            eventually(|| async {
                client
                    .get_run(&run, None)
                    .await
                    .unwrap()
                    .input
                    .map(|input| input.digest)
            })
            .await
        }
    };
    let their_input = input(&harness, theirs.id.clone()).await;
    let my_input = input(&scoped, mine.id.clone()).await;

    // Workflows: only those of the runs the key sees.
    eventually(|| async { scoped.get_script(&mine.code_digest).await.ok() }).await;
    assert_eq!(
        problem(scoped.get_script(&theirs.code_digest).await.unwrap_err()).kind(),
        Some(ProblemType::NotFound),
        "the code of a run the key does not see"
    );
    reader.get_script(&theirs.code_digest).await.unwrap();

    // Payloads: through a run the key sees, which references them.
    assert_eq!(
        scoped.get_payload(&my_input, &mine.id).await.unwrap(),
        json!({ "text": "m".repeat(2048) })
    );
    for (digest, run) in [
        (&their_input, &theirs.id),
        (&their_input, &mine.id),
        (&my_input, &theirs.id),
    ] {
        assert_eq!(
            problem(scoped.get_payload(digest, run).await.unwrap_err()).kind(),
            Some(ProblemType::NotFound),
            "{digest} through {run}"
        );
    }
    let tenant_reader = server.tenant_key("acme", "reader").await;
    assert_eq!(
        tenant_reader
            .get_payload(&their_input, &theirs.id)
            .await
            .unwrap(),
        json!({ "text": "t".repeat(2048) })
    );
    assert_eq!(
        problem(
            reader
                .get_payload(&their_input, &mine.id)
                .await
                .unwrap_err()
        )
        .kind(),
        Some(ProblemType::NotFound),
        "a run given is checked for every key"
    );

    // Who writes and attaches which secrets is the engine-server workstream's
    // (`archi/09-roadmap.md`): its tests come with it.
    let _ = (&harness, &theirs);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn harness_action_failures_are_bounded_like_results() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let action_set = harness
        .create_action_set(&ask_action_set(ActionPolicy::default()))
        .await
        .unwrap();
    let mut request = start(ASK);
    request.action_set = Some(action_set.id.clone());
    let run = harness.start_run(&request, None).await.unwrap();
    let task = TaskWorker::open(&harness, &CallStreamQuery::default())
        .await
        .task()
        .await;
    harness
        .defer_call(&task.token, &DeferCall::default())
        .await
        .unwrap();

    // A failure stays in its event: message and data together hold 64 KiB at most.
    let large = 64 * 1024;
    for failure in [
        FailCall {
            message: "m".repeat(large + 1),
            data: None,
            retryable: false,
        },
        FailCall {
            message: "the user is away".into(),
            data: Some(json!("d".repeat(large))),
            retryable: false,
        },
        FailCall {
            message: "m".repeat(large / 2),
            data: Some(json!("d".repeat(large / 2))),
            retryable: true,
        },
    ] {
        let refused = problem(harness.fail_call(&task.token, &failure).await.unwrap_err());
        assert_eq!(
            refused.kind(),
            Some(ProblemType::PayloadTooLarge),
            "{refused}"
        );
    }
    harness
        .fail_call(
            &task.token,
            &FailCall {
                message: "m".repeat(large / 2),
                data: Some(json!({ "reason": "away" })),
                retryable: false,
            },
        )
        .await
        .unwrap();
    let ended = harness.get_run(&run.id, wait("20s")).await.unwrap();
    assert_eq!(ended.status, RunStatus::Failed);
    assert_eq!(ended.error.unwrap().name, "ActionError");

    // A rejection's message is a failure's message.
    let approved = harness
        .create_action_set(&ask_action_set(ActionPolicy {
            approval: Some(true),
            ..ActionPolicy::default()
        }))
        .await
        .unwrap();
    request.action_set = Some(approved.id.clone());
    let run = harness.start_run(&request, None).await.unwrap();
    let call_id = eventually(|| async {
        let state = harness.get_run(&run.id, None).await.unwrap();
        state
            .waiting_for
            .into_iter()
            .find_map(|waiting| match waiting {
                Waiting::Call { call_id, .. } => Some(call_id),
                _ => None,
            })
    })
    .await;
    let refused = problem(
        harness
            .reject_call(
                &call_id,
                &RejectCall {
                    message: Some("r".repeat(large + 1)),
                },
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(
        refused.kind(),
        Some(ProblemType::PayloadTooLarge),
        "{refused}"
    );
    harness
        .reject_call(
            &call_id,
            &RejectCall {
                message: Some("not now".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        harness.get_run(&run.id, wait("20s")).await.unwrap().status,
        RunStatus::Failed
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deployed_server_sends_no_webhook_to_the_private_network() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let subscribe = |url: &str| {
        let harness = harness.clone();
        let request = CreateSubscription {
            url: url.into(),
            events: Vec::new(),
            labels: Labels::new(),
        };
        async move { harness.create_subscription(&request).await }
    };
    for url in [
        "http://example.com/hook",
        "http://localhost:8080/hook",
        "http://127.0.0.1:8080/hook",
        "https://localhost/hook",
        "https://127.0.0.1/hook",
        "https://[::1]/hook",
        "https://10.1.2.3/hook",
        "https://192.168.1.1/hook",
        "https://169.254.169.254/latest/meta-data",
        "https://[fd00:ec2::254]/hook",
        "https://[::ffff:127.0.0.1]/hook",
    ] {
        let refused = problem(subscribe(url).await.unwrap_err());
        assert_eq!(
            refused.kind(),
            Some(ProblemType::InvalidRequest),
            "{url}: {refused}"
        );
    }
    assert!(
        harness
            .list_subscriptions()
            .await
            .unwrap()
            .subscriptions
            .is_empty()
    );
    subscribe("https://93.184.215.14/hook").await.unwrap();
    server.stop().await;

    // An operator may allow them, on a trusted network.
    let trusted = TestServer::start(&["--allow-private-webhooks"]).await;
    let harness = trusted.key(BuiltInRole::Harness, &[]).await;
    harness
        .create_subscription(&CreateSubscription {
            url: "http://127.0.0.1:8080/hook".into(),
            events: Vec::new(),
            labels: Labels::new(),
        })
        .await
        .unwrap();
    trusted.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lookups_with_a_nul_character_match_nothing() {
    let server = TestServer::start(&[]).await;
    support::nul_lookups_match_nothing(&server).await;
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn text_with_a_nul_character_is_refused_with_400() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let invalid = |error: rewind_api::Error| {
        let problem = problem(error);
        assert_eq!(
            problem.kind(),
            Some(ProblemType::InvalidRequest),
            "{problem:?}"
        );
        assert_eq!(problem.status, 400);
        assert!(
            problem.detail.contains("NUL character"),
            "{}",
            problem.detail
        );
    };

    let mut labelled = start(SLEEP);
    labelled.input = json!({ "ms": 1 });
    labelled.labels = Labels::from_iter([("agent", "tri\u{0}age")]);
    invalid(harness.start_run(&labelled, None).await.unwrap_err());
    let mut source = start(&format!("{SLEEP}// \u{0}\n"));
    source.input = json!({ "ms": 1 });
    invalid(harness.start_run(&source, None).await.unwrap_err());
    let mut action_set = ask_action_set(ActionPolicy::default());
    action_set.harness[0].description = "Ask the end user\u{0}".into();
    invalid(harness.create_action_set(&action_set).await.unwrap_err());
    let subscription = CreateSubscription {
        url: "https://hooks.example.com/rewind".into(),
        events: vec![WebhookEventType::RunCompleted],
        labels: Labels::from_iter([("conversation", "\u{0}")]),
    };
    invalid(
        harness
            .create_subscription(&subscription)
            .await
            .unwrap_err(),
    );

    // Values are JSON in the store, where it is escaped: they keep it.
    let mut request = start(SLEEP);
    request.input = json!({ "ms": 1, "note": "a\u{0}b" });
    request.wait = wait("20s");
    let run = harness.start_run(&request, None).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed);
    server.stop().await;
}

const FAIL: &str = r#"export default async function main(input: { n: number }) {
  throw new TypeError(`bad ${input.n}`);
}
"#;

/// The ids of a page of runs, sorted.
async fn found(client: &rewind_api::Client, q: &str) -> Vec<String> {
    let mut ids: Vec<String> = client
        .list_runs(&RunFilter {
            q: Some(q.into()),
            ..RunFilter::default()
        })
        .await
        .unwrap()
        .runs
        .into_iter()
        .map(|run| run.id)
        .collect();
    ids.sort();
    ids
}

fn sorted(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn runs_are_searched_counted_and_grouped_by_failure() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let labelled = |code: &str, labels: &[(&str, &str)]| {
        let mut request = start(code);
        request.labels = labels.iter().copied().collect();
        request
    };

    let mut waiting = labelled(SIGNAL, &[("agent", "triage"), ("env", "prod")]);
    waiting.wait = wait("100ms");
    let waiting = harness.start_run(&waiting, None).await.unwrap();
    let mut failed = Vec::new();
    for n in [1, 2] {
        let mut request = labelled(FAIL, &[("agent", "triage"), ("env", "test")]);
        request.input = json!({ "n": n });
        request.wait = wait("20s");
        let run = harness.start_run(&request, None).await.unwrap();
        assert_eq!(run.status, RunStatus::Failed);
        failed.push(run.id);
    }
    let mut done = labelled(SLEEP, &[("agent", "support")]);
    done.input = json!({ "ms": 10 });
    done.wait = wait("20s");
    let done = harness.start_run(&done, None).await.unwrap();
    let action_set = harness
        .create_action_set(&ask_action_set(ActionPolicy {
            approval: Some(true),
            ..ActionPolicy::default()
        }))
        .await
        .unwrap();
    let mut asking = start(ASK);
    asking.action_set = Some(action_set.id.clone());
    let asking = harness.start_run(&asking, None).await.unwrap();

    // Once every run is found.
    eventually(|| async {
        let all = found(&harness, "").await;
        let approval = found(&harness, "waiting:approval").await;
        (all.len() == 5 && approval == vec![asking.id.clone()]).then_some(())
    })
    .await;

    let triage = sorted(vec![
        waiting.id.clone(),
        failed[0].clone(),
        failed[1].clone(),
    ]);
    assert_eq!(found(&harness, "@agent:triage").await, triage);
    assert_eq!(found(&harness, "@agent:tri*").await, triage);
    assert_eq!(
        found(&harness, "@agent:* -@agent:triage").await,
        vec![done.id.clone()]
    );
    assert_eq!(
        found(&harness, "waiting:signal signal:approval").await,
        vec![waiting.id.clone()]
    );
    assert_eq!(
        found(&harness, "status:failed error:TypeError \"bad 2\"").await,
        vec![failed[1].clone()]
    );
    assert_eq!(
        found(&harness, "status:completed,active -waiting:*").await,
        vec![done.id.clone()]
    );
    assert_eq!(
        found(&harness, &format!("id:{}", &waiting.id[..13])).await,
        vec![waiting.id.clone()]
    );

    // A run in a list says what it waits for, and how it failed.
    let page = harness
        .list_runs(&RunFilter {
            q: Some("@agent:triage".into()),
            sort: Some(RunSort::Wakes),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    let signal = page.runs.iter().find(|run| run.id == waiting.id).unwrap();
    assert_eq!(signal.waiting_for.len(), 1);
    assert_eq!(signal.waiting_for[0].kind, WaitKind::Signal);
    assert_eq!(signal.waiting_for[0].name.as_deref(), Some("approval"));
    assert_eq!(signal.waiting_for[0].callsite.unwrap().line, 3);
    let bad = page.runs.iter().find(|run| run.id == failed[0]).unwrap();
    assert_eq!(bad.error.as_deref(), Some("TypeError"));
    assert_eq!(bad.error_message.as_deref(), Some("bad 1"));
    assert_eq!(bad.error_line, Some(2));
    let fingerprint = bad.fingerprint.clone().unwrap();
    assert_eq!(
        found(&harness, &format!("fingerprint:{fingerprint}")).await,
        sorted(failed.clone())
    );

    // A query that cannot be read says where.
    let refused = problem(
        harness
            .list_runs(&RunFilter {
                q: Some("@agent:triage stauts:failed".into()),
                ..RunFilter::default()
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::InvalidQuery));
    assert_eq!(refused.status, 400);
    let span = refused.query.unwrap();
    assert_eq!((span.start, span.end), (14, 27));
    assert!(
        refused.detail.contains("unknown field \"stauts\""),
        "{}",
        refused.detail
    );

    // Statistics, from the counters: a scope (the tenant, or one namespace) and whole hours.
    let refused = problem(
        harness
            .run_stats(&RunStatsQuery {
                q: Some("@agent:triage".into()),
                ..RunStatsQuery::default()
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::InvalidQuery), "{refused}");
    for q in [None, Some("namespace:tests".to_owned())] {
        let stats = eventually(|| async {
            let stats = harness
                .run_stats(&RunStatsQuery {
                    q: q.clone(),
                    ..RunStatsQuery::default()
                })
                .await
                .unwrap();
            (stats.statuses.failed == 2).then_some(stats)
        })
        .await;
        assert_eq!(stats.runs, 5);
        assert_eq!(stats.statuses.completed, 1);
        assert_eq!(stats.statuses.active, 2);
        assert_eq!(stats.waiting.signal, 1);
        assert_eq!(stats.waiting.approval, 1);
        assert_eq!(stats.facets.namespaces[0].value, "tests");
    }
    let now = jiff::Timestamp::now();
    let series = harness
        .run_stats(&RunStatsQuery {
            since: Some(now - jiff::SignedDuration::from_hours(1)),
            until: Some(now + jiff::SignedDuration::from_secs(1)),
            bucket: Some("10m".parse().unwrap()),
            ..RunStatsQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(
        series.bucket,
        Some("1h".parse().unwrap()),
        "a bucket is rounded up to whole hours"
    );
    assert!((2..=3).contains(&series.series.len()), "{series:?}");
    let failed_in_series: u64 = series
        .series
        .iter()
        .map(|bucket| bucket.statuses.failed)
        .sum();
    assert_eq!(failed_in_series, 2);
    let too_many = problem(
        harness
            .run_stats(&RunStatsQuery {
                since: Some(jiff::Timestamp::now() - jiff::SignedDuration::from_hours(24 * 30)),
                bucket: Some("1h".parse().unwrap()),
                ..RunStatsQuery::default()
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(too_many.kind(), Some(ProblemType::InvalidRequest));

    // Failures grouped by fingerprint, across inputs.
    let failures = harness
        .run_failures(&FailureQuery::default())
        .await
        .unwrap();
    assert_eq!(failures.total_groups, 1);
    let group = &failures.groups[0];
    assert_eq!(group.fingerprint, fingerprint);
    assert_eq!(group.signature, "TypeError: bad <n>");
    assert_eq!(group.runs, 2);
    assert_eq!(group.line, Some(2));
    assert!(failed.contains(&group.latest.id));
    assert_eq!(
        group
            .labels
            .iter()
            .map(|label| (label.value.as_str(), label.runs))
            .collect::<Vec<_>>(),
        [("agent:triage", 2), ("env:test", 2)],
        "the labels of the group's latest runs"
    );

    // A key limited to labels searches within them.
    let support = server
        .key(BuiltInRole::Reader, &[("agent", "support")])
        .await;
    assert_eq!(found(&support, "").await, vec![done.id.clone()]);
    assert!(found(&support, "@agent:triage").await.is_empty());
    assert_eq!(
        problem(
            support
                .run_stats(&RunStatsQuery::default())
                .await
                .unwrap_err()
        )
        .kind(),
        Some(ProblemType::Forbidden),
        "counters know no labels"
    );
    server.stop().await;
}
