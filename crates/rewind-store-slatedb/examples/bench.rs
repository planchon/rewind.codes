//! The partition benchmark on an S3-compatible bucket (`archi/08-decisions.md`, W4): commit
//! latency for batches of 1, 10 and 100 runs' activations, a cold history read, the takeover of a
//! partition from a writer that is still running, and the object-store requests SlateDB makes
//! while nothing happens.
//!
//! ```text
//! REWIND_BENCH_S3_ENDPOINT=https://<account>.r2.cloudflarestorage.com \
//! REWIND_BENCH_S3_BUCKET=rewind-bench \
//! REWIND_BENCH_S3_ACCESS_KEY_ID=… REWIND_BENCH_S3_SECRET_ACCESS_KEY=… \
//! cargo run --release -p rewind-store-slatedb --example bench
//! ```
//!
//! Optional: `REWIND_BENCH_S3_REGION` (`auto`), `REWIND_BENCH_S3_ALLOW_HTTP=1` (MinIO),
//! `REWIND_BENCH_ROOT` (`bench/<unix ms>`, the prefix the run writes under), `REWIND_BENCH_ROUNDS`
//! (batches per size, 50), `REWIND_BENCH_IDLE_SECONDS` (60), `REWIND_BENCH_COMPARE_DEFAULTS=1`
//! (also counts the idle requests of SlateDB's own default timers). The objects stay in the bucket.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, RenameOptions,
};
use rewind_core::{
    ActionPolicy, ActionRoute, Command, Completion, Digest, Epoch, Event, HistoryEntry, Lifecycle,
    PartitionId, RunId, RunMode, RunStarted, RunStatus, Seed, Seq, Timestamp,
};
use rewind_engine::ports::partition::{Activation, CreateRun};
use rewind_engine::ports::{
    Attempt, Batch, CallTask, Change, CounterDelta, Counters, DueTimer, Hour, InboxItem,
    IndexedSummary, NamespaceName, NodeId, PartitionError, PartitionLog, PartitionStore, RunCounts,
    RunHead, RunMetadata, RunRef, RunSummary, TaskRecord, TaskRef, TaskState, TenantId,
    TimerChange, TimerKind, Trigger, Wait, WaitKind,
};
use rewind_store_slatedb::{Options, SlatePartitions, Timers};
use serde_json::json;
use slatedb::config::{FlushOptions, FlushType};

