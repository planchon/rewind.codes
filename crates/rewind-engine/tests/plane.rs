//! The use cases of the run plane, in the driver: the owner, its executors, its call executor
//! and its organization actor, with memory adapters and a fake executor
//! (`archi/07-code-architecture.md`, "Inside a node", "Tests"). The behaviours are those the
//! history's engine keeps (`tests/engine.rs`, `tests/tasks.rs`, `tests/quotas.rs`).

#![allow(clippy::unwrap_used, clippy::too_many_lines)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rewind_core::PartitionId;
use rewind_core::{
    AbortKind, ActionFailure, ActionFailureKind, ActionPolicy, ActionSet, CancelReason, Completion,
    Event, Lifecycle, RunStatus, Seq, Timestamp,
};
use rewind_engine::edge::EdgeError;
use rewind_engine::plane::HarnessActions;
use rewind_engine::ports::link::{
    HarnessFailure, NodeReply, NodeRequest, PartitionRequest, Refusal, StreamAttach, StreamId,
};
use rewind_engine::ports::{
    BlobStore, CatalogStore, Delivery, Dispatched, Enqueued, NamespaceName, Page, QuarantineKind,
    Quotas, RunArchive, RunOrder, RunQuery, Scope, SecretStore, TaskState, TenantId, Window,
};
use serde_json::json;
use support::interleaved::Interleaved;
use support::plane::{World, config};
use support::{DAY, FakeExecutor, HOUR, RecordingDispatcher, request, result_of};

fn chain() -> FakeExecutor {
    FakeExecutor::new().with(
        "chain",
        |run| {
            run.call("a", json!({ "n": 1 }));
        },
        |run, delivery| {
            let n = result_of(delivery)["n"].as_i64().unwrap();
            if n < 3 {
                run.call("b", json!({ "n": n + 1 }));
            } else {
                run.complete(json!({ "n": n }));
            }
        },
    )
}

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

fn one_call() -> FakeExecutor {
    FakeExecutor::new().with(
        "one_call",
        |run| {
            run.call("a", json!({ "x": 1 }));
        },
        |run, delivery| match delivery {
            Delivery::CallCompleted { result, .. } => run.complete(result.clone()),
            Delivery::CallFailed { error, .. } => run.fail("ActionError", &error.message),
            Delivery::CancelRequested { .. } => run.fail("CancelledError", "cancelled"),
            other => panic!("unexpected {other:?}"),
        },
    )
}

/// The action set of `request`, with `a` under `policy`.
fn with_policy(
    mut request: rewind_engine::StartRun,
    policy: ActionPolicy,
) -> rewind_engine::StartRun {
    let mut actions: Vec<rewind_core::Action> = request.action_set.actions().to_vec();
    for action in &mut actions {
        if action.name == "a" {
            action.policy = policy;
        }
    }
    request.action_set = ActionSet::new(Vec::new(), actions).unwrap();
    request
}

#[tokio::test]
async fn a_run_calls_its_actions_ends_and_is_sealed() {
    let mut world = World::new(chain(), RecordingDispatcher::echo()).await;
    let run = world.run(request("chain", json!({}))).await;
    let view = world.view(&run).await;
    assert_eq!(view.head.status, RunStatus::Completed);
    assert_eq!(
        world.events(&run).await,
        vec![
            "run_started",
            "schedule_call",
            "call_completed",
            "schedule_call",
            "call_completed",
            "schedule_call",
            "call_completed",
            "complete_run",
        ]
    );
    // Sealed: the attempt's history is in the archive, and the run is read from there.
    assert_eq!(view.head.sealed, Some(view.head.attempt));
    let sealed = world
        .plane
        .archive
        .get(&run, view.head.attempt)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(sealed.events.len(), 8);
    let summary = view.summary.unwrap();
    assert_eq!(summary.status, RunStatus::Completed);
    assert_eq!(summary.calls, 3);
    // Every call carried `{run}/{seq}`.
    let keys: Vec<String> = world
        .plane
        .dispatcher
        .calls()
        .iter()
        .map(|call| call.idempotency_key.clone())
        .collect();
    assert_eq!(
        keys,
        ["1", "2", "3"]
            .map(|seq| format!("{}/{seq}", run.id))
            .to_vec()
    );
}

/// A read of a history is two reads, the head then the events: the seal of the attempt,
/// committed between them, deletes the events the head counts. The read still answers the
/// whole history, from the archive, never an ended run with an empty one.
#[tokio::test]
async fn a_history_read_while_its_attempt_is_sealed_is_whole() {
    let store = Interleaved::new();
    let mut world = World::over(
        store.clone(),
        chain(),
        RecordingDispatcher::echo(),
        1,
        &[config("node-a", 1)],
    )
    .await;
    store.hold_next_seal();
    let run = world.run(request("chain", json!({}))).await;
    // The run has ended, and its seal is being committed.
    assert!(store.holding());
    let view = world.view(&run).await;
    assert_eq!(view.head.status, RunStatus::Completed);
    assert_eq!(view.head.sealed, None);
    assert_eq!(view.head.history_len, 8);

    // The history, read as the run's view reads it: the seal lands after the head is read.
    let history = world.history(&run).await;
    assert_eq!(store.applied(), 1);
    assert_eq!(history.len(), 8);
    assert_eq!(history[7].event.name(), "complete_run");
    assert_eq!(world.view(&run).await.head.sealed, Some(view.head.attempt));

    // The history's export, as `GET /v1/runs/{id}/export` reads it.
    let mut world = World::over(
        Interleaved::new(),
        chain(),
        RecordingDispatcher::echo(),
        1,
        &[config("node-a", 1)],
    )
    .await;
    let store = world.plane.partitions.clone();
    store.hold_next_seal();
    let run = world.run(request("chain", json!({}))).await;
    assert!(store.holding());
    let mut gateway = world.driver.gateway(&world.at.clone());
    let export = world.edge.export_run(&mut gateway, &run).await.unwrap();
    assert_eq!(store.applied(), 1);
    assert_eq!(export.events.len(), 8);
}

/// An owner whose read finds the partition opened by another node drops it, as a fenced commit
/// does: the request is answered `NotOwner`, and the node opens the partition again while the
/// map names it. A partition nobody writes to is never left answering reads with an error.
#[tokio::test]
async fn a_read_that_finds_the_partition_opened_elsewhere_drops_it_and_the_node_opens_it_again() {
    let store = Interleaved::new();
    let mut world = World::over(
        store.clone(),
        chain(),
        RecordingDispatcher::echo(),
        1,
        &[config("node-a", 1)],
    )
    .await;
    let run = world.run(request("chain", json!({}))).await;
    assert_eq!(store.opens(), 1);

    store.fence_next_read();
    let view = world.view(&run).await;
    assert_eq!(view.head.status, RunStatus::Completed);
    assert_eq!(store.opens(), 2, "the partition was opened again");
    assert_eq!(world.events(&run).await.len(), 8);
}

