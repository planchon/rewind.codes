//! Several engines share one database: each is a `PostgresStore` with its own pool and several
//! workers, driving runs the way the engine does (acquire, load the history, take the inbox,
//! commit, release), claiming and answering their tool calls, while signals arrive twice from
//! different engines. Some workers stall past their lease before committing.
//!
//! At the end, every run completed, and:
//!
//! - no commit is lost or duplicated: the ranges of the commits that succeeded tile each history
//!   exactly, and each wrote one feed item per consumer;
//! - fencing holds: a worker that stalled past its lease and lost the run was refused, and
//!   nothing it built reached the history;
//! - every call was answered once, under its tool's concurrency limit of 1 per run, and every
//!   signal was accepted and delivered once, though sent twice. A run waits for its signals
//!   before it completes.
//!
//! Runs that watch each other also end at once: a run and the run it started, whose ends reach
//! each other (the child's end to its parent, the parent's cancellation to its child). Their
//! commits lock both runs in the same order, so no two of them ever wait for each other in a
//! cycle: Postgres detects no deadlock.
//!
//! A new workflow version and the cleanup of tests race for the version's test: either the
//! version is stored and its test kept, or the test is marked for deletion and the version
//! refused. A version never references a test whose blob may be deleted.

mod support;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::future::join_all;
use rewind_core::{
    AbortKind, AbortReason, Command, Completion, Digest, Epoch, Event, HistoryEntry, Lifecycle,
    Payload, RunId, RunMode, RunStarted, RunStatus, Seed, Seq, Suite, SuiteTest, Timestamp,
    ToolOutcome, ToolPolicy, ToolRoute, Workflow, WorkflowSchemas,
};
use rewind_engine::ports::{
    AcquireRunnable, Activation, Enqueued, JournalStore, NewRun, NewToolTask, RunMetadata, RunRef,
    StartedBy, StoreError, TEST_GRACE, TaskFilter, TenantId, TenantScope, TestRecord, Trigger,
};
use rewind_store_postgres::PostgresStore;
use serde_json::json;
use support::Postgres;

const ENGINES: usize = 3;
const WORKERS: usize = 4;
const RUNS: u128 = 60;
const TENANTS: [&str; 3] = ["acme", "globex", "initech"];
/// Each run calls `BATCH` tools at once, `BATCHES` times, then completes.
const BATCH: u32 = 3;
const BATCHES: u32 = 3;
const SIGNALS_PER_RUN: usize = 2;
const LEASE: Duration = Duration::from_millis(300);
/// How long a stalled worker waits before committing: past its lease.
const STALL: Duration = Duration::from_millis(700);
const EPOCHS: &[Epoch] = &[Epoch(1)];

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_millis();
    Timestamp(u64::try_from(ms).expect("in range"))
}

fn run_ref(n: u128) -> RunRef {
    let tenant = TenantId::new(TENANTS[(n % TENANTS.len() as u128) as usize]).expect("tenant");
    RunRef::new(
        tenant,
        RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0000 | n),
    )
}

fn started() -> RunStarted {
    RunStarted {
        script: Digest::of(b"concurrency script"),
        toolset: Digest::of(b"concurrency toolset"),
        input: json!({ "batches": BATCHES }).into(),
        seed: Seed::from_bytes([9; 32]),
        epoch: Epoch(1),
        expires_at: Timestamp(now().0 + 86_400_000),
        mode: RunMode::Live,
    }
}

/// A host tool that allows one call in flight per run.
fn task(seq: u32) -> NewToolTask {
    let route = ToolRoute::Host { group: None };
    let mut policy = ToolPolicy::default_for(&route);
    policy.concurrency = 1;
    NewToolTask {
        seq: Seq(seq),
        tool: "desk.ask".into(),
        args: json!({ "question": seq }).into(),
        route,
        policy,
        server_concurrency: None,
    }
}

