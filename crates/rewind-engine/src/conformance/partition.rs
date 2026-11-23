//! The conformance suite of `PartitionStore`: fencing, durability across a new open, atomic
//! batches, the single writer's mistakes refused, signal keys, the end of an attempt, what an
//! activation writes, failures and quarantine, watchers, webhook tokens, recovery, seal and
//! erasure, the search of the index, the counters over whole hours, the gauges, the shapes, the
//! timers, the organizations, the outbox and tenant isolation.
//!
//! The factory creates an empty store. Each check opens the partition [`PARTITION`].

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{
    AbortKind, AbortReason, Annotation, Completion, Digest, Epoch, Event, HistoryEntry, Lifecycle,
    LogLevel, PartitionId, RunId, RunMode, RunStarted, RunStatus, Seed, Seq, Shape, Timestamp,
    ToolFailureKind, ToolPolicy, ToolRoute,
};
use serde_json::json;

use super::{T0, tenant};
use crate::ports::partition::{Activation, CreateRun};
use crate::ports::{
    Admission, Attempt, Batch, Change, CounterDelta, Counters, DueTimer, FailureCounts, Gauges,
    Hour, InboxItem, IndexedSummary, LatestRun, Month, NamespaceName, NodeId, OrganizationState,
    OutboxChange, OutboxEntry, OutboxId, Page, ParkKind, ParkReason, PartitionError, PartitionLog,
    PartitionStore, PayloadRecord, Predicate, Projection, QUARANTINE_AFTER_FAILURES, Quarantine,
    QuarantineKind, QuarantineNote, QuotaAlert, QuotaAlertKey, QuotaKind, RunCounts, RunHead,
    RunMetadata, RunOrder, RunQuery, RunRef, RunSummary, Scope, SearchCursor, TaskChange,
    TaskPhase, TaskRecord, TaskRef, TaskState, TenantId, Threshold, TimerChange, TimerKind,
    ToolCounts, ToolTask, Trigger, Wait, WaitKind, WaitingFor, Watcher, WebhookTarget, Window,
};

/// The partition every check opens.
pub const PARTITION: PartitionId = PartitionId::new(3);

const DAY: u64 = 86_400_000;
const HOUR: u64 = 3_600_000;

/// Runs every check, each on a new store.
pub async fn run_all<S: PartitionStore>(new_store: impl Fn() -> S) {
    macro_rules! partition_check {
        ($check:ident) => {{
            eprintln!(concat!("partition conformance: ", stringify!($check)));
            $check(&new_store()).await;
        }};
    }
    partition_check!(a_commit_is_read_by_a_later_open_on_any_node);
    partition_check!(an_open_fences_the_logs_opened_before);
    partition_check!(a_batch_is_applied_whole_or_not_at_all);
    partition_check!(the_mistakes_of_a_single_writer_are_refused);
    partition_check!(changes_to_a_missing_or_erased_run_are_refused);
    partition_check!(a_signal_key_is_taken_once_per_run);
    partition_check!(a_terminal_activation_clears_the_attempt);
    partition_check!(an_activation_writes_its_records);
    partition_check!(a_summary_is_written_alone);
    partition_check!(failures_and_interruptions_are_counted);
    partition_check!(watchers_are_kept_once);
    partition_check!(webhook_tokens_resolve_to_their_run);
    partition_check!(recover_returns_what_an_owner_needs);
    partition_check!(seal_keeps_the_head_and_the_summary);
    partition_check!(erase_deletes_everything_of_the_run);
    partition_check!(search_reads_the_scope_in_order_by_pages);
    partition_check!(search_stops_at_its_budget_with_a_cursor);
    partition_check!(a_pending_run_is_listed);
    partition_check!(erase_namespace_drops_its_counters_once_it_holds_no_run);
    partition_check!(counters_sum_whole_hours_of_the_scope);
    partition_check!(gauges_count_what_holds_now);
    partition_check!(shapes_merge_per_tool);
    partition_check!(timers_are_ordered_and_read_by_range);
    partition_check!(organizations_are_kept);
    partition_check!(outbox_entries_are_added_and_removed);
    partition_check!(runs_of_another_tenant_are_invisible);
}

// -------------------------------------------------------------------------------------------
// Helpers

fn node(id: &str) -> NodeId {
    NodeId::new(id).expect("the suite uses valid node ids")
}

async fn open<S: PartitionStore>(store: &S, id: &str) -> S::Log {
    store.open(PARTITION, &node(id)).await.expect("open")
}

fn acme() -> TenantId {
    tenant("acme")
}

fn namespace(name: &str) -> NamespaceName {
    NamespaceName::new(name).expect("the suites use valid namespace names")
}

fn alice() -> NamespaceName {
    namespace("alice")
}

fn bob() -> NamespaceName {
    namespace("bob")
}

fn later(at: Timestamp, ms: u64) -> Timestamp {
    Timestamp(at.0 + ms)
}

/// The run `n` of acme, drawn `n` ms after T0: run ids sort as `n` does.
fn run(n: u64) -> RunRef {
    run_of(&acme(), n)
}

fn run_of(tenant: &TenantId, n: u64) -> RunRef {
    RunRef::new(
        tenant.clone(),
        RunId::in_partition(PARTITION, Timestamp(T0.0 + n), n),
    )
}

fn script(name: &str) -> Digest {
    Digest::of(name.as_bytes())
}

fn toolset_digest() -> Digest {
    Digest::of(b"toolset")
}

fn head(run: &RunRef, namespace: &NamespaceName) -> RunHead {
    RunHead {
        run: run.clone(),
        namespace: namespace.clone(),
        script: script("script"),
        toolset: toolset_digest(),
        status: RunStatus::Pending,
        epoch: Epoch(1),
        attempt: Attempt::FIRST,
        sealed: None,
        failures: 0,
        attempts: 0,
        quarantine: None,
        waits: Vec::new(),
        expires_at: later(T0, DAY),
        history_len: 0,
        metadata: RunMetadata::default(),
        idempotency_key: None,
        created_at: T0,
        updated_at: T0,
        ended_at: None,
    }
}

fn create_with(head: RunHead, start_key: Option<&str>) -> Change {
    let mut summary = summary_of(&head.run, &head.namespace, RunStatus::Completed);
    summary.summary.status = RunStatus::Pending;
    CreateRun {
        run: head.run.clone(),
        head,
        start_key: start_key.map(str::to_owned),
        entries: Vec::new(),
        payloads: Vec::new(),
        summary,
    }
    .into()
}

/// Creates the run in alice.
fn create(run: &RunRef) -> Change {
    create_with(head(run, &alice()), None)
}

fn create_in(run: &RunRef, namespace: &NamespaceName, start_key: Option<&str>) -> Change {
    create_with(head(run, namespace), start_key)
}

fn started() -> RunStarted {
    RunStarted {
        script: script("script"),
        toolset: toolset_digest(),
        input: json!({ "n": 1 }).into(),
        seed: Seed::from_bytes([9; 32]),
        epoch: Epoch(1),
        expires_at: later(T0, DAY),
        mode: RunMode::Mock,
    }
}

fn entry(index: u64, event: Event) -> HistoryEntry {
    HistoryEntry {
        index,
        recorded_at: later(T0, index),
        event,
    }
}

fn run_started_entry() -> HistoryEntry {
    entry(0, Event::Lifecycle(Lifecycle::RunStarted(started())))
}

fn log_entry(index: u64) -> HistoryEntry {
    entry(
        index,
        Event::Annotation(Annotation::Log {
            level: LogLevel::Info,
            message: format!("entry {index}"),
            callsite: None,
        }),
    )
}

fn aborted_entry(index: u64) -> HistoryEntry {
    entry(
        index,
        Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
            kind: AbortKind::Internal,
            message: "given up".into(),
        })),
    )
}

fn started_item(id: u64) -> InboxItem {
    InboxItem {
        id,
        trigger: Trigger::RunStarted(started()),
        received_at: T0,
    }
}

fn signal(id: u64, name: &str) -> InboxItem {
    InboxItem {
        id,
        trigger: Trigger::Completion(Completion::SignalReceived {
            name: name.into(),
            payload: json!({ "id": id }).into(),
        }),
        received_at: later(T0, id),
    }
}

fn enqueue(run: &RunRef, item: InboxItem, signal_key: Option<&str>) -> Change {
    Change::Enqueue {
        run: run.clone(),
        item,
        signal_key: signal_key.map(str::to_owned),
    }
}

fn summary_of(run: &RunRef, namespace: &NamespaceName, status: RunStatus) -> IndexedSummary {
    IndexedSummary::new(RunSummary {
        run: run.clone(),
        namespace: namespace.clone(),
        script: script("script"),
        headline: "export default async () => {".into(),
        toolset: toolset_digest(),
        status: if status == RunStatus::Pending {
            RunStatus::Active
        } else {
            status
        },
        mode: Some(RunMode::Mock),
        labels: BTreeMap::new(),
        parent: None,
        workflow: None,
        started_by: None,
        started_at: Some(T0),
        ended_at: status.is_terminal().then_some(later(T0, 100)),
        last_activity_at: T0,
        tool_calls: 0,
        errors: 0,
        history_len: 1,
        waits: Vec::new(),
        wakes_at: None,
        ending: None,
        quarantine: None,
    })
}

