//! Forks: a new run that reuses its parent's recorded results while its commands match, then
//! goes live (`archi/03-execution.md`, "Forks").

#![allow(clippy::unwrap_used)]

mod support;

use std::time::Duration;

use ptc_core::{
    Completion, Event, HistoryEntry, Interruption, Lifecycle, RunStatus, Script, Seq, Timestamp,
    Tool, ToolFailure, ToolFailureKind, ToolRoute, Toolset,
};
use ptc_engine::ports::{Delivery, ForkParent, RunRef, VirtualTime};
use ptc_engine::{DriveOptions, ForkError, ForkPoint, ForkRun, ReplayVerdict, StartError};
use serde_json::json;
use support::*;

const HOUR_LATER: Timestamp = Timestamp(T0.0 + HOUR);

async fn drive(engine: &TestEngine) {
    let report = engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    assert!(!report.interrupted);
    assert!(report.failures.is_empty(), "{report:?}");
}

fn events(history: &[HistoryEntry]) -> Vec<&'static str> {
    history.iter().map(|entry| entry.event.name()).collect()
}

async fn status(engine: &TestEngine, run: &RunRef) -> RunStatus {
    engine.get_run(run).await.unwrap().unwrap().status
}

/// `a` answers with its args; `b` refuses "bad".
fn dispatcher() -> RecordingDispatcher {
    RecordingDispatcher::new(|call| {
        if call.args == json!("bad") {
            return Err(ToolFailure {
                kind: ToolFailureKind::Error,
                message: "bad argument".into(),
                data: None,
            });
        }
        Ok(call.args.clone())
    })
}

/// Calls `a`, then `b` with `arg`; completes with `b`'s result, fails if it failed.
fn two_steps(executor: FakeExecutor, name: &str, arg: &'static str) -> FakeExecutor {
    executor.with(
        name,
        |run| {
            run.call("a", json!(1));
        },
        move |run, delivery| match delivery {
            Delivery::ToolCallCompleted { seq: Seq(1), .. } => {
                run.call("b", json!(arg));
            }
            Delivery::ToolCallCompleted { result, .. } => run.complete(result.clone()),
            _ => run.fail("ToolError", "b failed"),
        },
    )
}

fn programs() -> FakeExecutor {
    let programs = two_steps(FakeExecutor::new(), "buggy", "bad");
    two_steps(programs, "fixed", "good")
}

async fn failed_parent(engine: &TestEngine) -> RunRef {
    let parent = engine
        .start_run(request("buggy", json!({ "n": 1 })))
        .await
        .unwrap()
        .run;
    drive(engine).await;
    assert_eq!(status(engine, &parent).await, RunStatus::Failed);
    engine.clock().advance_to(HOUR_LATER);
    parent
}

#[tokio::test]
async fn a_fork_reuses_the_recorded_calls_and_goes_live_where_it_differs() {
    let dispatcher = dispatcher();
    let engine = engine(programs(), dispatcher.clone());
    let parent = failed_parent(&engine).await;
    let parent_history = engine.history(&parent).await.unwrap();

    let fork = engine
        .fork(ForkRun::new(parent.clone(), Script::new("fixed")))
        .await
        .unwrap();
    assert!(fork.created);
    drive(&engine).await;
    let fork = fork.run;
    assert_eq!(status(&engine, &fork).await, RunStatus::Completed);

    let history = engine.history(&fork).await.unwrap();
    assert_eq!(
        events(&history),
        [
            "run_started",
            "schedule_tool_call",
            "tool_call_completed",
            "schedule_tool_call",
            "tool_call_completed",
            "complete_run"
        ]
    );
    // The parent's start and its result are copied with their time; what is live is later.
    assert_eq!(history[0].recorded_at, parent_history[0].recorded_at);
    assert_eq!(history[2].event, parent_history[2].event);
    assert_eq!(history[2].recorded_at, parent_history[2].recorded_at);
    assert_eq!(history[4].recorded_at, HOUR_LATER);
    let Event::Lifecycle(Lifecycle::RunStarted(started)) = &history[0].event else {
        unreachable!()
    };
    let Event::Lifecycle(Lifecycle::RunStarted(parent_started)) = &parent_history[0].event else {
        unreachable!()
    };
    assert_eq!(started.seed, parent_started.seed);
    assert_eq!(started.input, parent_started.input);
    assert_eq!(started.script, Script::new("fixed").digest());
    assert!(started.expires_at > parent_started.expires_at, "a new TTL");

    // `a` was called once, by the parent; `b` twice, the fork's call with the fork's key.
    let calls = dispatcher.calls();
    let tools: Vec<&str> = calls.iter().map(|call| call.tool.name.as_str()).collect();
    assert_eq!(tools, ["a", "b", "b"]);
    assert_eq!(calls[2].idempotency_key, format!("{}/2", fork.id));
    assert_eq!(calls[2].args, json!("good"));

    let record = engine.get_run(&fork).await.unwrap().unwrap();
    assert_eq!(
        record.metadata.parent,
        Some(ForkParent {
            run: parent.id,
            at: None
        })
    );
    // Self-contained: it replays without its parent.
    assert!(matches!(
        engine.replay(&fork, None).await.unwrap(),
        ReplayVerdict::Matches(_)
    ));
}

