//! The tool tasks' use cases: policies and backoff, claims and heartbeats, refused calls, parked
//! tasks and their webhooks, approvals, the host-tool worker, the `webhook` built-in, tenants.

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use rewind_core::{
    Builtin, Completion, Event, Interruption, JsonValue, McpServer, McpTransport, RunMode,
    RunStatus, Script, Seq, Timestamp, Tool, ToolFailure, ToolFailureKind, ToolPolicy, ToolRoute,
    Toolset,
};
use rewind_engine::ports::{
    CallAuth, Clock, CredentialsExpiredReason, Delivery, Dispatched, JournalStore, ParkKind,
    ParkReason, RunRef, Secret, Subscription, TaskFilter, TaskRef, TaskState, Tenant, TenantId,
    TenantScope, ToolCall, ToolDispatcher, VirtualTime, WebhookEventKind,
};
use rewind_engine::{
    Backoff, DriveOptions, Engine, EngineConfig, EngineError, HostClaim, HostFailure, StartError,
    StartRun,
};
use serde_json::json;
use support::*;

// -------------------------------------------------------------------------------------------
// Helpers

const ISSUE: &str = "github.get_issue";

/// Calls the tool named in its input (`{"tool": …}`), and completes with what came back:
/// `{"result": …}`, or `{"error": kind, "message": …}`.
fn caller() -> FakeExecutor {
    FakeExecutor::new().with(
        "caller",
        |run| {
            let tool = run.input["tool"].as_str().unwrap().to_owned();
            let args = run.input.get("args").cloned().unwrap_or(json!({}));
            run.call(&tool, args);
        },
        |run, delivery| match delivery {
            Delivery::ToolCallCompleted { result, .. } => {
                run.complete(json!({ "result": result }));
            }
            Delivery::ToolCallFailed { error, .. } => run.complete(json!({
                "error": error.kind.as_str(),
                "message": error.message,
            })),
            other => panic!("unexpected {other:?}"),
        },
    )
}

/// An MCP server `github` with the tool `github.get_issue` (its policy changed by `policy`),
/// the host tool `ask_user` and the `webhook` built-in.
fn toolset(policy: impl Fn(&mut ToolPolicy)) -> Toolset {
    let server = McpServer::new(
        "github",
        McpTransport::Http {
            url: "https://mcp.example.com/github".into(),
        },
    );
    let mut issue = Tool::new(
        ISSUE,
        ToolRoute::Mcp {
            server: "github".into(),
        },
        json!({}),
    );
    policy(&mut issue.policy);
    let mut ask = Tool::new("ask_user", ToolRoute::Host { group: None }, json!({}));
    policy(&mut ask.policy);
    let webhook = Tool::new("webhook", ToolRoute::Builtin(Builtin::Webhook), json!({}));
    Toolset::new([server], [issue, ask, webhook]).unwrap()
}

fn calling(tool: &str, toolset: Toolset) -> StartRun {
    StartRun::new(Script::new("caller"), toolset, json!({ "tool": tool }))
}

fn failure(kind: ToolFailureKind, message: &str) -> ToolFailure {
    ToolFailure {
        kind,
        message: message.into(),
        data: None,
    }
}

async fn drive(engine: &TestEngine) {
    let report = engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    assert!(!report.interrupted);
    assert!(report.failures.is_empty(), "{:?}", report.failures);
}

/// The run's output, once completed.
async fn output(engine: &TestEngine, run: &RunRef) -> JsonValue {
    let history = engine.history(run).await.unwrap();
    match &history.last().unwrap().event {
        Event::Command(rewind_core::Command::CompleteRun { output }) => {
            output.value().unwrap().clone()
        }
        other => panic!("the run has not completed: {other:?}"),
    }
}

/// When the completion of call `seq` was recorded.
async fn completed_at(engine: &TestEngine, run: &RunRef, seq: u32) -> Timestamp {
    let history = engine.history(run).await.unwrap();
    history
        .iter()
        .find(|entry| match &entry.event {
            Event::Completion(
                Completion::ToolCallCompleted { seq: done, .. }
                | Completion::ToolCallFailed { seq: done, .. },
            ) => *done == Seq(seq),
            _ => false,
        })
        .map(|entry| entry.recorded_at)
        .unwrap()
}

async fn status(engine: &TestEngine, run: &RunRef) -> RunStatus {
    engine.get_run(run).await.unwrap().unwrap().status
}

async fn task_state(engine: &TestEngine, run: &RunRef) -> TaskState {
    engine.tool_tasks(run).await.unwrap().remove(0).state
}