/// Two reads in flight when the first finds the partition opened by another node: the owner
/// drops the partition at the first, and the second, which comes back to an owner that closed,
/// is answered `NotOwner` too, never left waiting (an action set's creation, which reads every
/// partition, waited forever for a read that came back during a hand-over).
#[tokio::test]
async fn reads_in_flight_when_the_partition_is_dropped_are_answered_not_owner() {
    let store = Interleaved::new();
    let mut world = World::over(
        store.clone(),
        chain(),
        RecordingDispatcher::echo(),
        1,
        &[config("node-a", 1)],
    )
    .await;
    let run = world.run(request("chain", json!({}))).await;
    store.fence_next_reads(2);
    let read = || NodeRequest::Partition {
        partition: PartitionId::new(0),
        request: Box::new(PartitionRequest::GetRun { run: run.clone() }),
    };
    let at = world.at.clone();
    let replies = world
        .driver
        .requests(&at, vec![read(), read()])
        .await
        .unwrap();
    assert!(
        replies
            .iter()
            .all(|reply| matches!(reply, NodeReply::NotOwner { .. })),
        "{replies:?}"
    );
    // The node opened the partition again, and the run is read.
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);
}

#[tokio::test]
async fn a_start_key_starts_one_run_and_a_signal_key_is_taken_once() {
    let mut world = World::new(approval(), RecordingDispatcher::echo()).await;
    let mut first = request("approval", json!({}));
    first.idempotency_key = Some("conversation-7".into());
    let created = world.start(first.clone()).await.unwrap();
    assert!(created.created);
    let again = world.start(first).await.unwrap();
    assert_eq!(again.run, created.run);
    assert!(!again.created, "the key answers its run, whatever else");
    world.idle().await;
    let run = created.run;
    // Virtual time does not jump to the deadline: only the outside world wakes the run.
    assert_eq!(world.view(&run).await.head.status, RunStatus::Active);
    assert_eq!(world.now(), support::T0);

    let accepted = world
        .signal(
            &run,
            "approval",
            json!({ "by": "paul" }),
            Some("approval-1"),
        )
        .await
        .unwrap();
    assert_eq!(accepted, Enqueued::Accepted);
    let duplicate = world
        .signal(
            &run,
            "approval",
            json!({ "by": "paul" }),
            Some("approval-1"),
        )
        .await
        .unwrap();
    assert_eq!(duplicate, Enqueued::Duplicate);
    world.idle().await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);
    assert_eq!(
        world.events(&run).await,
        [
            "run_started",
            "signal_awaited",
            "signal_received",
            "complete_run"
        ]
    );
    let ended = world
        .signal(&run, "approval", json!(null), None)
        .await
        .unwrap();
    assert_eq!(ended, Enqueued::RunEnded);
    // A name Rewind keeps is refused.
    let reserved = world
        .signal(&run, "rewind:ended:x", json!(null), None)
        .await;
    assert!(
        matches!(reserved, Err(EdgeError::Refused(Refusal::Signal(_)))),
        "{reserved:?}"
    );
}

#[tokio::test]
async fn a_ten_day_sleep_takes_no_wall_clock_time() {
    let programs = FakeExecutor::new().with(
        "sleeper",
        |run| {
            run.timer(10 * DAY);
        },
        |run, delivery| {
            assert!(matches!(delivery, Delivery::TimerFired { .. }));
            run.complete(json!({ "at": run.now.0 }));
        },
    );
    let mut world = World::new(programs, RecordingDispatcher::echo()).await;
    let run = world.run(request("sleeper", json!({}))).await;
    let history = world.history(&run).await;
    assert_eq!(
        history
            .iter()
            .map(|entry| entry.event.name())
            .collect::<Vec<_>>(),
        ["run_started", "start_timer", "timer_fired", "complete_run"]
    );
    assert_eq!(history[2].recorded_at, Timestamp(support::T0.0 + 10 * DAY));
}

#[tokio::test]
async fn thousands_of_timers_due_at_once_fire_until_the_history_is_full() {
    // `await Promise.all(Array.from({ length: 9000 }, () => sleep(1)))`, again and again: each
    // batch of timers fires in the activations of a few commits, and each delivery costs what
    // it appends, not the size of the run's state.
    const AT_ONCE: u32 = 9_000;
    let programs = FakeExecutor::new().with(
        "bursts",
        |run| {
            for _ in 0..AT_ONCE {
                run.timer(1);
            }
        },
        |run, delivery| {
            let Delivery::TimerFired { seq } = delivery else {
                panic!("unexpected {delivery:?}");
            };
            if seq.0 % AT_ONCE == 0 {
                for _ in 0..AT_ONCE {
                    run.timer(1);
                }
            }
        },
    );
    let mut world = World::new(programs, RecordingDispatcher::echo()).await;
    let run = world.run(request("bursts", json!({}))).await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Aborted);
    let history = world.history(&run).await;
    let Event::Lifecycle(Lifecycle::RunAborted(reason)) = &history.last().unwrap().event else {
        panic!("the run is aborted");
    };
    assert_eq!(reason.kind, AbortKind::LimitExceeded);
    assert_eq!(reason.message, "the history would exceed 50000 events");
}

#[tokio::test]
async fn the_deadline_cancels_a_run_through_the_normal_path() {
    let mut world = World::new(approval(), RecordingDispatcher::echo()).await;
    let mut request = request("approval", json!({}));
    request.ttl = Some(Duration::from_millis(HOUR));
    let run = world.run(request).await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Active);
    world.advance(2 * HOUR);
    world.settle().await;
    let history = world.history(&run).await;
    assert_eq!(
        history[2].event,
        Event::Completion(Completion::CancelRequested {
            reason: CancelReason::Ttl
        })
    );
    assert_eq!(world.view(&run).await.head.status, RunStatus::Cancelled);
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
                run.call("cleanup", json!({}));
            },
        );
    let mut world = World::new(programs, RecordingDispatcher::echo()).await;
    let polite = world
        .start(request("cleans_up", json!({})))
        .await
        .unwrap()
        .run;
    let stubborn = world
        .start(request("lingers", json!({})))
        .await
        .unwrap()
        .run;
    world.settle().await;
    for run in [&polite, &stubborn] {
        assert_eq!(world.cancel(run).await.unwrap(), Enqueued::Accepted);
    }
    world.settle().await;
    assert_eq!(world.view(&polite).await.head.status, RunStatus::Completed);
    assert_eq!(
        world.events(&stubborn).await,
        [
            "run_started",
            "start_timer",
            "cancel_requested",
            "run_aborted"
        ]
    );
    let history = world.history(&stubborn).await;
    let Event::Lifecycle(Lifecycle::RunAborted(reason)) = &history[3].event else {
        unreachable!()
    };
    assert_eq!(reason.kind, AbortKind::Cancelled);
    assert!(world.tasks(&stubborn).await.is_empty(), "no call was left");
}

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

