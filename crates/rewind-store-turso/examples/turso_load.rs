//! Measures the embedded store on a laptop-sized load: the latency of `commit_activation` and
//! the throughput of activations, driven the way the engine drives the store.
//!
//! Each run calls `calls` host tools in sequence, then completes. One activation is: acquire a
//! runnable run, take its inbox, commit the trigger with the next command (and its tool task),
//! release. Between activations, workers claim ready tool tasks and complete them, which wakes
//! the runs. Payloads are small and inline, as most are.
//!
//! ```text
//! cargo run --release -p rewind-store-turso --example turso_load -- [--runs 300] [--calls 10] [--workers 4]
//! ```

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rewind_core::{
    Command, Completion, Digest, Epoch, Event, HistoryEntry, Lifecycle, RunId, RunMode, RunStarted,
    RunStatus, Seed, Seq, Timestamp, ToolOutcome, ToolPolicy, ToolRoute,
};
use rewind_engine::ports::{
    AcquireRunnable, Activation, JournalStore, NewRun, NewToolTask, RunMetadata, RunRef,
    STICKY_GRACE, TaskFilter, TenantId, TenantScope, Trigger,
};
use rewind_store_turso::{MasterKey, TursoStore};
use serde_json::json;
use tokio::sync::Mutex;

const LEASE: Duration = Duration::from_secs(30);
const EPOCHS: &[Epoch] = &[Epoch(1)];

struct Options {
    runs: u64,
    calls: u32,
    workers: u64,
}

fn options() -> Options {
    let mut options = Options {
        runs: 300,
        calls: 10,
        workers: 4,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    for pair in args.chunks(2) {
        let value = || -> u64 {
            pair.get(1)
                .and_then(|value| value.parse().ok())
                .unwrap_or_else(|| panic!("{} takes a number", pair[0]))
        };
        match pair[0].as_str() {
            "--runs" => options.runs = value(),
            "--calls" => options.calls = u32::try_from(value()).expect("--calls fits in u32"),
            "--workers" => options.workers = value().max(1),
            other => panic!("unknown option {other}"),
        }
    }
    options
}

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_millis();
    Timestamp(u64::try_from(ms).expect("in range"))
}

fn started() -> RunStarted {
    RunStarted {
        script: Digest::of(b"load script"),
        toolset: Digest::of(b"load toolset"),
        input: json!({ "issues": [1, 2, 3] }).into(),
        seed: Seed::from_bytes([7; 32]),
        epoch: Epoch(1),
        expires_at: Timestamp(now().0 + 86_400_000),
        mode: RunMode::Live,
    }
}

fn host_task(seq: u32) -> NewToolTask {
    let route = ToolRoute::Host { group: None };
    NewToolTask {
        seq: Seq(seq),
        tool: "github.get_issue".into(),
        args: json!({ "repo": "acme/api", "number": seq }).into(),
        policy: ToolPolicy::default_for(&route),
        route,
        server_concurrency: None,
    }
}

#[derive(Default)]
struct Measures {
    /// Latency of each store call, by method.
    calls: BTreeMap<&'static str, Vec<Duration>>,
    activations: Vec<Duration>,
}

impl Measures {
    /// Times one store call.
    async fn time<T>(&mut self, method: &'static str, call: impl Future<Output = T>) -> T {
        let started_at = Instant::now();
        let output = call.await;
        self.calls
            .entry(method)
            .or_default()
            .push(started_at.elapsed());
        output
    }

    fn store_calls(&self) -> usize {
        self.calls.values().map(Vec::len).sum()
    }
}

/// One activation of a runnable run, if any. Returns whether there was one.
async fn activate(
    store: &TursoStore,
    worker: &str,
    calls: u32,
    lengths: &Mutex<HashMap<RunId, u64>>,
    measures: &mut Measures,
) -> bool {
    let started_at = Instant::now();
    let request = AcquireRunnable {
        owner: worker,
        epochs: EPOCHS,
        tenants: &TenantScope::All,
        now: now(),
        lease_duration: LEASE,
        sticky_grace: STICKY_GRACE,
        draw: 0,
    };
    let lease = measures
        .time("acquire", store.acquire_runnable(&request))
        .await
        .expect("acquire_runnable");
    let Some(lease) = lease else {
        return false;
    };
    let inbox = measures
        .time("take_inbox", store.take_inbox(&lease))
        .await
        .expect("take_inbox");
    let mut index = *lengths.lock().await.entry(lease.run.id).or_default();
    let mut activation = Activation::new(Vec::new(), Vec::new(), RunStatus::Active);
    for item in inbox {
        let at = now();
        activation.consumed.push(item.id);
        let (trigger, next) = match item.trigger {
            Trigger::RunStarted(started) => (Event::Lifecycle(Lifecycle::RunStarted(started)), 1),
            Trigger::Completion(completion) => {
                let next = match &completion {
                    Completion::ToolCallCompleted { seq, .. } => seq.0 + 1,
                    _ => unreachable!("the load only completes tool calls"),
                };
                (Event::Completion(completion), next)
            }
        };
        let response = if next <= calls {
            let task = host_task(next);
            let command = Command::ScheduleToolCall {
                seq: task.seq,
                tool: task.tool.clone(),
                args: task.args.clone(),
                callsite: None,
            };
            activation.tool_calls.push(task);
            command
        } else {
            activation.status = RunStatus::Completed;
            Command::CompleteRun {
                output: json!({ "closed": calls }).into(),
            }
        };
        for event in [trigger, Event::Command(response)] {
            activation.entries.push(HistoryEntry {
                index,
                recorded_at: at,
                event,
            });
            index += 1;
        }
    }
    measures
        .time("commit", store.commit_activation(&lease, activation, now()))
        .await
        .expect("commit_activation");
    lengths.lock().await.insert(lease.run.id, index);
    measures
        .time("release", store.release(&lease))
        .await
        .expect("release");
    measures.activations.push(started_at.elapsed());
    true
}