fn task(run: &RunRef, seq: u32) -> TaskRef {
    TaskRef {
        run: run.clone(),
        seq: Seq(seq),
    }
}

/// An engine whose host tools wait for a harness's worker, as on a server.
fn worker_config() -> EngineConfig {
    EngineConfig {
        tool_tasks: TaskFilter::Dispatched {
            tenants: TenantScope::All,
        },
        ..EngineConfig::default()
    }
}

fn host_claim(tenant: &TenantId) -> HostClaim {
    HostClaim {
        tenant: tenant.clone(),
        namespace: None,
        labels: BTreeMap::new(),
        toolset: None,
        tools: BTreeSet::new(),
        limit: 10,
    }
}

async fn subscribe(engine: &TestEngine, labels: BTreeMap<String, String>) {
    engine
        .journal()
        .put_subscription(&Subscription {
            tenant: TenantId::local(),
            id: format!("sub-{}", labels.len()),
            url: "https://harness.example.com/hooks".into(),
            secret: Secret::new("whsec_test"),
            events: BTreeSet::new(),
            labels,
        })
        .await
        .unwrap();
}

// -------------------------------------------------------------------------------------------
// Policies and backoff

#[tokio::test]
async fn a_transient_failure_is_retried_after_a_backoff() {
    let dispatcher = RecordingDispatcher::dispatching(|call, _| {
        if call.attempt < 3 {
            Dispatched::Transient(failure(ToolFailureKind::Unavailable, "overloaded"))
        } else {
            Dispatched::Result(json!({ "number": 4 }))
        }
    });
    let engine = engine(caller(), dispatcher.clone());
    let run = engine
        .start_run(calling(ISSUE, toolset(|_| {})))
        .await
        .unwrap()
        .run;
    drive(&engine).await;

    assert_eq!(
        output(&engine, &run).await,
        json!({ "result": { "number": 4 } })
    );
    let calls = dispatcher.calls();
    assert_eq!(
        calls.iter().map(|call| call.attempt).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    let key = format!("{}/1", run.id);
    assert!(calls.iter().all(|call| call.idempotency_key == key));

    // In virtual time the clock jumped over both backoffs.
    let backoff = Backoff::default();
    let waited = backoff.delay(&key, 1) + backoff.delay(&key, 2);
    assert_eq!(
        completed_at(&engine, &run, 1).await,
        T0.saturating_add(waited)
    );
}

#[tokio::test]
async fn an_engine_restarted_during_a_backoff_wakes_up_for_the_retry() {
    let dispatcher = RecordingDispatcher::dispatching(|call, _| {
        if call.attempt < 2 {
            Dispatched::Transient(failure(ToolFailureKind::Unavailable, "overloaded"))
        } else {
            Dispatched::Result(json!({ "number": 4 }))
        }
    });
    let engine = engine(caller(), dispatcher.clone());
    let run = engine
        .start_run(calling(ISSUE, toolset(|_| {})))
        .await
        .unwrap()
        .run;
    // At the current time only: the first attempt fails, and the retry waits for its backoff.
    engine
        .run_until_idle(&DriveOptions::default())
        .await
        .unwrap();
    assert_eq!(dispatcher.calls().len(), 1);
    let TaskState::Ready { next_attempt_at } = task_state(&engine, &run).await else {
        panic!("the task waits for its next attempt");
    };

    // Another engine on the same store knows nothing of the first one's retries: the store
    // tells it when the call is due.
    let engine = Engine::new(engine.into_adapters(), EngineConfig::default());
    assert_eq!(
        engine.next_tool_wake_up().await.unwrap(),
        Some(next_attempt_at)
    );
    drive(&engine).await;
    assert_eq!(dispatcher.calls().len(), 2);
    assert_eq!(
        output(&engine, &run).await,
        json!({ "result": { "number": 4 } })
    );
    assert_eq!(completed_at(&engine, &run, 1).await, next_attempt_at);
}

#[tokio::test]
async fn a_transient_failure_is_delivered_after_the_last_attempt() {
    let dispatcher = RecordingDispatcher::dispatching(|_, _| {
        Dispatched::Transient(failure(ToolFailureKind::Unavailable, "unreachable"))
    });
    let engine = engine(caller(), dispatcher.clone());
    let run = engine
        .start_run(calling(ISSUE, toolset(|_| {})))
        .await
        .unwrap()
        .run;
    drive(&engine).await;

    assert_eq!(dispatcher.calls().len(), 5);
    assert_eq!(
        output(&engine, &run).await,
        json!({ "error": "unavailable", "message": "unreachable" })
    );
}

#[tokio::test]
async fn an_at_most_once_tool_is_never_called_twice() {
    let dispatcher = RecordingDispatcher::dispatching(|_, _| {
        Dispatched::Transient(failure(ToolFailureKind::Timeout, "no answer in 5 min"))
    });
    let engine = engine(caller(), dispatcher.clone());
    let at_most_once = toolset(|policy| policy.on_interruption = Interruption::AtMostOnce);
    let run = engine
        .start_run(calling(ISSUE, at_most_once.clone()))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    assert_eq!(
        dispatcher.calls().len(),
        1,
        "a transient failure is delivered"
    );
    assert_eq!(
        output(&engine, &run).await,
        json!({ "error": "timeout", "message": "no answer in 5 min" })
    );

    // A dispatcher dies mid-call: the outcome is unknown, and the call is not made again.
    let interrupted = engine
        .start_run(calling(ISSUE, at_most_once))
        .await
        .unwrap()
        .run;
    engine.activate_next().await.unwrap();
    assert_eq!(engine.claim_tool_tasks(10).await.unwrap().len(), 1);
    let expired = engine.clock().now().saturating_add(Duration::from_secs(60));
    engine.clock().advance_to(expired);
    drive(&engine).await;
    assert_eq!(dispatcher.calls().len(), 1);
    assert_eq!(
        output(&engine, &interrupted).await,
        json!({
            "error": "outcome_unknown",
            "message": "the call to github.get_issue was interrupted and may or may not have \
                        taken effect; the tool is at_most_once, so it is not called again",
        })
    );
}

// -------------------------------------------------------------------------------------------
// Claims and heartbeats

/// Answers once released, and counts the calls that reached it.
#[derive(Debug, Clone, Default)]
struct Gate {
    open: Arc<tokio::sync::Notify>,
    calls: Arc<std::sync::Mutex<u32>>,
}

impl ToolDispatcher for Gate {
    async fn dispatch(&self, _call: ToolCall, _auth: CallAuth) -> Dispatched {
        *self.calls.lock().unwrap() += 1;
        self.open.notified().await;
        Dispatched::Result(json!("late"))
    }
}

#[tokio::test]
async fn the_engine_heartbeats_its_claims_until_their_execution_timeout() {
    use rewind_engine::adapters::{
        MemoryBlobs, MemoryJournal, MemorySecrets, MemoryWebhooks, SeededEntropy, VirtualClock,
    };
    use rewind_engine::{Adapters, Engine};

    let gate = Gate::default();
    let engine = Engine::new(
        Adapters {
            journal: MemoryJournal::new(),
            blobs: MemoryBlobs::new(),
            executor: caller(),
            dispatcher: gate.clone(),
            clock: VirtualClock::new(T0),
            entropy: SeededEntropy::new(42),
            secrets: MemorySecrets::new(),
            webhooks: MemoryWebhooks::new(),
        },
        EngineConfig::default(),
    );
    let slow = toolset(|policy| policy.execution_timeout = Some(Duration::from_secs(90)));
    let run = engine.start_run(calling(ISSUE, slow)).await.unwrap().run;
    engine.activate_next().await.unwrap();
    let claimed = engine.claim_tool_tasks(10).await.unwrap().remove(0);
    let at = |s: u64| T0.saturating_add(Duration::from_secs(s));

    let dispatching = engine.run_tool_task(claimed);
    let watching = async {
        tokio::task::yield_now().await;
        assert_eq!(*gate.calls.lock().unwrap(), 1, "the call is in flight");
        engine.clock().advance_to(at(10));
        assert_eq!(engine.heartbeat_tool_claims().await.unwrap(), 0, "not due");
        engine.clock().advance_to(at(30));
        assert_eq!(engine.heartbeat_tool_claims().await.unwrap(), 1);
        engine.clock().advance_to(at(70));
        assert!(
            engine.claim_tool_tasks(10).await.unwrap().is_empty(),
            "the heartbeat kept the claim alive past its first 60 s"
        );
        assert_eq!(engine.heartbeat_tool_claims().await.unwrap(), 1);
        engine.clock().advance_to(at(95));
        assert_eq!(
            engine.heartbeat_tool_claims().await.unwrap(),
            0,
            "past its execution timeout, the claim is no longer renewed"
        );
        gate.open.notify_one();
    };
    let (dispatched, ()) = tokio::join!(dispatching, watching);
    dispatched.unwrap();
    engine.activate_next().await.unwrap();
    let history = engine.history(&run).await.unwrap();
    assert_eq!(
        history.last().unwrap().event,
        Event::Command(rewind_core::Command::CompleteRun {
            output: json!({ "result": "late" }).into()
        })
    );
}

// -------------------------------------------------------------------------------------------
// Refused calls and parked tasks
//
// How a call's secret is read, a missing secret parks it, and a secret put again resumes it:
// `tests/secrets.rs`.

/// Answers 401 to the first attempt of each call, then answers.
fn refusing_once() -> RecordingDispatcher {
    RecordingDispatcher::dispatching(|call, _| {
        if call.attempt > 1 {
            Dispatched::Result(json!({ "attempt": call.attempt, "key": call.idempotency_key }))
        } else {
            Dispatched::Park(ParkReason {
                kind: ParkKind::CredentialsRefused,
                message: format!("{} answered 401", call.tool.name),
            })
        }
    })
}

#[tokio::test]
async fn a_401_parks_the_call_until_the_harness_approves_it() {
    let dispatcher = refusing_once();
    let engine = engine(caller(), dispatcher.clone());
    let mut request = calling(ISSUE, toolset(|_| {}));
    request.metadata.labels = BTreeMap::from([("agent".to_owned(), "triage".to_owned())]);
    let run = engine.start_run(request).await.unwrap().run;
    subscribe(&engine, BTreeMap::new()).await;
    subscribe(
        &engine,
        BTreeMap::from([("agent".to_owned(), "other".to_owned())]),
    )
    .await;
    drive(&engine).await;

    assert!(matches!(
        task_state(&engine, &run).await,
        TaskState::Parked(ParkReason {
            kind: ParkKind::CredentialsRefused,
            ..
        })
    ));
    assert_eq!(status(&engine, &run).await, RunStatus::Active);
    let deliveries = engine.webhooks().deliveries();
    assert_eq!(
        deliveries.len(),
        1,
        "only the subscription whose labels match"
    );
    let (subscription, event) = &deliveries[0];
    assert_eq!(subscription.id, "sub-0");
    assert_eq!(event.id, format!("{}/1/1", run.id));
    assert_eq!(event.labels["agent"], "triage");
    assert_eq!(
        event.kind,
        WebhookEventKind::CredentialsExpired {
            seq: Seq(1),
            tool: ISSUE.into(),
            message: "github.get_issue answered 401".into(),
            reason: CredentialsExpiredReason::Refused,
            secret: None,
        }
    );

    // Approved once the server accepts the key again, the call is sent with the same key.
    engine
        .approve_task(&TaskRef {
            run: run.clone(),
            seq: Seq(1),
        })
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(
        output(&engine, &run).await,
        json!({ "result": { "attempt": 2, "key": format!("{}/1", run.id) } })
    );
    assert_eq!(
        dispatcher.auth(),
        [CallAuth::None, CallAuth::None],
        "a server without auth gets nothing"
    );
}

/// Parks a call whose schema changed, unless the harness approved the change.
fn schema_checking() -> RecordingDispatcher {
    RecordingDispatcher::dispatching(|call, _| {
        if call.schema_change_approved {
            Dispatched::Result(json!({ "against": "the new schema" }))
        } else {
            Dispatched::Park(ParkReason {
                kind: ParkKind::SchemaChanged,
                message: "`repo` became `repository`".into(),
            })
        }
    })
}

#[tokio::test]
async fn a_schema_change_parks_the_call_until_the_harness_decides() {
    let dispatcher = schema_checking();
    let engine = engine(caller(), dispatcher.clone());
    subscribe(&engine, BTreeMap::new()).await;
    let approved = engine
        .start_run(calling(ISSUE, toolset(|_| {})))
        .await
        .unwrap()
        .run;
    let rejected = engine
        .start_run(calling(ISSUE, toolset(|_| {})))
        .await
        .unwrap()
        .run;
    drive(&engine).await;

    let kinds: Vec<_> = engine
        .webhooks()
        .deliveries()
        .into_iter()
        .map(|(_, event)| event.kind)
        .collect();
    let changed = WebhookEventKind::ToolSchemaChanged {
        seq: Seq(1),
        tool: ISSUE.into(),
        message: "`repo` became `repository`".into(),
    };
    assert_eq!(kinds, [changed.clone(), changed]);

    engine.approve_task(&task(&approved, 1)).await.unwrap();
    engine
        .reject_task(&task(&rejected, 1), Some("fix the script".into()))
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(
        output(&engine, &approved).await,
        json!({ "result": { "against": "the new schema" } })
    );
    assert_eq!(
        output(&engine, &rejected).await,
        json!({ "error": "error", "message": "fix the script" })
    );
    assert_eq!(dispatcher.calls().len(), 3);
}

#[tokio::test]
async fn the_approval_policy_waits_for_the_harness() {
    let dispatcher = RecordingDispatcher::echo();
    let engine = engine(caller(), dispatcher.clone());
    let gated = toolset(|policy| policy.approval = true);
    let approved = engine
        .start_run(calling(ISSUE, gated.clone()))
        .await
        .unwrap()
        .run;
    let rejected = engine.start_run(calling(ISSUE, gated)).await.unwrap().run;
    drive(&engine).await;
    assert!(
        dispatcher.calls().is_empty(),
        "nothing is sent before approval"
    );
    assert_eq!(task_state(&engine, &approved).await, TaskState::Approval);

    engine.approve_task(&task(&approved, 1)).await.unwrap();
    assert_eq!(
        engine.approve_task(&task(&approved, 1)).await,
        Err(EngineError::TaskNotWaiting {
            task: format!("{}/1", approved.id),
            state: "ready",
        })
    );
    engine.reject_task(&task(&rejected, 1), None).await.unwrap();
    assert_eq!(
        engine.reject_task(&task(&rejected, 1), None).await,
        Err(EngineError::TaskGone)
    );
    drive(&engine).await;

    assert_eq!(output(&engine, &approved).await, json!({ "result": {} }));
    assert_eq!(
        output(&engine, &rejected).await,
        json!({ "error": "error", "message": "rejected by the harness" })
    );
    assert_eq!(dispatcher.calls().len(), 1);
}

// -------------------------------------------------------------------------------------------
// Host tools and their worker

#[tokio::test]
async fn a_host_worker_claims_heartbeats_and_completes() {
    let dispatcher = RecordingDispatcher::echo();
    let engine = engine_with(caller(), dispatcher.clone(), worker_config());
    let mut request = calling("ask_user", toolset(|_| {}));
    request.input["args"] = json!({ "question": "ship it?" });
    request.metadata.labels = BTreeMap::from([("conversation".to_owned(), "c-42".to_owned())]);
    let run = engine.start_run(request).await.unwrap().run;
    drive(&engine).await;
    assert!(
        dispatcher.calls().is_empty(),
        "host tools are not dispatched"
    );

    let tenant = TenantId::local();
    let mut claimed = engine.claim_host_tasks(&host_claim(&tenant)).await.unwrap();
    assert_eq!(claimed.len(), 1);
    let host = claimed.remove(0);
    assert_eq!(host.task, task(&run, 1));
    assert_eq!(host.tool, "ask_user");
    assert_eq!(host.args, json!({ "question": "ship it?" }));
    assert_eq!(host.idempotency_key, format!("{}/1", run.id));
    assert_eq!(host.attempt, 1);
    assert_eq!(host.heartbeat_timeout, Duration::from_secs(60));
    assert_eq!(host.labels["conversation"], "c-42");
    assert!(
        engine
            .claim_host_tasks(&host_claim(&tenant))
            .await
            .unwrap()
            .is_empty()
    );

    // Another tenant's worker cannot use the token.
    let intruder = TenantId::new("intruder").unwrap();
    assert_eq!(
        engine.heartbeat_task(&intruder, &host.token).await,
        Err(EngineError::TaskGone)
    );
    assert_eq!(
        engine
            .complete_task(&intruder, &host.token, json!("no"))
            .await,
        Err(EngineError::TaskGone)
    );

    let later = T0.saturating_add(Duration::from_secs(50));
    engine.clock().advance_to(later);
    assert_eq!(
        engine.heartbeat_task(&tenant, &host.token).await.unwrap(),
        later.saturating_add(Duration::from_secs(60))
    );
    engine
        .complete_task(&tenant, &host.token, json!("yes"))
        .await
        .unwrap();
    assert_eq!(
        engine
            .complete_task(&tenant, &host.token, json!("again"))
            .await,
        Err(EngineError::TaskGone),
        "answered once"
    );
    drive(&engine).await;
    assert_eq!(output(&engine, &run).await, json!({ "result": "yes" }));
}

#[tokio::test]
async fn a_host_tool_answers_later_through_its_token() {
    let engine = engine_with(caller(), RecordingDispatcher::echo(), worker_config());
    let mut request = calling("ask_user", toolset(|_| {}));
    request.ttl = Some(Duration::from_secs(7 * 86_400));
    let answered = engine.start_run(request.clone()).await.unwrap().run;
    let late = engine.start_run(request).await.unwrap().run;
    drive(&engine).await;
    let tenant = TenantId::local();
    let claimed = engine.claim_host_tasks(&host_claim(&tenant)).await.unwrap();
    let of = |run: &RunRef| claimed.iter().find(|task| task.task.run == *run).unwrap();
    let (first, second) = (of(&answered), of(&late));

    // Without a timeout, the deferral lasts until the run's deadline; with one, less.
    let deadline = T0.saturating_add(Duration::from_secs(7 * 86_400));
    assert_eq!(
        engine
            .defer_task(&tenant, &first.token, None)
            .await
            .unwrap(),
        deadline
    );
    let hour = T0.saturating_add(Duration::from_secs(3_600));
    assert_eq!(
        engine
            .defer_task(&tenant, &second.token, Some(Duration::from_secs(3_600)))
            .await
            .unwrap(),
        hour
    );
    assert_eq!(
        engine
            .defer_task(&tenant, &second.token, None)
            .await
            .unwrap(),
        hour,
        "deferring twice changes nothing"
    );
    assert_eq!(
        engine.heartbeat_task(&tenant, &first.token).await.unwrap(),
        deadline,
        "a deferred task needs no heartbeat"
    );

    // Two days later, the first answer arrives; the second never does.
    engine
        .clock()
        .advance_to(T0.saturating_add(Duration::from_secs(2 * 86_400)));
    engine
        .complete_task(&tenant, &first.token, json!("approved"))
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(
        output(&engine, &answered).await,
        json!({ "result": "approved" })
    );
    assert_eq!(
        output(&engine, &late).await,
        json!({ "error": "timeout", "message": "the tool did not answer before its deadline" })
    );
}

#[tokio::test]
async fn a_retryable_host_failure_is_retried_after_a_backoff() {
    let engine = engine_with(caller(), RecordingDispatcher::echo(), worker_config());
    let run = engine
        .start_run(calling("ask_user", toolset(|_| {})))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    let tenant = TenantId::local();
    let first = engine
        .claim_host_tasks(&host_claim(&tenant))
        .await
        .unwrap()
        .remove(0);
    engine
        .fail_task(
            &tenant,
            &first.token,
            HostFailure {
                message: "the user is away".into(),
                data: None,
                retryable: true,
            },
        )
        .await
        .unwrap();
    let TaskState::Ready { next_attempt_at } = task_state(&engine, &run).await else {
        panic!("the task waits for its next attempt");
    };
    let delay = Backoff::default().delay(&first.idempotency_key, 1);
    assert_eq!(next_attempt_at, T0.saturating_add(delay));
    assert_eq!(
        engine.next_tool_wake_up().await.unwrap(),
        Some(next_attempt_at)
    );
    assert!(
        engine
            .claim_host_tasks(&host_claim(&tenant))
            .await
            .unwrap()
            .is_empty()
    );

    engine.clock().advance_to(next_attempt_at);
    let second = engine
        .claim_host_tasks(&host_claim(&tenant))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(second.attempt, 2);
    assert_eq!(second.idempotency_key, first.idempotency_key);
    engine
        .fail_task(
            &tenant,
            &second.token,
            HostFailure {
                message: "no such user".into(),
                data: Some(json!({ "user": 7 })),
                retryable: false,
            },
        )
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(
        output(&engine, &run).await,
        json!({ "error": "error", "message": "no such user" })
    );
}

#[tokio::test]
async fn an_interrupted_host_call_is_handed_out_again_unless_at_most_once() {
    let engine = engine_with(caller(), RecordingDispatcher::echo(), worker_config());
    let retried = engine
        .start_run(calling("ask_user", toolset(|_| {})))
        .await
        .unwrap()
        .run;
    let at_most_once = toolset(|policy| policy.on_interruption = Interruption::AtMostOnce);
    let once = engine
        .start_run(calling("ask_user", at_most_once))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    let tenant = TenantId::local();
    assert_eq!(
        engine
            .claim_host_tasks(&host_claim(&tenant))
            .await
            .unwrap()
            .len(),
        2
    );

    // The worker dies: no heartbeat comes.
    engine
        .clock()
        .advance_to(T0.saturating_add(Duration::from_secs(60)));
    let again = engine.claim_host_tasks(&host_claim(&tenant)).await.unwrap();
    assert_eq!(
        again.len(),
        1,
        "the at_most_once call is not handed out again"
    );
    assert_eq!(again[0].task.run, retried);
    assert_eq!(again[0].attempt, 2);
    drive(&engine).await;
    assert_eq!(
        output(&engine, &once).await["error"],
        json!("outcome_unknown")
    );
}

#[tokio::test]
async fn an_interrupted_at_most_once_host_call_fails_even_when_no_worker_polls_again() {
    let engine = engine_with(caller(), RecordingDispatcher::echo(), worker_config());
    let retried = engine
        .start_run(calling("ask_user", toolset(|_| {})))
        .await
        .unwrap()
        .run;
    let at_most_once = toolset(|policy| policy.on_interruption = Interruption::AtMostOnce);
    let once = engine
        .start_run(calling("ask_user", at_most_once))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    let tenant = TenantId::local();
    let claimed = engine.claim_host_tasks(&host_claim(&tenant)).await.unwrap();
    assert_eq!(claimed.len(), 2);
    assert_eq!(
        engine.fail_interrupted_host_tasks().await.unwrap(),
        0,
        "the claims are alive"
    );

    // The harness's worker dies, and none polls again.
    engine
        .clock()
        .advance_to(T0.saturating_add(Duration::from_secs(60)));
    assert_eq!(engine.fail_interrupted_host_tasks().await.unwrap(), 1);
    drive(&engine).await;
    assert_eq!(
        output(&engine, &once).await["error"],
        json!("outcome_unknown")
    );
    assert!(
        matches!(
            task_state(&engine, &retried).await,
            TaskState::Claimed { .. }
        ),
        "a retry call waits for a worker to claim it again"
    );
    let again = engine.claim_host_tasks(&host_claim(&tenant)).await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].task.run, retried);
    assert_eq!(again[0].attempt, 2);
}