/// An activation of the run as it is now: appends `entries`, consumes `consumed`, and leaves
/// the run in `status`, with nothing else.
async fn activation<L: PartitionLog>(
    log: &L,
    run: &RunRef,
    entries: Vec<HistoryEntry>,
    consumed: Vec<u64>,
    status: RunStatus,
) -> Activation {
    let mut head = log.run(run).await.expect("run").expect("the run exists");
    head.status = status;
    head.failures = 0;
    head.attempts = 0;
    head.ended_at = status.is_terminal().then_some(later(T0, 100));
    let mut summary = summary_of(run, &head.namespace, status);
    summary.summary.history_len = head.history_len + entries.len() as u64;
    Activation {
        run: run.clone(),
        attempt: head.attempt,
        entries,
        consumed,
        payloads: Vec::new(),
        tasks: Vec::new(),
        timers: Vec::new(),
        head,
        summary,
        counters: Vec::new(),
        outbox: Vec::new(),
    }
}

async fn activate<L: PartitionLog>(
    log: &L,
    run: &RunRef,
    entries: Vec<HistoryEntry>,
    consumed: Vec<u64>,
    status: RunStatus,
) {
    let activation = activation(log, run, entries, consumed, status).await;
    commit(log, vec![activation.into()]).await;
}

/// Creates the run in `namespace`, and activates it with its `run_started` and `summary`.
async fn index<L: PartitionLog>(
    log: &L,
    run: &RunRef,
    namespace: &NamespaceName,
    summary: RunSummary,
) {
    commit(
        log,
        vec![
            create_in(run, namespace, None),
            enqueue(run, started_item(1), None),
        ],
    )
    .await;
    let mut activation =
        activation(log, run, vec![run_started_entry()], vec![1], summary.status).await;
    activation.summary = IndexedSummary::new(summary);
    commit(log, vec![activation.into()]).await;
}

/// Ends the run: an activation that appends `run_aborted`.
async fn end<L: PartitionLog>(log: &L, run: &RunRef) {
    let next = log.run(run).await.expect("run").expect("run").history_len;
    let entries = if next == 0 {
        vec![run_started_entry(), aborted_entry(1)]
    } else {
        vec![aborted_entry(next)]
    };
    activate(log, run, entries, Vec::new(), RunStatus::Aborted).await;
}

fn task(run: &RunRef, seq: u32, state: TaskState) -> TaskRecord {
    let route = ToolRoute::Host { group: None };
    TaskRecord {
        task: ToolTask {
            task: TaskRef {
                run: run.clone(),
                seq: Seq(seq),
            },
            toolset: toolset_digest(),
            tool: "github.close_issue".into(),
            policy: ToolPolicy::default_for(&route),
            route,
            args: json!({ "seq": seq }).into(),
            state,
            attempt: 0,
            schema_change_approved: false,
            created_at: T0,
        },
        token_hash: None,
    }
}

fn ready(at: Timestamp) -> TaskState {
    TaskState::Ready {
        next_attempt_at: at,
    }
}

fn put_task(record: TaskRecord) -> Change {
    Change::Task {
        run: record.task.task.run.clone(),
        seq: record.task.task.seq,
        change: TaskChange::Put(Box::new(record)),
    }
}

fn set(at: Timestamp, kind: TimerKind) -> TimerChange {
    TimerChange::Set(DueTimer { at, kind })
}

fn script_timer(run: &RunRef, seq: u32) -> TimerKind {
    TimerKind::Script {
        run: run.clone(),
        seq: Seq(seq),
    }
}

fn payload(text: &str) -> PayloadRecord {
    let value = json!({ "text": text.repeat(100) });
    PayloadRecord {
        digest: Digest::of(text.as_bytes()),
        size: 1_200,
        preview: "{\"text\":".into(),
        value: Some(value),
    }
}

fn outbox_entry(id: u64, run: Option<&RunRef>) -> OutboxEntry {
    OutboxEntry {
        id: OutboxId(id),
        tenant: acme(),
        run: run.map(|run| run.id),
        projection: Projection::Otel {
            spans: Vec::new(),
            logs: Vec::new(),
        },
    }
}

fn alert(namespace: Option<NamespaceName>) -> QuotaAlert {
    QuotaAlert {
        key: QuotaAlertKey {
            tenant: acme(),
            namespace,
            quota: QuotaKind::CpuPerMonth,
            period: Month::of(T0),
            threshold: Threshold::Near,
        },
        used: 80,
        limit: 100,
        raised_at: T0,
        sent_at: None,
    }
}

fn hour_delta(
    tenant: &TenantId,
    namespace: &NamespaceName,
    hour: Hour,
    counts: Counters,
) -> CounterDelta {
    CounterDelta::Hour {
        tenant: tenant.clone(),
        namespace: namespace.clone(),
        hour,
        counts,
    }
}

fn created(count: u64) -> Counters {
    Counters {
        created: count,
        runs: RunCounts {
            started: count,
            ended: BTreeMap::new(),
        },
        ..Counters::default()
    }
}

async fn commit<L: PartitionLog>(log: &L, changes: Vec<Change>) {
    log.commit(Batch { changes }).await.expect("commit");
}

async fn refused<L: PartitionLog>(log: &L, changes: Vec<Change>) -> PartitionError {
    log.commit(Batch { changes })
        .await
        .expect_err("the batch is refused")
}

fn is_conflict(error: &PartitionError) -> bool {
    matches!(error, PartitionError::Conflict(_))
}

async fn timers_of<L: PartitionLog>(log: &L) -> Vec<DueTimer> {
    log.timers(Timestamp(0), Timestamp(u64::MAX))
        .await
        .expect("timers")
}

fn ids(summaries: &[RunSummary]) -> Vec<RunRef> {
    summaries
        .iter()
        .map(|summary| summary.run.clone())
        .collect()
}

// -------------------------------------------------------------------------------------------
// Fencing, durability, atomicity

pub async fn a_commit_is_read_by_a_later_open_on_any_node<S: PartitionStore>(store: &S) {
    let first = open(store, "n1").await;
    assert_eq!(first.partition(), PARTITION);
    let (r1, r2) = (run(1), run(2));
    let token = Digest::of(b"webhook token");
    let state = OrganizationState {
        alerts: vec![alert(None)],
        admissions: vec![Admission {
            run: r2.id,
            namespace: alice(),
            script: script("script"),
            admitted_at: T0,
        }],
    };
    commit(
        &first,
        vec![
            create_in(&r1, &alice(), Some("start-1")),
            create(&r2),
            enqueue(&r1, started_item(1), None),
            set(later(T0, DAY), TimerKind::RunDeadline { run: r1.clone() }).into(),
            Change::Watch {
                run: r1.clone(),
                watcher: Watcher {
                    run: r2.id,
                    seq: Seq(1),
                    attached: true,
                },
            },
            Change::WebhookToken {
                run: r1.clone(),
                name: "approval".into(),
                hash: token,
            },
            Change::Organization {
                tenant: acme(),
                state: state.clone(),
            },
            Change::Counters(vec![hour_delta(
                &acme(),
                &alice(),
                Hour::of(T0),
                created(1),
            )]),
        ],
    )
    .await;

    let second = open(store, "n2").await;
    assert_eq!(
        second.run(&r1).await.expect("run"),
        Some(head(&r1, &alice())),
        "the head as it was written"
    );
    assert_eq!(second.inbox(&r1).await.expect("inbox"), [started_item(1)]);
    assert_eq!(
        second
            .started(&acme(), &alice(), "start-1")
            .await
            .expect("started"),
        Some(r1.id)
    );
    assert_eq!(
        timers_of(&second).await,
        [DueTimer {
            at: later(T0, DAY),
            kind: TimerKind::RunDeadline { run: r1.clone() }
        }]
    );
    assert_eq!(
        second.watchers(&r1).await.expect("watchers"),
        [Watcher {
            run: r2.id,
            seq: Seq(1),
            attached: true
        }]
    );
    assert_eq!(
        second.webhook(token).await.expect("webhook"),
        Some(WebhookTarget {
            run: r1.clone(),
            name: "approval".into()
        })
    );
    assert_eq!(
        second.organization(&acme()).await.expect("organization"),
        Some(state)
    );
    assert_eq!(
        second
            .counters(
                &Scope::Tenant(acme()),
                Window::of_day(crate::ports::Day::of(T0))
            )
            .await
            .expect("counters")
            .created,
        1
    );

    commit(&second, vec![enqueue(&r1, signal(2, "go"), None)]).await;
    let third = open(store, "n1").await;
    assert_eq!(
        third.inbox(&r1).await.expect("inbox"),
        [started_item(1), signal(2, "go")],
        "what the second owner committed is read by the next"
    );
    let elsewhere = store
        .open(PartitionId::new(PARTITION.get() + 1), &node("n1"))
        .await
        .expect("open");
    assert_eq!(
        elsewhere.run(&r1).await.expect("run"),
        None,
        "partitions are apart"
    );
}

pub async fn an_open_fences_the_logs_opened_before<S: PartitionStore>(store: &S) {
    let first = open(store, "n1").await;
    commit(&first, vec![create(&run(1))]).await;
    let second = open(store, "n2").await;
    let fenced = first.commit(Batch::new().with(create(&run(2)))).await;
    assert_eq!(
        fenced,
        Err(PartitionError::Fenced {
            partition: PARTITION
        })
    );
    assert_eq!(
        second.run(&run(2)).await.expect("run"),
        None,
        "nothing of a fenced commit is visible"
    );
    assert_eq!(
        first.commit(Batch::new()).await,
        Err(PartitionError::Fenced {
            partition: PARTITION
        }),
        "even an empty batch"
    );
    let other = store
        .open(PartitionId::new(PARTITION.get() + 1), &node("n1"))
        .await
        .expect("open");
    commit(&other, vec![create(&run(7))]).await;
    commit(&second, vec![create(&run(2))]).await;
    assert!(
        second.run(&run(1)).await.expect("run").is_some(),
        "the second log reads what the first committed before"
    );
    let third = open(store, "n2").await;
    assert!(matches!(
        second.commit(Batch::new().with(create(&run(3)))).await,
        Err(PartitionError::Fenced { .. })
    ));
    assert!(third.run(&run(2)).await.expect("run").is_some());
    assert_eq!(third.run(&run(3)).await.expect("run"), None);
}

