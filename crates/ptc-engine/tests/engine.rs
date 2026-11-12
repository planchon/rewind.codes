//! The engine's use cases, with in-memory adapters, a virtual clock and a fake executor.

#![allow(clippy::unwrap_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use ptc_core::{
    AbortKind, CancelReason, Command, Completion, Event, HistoryEntry, Lifecycle, Payload,
    RunStatus, Script, Seq, Timestamp, ToolFailure, ToolFailureKind,
};
use ptc_engine::adapters::{MemoryJournal, VirtualClock};
use ptc_engine::ports::{
    AcquireRunnable, Clock, Delivery, Enqueued, ExecutorError, JournalStore, QuarantineKind,
    RunFilter, RunRef, STICKY_GRACE, TenantId, TenantScope, VirtualTime,
};
use ptc_engine::replay::ReplayEnd;
use ptc_engine::{
    ActivationOutcome, DriveOptions, EngineConfig, Limits, ReplayVerdict, StartError, ToolOrder,
};
use serde_json::json;
use support::*;

fn events(history: &[HistoryEntry]) -> Vec<&'static str> {
    history.iter().map(|entry| entry.event.name()).collect()
}

async fn drive(engine: &TestEngine) {
    let report = engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    assert!(!report.interrupted);
}

async fn status(engine: &TestEngine, run: &RunRef) -> RunStatus {
    engine.get_run(run).await.unwrap().unwrap().status
}

/// Calls `a` with the input, and completes with its result.
fn one_call() -> FakeExecutor {
    FakeExecutor::new().with(
        "one_call",
        |run| {
            let args = run.input.clone();
            run.call("a", args);
        },
        |run, delivery| run.complete(result_of(delivery).clone()),
    )
}

#[tokio::test]
async fn a_run_commits_complete_trigger_groups_and_replays_identically() {
    let dispatcher = RecordingDispatcher::echo();
    let engine = engine(one_call(), dispatcher.clone());
    let run = engine
        .start_run(request("one_call", json!({ "n": 1 })))
        .await
        .unwrap();
    assert!(run.created);
    assert_eq!(status(&engine, &run.run).await, RunStatus::Pending);

    drive(&engine).await;

    let history = engine.history(&run.run).await.unwrap();
    assert_eq!(
        events(&history),
        [
            "run_started",
            "schedule_tool_call",
            "tool_call_completed",
            "complete_run"
        ]
    );
    assert_eq!(status(&engine, &run.run).await, RunStatus::Completed);
    let state = engine.run_state(&run.run).await.unwrap();
    assert!(state.pending_tool_calls().is_empty());

    // Every tool call carries its idempotency key.
    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].idempotency_key, format!("{}/1", run.run.id));
    assert_eq!(calls[0].attempt, 1);

    assert_eq!(
        engine.replay(&run.run, None).await.unwrap(),
        ReplayVerdict::Matches(ReplayEnd::EndOfHistory)
    );
}