#[tokio::test]
async fn a_result_above_the_payload_limit_is_refused() {
    let big = json!("x".repeat(400));
    let answer = big.clone();
    let dispatcher = RecordingDispatcher::new(move |_| Ok(answer.clone()));
    let mut config = worker_config();
    config.limits.max_payload_bytes = 300;
    let engine = engine_with(caller(), dispatcher, config);
    let dispatched = engine
        .start_run(calling(ISSUE, toolset(|_| {})))
        .await
        .unwrap()
        .run;
    let host = engine
        .start_run(calling("ask_user", toolset(|_| {})))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    assert_eq!(
        output(&engine, &dispatched).await,
        json!({
            "error": "error",
            "message": "the result is 402 bytes as canonical JSON, above the limit of 300 bytes",
        })
    );

    let tenant = TenantId::local();
    let claimed = engine
        .claim_host_tasks(&host_claim(&tenant))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(
        engine.complete_task(&tenant, &claimed.token, big).await,
        Err(EngineError::ResultTooLarge {
            size: 402,
            max: 300
        })
    );
    engine
        .complete_task(&tenant, &claimed.token, json!("short"))
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(output(&engine, &host).await, json!({ "result": "short" }));
}

// -------------------------------------------------------------------------------------------
// The webhook built-in

#[tokio::test]
async fn the_webhook_builtin_returns_a_url_bound_to_the_run_and_the_name() {
    let dispatcher = RecordingDispatcher::new(|_| Ok(json!({ "url": "mock://hook" })));
    let config = EngineConfig {
        public_url: Some("https://rewind.example.com/".into()),
        ..EngineConfig::default()
    };
    let engine = engine_with(caller(), dispatcher.clone(), config);
    let mut request = calling("webhook", toolset(|_| {}));
    request.input["args"] = json!({ "name": "pr_merged" });
    let live = engine.start_run(request.clone()).await.unwrap().run;
    request.mode = RunMode::Mock;
    let mock = engine.start_run(request).await.unwrap().run;
    engine.activate_next().await.unwrap();
    engine.activate_next().await.unwrap();
    assert_eq!(engine.dispatch_tool_calls(10).await.unwrap(), 2);

    // While the run lives, its token resolves to it and the signal name; asked again for the
    // same name, the store gives the same token.
    let token = engine
        .journal()
        .create_webhook_token(&live, "pr_merged")
        .await
        .unwrap();
    let target = engine
        .journal()
        .resolve_webhook_token(&token)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((&target.run, target.name.as_str()), (&live, "pr_merged"));
    drive(&engine).await;
    assert_eq!(
        output(&engine, &live).await,
        json!({ "result": { "url": format!("https://rewind.example.com/hooks/{}", token.0) } })
    );
    assert_eq!(
        output(&engine, &mock).await,
        json!({ "result": { "url": "mock://hook" } }),
        "in mock mode, the mocks answer"
    );
    assert_eq!(dispatcher.calls().len(), 1);
}