/// Claims ready tool tasks and completes them. Returns how many.
async fn answer_tools(store: &TursoStore, worker: &str, measures: &mut Measures) -> usize {
    let claimed = measures
        .time(
            "claim_tasks",
            store.claim_tool_tasks(worker, &TaskFilter::All, now(), 16),
        )
        .await
        .expect("claim_tool_tasks");
    for task in &claimed {
        let result = ToolOutcome::Result(json!({
            "number": task.task.task.seq.0,
            "title": "Crash on start",
            "labels": ["bug", "p1"],
        }));
        measures
            .time(
                "complete_task",
                store.complete_tool_task(&task.token, result, now()),
            )
            .await
            .expect("complete_tool_task");
    }
    claimed.len()
}

fn percentile(sorted: &[Duration], p: usize) -> Duration {
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn report(name: &str, mut values: Vec<Duration>) {
    values.sort_unstable();
    let total: Duration = values.iter().sum();
    let count = u32::try_from(values.len().max(1)).expect("in range");
    println!(
        "{name:<14} n={:<6} mean={:>9.3?} p50={:>9.3?} p95={:>9.3?} p99={:>9.3?} max={:>9.3?}",
        values.len(),
        total / count,
        percentile(&values, 50),
        percentile(&values, 95),
        percentile(&values, 99),
        values[values.len() - 1],
    );
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let options = options();
    let dir = tempfile::TempDir::new().expect("temporary directory");
    let path = dir.path().join("journal.db");
    let store = TursoStore::open(&path, MasterKey::generate().expect("master key"))
        .await
        .expect("open the store");

    let tenant = TenantId::local();
    for n in 0..options.runs {
        let run = RunRef::new(tenant.clone(), RunId::from_u128(u128::from(n) + 1));
        let new_run = NewRun {
            run,
            started: started(),
            metadata: RunMetadata::default(),
            created_at: now(),
        };
        store.create_run(new_run, None).await.expect("create_run");
    }

    let lengths = Arc::new(Mutex::new(HashMap::new()));
    let completed = Arc::new(AtomicU64::new(0));
    let expected = options.runs * (u64::from(options.calls) + 1);
    let started_at = Instant::now();
    let mut workers = Vec::new();
    for w in 0..options.workers {
        let store = store.clone();
        let lengths = Arc::clone(&lengths);
        let completed = Arc::clone(&completed);
        let calls = options.calls;
        workers.push(tokio::spawn(async move {
            let worker = format!("worker-{w}");
            let mut measures = Measures::default();
            while completed.load(Ordering::SeqCst) < expected {
                if activate(&store, &worker, calls, &lengths, &mut measures).await {
                    completed.fetch_add(1, Ordering::SeqCst);
                    continue;
                }
                if answer_tools(&store, &worker, &mut measures).await == 0 {
                    tokio::task::yield_now().await;
                }
            }
            measures
        }));
    }
    let mut all = Measures::default();
    for worker in workers {
        let measures = worker.await.expect("worker");
        for (method, latencies) in measures.calls {
            all.calls.entry(method).or_default().extend(latencies);
        }
        all.activations.extend(measures.activations);
    }
    let elapsed = started_at.elapsed();

    let size: u64 = std::fs::read_dir(dir.path())
        .expect("read the directory")
        .map(|file| file.expect("entry").metadata().expect("metadata").len())
        .sum();
    println!(
        "{} runs × {} tool calls, {} workers: {} activations in {:.2?}",
        options.runs,
        options.calls,
        options.workers,
        all.activations.len(),
        elapsed
    );
    println!(
        "throughput   {:.0} activations/s, {:.0} store calls/s",
        all.activations.len() as f64 / elapsed.as_secs_f64(),
        all.store_calls() as f64 / elapsed.as_secs_f64(),
    );
    report("activation", all.activations);
    for (method, latencies) in all.calls {
        report(method, latencies);
    }
    println!(
        "database     {:.1} MiB (journal.db and its WAL)",
        size as f64 / 1_048_576.0
    );
}
