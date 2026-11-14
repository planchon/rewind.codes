//! What the worker role does for quotas and erased namespaces: the CPU time of the calls that
//! execute, counted per namespace and day and written by `flush_usage`; quota alerts sent as
//! `quota.alert`; the erasure of a namespace in the retention jobs
//! (`archi/06-storage-and-scale.md`, "Quotas", "Retention and deletion").

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rewind_core::{RunStatus, Timestamp};
use rewind_engine::adapters::MemoryReadModel;
use rewind_engine::ports::{
    Clock, Credentials, Day, DeleteNamespace, Delivery, JournalStore, Month, NamespaceName,
    QuotaKind, Quotas, RunFilter, Secret, SecretStore, Subscription, Tenant, TenantId, Threshold,
    UsageOf, WebhookEventKind, WebhookEventType,
};
use rewind_engine::quotas::QuotaAlerts;
use rewind_engine::retention::Housekeeper;
use rewind_engine::{DriveOptions, EngineConfig, StartRun};
use serde_json::json;
use support::*;

/// Calls `a`, then `b`, then completes: three calls into the script, of 5, 7 and 11 ms.
fn steps() -> FakeExecutor {
    FakeExecutor::new().with(
        "steps",
        |run| {
            run.spend(Duration::from_millis(5));
            run.call("a", json!(1));
        },
        |run, delivery| match delivery {
            Delivery::ToolCallCompleted { seq, .. } if seq.0 == 1 => {
                run.spend(Duration::from_millis(7));
                run.call("b", json!(2));
            }
            Delivery::ToolCallCompleted { .. } => {
                run.spend(Duration::from_millis(11));
                run.complete(json!("done"));
            }
            other => panic!("unexpected {other:?}"),
        },
    )
}

fn namespace(name: &str) -> NamespaceName {
    NamespaceName::new(name).unwrap()
}

fn in_namespace(name: &str, program: &str) -> StartRun {
    StartRun {
        namespace: namespace(name),
        ..request(program, json!({}))
    }
}

async fn drive(engine: &TestEngine) {
    let report = engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
}

fn local() -> TenantId {
    TenantId::local()
}

#[tokio::test]
async fn cpu_time_counts_the_calls_that_execute_not_those_of_a_replay() {
    // No instance cache: every activation replays the run's history first.
    let engine = engine_with(steps(), RecordingDispatcher::echo(), replaying());
    let run = engine
        .start_run(in_namespace("alice", "steps"))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    assert_eq!(
        engine.get_run(&run).await.unwrap().unwrap().status,
        RunStatus::Completed
    );
    // Three activations replayed 5, then 5 + 7 ms: none of it counts.
    assert!(engine.executor().starts() >= 3);
    let day = Day::of(engine.clock().now());
    let unwritten = engine.unwritten_usage();
    assert_eq!(unwritten.len(), 1);
    assert_eq!(unwritten[0].namespace, namespace("alice"));
    assert_eq!(unwritten[0].day, day);
    assert_eq!(unwritten[0].cpu, Duration::from_millis(23));

    assert!(engine.flush_usage().await.unwrap().is_empty());
    assert!(engine.unwritten_usage().is_empty());
    let month = Month::of(engine.clock().now());
    for of in [UsageOf::Namespace(namespace("alice")), UsageOf::Tenant] {
        let usage = engine
            .journal()
            .namespace_usage(&local(), &of, month)
            .await
            .unwrap();
        assert_eq!(usage.cpu, Duration::from_millis(23));
        assert_eq!(usage.runs_started, 1);
    }
    // Nothing more to write.
    assert!(engine.flush_usage().await.unwrap().is_empty());
    let usage = engine
        .journal()
        .namespace_usage(&local(), &UsageOf::Tenant, month)
        .await
        .unwrap();
    assert_eq!(usage.cpu, Duration::from_millis(23));
}

#[tokio::test]
async fn cpu_time_below_a_millisecond_waits_for_the_next_write() {
    let engine = engine(
        FakeExecutor::new().with(
            "light",
            |run| {
                run.spend(Duration::from_micros(1_500));
                run.timer(1_000);
            },
            |run, _| {
                run.spend(Duration::from_micros(700));
                run.complete(json!(null));
            },
        ),
        RecordingDispatcher::echo(),
    );
    engine
        .start_run(in_namespace("alice", "light"))
        .await
        .unwrap();
    engine
        .run_until_idle(&DriveOptions::default())
        .await
        .unwrap();
    engine.flush_usage().await.unwrap();
    // 1 ms written, 0.5 ms kept.
    assert_eq!(engine.unwritten_usage()[0].cpu, Duration::from_micros(500));
    drive(&engine).await;
    engine.flush_usage().await.unwrap();
    // 0.5 + 0.7 ms: 1 ms more written, 0.2 ms kept.
    let month = Month::of(engine.clock().now());
    let usage = engine
        .journal()
        .namespace_usage(&local(), &UsageOf::Namespace(namespace("alice")), month)
        .await
        .unwrap();
    assert_eq!(usage.cpu, Duration::from_millis(2));
    assert_eq!(engine.unwritten_usage()[0].cpu, Duration::from_micros(200));
}

