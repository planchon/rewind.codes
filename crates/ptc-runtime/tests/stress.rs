//! The lifecycle of isolates, many at a time: the snapshot built at the first one, starts,
//! deliveries, ends by a limit, drops on their threads. On macOS the same tests run again in a
//! child process under Guard Malloc, where reading or freeing memory that is not allocated
//! faults at once: the `ptc` binary aborted, now and then, when V8 freed an array buffer while
//! it disposed an isolate ("pointer being freed was not allocated"), and Guard Malloc showed V8
//! reading freed memory while it built the snapshot (`archi/08-decisions.md`).

#![allow(clippy::unwrap_used)]

use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use ptc_core::{Epoch, RunId, Script, Seed, Seq, Timestamp, Toolset};
use ptc_engine::ports::{Delivery, ScriptExecutor, ScriptInstance, StartScript};
use ptc_runtime::{SandboxLimits, V8Executor};
use serde_json::json;

const T0: Timestamp = Timestamp(1_767_225_600_000);

/// Uses what makes array buffers, in V8 and from Rust, then waits for a timer.
const SCRIPT: &str = r#"
import { sleep } from "ptc";
export default async function main(input: number) {
  const text = new TextEncoder().encode(`run ${input}`);
  const copy = structuredClone({ text, at: Date.now(), id: crypto.randomUUID() });
  const growable = new ArrayBuffer(8, { maxByteLength: 1024 });
  growable.resize(64);
  await sleep("1s");
  return [copy.text.length, growable.transfer().byteLength, new URL(`https://example.com/${input}`).pathname];
}
"#;

const LIMITED: [&str; 4] = [
    "export default async function main() { const a = []; for (;;) a.push(new Array(1e5).fill(1)); }",
    "export default async function main() { for (;;) {} }",
    "export default async function main() { const line = \"x\".repeat(1 << 20); for (let i = 0; i < 100; i++) console.log(line); }",
    "export default async function main() { return new ArrayBuffer(2 ** 36).byteLength; }",
];

fn rounds(default: u64) -> u64 {
    std::env::var("PTC_STRESS_ROUNDS")
        .ok()
        .and_then(|rounds| rounds.parse().ok())
        .unwrap_or(default)
}

fn start(executor: &V8Executor, n: u64, source: &str) -> StartScript {
    StartScript {
        run_id: RunId::from_u128(u128::from(n) + 1),
        compiled: executor
            .compile(&Script::new(source), &Toolset::default())
            .unwrap(),
        toolset: Toolset::default(),
        input: json!(n),
        seed: Seed::from_bytes([1; 32]),
        epoch: Epoch(1),
        started_at: T0,
    }
}

/// Runs `each` for `rounds` rounds on 8 tasks at once.
async fn at_once<F, Fut>(rounds: u64, each: F)
where
    F: Fn(u64, u64) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let each = Arc::new(each);
    let mut tasks = Vec::new();
    for worker in 0..8 {
        let each = Arc::clone(&each);
        tasks.push(tokio::spawn(async move {
            for round in 0..rounds {
                each(worker, round).await;
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn lifecycle_of_many_instances_at_once() {
    let executor = Arc::new(V8Executor::new(SandboxLimits::default()).unwrap());
    at_once(rounds(25), move |worker, round| {
        let executor = Arc::clone(&executor);
        async move {
            let n = worker * 1000 + round;
            let (mut instance, emitted) =
                executor.start(start(&executor, n, SCRIPT)).await.unwrap();
            assert_eq!(emitted.len(), 1, "{emitted:?}");
            let emitted = instance
                .deliver(Delivery::TimerFired { seq: Seq::FIRST }, T0)
                .await
                .unwrap();
            assert_eq!(emitted.len(), 1, "{emitted:?}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn lifecycle_of_instances_ended_by_a_limit() {
    let executor = Arc::new(
        V8Executor::new(SandboxLimits {
            heap_bytes: 64 * 1024 * 1024,
            cpu_per_trigger: Duration::from_millis(300),
            ..SandboxLimits::default()
        })
        .unwrap(),
    );
    at_once(rounds(4), move |worker, round| {
        let executor = Arc::clone(&executor);
        async move {
            let source = LIMITED[usize::try_from((worker + round) % 4).unwrap()];
            // Each ends: by its heap, its CPU time, what it emits, or a RangeError.
            let _ = executor
                .start(start(&executor, worker * 1000 + round, source))
                .await;
        }
    })
    .await;
}

/// The tests above, in a child process under Guard Malloc. Without it, reading freed memory
/// goes unnoticed, and a corrupted heap shows only later, elsewhere, now and then.
#[test]
fn guard_malloc_finds_no_memory_error_in_the_lifecycle() {
    const GUARD_MALLOC: &str = "/usr/lib/libgmalloc.dylib";
    if !cfg!(target_os = "macos") || !std::path::Path::new(GUARD_MALLOC).exists() {
        eprintln!("Guard Malloc is a macOS library: skipped");
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["lifecycle_of_", "--test-threads", "1"])
        .env("DYLD_INSERT_LIBRARIES", GUARD_MALLOC)
        .env("MALLOC_PROTECT_BEFORE", "1")
        .env("PTC_STRESS_ROUNDS", "2")
        .output()
        .unwrap();
    let report = |stream: &[u8]| {
        String::from_utf8_lossy(stream)
            .lines()
            .filter(|line| !line.starts_with("GuardMalloc["))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        output.status.success(),
        "the lifecycle faulted under Guard Malloc ({}):\n{}\n{}",
        output.status,
        report(&output.stdout),
        report(&output.stderr)
    );
    assert!(
        report(&output.stdout).contains("2 passed"),
        "{}",
        report(&output.stdout)
    );
}
