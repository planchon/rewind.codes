//! The executors of a node, in the driver (`archi/03-execution.md`, "Sticky execution";
//! `archi/06-storage-and-scale.md`, "Quotas"): a run's instance kept between its activations, a
//! replay where none is kept, the eviction that bounds the cache, and the CPU time counted for
//! the calls into a script that execute, never those of a replay.

#![allow(clippy::unwrap_used)]

use crate::support;

use std::time::Duration;

use rewind_core::{RunStatus, Timestamp};
use rewind_engine::plane::NodeConfig;
use rewind_engine::ports::{Delivery, NamespaceName, Scope, TenantId, Window};
use serde_json::json;
use support::plane::{World, config};
use support::{DAY, FakeExecutor, RecordingDispatcher, T0, request, result_of};

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

/// A node that keeps at most `instances` instances.
fn keeping(instances: usize) -> NodeConfig {
    let mut node = config("node-a", 1);
    node.cache.max_instances = instances;
    node
}

#[tokio::test]
async fn a_kept_instance_is_used_again_without_a_replay() {
    let programs = three_calls();
    let mut world = World::new(programs.clone(), RecordingDispatcher::echo()).await;
    let run = world.run(request("three_calls", json!({}))).await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);
    // Four activations (the start and three results), one instance: no replay.
    assert_eq!(programs.starts(), 1);
    let node = world.driver.node(&world.at).unwrap();
    assert_eq!(node.cached_instances(), 0, "an ended run is not kept");
}

#[tokio::test]
async fn without_the_cache_every_activation_replays() {
    let programs = three_calls();
    let mut world = World::with(
        programs.clone(),
        RecordingDispatcher::echo(),
        1,
        &[keeping(0)],
    )
    .await;
    let run = world.run(request("three_calls", json!({}))).await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);
    // The start, then a replay for each of the three results.
    assert_eq!(programs.starts(), 4);
}

#[tokio::test]
async fn an_evicted_instance_is_replayed_at_its_next_activation() {
    let programs = FakeExecutor::new().with(
        "waits",
        |run| run.await_signal("go"),
        |run, _| run.complete(json!("done")),
    );
    let mut world = World::with(
        programs.clone(),
        RecordingDispatcher::echo(),
        1,
        &[keeping(1)],
    )
    .await;
    let first = world.run(request("waits", json!({}))).await;
    assert_eq!(world.driver.node(&world.at).unwrap().cached_instances(), 1);
    // A second run takes the only place: the first run's instance is evicted.
    let second = world.run(request("waits", json!({}))).await;
    assert_eq!(world.driver.node(&world.at).unwrap().cached_instances(), 1);
    assert_eq!(programs.starts(), 2);
    // The first run is replayed at its next activation; then the second is replayed in turn.
    world.signal(&first, "go", json!(null), None).await.unwrap();
    world.idle().await;
    assert_eq!(programs.starts(), 3);
    world
        .signal(&second, "go", json!(null), None)
        .await
        .unwrap();
    world.idle().await;
    assert_eq!(world.view(&first).await.head.status, RunStatus::Completed);
    assert_eq!(world.view(&second).await.head.status, RunStatus::Completed);
}

#[tokio::test]
async fn cpu_time_counts_the_calls_that_execute_not_those_of_a_replay() {
    // No instance cache: every activation replays the run's history first.
    let programs = steps();
    let mut world = World::with(
        programs.clone(),
        RecordingDispatcher::echo(),
        1,
        &[keeping(0)],
    )
    .await;
    let alice = NamespaceName::new("alice").unwrap();
    let mut start = request("steps", json!({}));
    start.namespace = alice.clone();
    let run = world.run(start).await;
    assert_eq!(world.view(&run).await.head.status, RunStatus::Completed);
    // Three activations replayed 5, then 5 + 7 ms: none of it counts.
    assert!(programs.starts() >= 3);
    let window = Window::covering(T0, Timestamp(T0.0 + DAY));
    let mut gateway = world.driver.gateway(&world.at.clone());
    for scope in [
        Scope::Namespace {
            tenant: TenantId::local(),
            namespace: alice.clone(),
        },
        Scope::Tenant(TenantId::local()),
    ] {
        let (counters, _) = world
            .edge
            .statistics(&mut gateway, &scope, window)
            .await
            .unwrap();
        assert_eq!(counters.cpu, Duration::from_millis(23));
        assert_eq!(counters.created, 1);
    }
}