pub async fn a_batch_is_applied_whole_or_not_at_all<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let (r1, r2) = (run(1), run(2));
    let error = refused(&log, vec![create(&r1), enqueue(&r2, started_item(1), None)]).await;
    assert!(is_conflict(&error), "{error:?}");
    assert_eq!(
        log.run(&r1).await.expect("run"),
        None,
        "none of its changes"
    );
    commit(
        &log,
        vec![
            create(&r1),
            enqueue(&r1, started_item(1), None),
            enqueue(&r1, signal(2, "go"), None),
        ],
    )
    .await;
    assert_eq!(
        log.inbox(&r1).await.expect("inbox"),
        [started_item(1), signal(2, "go")],
        "the changes of a batch apply in order"
    );
    let wrong = activation(&log, &r1, vec![log_entry(3)], vec![1], RunStatus::Active).await;
    let error = refused(
        &log,
        vec![enqueue(&r1, signal(3, "later"), None), wrong.into()],
    )
    .await;
    assert!(is_conflict(&error), "{error:?}");
    assert_eq!(
        log.inbox(&r1).await.expect("inbox"),
        [started_item(1), signal(2, "go")]
    );
    let reopened = open(store, "n2").await;
    assert_eq!(
        reopened.inbox(&r1).await.expect("inbox").len(),
        2,
        "a refused batch is not durable either"
    );
}

pub async fn the_mistakes_of_a_single_writer_are_refused<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let (r1, r2) = (run(1), run(2));
    commit(
        &log,
        vec![
            create_in(&r1, &alice(), Some("k")),
            create(&r2),
            enqueue(&r1, started_item(1), None),
        ],
    )
    .await;
    let mistakes: Vec<(&str, Vec<Change>)> = vec![
        ("a run created twice", vec![create(&r1)]),
        (
            "a start key already used in the namespace",
            vec![create_in(&run(3), &alice(), Some("k"))],
        ),
        (
            "an inbox item numbered twice",
            vec![enqueue(&r1, signal(1, "again"), None)],
        ),
        (
            "an outbox entry numbered twice",
            vec![
                OutboxChange::Add(Box::new(outbox_entry(1, None))).into(),
                OutboxChange::Add(Box::new(outbox_entry(1, None))).into(),
            ],
        ),
        (
            "the retry of an outbox entry that is not there",
            vec![
                set(
                    T0,
                    TimerKind::WebhookRetry {
                        tenant: acme(),
                        entry: OutboxId(9),
                    },
                )
                .into(),
            ],
        ),
        (
            "the seal of an attempt that has not ended",
            vec![Change::Seal {
                run: r1.clone(),
                attempt: Attempt::FIRST,
            }],
        ),
    ];
    for (mistake, changes) in mistakes {
        let error = refused(&log, changes).await;
        assert!(is_conflict(&error), "{mistake}: {error:?}");
    }
    commit(&log, vec![create_in(&run(3), &bob(), Some("k"))]).await;
    commit(
        &log,
        vec![create_in(&run_of(&tenant("other"), 4), &alice(), Some("k"))],
    )
    .await;

    for (mistake, entries) in [
        (
            "entries after a gap",
            vec![run_started_entry(), log_entry(2)],
        ),
        (
            "entries that do not start at the length",
            vec![log_entry(1)],
        ),
    ] {
        let wrong = activation(&log, &r1, entries, vec![1], RunStatus::Active).await;
        let error = refused(&log, vec![wrong.into()]).await;
        assert!(is_conflict(&error), "{mistake}: {error:?}");
    }
    let missing = activation(
        &log,
        &r1,
        vec![run_started_entry()],
        vec![1, 99],
        RunStatus::Active,
    )
    .await;
    let error = refused(&log, vec![missing.into()]).await;
    assert!(
        is_conflict(&error),
        "a consumed inbox item that does not exist: {error:?}"
    );
    let mut another_attempt = activation(
        &log,
        &r1,
        vec![run_started_entry()],
        vec![1],
        RunStatus::Active,
    )
    .await;
    another_attempt.attempt = Attempt::FIRST.next();
    another_attempt.head.attempt = Attempt::FIRST.next();
    let error = refused(&log, vec![another_attempt.into()]).await;
    assert!(
        is_conflict(&error),
        "an attempt that is not the run's: {error:?}"
    );
    let mut twice = activation(
        &log,
        &r1,
        vec![run_started_entry()],
        vec![1],
        RunStatus::Active,
    )
    .await;
    twice.tasks = vec![task(&r1, 1, ready(T0)), task(&r1, 1, ready(T0))];
    let error = refused(&log, vec![twice.into()]).await;
    assert!(is_conflict(&error), "a task created twice: {error:?}");

    activate(
        &log,
        &r1,
        vec![run_started_entry()],
        vec![1],
        RunStatus::Active,
    )
    .await;
    let overlap = activation(
        &log,
        &r1,
        vec![run_started_entry()],
        Vec::new(),
        RunStatus::Active,
    )
    .await;
    let error = refused(&log, vec![overlap.into()]).await;
    assert!(is_conflict(&error), "entries over the history: {error:?}");
    end(&log, &r1).await;
    let after_end = activation(&log, &r1, vec![log_entry(2)], Vec::new(), RunStatus::Active).await;
    let error = refused(&log, vec![after_end.into()]).await;
    assert!(
        is_conflict(&error),
        "an activation of an ended attempt: {error:?}"
    );
    assert_eq!(
        log.history(&r1, Attempt::FIRST, 0)
            .await
            .expect("history")
            .len(),
        2,
        "nothing of the refused batches"
    );
}

pub async fn changes_to_a_missing_or_erased_run_are_refused<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let (missing, erased) = (run(9), run(2));
    commit(&log, vec![create(&erased), create(&run(1))]).await;
    let template = activation(
        &log,
        &run(1),
        vec![run_started_entry()],
        Vec::new(),
        RunStatus::Active,
    )
    .await;
    commit(
        &log,
        vec![Change::Erase {
            run: erased.clone(),
        }],
    )
    .await;
    for gone in [&missing, &erased] {
        let mut activation = template.clone();
        activation.run = gone.clone();
        activation.head.run = gone.clone();
        activation.summary.summary.run = gone.clone();
        let changes: Vec<(&str, Change)> = vec![
            ("Enqueue", enqueue(gone, started_item(1), None)),
            ("Activation", activation.into()),
            ("Task", put_task(task(gone, 1, ready(T0)))),
            ("Timer", set(T0, script_timer(gone, 1)).into()),
            (
                "Failure",
                Change::Failure {
                    run: gone.clone(),
                    error: "x".into(),
                    at: T0,
                },
            ),
            ("Interrupted", Change::Interrupted { run: gone.clone() }),
            (
                "Quarantine",
                Change::Quarantine {
                    run: gone.clone(),
                    quarantine: Quarantine {
                        kind: QuarantineKind::NonDeterminism,
                        reason: "x".into(),
                        since: T0,
                        retried_by: None,
                    },
                },
            ),
            ("Release", Change::Release { run: gone.clone() }),
            (
                "Watch",
                Change::Watch {
                    run: gone.clone(),
                    watcher: Watcher {
                        run: run(1).id,
                        seq: Seq(1),
                        attached: false,
                    },
                },
            ),
            (
                "WebhookToken",
                Change::WebhookToken {
                    run: gone.clone(),
                    name: "x".into(),
                    hash: Digest::of(b"t"),
                },
            ),
        ];
        for (kind, change) in changes {
            let error = refused(&log, vec![change]).await;
            assert!(is_conflict(&error), "{kind} of {gone}: {error:?}");
        }
    }
    commit(
        &log,
        vec![
            Change::Seal {
                run: missing.clone(),
                attempt: Attempt::FIRST,
            },
            Change::Erase {
                run: missing.clone(),
            },
            Change::Seal {
                run: erased.clone(),
                attempt: Attempt::FIRST,
            },
            Change::Erase { run: erased },
        ],
    )
    .await;
}

// -------------------------------------------------------------------------------------------
// Signal keys and the end of an attempt

pub async fn a_signal_key_is_taken_once_per_run<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let (r1, r2) = (run(1), run(2));
    commit(&log, vec![create(&r1), create(&r2)]).await;
    commit(
        &log,
        vec![
            enqueue(&r1, signal(1, "approval"), Some("key-1")),
            enqueue(&r1, signal(2, "approval"), Some("key-1")),
        ],
    )
    .await;
    commit(
        &log,
        vec![enqueue(&r1, signal(3, "approval"), Some("key-1"))],
    )
    .await;
    assert_eq!(
        log.inbox(&r1).await.expect("inbox"),
        [signal(1, "approval")],
        "a key already seen for the run changes nothing"
    );
    assert!(log.signal_key(&r1, "key-1").await.expect("signal_key"));
    assert!(!log.signal_key(&r1, "key-2").await.expect("signal_key"));
    assert!(!log.signal_key(&r2, "key-1").await.expect("signal_key"));
    commit(
        &log,
        vec![
            enqueue(&r2, signal(1, "approval"), Some("key-1")),
            enqueue(&r1, signal(4, "approval"), None),
            enqueue(&r1, signal(5, "approval"), None),
        ],
    )
    .await;
    assert_eq!(
        log.inbox(&r2).await.expect("inbox").len(),
        1,
        "keys are per run"
    );
    assert_eq!(
        log.inbox(&r1).await.expect("inbox"),
        [
            signal(1, "approval"),
            signal(4, "approval"),
            signal(5, "approval")
        ],
        "without a key, every item is taken"
    );
    activate(
        &log,
        &r1,
        vec![run_started_entry()],
        vec![1, 4],
        RunStatus::Active,
    )
    .await;
    commit(
        &log,
        vec![enqueue(&r1, signal(6, "approval"), Some("key-1"))],
    )
    .await;
    assert_eq!(
        log.inbox(&r1).await.expect("inbox"),
        [signal(5, "approval")],
        "a key outlives the delivery of its signal"
    );
}

