//! The API `/v1` end to end over HTTP, on Turso: runs, idempotency, type-checking at start,
//! exports, host-tool tasks, approvals, signals, inbound and outbound webhooks, the event
//! stream, quotas, keys, roles and label scopes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::HeaderMap;
use futures_util::StreamExt;
use ptc_api::admin::Role;
use ptc_api::common::{Labels, RunStatus};
use ptc_api::events::{Event, EventsQuery, StreamEvent};
use ptc_api::headers::IdempotencyKey;
use ptc_api::runs::{CancelRun, ForkRun, JournalExport, RunFilter, StartRun, TraceFormat, Waiting};
use ptc_api::scripts::{CheckRequest, ScriptFilter};
use ptc_api::tasks::{CompleteTask, DeferTask, FailTask, RejectTask, TaskQuery, TaskState};
use ptc_api::toolsets::{CreateToolset, HostTool, ToolPolicy};
use ptc_api::webhooks::{
    CreateSubscription, SignedHeaders, WebhookEvent, WebhookEventType, WebhookSecret,
};
use ptc_api::{Problem, ProblemType};
use serde_json::json;
use support::{TestServer, eventually, wait};

const SLEEP: &str = r#"import { sleep } from "ptc";
export default async function main(input: { ms: number }) {
  await sleep(input.ms);
  console.log("slept", input.ms);
  return { slept: input.ms };
}
"#;

const SIGNAL: &str = r#"import { signal } from "ptc";
export default async function main() {
  const decision = await signal<{ ok: boolean }>("approval");
  return { decision };
}
"#;

const ASK: &str = r#"import { ask_user } from "ptc:tools";
export default async function main() {
  const answer = await ask_user({ question: "ready?" });
  return { answer };
}
"#;

const HOOK: &str = r#"import { signal, webhook } from "ptc";
export default async function main() {
  const hook = await webhook("pr_merged");
  console.log(hook.url);
  await signal("go");
  const first = await signal("pr_merged");
  const second = await signal("pr_merged", { timeout: "50ms" });
  return { first, second: second ?? null };
}
"#;