#[tokio::test]
async fn an_idempotency_key_starts_one_run() {
    let engine = engine(one_call(), RecordingDispatcher::echo());
    let mut request = request("one_call", json!({}));
    request.idempotency_key = Some("toolu_01".into());
    let first = engine.start_run(request.clone()).await.unwrap();
    let retry = engine.start_run(request).await.unwrap();
    assert!(first.created && !retry.created);
    assert_eq!(first.run, retry.run);
    assert_eq!(
        engine
            .list_runs(&RunFilter::new(TenantId::local()))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_script_that_does_not_compile_is_refused_before_a_run_exists() {
    let engine = engine(one_call(), RecordingDispatcher::echo());
    let refused = engine
        .start_run(request("syntax error here", json!({})))
        .await
        .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "the script is invalid: syntax error here: unexpected token"
    );
    assert!(
        engine
            .list_runs(&RunFilter::new(TenantId::local()))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn invalid_requests_are_rejected_before_a_run_exists() {
    let engine = engine(one_call(), RecordingDispatcher::echo());
    let mut too_long = request("one_call", json!({}));
    too_long.ttl = Some(Duration::from_secs(400 * 86_400));
    assert!(matches!(
        engine.start_run(too_long).await,
        Err(StartError::InvalidTtl { .. })
    ));
    assert!(
        engine
            .list_runs(&RunFilter::new(TenantId::local()))
            .await
            .unwrap()
            .is_empty()
    );
}

/// What passes the checks at start but fails when the script loads, such as an error thrown by
/// its top-level code, aborts the run.
#[tokio::test]
async fn a_script_that_cannot_load_is_aborted() {
    let engine = engine(FakeExecutor::new(), RecordingDispatcher::echo());
    let run = engine
        .start_run(request("unloadable", json!({})))
        .await
        .unwrap();
    drive(&engine).await;
    let history = engine.history(&run.run).await.unwrap();
    assert_eq!(events(&history), ["run_started", "run_aborted"]);
    let Event::Lifecycle(Lifecycle::RunAborted(reason)) = &history[1].event else {
        unreachable!()
    };
    assert_eq!(reason.kind, AbortKind::InvalidScript);
    assert!(reason.message.contains("top-level failure"), "{reason}");
    assert_eq!(status(&engine, &run.run).await, RunStatus::Aborted);
    // Replay reproduces the abort.
    assert!(matches!(
        engine.replay(&run.run, None).await.unwrap(),
        ReplayVerdict::Matches(ReplayEnd::Aborted(_))
    ));
}

#[tokio::test]
async fn durable_timers_move_logical_time_in_virtual_time() {
    let programs = FakeExecutor::new().with(
        "sleep",
        |run| {
            run.timer(10 * DAY);
        },
        |run, _| {
            let now = run.now.0;
            run.complete(json!(now));
        },
    );
    let engine = engine(programs, RecordingDispatcher::echo());
    let run = engine.start_run(request("sleep", json!({}))).await.unwrap();
    drive(&engine).await;

    let history = engine.history(&run.run).await.unwrap();
    assert_eq!(
        events(&history),
        ["run_started", "start_timer", "timer_fired", "complete_run"]
    );
    assert_eq!(history[2].recorded_at, Timestamp(T0.0 + 10 * DAY));
    assert_eq!(
        history[3].event,
        Event::Command(Command::CompleteRun {
            output: json!(T0.0 + 10 * DAY).into()
        })
    );
    assert_eq!(engine.clock().now(), Timestamp(T0.0 + 10 * DAY));
}

/// Waits for "approval", and completes with its payload.
fn approval() -> FakeExecutor {
    FakeExecutor::new().with(
        "approval",
        |run| run.await_signal("approval"),
        |run, delivery| match delivery {
            Delivery::SignalReceived { payload, .. } => run.complete(payload.clone()),
            Delivery::CancelRequested { .. } => run.fail("CancelledError", "run cancelled"),
            other => panic!("unexpected {other:?}"),
        },
    )
}

#[tokio::test]
async fn a_run_waiting_for_a_signal_waits_for_the_outside_world() {
    let engine = engine(approval(), RecordingDispatcher::echo());
    let run = engine
        .start_run(request("approval", json!({})))
        .await
        .unwrap();
    drive(&engine).await;

    // Virtual time does not jump to the deadline: only the outside world can wake the run.
    assert_eq!(status(&engine, &run.run).await, RunStatus::Active);
    assert_eq!(engine.clock().now(), T0);
    let state = engine.run_state(&run.run).await.unwrap();
    assert_eq!(state.awaited_signals().len(), 1);

    let key = Some("approval-1".to_owned());
    assert_eq!(
        engine
            .signal(&run.run, "approval", json!({ "by": "paul" }), key.clone())
            .await
            .unwrap(),
        Enqueued::Accepted
    );
    assert_eq!(
        engine
            .signal(&run.run, "approval", json!({ "by": "paul" }), key)
            .await
            .unwrap(),
        Enqueued::Duplicate
    );
    drive(&engine).await;
    assert_eq!(status(&engine, &run.run).await, RunStatus::Completed);
    let history = engine.history(&run.run).await.unwrap();
    assert_eq!(
        events(&history),
        [
            "run_started",
            "signal_awaited",
            "signal_received",
            "complete_run"
        ]
    );

    // A signal to a run that has ended is refused.
    assert_eq!(
        engine
            .signal(&run.run, "approval", json!(null), None)
            .await
            .unwrap(),
        Enqueued::RunEnded
    );
}

#[tokio::test]
async fn the_deadline_cancels_a_run_through_the_normal_path() {
    let engine = engine(approval(), RecordingDispatcher::echo());
    let mut request = request("approval", json!({}));
    request.ttl = Some(Duration::from_millis(HOUR));
    let run = engine.start_run(request).await.unwrap();
    drive(&engine).await;
    assert_eq!(status(&engine, &run.run).await, RunStatus::Active);

    engine.clock().advance_to(Timestamp(T0.0 + 2 * HOUR));
    drive(&engine).await;
    let history = engine.history(&run.run).await.unwrap();
    assert_eq!(
        history[2].event,
        Event::Completion(Completion::CancelRequested {
            reason: CancelReason::Ttl
        })
    );
    assert_eq!(status(&engine, &run.run).await, RunStatus::Cancelled);
}

#[tokio::test]
async fn cancellation_gives_the_script_one_grace_delivery() {
    let programs = FakeExecutor::new()
        .with(
            "cleans_up",
            |run| {
                run.timer(30 * DAY);
            },
            |run, _| run.complete(json!("cleaned up")),
        )
        .with(
            "lingers",
            |run| {
                run.timer(30 * DAY);
            },
            |run, _| {
                run.log("cleaning up");
                run.call("cleanup", json!({}));
            },
        );
    let engine = engine(programs, RecordingDispatcher::echo());

    let polite = engine
        .start_run(request("cleans_up", json!({})))
        .await
        .unwrap();
    let stubborn = engine
        .start_run(request("lingers", json!({})))
        .await
        .unwrap();
    engine
        .run_until_idle(&DriveOptions::default())
        .await
        .unwrap();
    for run in [&polite.run, &stubborn.run] {
        assert_eq!(
            engine.cancel(run, Some("user asked".into())).await.unwrap(),
            Enqueued::Accepted
        );
    }
    drive(&engine).await;

    assert_eq!(status(&engine, &polite.run).await, RunStatus::Completed);

    // The script did not end within its grace: the trigger is committed with run_aborted, and
    // what it emitted in response is dropped with its effects.
    let history = engine.history(&stubborn.run).await.unwrap();
    assert_eq!(
        events(&history),
        [
            "run_started",
            "start_timer",
            "cancel_requested",
            "run_aborted"
        ]
    );
    let Event::Lifecycle(Lifecycle::RunAborted(reason)) = &history[3].event else {
        unreachable!()
    };
    assert_eq!(reason.kind, AbortKind::Cancelled);
    assert!(engine.claim_tool_tasks(10).await.unwrap().is_empty());
    assert_eq!(engine.journal().next_timer().await.unwrap(), None);
}

#[tokio::test]
async fn payloads_leave_the_events_by_size() {
    let big = "x".repeat(5_000);
    let engine = engine(one_call(), RecordingDispatcher::echo());
    let run = engine
        .start_run(request("one_call", json!({ "text": big })))
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(status(&engine, &run.run).await, RunStatus::Completed);

    let stored = engine.history(&run.run).await.unwrap();
    let digests = stored
        .iter()
        .filter(|entry| {
            matches!(
                &entry.event,
                Event::Lifecycle(Lifecycle::RunStarted(started)) if matches!(started.input, Payload::Digest(_))
            ) || matches!(
                &entry.event,
                Event::Command(Command::ScheduleToolCall { args: Payload::Digest(_), .. })
                    | Event::Completion(Completion::ToolCallCompleted { result: Payload::Digest(_), .. })
                    | Event::Command(Command::CompleteRun { output: Payload::Digest(_) })
            )
        })
        .count();
    assert_eq!(digests, 4, "input, args, result and output are offloaded");
    // 5 KB: in the payloads table, deduplicated, not in the blob store.
    assert_eq!(engine.blobs().len(), 0);
    let value = json!({ "text": big });
    let row = engine
        .journal()
        .get_payload(&run.run.tenant, Payload::digest_of(&value))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.value, Some(value));
    assert_eq!(row.size, 5_011);

    let exported = engine.export_history(&run.run).await.unwrap();
    assert_eq!(
        exported[3].event,
        Event::Command(Command::CompleteRun {
            output: json!({ "text": "x".repeat(5_000) }).into()
        })
    );
    assert_eq!(
        engine.replay(&run.run, None).await.unwrap(),
        ReplayVerdict::Matches(ReplayEnd::EndOfHistory)
    );
}

#[tokio::test]
async fn payloads_above_the_table_tier_live_in_the_blob_store() {
    let huge = json!({ "text": "y".repeat(70_000) });
    let engine = engine_with(one_call(), RecordingDispatcher::echo(), replaying());
    let run = engine
        .start_run(request("one_call", huge.clone()))
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(status(&engine, &run.run).await, RunStatus::Completed);
    assert_eq!(engine.blobs().len(), 1, "one value, deduplicated");
    let row = engine
        .journal()
        .get_payload(&run.run.tenant, Payload::digest_of(&huge))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.size, row.value), (70_011, None));
    assert!(row.preview.starts_with("{\"text\":\"yyy"));
    assert_eq!(
        engine.replay(&run.run, None).await.unwrap(),
        ReplayVerdict::Matches(ReplayEnd::EndOfHistory)
    );
}