pub async fn a_terminal_activation_clears_the_attempt<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let (r1, r2) = (run(1), run(2));
    let token = Digest::of(b"token");
    commit(
        &log,
        vec![
            create(&r1),
            create(&r2),
            enqueue(&r1, started_item(1), None),
            enqueue(&r1, signal(2, "later"), None),
        ],
    )
    .await;
    let mut first = activation(
        &log,
        &r1,
        vec![run_started_entry()],
        vec![1],
        RunStatus::Active,
    )
    .await;
    first.tasks = vec![task(&r1, 1, ready(T0)), task(&r1, 2, TaskState::Approval)];
    first.timers = vec![
        set(later(T0, 1_000), script_timer(&r1, 3)),
        set(later(T0, DAY), TimerKind::RunDeadline { run: r1.clone() }),
    ];
    first.outbox = vec![outbox_entry(1, Some(&r1))];
    commit(
        &log,
        vec![
            first.into(),
            Change::WebhookToken {
                run: r1.clone(),
                name: "hook".into(),
                hash: token,
            },
            set(
                later(T0, 30_000),
                TimerKind::ActivationRetry { run: r1.clone() },
            )
            .into(),
            set(
                later(T0, 60_000),
                TimerKind::WebhookRetry {
                    tenant: acme(),
                    entry: OutboxId(1),
                },
            )
            .into(),
            set(later(T0, 5), script_timer(&r2, 1)).into(),
        ],
    )
    .await;
    assert_eq!(timers_of(&log).await.len(), 5);
    assert!(log.webhook(token).await.expect("webhook").is_some());

    end(&log, &r1).await;
    assert_eq!(log.tasks(&r1).await.expect("tasks"), [], "its tool tasks");
    assert_eq!(log.inbox(&r1).await.expect("inbox"), [], "its inbox");
    assert_eq!(
        log.webhook(token).await.expect("webhook"),
        None,
        "its webhook tokens stop resolving"
    );
    assert_eq!(
        timers_of(&log).await,
        [
            DueTimer {
                at: later(T0, 5),
                kind: script_timer(&r2, 1)
            },
            DueTimer {
                at: later(T0, 60_000),
                kind: TimerKind::WebhookRetry {
                    tenant: acme(),
                    entry: OutboxId(1)
                }
            },
        ],
        "its timers of the attempt; another run's and an outbox entry's stay"
    );
    commit(
        &log,
        vec![
            enqueue(&r1, signal(3, "late"), None),
            put_task(task(&r1, 4, ready(T0))),
            Change::WebhookToken {
                run: r1.clone(),
                name: "hook".into(),
                hash: Digest::of(b"another"),
            },
        ],
    )
    .await;
    assert_eq!(
        log.inbox(&r1).await.expect("inbox"),
        [],
        "nothing is enqueued for an ended attempt"
    );
    assert_eq!(log.tasks(&r1).await.expect("tasks"), []);
    assert_eq!(
        log.webhook(Digest::of(b"another")).await.expect("webhook"),
        None
    );
    let head = log.run(&r1).await.expect("run").expect("run");
    assert_eq!(
        (head.status, head.history_len, head.sealed),
        (RunStatus::Aborted, 2, None)
    );
}

pub async fn an_activation_writes_its_records<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let (r1, r2) = (run(1), run(2));
    commit(
        &log,
        vec![
            create(&r1),
            create(&r2),
            enqueue(&r1, started_item(1), None),
            enqueue(&r1, signal(2, "go"), None),
            enqueue(&r1, signal(3, "next"), None),
        ],
    )
    .await;
    let mut first = activation(
        &log,
        &r1,
        vec![run_started_entry(), log_entry(1)],
        vec![1, 2],
        RunStatus::Active,
    )
    .await;
    first.head.history_len = 99;
    first.head.updated_at = later(T0, 1);
    first.payloads = vec![payload("large")];
    first.tasks = vec![task(&r1, 1, ready(T0))];
    first.timers = vec![set(later(T0, 1_000), script_timer(&r1, 2))];
    first.summary.tools.insert("github.close_issue".into());
    first.counters = vec![hour_delta(&acme(), &alice(), Hour::of(T0), created(1))];
    first.outbox = vec![outbox_entry(1, Some(&r1))];
    let summary = first.summary.clone();
    commit(&log, vec![first.into()]).await;

    assert_eq!(
        log.history(&r1, Attempt::FIRST, 0).await.expect("history"),
        [run_started_entry(), log_entry(1)]
    );
    assert_eq!(
        log.history(&r1, Attempt::FIRST, 1).await.expect("history"),
        [log_entry(1)],
        "from an index"
    );
    assert_eq!(
        log.history(&r1, Attempt::FIRST.next(), 0)
            .await
            .expect("history"),
        [],
        "another attempt"
    );
    assert_eq!(
        log.inbox(&r1).await.expect("inbox"),
        [signal(3, "next")],
        "the items consumed are gone"
    );
    assert_eq!(
        log.payload(&r1, payload("large").digest)
            .await
            .expect("payload"),
        Some(payload("large"))
    );
    assert_eq!(
        log.payload(&r2, payload("large").digest)
            .await
            .expect("payload"),
        None,
        "a payload belongs to its run"
    );
    assert_eq!(
        log.tasks(&r1).await.expect("tasks"),
        [task(&r1, 1, ready(T0))]
    );
    assert_eq!(
        timers_of(&log).await,
        [DueTimer {
            at: later(T0, 1_000),
            kind: script_timer(&r1, 2)
        }]
    );
    let head = log.run(&r1).await.expect("run").expect("run");
    assert_eq!(
        (head.status, head.history_len, head.updated_at),
        (RunStatus::Active, 2, later(T0, 1)),
        "the head as written, its history length the store's"
    );
    assert_eq!(log.summary(&r1).await.expect("summary"), Some(summary));
    assert_eq!(
        log.counters(
            &Scope::Tenant(acme()),
            Window::hours(Hour::of(T0), Hour::of(T0).next())
        )
        .await
        .expect("counters")
        .created,
        1
    );
    assert_eq!(
        log.recover(T0).await.expect("recover").outbox,
        [outbox_entry(1, Some(&r1))]
    );

    let mut second = activation(&log, &r1, vec![log_entry(2)], vec![3], RunStatus::Active).await;
    second.timers = vec![TimerChange::Cancel(script_timer(&r1, 2))];
    commit(&log, vec![second.into()]).await;
    assert_eq!(
        log.history(&r1, Attempt::FIRST, 0)
            .await
            .expect("history")
            .len(),
        3
    );
    assert_eq!(timers_of(&log).await, [], "a cancelled timer is gone");
    assert_eq!(
        log.tasks(&r1).await.expect("tasks").len(),
        1,
        "the tasks of an earlier activation stay"
    );
}

// -------------------------------------------------------------------------------------------
// Failures, quarantine, watchers, webhook tokens

/// `Summary` writes the run's summary as it is given, outside an activation: what its tasks
/// became shows in the index and in the search. Refused for a run that does not exist.
pub async fn a_summary_is_written_alone<S: PartitionStore>(store: &S) {
    let log = open(store, "a").await;
    let run = run(1);
    let mut summary = summary_of(&run, &alice(), RunStatus::Active);
    index(&log, &run, &alice(), summary.summary.clone()).await;
    summary.summary.waits = vec![Wait {
        kind: WaitKind::Call,
        seq: Some(Seq(1)),
        name: Some("github.close_issue".into()),
        since: T0,
        until: None,
        callsite: None,
        task: Some(TaskPhase::Parked),
    }];
    commit(
        &log,
        vec![Change::Summary {
            run: run.clone(),
            summary: Box::new(summary.clone()),
        }],
    )
    .await;
    assert_eq!(log.summary(&run).await.expect("summary"), Some(summary.clone()));
    let parked = RunQuery {
        tenant: acme(),
        terms: vec![crate::ports::Term {
            predicate: Predicate::Waiting([WaitingFor::Parked].into()),
            negated: false,
        }],
    };
    let found = log
        .search(
            &Scope::Tenant(acme()),
            &parked,
            RunOrder::Started,
            &Page::first(10),
        )
        .await
        .expect("search");
    assert_eq!(ids(&found.summaries), vec![run.clone()], "the search reads it");

    let missing = run_of(&acme(), 99);
    let error = refused(
        &log,
        vec![Change::Summary {
            run: missing.clone(),
            summary: Box::new(summary_of(&missing, &alice(), RunStatus::Active)),
        }],
    )
    .await;
    assert!(is_conflict(&error), "{error:?}");
}