#[tokio::test]
async fn without_a_public_url_the_webhook_builtin_is_unavailable() {
    let engine = engine(caller(), RecordingDispatcher::echo());
    let mut request = calling("webhook", toolset(|_| {}));
    request.input["args"] = json!({ "name": "pr_merged" });
    let run = engine.start_run(request).await.unwrap().run;
    drive(&engine).await;
    assert_eq!(
        output(&engine, &run).await,
        json!({
            "error": "unavailable",
            "message": "this server has no public URL, so it cannot receive webhooks",
        })
    );
}

#[tokio::test]
async fn a_webhook_name_with_a_nul_character_is_refused() {
    let config = EngineConfig {
        public_url: Some("https://rewind.example.com/".into()),
        ..EngineConfig::default()
    };
    let engine = engine_with(caller(), RecordingDispatcher::echo(), config);
    let mut request = calling("webhook", toolset(|_| {}));
    request.input["args"] = json!({ "name": "pr\u{0}merged" });
    let run = engine.start_run(request).await.unwrap().run;
    drive(&engine).await;
    assert_eq!(
        output(&engine, &run).await,
        json!({
            "error": "error",
            "message": "the webhook name contains a NUL character (U+0000), which cannot be \
                        stored: remove it",
        })
    );
}

// -------------------------------------------------------------------------------------------
// Tenants