fn replaying(world_config: &mut rewind_engine::plane::NodeConfig) {
    world_config.cache.max_instances = 0;
}

#[tokio::test]
async fn a_replay_mismatch_quarantines_the_run_and_commits_nothing() {
    let mut node = config("node-a", 1);
    replaying(&mut node);
    let mut world = World::with(non_deterministic(), RecordingDispatcher::echo(), 1, &[node]).await;
    let run = world.run(request("flaky_platform", json!({}))).await;
    // The second activation replays, diverges, and commits nothing.
    assert_eq!(world.events(&run).await, ["run_started", "schedule_call"]);
    let view = world.view(&run).await;
    assert_eq!(
        view.head.status,
        RunStatus::Active,
        "quarantine is not a status"
    );
    let quarantine = view.head.quarantine.clone().unwrap();
    assert_eq!(quarantine.kind, QuarantineKind::NonDeterminism);
    assert_eq!(
        quarantine.reason,
        "event 1 is call #1 a({\"n\":0}), but the code emitted call #1 a({\"n\":1})"
    );
    assert!(
        view.summary.unwrap().quarantine.is_some(),
        "the index shows it"
    );

    // Released after a fix that fixes nothing: quarantined again at its next activation.
    assert!(world.release(&run).await);
    world.advance(DAY);
    world.settle().await;
    assert!(world.view(&run).await.head.quarantine.is_some());

    // An operator gives up.
    assert!(world.give_up(&run).await);
    let history = world.history(&run).await;
    let Event::Lifecycle(Lifecycle::RunAborted(reason)) = &history.last().unwrap().event else {
        panic!("{history:?}")
    };
    assert_eq!(reason.kind, AbortKind::NonDeterminism);
    let view = world.view(&run).await;
    assert_eq!(view.head.status, RunStatus::Aborted);
    assert_eq!(view.head.quarantine, None);
    assert!(!world.give_up(&run).await);
}

#[tokio::test]
async fn a_quarantined_run_is_given_up_at_its_deadline() {
    let mut node = config("node-a", 1);
    replaying(&mut node);
    let mut world = World::with(non_deterministic(), RecordingDispatcher::echo(), 1, &[node]).await;
    let mut request = request("flaky_platform", json!({}));
    request.ttl = Some(Duration::from_millis(2 * DAY));
    let run = world.run(request).await;
    world.idle().await;
    assert!(world.view(&run).await.head.quarantine.is_some());
    world.advance(2 * DAY);
    world.settle().await;
    let view = world.view(&run).await;
    assert_eq!(view.head.status, RunStatus::Aborted);
    assert_eq!(view.head.quarantine, None);
}

#[tokio::test]
async fn failing_activations_are_tried_again_by_a_timer_then_quarantined() {
    // The code loads once; every later start fails inside the executor: a replay that cannot
    // go on, worth trying again later.
    let starts = Arc::new(AtomicU32::new(0));
    let programs = FakeExecutor::new().with(
        "unlucky",
        move |run| {
            if starts.fetch_add(1, Ordering::SeqCst) > 0 {
                run.refuse(rewind_engine::ports::ExecutorError::Internal(
                    "out of threads".into(),
                ));
                return;
            }
            run.timer(1_000);
        },
        |run, _| run.complete(json!("done")),
    );
    let mut node = config("node-a", 1);
    replaying(&mut node);
    let mut world = World::with(programs, RecordingDispatcher::echo(), 1, &[node]).await;
    let run = world.run(request("unlucky", json!({}))).await;
    let view = world.view(&run).await;
    let quarantine = view.head.quarantine.unwrap();
    assert_eq!(quarantine.kind, QuarantineKind::Failures);
    assert!(
        quarantine.reason.contains("out of threads"),
        "{}",
        quarantine.reason
    );
    assert_eq!(view.head.failures, 5);
    // Each failure waited for its timer: 30 s apart.
    assert_eq!(world.now(), Timestamp(support::T0.0 + 1_000 + 4 * 30_000));
    assert_eq!(world.events(&run).await, ["run_started", "start_timer"]);
}

#[tokio::test]
async fn a_call_under_approval_waits_for_the_harness() {
    let mut world = World::new(one_call(), RecordingDispatcher::echo()).await;
    let mut policy = ActionPolicy::default_for(&rewind_core::ActionRoute::Harness { group: None });
    policy.approval = true;
    let approved = world
        .run(with_policy(request("one_call", json!({})), policy))
        .await;
    let rejected = world
        .run(with_policy(request("one_call", json!({})), policy))
        .await;
    for run in [&approved, &rejected] {
        let tasks = world.tasks(run).await;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].state, TaskState::Approval);
        let summary = world.view(run).await.summary.unwrap();
        assert_eq!(
            summary.waits[0].task,
            Some(rewind_engine::ports::TaskPhase::Approval),
            "the index shows what the call waits for"
        );
    }
    assert!(
        world.plane.dispatcher.calls().is_empty(),
        "nothing was sent"
    );
    let task = world.tasks(&approved).await[0].task.clone();
    world.approve(&task).await.unwrap();
    world.idle().await;
    assert_eq!(
        world.view(&approved).await.head.status,
        RunStatus::Completed
    );

    let task = world.tasks(&rejected).await[0].task.clone();
    world.reject(&task, "not today").await.unwrap();
    world.idle().await;
    let history = world.history(&rejected).await;
    let Event::Completion(Completion::CallFailed { error, .. }) = &history[2].event else {
        panic!("{history:?}")
    };
    assert_eq!(error.message, "not today");
    assert_eq!(world.view(&rejected).await.head.status, RunStatus::Failed);
    // A task that no longer waits is refused.
    assert!(matches!(
        world.approve(&task).await,
        Err(EdgeError::Refused(Refusal::TaskGone))
    ));
}