/// Emits a different argument on every start: a platform bug that replay must catch.
fn non_deterministic() -> FakeExecutor {
    let starts = Arc::new(AtomicU32::new(0));
    FakeExecutor::new().with(
        "flaky_platform",
        move |run| {
            let n = starts.fetch_add(1, Ordering::SeqCst);
            run.call("a", json!({ "n": n }));
        },
        |run, _| {
            run.timer(DAY);
        },
    )
}

#[tokio::test]
async fn a_replay_mismatch_quarantines_the_run_and_commits_nothing() {
    let engine = engine_with(
        non_deterministic(),
        RecordingDispatcher::echo(),
        replaying(),
    );
    let run = engine
        .start_run(request("flaky_platform", json!({})))
        .await
        .unwrap();
    let report = engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    assert_eq!(report.quarantined, 1);

    let history = engine.history(&run.run).await.unwrap();
    assert_eq!(events(&history), ["run_started", "schedule_tool_call"]);
    let record = engine.get_run(&run.run).await.unwrap().unwrap();
    assert_eq!(
        record.status,
        RunStatus::Active,
        "quarantine is not a status"
    );
    assert_eq!(
        record.quarantine.as_ref().map(|q| q.reason.as_str()),
        Some(
            "history entry 1 is tool call #1 a({\"n\":0}), \
             but the script emitted tool call #1 a({\"n\":1})"
        )
    );
    assert_eq!(engine.activate_next().await.unwrap(), None, "never offered");

    // Released after a "fix" that does not fix anything: quarantined again.
    assert!(engine.release_quarantine(&run.run).await.unwrap());
    let report = engine.activate_next().await.unwrap().unwrap();
    assert!(matches!(
        report.outcome,
        ActivationOutcome::Quarantined {
            kind: QuarantineKind::NonDeterminism,
            ..
        }
    ));

    // An operator gives up.
    assert!(engine.give_up(&run.run).await.unwrap());
    let history = engine.history(&run.run).await.unwrap();
    let Event::Lifecycle(Lifecycle::RunAborted(reason)) = &history.last().unwrap().event else {
        panic!("{history:?}")
    };
    assert_eq!(reason.kind, AbortKind::NonDeterminism);
    assert_eq!(status(&engine, &run.run).await, RunStatus::Aborted);
    assert!(!engine.give_up(&run.run).await.unwrap());
}