const PARTITION: PartitionId = PartitionId::new(0);
const RUNS: usize = 100;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let env = |name: &str| std::env::var(name).ok();
    let required = |name: &str| env(name).unwrap_or_else(|| panic!("{name} is required"));
    let root = env("REWIND_BENCH_ROOT").unwrap_or_else(|| format!("bench/{}", now_ms()));
    let rounds: usize = env("REWIND_BENCH_ROUNDS").map_or(50, |n| n.parse().unwrap());
    let idle =
        Duration::from_secs(env("REWIND_BENCH_IDLE_SECONDS").map_or(60, |n| n.parse().unwrap()));
    let bucket = AmazonS3Builder::new()
        .with_endpoint(required("REWIND_BENCH_S3_ENDPOINT"))
        .with_bucket_name(required("REWIND_BENCH_S3_BUCKET"))
        .with_access_key_id(required("REWIND_BENCH_S3_ACCESS_KEY_ID"))
        .with_secret_access_key(required("REWIND_BENCH_S3_SECRET_ACCESS_KEY"))
        .with_region(env("REWIND_BENCH_S3_REGION").unwrap_or_else(|| "auto".into()))
        .with_allow_http(env("REWIND_BENCH_S3_ALLOW_HTTP").is_some())
        .build()
        .expect("the S3 client");
    let bucket: Arc<dyn ObjectStore> = Arc::new(bucket);
    println!("# Partition benchmark\n\nroot `{root}`, {rounds} batches per size\n");
    println!("{:#?}\n", Options::default().timers);

    // The first writer.
    let first = Counting::new(&bucket);
    let store = SlatePartitions::on(first.store(), root.clone(), Options::default());
    let started = Instant::now();
    let log = store.open(PARTITION, &node("n1")).await.unwrap();
    println!("open of an empty partition: {}\n", ms(started.elapsed()));

    let mut runs = Runs::new();
    log.commit(runs.create()).await.unwrap();
    log.commit(runs.activate(RUNS)).await.unwrap();

    println!("## Commits\n");
    println!("| runs per batch | p50 | p90 | p99 | max | requests per batch |");
    println!("| --- | --- | --- | --- | --- | --- |");
    for size in [1, 10, 100] {
        let before = first.total();
        let mut latencies = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            let batch = runs.activate(size);
            let started = Instant::now();
            log.commit(batch).await.unwrap();
            latencies.push(started.elapsed());
        }
        let requests = (first.total() - before) as f64 / rounds as f64;
        let [p50, p90, p99, max] = percentiles(&mut latencies);
        println!(
            "| {size} | {} | {} | {} | {} | {requests:.1} |",
            ms(p50),
            ms(p90),
            ms(p99),
            ms(max)
        );
    }
    println!("\nrequests while committing:\n{}", first.report());

    // A second node takes the partition over while the first still runs.
    println!("## Takeover\n");
    let second = Counting::new(&bucket);
    let other = SlatePartitions::on(second.store(), root.clone(), Options::default());
    let started = Instant::now();
    let taken = other.open(PARTITION, &node("n2")).await.unwrap();
    println!(
        "open while the first writer runs (WAL replay of {} batches): {}",
        runs.batches,
        ms(started.elapsed())
    );
    println!("requests of the open:\n{}", second.report());
    let started = Instant::now();
    let fenced = log.commit(runs.activate(1)).await;
    assert!(
        matches!(fenced, Err(PartitionError::Fenced { .. })),
        "{fenced:?}"
    );
    println!(
        "the first writer's next commit is refused as fenced in {}\n",
        ms(started.elapsed())
    );
    drop(log);

    // The history leaves the WAL for a level-0 table, and a third node, its cache empty, takes
    // the partition over: its reads go to the bucket.
    taken
        .db()
        .flush_with_options(FlushOptions {
            flush_type: FlushType::MemTable,
        })
        .await
        .unwrap();
    let third = Counting::new(&bucket);
    let fresh = SlatePartitions::on(third.store(), root.clone(), Options::default());
    let started = Instant::now();
    let reader = fresh.open(PARTITION, &node("n3")).await.unwrap();
    println!(
        "open once the WAL is in a level-0 table: {}\nrequests of the open:\n{}",
        ms(started.elapsed()),
        third.report()
    );
    drop(taken);

    println!("## Cold history reads (a new node's cache, the history in a level-0 table)\n");
    let mut cold = Vec::new();
    let mut warm = Vec::new();
    for run in runs.runs.iter().take(20) {
        let started = Instant::now();
        let entries = reader.history(run, Attempt::FIRST, 0).await.unwrap();
        cold.push(started.elapsed());
        assert!(!entries.is_empty());
        let started = Instant::now();
        reader.history(run, Attempt::FIRST, 0).await.unwrap();
        warm.push(started.elapsed());
    }
    let [p50, p90, p99, max] = percentiles(&mut cold);
    println!(
        "first read of a run's history: p50 {}, p90 {}, p99 {}, max {} ({} entries per history)",
        ms(p50),
        ms(p90),
        ms(p99),
        ms(max),
        runs.history_len(0)
    );
    let [p50, _, p99, _] = percentiles(&mut warm);
    println!("read again: p50 {}, p99 {}", ms(p50), ms(p99));
    println!("requests of the reads:\n{}", third.report());

    println!("## Idle\n");
    idle_requests("this store's timers", &third, idle).await;
    drop(reader);

    if env("REWIND_BENCH_COMPARE_DEFAULTS").is_some() {
        let defaults = Counting::new(&bucket);
        let options = Options {
            timers: Timers {
                manifest_poll: Duration::from_secs(1),
                compactor_poll: Duration::from_secs(5),
                compactor_commit: Duration::from_secs(1),
                worker_poll: Duration::from_secs(5),
                collector: Duration::from_secs(600),
                wal_flushes_per_table: 4_096,
            },
            ..Options::default()
        };
        let store = SlatePartitions::on(defaults.store(), format!("{root}-defaults"), options);
        let log = store.open(PARTITION, &node("n3")).await.unwrap();
        log.commit(Runs::new().create()).await.unwrap();
        idle_requests("SlateDB's default timers", &defaults, idle).await;
        drop(log);
    }
}