#[tokio::test]
async fn transient_failures_are_retried_with_backoff_under_retry_only() {
    let attempts = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&attempts);
    let dispatcher = RecordingDispatcher::dispatching(move |call, _| {
        if counted.fetch_add(1, Ordering::SeqCst) < 2 {
            return Dispatched::Transient(ActionFailure {
                kind: ActionFailureKind::Unavailable,
                message: "busy".into(),
                data: None,
            });
        }
        Dispatched::Result(call.args.clone())
    });
    let mut world = World::new(one_call(), dispatcher).await;
    let run = world.run(request("one_call", json!({}))).await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);
    let calls = world.plane.dispatcher.calls();
    assert_eq!(
        calls.iter().map(|call| call.delivery).collect::<Vec<_>>(),
        [1, 2, 3],
        "each try is an attempt more"
    );
    assert!(
        calls
            .iter()
            .all(|call| call.idempotency_key == calls[0].idempotency_key)
    );
    // The backoff of the first two attempts passed, in virtual time.
    assert!(world.now() > Timestamp(support::T0.0 + 2_000));

    // Not idempotent: a transient failure is the outcome.
    let attempts = Arc::new(AtomicU32::new(0));
    let counted = Arc::clone(&attempts);
    let dispatcher = RecordingDispatcher::dispatching(move |_, _| {
        counted.fetch_add(1, Ordering::SeqCst);
        Dispatched::Transient(ActionFailure {
            kind: ActionFailureKind::Unavailable,
            message: "busy".into(),
            data: None,
        })
    });
    let mut world = World::new(one_call(), dispatcher).await;
    let mut policy = ActionPolicy::default_for(&rewind_core::ActionRoute::Harness { group: None });
    policy.idempotent = false;
    let run = world
        .run(with_policy(request("one_call", json!({})), policy))
        .await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Failed);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_deferred_call_is_answered_through_its_token_or_times_out() {
    let dispatcher = RecordingDispatcher::dispatching(|_, _| Dispatched::Deferred);
    let mut world = World::new(one_call(), dispatcher).await;
    let answered = world.run(request("one_call", json!({}))).await;
    let tasks = world.tasks(&answered).await;
    assert!(
        matches!(tasks[0].state, TaskState::Deferred { .. }),
        "{tasks:?}"
    );
    let token = world.plane.dispatcher.calls()[0].token.clone();
    world
        .complete(&token, json!({ "later": true }))
        .await
        .unwrap();
    world.idle().await;
    let view = world.view(&answered).await;
    assert_eq!(view.head.status, RunStatus::Completed);
    // Answered once: the token is gone.
    assert!(matches!(
        world.complete(&token, json!({})).await,
        Err(EdgeError::Refused(Refusal::TaskGone))
    ));

    // A deferred call that nobody answers fails at its deadline, bounded by its execution
    // timeout.
    let mut policy = ActionPolicy::default_for(&rewind_core::ActionRoute::Harness { group: None });
    policy.execution_timeout = Some(Duration::from_secs(60));
    let late = world
        .run(with_policy(request("one_call", json!({})), policy))
        .await;
    assert_eq!(world.view(&late).await.head.status, RunStatus::Active);
    world.advance(61_000);
    world.settle().await;
    let history = world.history(&late).await;
    let Event::Completion(Completion::CallFailed { error, .. }) = &history[2].event else {
        panic!("{history:?}")
    };
    assert_eq!(error.kind, ActionFailureKind::Timeout);
}

/// The organization actor of a tenant lives in its home partition's owner: when the partition
/// is opened again (here after a read found it opened elsewhere), the new actor starts without
/// the streams the previous one held, and every edge attaches its streams of the tenant again.
#[tokio::test]
async fn a_stream_attached_before_its_home_partition_is_opened_again_is_still_handed_tasks() {
    let mut node = config("node-a", 1);
    node.harness_actions = HarnessActions::Streams;
    let store = Interleaved::new();
    let mut world = World::over(
        store.clone(),
        one_call(),
        RecordingDispatcher::echo(),
        1,
        &[node],
    )
    .await;
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
                slots: 4,
            },
        )
        .unwrap();
    let first = world.run(request("one_call", json!({}))).await;
    let task = support::plane::handed(&mut stream).remove(0);
    world.complete(&task.token, json!("one")).await.unwrap();
    world.idle().await;

    store.fence_next_read();
    assert_eq!(world.view(&first).await.head.status, RunStatus::Completed);
    assert_eq!(store.opens(), 2, "the partition was opened again");

    let second = world.run(request("one_call", json!({}))).await;
    let handed = support::plane::handed(&mut stream);
    assert_eq!(
        handed.len(),
        1,
        "the new actor hands the task to the stream"
    );
    assert_eq!(handed[0].task.run, second);
    world
        .complete(&handed[0].token, json!("two"))
        .await
        .unwrap();
    world.idle().await;
    assert_eq!(world.view(&second).await.head.status, RunStatus::Completed);
}

#[tokio::test]
async fn harness_tasks_are_handed_to_sdk_streams_and_answered_through_their_token() {
    let mut node = config("node-a", 1);
    node.harness_actions = HarnessActions::Streams;
    let mut world = World::with(one_call(), RecordingDispatcher::echo(), 1, &[node]).await;
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
                slots: 4,
            },
        )
        .unwrap();
    let run = world.run(request("one_call", json!({}))).await;
    let handed = support::plane::handed(&mut stream);
    assert_eq!(handed.len(), 1);
    let task = &handed[0];
    assert_eq!(task.action, "a");
    assert_eq!(task.delivery, 1);
    assert_eq!(task.idempotency_key, format!("{}/1", run.id));
    assert!(
        world.plane.dispatcher.calls().is_empty(),
        "a harness action is not dispatched"
    );
    world
        .complete(&task.token, json!({ "from": "harness" }))
        .await
        .unwrap();
    world.idle().await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);

    // A failure the handler says is transient is tried again, handed out with a new token.
    let again = world.run(request("one_call", json!({}))).await;
    let first = support::plane::handed(&mut stream);
    world
        .fail(
            &first[0].token,
            HarnessFailure {
                message: "try later".into(),
                data: None,
                retryable: true,
            },
        )
        .await
        .unwrap();
    world.idle().await;
    let second = support::plane::handed(&mut stream);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].delivery, 2);
    assert_ne!(second[0].token, first[0].token);
    world.complete(&second[0].token, json!({})).await.unwrap();
    world.idle().await;
    assert_eq!(world.view(&again).await.head.status, RunStatus::Completed);
    assert!(
        support::plane::handed(&mut stream).is_empty(),
        "an answered task is not gone"
    );

    // A run cancelled while its task is handed: the task no longer waits for an answer, and the
    // stream hears it is gone; its token answers nothing.
    let cancelled = world.run(request("one_call", json!({}))).await;
    let handed = support::plane::handed(&mut stream);
    assert_eq!(handed.len(), 1);
    world.cancel(&cancelled).await.unwrap();
    world.idle().await;
    let (tasks, gone) = support::plane::received(&mut stream);
    assert!(tasks.is_empty());
    assert_eq!(gone, [handed[0].token.clone()]);
    assert!(matches!(
        world.complete(&handed[0].token, json!({})).await,
        Err(EdgeError::Refused(Refusal::TaskGone))
    ));
}

