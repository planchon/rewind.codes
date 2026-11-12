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
    let first = engine.start_run(request("waits", json!({}))).await.unwrap();
    drive(&engine).await;
    assert_eq!(engine.cached_instances(), 1);
    let record = engine.get_run(&first.run).await.unwrap().unwrap();
    assert_eq!(record.sticky_worker.as_deref(), Some("worker"));

    // A second run takes the only place: the first run's instance is evicted.
    let second = engine.start_run(request("waits", json!({}))).await.unwrap();
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
            let later = engine
                .clock()
                .now()
                .saturating_add(std::time::Duration::from_secs(31));
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

/// Starts the next activation and leaves it while its script runs, as a worker that dies there
/// does: nothing is committed, no failure is recorded, and the lease is held until it expires.
fn abandon_next_activation(engine: &TestEngine) {
    use std::future::Future;

    let mut activation = std::pin::pin!(engine.activate_next());
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        activation.as_mut().poll(&mut context).is_pending(),
        "the script runs when its worker dies"
    );
}

/// Moves the clock past the lease of the activation just abandoned.
fn after_the_lease(engine: &TestEngine) {
    let later = engine
        .clock()
        .now()
        .saturating_add(std::time::Duration::from_secs(31));
    engine.clock().advance_to(later);
}

#[tokio::test]
async fn a_run_whose_activations_never_finish_is_quarantined_before_its_script_runs_again() {
    let programs = FakeExecutor::new().with("poison", |run| run.hang(), |run, _| run.hang());
    let engine = engine_with(programs.clone(), RecordingDispatcher::echo(), replaying());
    let run = engine
        .start_run(request("poison", json!({})))
        .await
        .unwrap()
        .run;

    for attempt in 1..=5 {
        abandon_next_activation(&engine);
        assert_eq!(programs.starts(), attempt);
        let record = engine.get_run(&run).await.unwrap().unwrap();
        assert_eq!((record.attempts, record.failures), (attempt as u32, 0));
        assert_eq!(record.quarantine, None);
        assert_eq!(engine.activate_next().await.unwrap(), None, "still leased");
        after_the_lease(&engine);
    }

    // The sixth activation quarantines the run instead of running the script again.
    let report = engine.activate_next().await.unwrap().unwrap();
    let ActivationOutcome::Quarantined { kind, reason } = report.outcome else {
        panic!("{report:?}");
    };
    assert_eq!(kind, QuarantineKind::Failures);
    assert!(
        reason.starts_with("the activation of this run never finished 5 times in a row"),
        "{reason}"
    );
    assert_eq!(programs.starts(), 5, "the script did not run a sixth time");
    let record = engine.get_run(&run).await.unwrap().unwrap();
    assert_eq!(record.quarantine.unwrap().kind, QuarantineKind::Failures);
    assert_eq!(record.history_len, 0, "nothing was committed");
    assert_eq!(engine.activate_next().await.unwrap(), None, "never offered");

    // Given up, it ends with `internal` and the reason.
    assert!(engine.give_up(&run).await.unwrap());
    let history = engine.history(&run).await.unwrap();
    let Event::Lifecycle(Lifecycle::RunAborted(aborted)) = &history.last().unwrap().event else {
        panic!("{history:?}")
    };
    assert_eq!(aborted.kind, AbortKind::Internal);
    assert_eq!(aborted.message, reason);
}

#[tokio::test]
async fn activations_that_never_finish_and_failed_ones_count_together() {
    // The first start commits; then the replays hang in the delivery of the signal, except the
    // second, which cannot start.
    let starts = Arc::new(AtomicU32::new(0));
    let programs = FakeExecutor::new().with(
        "unstable",
        move |run| {
            if starts.fetch_add(1, Ordering::SeqCst) == 2 {
                run.refuse(ExecutorError::Internal(
                    "the isolate could not start".into(),
                ));
                return;
            }
            run.await_signal("go");
        },
        |run, _| run.hang(),
    );
    let engine = engine_with(programs.clone(), RecordingDispatcher::echo(), replaying());
    let run = engine
        .start_run(request("unstable", json!({})))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    engine.signal(&run, "go", json!(1), None).await.unwrap();

    abandon_next_activation(&engine);
    after_the_lease(&engine);
    let report = engine.activate_next().await.unwrap().unwrap();
    assert!(
        matches!(report.outcome, ActivationOutcome::Failed(_)),
        "{report:?}"
    );
    after_the_lease(&engine);
    for _ in 0..3 {
        abandon_next_activation(&engine);
        after_the_lease(&engine);
    }
    let record = engine.get_run(&run).await.unwrap().unwrap();
    assert_eq!((record.attempts, record.failures), (5, 1));
    assert_eq!(
        record.quarantine, None,
        "the fifth failure was not recorded"
    );

    let report = engine.activate_next().await.unwrap().unwrap();
    let ActivationOutcome::Quarantined { kind, reason } = report.outcome else {
        panic!("{report:?}");
    };
    assert_eq!(kind, QuarantineKind::Failures);
    assert!(
        reason
            .starts_with("5 activations of this run in a row committed nothing: 4 never finished"),
        "{reason}"
    );
    assert!(reason.ends_with("and 1 failed"), "{reason}");
    assert_eq!(programs.starts(), 6, "the start, then five replays");
}

