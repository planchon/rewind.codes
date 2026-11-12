//! The memory an instance holds, as the V8 executor measures it, and the sticky cache's bound on
//! it (`archi/03-execution.md`, "Sticky execution").

#![allow(clippy::unwrap_used)]

use ptc_core::{Epoch, RunId, RunStatus, Script, Seed, Timestamp, Toolset};
use ptc_engine::adapters::{
    MemoryBlobs, MemoryJournal, MemorySecrets, MemoryWebhooks, SeededEntropy, StaticDispatcher,
    VirtualClock,
};
use ptc_engine::ports::{Delivery, RunRef, ScriptExecutor, ScriptInstance, StartScript};
use ptc_engine::{Adapters, CacheConfig, DriveOptions, Engine, EngineConfig, StartRun};
use ptc_runtime::{SandboxLimits, V8Executor};
use serde_json::json;

const T0: Timestamp = Timestamp(1_767_225_600_000); // 2026-01-01T00:00:00Z
const MIB: u64 = 1024 * 1024;

/// Keeps an array of `input` doubles (8 bytes each on the V8 heap), then grows it by the size
/// each `grow` signal carries, until `done`.
const HOARDER: &str = r#"
import { signal } from "ptc";

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

fn start(executor: &V8Executor, input: u64) -> StartScript {
    StartScript {
        run_id: RunId::from_u128(1),
        compiled: executor
            .compile(&Script::new(HOARDER), &Toolset::default())
            .unwrap(),
        toolset: Toolset::default(),
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

type V8Engine = Engine<
    MemoryJournal,
    MemoryBlobs,
    V8Executor,
    StaticDispatcher,
    VirtualClock,
    SeededEntropy,
    MemorySecrets,
    MemoryWebhooks,
>;

fn engine(cache: CacheConfig) -> V8Engine {
    Engine::new(
        Adapters {
            journal: MemoryJournal::new(),
            blobs: MemoryBlobs::new(),
            executor: executor(),
            dispatcher: StaticDispatcher::new(),
            clock: VirtualClock::new(T0),
            entropy: SeededEntropy::new(1),
            secrets: MemorySecrets::new(),
            webhooks: MemoryWebhooks::new(),
        },
        EngineConfig {
            cache,
            ..EngineConfig::default()
        },
    )
}

/// Starts three runs that each keep 16 MiB and wait for a signal, then ends them. Returns how
/// many instances the cache kept once the three were waiting.
async fn three_hoarders(cache: CacheConfig) -> usize {
    let engine = engine(cache);
    let mut runs: Vec<RunRef> = Vec::new();
    for _ in 0..3 {
        let request = StartRun::new(Script::new(HOARDER), Toolset::default(), json!(2_000_000));
        runs.push(engine.start_run(request).await.unwrap().run);
        engine
            .run_until_idle_virtual(&DriveOptions::default())
            .await
            .unwrap();
    }
    let cached = engine.cached_instances();

    // An evicted run replays its history on its next activation, and goes on.
    for run in &runs {
        engine.signal(run, "grow", json!(null), None).await.unwrap();
    }
    let report = engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    for run in &runs {
        let record = engine.get_run(run).await.unwrap().unwrap();
        assert_eq!(record.status, RunStatus::Completed);
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