#[tokio::test]
async fn a_quarantined_run_is_given_up_at_its_deadline() {
    let engine = engine_with(
        non_deterministic(),
        RecordingDispatcher::echo(),
        replaying(),
    );
    let mut request = request("flaky_platform", json!({}));
    request.ttl = Some(Duration::from_millis(HOUR));
    let run = engine.start_run(request).await.unwrap();
    drive(&engine).await;
    assert!(
        engine
            .get_run(&run.run)
            .await
            .unwrap()
            .unwrap()
            .quarantine
            .is_some()
    );

    engine.clock().advance_to(Timestamp(T0.0 + HOUR));
    assert_eq!(engine.expire_due_runs().await.unwrap(), 1);
    assert_eq!(status(&engine, &run.run).await, RunStatus::Aborted);
    let record = engine.get_run(&run.run).await.unwrap().unwrap();
    assert_eq!(record.quarantine, None);
}

#[tokio::test]
async fn limits_end_the_run_and_drop_what_broke_them() {
    let programs = FakeExecutor::new()
        .with(
            "fan_out",
            |run| {
                for n in 0..20 {
                    run.call("a", json!(n));
                }
            },
            |_, _| {},
        )
        .with(
            "huge_output",
            |run| run.complete(json!("y".repeat(2_000))),
            |_, _| {},
        );
    let config = EngineConfig {
        limits: Limits {
            max_commands_per_activation: 10,
            max_payload_bytes: 1_000,
            ..Limits::default()
        },
        ..EngineConfig::default()
    };
    let engine = engine_with(programs, RecordingDispatcher::echo(), config);

    for (script, message) in [
        (
            "fan_out",
            "the script emitted more than 10 commands in one activation",
        ),
        (
            "huge_output",
            "the payload of complete_run is 2002 bytes, above the limit of 1000 bytes",
        ),
    ] {
        let run = engine.start_run(request(script, json!({}))).await.unwrap();
        drive(&engine).await;
        let history = engine.history(&run.run).await.unwrap();
        assert_eq!(events(&history), ["run_started", "run_aborted"], "{script}");
        let Event::Lifecycle(Lifecycle::RunAborted(reason)) = &history[1].event else {
            unreachable!()
        };
        assert_eq!(
            (reason.kind, reason.message.as_str()),
            (AbortKind::LimitExceeded, message)
        );
    }
    assert!(engine.claim_tool_tasks(100).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_executor_that_emits_after_the_end_aborts_the_run() {
    let programs = FakeExecutor::new().with(
        "broken_executor",
        |run| {
            run.complete(json!(1));
            run.call("a", json!({}));
        },
        |_, _| {},
    );
    let engine = engine(programs, RecordingDispatcher::echo());
    let run = engine
        .start_run(request("broken_executor", json!({})))
        .await
        .unwrap();
    drive(&engine).await;
    let history = engine.history(&run.run).await.unwrap();
    assert_eq!(events(&history), ["run_started", "run_aborted"]);
    let Event::Lifecycle(Lifecycle::RunAborted(reason)) = &history[1].event else {
        unreachable!()
    };
    assert_eq!(reason.kind, AbortKind::Internal);
}

#[tokio::test]
async fn completions_arrive_in_the_order_the_tools_answer() {
    let programs = FakeExecutor::new().with(
        "fan_out",
        |run| {
            run.call("a", json!(1));
            run.call("a", json!(2));
            run.call("a", json!(3));
        },
        |run, delivery| {
            if let Delivery::ToolCallCompleted { seq: Seq(1), .. } = delivery {
                run.complete(json!("last answer"));
            }
        },
    );
    let engine = engine(programs, RecordingDispatcher::echo());
    let run = engine
        .start_run(request("fan_out", json!({})))
        .await
        .unwrap();
    let options = DriveOptions {
        tool_order: ToolOrder::Reversed,
        ..DriveOptions::default()
    };
    engine.run_until_idle_virtual(&options).await.unwrap();
    let delivered: Vec<Seq> = engine
        .history(&run.run)
        .await
        .unwrap()
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Completion(Completion::ToolCallCompleted { seq, .. }) => Some(*seq),
            _ => None,
        })
        .collect();
    assert_eq!(delivered, [Seq(3), Seq(2), Seq(1)]);
    assert_eq!(status(&engine, &run.run).await, RunStatus::Completed);
    assert_eq!(
        engine.replay(&run.run, None).await.unwrap(),
        ReplayVerdict::Matches(ReplayEnd::EndOfHistory)
    );
}

