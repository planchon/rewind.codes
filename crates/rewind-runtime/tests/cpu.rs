//! The CPU time of each call into the script, as the V8 executor reports it: the engine counts
//! the calls that execute toward the namespace's `cpu_per_month`
//! (`archi/06-storage-and-scale.md`, "Quotas").

#![allow(clippy::unwrap_used)]

use std::time::Duration;

use rewind_core::{Epoch, RunId, Script, Seed, Timestamp, Toolset};
use rewind_engine::ports::{Delivery, ExecutorError, ScriptExecutor, ScriptInstance, StartScript};
use rewind_runtime::{SandboxLimits, V8Executor};
use serde_json::json;

const T0: Timestamp = Timestamp(1_767_225_600_000); // 2026-01-01T00:00:00Z

/// Spins `input` milliseconds of work at start, then as many as each `spin` signal says.
const SPINNER: &str = r#"
import { signal } from "rewind";

function spin(iterations: number) {
  let x = 0;
  for (let i = 0; i < iterations; i++) x = (x + i) % 1_000_003;
  return x;
}

export default async function main(input: number) {
  spin(input);
  for (;;) {
    const iterations = await signal<number>("spin");
    spin(iterations);
  }
}
"#;

fn start(executor: &V8Executor, input: u64) -> StartScript {
    StartScript {
        run_id: RunId::from_u128(1),
        compiled: executor
            .compile(&Script::new(SPINNER), &Toolset::default())
            .unwrap(),
        toolset: Toolset::default(),
        input: json!(input),
        seed: Seed::from_bytes([0; 32]),
        epoch: Epoch(1),
        started_at: T0,
    }
}

fn spin(iterations: u64) -> Delivery {
    Delivery::SignalReceived {
        name: "spin".into(),
        payload: json!(iterations),
    }
}

#[tokio::test]
async fn each_call_reports_the_cpu_time_it_used() {
    let executor = V8Executor::new(SandboxLimits::default()).unwrap();
    let (mut instance, _) = executor.start(start(&executor, 0)).await.unwrap();
    // Loading the script and `main(input)` take some CPU time, however little.
    assert!(instance.cpu_time() > Duration::ZERO);

    instance.deliver(spin(50_000_000), T0).await.unwrap();
    let heavy = instance.cpu_time();
    instance.deliver(spin(0), T0).await.unwrap();
    let light = instance.cpu_time();
    // Each call reports its own CPU time, not a running total.
    assert!(
        heavy > light * 10,
        "{heavy:?} for a long call, {light:?} for a short one"
    );
    assert!(heavy >= Duration::from_millis(5), "{heavy:?}");
}

#[tokio::test]
async fn a_call_stopped_by_its_cpu_budget_reports_what_it_used() {
    let limits = SandboxLimits {
        cpu_per_trigger: Duration::from_millis(100),
        ..SandboxLimits::default()
    };
    let executor = V8Executor::new(limits).unwrap();
    let (mut instance, _) = executor.start(start(&executor, 0)).await.unwrap();
    let stopped = instance
        .deliver(spin(u64::from(u32::MAX) * 1_000), T0)
        .await;
    assert!(matches!(stopped, Err(ExecutorError::LimitExceeded(_))));
    let used = instance.cpu_time();
    assert!(used >= Duration::from_millis(100), "{used:?}");
    assert!(used < Duration::from_secs(5), "{used:?}");
}