pub async fn failures_and_interruptions_are_counted<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let r1 = run(1);
    commit(&log, vec![create(&r1), enqueue(&r1, started_item(1), None)]).await;
    activate(
        &log,
        &r1,
        vec![run_started_entry()],
        vec![1],
        RunStatus::Active,
    )
    .await;
    commit(
        &log,
        vec![
            Change::Interrupted { run: r1.clone() },
            Change::Interrupted { run: r1.clone() },
        ],
    )
    .await;
    assert_eq!(log.run(&r1).await.expect("run").expect("run").attempts, 2);
    for n in 1..QUARANTINE_AFTER_FAILURES {
        commit(
            &log,
            vec![Change::Failure {
                run: r1.clone(),
                error: format!("failure {n}"),
                at: later(T0, u64::from(n)),
            }],
        )
        .await;
        let head = log.run(&r1).await.expect("run").expect("run");
        assert_eq!((head.failures, head.quarantine), (n, None));
    }
    let at = later(T0, 100);
    commit(
        &log,
        vec![Change::Failure {
            run: r1.clone(),
            error: "the last error".into(),
            at,
        }],
    )
    .await;
    let quarantine = Quarantine {
        kind: QuarantineKind::Failures,
        reason: "the last error".into(),
        since: at,
        retried_by: None,
    };
    let head = log.run(&r1).await.expect("run").expect("run");
    assert_eq!(
        (head.failures, head.quarantine.clone()),
        (QUARANTINE_AFTER_FAILURES, Some(quarantine.clone())),
        "the fifth failure in a row quarantines the run"
    );
    assert_eq!(
        log.summary(&r1)
            .await
            .expect("summary")
            .expect("summary")
            .summary
            .quarantine,
        Some(QuarantineNote::from(&quarantine)),
        "the summary shows the quarantine"
    );
    commit(&log, vec![Change::Release { run: r1.clone() }]).await;
    let head = log.run(&r1).await.expect("run").expect("run");
    assert_eq!(
        (head.failures, head.attempts, head.quarantine),
        (0, 0, None),
        "a release lifts it, and resets the counts"
    );
    assert_eq!(
        log.summary(&r1)
            .await
            .expect("summary")
            .expect("summary")
            .summary
            .quarantine,
        None
    );
    let retried = Quarantine {
        kind: QuarantineKind::NonDeterminism,
        reason: "replay mismatch at entry 3".into(),
        since: later(T0, 200),
        retried_by: Some("build-2".into()),
    };
    commit(
        &log,
        vec![Change::Quarantine {
            run: r1.clone(),
            quarantine: retried.clone(),
        }],
    )
    .await;
    assert_eq!(
        log.run(&r1).await.expect("run").expect("run").quarantine,
        Some(retried)
    );
}

pub async fn watchers_are_kept_once<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let (child, starter, fork) = (run(1), run(2), run(3));
    commit(&log, vec![create(&child), create(&starter), create(&fork)]).await;
    let watch = |watcher: &RunRef, attached: bool| Change::Watch {
        run: child.clone(),
        watcher: Watcher {
            run: watcher.id,
            seq: Seq(4),
            attached,
        },
    };
    commit(
        &log,
        vec![
            watch(&fork, false),
            watch(&starter, true),
            watch(&starter, false),
        ],
    )
    .await;
    assert_eq!(
        log.watchers(&child).await.expect("watchers"),
        [
            Watcher {
                run: starter.id,
                seq: Seq(4),
                attached: true
            },
            Watcher {
                run: fork.id,
                seq: Seq(4),
                attached: false
            },
        ],
        "by run id, a second watch changing nothing"
    );
    assert_eq!(log.watchers(&starter).await.expect("watchers"), []);
}

pub async fn webhook_tokens_resolve_to_their_run<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let (r1, r2) = (run(1), run(2));
    let (first, second) = (Digest::of(b"first"), Digest::of(b"second"));
    commit(&log, vec![create(&r1), create(&r2)]).await;
    let token = |run: &RunRef, name: &str, hash: Digest| Change::WebhookToken {
        run: run.clone(),
        name: name.into(),
        hash,
    };
    commit(&log, vec![token(&r1, "approval", first)]).await;
    assert_eq!(
        log.webhook(first).await.expect("webhook"),
        Some(WebhookTarget {
            run: r1.clone(),
            name: "approval".into()
        })
    );
    assert_eq!(log.webhook(second).await.expect("webhook"), None);
    let error = refused(&log, vec![token(&r2, "approval", first)]).await;
    assert!(is_conflict(&error), "a token bound to two runs: {error:?}");
    commit(
        &log,
        vec![token(&r1, "approval", first), token(&r1, "payment", second)],
    )
    .await;
    assert_eq!(
        log.webhook(second)
            .await
            .expect("webhook")
            .map(|target| target.name),
        Some("payment".into())
    );
}

// -------------------------------------------------------------------------------------------
// Recovery, seal, erasure

pub async fn recover_returns_what_an_owner_needs<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let (runnable, quarantined, active, unsealed, sealed) =
        (run(1), run(2), run(3), run(4), run(5));
    let quarantine = Quarantine {
        kind: QuarantineKind::NonDeterminism,
        reason: "mismatch".into(),
        since: T0,
        retried_by: None,
    };
    commit(
        &log,
        vec![
            create(&runnable),
            create(&quarantined),
            create(&active),
            create(&unsealed),
            create(&sealed),
            enqueue(&runnable, started_item(1), None),
            enqueue(&quarantined, started_item(1), None),
            Change::Quarantine {
                run: quarantined.clone(),
                quarantine: quarantine.clone(),
            },
            enqueue(&active, started_item(1), None),
        ],
    )
    .await;
    let mut activation = activation(
        &log,
        &active,
        vec![run_started_entry()],
        vec![1],
        RunStatus::Active,
    )
    .await;
    activation.tasks = vec![
        task(&active, 1, ready(T0)),
        task(
            &active,
            2,
            TaskState::Deferred {
                until: later(T0, DAY),
            },
        ),
        task(&active, 3, TaskState::Approval),
        task(
            &active,
            4,
            TaskState::Parked(ParkReason {
                kind: ParkKind::SchemaChanged,
                message: "x".into(),
            }),
        ),
    ];
    activation.timers = vec![
        set(later(T0, 10), script_timer(&active, 5)),
        set(later(T0, 2 * HOUR), script_timer(&active, 6)),
        set(
            later(T0, DAY),
            TimerKind::RunDeadline {
                run: active.clone(),
            },
        ),
    ];
    let active_head = activation.head.clone();
    commit(&log, vec![activation.into()]).await;
    end(&log, &unsealed).await;
    end(&log, &sealed).await;
    let state = OrganizationState {
        alerts: vec![alert(Some(alice()))],
        admissions: Vec::new(),
    };
    commit(
        &log,
        vec![
            Change::Seal {
                run: sealed.clone(),
                attempt: Attempt::FIRST,
            },
            OutboxChange::Add(Box::new(outbox_entry(1, Some(&active)))).into(),
            OutboxChange::Add(Box::new(outbox_entry(2, None))).into(),
            OutboxChange::Remove(OutboxId(1)).into(),
            Change::Counters(vec![hour_delta(&acme(), &bob(), Hour::of(T0), created(1))]),
            Change::Organization {
                tenant: acme(),
                state: state.clone(),
            },
        ],
    )
    .await;

    let recovery = log.recover(later(T0, HOUR)).await.expect("recover");
    assert_eq!(
        recovery
            .runnable
            .iter()
            .map(|head| head.run.clone())
            .collect::<Vec<_>>(),
        std::slice::from_ref(&runnable),
        "a non-empty inbox, not quarantined, not ended"
    );
    assert_eq!(recovery.runnable[0], head(&runnable, &alice()));
    let mut quarantined_head = head(&quarantined, &alice());
    quarantined_head.quarantine = Some(quarantine);
    assert_eq!(recovery.quarantined, [quarantined_head]);
    assert_eq!(
        recovery.timers,
        [DueTimer {
            at: later(T0, 10),
            kind: script_timer(&active, 5)
        }],
        "the timers due before the horizon"
    );
    assert_eq!(
        recovery
            .tasks
            .iter()
            .map(|task| task.task.task.seq.0)
            .collect::<Vec<_>>(),
        [1, 2, 3],
        "the tasks ready, deferred or waiting for their approval"
    );
    assert_eq!(
        recovery.unsealed,
        [(unsealed.clone(), Attempt::FIRST)],
        "the attempts that ended and are not sealed"
    );
    assert_eq!(recovery.outbox, [outbox_entry(2, None)]);
    let counted: Vec<(String, u64, u64)> = recovery
        .counters
        .iter()
        .map(|counted| {
            assert_eq!(counted.tenant, acme());
            (
                counted.namespace.to_string(),
                counted.gauges.active_runs,
                counted.gauges.distinct_scripts(),
            )
        })
        .collect();
    assert_eq!(
        counted,
        [("alice".to_owned(), 3, 1), ("bob".to_owned(), 0, 0)],
        "every namespace with something counted, with its gauges"
    );
    assert_eq!(recovery.organizations, BTreeMap::from([(acme(), state)]));
    assert_eq!(
        log.run(&active).await.expect("run"),
        Some(RunHead {
            history_len: 1,
            ..active_head
        })
    );
}