fn charge() -> FakeExecutor {
    FakeExecutor::new().with(
        "charge",
        |run| {
            run.call("a", json!({ "amount": 100 }));
        },
        |run, delivery| match delivery {
            Delivery::ToolCallCompleted { result, .. } => run.complete(result.clone()),
            Delivery::ToolCallFailed { error, .. } => run.complete(json!(error.kind.as_str())),
            other => panic!("unexpected {other:?}"),
        },
    )
}

#[tokio::test]
async fn an_interrupted_call_is_retried_with_the_same_idempotency_key() {
    let dispatcher = RecordingDispatcher::echo();
    let engine = engine(charge(), dispatcher.clone());
    let run = engine
        .start_run(request("charge", json!({})))
        .await
        .unwrap();
    engine.activate_next().await.unwrap();

    // A dispatcher claims the call and dies before reporting its outcome.
    let claimed = engine.claim_tool_tasks(10).await.unwrap();
    assert_eq!(claimed.len(), 1);
    engine.clock().advance_to(Timestamp(T0.0 + 60_000));
    drive(&engine).await;

    let calls = dispatcher.calls();
    assert_eq!(calls.len(), 1, "only the second attempt reached the tool");
    assert_eq!(calls[0].attempt, 2);
    assert_eq!(calls[0].idempotency_key, format!("{}/1", run.run.id));
    assert_eq!(status(&engine, &run.run).await, RunStatus::Completed);
    let history = engine.history(&run.run).await.unwrap();
    assert_eq!(
        history[3].event,
        Event::Command(Command::CompleteRun {
            output: json!({ "amount": 100 }).into()
        })
    );
}