#[tokio::test]
async fn tasks_in_flight_read_claimed() {
    let mut node = config("node-a", 1);
    node.harness_actions = HarnessActions::Streams;
    let mut world = World::with(one_call(), RecordingDispatcher::echo(), 1, &[node]).await;
    let at = world.at.clone();
    // Recorded `ready`, it is in flight once its owner handed it to the organization actor for
    // a stream: it reads `claimed`, from the owner's memory.
    let run = world.run(request("one_call", json!({}))).await;
    let claimed =
        |state: &TaskState| matches!(state, TaskState::Claimed { owner, .. } if owner == "node-a");
    let tasks = world.tasks(&run).await;
    assert!(claimed(&tasks[0].state), "{:?}", tasks[0].state);
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
    world.settle().await;
    let handed = support::plane::handed(&mut stream);
    assert_eq!(handed.len(), 1);
    let tasks = world.tasks(&run).await;
    assert!(claimed(&tasks[0].state), "{:?}", tasks[0].state);
    world.complete(&handed[0].token, json!({})).await.unwrap();
    world.idle().await;
    assert!(world.tasks(&run).await.is_empty());
}

#[tokio::test]
async fn a_start_above_the_active_runs_limit_is_refused() {
    let mut world = World::new(approval(), RecordingDispatcher::echo()).await;
    let tenant = TenantId::local();
    let local = NamespaceName::new("local").unwrap();
    let quotas = Quotas {
        active_runs: Some(1),
        ..Quotas::default()
    };
    world
        .plane
        .catalog
        .put_namespace(
            &tenant,
            &local,
            Some(&quotas),
            world.partitions,
            world.now(),
        )
        .await
        .unwrap();
    let first = world.run(request("approval", json!({}))).await;
    let refused = world.start(request("approval", json!({}))).await;
    let Err(rewind_engine::StartError::QuotaExceeded(exceeded)) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!(exceeded.quota, rewind_engine::ports::QuotaKind::ActiveRuns);
    assert_eq!((exceeded.limit, exceeded.used), (1, 1));
    // A retried request with its key still returns its run; once the first ends, a start goes.
    world.cancel(&first).await.unwrap();
    world.idle().await;
    world.start(request("approval", json!({}))).await.unwrap();
}

#[tokio::test]
async fn a_start_above_the_tenant_s_ceiling_is_refused_in_any_namespace() {
    let mut world = World::new(approval(), RecordingDispatcher::echo()).await;
    let acme = TenantId::new("acme").unwrap();
    world
        .plane
        .catalog
        .put_tenant(&rewind_engine::ports::Tenant {
            quotas: Quotas {
                active_runs: Some(1),
                ..Quotas::default()
            },
            ..rewind_engine::ports::Tenant::new(acme.clone())
        })
        .await
        .unwrap();
    let start = |namespace: &str| {
        let mut start = request("approval", json!({}));
        start.tenant = acme.clone();
        start.namespace = NamespaceName::new(namespace).unwrap();
        start
    };
    let first = world.run(start("alice")).await;
    let refused = world.start(start("bob")).await;
    let Err(rewind_engine::StartError::QuotaExceeded(exceeded)) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!(
        exceeded,
        rewind_engine::ports::QuotaExceeded {
            quota: rewind_engine::ports::QuotaKind::ActiveRuns,
            scope: rewind_engine::ports::QuotaScope::Tenant,
            limit: 1,
            used: 1,
            resets_at: None,
        },
        "the ceiling counts the runs of every namespace"
    );
    // Once the first run has ended, another can start.
    world.cancel(&first).await.unwrap();
    world.idle().await;
    world.start(start("bob")).await.unwrap();
}

#[tokio::test]
async fn an_erased_run_leaves_nothing_and_retention_erases_an_ended_one() {
    let mut world = World::new(chain(), RecordingDispatcher::echo()).await;
    let run = world.run(request("chain", json!({}))).await;
    assert!(world.erase(&run).await);
    let mut gateway = world.driver.gateway(&world.at.clone());
    assert_eq!(world.edge.get_run(&mut gateway, &run).await.unwrap(), None);
    assert_eq!(
        world
            .plane
            .archive
            .get(&run, Default::default())
            .await
            .unwrap(),
        None
    );
    assert!(!world.erase(&run).await);

    // The retention of the tenant, a year by default, erases an ended run.
    let kept = world.run(request("chain", json!({}))).await;
    world.advance(366 * DAY);
    world.settle().await;
    let mut gateway = world.driver.gateway(&world.at.clone());
    assert_eq!(world.edge.get_run(&mut gateway, &kept).await.unwrap(), None);
}

#[tokio::test]
async fn webhooks_and_spans_leave_through_the_outbox() {
    let mut world = World::new(chain(), RecordingDispatcher::echo()).await;
    world
        .plane
        .catalog
        .put_subscription(&rewind_engine::ports::Subscription {
            tenant: TenantId::local(),
            id: "sub-1".into(),
            url: "https://harness.example.com/hooks".into(),
            secret: rewind_engine::ports::Secret::new("whsec".to_owned()),
            events: Default::default(),
            labels: BTreeMap::new(),
        })
        .await
        .unwrap();
    let mut request = request("chain", json!({}));
    request.mode = rewind_core::RunMode::Live;
    let run = world.run(request).await;
    let delivered = world.plane.webhooks.deliveries();
    assert!(
        delivered.iter().any(|(_, event)| event.run == Some(run.id)
            && event.kind.event_type().as_str() == "run.completed"),
        "{delivered:?}"
    );
    let spans = world.plane.spans.spans();
    assert!(!spans.is_empty(), "the run's spans are exported");
}

fn subscription(url: &str) -> rewind_engine::ports::Subscription {
    rewind_engine::ports::Subscription {
        tenant: TenantId::local(),
        id: format!("sub-{url}"),
        url: url.into(),
        secret: rewind_engine::ports::Secret::new("whsec".to_owned()),
        events: Default::default(),
        labels: BTreeMap::new(),
    }
}