#[tokio::test]
async fn at_forces_the_fork_live_although_the_commands_match() {
    let dispatcher = dispatcher();
    let programs = two_steps(FakeExecutor::new(), "good", "good");
    let engine = engine(programs, dispatcher.clone());
    let parent = engine
        .start_run(request("good", json!({})))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    engine.clock().advance_to(HOUR_LATER);

    // Entry 3 is the call to `b`: the fork calls it again.
    let mut request = ForkRun::new(parent.clone(), Script::new("good"));
    request.at = Some(3);
    let fork = engine.fork(request).await.unwrap().run;
    drive(&engine).await;
    assert_eq!(status(&engine, &fork).await, RunStatus::Completed);
    let tools: Vec<String> = dispatcher
        .calls()
        .iter()
        .map(|call| format!("{} {}", call.tool.name, call.idempotency_key))
        .collect();
    assert_eq!(
        tools,
        [
            format!("a {}/1", parent.id),
            format!("b {}/2", parent.id),
            format!("b {}/2", fork.id)
        ]
    );
    let record = engine.get_run(&fork).await.unwrap().unwrap();
    assert_eq!(record.metadata.parent.unwrap().at, Some(3));
}

#[tokio::test]
async fn the_parent_s_outcome_of_an_inherited_call_is_reused_after_the_fork_goes_live() {
    // Both calls start at once. The fork diverges in its response to `a`'s result, before
    // `b`'s result is delivered: `b`'s recorded result is reused, not called again.
    let both = |run: &mut FakeRun| {
        run.call("a", json!(1));
        run.call("b", json!(2));
    };
    let programs = FakeExecutor::new()
        .with("parent", both, |run, delivery| {
            if matches!(delivery, Delivery::ToolCallCompleted { seq: Seq(2), .. }) {
                run.complete(json!("parent"));
            }
        })
        .with("fork", both, |run, delivery| match delivery {
            Delivery::ToolCallCompleted { seq: Seq(1), .. } => {
                run.call("cleanup", json!(3));
            }
            Delivery::ToolCallCompleted { seq: Seq(3), .. } => run.complete(json!("fork")),
            _ => {}
        });
    let dispatcher = RecordingDispatcher::echo();
    let engine = engine(programs, dispatcher.clone());
    let parent = engine
        .start_run(request("parent", json!({})))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    engine.clock().advance_to(HOUR_LATER);
    let fork = engine
        .fork(ForkRun::new(parent.clone(), Script::new("fork")))
        .await
        .unwrap()
        .run;
    drive(&engine).await;

    let tools: Vec<String> = dispatcher
        .calls()
        .iter()
        .map(|call| call.tool.name.clone())
        .collect();
    assert_eq!(tools, ["a", "b", "cleanup"], "b was not called again");
    let history = engine.history(&fork).await.unwrap();
    let reused = history
        .iter()
        .find(|entry| {
            matches!(
                entry.event,
                Event::Completion(Completion::ToolCallCompleted { seq: Seq(2), .. })
            )
        })
        .unwrap();
    assert_eq!(reused.recorded_at, HOUR_LATER, "delivered live");
    let state = engine.run_state(&fork).await.unwrap();
    assert!(state.pending_tool_calls().is_empty());
}

/// Calls `a`, then waits for its result and completes with it.
fn one_call() -> FakeExecutor {
    FakeExecutor::new().with(
        "one_call",
        |run| {
            run.call("a", json!("x"));
        },
        |run, delivery| match delivery {
            Delivery::ToolCallCompleted { result, .. } => run.complete(result.clone()),
            Delivery::ToolCallFailed { error, .. } => run.fail("ToolError", &error.message),
            _ => run.fail("CancelledError", "cancelled"),
        },
    )
}