#[tokio::test]
async fn a_call_interrupted_too_many_times_has_an_unknown_outcome() {
    let dispatcher = RecordingDispatcher::echo();
    let engine = engine(charge(), dispatcher.clone());
    let run = engine
        .start_run(request("charge", json!({})))
        .await
        .unwrap();
    engine.activate_next().await.unwrap();

    // Five dispatchers in a row claim the call and die.
    for attempt in 1..=5_u32 {
        let claimed = engine.claim_tool_tasks(10).await.unwrap();
        assert_eq!(claimed[0].task.attempt, attempt);
        engine
            .clock()
            .advance_to(Timestamp(T0.0 + 60_000 * u64::from(attempt)));
    }
    drive(&engine).await;

    assert!(dispatcher.calls().is_empty(), "no sixth attempt is made");
    let history = engine.history(&run.run).await.unwrap();
    assert_eq!(
        history[2].event,
        Event::Completion(Completion::ToolCallFailed {
            seq: Seq(1),
            error: ToolFailure {
                kind: ToolFailureKind::OutcomeUnknown,
                message: "the call to a was interrupted after 5 attempts and may or may not \
                          have taken effect"
                    .into(),
                data: None,
            },
        })
    );
    assert_eq!(
        history[3].event,
        Event::Command(Command::CompleteRun {
            output: json!("outcome_unknown").into()
        })
    );
}