#[tokio::test]
async fn a_start_above_the_tenants_quota_is_refused() {
    let engine = engine_with(caller(), RecordingDispatcher::echo(), worker_config());
    let acme = TenantId::new("acme").unwrap();
    engine
        .journal()
        .put_tenant(&Tenant {
            quotas: rewind_engine::ports::Quotas {
                active_runs: Some(1),
                ..Default::default()
            },
            ..Tenant::new(acme.clone())
        })
        .await
        .unwrap();
    let request = || {
        let mut request = calling("ask_user", toolset(|_| {}));
        request.tenant = acme.clone();
        request
    };
    let first = engine.start_run(request()).await.unwrap().run;

    assert_eq!(
        engine.start_run(request()).await,
        Err(StartError::QuotaExceeded(
            rewind_engine::ports::QuotaExceeded {
                quota: rewind_engine::ports::QuotaKind::ActiveRuns,
                scope: rewind_engine::ports::QuotaScope::Tenant,
                limit: 1,
                used: 1,
                resets_at: None,
            }
        ))
    );
    assert_eq!(
        engine
            .list_runs(&rewind_engine::ports::RunFilter::new(acme.clone()))
            .await
            .unwrap()
            .len(),
        1
    );

    // Once the first run has ended, another can start.
    drive(&engine).await;
    let claimed = engine
        .claim_host_tasks(&host_claim(&acme))
        .await
        .unwrap()
        .remove(0);
    assert_eq!(claimed.task.run, first);
    engine
        .complete_task(&acme, &claimed.token, json!("done"))
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(status(&engine, &first).await, RunStatus::Completed);
    engine.start_run(request()).await.unwrap();
}

