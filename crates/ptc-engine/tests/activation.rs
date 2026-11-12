//! Sticky execution, failure counts and quarantine, with in-memory adapters and a fake executor.

#![allow(clippy::unwrap_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use ptc_core::{AbortKind, Event, Lifecycle, RunStatus};
use ptc_engine::ports::{Clock, ExecutorError, QuarantineKind, RunRef, VirtualTime};
use ptc_engine::{ActivationOutcome, DriveOptions, EngineConfig};
use serde_json::json;
use support::*;

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

/// Three calls in a row, then completes with the count.
fn three_calls() -> FakeExecutor {
    FakeExecutor::new().with(
        "three_calls",
        |run| {
            run.call("a", json!(1));
        },
        |run, delivery| {
            let n = result_of(delivery).as_u64().unwrap();
            if n < 3 {
                run.call("a", json!(n + 1));
            } else {
                run.complete(json!(n));
            }
        },
    )
}

#[tokio::test]
async fn a_cached_instance_is_used_again_without_a_replay() {
    let programs = three_calls();
    let engine = engine(programs.clone(), RecordingDispatcher::echo());
    let run = engine
        .start_run(request("three_calls", json!({})))
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(status(&engine, &run.run).await, RunStatus::Completed);
    // Four activations (the start and three results), one instance: no replay.
    assert_eq!(programs.starts(), 1);
    assert_eq!(engine.cached_instances(), 0, "an ended run is not kept");
    let record = engine.get_run(&run.run).await.unwrap().unwrap();
    assert_eq!(record.sticky_worker.as_deref(), Some("worker"));
}

#[tokio::test]
async fn without_the_cache_every_activation_replays() {
    let programs = three_calls();
    let engine = engine_with(programs.clone(), RecordingDispatcher::echo(), replaying());
    let run = engine
        .start_run(request("three_calls", json!({})))
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(status(&engine, &run.run).await, RunStatus::Completed);
    // The start, then a replay for each of the three results.
    assert_eq!(programs.starts(), 4);
}

#[tokio::test]
async fn an_evicted_instance_gives_up_its_stickiness_and_is_replayed() {
    let programs = FakeExecutor::new().with(
        "waits",
        |run| run.await_signal("go"),
        |run, _| run.complete(json!("done")),
    );
    let mut config = EngineConfig::default();
    config.cache.max_instances = 1;
    let engine = engine_with(programs.clone(), RecordingDispatcher::echo(), config);
    let first = engine
        .start_run(request("waits", json!({})))
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(engine.cached_instances(), 1);
    let record = engine.get_run(&first.run).await.unwrap().unwrap();
    assert_eq!(record.sticky_worker.as_deref(), Some("worker"));

    // A second run takes the only place: the first run's instance is evicted.
    let second = engine
        .start_run(request("waits", json!({})))
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(engine.cached_instances(), 1);
    let record = engine.get_run(&first.run).await.unwrap().unwrap();
    assert_eq!(record.sticky_worker, None, "the priority is given up");
    assert_eq!(programs.starts(), 2);

    // The first run is replayed on its next activation; the second is a hit.
    engine
        .signal(&first.run, "go", json!(null), None)
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(programs.starts(), 3);
    engine
        .signal(&second.run, "go", json!(null), None)
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(
        programs.starts(),
        3,
        "the first run ended without taking the place back: a hit"
    );
    assert_eq!(status(&engine, &first.run).await, RunStatus::Completed);
    assert_eq!(status(&engine, &second.run).await, RunStatus::Completed);
}

/// A run whose replays fail while `broken` is set: a fault of the platform that is not a
/// mismatch (an isolate that cannot start).
fn fragile(broken: Arc<AtomicBool>) -> FakeExecutor {
    let starts = Arc::new(AtomicU32::new(0));
    FakeExecutor::new().with(
        "fragile",
        move |run| {
            if starts.fetch_add(1, Ordering::SeqCst) > 0 && broken.load(Ordering::SeqCst) {
                run.refuse(ExecutorError::Internal(
                    "the isolate could not start".into(),
                ));
                return;
            }
            run.await_signal("go");
        },
        |run, _| run.complete(json!("done")),
    )
}