#[tokio::test]
async fn a_namespace_erased_has_its_runs_cancelled_and_erased_then_is_dropped() {
    let programs = approval().with("chain", |run| run.complete(json!("done")), |_, _| {});
    let mut world = World::new(programs, RecordingDispatcher::echo()).await;
    let gone = NamespaceName::new("gone").unwrap();
    let mut active = request("approval", json!({}));
    active.namespace = gone.clone();
    let mut ended = request("chain", json!({}));
    ended.namespace = gone.clone();
    let active = world.run(active).await;
    let ended = world.run(ended).await;
    assert_eq!(world.view(&ended).await.head.status, RunStatus::Completed);
    // What the control plane keeps of the end user: a test recorded from its runs, a secret.
    let tenant = TenantId::local();
    let mut recorded = rewind_core::Test::new("recorded");
    recorded.origin = Some(rewind_core::TestOrigin {
        run: ended.id,
        version: rewind_core::Digest::of(b"v1"),
        namespace: "gone".into(),
    });
    let suites = world.edge.suites(support::test_format());
    let test = suites
        .store_tests(&tenant, &[recorded], world.now())
        .await
        .unwrap()[0];
    let secret = rewind_engine::ports::SecretId::of_namespace(
        gone.clone(),
        rewind_core::SecretName::new("github").unwrap(),
    );
    world
        .plane
        .secrets
        .put(
            &tenant,
            &rewind_engine::ports::NewSecret {
                id: secret.clone(),
                value: rewind_engine::ports::Secret::new("token"),
                labels: BTreeMap::new(),
                by: "user:u_1".into(),
            },
            world.now(),
        )
        .await
        .unwrap();

    world
        .plane
        .catalog
        .delete_namespace(&tenant, &gone, world.now())
        .await
        .unwrap();
    world.settle().await;
    let mut gateway = world.driver.gateway(&world.at.clone());
    assert_eq!(
        world.edge.get_run(&mut gateway, &active).await.unwrap(),
        None
    );
    assert_eq!(
        world.edge.get_run(&mut gateway, &ended).await.unwrap(),
        None
    );
    assert_eq!(
        world
            .plane
            .catalog
            .get_namespace(&tenant, &gone)
            .await
            .unwrap(),
        None,
        "the control plane drops the namespace once its partition holds nothing of it"
    );
    let suites = world.edge.suites(support::test_format());
    assert_eq!(
        world.plane.catalog.get_test(&tenant, test).await.unwrap(),
        None
    );
    assert_eq!(suites.load_test(&tenant, test).await.unwrap(), None);
    assert_eq!(
        world
            .plane
            .blobs
            .get(
                &rewind_engine::ports::BlobOwner::Tests(tenant.clone()),
                test
            )
            .await
            .unwrap(),
        None,
        "the tests recorded from its runs are erased with it"
    );
    assert_eq!(
        world
            .plane
            .secrets
            .meta(&tenant, &secret, rewind_engine::ports::Day(0))
            .await
            .unwrap(),
        None,
        "its secrets are erased with it"
    );
}