#[tokio::test]
async fn cpu_time_is_counted_on_the_day_it_is_spent() {
    let engine = engine(
        FakeExecutor::new().with(
            "overnight",
            |run| {
                run.timer(DAY);
            },
            |run, _| run.complete(json!(null)),
        ),
        RecordingDispatcher::echo(),
    );
    engine
        .start_run(in_namespace("alice", "overnight"))
        .await
        .unwrap();
    drive(&engine).await;
    let mut days: Vec<(Day, Duration)> = engine
        .unwritten_usage()
        .into_iter()
        .map(|delta| (delta.day, delta.cpu))
        .collect();
    days.sort();
    assert_eq!(
        days,
        vec![
            (Day::of(T0), CALL_CPU),
            (Day::of(Timestamp(T0.0 + DAY)), CALL_CPU)
        ]
    );
}

fn subscription(id: &str, events: &[WebhookEventType], labels: &[(&str, &str)]) -> Subscription {
    Subscription {
        tenant: local(),
        id: id.to_owned(),
        url: format!("memory://{id}"),
        secret: Secret::new("whsec_test"),
        events: events.iter().copied().collect::<BTreeSet<_>>(),
        labels: labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
    }
}

#[tokio::test]
async fn cpu_alerts_are_raised_by_the_write_and_sent_once_to_who_takes_them() {
    let engine = engine(steps(), RecordingDispatcher::echo());
    let mut tenant = Tenant::new(local());
    tenant.namespace_defaults = Quotas {
        cpu_per_month: Some(Duration::from_millis(20)),
        ..Quotas::default()
    };
    engine.journal().put_tenant(&tenant).await.unwrap();
    for subscription in [
        subscription("alerts", &[WebhookEventType::QuotaAlert], &[]),
        subscription("everything", &[], &[]),
        subscription("runs", &[WebhookEventType::RunCompleted], &[]),
        // An alert carries no labels: a subscription limited to labels does not get it.
        subscription("labelled", &[], &[("agent", "triage")]),
        subscription("down", &[WebhookEventType::QuotaAlert], &[]),
    ] {
        engine
            .journal()
            .put_subscription(&subscription)
            .await
            .unwrap();
    }
    engine.webhooks().refuse("memory://down");
    engine
        .start_run(in_namespace("alice", "steps"))
        .await
        .unwrap();
    drive(&engine).await;

    // 23 ms of 20: 80 % and 100 % crossed, by the write.
    let raised = engine.flush_usage().await.unwrap();
    let thresholds: Vec<_> = raised.iter().map(|alert| alert.key.threshold).collect();
    assert_eq!(thresholds, vec![Threshold::Near, Threshold::Reached]);
    assert!(raised.iter().all(|alert| {
        alert.key.quota == QuotaKind::CpuPerMonth
            && alert.key.namespace == Some(namespace("alice"))
            && alert.limit == 20
            && alert.used == 23
    }));

    let sender = QuotaAlerts {
        journal: engine.journal(),
        webhooks: engine.webhooks(),
        owner: "worker",
        lease_duration: Duration::from_secs(60),
        batch: 10,
    };
    let now = engine.clock().now();
    assert_eq!(sender.send(now).await.unwrap(), 2);
    let mut delivered: Vec<(String, String)> = engine
        .webhooks()
        .deliveries()
        .into_iter()
        .filter(|(_, event)| matches!(event.kind, WebhookEventKind::QuotaAlert(_)))
        .map(|(subscription, event)| {
            assert_eq!(event.namespace, Some(namespace("alice")));
            assert_eq!(event.run, None);
            (subscription.id, event.id)
        })
        .collect();
    delivered.sort();
    let month = Month::of(now);
    let near = format!("quota.alert:alice:cpu_per_month:{month}:80");
    let reached = format!("quota.alert:alice:cpu_per_month:{month}:100");
    assert_eq!(
        delivered,
        vec![
            ("alerts".to_owned(), reached.clone()),
            ("alerts".to_owned(), near.clone()),
            ("everything".to_owned(), reached),
            ("everything".to_owned(), near),
        ]
    );
    // Acknowledged, the refused delivery too: nothing is sent twice.
    assert_eq!(sender.send(now).await.unwrap(), 0);
    let alerts = engine
        .journal()
        .quota_alerts(&local(), Some(&namespace("alice")), month)
        .await
        .unwrap();
    assert_eq!(alerts.len(), 2);
    assert!(alerts.iter().all(|alert| alert.sent_at == Some(now)));

    // Once per month and threshold: more CPU time raises nothing.
    engine
        .start_run(in_namespace("alice", "steps"))
        .await
        .unwrap();
    drive(&engine).await;
    assert!(engine.flush_usage().await.unwrap().is_empty());
}

