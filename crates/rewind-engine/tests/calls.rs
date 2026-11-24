//! What the call executor and the owner make of a call, in the driver (`archi/04-tools.md`,
//! "Retries", "Harness actions", "Webhooks"): the wait a server asks for before a retry, a result
//! above the payload limit, and the `webhook` built-in.

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{
    Action, ActionFailure, ActionFailureKind, ActionRoute, ActionSet, Builtin, Code, Command,
    Completion, Event, JsonValue, McpServer, McpTransport, RunMode,
};
use rewind_engine::StartRun;
use rewind_engine::edge::EdgeError;
use rewind_engine::plane::{HarnessActions, NodeConfig};
use rewind_engine::ports::link::{Refusal, StreamAttach, StreamId};
use rewind_engine::ports::{Delivery, Dispatched, RunRef, TenantId};
use serde_json::json;
use support::plane::{World, config, handed};
use support::{FakeExecutor, RecordingDispatcher, T0};

const ISSUE: &str = "github.get_issue";
const PUBLIC_URL: &str = "https://rewind.example.com/";

/// Calls the action named in its input (`{"action": …, "args": …}`), and completes with what came
/// back: `{"result": …}`, or `{"error": kind, "message": …}`.
fn caller() -> FakeExecutor {
    FakeExecutor::new().with(
        "caller",
        |run| {
            let action = run.input["action"].as_str().unwrap().to_owned();
            let args = run.input.get("args").cloned().unwrap_or(json!({}));
            run.call(&action, args);
        },
        |run, delivery| match delivery {
            Delivery::CallCompleted { result, .. } => {
                run.complete(json!({ "result": result }));
            }
            Delivery::CallFailed { error, .. } => run.complete(json!({
                "error": error.kind.as_str(),
                "message": error.message,
            })),
            other => panic!("unexpected {other:?}"),
        },
    )
}

/// An MCP server `github` with the action `github.get_issue`, the harness action `ask_user` and the
/// `webhook` built-in.
fn action_set() -> ActionSet {
    let server = McpServer::new(
        "github",
        McpTransport::Http {
            url: "https://mcp.example.com/github".into(),
        },
    );
    let issue = Action::new(
        ISSUE,
        ActionRoute::Mcp {
            server: "github".into(),
        },
        json!({}),
    );
    let ask = Action::new("ask_user", ActionRoute::Harness { group: None }, json!({}));
    let webhook = Action::new("webhook", ActionRoute::Builtin(Builtin::Webhook), json!({}));
    ActionSet::new([server], [issue, ask, webhook]).unwrap()
}

fn calling(action: &str, args: JsonValue) -> StartRun {
    StartRun::new(
        Code::new("caller"),
        action_set(),
        json!({ "action": action, "args": args }),
    )
}

/// A node that receives webhooks at [`PUBLIC_URL`].
fn public() -> NodeConfig {
    let mut node = config("node-a", 1);
    node.public_url = Some(PUBLIC_URL.into());
    node
}

async fn output(world: &mut World, run: &RunRef) -> JsonValue {
    let history = world.history(run).await;
    match &history.last().unwrap().event {
        Event::Command(Command::CompleteRun { output }) => output.value().unwrap().clone(),
        other => panic!("the run has not completed: {other:?}"),
    }
}

#[tokio::test]
async fn a_transient_failure_is_retried_no_sooner_than_its_retry_after() {
    let dispatcher = RecordingDispatcher::dispatching(|call, _| {
        if call.delivery < 2 {
            Dispatched::Transient(ActionFailure {
                kind: ActionFailureKind::Unavailable,
                message: "rate limited".into(),
                data: Some(json!({ "status": 429, "retryAfter": 3600 })),
            })
        } else {
            Dispatched::Result(json!({ "number": 4 }))
        }
    });
    let node = config("node-a", 1);
    let backoff = node.backoff;
    let mut world = World::with(caller(), dispatcher, 1, &[node]).await;
    let run = world.run(calling(ISSUE, json!({}))).await;

    assert_eq!(
        output(&mut world, &run).await,
        json!({ "result": { "number": 4 } })
    );
    let waited = backoff
        .delay(&format!("{}/1", run.id), 1)
        .max(Duration::from_secs(3600));
    let history = world.history(&run).await;
    let completed = history
        .iter()
        .find(|entry| {
            matches!(
                entry.event,
                Event::Completion(Completion::CallCompleted { .. })
            )
        })
        .unwrap();
    assert_eq!(
        completed.recorded_at,
        T0.saturating_add(waited),
        "the API asked for an hour, longer than the backoff"
    );
}