/// Counts the requests made while nothing is committed, for `idle`.
async fn idle_requests(what: &str, counting: &Counting, idle: Duration) {
    counting.reset();
    tokio::time::sleep(idle).await;
    let total = counting.total();
    let minutes = idle.as_secs_f64() / 60.0;
    println!(
        "{what}: {total} requests in {} s, {:.1} per minute\n{}",
        idle.as_secs(),
        total as f64 / minutes,
        counting.report()
    );
}

fn node(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

fn ms(duration: Duration) -> String {
    format!("{:.1} ms", duration.as_secs_f64() * 1_000.0)
}

/// p50, p90, p99 and the maximum, nearest rank.
fn percentiles(values: &mut [Duration]) -> [Duration; 4] {
    values.sort();
    let at = |p: usize| values[((p * values.len()).div_ceil(100)).max(1) - 1];
    [at(50), at(90), at(99), values[values.len() - 1]]
}

// -------------------------------------------------------------------------------------------
// The runs the benchmark activates

struct Runs {
    runs: Vec<RunRef>,
    heads: Vec<RunHead>,
    summaries: Vec<IndexedSummary>,
    next: usize,
    batches: usize,
    namespace: NamespaceName,
}

impl Runs {
    fn new() -> Self {
        let tenant = TenantId::new("bench").unwrap();
        let namespace = NamespaceName::new("user-1").unwrap();
        let at = Timestamp(now_ms());
        let runs: Vec<RunRef> = (0..RUNS)
            .map(|n| RunRef::new(tenant.clone(), RunId::in_partition(PARTITION, at, n as u64)))
            .collect();
        let heads = runs
            .iter()
            .map(|run| RunHead {
                run: run.clone(),
                namespace: namespace.clone(),
                origin: "script".into(),
                suffix_len: 12,
                script: Digest::of(b"script"),
                action_set: Digest::of(b"action_set"),
                status: RunStatus::Pending,
                epoch: Epoch(1),
                attempt: Attempt::FIRST,
                sealed: None,
                failures: 0,
                attempts: 0,
                quarantine: None,
                waits: Vec::new(),
                expires_at: Timestamp(at.0 + 30 * 86_400_000),
                history_len: 0,
                metadata: RunMetadata {
                    labels: BTreeMap::from([("agent".into(), "triage".into())]),
                    ..RunMetadata::default()
                },
                idempotency_key: None,
                created_at: at,
                updated_at: at,
                ended_at: None,
            })
            .collect();
        let summaries = runs
            .iter()
            .map(|run| {
                IndexedSummary::new(RunSummary {
                    run: run.clone(),
                    namespace: namespace.clone(),
                    origin: "script".into(),
                    suffix_len: 12,
                    script: Digest::of(b"script"),
                    headline: "export default async ({ issue }) => {".into(),
                    action_set: Digest::of(b"action_set"),
                    status: RunStatus::Pending,
                    mode: Some(RunMode::Live),
                    labels: BTreeMap::from([("agent".into(), "triage".into())]),
                    parent: None,
                    workflow: Some("triage".into()),
                    started_by: None,
                    started_at: None,
                    ended_at: None,
                    last_activity_at: at,
                    calls: 0,
                    errors: 0,
                    history_len: 0,
                    waits: Vec::new(),
                    wakes_at: None,
                    ending: None,
                    quarantine: None,
                })
            })
            .collect();
        Self {
            runs,
            heads,
            summaries,
            next: 0,
            batches: 0,
            namespace,
        }
    }

    fn started(&self) -> RunStarted {
        RunStarted {
            script: Digest::of(b"script"),
            action_set: Digest::of(b"action_set"),
            input: json!({ "issue": 42, "title": "x".repeat(200) }).into(),
            seed: Seed::from_bytes([7; 32]),
            epoch: Epoch(1),
            expires_at: self.heads[0].expires_at,
            mode: RunMode::Live,
        }
    }

    /// Creates every run, its `run_started` in its inbox.
    fn create(&mut self) -> Batch {
        let mut batch = Batch::new();
        for n in 0..RUNS {
            batch.push(CreateRun {
                run: self.runs[n].clone(),
                head: self.heads[n].clone(),
                start_key: Some(format!("start-{n}")),
                entries: Vec::new(),
                payloads: Vec::new(),
                summary: self.summaries[n].clone(),
            });
            batch.push(Change::Enqueue {
                run: self.runs[n].clone(),
                item: InboxItem {
                    id: 1,
                    trigger: Trigger::RunStarted(self.started()),
                    received_at: self.heads[n].created_at,
                },
                signal_key: None,
            });
        }
        self.batches += 1;
        batch
    }

    fn history_len(&self, n: usize) -> u64 {
        self.heads[n].history_len
    }

    /// One activation of each of `count` runs: a completion and the call it answers with, its
    /// task and its timeout, the head, the summary and the counters.
    fn activate(&mut self, count: usize) -> Batch {
        let mut batch = Batch::new();
        for _ in 0..count {
            let n = self.next % RUNS;
            self.next += 1;
            batch.push(self.activation(n));
        }
        self.batches += 1;
        batch
    }

    fn activation(&mut self, n: usize) -> Activation {
        let run = self.runs[n].clone();
        let at = Timestamp(now_ms());
        let len = self.heads[n].history_len;
        let entry = |index: u64, event: Event| HistoryEntry {
            index,
            recorded_at: at,
            event,
        };
        let (entries, consumed, seq) = if len == 0 {
            (
                vec![
                    entry(0, Event::Lifecycle(Lifecycle::RunStarted(self.started()))),
                    entry(
                        1,
                        Event::Command(Command::ScheduleCall {
                            seq: Seq(1),
                            action: "github.list_issues".into(),
                            args: json!({ "repo": "acme/app", "state": "open" }).into(),
                            callsite: None,
                        }),
                    ),
                ],
                vec![1],
                1,
            )
        } else {
            let seq = u32::try_from(len / 2 + 1).unwrap();
            (
                vec![
                    entry(
                        len,
                        Event::Completion(Completion::CallCompleted {
                            seq: Seq(seq - 1),
                            result: json!({ "issues": [{ "id": 1, "title": "y".repeat(300) }] })
                                .into(),
                        }),
                    ),
                    entry(
                        len + 1,
                        Event::Command(Command::ScheduleCall {
                            seq: Seq(seq),
                            action: "github.close_issue".into(),
                            args: json!({ "repo": "acme/app", "issue": seq }).into(),
                            callsite: None,
                        }),
                    ),
                ],
                Vec::new(),
                seq,
            )
        };
        let route = ActionRoute::Mcp {
            server: "github".into(),
        };
        let task = TaskRecord {
            task: CallTask {
                task: TaskRef {
                    run: run.clone(),
                    seq: Seq(seq),
                },
                action_set: Digest::of(b"action_set"),
                action: "github.close_issue".into(),
                policy: ActionPolicy::default_for(&route),
                route,
                args: json!({ "repo": "acme/app", "issue": seq }).into(),
                state: TaskState::Ready {
                    next_delivery_at: at,
                },
                delivery: 0,
                schema_change_approved: false,
                created_at: at,
            },
            token_hash: None,
        };
        let mut head = self.heads[n].clone();
        head.status = RunStatus::Active;
        head.updated_at = at;
        head.history_len = len + entries.len() as u64;
        head.waits = vec![Wait {
            kind: WaitKind::Call,
            seq: Some(Seq(seq)),
            name: Some("github.close_issue".into()),
            since: at,
            until: None,
            callsite: None,
            task: None,
        }];
        let mut summary = self.summaries[n].clone();
        summary.summary.status = RunStatus::Active;
        summary.summary.started_at.get_or_insert(at);
        summary.summary.last_activity_at = at;
        summary.summary.calls += 1;
        summary.summary.history_len = head.history_len;
        summary.summary.waits = head.waits.clone();
        summary.actions.insert("github.close_issue".into());
        let counts = Counters {
            runs: RunCounts {
                started: u64::from(len == 0),
                ended: BTreeMap::new(),
            },
            ..Counters::default()
        };
        self.heads[n] = head.clone();
        self.summaries[n] = summary.clone();
        Activation {
            run: run.clone(),
            attempt: Attempt::FIRST,
            entries,
            consumed,
            payloads: Vec::new(),
            tasks: vec![task],
            timers: vec![TimerChange::Set(DueTimer {
                at: Timestamp(at.0 + 300_000),
                kind: TimerKind::CallTimeout { run, seq: Seq(seq) },
            })],
            head,
            summary,
            counters: vec![CounterDelta::Hour {
                tenant: self.runs[n].tenant.clone(),
                namespace: self.namespace.clone(),
                hour: Hour::of(at),
                counts,
            }],
            outbox: Vec::new(),
        }
    }
}

// -------------------------------------------------------------------------------------------
// An object store that counts its requests

#[derive(Clone)]
struct Counting {
    inner: Arc<dyn ObjectStore>,
    counts: Arc<Mutex<BTreeMap<(&'static str, &'static str), u64>>>,
}

impl fmt::Debug for Counting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Counting")
    }
}