fn problem(error: ptc_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

fn start(script: &str) -> StartRun {
    StartRun::new(script)
}

fn ask_toolset(policy: ToolPolicy) -> CreateToolset {
    CreateToolset {
        host: vec![HostTool {
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
        ..CreateToolset::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_starts_waits_and_is_read_back() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(Role::Harness, &[]).await;
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
    let trace = harness.run_trace(&run.id, TraceFormat::Llm).await.unwrap();
    assert!(trace.text.contains("completed"), "{}", trace.text);
    let entry = harness.run_entry(&run.id, 0).await.unwrap();
    assert!(matches!(entry.event, Event::RunStarted { .. }));
    assert_eq!(
        problem(harness.run_entry(&run.id, 99).await.unwrap_err()).kind(),
        Some(ProblemType::NotFound)
    );

    let journal = harness
        .export_journal(&run.id, JournalExport::Journal)
        .await
        .unwrap();
    let document = ptc_format::decode_journal(&journal.to_string()).unwrap();
    assert_eq!(document.run.to_string(), run.id);
    assert_eq!(document.entries.len() as u64, run.history_len);
    let cassette = harness
        .export_journal(&run.id, JournalExport::Cassette)
        .await
        .unwrap();
    assert_eq!(cassette["v"], 1);

    // The read model lists it once the projector has applied its entries.
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
    assert_eq!(listed.headline, "import { sleep } from \"ptc\";");
    let scripts = harness
        .list_scripts(&ScriptFilter {
            digest: Some(run.script.clone()),
            ..ScriptFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(scripts.scripts.len(), 1);
    assert_eq!(scripts.scripts[0].runs.completed, 1);
    let script = harness.get_script(&run.script).await.unwrap();
    assert_eq!(script.source, SLEEP);
    assert_eq!(script.runs.completed, 1);

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
    let harness = server.key(Role::Harness, &[]).await;
    let script = "export default function main() {\n  const n: string = 42;\n  return n;\n}\n";
    let refused = problem(harness.start_run(&start(script), None).await.unwrap_err());
    assert_eq!(refused.kind(), Some(ProblemType::InvalidScript));
    assert_eq!(refused.status, 422);
    let diagnostic = &refused.diagnostics[0];
    assert_eq!(diagnostic.code, "TS2322");
    assert_eq!(diagnostic.location.unwrap().line, 2);

    let checked = harness
        .check(&CheckRequest {
            script: script.into(),
            toolset: None,
        })
        .await
        .unwrap();
    assert!(!checked.ok);

    let mut skipped = start(script);
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
    assert_eq!(invalid.kind(), Some(ProblemType::InvalidScript));
    assert_eq!(invalid.diagnostics[0].code, "ptc/invalid-script");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_tools_are_long_polled_heartbeated_and_answered() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(Role::Harness, &[]).await;
    let worker = server.key(Role::Worker, &[]).await;
    let toolset = harness
        .create_toolset(&ask_toolset(ToolPolicy {
            heartbeat_timeout: Some("5s".parse().unwrap()),
            ..ToolPolicy::default()
        }))
        .await
        .unwrap();
    assert!(toolset.tools.iter().any(|tool| tool.name == "webhook"));
    assert_eq!(harness.list_toolsets().await.unwrap().toolsets.len(), 1);

    assert!(
        toolset
            .declarations
            .dts
            .contains("export function ask_user"),
        "{}",
        toolset.declarations.dts
    );
    let mut wrong = start(&ASK.replace("\"ready?\"", "42"));
    wrong.toolset = Some(toolset.id.clone());
    let refused = problem(harness.start_run(&wrong, None).await.unwrap_err());
    assert_eq!(refused.kind(), Some(ProblemType::InvalidScript));
    assert_eq!(refused.diagnostics[0].code, "TS2322", "{refused}");

    let mut request = start(ASK);
    request.toolset = Some(toolset.id.clone());
    let run = harness.start_run(&request, None).await.unwrap();

    let query = TaskQuery {
        toolset: Some(toolset.id.clone()),
        tools: vec!["ask_user".into()],
        limit: Some(1),
        wait: Some("10s".parse().unwrap()),
    };
    let batch = worker.claim_tasks(&query).await.unwrap();
    let task = &batch.tasks[0];
    assert_eq!(task.run_id, run.id);
    assert_eq!(task.args, json!({ "question": "ready?" }));
    assert_eq!(task.idempotency_key, format!("{}/1", run.id));
    assert_eq!(task.heartbeat_timeout_ms, 5_000);
    assert_eq!(
        task.timeout_ms, None,
        "a host tool has no execution timeout"
    );
    let ack = worker.heartbeat_task(&task.token).await.unwrap();
    assert!(ack.heartbeat_timeout_ms > 0);

    // Deferred, then answered later through the same token.
    worker
        .defer_task(&task.token, &DeferTask::default())
        .await
        .unwrap();
    let state = harness.get_run(&run.id, None).await.unwrap();
    assert!(matches!(
        &state.waiting_for[0],
        Waiting::ToolCall {
            state: TaskState::Deferred,
            ..
        }
    ));
    worker
        .complete_task(
            &task.token,
            &CompleteTask {
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
        problem(worker.heartbeat_task(&task.token).await.unwrap_err()).kind(),
        Some(ProblemType::TaskGone)
    );

    // A retryable failure is retried: the next claim is attempt 2.
    let run = harness.start_run(&request, None).await.unwrap();
    let first = worker.claim_tasks(&query).await.unwrap().tasks.remove(0);
    worker
        .fail_task(
            &first.token,
            &FailTask {
                message: "the user is away".into(),
                data: None,
                retryable: true,
            },
        )
        .await
        .unwrap();
    let mut retry_query = query.clone();
    retry_query.wait = Some("20s".parse().unwrap());
    let second = worker
        .claim_tasks(&retry_query)
        .await
        .unwrap()
        .tasks
        .remove(0);
    assert_eq!(
        (second.run_id.as_str(), second.attempt),
        (run.id.as_str(), 2)
    );
    worker
        .complete_task(
            &second.token,
            &CompleteTask {
                result: json!("back"),
            },
        )
        .await
        .unwrap();
    let ended = harness.get_run(&run.id, wait("20s")).await.unwrap();
    assert_eq!(ended.status, RunStatus::Completed);
    let attempts = eventually(|| async {
        let run = harness.get_run(&run.id, None).await.unwrap();
        (!run.attempts.is_empty()).then_some(run.attempts)
    })
    .await;
    assert_eq!((attempts[0].seq, attempts[0].attempts), (1, 2));
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_under_approval_waits_for_the_harness_with_its_arguments() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(Role::Harness, &[]).await;
    let toolset = harness
        .create_toolset(&ask_toolset(ToolPolicy {
            approval: Some(true),
            ..ToolPolicy::default()
        }))
        .await
        .unwrap();
    let mut request = start(ASK);
    request.toolset = Some(toolset.id.clone());

    // Rejected: the call fails with a ToolError, and the run fails.
    let run = harness.start_run(&request, None).await.unwrap();
    let (task_id, args) = eventually(|| async {
        let state = harness.get_run(&run.id, None).await.unwrap();
        state
            .waiting_for
            .into_iter()
            .find_map(|waiting| match waiting {
                Waiting::ToolCall {
                    task_id,
                    state: TaskState::Approval,
                    args,
                    ..
                } => Some((task_id, args)),
                _ => None,
            })
    })
    .await;
    assert_eq!(args.value, Some(json!({ "question": "ready?" })));
    harness
        .reject_task(
            &task_id,
            &RejectTask {
                message: Some("not now".into()),
            },
        )
        .await
        .unwrap();
    let ended = harness.get_run(&run.id, wait("20s")).await.unwrap();
    assert_eq!(ended.status, RunStatus::Failed);
    let error = ended.error.unwrap();
    assert_eq!(error.name, "ToolError");
    assert!(error.message.contains("not now"), "{}", error.message);

    // Approved: the worker gets it.
    let run = harness.start_run(&request, None).await.unwrap();
    let task_id = eventually(|| async {
        let state = harness.get_run(&run.id, None).await.unwrap();
        state
            .waiting_for
            .into_iter()
            .find_map(|waiting| match waiting {
                Waiting::ToolCall { task_id, .. } => Some(task_id),
                _ => None,
            })
    })
    .await;
    assert_eq!(
        problem(harness.approve_task("nope.1").await.unwrap_err()).kind(),
        Some(ProblemType::NotFound)
    );
    harness.approve_task(&task_id).await.unwrap();
    assert_eq!(
        problem(harness.approve_task(&task_id).await.unwrap_err()).kind(),
        Some(ProblemType::TaskNotWaiting)
    );
    let task = harness
        .claim_tasks(&TaskQuery {
            wait: Some("10s".parse().unwrap()),
            ..TaskQuery::default()
        })
        .await
        .unwrap()
        .tasks
        .remove(0);
    harness
        .complete_task(
            &task.token,
            &CompleteTask {
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
    let harness = server.key(Role::Harness, &[]).await;
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
    let server = TestServer::start(&["--public-url", "https://ptc.example.com"]).await;
    let harness = server.key(Role::Harness, &[]).await;
    let run = harness.start_run(&start(HOOK), None).await.unwrap();
    let url = eventually(|| async {
        let state = harness.get_run(&run.id, None).await.unwrap();
        state.logs.first().map(|line| line.message.clone())
    })
    .await;
    let token = url
        .strip_prefix("https://ptc.example.com/hooks/")
        .unwrap_or_else(|| panic!("unexpected webhook URL {url}"));
    // Anyone may post to the URL: no key.
    let anonymous = ptc_api::Client::remote(&server.url, None).unwrap();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_webhooks_are_signed_and_filtered_by_labels() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(Role::Harness, &[]).await;
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
    let harness = server.key(Role::Reader, &[]).await;
    let starter = server.key(Role::Harness, &[]).await;
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
            StreamEvent::Entry(entry) => names.push(entry.event.name()),
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
        if let StreamEvent::Entry(entry) = event.unwrap() {
            assert!(entry.idx >= 3);
            count += 1;
        }
    }
    assert_eq!(count, 2);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_above_its_quota_is_refused_with_429() {
    let server = TestServer::start(&["--run-quota", "1"]).await;
    let harness = server.key_of("small", Role::Harness, &[]).await;
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
    let harness = server.key(Role::Harness, &[]).await;
    let reader = server.key(Role::Reader, &[]).await;
    let worker = server.key(Role::Worker, &[]).await;
    let scoped = server.key(Role::Harness, &[("conversation", "c-1")]).await;
    let stranger = server.key_of("other", Role::Harness, &[]).await;

    let unknown = ptc_api::Client::remote(&server.url, Some("ptc_sk_unknown")).unwrap();
    assert_eq!(
        problem(unknown.list_runs(&RunFilter::default()).await.unwrap_err()).kind(),
        Some(ProblemType::Unauthorized)
    );
    let anonymous = ptc_api::Client::remote(&server.url, None).unwrap();
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
    assert_eq!((me.tenant.as_str(), me.role), ("acme", Role::Harness));
    assert_eq!(
        me.allowed_labels,
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

    // A fork without labels keeps its parent's: the scoped key sees it.
    harness
        .signal_run(&mine.id, "approval", &json!({ "ok": true }), None)
        .await
        .unwrap();
    harness.get_run(&mine.id, wait("20s")).await.unwrap();
    let mut fork = ForkRun::new(SIGNAL);
    fork.at = Some(1);
    let forked = scoped.fork_run(&mine.id, &fork, None).await.unwrap();
    assert_eq!(forked.labels, Labels::from_iter([("conversation", "c-1")]));
    assert_eq!(forked.parent.unwrap().run, mine.id);

    // A revoked key is refused.
    let made = server
        .admin
        .create_api_key(&ptc_api::admin::CreateApiKey {
            tenant: "acme".into(),
            role: Role::Reader,
            allowed_labels: Labels::new(),
            name: None,
        })
        .await
        .unwrap();
    server.admin.revoke_api_key(&made.id).await.unwrap();
    let revoked = ptc_api::Client::remote(&server.url, made.key.as_deref()).unwrap();
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
    assert_eq!(info.version, ptc_server::VERSION);
    assert!(!info.embedded);
    let page = reqwest::get(format!("{}/", server.url)).await.unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(page.headers()["ptc-version"], ptc_server::VERSION);
    assert!(page.text().await.unwrap().contains("<div id=\"root\">"));
    let missing = reqwest::get(format!("{}/v1/nothing", server.url))
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    assert_eq!(
        problem(server.admin.stop_server().await.unwrap_err()).kind(),
        Some(ProblemType::Forbidden),
        "only the embedded server stops through the API"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_http_tool_is_pinned_with_its_allow_list_only() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(Role::Harness, &[]).await;
    let toolset = harness
        .create_toolset(&CreateToolset {
            http: Some(ptc_api::toolsets::HttpTool {
                allow: vec!["api.example.com".into()],
                policy: None,
            }),
            ..CreateToolset::default()
        })
        .await
        .unwrap();
    let names: Vec<&str> = toolset
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(names, ["http", "webhook"]);
    assert!(
        toolset
            .declarations
            .dts
            .starts_with(ptc_check::HTTP_TOOL_REFERENCE),
        "the declarations bring fetch to the type-checker"
    );
    let checked = harness
        .check(&CheckRequest {
            script: "export default async function main() {\n  const response = await fetch(\"https://api.example.com/x\");\n  return response.status;\n}\n".into(),
            toolset: Some(toolset.id.clone()),
        })
        .await
        .unwrap();
    assert!(checked.ok, "{:?}", checked.diagnostics);
    let without = harness
        .check(&CheckRequest {
            script: "export default async function main() {\n  const response = await fetch(\"https://api.example.com/x\");\n  return response.status;\n}\n".into(),
            toolset: None,
        })
        .await
        .unwrap();
    assert!(!without.ok, "no http tool, no fetch");
    let empty = problem(
        harness
            .create_toolset(&CreateToolset {
                http: Some(ptc_api::toolsets::HttpTool {
                    allow: Vec::new(),
                    policy: None,
                }),
                ..CreateToolset::default()
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(empty.kind(), Some(ProblemType::InvalidToolset));
    server.stop().await;
}