// -------------------------------------------------------------------------------------------
// Concurrency

/// Calls the tool named in its input three times at once, and waits.
fn three_calls() -> FakeExecutor {
    FakeExecutor::new().with(
        "three_calls",
        |run| {
            let tool = run.input["tool"].as_str().unwrap().to_owned();
            for n in 0..3 {
                run.call(&tool, json!({ "n": n }));
            }
        },
        |_, _| {},
    )
}

#[tokio::test]
async fn claims_respect_the_tools_and_the_servers_concurrency() {
    let engine = engine(three_calls(), RecordingDispatcher::echo());
    let serialized = toolset(|policy| policy.concurrency = 1);
    let mut request = calling(ISSUE, serialized);
    request.script = Script::new("three_calls");
    engine.start_run(request).await.unwrap();

    // Another server, which takes two calls at once from each worker.
    let server = McpServer {
        concurrency: 2,
        ..McpServer::new(
            "gitlab",
            McpTransport::Http {
                url: "https://mcp.example.com/gitlab".into(),
            },
        )
    };
    let issue = Tool::new(
        "gitlab.get_issue",
        ToolRoute::Mcp {
            server: "gitlab".into(),
        },
        json!({}),
    );
    let mut request = calling("gitlab.get_issue", Toolset::new([server], [issue]).unwrap());
    request.script = Script::new("three_calls");
    engine.start_run(request).await.unwrap();
    engine.activate_next().await.unwrap();
    engine.activate_next().await.unwrap();

    // One call of the serialized tool, two calls to the server of the other run.
    let claimed = engine.claim_tool_tasks(10).await.unwrap();
    assert_eq!(claimed.len(), 3, "{claimed:?}");
    assert!(engine.claim_tool_tasks(10).await.unwrap().is_empty());

    // Answered calls free their slots: the serialized tool goes one call at a time.
    let mut claimed = claimed;
    for expected in [2, 1, 0] {
        for task in claimed {
            engine.run_tool_task(task).await.unwrap();
        }
        claimed = engine.claim_tool_tasks(10).await.unwrap();
        assert_eq!(claimed.len(), expected);
    }
}
