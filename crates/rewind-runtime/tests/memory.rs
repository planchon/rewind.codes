//! The memory an instance holds, as the V8 executor measures it, and the sticky cache's bound on
//! it (`archi/03-execution.md`, "Sticky execution").

#![allow(clippy::unwrap_used)]

use rewind_core::{ActionSet, Code, Epoch, RunId, RunStatus, Seed};
use rewind_engine::adapters::StaticDispatcher;
use rewind_engine::ports::{CodeExecutor, CodeInstance, Delivery, RunRef, StartCode};
use rewind_engine::{CacheConfig, StartRun};
use rewind_runtime::{SandboxLimits, V8Executor};
use serde_json::json;

mod support;

use support::{T0, World};

const MIB: u64 = 1024 * 1024;

/// Keeps an array of `input` doubles (8 bytes each on the V8 heap), then grows it by the size
/// each `grow` signal carries, until `done`.
const HOARDER: &str = r#"
import { signal } from "rewind";

const kept: number[][] = [];

function hoard(size: number) {
  kept.push(Array.from({ length: size }, (_, i) => i + 0.5));
}

export default async function main(input: number) {
  hoard(input);
  for (;;) {
    const size = await signal<number | null>("grow");
    if (size === null) return kept.length;
    hoard(size);
  }
}
"#;

fn executor() -> V8Executor {
    V8Executor::new(SandboxLimits::default()).unwrap()
}

fn start(executor: &V8Executor, input: u64) -> StartCode {
    StartCode {
        run_id: RunId::from_u128(1),
        compiled: executor
            .compile(&Code::new(HOARDER), &ActionSet::default())
            .unwrap(),
        action_set: ActionSet::default(),
        input: json!(input),
        seed: Seed::from_bytes([0; 32]),
        epoch: Epoch(1),
        started_at: T0,
    }
}

#[tokio::test]
async fn an_instance_reports_the_memory_its_isolate_holds() {
    let executor = executor();
    let (mut instance, _) = executor.start(start(&executor, 0)).await.unwrap();
    let at_start = instance.memory_usage();
    assert!(at_start > 0, "an isolate holds memory from its start");

    // 2M doubles: 16 MiB more on the heap, measured after the delivery.
    instance
        .deliver(
            Delivery::SignalReceived {
                name: "grow".into(),
                payload: json!(2_000_000),
            },
            T0,
        )
        .await
        .unwrap();
    let grown = instance.memory_usage();
    assert!(
        grown >= at_start + 16 * MIB,
        "{at_start} bytes at start, {grown} after keeping 16 MiB"
    );
}

/// Starts three runs that each keep 16 MiB and wait for a signal, then ends them. Returns how
/// many instances the node's executors kept once the three were waiting.
async fn three_hoarders(cache: CacheConfig) -> usize {
    let world = World::new(executor(), StaticDispatcher::new(), 1, |config| {
        config.cache = cache;
    })
    .await;
    let mut runs: Vec<RunRef> = Vec::new();
    for _ in 0..3 {
        let request = StartRun::new(Code::new(HOARDER), ActionSet::default(), json!(2_000_000));
        runs.push(world.start(request).await.unwrap());
        world.idle().await;
    }
    let cached = world.cached_instances().await;

    // An evicted run replays its history on its next activation, and goes on: no activation
    // fails (`idle` checks it).
    for run in &runs {
        world.signal(run, "grow", json!(null)).await.unwrap();
    }
    world.idle().await;
    for run in &runs {
        assert_eq!(world.view(run).await.head.status, RunStatus::Completed);
    }
    cached
}

#[tokio::test]
async fn the_sticky_cache_evicts_instances_past_its_memory_bound() {
    // What one waiting run holds, measured the way the cache measures it.
    let executor = executor();
    let (instance, _) = executor.start(start(&executor, 2_000_000)).await.unwrap();
    let one = instance.memory_usage();
    assert!(one >= 16 * MIB, "{one} bytes");
    drop(instance);

    let unbounded = CacheConfig {
        max_instances: 10,
        max_bytes: u64::MAX,
    };
    assert_eq!(three_hoarders(unbounded).await, 3);

    // Room for two and a half: the least recently used instance goes.
    let bounded = CacheConfig {
        max_bytes: one * 5 / 2,
        ..unbounded
    };
    assert_eq!(three_hoarders(bounded).await, 2);
}