#[tokio::test]
async fn a_quota_crossed_raises_its_alert_once_through_the_outbox() {
    let programs = FakeExecutor::new().with("quick", |run| run.complete(json!("done")), |_, _| {});
    let mut world = World::new(programs, RecordingDispatcher::echo()).await;
    let tenant = TenantId::local();
    world
        .plane
        .catalog
        .put_subscription(&subscription("https://harness.example.com/hooks"))
        .await
        .unwrap();
    let quotas = Quotas {
        runs_per_month: Some(5),
        ..Quotas::default()
    };
    let local = NamespaceName::new("local").unwrap();
    world
        .plane
        .catalog
        .put_namespace(
            &tenant,
            &local,
            Some(&quotas),
            world.partitions,
            world.now(),
        )
        .await
        .unwrap();
    for _ in 0..5 {
        world.run(request("quick", json!({}))).await;
    }
    let alerts: Vec<String> = world
        .plane
        .webhooks
        .deliveries()
        .into_iter()
        .filter(|(_, event)| event.kind.event_type().as_str() == "quota.alert")
        .map(|(_, event)| event.id)
        .collect();
    assert_eq!(alerts.len(), 2, "80 % and 100 %, once each: {alerts:?}");
    // The organization actor answers the alerts of the month, the newest first.
    let month = rewind_engine::ports::Month::of(world.now());
    let mut gateway = world.driver.gateway(&world.at.clone());
    let recorded = world
        .edge
        .quota_alerts(&mut gateway, &tenant, Some(&local), month)
        .await
        .unwrap();
    assert_eq!(
        recorded
            .iter()
            .map(|alert| alert.key.threshold)
            .collect::<Vec<_>>(),
        [
            rewind_engine::ports::Threshold::Reached,
            rewind_engine::ports::Threshold::Near
        ]
    );
    assert!(recorded.iter().all(|alert| alert.key.period == month));
    let other = NamespaceName::new("other").unwrap();
    assert!(
        world
            .edge
            .quota_alerts(&mut gateway, &tenant, Some(&other), month)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        world
            .edge
            .quota_alerts(&mut gateway, &tenant, None, month)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(
        world
            .edge
            .quota_alerts(&mut gateway, &tenant, None, month.next())
            .await
            .unwrap()
            .is_empty()
    );
    let refused = world.start(request("quick", json!({}))).await;
    let Err(rewind_engine::StartError::QuotaExceeded(exceeded)) = refused else {
        panic!("{refused:?}")
    };
    assert_eq!(
        exceeded.quota,
        rewind_engine::ports::QuotaKind::RunsPerMonth
    );
    assert!(exceeded.resets_at.is_some());
}

#[tokio::test]
async fn a_webhook_refused_is_tried_again_by_timers_until_given_up() {
    let mut world = World::new(chain(), RecordingDispatcher::echo()).await;
    let url = "https://down.example.com/hooks";
    world
        .plane
        .catalog
        .put_subscription(&subscription(url))
        .await
        .unwrap();
    world.plane.webhooks.refuse(url);
    let mut request = request("chain", json!({}));
    request.mode = rewind_core::RunMode::Live;
    world.run(request).await;
    // Every try failed: the retries waited for their backoff, in virtual time.
    assert!(
        world.now() > Timestamp(support::T0.0 + 60_000),
        "{:?}",
        world.now()
    );
    assert!(world.plane.webhooks.deliveries().is_empty());
}

#[tokio::test]
async fn a_search_across_partitions_pages_through_every_run_in_order() {
    let mut world = World::with(
        chain(),
        RecordingDispatcher::echo(),
        4,
        &[config("node-a", 4)],
    )
    .await;
    let mut started = Vec::new();
    for n in 0..8 {
        for _ in 0..2 {
            let mut request = request("chain", json!({}));
            request.namespace = NamespaceName::new(format!("user-{n}")).unwrap();
            started.push(world.run(request).await);
        }
    }
    let tenant = TenantId::local();
    let mut seen = Vec::new();
    let mut page = Page::first(3);
    loop {
        let mut gateway = world.driver.gateway(&world.at.clone());
        let found = world
            .edge
            .search(
                &mut gateway,
                &Scope::Tenant(tenant.clone()),
                &RunQuery::new(tenant.clone()),
                RunOrder::Started,
                &page,
            )
            .await
            .unwrap();
        seen.extend(found.summaries.iter().map(|summary| summary.run.id));
        match found.cursor {
            Some(cursor) => page = Page::after(cursor, 3),
            None => break,
        }
    }
    let mut expected: Vec<_> = started.iter().map(|run| run.id).collect();
    expected.sort_by(|a, b| b.cmp(a));
    assert_eq!(
        seen, expected,
        "newest first, each once, across the partitions"
    );
}

#[tokio::test]
async fn searches_and_statistics_read_the_partitions_of_the_scope() {
    let mut world = World::new(chain(), RecordingDispatcher::echo()).await;
    for _ in 0..3 {
        world.run(request("chain", json!({}))).await;
    }
    let tenant = TenantId::local();
    let mut gateway = world.driver.gateway(&world.at.clone());
    let found = world
        .edge
        .search(
            &mut gateway,
            &Scope::Tenant(tenant.clone()),
            &RunQuery::new(tenant.clone()),
            RunOrder::Started,
            &Page::first(2),
        )
        .await
        .unwrap();
    assert_eq!(found.summaries.len(), 2);
    assert!(found.cursor.is_some(), "a page at its limit has a cursor");
    let rest = world
        .edge
        .search(
            &mut gateway,
            &Scope::Tenant(tenant.clone()),
            &RunQuery::new(tenant.clone()),
            RunOrder::Started,
            &Page::after(found.cursor.unwrap(), 2),
        )
        .await
        .unwrap();
    assert_eq!(rest.summaries.len(), 1);
    let window = Window::covering(support::T0, Timestamp(support::T0.0 + DAY));
    let (counters, gauges) = world
        .edge
        .statistics(&mut gateway, &Scope::Tenant(tenant), window)
        .await
        .unwrap();
    assert_eq!(counters.created, 3);
    assert_eq!(counters.runs.started, 3);
    assert_eq!(counters.runs.ended[&RunStatus::Completed], 3);
    assert_eq!(counters.actions["a"].calls, 3);
    assert!(
        counters.cpu > Duration::ZERO,
        "the CPU of the calls that executed is counted"
    );
    assert_eq!(gauges.active_runs, 0);
    let _ = Seq(1);
}

#[tokio::test]
async fn a_series_and_the_namespaces_use_are_one_request_per_partition() {
    let programs = approval().with("chain", |run| run.complete(json!("done")), |_, _| {});
    let mut world = World::with(
        programs,
        RecordingDispatcher::echo(),
        4,
        &[config("node-a", 4)],
    )
    .await;
    let tenant = TenantId::local();
    let alice = NamespaceName::new("alice").unwrap();
    let bob = NamespaceName::new("bob").unwrap();
    let in_namespace = |name: &str, namespace: &NamespaceName| {
        let mut start = request(name, json!({}));
        start.namespace = namespace.clone();
        start
    };
    // Alice runs twice in the first hour and once two hours later, and one run of hers waits;
    // Bob once.
    world.run(in_namespace("chain", &alice)).await;
    world.run(in_namespace("chain", &alice)).await;
    world.run(in_namespace("approval", &alice)).await;
    world.advance(2 * HOUR);
    world.run(in_namespace("chain", &alice)).await;
    world.run(in_namespace("chain", &bob)).await;

    let window = Window::covering(support::T0, Timestamp(support::T0.0 + 4 * HOUR));
    let steps = world.driver.steps();
    let mut gateway = world.driver.gateway(&world.at.clone());
    let series = world
        .edge
        .series(
            &mut gateway,
            &Scope::Tenant(tenant.clone()),
            window,
            std::num::NonZeroU32::new(1).unwrap(),
        )
        .await
        .unwrap();
    let created: Vec<u64> = series
        .iter()
        .map(|(_, counters)| counters.created)
        .collect();
    assert_eq!(created, [3, 0, 2, 0], "one bucket per hour");
    assert_eq!(
        series[2].0,
        Window::hours(series[2].0.from, series[2].0.from.next())
    );
    let messages = world.driver.steps().messages - steps.messages;
    // Two buckets of three hours: the last one is cut at the window's end.
    let mut gateway = world.driver.gateway(&world.at.clone());
    let series = world
        .edge
        .series(
            &mut gateway,
            &Scope::Tenant(tenant.clone()),
            window,
            std::num::NonZeroU32::new(3).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(series.len(), 2);
    assert_eq!(series[0].1.created, 5);
    assert_eq!(series[1].0.to, window.to);
    assert_eq!(
        world.driver.steps().messages - steps.messages,
        2 * messages,
        "a series is one request per partition, whatever its buckets"
    );

    // The use of the month, by namespace, then by day for one.
    let month = rewind_engine::ports::Month::of(world.now());
    let mut gateway = world.driver.gateway(&world.at.clone());
    let used = world
        .edge
        .usage(&mut gateway, &Scope::Tenant(tenant.clone()), month, false)
        .await
        .unwrap();
    assert_eq!(
        used.keys().cloned().collect::<Vec<_>>(),
        [alice.clone(), bob.clone()]
    );
    let of_alice = &used[&alice];
    assert_eq!(of_alice.runs_started, 4);
    assert_eq!(of_alice.active_runs, 1);
    assert_eq!(of_alice.workflows, 2);
    assert!(of_alice.cpu > Duration::ZERO);
    assert!(of_alice.days.is_empty());
    assert_eq!(used[&bob].runs_started, 1);
    let scope = Scope::Namespace {
        tenant: tenant.clone(),
        namespace: alice.clone(),
    };
    let used = world
        .edge
        .usage(&mut gateway, &scope, month, true)
        .await
        .unwrap();
    assert_eq!(used.len(), 1);
    let days = &used[&alice].days;
    assert_eq!(days.len(), 1, "one day of use");
    assert_eq!(days[0].0, rewind_engine::ports::Day::of(support::T0));
    assert_eq!(days[0].1.runs_started, 4);
    assert!(
        world
            .edge
            .usage(&mut gateway, &scope, month.next(), true)
            .await
            .unwrap()
            .get(&alice)
            .is_none_or(|next| next.runs_started == 0 && next.cpu == Duration::ZERO),
        "the next month used nothing yet"
    );
}

#[tokio::test]
async fn cpu_over_its_quota_raises_alerts_and_refuses_nothing() {
    let programs = FakeExecutor::new().with(
        "busy",
        |run| {
            run.spend(Duration::from_millis(3));
            run.complete(json!("done"));
        },
        |_, _| {},
    );
    let mut world = World::new(programs, RecordingDispatcher::echo()).await;
    let tenant = TenantId::local();
    world
        .plane
        .catalog
        .put_subscription(&subscription("https://harness.example.com/hooks"))
        .await
        .unwrap();
    let local = NamespaceName::new("local").unwrap();
    let quotas = Quotas {
        cpu_per_month: Some(Duration::from_millis(10)),
        ..Quotas::default()
    };
    world
        .plane
        .catalog
        .put_namespace(
            &tenant,
            &local,
            Some(&quotas),
            world.partitions,
            world.now(),
        )
        .await
        .unwrap();
    let cpu_alerts = |world: &World| -> Vec<String> {
        world
            .plane
            .webhooks
            .deliveries()
            .into_iter()
            .filter(|(_, event)| {
                event.kind.event_type().as_str() == "quota.alert"
                    && event.id.contains(":cpu_per_month:")
            })
            .map(|(_, event)| event.id)
            .collect()
    };
    // 3 ms, 6 ms: below 80 % of 10 ms.
    for _ in 0..2 {
        world.run(request("busy", json!({}))).await;
    }
    assert!(cpu_alerts(&world).is_empty());
    // 9 ms: 80 %; 12 ms: 100 %; 15 ms: nothing new.
    for _ in 0..3 {
        let run = world.run(request("busy", json!({}))).await;
        assert_eq!(
            world.view(&run).await.head.status,
            RunStatus::Completed,
            "CPU over its quota refuses no run"
        );
    }
    let alerts = cpu_alerts(&world);
    assert_eq!(
        alerts,
        [
            "quota.alert:local:cpu_per_month:2026-01:80",
            "quota.alert:local:cpu_per_month:2026-01:100"
        ],
        "80 % and 100 %, once each"
    );
    let month = rewind_engine::ports::Month::of(world.now());
    let mut gateway = world.driver.gateway(&world.at.clone());
    let recorded = world
        .edge
        .quota_alerts(&mut gateway, &tenant, Some(&local), month)
        .await
        .unwrap();
    assert_eq!(recorded.len(), 2);
    assert!(recorded.iter().all(|alert| {
        alert.key.quota == rewind_engine::ports::QuotaKind::CpuPerMonth && alert.limit == 10
    }));
    assert_eq!(
        recorded[0].used, 12,
        "the use when the limit was reached, in ms"
    );
}

#[tokio::test]
async fn a_call_parked_by_a_schema_change_is_sent_once_the_harness_approves_it() {
    let dispatcher = RecordingDispatcher::dispatching(|call, _| {
        if call.schema_change_approved {
            Dispatched::Result(json!({ "sent": "against the new schema" }))
        } else {
            Dispatched::Park(rewind_engine::ports::ParkReason {
                kind: rewind_engine::ports::ParkKind::SchemaChanged,
                message: "the input schema of a changed: x is now a string".into(),
            })
        }
    });
    let mut world = World::new(one_call(), dispatcher).await;
    world
        .plane
        .catalog
        .put_subscription(&subscription("https://harness.example.com/hooks"))
        .await
        .unwrap();
    let run = world.run(request("one_call", json!({}))).await;
    let tasks = world.tasks(&run).await;
    let TaskState::Parked(reason) = &tasks[0].state else {
        panic!("{:?}", tasks[0].state)
    };
    assert_eq!(reason.kind, rewind_engine::ports::ParkKind::SchemaChanged);
    assert!(!tasks[0].schema_change_approved);
    assert_eq!(
        world.view(&run).await.summary.unwrap().waits[0].task,
        Some(rewind_engine::ports::TaskPhase::Parked),
        "the index shows the call parked"
    );
    assert!(
        world
            .plane
            .webhooks
            .deliveries()
            .iter()
            .any(|(_, event)| event.kind.event_type().as_str() == "action.schema_changed"),
        "the harness is told"
    );

    world.approve(&tasks[0].task).await.unwrap();
    world.idle().await;
    let calls = world.plane.dispatcher.calls();
    assert_eq!(calls.len(), 2);
    assert!(!calls[0].schema_change_approved);
    assert!(
        calls[1].schema_change_approved,
        "sent against the action's current schema"
    );
    let view = world.view(&run).await;
    assert_eq!(view.head.status, RunStatus::Completed);
    let history = world.history(&run).await;
    let Event::Command(rewind_core::Command::CompleteRun { output }) =
        &history.last().unwrap().event
    else {
        panic!("{history:?}")
    };
    assert_eq!(
        output.value(),
        Some(&json!({ "sent": "against the new schema" }))
    );
}

#[tokio::test]
async fn a_signal_url_bound_to_no_run_is_a_run_not_found() {
    let mut world = World::new(approval(), RecordingDispatcher::echo()).await;
    let run = world.run(request("approval", json!({}))).await;
    let token = format!("rewind_wh_{}_{}", run.id, "00".repeat(32));
    let mut gateway = world.driver.gateway(&world.at.clone());
    let refused = world
        .edge
        .signal_by_url(&mut gateway, &token, json!({}), None)
        .await;
    assert_eq!(
        refused,
        Err(EdgeError::Refused(Refusal::RunNotFound(run.id))),
        "a signal URL token its run does not hold"
    );
    let refused = world
        .edge
        .signal_by_url(&mut gateway, "rewind_wh_nothing", json!({}), None)
        .await;
    assert!(
        matches!(refused, Err(EdgeError::Refused(_))),
        "a token that names no run: {refused:?}"
    );
}

/// A signal's key is kept for the life of the run (`archi/06-storage-and-scale.md`,
/// "Idempotency of the API"): a late retry of a delivered signal is acknowledged, its attempt
/// sealed or not; a new signal is refused, the run having ended.
#[tokio::test]
async fn a_signal_key_is_acknowledged_for_the_life_of_the_run() {
    let mut world = World::new(approval(), RecordingDispatcher::echo()).await;
    let run = world.run(request("approval", json!({}))).await;
    let key = Some("approval-1");
    assert_eq!(
        world
            .signal(&run, "approval", json!("yes"), key)
            .await
            .unwrap(),
        Enqueued::Accepted
    );
    // Sent again while the run ends: acknowledged, whether it has ended yet or not.
    assert_eq!(
        world
            .signal(&run, "approval", json!("yes"), key)
            .await
            .unwrap(),
        Enqueued::Duplicate
    );
    world.idle().await;
    let head = world.view(&run).await.head;
    assert_eq!(head.status, RunStatus::Completed);
    assert!(!head.is_unsealed(), "the attempt is sealed");
    assert_eq!(
        world
            .signal(&run, "approval", json!("yes"), key)
            .await
            .unwrap(),
        Enqueued::Duplicate,
        "a late retry, the attempt sealed"
    );
    assert_eq!(
        world
            .signal(&run, "approval", json!("yes"), Some("approval-2"))
            .await
            .unwrap(),
        Enqueued::RunEnded
    );
}