pub async fn seal_keeps_the_head_and_the_summary<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let r1 = run(1);
    commit(
        &log,
        vec![
            create_in(&r1, &alice(), Some("start")),
            enqueue(&r1, started_item(1), Some("signal-key")),
        ],
    )
    .await;
    let mut first = activation(
        &log,
        &r1,
        vec![run_started_entry()],
        vec![1],
        RunStatus::Active,
    )
    .await;
    first.payloads = vec![payload("large")];
    commit(&log, vec![first.into()]).await;
    end(&log, &r1).await;
    let ended = log.run(&r1).await.expect("run").expect("run");
    let summary = log.summary(&r1).await.expect("summary");
    assert!(ended.is_unsealed());

    let seal = Change::Seal {
        run: r1.clone(),
        attempt: Attempt::FIRST,
    };
    commit(&log, vec![seal.clone()]).await;
    assert_eq!(
        log.history(&r1, Attempt::FIRST, 0).await.expect("history"),
        [],
        "its history"
    );
    assert_eq!(
        log.payload(&r1, payload("large").digest)
            .await
            .expect("payload"),
        None,
        "its payload records"
    );
    assert!(
        !log.signal_key(&r1, "signal-key").await.expect("signal_key"),
        "its signal keys"
    );
    let sealed = log.run(&r1).await.expect("run").expect("the head stays");
    assert_eq!(
        sealed,
        RunHead {
            sealed: Some(Attempt::FIRST),
            ..ended
        }
    );
    assert!(!sealed.is_unsealed());
    assert_eq!(
        log.summary(&r1).await.expect("summary"),
        summary,
        "its summary stays"
    );
    let found = log
        .search(
            &Scope::Tenant(acme()),
            &RunQuery::new(acme()),
            RunOrder::Started,
            &Page::first(10),
        )
        .await
        .expect("search");
    assert_eq!(
        ids(&found.summaries),
        std::slice::from_ref(&r1),
        "and its index entries"
    );
    assert_eq!(
        log.started(&acme(), &alice(), "start")
            .await
            .expect("started"),
        Some(r1.id)
    );
    assert!(log.recover(T0).await.expect("recover").unsealed.is_empty());
    commit(&log, vec![seal]).await;
    assert_eq!(
        log.run(&r1).await.expect("run").expect("run").sealed,
        Some(Attempt::FIRST),
        "a second seal changes nothing"
    );
}

pub async fn erase_deletes_everything_of_the_run<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let (erased, kept) = (run(1), run(2));
    let token = Digest::of(b"token");
    commit(
        &log,
        vec![
            create_in(&erased, &alice(), Some("start")),
            create(&kept),
            enqueue(&erased, started_item(1), None),
        ],
    )
    .await;
    let mut first = activation(
        &log,
        &erased,
        vec![run_started_entry()],
        vec![1],
        RunStatus::Active,
    )
    .await;
    first.payloads = vec![payload("large")];
    first.outbox = vec![outbox_entry(1, Some(&erased)), outbox_entry(2, Some(&kept))];
    commit(
        &log,
        vec![
            first.into(),
            Change::Watch {
                run: erased.clone(),
                watcher: Watcher {
                    run: kept.id,
                    seq: Seq(1),
                    attached: true,
                },
            },
            Change::WebhookToken {
                run: erased.clone(),
                name: "hook".into(),
                hash: token,
            },
            set(
                later(T0, DAY),
                TimerKind::RunDeadline {
                    run: erased.clone(),
                },
            )
            .into(),
            set(later(T0, DAY), TimerKind::RunDeadline { run: kept.clone() }).into(),
            set(
                later(T0, 1_000),
                TimerKind::WebhookRetry {
                    tenant: acme(),
                    entry: OutboxId(1),
                },
            )
            .into(),
        ],
    )
    .await;
    end(&log, &erased).await;
    commit(
        &log,
        vec![
            set(
                later(T0, 365 * DAY),
                TimerKind::RetentionEnd {
                    run: erased.clone(),
                },
            )
            .into(),
        ],
    )
    .await;
    assert_eq!(
        log.gauges(&Scope::Tenant(acme()))
            .await
            .expect("gauges")
            .scripts
            .get(&script("script")),
        Some(&2)
    );

    commit(
        &log,
        vec![Change::Erase {
            run: erased.clone(),
        }],
    )
    .await;
    assert_eq!(log.run(&erased).await.expect("run"), None);
    assert_eq!(log.summary(&erased).await.expect("summary"), None);
    assert_eq!(
        log.history(&erased, Attempt::FIRST, 0)
            .await
            .expect("history"),
        []
    );
    assert_eq!(log.watchers(&erased).await.expect("watchers"), []);
    assert_eq!(
        log.payload(&erased, payload("large").digest)
            .await
            .expect("payload"),
        None
    );
    assert_eq!(log.webhook(token).await.expect("webhook"), None);
    assert_eq!(
        log.started(&acme(), &alice(), "start")
            .await
            .expect("started"),
        None,
        "its start key"
    );
    assert_eq!(
        timers_of(&log).await,
        [DueTimer {
            at: later(T0, DAY),
            kind: TimerKind::RunDeadline { run: kept.clone() }
        }],
        "its timers, its retention and its outbox entries' retries"
    );
    assert_eq!(
        log.recover(T0).await.expect("recover").outbox,
        [outbox_entry(2, Some(&kept))],
        "its outbox entries"
    );
    let gauges = log.gauges(&Scope::Tenant(acme())).await.expect("gauges");
    assert_eq!(
        (gauges.active_runs, gauges.scripts.get(&script("script"))),
        (1, Some(&1)),
        "what it held in the gauges"
    );
    let found = log
        .search(
            &Scope::Tenant(acme()),
            &RunQuery::new(acme()),
            RunOrder::Started,
            &Page::first(10),
        )
        .await
        .expect("search");
    assert_eq!(
        ids(&found.summaries),
        vec![kept.clone()],
        "and in the index: only the run kept is left"
    );
    commit(
        &log,
        vec![
            Change::Erase {
                run: erased.clone(),
            },
            create_in(&run(3), &alice(), Some("start")),
        ],
    )
    .await;
    assert_eq!(
        log.started(&acme(), &alice(), "start")
            .await
            .expect("started"),
        Some(run(3).id),
        "an erased run's start key starts another run"
    );
}

// -------------------------------------------------------------------------------------------
// The index

fn summary(run: &RunRef, namespace: &NamespaceName, status: RunStatus, calls: u64) -> RunSummary {
    let mut summary = summary_of(run, namespace, status).summary;
    summary.tool_calls = calls;
    summary
}

pub async fn search_reads_the_scope_in_order_by_pages<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let runs: Vec<RunRef> = (1..=6).map(run).collect();
    let statuses = [
        RunStatus::Completed,
        RunStatus::Active,
        RunStatus::Failed,
        RunStatus::Completed,
        RunStatus::Active,
        RunStatus::Completed,
    ];
    let calls = [3, 9, 1, 9, 0, 5];
    for (n, run) in runs.iter().enumerate() {
        let namespace = if n % 2 == 0 { alice() } else { bob() };
        index(
            &log,
            run,
            &namespace,
            summary(run, &namespace, statuses[n], calls[n]),
        )
        .await;
    }
    let other = run_of(&tenant("other"), 8);
    index(
        &log,
        &other,
        &alice(),
        summary(&other, &alice(), RunStatus::Completed, 0),
    )
    .await;

    let tenant_scope = Scope::Tenant(acme());
    let reader = &log;
    let search = move |scope: Scope, query: RunQuery, order: RunOrder, page: Page| async move {
        reader
            .search(&scope, &query, order, &page)
            .await
            .expect("search")
    };
    let all = search(
        tenant_scope.clone(),
        RunQuery::new(acme()),
        RunOrder::Started,
        Page::first(10),
    )
    .await;
    assert_eq!(
        ids(&all.summaries),
        runs.iter().rev().cloned().collect::<Vec<_>>(),
        "newest first; another tenant's is not in scope"
    );
    assert_eq!(all.cursor, None, "the whole scope was read");
    let oldest = search(
        tenant_scope.clone(),
        RunQuery::new(acme()),
        RunOrder::Oldest,
        Page::first(10),
    )
    .await;
    assert_eq!(ids(&oldest.summaries), runs);
    let by_calls = search(
        tenant_scope.clone(),
        RunQuery::new(acme()),
        RunOrder::Calls,
        Page::first(10),
    )
    .await;
    assert_eq!(
        ids(&by_calls.summaries),
        [&runs[3], &runs[1], &runs[5], &runs[0], &runs[2], &runs[4]].map(Clone::clone),
        "most calls first, then newest"
    );
    let completed = RunQuery::new(acme()).with(Predicate::Status([RunStatus::Completed].into()));
    let found = search(
        tenant_scope.clone(),
        completed.clone(),
        RunOrder::Oldest,
        Page::first(10),
    )
    .await;
    assert_eq!(
        ids(&found.summaries),
        [&runs[0], &runs[3], &runs[5]].map(Clone::clone),
        "every term holds"
    );
    let not_completed =
        RunQuery::new(acme()).without(Predicate::Status([RunStatus::Completed].into()));
    let found = search(
        tenant_scope.clone(),
        not_completed,
        RunOrder::Oldest,
        Page::first(10),
    )
    .await;
    assert_eq!(
        ids(&found.summaries),
        [&runs[1], &runs[2], &runs[4]].map(Clone::clone),
        "a negated term"
    );
    let in_alice = Scope::Namespace {
        tenant: acme(),
        namespace: alice(),
    };
    let found = search(in_alice, completed, RunOrder::Oldest, Page::first(10)).await;
    assert_eq!(
        ids(&found.summaries),
        [&runs[0]].map(Clone::clone),
        "a namespace's scope"
    );
    let found = search(
        tenant_scope.clone(),
        RunQuery::new(tenant("other")),
        RunOrder::Oldest,
        Page::first(10),
    )
    .await;
    assert!(
        found.summaries.is_empty(),
        "a query of another tenant than the scope's"
    );

    let mut pages = Vec::new();
    let mut page = Page::first(4);
    loop {
        let found = search(
            tenant_scope.clone(),
            RunQuery::new(acme()),
            RunOrder::Oldest,
            page,
        )
        .await;
        pages.push(ids(&found.summaries));
        match found.cursor {
            Some(cursor) => page = Page::after(cursor, 4),
            None => break,
        }
    }
    assert_eq!(
        pages,
        [runs[..4].to_vec(), runs[4..].to_vec()],
        "a cursor resumes after the last run of its page"
    );
    let exact = search(
        tenant_scope.clone(),
        RunQuery::new(acme()),
        RunOrder::Oldest,
        Page::first(6),
    )
    .await;
    assert_eq!(
        (exact.summaries.len(), exact.cursor),
        (6, None),
        "a page that reached the end has no cursor"
    );
    let first = search(
        tenant_scope.clone(),
        RunQuery::new(acme()),
        RunOrder::Calls,
        Page::first(2),
    )
    .await;
    let rest = search(
        tenant_scope,
        RunQuery::new(acme()),
        RunOrder::Calls,
        Page::after(first.cursor.expect("more"), 10),
    )
    .await;
    assert_eq!(
        [ids(&first.summaries), ids(&rest.summaries)].concat(),
        ids(&by_calls.summaries),
        "pages keep the order asked"
    );
}