#[tokio::test]
async fn five_failed_activations_in_a_row_quarantine_the_run() {
    let broken = Arc::new(AtomicBool::new(true));
    let engine = engine_with(
        fragile(broken.clone()),
        RecordingDispatcher::echo(),
        replaying(),
    );
    let run = engine
        .start_run(request("fragile", json!({})))
        .await
        .unwrap();
    drive(&engine).await;
    engine.signal(&run.run, "go", json!(1), None).await.unwrap();

    for attempt in 1..=5 {
        let report = engine.activate_next().await.unwrap().unwrap();
        let record = engine.get_run(&run.run).await.unwrap().unwrap();
        if attempt < 5 {
            assert!(
                matches!(report.outcome, ActivationOutcome::Failed(_)),
                "{report:?}"
            );
            assert_eq!(record.failures, attempt);
            assert_eq!(record.quarantine, None);
            // The lease is kept until it expires.
            assert_eq!(engine.activate_next().await.unwrap(), None);
            let later = engine.clock().now().saturating_add(std::time::Duration::from_secs(31));
            engine.clock().advance_to(later);
        } else {
            let ActivationOutcome::Quarantined { kind, reason } = report.outcome else {
                panic!("{report:?}");
            };
            assert_eq!(kind, QuarantineKind::Failures);
            assert!(reason.contains("the isolate could not start"), "{reason}");
            let quarantine = record.quarantine.unwrap();
            assert_eq!(quarantine.kind, QuarantineKind::Failures);
        }
    }
    assert_eq!(engine.activate_next().await.unwrap(), None, "never offered");

    // Given up, it ends with `internal`.
    assert!(engine.give_up(&run.run).await.unwrap());
    let history = engine.history(&run.run).await.unwrap();
    let Event::Lifecycle(Lifecycle::RunAborted(reason)) = &history.last().unwrap().event else {
        panic!("{history:?}")
    };
    assert_eq!(reason.kind, AbortKind::Internal);
    assert!(reason.message.contains("the isolate could not start"));
}

#[tokio::test]
async fn a_worker_of_a_new_build_retries_each_quarantined_run_once() {
    let broken = Arc::new(AtomicBool::new(true));
    let programs = fragile(broken.clone());
    let mut config = replaying();
    config.build = "build-1".into();
    let engine = engine_with(programs, RecordingDispatcher::echo(), config);
    let run = engine
        .start_run(request("fragile", json!({})))
        .await
        .unwrap();
    drive(&engine).await;
    engine.signal(&run.run, "go", json!(1), None).await.unwrap();
    for _ in 0..5 {
        engine.activate_next().await.unwrap();
        let later = engine.clock().now().saturating_add(std::time::Duration::from_secs(31));
        engine.clock().advance_to(later);
    }
    assert!(
        engine
            .get_run(&run.run)
            .await
            .unwrap()
            .unwrap()
            .quarantine
            .is_some()
    );

    // Still broken: the retry fails, commits nothing, and leaves the run quarantined; the same
    // build does not retry it again.
    let reports = engine.retry_quarantined().await.unwrap();
    assert_eq!(reports.len(), 1);
    assert!(matches!(reports[0].outcome, ActivationOutcome::Failed(_)));
    let record = engine.get_run(&run.run).await.unwrap().unwrap();
    assert_eq!(
        record.quarantine.as_ref().unwrap().retried_by.as_deref(),
        Some("build-1")
    );
    assert!(engine.retry_quarantined().await.unwrap().is_empty());
    assert_eq!(record.history_len, 2, "run_started and signal_awaited");

    // A new build where the fault is fixed: the run leaves quarantine and goes on, its signal
    // delivered.
    broken.store(false, Ordering::SeqCst);
    let mut config = replaying();
    config.build = "build-2".into();
    let engine = ptc_engine::Engine::new(engine.into_adapters(), config);
    let reports = engine.retry_quarantined().await.unwrap();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(
        matches!(
            reports[0].outcome,
            ActivationOutcome::Committed {
                status: RunStatus::Completed,
                ..
            }
        ),
        "{reports:?}"
    );
    let record = engine.get_run(&run.run).await.unwrap().unwrap();
    assert_eq!(record.quarantine, None);
    assert_eq!(record.status, RunStatus::Completed);
}