#[tokio::test]
async fn a_result_above_the_payload_limit_is_refused() {
    let big = json!("x".repeat(400));
    let answer = big.clone();
    let mut node = config("node-a", 1);
    node.limits.max_payload_bytes = 300;
    node.harness_actions = HarnessActions::Streams;
    let mut world = World::with(
        caller(),
        RecordingDispatcher::new(move |_| Ok(answer.clone())),
        1,
        &[node],
    )
    .await;
    let dispatched = world.run(calling(ISSUE, json!({}))).await;
    assert_eq!(
        output(&mut world, &dispatched).await,
        json!({
            "error": "error",
            "message": "the result is 402 bytes as canonical JSON, above the limit of 300 bytes",
        })
    );

    // A harness task's answer above the limit is refused; the task still waits for one.
    let at = world.at.clone();
    let mut stream = world
        .driver
        .attach_stream(
            &at,
            TenantId::local(),
            StreamAttach {
                stream: StreamId("s1".into()),
                worker: "w1".into(),
                node: at.clone(),
                namespace: None,
                labels: BTreeMap::new(),
                action_set: None,
                actions: Default::default(),
                slots: 1,
            },
        )
        .unwrap();
    let host = world.run(calling("ask_user", json!({}))).await;
    let task = handed(&mut stream).remove(0);
    assert!(matches!(
        world.complete(&task.token, big).await,
        Err(EdgeError::Refused(Refusal::ResultTooLarge {
            size: 402,
            max: 300
        }))
    ));
    world.complete(&task.token, json!("short")).await.unwrap();
    world.idle().await;
    assert_eq!(
        output(&mut world, &host).await,
        json!({ "result": "short" })
    );
}

/// Makes the webhook `pr_merged`, waits for its signal, and completes with it.
fn hooked() -> FakeExecutor {
    caller().with(
        "hooked",
        |run| {
            run.call("webhook", json!({ "name": "pr_merged" }));
        },
        |run, delivery| match delivery {
            Delivery::CallCompleted { .. } => run.await_signal("pr_merged"),
            Delivery::SignalReceived { name, payload } => {
                run.complete(json!({ "name": name, "payload": payload }));
            }
            other => panic!("unexpected {other:?}"),
        },
    )
}

/// The result of the run's first call.
async fn first_result(world: &mut World, run: &RunRef) -> JsonValue {
    world
        .history(run)
        .await
        .iter()
        .find_map(|entry| match &entry.event {
            Event::Completion(Completion::CallCompleted { result, .. }) => result.value().cloned(),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn the_webhook_builtin_returns_a_url_bound_to_the_run_and_the_name() {
    let dispatcher = RecordingDispatcher::new(|_| Ok(json!({ "url": "mock://hook" })));
    let mut world = World::with(hooked(), dispatcher, 1, &[public()]).await;
    let request = StartRun::new(Code::new("hooked"), action_set(), json!({}));
    let live = world.run(request.clone()).await;
    let mut mock = request;
    mock.mode = RunMode::Test;
    let mock = world.run(mock).await;

    let made = first_result(&mut world, &live).await;
    let url = made["url"].as_str().unwrap().to_owned();
    let token = url
        .strip_prefix("https://rewind.example.com/hooks/")
        .unwrap_or_else(|| panic!("a URL under the public URL: {url}"));
    // A request posted to it is the signal of the name it was made for, to its run.
    let mut gateway = world.driver.gateway(&world.at);
    world
        .edge
        .webhook(&mut gateway, token, json!({ "merged": true }), None)
        .await
        .unwrap();
    world.idle().await;
    assert_eq!(
        output(&mut world, &live).await,
        json!({ "name": "pr_merged", "payload": { "merged": true } })
    );
    assert_eq!(
        first_result(&mut world, &mock).await,
        json!({ "url": "mock://hook" }),
        "in a test run, the handlers answer"
    );
    assert_eq!(world.plane.dispatcher.calls().len(), 1);
}

#[tokio::test]
async fn without_a_public_url_the_webhook_builtin_is_unavailable() {
    let mut world = World::new(caller(), RecordingDispatcher::echo()).await;
    let run = world
        .run(calling("webhook", json!({ "name": "pr_merged" })))
        .await;
    assert_eq!(
        output(&mut world, &run).await,
        json!({
            "error": "unavailable",
            "message": "this server has no public URL, so it cannot receive webhooks",
        })
    );
}

#[tokio::test]
async fn a_webhook_name_with_a_nul_character_is_refused() {
    let mut world = World::with(caller(), RecordingDispatcher::echo(), 1, &[public()]).await;
    let run = world
        .run(calling("webhook", json!({ "name": "pr\u{0}merged" })))
        .await;
    assert_eq!(
        output(&mut world, &run).await,
        json!({
            "error": "error",
            "message": "the webhook name contains a NUL character (U+0000), which cannot be \
                        stored: remove it",
        })
    );
}