/// What the test observed.
#[derive(Default)]
struct Observed {
    /// Successful commits with entries: (run, from, to).
    commits: Vec<(RunId, u64, u64)>,
    /// Commits refused because the lease was lost.
    fenced: u64,
    /// Workers that stalled past their lease before committing.
    stalls: u64,
    /// Claims per task.
    claims: BTreeMap<(RunId, u32), u32>,
    /// Signals the store accepted, by payload.
    signals_accepted: BTreeSet<String>,
    duplicates: u64,
}

/// Calls in flight per run, as the claimers see them; the most seen at once.
#[derive(Default)]
struct InFlight {
    current: HashMap<RunId, u32>,
    max: u32,
}

struct Shared {
    observed: Mutex<Observed>,
    in_flight: Mutex<InFlight>,
    completed_runs: AtomicU64,
}

/// A small generator for the tenant draws and the stalls.
struct Draws(u64);

impl Draws {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The run's progress, from its history.
struct Progress {
    scheduled: u32,
    answered: BTreeSet<u32>,
    signals: usize,
    ended: bool,
}

fn progress(history: &[HistoryEntry]) -> Progress {
    let mut progress = Progress {
        scheduled: 0,
        answered: BTreeSet::new(),
        signals: 0,
        ended: false,
    };
    for entry in history {
        match &entry.event {
            Event::Command(Command::ScheduleToolCall { seq, .. }) => {
                progress.scheduled = progress.scheduled.max(seq.0);
            }
            Event::Completion(Completion::ToolCallCompleted { seq, .. }) => {
                progress.answered.insert(seq.0);
            }
            Event::Completion(Completion::SignalReceived { .. }) => progress.signals += 1,
            Event::Command(Command::CompleteRun { .. }) => progress.ended = true,
            _ => {}
        }
    }
    progress
}

/// One activation, if a run is runnable. Returns whether there was one.
async fn activate(store: &PostgresStore, worker: &str, draws: &mut Draws, shared: &Shared) -> bool {
    let request = AcquireRunnable {
        owner: worker,
        epochs: EPOCHS,
        tenants: &TenantScope::All,
        now: now(),
        lease_duration: LEASE,
        sticky_grace: Duration::from_millis(50),
        draw: draws.next(),
    };
    let Some(lease) = store.acquire_runnable(&request).await.expect("acquire") else {
        return false;
    };
    let history = store.load_history(&lease.run, 0).await.expect("history");
    let inbox = match store.take_inbox(&lease).await {
        Ok(inbox) => inbox,
        Err(StoreError::Fenced { .. }) => return true,
        Err(err) => panic!("take_inbox: {err}"),
    };
    let mut progress = progress(&history);
    let mut index = history.len() as u64;
    let mut activation = Activation::new(Vec::new(), Vec::new(), RunStatus::Active);
    for item in inbox {
        activation.consumed.push(item.id);
        if progress.ended {
            continue;
        }
        let at = now();
        let trigger = match item.trigger {
            Trigger::RunStarted(started) => Event::Lifecycle(Lifecycle::RunStarted(started)),
            Trigger::Completion(completion) => {
                match &completion {
                    Completion::ToolCallCompleted { seq, .. } => {
                        progress.answered.insert(seq.0);
                    }
                    Completion::SignalReceived { .. } => progress.signals += 1,
                    _ => {}
                }
                Event::Completion(completion)
            }
        };
        // The next batch once the last one is answered; the end once every batch is answered
        // and every signal arrived.
        let mut events = vec![trigger];
        let batch_done = (1..=progress.scheduled).all(|seq| progress.answered.contains(&seq));
        if batch_done && progress.scheduled < BATCH * BATCHES {
            for seq in progress.scheduled + 1..=progress.scheduled + BATCH {
                let task = task(seq);
                events.push(Event::Command(Command::ScheduleToolCall {
                    seq: task.seq,
                    tool: task.tool.clone(),
                    args: task.args.clone(),
                    callsite: None,
                }));
                activation.tool_calls.push(task);
            }
            progress.scheduled += BATCH;
        } else if batch_done && progress.signals == SIGNALS_PER_RUN {
            events.push(Event::Command(Command::CompleteRun {
                output: json!({ "answered": progress.answered.len() }).into(),
            }));
            activation.status = RunStatus::Completed;
            progress.ended = true;
        }
        for event in events {
            activation.entries.push(HistoryEntry {
                index,
                recorded_at: at,
                event,
            });
            index += 1;
        }
    }
    if draws.next().is_multiple_of(12) {
        // A stalled worker: a GC pause, a network partition. Another worker may take the run.
        shared.observed.lock().expect("lock").stalls += 1;
        tokio::time::sleep(STALL).await;
    }
    let from = history.len() as u64;
    let ended = activation.status.is_terminal();
    let has_entries = !activation.entries.is_empty();
    match store.commit_activation(&lease, activation, now()).await {
        Ok(()) => {
            if has_entries {
                let mut observed = shared.observed.lock().expect("lock");
                observed.commits.push((lease.run.id, from, index));
            }
            if ended {
                shared.completed_runs.fetch_add(1, Ordering::SeqCst);
            }
            store.release(&lease).await.expect("release");
        }
        Err(StoreError::Fenced { .. }) => {
            shared.observed.lock().expect("lock").fenced += 1;
        }
        Err(err) => panic!("commit_activation: {err}"),
    }
    true
}

/// Claims ready calls and answers them. Returns how many.
async fn answer(store: &PostgresStore, worker: &str, shared: &Shared) -> usize {
    let claimed = store
        .claim_tool_tasks(worker, &TaskFilter::All, now(), 8)
        .await
        .expect("claim");
    for task in &claimed {
        let run = task.task.task.run.id;
        {
            let mut in_flight = shared.in_flight.lock().expect("lock");
            let count = in_flight.current.entry(run).or_default();
            *count += 1;
            let count = *count;
            in_flight.max = in_flight.max.max(count);
            let mut observed = shared.observed.lock().expect("lock");
            *observed
                .claims
                .entry((run, task.task.task.seq.0))
                .or_default() += 1;
        }
    }
    for task in &claimed {
        let run = task.task.task.run.id;
        // Out of flight here, before the store hears it: the store frees the slot only once
        // the answer is in.
        *shared
            .in_flight
            .lock()
            .expect("lock")
            .current
            .entry(run)
            .or_default() -= 1;
        let answer = ToolOutcome::Result(json!({ "answer": task.task.task.seq.0 }));
        store
            .complete_tool_task(&task.token, answer, now())
            .await
            .expect("complete");
    }
    claimed.len()
}

/// Sends each run's signals twice, from two engines at once.
async fn send_signals(stores: &[PostgresStore], shared: &Shared) {
    for n in 0..RUNS {
        for s in 0..SIGNALS_PER_RUN {
            let run = run_ref(n);
            let key = format!("signal-{n}-{s}");
            let send = |store: &PostgresStore| {
                let run = run.clone();
                let key = key.clone();
                let store = store.clone();
                async move {
                    let signal = Completion::SignalReceived {
                        name: "note".into(),
                        payload: json!(key.clone()).into(),
                    };
                    store.enqueue(&run, signal, Some(key), now()).await
                }
            };
            let (first, second) = tokio::join!(
                send(&stores[n as usize % stores.len()]),
                send(&stores[(n as usize + 1) % stores.len()])
            );
            for outcome in [first, second] {
                let mut observed = shared.observed.lock().expect("lock");
                match outcome.expect("enqueue") {
                    Enqueued::Accepted => {
                        assert!(
                            observed.signals_accepted.insert(key.clone()),
                            "the signal {key} was accepted twice"
                        );
                    }
                    Enqueued::Duplicate => observed.duplicates += 1,
                    Enqueued::RunEnded => {}
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn engines_sharing_a_database_lose_and_duplicate_nothing() {
    let Some(postgres) = Postgres::start("concurrency") else {
        return;
    };
    let database = postgres.create_database();
    // The engines connect at once: the schema is migrated once.
    let stores: Vec<PostgresStore> =
        join_all((0..ENGINES).map(|_| postgres.connect_with(&database, 8))).await;

    for n in 0..RUNS {
        let store = &stores[n as usize % ENGINES];
        let new_run = NewRun {
            run: run_ref(n),
            namespace: rewind_engine::ports::NamespaceName::migrated(),
            origin: "script".into(),
            started: started(),
            metadata: RunMetadata::default(),
            created_at: now(),
        };
        store.create_run(new_run, None).await.expect("create_run");
    }

    let shared = Arc::new(Shared {
        observed: Mutex::new(Observed::default()),
        in_flight: Mutex::new(InFlight::default()),
        completed_runs: AtomicU64::new(0),
    });
    let mut workers = Vec::new();
    for (e, store) in stores.iter().enumerate() {
        for w in 0..WORKERS {
            let store = store.clone();
            let shared = Arc::clone(&shared);
            workers.push(tokio::spawn(async move {
                let worker = format!("engine-{e}/worker-{w}");
                let mut draws = Draws(0x9e37_79b9_7f4a_7c15 ^ ((e * WORKERS + w) as u64 + 1));
                while shared.completed_runs.load(Ordering::SeqCst) < RUNS as u64 {
                    let activated = activate(&store, &worker, &mut draws, &shared).await;
                    let answered = answer(&store, &worker, &shared).await;
                    if !activated && answered == 0 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            }));
        }
    }
    send_signals(&stores, &shared).await;
    let all = async {
        for worker in workers {
            worker.await.expect("worker");
        }
    };
    tokio::time::timeout(Duration::from_secs(240), all)
        .await
        .expect("every run completes");

    let store = &stores[0];
    let observed = std::mem::take(&mut *shared.observed.lock().expect("lock"));
    let mut commits: BTreeMap<RunId, Vec<(u64, u64)>> = BTreeMap::new();
    for (run, from, to) in &observed.commits {
        commits.entry(*run).or_default().push((*from, *to));
    }
    let mut delivered_signals = BTreeSet::new();
    for n in 0..RUNS {
        let run = run_ref(n);
        let record = store.get_run(&run).await.expect("get").expect("the run");
        assert_eq!(record.status, RunStatus::Completed, "{run}");
        let history = store.load_history(&run, 0).await.expect("history");
        assert_eq!(record.history_len, history.len() as u64);

        // The commits that succeeded tile the history: none lost, none twice.
        let mut ranges = commits.remove(&run.id).unwrap_or_default();
        ranges.sort_unstable();
        let mut next = 0;
        for (from, to) in &ranges {
            assert_eq!(*from, next, "run {run}: commits {ranges:?}");
            next = *to;
        }
        assert_eq!(next, history.len() as u64, "run {run}: commits {ranges:?}");

        // Every call answered once; every signal delivered once.
        let mut answered = BTreeMap::new();
        for entry in &history {
            match &entry.event {
                Event::Completion(Completion::ToolCallCompleted { seq, .. }) => {
                    *answered.entry(seq.0).or_insert(0) += 1;
                }
                Event::Completion(Completion::SignalReceived { payload, .. }) => {
                    let Payload::Value(key) = payload else {
                        panic!("an inline signal payload")
                    };
                    let key = key.as_str().expect("a string").to_owned();
                    assert!(
                        delivered_signals.insert(key.clone()),
                        "{key} delivered twice"
                    );
                }
                _ => {}
            }
        }
        assert_eq!(answered.len() as u32, BATCH * BATCHES, "run {run}");
        assert!(
            answered.values().all(|count| *count == 1),
            "run {run}: {answered:?}"
        );
        assert!(store.tool_tasks(&run).await.expect("tasks").is_empty());
    }
    assert!(commits.is_empty(), "commits on unknown runs: {commits:?}");
    assert_eq!(
        delivered_signals, observed.signals_accepted,
        "the accepted signals, and only them, reached the histories"
    );
    assert!(observed.duplicates > 0, "signals were sent twice");
    assert!(
        observed.claims.values().all(|claims| *claims == 1),
        "a task was claimed twice"
    );
    assert_eq!(
        observed.claims.len() as u128,
        RUNS * u128::from(BATCH * BATCHES)
    );
    assert_eq!(
        shared.in_flight.lock().expect("lock").max,
        1,
        "one call of the tool in flight per run"
    );
    assert!(observed.stalls > 0, "some workers stalled");

    // One feed item per consumer for each commit; nothing left in the inboxes.
    let feed: u64 = postgres
        .psql(&database, "SELECT count(*) FROM feed")
        .parse()
        .expect("a count");
    assert_eq!(feed, 3 * observed.commits.len() as u64);
    assert_eq!(postgres.psql(&database, "SELECT count(*) FROM inbox"), "0");
    eprintln!(
        "{} commits, {} refused after a stall ({} stalls), {} duplicate signals",
        observed.commits.len(),
        observed.fenced,
        observed.stalls,
        observed.duplicates
    );
}

/// Pairs of a run and the run it started attached, in each round.
const PAIRS: u128 = 24;
const ROUNDS: u128 = 4;

/// The run `n` of the watchers' test, in one tenant.
fn watcher_run(n: u128) -> RunRef {
    RunRef::new(
        TenantId::new("acme").expect("tenant"),
        RunId::from_u128(0x0193_0000_0000_7000_9000_0000_0000_0000 | n),
    )
}

/// Creates a run, started by `started_by` when given, and commits its `run_started`.
async fn start_watched(store: &PostgresStore, run: &RunRef, started_by: Option<StartedBy>) {
    let new_run = NewRun {
        run: run.clone(),
        namespace: rewind_engine::ports::NamespaceName::migrated(),
        origin: "script".into(),
        started: started(),
        metadata: RunMetadata {
            started_by,
            ..RunMetadata::default()
        },
        created_at: now(),
    };
    store.create_run(new_run, None).await.expect("create_run");
    let lease = store
        .acquire_run(run, "setup", now(), LEASE)
        .await
        .expect("acquire_run")
        .expect("a pending run");
    let consumed = store
        .take_inbox(&lease)
        .await
        .expect("take_inbox")
        .iter()
        .map(|item| item.id)
        .collect();
    let entry = HistoryEntry {
        index: 0,
        recorded_at: now(),
        event: Event::Lifecycle(Lifecycle::RunStarted(started())),
    };
    store
        .commit_activation(
            &lease,
            Activation::new(vec![entry], consumed, RunStatus::Active),
            now(),
        )
        .await
        .expect("commit");
    store.release(&lease).await.expect("release");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn runs_that_watch_each_other_end_at_once_without_deadlocks() {
    let Some(postgres) = Postgres::start("watchers") else {
        return;
    };
    let database = postgres.create_database();
    let store = postgres.connect_with(&database, 2 * PAIRS as u32).await;
    for round in 0..ROUNDS {
        // Half the parents have the lower id of their pair: both orders of locks are tried.
        let pairs: Vec<(RunRef, RunRef)> = (0..PAIRS)
            .map(|n| {
                let low = watcher_run(2 * (round * PAIRS + n));
                let high = watcher_run(2 * (round * PAIRS + n) + 1);
                if n % 2 == 0 { (low, high) } else { (high, low) }
            })
            .collect();
        let mut leases = Vec::new();
        for (parent, child) in &pairs {
            start_watched(&store, parent, None).await;
            let started_by = StartedBy {
                run: parent.id,
                seq: Seq(1),
                detached: false,
            };
            start_watched(&store, child, Some(started_by)).await;
            for run in [parent, child] {
                let lease = store
                    .acquire_run(run, "ender", now(), LEASE * 100)
                    .await
                    .expect("acquire_run")
                    .expect("an active run");
                leases.push(lease);
            }
        }
        // Every run ends at the same moment, with an end for its watchers.
        let barrier = Arc::new(tokio::sync::Barrier::new(leases.len()));
        let ends = leases.into_iter().map(|lease| {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            tokio::spawn(async move {
                let aborted = Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
                    kind: AbortKind::Internal,
                    message: "ended with its pair".into(),
                }));
                let entry = HistoryEntry {
                    index: 1,
                    recorded_at: now(),
                    event: aborted,
                };
                let mut activation = Activation::new(vec![entry], Vec::new(), RunStatus::Aborted);
                activation.end = Some(json!({ "status": "aborted" }));
                barrier.wait().await;
                store.commit_activation(&lease, activation, now()).await
            })
        });
        for ended in join_all(ends).await {
            ended.expect("task").expect("a terminal commit");
        }
        for (parent, child) in &pairs {
            for run in [parent, child] {
                let record = store.get_run(run).await.expect("get").expect("the run");
                assert_eq!(record.status, RunStatus::Aborted, "{run}");
            }
        }
    }
    // What a run's end enqueued into a run that ended next was cleared with it.
    assert_eq!(postgres.psql(&database, "SELECT count(*) FROM inbox"), "0");

    // The backends report their deadlocks when they exit.
    store.close().await;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let others = format!(
        "SELECT count(*) FROM pg_stat_activity WHERE datname = '{database}' \
         AND pid <> pg_backend_pid()"
    );
    while postgres.psql(&database, &others) != "0" {
        assert!(
            std::time::Instant::now() < deadline,
            "the store's connections close"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        postgres.psql(
            &database,
            &format!("SELECT deadlocks FROM pg_stat_database WHERE datname = '{database}'")
        ),
        "0",
        "terminal commits of related runs waited for each other in a cycle"
    );
}

/// Version `n` of a workflow whose suite is `test`.
fn version_with_test(n: usize, test: Digest) -> Workflow {
    Workflow::new(
        "triage",
        None,
        Digest::of(format!("script {n}").as_bytes()),
        Digest::of(b"toolset"),
        WorkflowSchemas {
            input: json!({ "type": "object" }),
            output: json!({}),
            signals: BTreeMap::new(),
        },
    )
    .expect("a valid workflow")
    .with_suite(Suite {
        parent: None,
        tests: vec![SuiteTest {
            name: "the test".to_owned(),
            test,
        }],
        retired: Vec::new(),
        updated: Vec::new(),
    })
    .expect("a valid suite")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_version_and_the_cleanup_of_its_test_never_both_win() {
    const TESTS: usize = 40;
    let Some(postgres) = Postgres::start("tests") else {
        return;
    };
    let database = postgres.create_database();
    let writer = postgres.connect(&database).await;
    let cleanup = postgres.connect(&database).await;
    let tenant = TenantId::new("acme").expect("tenant");
    let reserved_at = now();
    let due = reserved_at.saturating_add(TEST_GRACE);
    let (mut stored, mut refused) = (0, 0);
    // One test at a time: the tests of the rounds before are referenced or being deleted, so
    // the cleanup can only mark this round's.
    for n in 0..TESTS {
        let test = TestRecord {
            digest: Digest::of(format!("test {n}").as_bytes()),
            namespace: None,
            size: 100,
        };
        writer
            .reserve_tests(&tenant, std::slice::from_ref(&test), reserved_at)
            .await
            .expect("reserve_tests");
        let version = version_with_test(n, test.digest);
        let (put, marked) = tokio::join!(
            writer.put_workflow(&tenant, &version, due),
            cleanup.unreferenced_tests(due, TESTS),
        );
        let marked = marked
            .expect("unreferenced_tests")
            .contains(&(tenant.clone(), test.digest));
        let kept = writer
            .get_test(&tenant, test.digest)
            .await
            .expect("get_test")
            .is_some();
        let found = writer
            .get_workflow(&tenant, version.version())
            .await
            .expect("get_workflow")
            .is_some();
        match put {
            Ok(_) => {
                assert!(
                    kept && !marked && found,
                    "round {n}: stored, yet its test is marked"
                );
                stored += 1;
            }
            Err(StoreError::Conflict(_)) => {
                assert!(
                    !kept && marked && !found,
                    "round {n}: refused, yet its test is kept"
                );
                refused += 1;
            }
            Err(err) => panic!("round {n}: put_workflow: {err}"),
        }
    }
    eprintln!("{stored} versions stored, {refused} refused");
}