/// A run is listed from its creation, `pending` until its first activation.
pub async fn a_pending_run_is_listed<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let pending = run(1);
    commit(&log, vec![create(&pending)]).await;
    let found = log
        .search(
            &Scope::Tenant(acme()),
            &RunQuery::new(acme()).with(Predicate::Status([RunStatus::Pending].into())),
            RunOrder::Started,
            &Page::first(10),
        )
        .await
        .expect("search");
    assert_eq!(ids(&found.summaries), vec![pending.clone()]);
    let summary = log
        .summary(&pending)
        .await
        .expect("summary")
        .expect("summary");
    assert_eq!(summary.summary.status, RunStatus::Pending);
}

/// A namespace's counters, shapes and start keys go once the partition holds none of its runs.
pub async fn erase_namespace_drops_its_counters_once_it_holds_no_run<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let kept = run(1);
    commit(
        &log,
        vec![create_with(head(&kept, &alice()), Some("start-1"))],
    )
    .await;
    let erase_alice = Change::EraseNamespace {
        tenant: acme(),
        namespace: alice(),
    };
    let refused = log.commit(Batch::new().with(erase_alice.clone())).await;
    assert!(
        matches!(refused, Err(PartitionError::Conflict(_))),
        "a namespace that still holds a run is not erased: {refused:?}"
    );
    commit(&log, vec![Change::Erase { run: kept.clone() }]).await;
    commit(&log, vec![erase_alice]).await;
    assert_eq!(
        log.started(&acme(), &alice(), "start-1")
            .await
            .expect("started"),
        None
    );
    let window = Window::covering(T0, later(T0, DAY));
    let counters = log
        .counters(
            &Scope::Namespace {
                tenant: acme(),
                namespace: alice(),
            },
            window,
        )
        .await
        .expect("counters");
    assert_eq!(
        counters,
        Counters::default(),
        "nothing is left of alice's counters"
    );
}

pub async fn search_stops_at_its_budget_with_a_cursor<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let runs: Vec<RunRef> = (1..=5).map(run).collect();
    for (n, run) in runs.iter().enumerate() {
        let status = if n == 0 {
            RunStatus::Failed
        } else {
            RunStatus::Completed
        };
        index(&log, run, &alice(), summary(run, &alice(), status, 0)).await;
    }
    let failed = RunQuery::new(acme()).with(Predicate::Status([RunStatus::Failed].into()));
    let scope = Scope::Tenant(acme());
    let mut page = Page {
        cursor: None,
        limit: 10,
        budget: 2,
    };
    let mut seen = Vec::new();
    let mut cursors = Vec::new();
    loop {
        let found = log
            .search(&scope, &failed, RunOrder::Started, &page)
            .await
            .expect("search");
        seen.extend(ids(&found.summaries));
        match found.cursor {
            Some(cursor) => {
                cursors.push(cursor);
                page.cursor = Some(cursor);
            }
            None => break,
        }
    }
    assert_eq!(
        seen,
        [runs[0].clone()],
        "every page together finds what matches"
    );
    assert_eq!(
        cursors,
        [
            SearchCursor {
                key: None,
                run: runs[3].id
            },
            SearchCursor {
                key: None,
                run: runs[1].id
            }
        ],
        "a page cut by the budget comes back short, with where it stopped"
    );
}

pub async fn counters_sum_whole_hours_of_the_scope<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let hour = Hour::of(T0);
    let failure = |run: u64, at: u64| FailureCounts {
        signature: "TypeError: <n>".into(),
        error: "TypeError".into(),
        runs: 1,
        scripts: BTreeMap::from([(script("script"), 1)]),
        lines: BTreeMap::from([(7, 1)]),
        latest: vec![LatestRun {
            run: self::run(run).id,
            ended_at: later(T0, at),
            status: RunStatus::Failed,
            message: "x is not a function".into(),
        }],
    };
    let mut first = created(1);
    first.cpu = Duration::from_millis(1_500);
    first.failures = BTreeMap::from([("f1".to_owned(), failure(1, 10))]);
    first.tools = BTreeMap::from([(
        "github.close_issue".to_owned(),
        ToolCounts {
            calls: 2,
            completed: 1,
            failed: 1,
            errors: BTreeMap::from([(ToolFailureKind::Timeout, 1)]),
            retries: 1,
            latency: crate::ports::Sketch::of([120]),
        },
    )]);
    let mut second = created(2);
    second.failures = BTreeMap::from([("f1".to_owned(), failure(2, 20))]);
    commit(
        &log,
        vec![Change::Counters(vec![
            hour_delta(&acme(), &alice(), hour, first),
            hour_delta(&acme(), &alice(), hour.next(), second),
            hour_delta(&acme(), &bob(), hour.next(), created(4)),
            hour_delta(&acme(), &alice(), Hour(hour.0 + 3), created(8)),
            hour_delta(&tenant("other"), &alice(), hour, created(16)),
        ])],
    )
    .await;
    let third = Counters {
        created: 32,
        ..Counters::default()
    };
    commit(
        &log,
        vec![Change::Counters(vec![hour_delta(
            &acme(),
            &alice(),
            hour.next(),
            third,
        )])],
    )
    .await;

    let in_alice = Scope::Namespace {
        tenant: acme(),
        namespace: alice(),
    };
    let window = Window::covering(
        later(hour.start(), HOUR / 2),
        later(hour.next().start(), 60_000),
    );
    assert_eq!(
        window,
        Window::hours(hour, Hour(hour.0 + 2)),
        "rounded out to whole hours"
    );
    let counters = log.counters(&in_alice, window).await.expect("counters");
    assert_eq!(
        counters.created, 35,
        "the hours of the window, deltas of an hour added"
    );
    assert_eq!(counters.cpu, Duration::from_millis(1_500));
    let group = &counters.failures["f1"];
    assert_eq!((group.runs, group.lines[&7]), (2, 2));
    assert_eq!(group.latest[0].run, run(2).id, "the latest run first");
    assert_eq!(counters.tools["github.close_issue"].latency.count, 1);
    assert_eq!(
        counters.namespaces,
        BTreeMap::from([(
            alice(),
            RunCounts {
                started: 3,
                ended: BTreeMap::new()
            }
        )])
    );

    let tenant_wide = log
        .counters(&Scope::Tenant(acme()), window)
        .await
        .expect("counters");
    assert_eq!(tenant_wide.created, 39, "the scope sums its namespaces");
    assert_eq!(
        tenant_wide
            .namespaces
            .iter()
            .map(|(name, runs)| (name.to_string(), runs.started))
            .collect::<Vec<_>>(),
        [("alice".to_owned(), 3), ("bob".to_owned(), 4)],
        "with the runs of each namespace"
    );
    assert_eq!(
        log.counters(
            &Scope::Tenant(acme()),
            Window::hours(hour, Hour(hour.0 + 4))
        )
        .await
        .expect("counters")
        .created,
        47
    );
    assert_eq!(
        log.counters(&Scope::Tenant(tenant("other")), window)
            .await
            .expect("counters")
            .created,
        16
    );
    assert!(
        log.counters(
            &Scope::Tenant(acme()),
            Window::hours(Hour(hour.0 + 2), Hour(hour.0 + 3))
        )
        .await
        .expect("counters")
        .is_empty(),
        "an hour without counts"
    );
    assert_eq!(
        log.counters(&in_alice, Window::of_month(Month::of(T0)))
            .await
            .expect("counters")
            .created,
        43,
        "a month is a window"
    );
}