/// Waits for the signal `go`, and ends cancelled when cancelled.
fn waiter() -> FakeExecutor {
    steps().with(
        "waiter",
        |run| run.await_signal("go"),
        |run, delivery| match delivery {
            Delivery::SignalReceived { .. } => run.complete(json!("went")),
            Delivery::CancelRequested { .. } => run.fail("CancelledError", "run cancelled"),
            other => panic!("unexpected {other:?}"),
        },
    )
}

fn credentials() -> Credentials {
    Credentials {
        servers: BTreeMap::new(),
    }
}

#[tokio::test]
async fn an_erased_namespace_has_its_runs_cancelled_then_erased() {
    let engine = engine_with(
        waiter(),
        RecordingDispatcher::echo(),
        EngineConfig::default(),
    );
    let read_model = MemoryReadModel::new();
    let housekeeper = Housekeeper {
        journal: engine.journal(),
        blobs: engine.blobs(),
        read_model: &read_model,
        secrets: engine.secrets(),
        batch: 1,
    };
    let toolset = request("waiter", json!({})).toolset;
    let waiting = engine
        .start_run(in_namespace("alice", "waiter"))
        .await
        .unwrap()
        .run;
    let ended = engine
        .start_run(in_namespace("alice", "steps"))
        .await
        .unwrap()
        .run;
    let bobs = engine
        .start_run(in_namespace("bob", "waiter"))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    engine.flush_usage().await.unwrap();
    for name in ["alice", "bob"] {
        engine
            .set_namespace_credentials(&local(), &namespace(name), toolset.digest(), &credentials())
            .await
            .unwrap();
    }
    let month = Month::of(engine.clock().now());
    let tenant_cpu = engine
        .journal()
        .namespace_usage(&local(), &UsageOf::Tenant, month)
        .await
        .unwrap()
        .cpu;
    assert!(tenant_cpu > Duration::ZERO);

    // Nothing to erase while no namespace is marked.
    let now = engine.clock().now();
    assert_eq!(
        housekeeper.erase_namespaces(now).await.unwrap().namespaces,
        0
    );
    assert_eq!(
        engine
            .journal()
            .delete_namespace(&local(), &namespace("alice"), now)
            .await
            .unwrap(),
        DeleteNamespace::Started
    );
    // The first round erases the run that ended and cancels the other, once.
    let erased = housekeeper.erase_namespaces(now).await.unwrap();
    assert_eq!(
        (erased.runs, erased.cancelled, erased.namespaces),
        (1, 1, 0)
    );
    assert!(engine.get_run(&ended).await.unwrap().is_none());
    let erased = housekeeper.erase_namespaces(now).await.unwrap();
    assert_eq!(
        (erased.runs, erased.cancelled, erased.namespaces),
        (0, 0, 0)
    );

    // The cancellation goes through the normal path: the script is told, and ends.
    drive(&engine).await;
    assert!(
        engine
            .get_run(&waiting)
            .await
            .unwrap()
            .unwrap()
            .status
            .is_terminal()
    );
    let ended_at = engine.clock().now();
    let erased = housekeeper.erase_namespaces(ended_at).await.unwrap();
    assert_eq!((erased.runs, erased.namespaces), (1, 1));
    assert!(engine.get_run(&waiting).await.unwrap().is_none());
    assert!(
        engine
            .journal()
            .get_namespace(&local(), &namespace("alice"))
            .await
            .unwrap()
            .is_none()
    );
    // Its credentials are gone; Bob's stay.
    assert_eq!(
        engine
            .secrets()
            .resolve(&local(), waiting.id, &namespace("alice"), toolset.digest())
            .await
            .unwrap(),
        None
    );
    assert!(
        engine
            .secrets()
            .resolve(&local(), bobs.id, &namespace("bob"), toolset.digest())
            .await
            .unwrap()
            .is_some()
    );
    // Its use is gone; the tenant's own use keeps what it consumed.
    let alice = engine
        .journal()
        .namespace_usage(&local(), &UsageOf::Namespace(namespace("alice")), month)
        .await
        .unwrap();
    assert_eq!((alice.cpu, alice.runs_started), (Duration::ZERO, 0));
    let tenant = engine
        .journal()
        .namespace_usage(&local(), &UsageOf::Tenant, month)
        .await
        .unwrap();
    assert_eq!(tenant.cpu, tenant_cpu);
    assert_eq!(tenant.runs_started, 3);
    // Bob's run is untouched.
    assert!(
        !engine
            .get_run(&bobs)
            .await
            .unwrap()
            .unwrap()
            .status
            .is_terminal()
    );
    let runs = engine
        .journal()
        .list_runs(&RunFilter::new(local()))
        .await
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run, bobs);
}