impl fmt::Display for Counting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Counting({})", self.inner)
    }
}

impl Counting {
    fn new(inner: &Arc<dyn ObjectStore>) -> Self {
        Self {
            inner: Arc::clone(inner),
            counts: Arc::default(),
        }
    }

    fn store(&self) -> Arc<dyn ObjectStore> {
        Arc::new(self.clone())
    }

    fn count(&self, request: &'static str, path: Option<&Path>) {
        let path = path.map(Path::as_ref).unwrap_or_default();
        let what = if path.contains("/manifest") {
            "manifest"
        } else if path.contains("/wal") {
            "wal"
        } else if path.contains("/compacted") {
            "compacted"
        } else if path.contains("compactions") {
            "compactions"
        } else {
            "other"
        };
        *self
            .counts
            .lock()
            .unwrap()
            .entry((request, what))
            .or_default() += 1;
    }

    fn total(&self) -> u64 {
        self.counts.lock().unwrap().values().sum()
    }

    fn reset(&self) {
        self.counts.lock().unwrap().clear();
    }

    fn report(&self) -> String {
        let counts = std::mem::take(&mut *self.counts.lock().unwrap());
        let mut out = String::new();
        for ((request, what), count) in counts {
            out.push_str(&format!("- {request} {what}: {count}\n"));
        }
        out
    }
}

#[async_trait]
impl ObjectStore for Counting {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.count("PUT", Some(location));
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.count("PUT multipart", Some(location));
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.count(if options.head { "HEAD" } else { "GET" }, Some(location));
        self.inner.get_opts(location, options).await
    }

    async fn get_ranges(
        &self,
        location: &Path,
        ranges: &[Range<u64>],
    ) -> object_store::Result<Vec<slatedb::bytes::Bytes>> {
        for _ in ranges {
            self.count("GET range", Some(location));
        }
        self.inner.get_ranges(location, ranges).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        let counting = self.clone();
        let locations = locations
            .inspect(move |location| {
                if let Ok(location) = location {
                    counting.count("DELETE", Some(location));
                }
            })
            .boxed();
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.count("LIST", prefix);
        self.inner.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.count("LIST", prefix);
        self.inner.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.count("LIST", prefix);
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.count("COPY", Some(to));
        self.inner.copy_opts(from, to, options).await
    }

    async fn rename_opts(
        &self,
        from: &Path,
        to: &Path,
        options: RenameOptions,
    ) -> object_store::Result<()> {
        self.count("RENAME", Some(to));
        self.inner.rename_opts(from, to, options).await
    }
}