#[tokio::test]
async fn a_commit_between_activations_that_never_finish_resets_their_count() {
    // The delivery of the signal hangs four times, then the script ends.
    let deliveries = Arc::new(AtomicU32::new(0));
    let programs = FakeExecutor::new().with(
        "recovers",
        |run| run.await_signal("go"),
        move |run, _| {
            if deliveries.fetch_add(1, Ordering::SeqCst) < 4 {
                run.hang();
            } else {
                run.complete(json!("done"));
            }
        },
    );
    let engine = engine_with(programs, RecordingDispatcher::echo(), replaying());
    let run = engine
        .start_run(request("recovers", json!({})))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    engine.signal(&run, "go", json!(1), None).await.unwrap();
    for _ in 0..4 {
        abandon_next_activation(&engine);
        after_the_lease(&engine);
    }
    let report = engine.activate_next().await.unwrap().unwrap();
    assert!(
        matches!(
            report.outcome,
            ActivationOutcome::Committed {
                status: RunStatus::Completed,
                ..
            }
        ),
        "{report:?}"
    );
    let record = engine.get_run(&run).await.unwrap().unwrap();
    assert_eq!((record.attempts, record.failures), (0, 0));
    assert_eq!(record.quarantine, None);
}

#[tokio::test]
async fn a_sticky_worker_that_dies_mid_activation_delays_the_others_by_its_lease_only() {
    let hangs = Arc::new(AtomicBool::new(true));
    let hanging = hangs.clone();
    let programs = FakeExecutor::new().with(
        "waits",
        |run| run.await_signal("go"),
        move |run, _| {
            if hanging.load(Ordering::SeqCst) {
                run.hang();
            } else {
                run.complete(json!("done"));
            }
        },
    );
    let mut config = EngineConfig::default();
    config.worker = "w1".into();
    let engine = engine_with(programs, RecordingDispatcher::echo(), config);
    let run = engine
        .start_run(request("waits", json!({})))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    let record = engine.get_run(&run).await.unwrap().unwrap();
    assert_eq!(record.sticky_worker.as_deref(), Some("w1"));

    // w1, the run's sticky worker, takes its next activation and dies during it.
    engine.signal(&run, "go", json!(1), None).await.unwrap();
    let signalled = engine.clock().now();
    abandon_next_activation(&engine);
    hangs.store(false, Ordering::SeqCst);

    // w2 takes the run over the moment the lease expires: the sticky priority of the dead
    // worker adds nothing to it.
    let mut config = EngineConfig::default();
    config.worker = "w2".into();
    let engine = ptc_engine::Engine::new(engine.into_adapters(), config);
    let expiry = signalled.saturating_add(engine.config().lease_duration);
    engine.clock().advance_to(ptc_core::Timestamp(expiry.0 - 1));
    assert_eq!(engine.activate_next().await.unwrap(), None, "still leased");
    engine.clock().advance_to(expiry);
    let report = engine.activate_next().await.unwrap().unwrap();
    assert!(
        matches!(
            report.outcome,
            ActivationOutcome::Committed {
                status: RunStatus::Completed,
                ..
            }
        ),
        "{report:?}"
    );
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
        let later = engine
            .clock()
            .now()
            .saturating_add(std::time::Duration::from_secs(31));
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

#[tokio::test]
async fn a_worker_never_retries_the_quarantined_runs_of_tenants_it_does_not_serve() {
    use std::collections::BTreeSet;

    use ptc_engine::ports::{TenantId, TenantScope};

    let broken = Arc::new(AtomicBool::new(true));
    let mut config = replaying();
    config.build = "build-1".into();
    let engine = engine_with(fragile(broken.clone()), RecordingDispatcher::echo(), config);
    let acme = TenantId::new("acme").unwrap();
    let mut started = request("fragile", json!({}));
    started.tenant = acme.clone();
    let run = engine.start_run(started).await.unwrap().run;
    drive(&engine).await;
    engine.signal(&run, "go", json!(1), None).await.unwrap();
    for _ in 0..5 {
        engine.activate_next().await.unwrap();
        let later = engine
            .clock()
            .now()
            .saturating_add(std::time::Duration::from_secs(31));
        engine.clock().advance_to(later);
    }
    let quarantine = engine.get_run(&run).await.unwrap().unwrap().quarantine;
    assert!(quarantine.is_some());
    broken.store(false, Ordering::SeqCst);

    // A worker of a new build in another tenant's pool: the run's script never runs there.
    let mut config = replaying();
    config.build = "build-2".into();
    config.tenants = TenantScope::Only(BTreeSet::from([TenantId::new("globex").unwrap()]));
    let engine = ptc_engine::Engine::new(engine.into_adapters(), config);
    let reports = engine.retry_quarantined().await.unwrap();
    assert!(reports.is_empty(), "{reports:?}");
    let record = engine.get_run(&run).await.unwrap().unwrap();
    let quarantine = record.quarantine.expect("still quarantined");
    assert_eq!(quarantine.retried_by, None, "build-2's retry is not spent");
    assert_eq!(record.status, RunStatus::Active);
    assert_eq!(engine.cached_instances(), 0, "no instance of acme's script");

    // A worker of the same build in acme's pool still retries it, once.
    let mut config = replaying();
    config.build = "build-2".into();
    config.tenants = TenantScope::Only(BTreeSet::from([acme]));
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
    let record = engine.get_run(&run).await.unwrap().unwrap();
    assert_eq!(record.quarantine, None);
}