#[tokio::test]
async fn replay_with_another_script_tells_where_it_diverges() {
    let programs = one_call().with(
        "other_call",
        |run| {
            run.call("b", json!({}));
        },
        |run, delivery| run.complete(result_of(delivery).clone()),
    );
    let engine = engine(programs, RecordingDispatcher::echo());
    let run = engine
        .start_run(request("one_call", json!({})))
        .await
        .unwrap();
    drive(&engine).await;

    let verdict = engine
        .replay(&run.run, Some(&Script::new("other_call")))
        .await
        .unwrap();
    let ReplayVerdict::Diverges(mismatch) = verdict else {
        panic!("{verdict:?}")
    };
    assert_eq!(
        mismatch.to_string(),
        "history entry 1 is tool call #1 a({}), but the script emitted tool call #1 b({})"
    );
}

#[tokio::test]
async fn a_lost_lease_cannot_commit() {
    let engine = engine(one_call(), RecordingDispatcher::echo());
    let run = engine
        .start_run(request("one_call", json!({})))
        .await
        .unwrap();
    let journal: &MemoryJournal = engine.journal();
    let clock: &VirtualClock = engine.clock();
    let stale = journal
        .acquire_runnable(&AcquireRunnable {
            owner: "paused-worker",
            epochs: &[ptc_core::Epoch(1)],
            tenants: &TenantScope::All,
            now: clock.now(),
            lease_duration: Duration::from_secs(30),
            sticky_grace: STICKY_GRACE,
            draw: 0,
        })
        .await
        .unwrap()
        .unwrap();
    clock.advance_to(Timestamp(T0.0 + 31_000));
    drive(&engine).await;
    assert_eq!(status(&engine, &run.run).await, RunStatus::Completed);
    assert!(journal.take_inbox(&stale).await.is_err());
}

#[tokio::test]
async fn a_failing_activation_is_retried_later_and_does_not_block_other_runs() {
    let starts = Arc::new(AtomicU32::new(0));
    let programs = one_call().with(
        "unlucky",
        move |run| {
            // The second start is the replay of the second activation.
            if starts.fetch_add(1, Ordering::SeqCst) == 1 {
                run.refuse(ExecutorError::Internal(
                    "the isolate could not start".into(),
                ));
                return;
            }
            run.call("a", json!({}));
        },
        |run, delivery| run.complete(result_of(delivery).clone()),
    );
    let engine = engine_with(programs, RecordingDispatcher::echo(), replaying());
    let unlucky = engine
        .start_run(request("unlucky", json!({})))
        .await
        .unwrap();
    let lucky = engine
        .start_run(request("one_call", json!({})))
        .await
        .unwrap();

    let report = engine
        .run_until_idle(&DriveOptions::default())
        .await
        .unwrap();
    assert_eq!(report.failures.len(), 1, "{report:?}");
    assert_eq!(report.failures[0].0, unlucky.run);
    assert!(matches!(
        report.failures[0].1,
        ptc_engine::EngineError::ReplayInterrupted { .. }
    ));
    assert_eq!(status(&engine, &lucky.run).await, RunStatus::Completed);
    let record = engine.get_run(&unlucky.run).await.unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Active);
    assert_eq!(
        record.quarantine, None,
        "a transient failure does not quarantine"
    );

    // Once its lease expires, the run is activated again, and goes on.
    engine.clock().advance_to(Timestamp(T0.0 + 31_000));
    let report = engine
        .run_until_idle(&DriveOptions::default())
        .await
        .unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(status(&engine, &unlucky.run).await, RunStatus::Completed);
}

#[tokio::test]
async fn values_reach_the_script_with_their_keys_in_canonical_order() {
    let programs = FakeExecutor::new().with(
        "keys",
        |run| {
            let keys: Vec<String> = run.input.as_object().unwrap().keys().cloned().collect();
            run.complete(json!(keys));
        },
        |_, _| {},
    );
    let engine = engine(programs, RecordingDispatcher::echo());
    let input: serde_json::Value = serde_json::from_str(r#"{"z":1,"a":2}"#).unwrap();
    let run = engine.start_run(request("keys", input)).await.unwrap();
    drive(&engine).await;
    let history = engine.history(&run.run).await.unwrap();
    assert_eq!(
        history.last().unwrap().event,
        Event::Command(Command::CompleteRun {
            output: json!(["a", "z"]).into()
        })
    );
}