pub async fn gauges_count_what_holds_now<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let wait = |kind: WaitKind, name: &str, task: Option<TaskPhase>| Wait {
        kind,
        seq: Some(Seq(1)),
        name: Some(name.into()),
        since: T0,
        until: None,
        callsite: None,
        task,
    };
    let (pending, signal_wait, approval, quarantined, ended, elsewhere) =
        (run(1), run(2), run(3), run(4), run(5), run(6));
    commit(&log, vec![create_with(head(&pending, &alice()), None)]).await;
    let mut waiting = summary(&signal_wait, &alice(), RunStatus::Active, 0);
    waiting.waits = vec![wait(WaitKind::Signal, "approval", None)];
    index(&log, &signal_wait, &alice(), waiting).await;
    let mut two_waits = summary(&approval, &alice(), RunStatus::Active, 1);
    two_waits.waits = vec![
        wait(WaitKind::Call, "github.merge", Some(TaskPhase::Approval)),
        wait(WaitKind::Signal, "approval", None),
        wait(WaitKind::Signal, "other", None),
    ];
    index(&log, &approval, &alice(), two_waits).await;
    commit(
        &log,
        vec![create_with(
            RunHead {
                script: script("second"),
                ..head(&quarantined, &alice())
            },
            None,
        )],
    )
    .await;
    commit(
        &log,
        vec![Change::Quarantine {
            run: quarantined.clone(),
            quarantine: Quarantine {
                kind: QuarantineKind::Failures,
                reason: "x".into(),
                since: T0,
                retried_by: None,
            },
        }],
    )
    .await;
    commit(
        &log,
        vec![create_with(
            RunHead {
                script: script("third"),
                ..head(&ended, &alice())
            },
            None,
        )],
    )
    .await;
    end(&log, &ended).await;
    commit(&log, vec![create_in(&elsewhere, &bob(), None)]).await;

    let in_alice = Scope::Namespace {
        tenant: acme(),
        namespace: alice(),
    };
    let gauges = log.gauges(&in_alice).await.expect("gauges");
    assert_eq!(
        gauges,
        Gauges {
            active_runs: 4,
            waiting: BTreeMap::from([(WaitingFor::Signal, 2), (WaitingFor::Approval, 1)]),
            quarantined: 1,
            scripts: BTreeMap::from([
                (script("script"), 3),
                (script("second"), 1),
                (script("third"), 1)
            ]),
        },
        "a run waiting for a kind twice counts once in it"
    );
    assert_eq!(gauges.distinct_scripts(), 3);
    assert_eq!(gauges.waiting_for(WaitingFor::Timer), 0);
    let tenant_wide = log.gauges(&Scope::Tenant(acme())).await.expect("gauges");
    assert_eq!(tenant_wide.active_runs, 5);
    commit(&log, vec![Change::Erase { run: ended }]).await;
    assert_eq!(
        log.gauges(&in_alice)
            .await
            .expect("gauges")
            .scripts
            .get(&script("third")),
        None,
        "an erased run is kept no more"
    );
    assert_eq!(
        log.gauges(&Scope::Tenant(tenant("other")))
            .await
            .expect("gauges"),
        Gauges::default()
    );
}

pub async fn shapes_merge_per_tool<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let shape = |value: serde_json::Value| Shape::of(&value);
    let observed =
        |namespace: &NamespaceName, tool: &str, value: serde_json::Value| CounterDelta::Shape {
            tenant: acme(),
            namespace: namespace.clone(),
            tool: tool.into(),
            shape: shape(value),
        };
    commit(
        &log,
        vec![Change::Counters(vec![
            observed(&alice(), "t", json!({ "a": 1 })),
            observed(&alice(), "t", json!({ "b": "x" })),
            observed(&bob(), "t", json!({ "c": true })),
            observed(&alice(), "u", json!(1)),
        ])],
    )
    .await;
    let tools = ["t".to_owned(), "v".to_owned()];
    assert_eq!(
        log.shapes(&acme(), Some(&alice()), &tools)
            .await
            .expect("shapes"),
        BTreeMap::from([(
            "t".to_owned(),
            shape(json!({ "a": 1 })).merge(shape(json!({ "b": "x" })))
        )]),
        "a namespace's, for the tools observed"
    );
    assert_eq!(
        log.shapes(&acme(), None, &tools).await.expect("shapes"),
        BTreeMap::from([(
            "t".to_owned(),
            shape(json!({ "c": true }))
                .merge(shape(json!({ "b": "x" })))
                .merge(shape(json!({ "a": 1 })))
        )]),
        "across the tenant's namespaces, in any order"
    );
    assert!(
        log.shapes(&tenant("other"), None, &tools)
            .await
            .expect("shapes")
            .is_empty()
    );
}

// -------------------------------------------------------------------------------------------
// Timers, organizations, outbox, tenants

pub async fn timers_are_ordered_and_read_by_range<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let (r1, r2) = (run(1), run(2));
    commit(&log, vec![create(&r1), create(&r2)]).await;
    commit(
        &log,
        vec![
            set(later(T0, 10), script_timer(&r2, 1)).into(),
            set(later(T0, 10), TimerKind::RunDeadline { run: r1.clone() }).into(),
            set(later(T0, 10), script_timer(&r1, 2)).into(),
            set(later(T0, 5), script_timer(&r1, 1)).into(),
            set(
                later(T0, 20),
                TimerKind::CallTimeout {
                    run: r1.clone(),
                    seq: Seq(1),
                },
            )
            .into(),
            set(later(T0, 15), script_timer(&r1, 1)).into(),
        ],
    )
    .await;
    let due = |at: u64, kind: TimerKind| DueTimer {
        at: later(T0, at),
        kind,
    };
    assert_eq!(
        timers_of(&log).await,
        [
            due(10, script_timer(&r1, 2)),
            due(10, TimerKind::RunDeadline { run: r1.clone() }),
            due(10, script_timer(&r2, 1)),
            due(15, script_timer(&r1, 1)),
            due(
                20,
                TimerKind::CallTimeout {
                    run: r1.clone(),
                    seq: Seq(1)
                }
            ),
        ],
        "by due time, then run and kind; a timer set again moves"
    );
    assert_eq!(
        log.timers(later(T0, 10), later(T0, 15))
            .await
            .expect("timers")
            .len(),
        3,
        "from included, to excluded"
    );
    assert_eq!(
        log.timers(later(T0, 21), later(T0, 99))
            .await
            .expect("timers"),
        []
    );
    commit(
        &log,
        vec![
            TimerChange::Cancel(script_timer(&r1, 2)).into(),
            TimerChange::Cancel(script_timer(&r1, 9)).into(),
        ],
    )
    .await;
    assert_eq!(
        log.timers(later(T0, 10), later(T0, 11))
            .await
            .expect("timers"),
        [
            due(10, TimerKind::RunDeadline { run: r1.clone() }),
            due(10, script_timer(&r2, 1))
        ],
        "a cancelled timer is gone; cancelling one not set changes nothing"
    );
}

pub async fn organizations_are_kept<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    assert_eq!(log.organization(&acme()).await.expect("organization"), None);
    let first = OrganizationState {
        alerts: Vec::new(),
        admissions: vec![Admission {
            run: run(1).id,
            namespace: alice(),
            script: script("script"),
            admitted_at: T0,
        }],
    };
    commit(
        &log,
        vec![Change::Organization {
            tenant: acme(),
            state: first,
        }],
    )
    .await;
    let second = OrganizationState {
        alerts: vec![alert(Some(alice())), alert(None)],
        admissions: Vec::new(),
    };
    commit(
        &log,
        vec![Change::Organization {
            tenant: acme(),
            state: second.clone(),
        }],
    )
    .await;
    assert_eq!(
        log.organization(&acme()).await.expect("organization"),
        Some(second),
        "a state written again replaces the first"
    );
    assert_eq!(
        log.organization(&tenant("other"))
            .await
            .expect("organization"),
        None
    );
}

pub async fn outbox_entries_are_added_and_removed<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let r1 = run(1);
    commit(&log, vec![create(&r1)]).await;
    let quota = OutboxEntry {
        projection: Projection::QuotaAlert(alert(None)),
        ..outbox_entry(3, None)
    };
    commit(
        &log,
        vec![
            OutboxChange::Add(Box::new(outbox_entry(2, Some(&r1)))).into(),
            OutboxChange::Add(Box::new(quota.clone())).into(),
            OutboxChange::Add(Box::new(outbox_entry(1, Some(&r1)))).into(),
            set(
                later(T0, 1_000),
                TimerKind::WebhookRetry {
                    tenant: acme(),
                    entry: OutboxId(3),
                },
            )
            .into(),
        ],
    )
    .await;
    assert_eq!(
        log.recover(T0).await.expect("recover").outbox,
        [
            outbox_entry(1, Some(&r1)),
            outbox_entry(2, Some(&r1)),
            quota.clone()
        ],
        "by id"
    );
    commit(
        &log,
        vec![
            OutboxChange::Remove(OutboxId(1)).into(),
            OutboxChange::Remove(OutboxId(9)).into(),
        ],
    )
    .await;
    assert_eq!(
        log.recover(T0).await.expect("recover").outbox,
        [outbox_entry(2, Some(&r1)), quota],
        "removing an entry that is not there changes nothing"
    );
    commit(&log, vec![OutboxChange::Remove(OutboxId(3)).into()]).await;
    assert_eq!(
        timers_of(&log).await,
        [],
        "a delivered entry's retry goes with it"
    );
}

pub async fn runs_of_another_tenant_are_invisible<S: PartitionStore>(store: &S) {
    let log = open(store, "n1").await;
    let r1 = run(1);
    let foreign = RunRef::new(tenant("other"), r1.id);
    commit(
        &log,
        vec![
            create_in(&r1, &alice(), Some("k")),
            enqueue(&r1, started_item(1), Some("signal")),
        ],
    )
    .await;
    activate(
        &log,
        &r1,
        vec![run_started_entry()],
        Vec::new(),
        RunStatus::Active,
    )
    .await;
    assert_eq!(log.run(&foreign).await.expect("run"), None);
    assert_eq!(log.summary(&foreign).await.expect("summary"), None);
    assert_eq!(log.inbox(&foreign).await.expect("inbox"), []);
    assert_eq!(
        log.history(&foreign, Attempt::FIRST, 0)
            .await
            .expect("history"),
        []
    );
    assert!(
        !log.signal_key(&foreign, "signal")
            .await
            .expect("signal_key")
    );
    assert_eq!(
        log.started(&tenant("other"), &alice(), "k")
            .await
            .expect("started"),
        None
    );
    let error = refused(&log, vec![enqueue(&foreign, signal(2, "x"), None)]).await;
    assert!(is_conflict(&error), "{error:?}");
    commit(&log, vec![Change::Erase { run: foreign }]).await;
    assert!(
        log.run(&r1).await.expect("run").is_some(),
        "erasing another tenant's run leaves this one"
    );
    let found = log
        .search(
            &Scope::Tenant(tenant("other")),
            &RunQuery::new(tenant("other")),
            RunOrder::Started,
            &Page::first(10),
        )
        .await
        .expect("search");
    assert!(found.summaries.is_empty());
}