/// Starts a run of `one_call` and cancels it before its call is dispatched.
async fn cancelled_with_a_call_in_flight(engine: &TestEngine, toolset: Toolset) -> RunRef {
    let mut request = request("one_call", json!({}));
    request.toolset = toolset;
    let parent = engine.start_run(request).await.unwrap().run;
    engine.activate_next().await.unwrap().unwrap();
    engine.cancel(&parent, None).await.unwrap();
    engine.activate_next().await.unwrap().unwrap();
    assert_eq!(status(engine, &parent).await, RunStatus::Cancelled);
    parent
}

#[tokio::test]
async fn a_call_left_without_an_outcome_is_called_again_with_the_parent_s_key() {
    let dispatcher = RecordingDispatcher::echo();
    let engine = engine(one_call(), dispatcher.clone());
    let toolset = Toolset::from_host_tools(["a"]).unwrap();
    let parent = cancelled_with_a_call_in_flight(&engine, toolset).await;
    assert!(dispatcher.calls().is_empty());

    let fork = engine
        .fork(ForkRun::new(parent.clone(), Script::new("one_call")))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    assert_eq!(status(&engine, &fork).await, RunStatus::Completed);
    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].idempotency_key, format!("{}/1", parent.id));
}

#[tokio::test]
async fn an_at_most_once_call_left_without_an_outcome_fails_with_outcome_unknown() {
    let dispatcher = RecordingDispatcher::echo();
    let engine = engine(one_call(), dispatcher.clone());
    let mut tool = Tool::new("a", ToolRoute::Host { group: None }, json!({}));
    tool.policy.on_interruption = Interruption::AtMostOnce;
    let toolset = Toolset::new([], [tool]).unwrap();
    let parent = cancelled_with_a_call_in_flight(&engine, toolset).await;

    let fork = engine
        .fork(ForkRun::new(parent.clone(), Script::new("one_call")))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    assert_eq!(status(&engine, &fork).await, RunStatus::Failed);
    assert!(dispatcher.calls().is_empty(), "never called twice");
    let history = engine.history(&fork).await.unwrap();
    let failure = history
        .iter()
        .find_map(|entry| match &entry.event {
            Event::Completion(Completion::ToolCallFailed { error, .. }) => Some(error),
            _ => None,
        })
        .unwrap();
    assert_eq!(failure.kind, ToolFailureKind::OutcomeUnknown);
    assert!(
        failure.message.contains("without an outcome"),
        "{failure:?}"
    );
}

#[tokio::test]
async fn compatibility_tells_where_a_corrected_script_diverges_and_what_a_fork_reuses() {
    let engine = engine(programs(), dispatcher());
    let parent = failed_parent(&engine).await;

    let same = engine
        .compatibility(&parent, &Script::new("buggy"), None)
        .await
        .unwrap();
    assert!(same.is_compatible(), "{same:?}");
    assert_eq!(same.point, ForkPoint::EndOfParent);
    assert_eq!(same.status, RunStatus::Failed);

    let fixed = engine
        .compatibility(&parent, &Script::new("fixed"), None)
        .await
        .unwrap();
    assert!(!fixed.is_compatible());
    let ForkPoint::Diverged(mismatch) = &fixed.point else {
        panic!("{fixed:?}")
    };
    assert_eq!(
        mismatch.to_string(),
        "history entry 3 is tool call #2 b(\"bad\"), but the script emitted tool call #2 b(\"good\")"
    );
    assert_eq!(fixed.status, RunStatus::Active);

    let forced = engine
        .compatibility(&parent, &Script::new("buggy"), Some(2))
        .await
        .unwrap();
    assert_eq!(forced.point, ForkPoint::At(2));
    assert_eq!(forced.reused, [Seq(1)], "a's result is reused");
}

#[tokio::test]
async fn a_fork_is_refused_like_a_run_that_cannot_start() {
    let engine = engine(programs(), dispatcher());
    let parent = failed_parent(&engine).await;
    let unknown = RunRef::new(parent.tenant.clone(), ptc_core::RunId::from_u128(7));
    assert_eq!(
        engine
            .fork(ForkRun::new(unknown.clone(), Script::new("fixed")))
            .await,
        Err(ForkError::ParentNotFound(unknown.id))
    );
    let refused = engine
        .fork(ForkRun::new(parent.clone(), Script::new("syntax error")))
        .await
        .unwrap_err();
    assert!(matches!(
        refused,
        ForkError::Start(StartError::InvalidScript(_))
    ));
    let mut request = ForkRun::new(parent, Script::new("fixed"));
    request.ttl = Some(Duration::ZERO);
    assert!(matches!(
        engine.fork(request).await,
        Err(ForkError::Start(StartError::InvalidTtl { .. }))
    ));
}
