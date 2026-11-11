//! The conformance suite every `JournalStore` adapter must pass (feature `conformance`).
//!
//! It checks the contract of the port, not a table layout: the commit invariant and fencing,
//! single writers, idempotency keys, epochs, quarantine, timers, run expiry and tool tasks.
//! An adapter's test calls [`run_all`] with a function that creates an empty store:
//!
//! ```ignore
//! #[tokio::test]
//! async fn conformance() {
//!     ptc_engine::conformance::run_all(MyStore::new_empty).await;
//! }
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use ptc_core::{
    CancelReason, Command, Completion, Digest, Epoch, Event, HistoryEntry, Lifecycle, Payload,
    RunId, RunMode, RunStarted, RunStatus, Script, Seed, Seq, Timestamp, Toolset,
};
use serde_json::json;

use crate::ports::{
    Activation, CompiledScript, Enqueued, ExpiredRuns, JournalStore, Lease, NewRun, NewTimer,
    NewToolTask, RunFilter, RunMetadata, StoreError, Trigger,
};

const T0: Timestamp = Timestamp(1_767_225_600_000);
const LEASE: Duration = Duration::from_secs(30);
const EPOCHS: &[Epoch] = &[Epoch(1)];

/// Runs every check, each on a new store.
pub async fn run_all<S: JournalStore>(new_store: impl Fn() -> S) {
    scripts_and_toolsets_are_stored_by_digest(&new_store()).await;
    create_run_enqueues_run_started(&new_store()).await;
    an_idempotency_key_returns_the_existing_run(&new_store()).await;
    a_run_has_one_writer_at_a_time(&new_store()).await;
    acquire_runnable_respects_epochs_quarantine_and_order(&new_store()).await;
    a_commit_appends_entries_consumes_the_inbox_and_writes_effects(&new_store()).await;
    a_commit_with_a_stale_fence_writes_nothing(&new_store()).await;
    a_commit_must_continue_the_history(&new_store()).await;
    a_terminal_commit_clears_the_run(&new_store()).await;
    enqueue_deduplicates_keys_and_refuses_ended_runs(&new_store()).await;
    quarantine_releases_the_lease_and_hides_the_run(&new_store()).await;
    timers_fire_once(&new_store()).await;
    runs_expire_once_and_quarantined_runs_are_reported(&new_store()).await;
    tool_tasks_are_claimed_reclaimed_and_completed_once(&new_store()).await;
    list_runs_filters_by_status_and_labels(&new_store()).await;
}

fn id(n: u128) -> RunId {
    RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0000 | n)
}

fn started(epoch: u32, expires_at: Timestamp) -> RunStarted {
    RunStarted {
        script: Digest::of(b"script"),
        toolset: Digest::of(b"toolset"),
        input: json!({ "n": 1 }).into(),
        seed: Seed::from_bytes([9; 32]),
        epoch: Epoch(epoch),
        expires_at,
        mode: RunMode::Mock,
    }
}

fn new_run(n: u128) -> NewRun {
    NewRun {
        id: id(n),
        started: started(1, Timestamp(T0.0 + 86_400_000)),
        metadata: RunMetadata::default(),
        created_at: T0,
    }
}

fn entry(index: u64, event: Event) -> HistoryEntry {
    HistoryEntry {
        index,
        recorded_at: Timestamp(T0.0 + index),
        event,
    }
}

fn run_started_entry() -> HistoryEntry {
    entry(
        0,
        Event::Lifecycle(Lifecycle::RunStarted(started(
            1,
            Timestamp(T0.0 + 86_400_000),
        ))),
    )
}

fn activation(entries: Vec<HistoryEntry>, consumed: Vec<u64>, status: RunStatus) -> Activation {
    Activation {
        entries,
        consumed,
        tool_calls: Vec::new(),
        timers: Vec::new(),
        cancelled_timers: Vec::new(),
        status,
    }
}

async fn create<S: JournalStore>(store: &S, n: u128) -> RunId {
    let created = store
        .create_run(new_run(n), None)
        .await
        .expect("create_run");
    assert!(created.created);
    created.run_id
}

async fn acquire<S: JournalStore>(store: &S, at: Timestamp) -> Option<Lease> {
    store
        .acquire_runnable("worker", EPOCHS, at, LEASE)
        .await
        .expect("acquire_runnable")
}

/// Activates the run's `run_started`: the run becomes active, the lease is released.
async fn start<S: JournalStore>(store: &S, n: u128) -> RunId {
    let run_id = create(store, n).await;
    let lease = store
        .acquire_run(run_id, "worker", T0, LEASE)
        .await
        .expect("acquire_run")
        .expect("a new run can be leased");
    let inbox = store.take_inbox(&lease).await.expect("take_inbox");
    let consumed = inbox.iter().map(|item| item.id).collect();
    store
        .commit_activation(
            &lease,
            activation(vec![run_started_entry()], consumed, RunStatus::Active),
        )
        .await
        .expect("commit_activation");
    store.release(&lease).await.expect("release");
    run_id
}

async fn scripts_and_toolsets_are_stored_by_digest<S: JournalStore>(store: &S) {
    let script = Script::new("export default () => 1");
    let compiled = CompiledScript {
        javascript: "export default () => 1".into(),
        source_map: "{}".into(),
    };
    assert_eq!(
        store.get_script(script.digest()).await.expect("get_script"),
        None
    );
    store
        .put_script(&script, &compiled)
        .await
        .expect("put_script");
    store
        .put_script(&script, &compiled)
        .await
        .expect("put_script is idempotent");
    let stored = store
        .get_script(script.digest())
        .await
        .expect("get_script")
        .expect("stored");
    assert_eq!((stored.script, stored.compiled), (script, compiled));

    let toolset = Toolset::from_names(["a", "b"]);
    store.put_toolset(&toolset).await.expect("put_toolset");
    store
        .put_toolset(&toolset)
        .await
        .expect("put_toolset is idempotent");
    assert_eq!(
        store
            .get_toolset(toolset.digest())
            .await
            .expect("get_toolset"),
        Some(toolset)
    );
}

async fn create_run_enqueues_run_started<S: JournalStore>(store: &S) {
    let run_id = create(store, 1).await;
    let record = store
        .get_run(run_id)
        .await
        .expect("get_run")
        .expect("the run exists");
    assert_eq!(record.status, RunStatus::Pending);
    assert_eq!(record.epoch, Epoch(1));
    assert_eq!(record.history_len, 0);
    assert_eq!(record.expires_at, Timestamp(T0.0 + 86_400_000));

    let lease = acquire(store, T0).await.expect("a pending run is runnable");
    let inbox = store.take_inbox(&lease).await.expect("take_inbox");
    assert_eq!(inbox.len(), 1);
    assert!(matches!(&inbox[0].trigger, Trigger::RunStarted(s) if *s == new_run(1).started));
    assert!(
        store
            .load_history(run_id, 0)
            .await
            .expect("load_history")
            .is_empty()
    );
    assert!(matches!(
        store.create_run(new_run(1), None).await,
        Err(StoreError::Conflict(_))
    ));
}

async fn an_idempotency_key_returns_the_existing_run<S: JournalStore>(store: &S) {
    let first = store
        .create_run(new_run(1), Some("tool-call-1".into()))
        .await
        .expect("create_run");
    let retry = store
        .create_run(new_run(2), Some("tool-call-1".into()))
        .await
        .expect("create_run");
    assert!(first.created);
    assert!(!retry.created);
    assert_eq!(retry.run_id, first.run_id);
    assert_eq!(store.get_run(id(2)).await.expect("get_run"), None);
    let record = store
        .get_run(first.run_id)
        .await
        .expect("get_run")
        .expect("exists");
    assert_eq!(record.idempotency_key.as_deref(), Some("tool-call-1"));
}

async fn a_run_has_one_writer_at_a_time<S: JournalStore>(store: &S) {
    let run_id = create(store, 1).await;
    let first = acquire(store, T0).await.expect("runnable");
    assert_eq!(
        acquire(store, T0).await,
        None,
        "a leased run is not runnable"
    );
    assert_eq!(
        store
            .acquire_run(run_id, "other", T0, LEASE)
            .await
            .expect("acquire_run"),
        None
    );

    // The lease expires: another worker takes over, and the first one is fenced off.
    let later = T0.saturating_add(LEASE + Duration::from_millis(1));
    let second = acquire(store, later)
        .await
        .expect("an expired lease can be taken over");
    assert!(second.fence > first.fence);
    assert!(matches!(
        store.take_inbox(&first).await,
        Err(StoreError::Fenced { .. })
    ));

    // Releasing a stale lease does nothing; releasing the live one frees the run.
    store.release(&first).await.expect("release");
    assert_eq!(acquire(store, later).await, None);
    store.release(&second).await.expect("release");
    let third = acquire(store, later).await.expect("released");
    assert!(third.fence > second.fence);
}

async fn acquire_runnable_respects_epochs_quarantine_and_order<S: JournalStore>(store: &S) {
    let mut epoch_2 = new_run(1);
    epoch_2.started.epoch = Epoch(2);
    store.create_run(epoch_2, None).await.expect("create_run");
    assert_eq!(
        acquire(store, T0).await,
        None,
        "epoch 2 is not offered for epoch 1"
    );
    let lease = store
        .acquire_runnable("worker", &[Epoch(1), Epoch(2)], T0, LEASE)
        .await
        .expect("acquire_runnable")
        .expect("epoch 2 is offered to a worker that supports it");
    store.release(&lease).await.expect("release");

    // The run whose oldest inbox item is oldest goes first.
    let older = create(store, 2).await;
    let newer = create(store, 3).await;
    let first = store
        .acquire_runnable("worker", &[Epoch(1)], T0, LEASE)
        .await
        .expect("acquire")
        .expect("runnable");
    assert_eq!(first.run_id, older);
    let second = acquire(store, T0).await.expect("runnable");
    assert_eq!(second.run_id, newer);
}

async fn a_commit_appends_entries_consumes_the_inbox_and_writes_effects<S: JournalStore>(
    store: &S,
) {
    let run_id = create(store, 1).await;
    let lease = acquire(store, T0).await.expect("runnable");
    let inbox = store.take_inbox(&lease).await.expect("take_inbox");
    let call = Command::ScheduleToolCall {
        seq: Seq(1),
        tool: "a".into(),
        args: json!({ "x": 1 }).into(),
        callsite: None,
    };
    let entries = vec![
        run_started_entry(),
        entry(1, Event::Command(call)),
        entry(
            2,
            Event::Command(Command::StartTimer {
                seq: Seq(2),
                duration_ms: 10,
                fire_at: Timestamp(T0.0 + 10),
                callsite: None,
            }),
        ),
    ];
    let mut commit = activation(entries.clone(), vec![inbox[0].id], RunStatus::Active);
    commit.tool_calls.push(NewToolTask {
        seq: Seq(1),
        tool: "a".into(),
        args: json!({ "x": 1 }).into(),
    });
    commit.timers.push(NewTimer {
        seq: Seq(2),
        fire_at: Timestamp(T0.0 + 10),
    });
    store
        .commit_activation(&lease, commit)
        .await
        .expect("commit_activation");

    assert_eq!(
        store.load_history(run_id, 0).await.expect("load_history"),
        entries
    );
    assert_eq!(
        store.load_history(run_id, 2).await.expect("load_history"),
        entries[2..]
    );
    assert!(
        store
            .take_inbox(&lease)
            .await
            .expect("take_inbox")
            .is_empty()
    );
    let record = store
        .get_run(run_id)
        .await
        .expect("get_run")
        .expect("exists");
    assert_eq!(record.status, RunStatus::Active);
    assert_eq!(record.history_len, 3);
    assert_eq!(record.updated_at, Timestamp(T0.0 + 2));
    assert_eq!(
        store.next_timer().await.expect("next_timer"),
        Some(Timestamp(T0.0 + 10))
    );
    store.release(&lease).await.expect("release");
    let tasks = store
        .claim_tool_tasks("worker", T0, LEASE, 10)
        .await
        .expect("claim_tool_tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        (tasks[0].run_id, tasks[0].seq, tasks[0].attempt),
        (run_id, Seq(1), 1)
    );
    assert_eq!(tasks[0].args, Payload::from(json!({ "x": 1 })));
}

async fn a_commit_with_a_stale_fence_writes_nothing<S: JournalStore>(store: &S) {
    let run_id = create(store, 1).await;
    let stale = acquire(store, T0).await.expect("runnable");
    let inbox = store.take_inbox(&stale).await.expect("take_inbox");
    let later = T0.saturating_add(LEASE + Duration::from_millis(1));
    let live = acquire(store, later).await.expect("taken over");

    let result = store
        .commit_activation(
            &stale,
            activation(
                vec![run_started_entry()],
                vec![inbox[0].id],
                RunStatus::Active,
            ),
        )
        .await;
    assert!(
        matches!(result, Err(StoreError::Fenced { .. })),
        "{result:?}"
    );
    assert!(
        store
            .load_history(run_id, 0)
            .await
            .expect("load_history")
            .is_empty()
    );
    assert_eq!(store.take_inbox(&live).await.expect("take_inbox").len(), 1);
    assert!(matches!(
        store.quarantine(&stale, "x".into()).await,
        Err(StoreError::Fenced { .. })
    ));
}

async fn a_commit_must_continue_the_history<S: JournalStore>(store: &S) {
    let run_id = start(store, 1).await;
    let lease = store
        .acquire_run(run_id, "worker", T0, LEASE)
        .await
        .expect("acquire_run")
        .expect("leasable");
    let log = Event::Annotation(ptc_core::Annotation::Log {
        level: ptc_core::LogLevel::Info,
        message: "x".into(),
        callsite: None,
    });
    for index in [0, 2] {
        let result = store
            .commit_activation(
                &lease,
                activation(vec![entry(index, log.clone())], vec![], RunStatus::Active),
            )
            .await;
        assert!(
            matches!(result, Err(StoreError::Conflict(_))),
            "index {index}: {result:?}"
        );
    }
    assert_eq!(
        store
            .load_history(run_id, 0)
            .await
            .expect("load_history")
            .len(),
        1
    );
}

async fn a_terminal_commit_clears_the_run<S: JournalStore>(store: &S) {
    let run_id = start(store, 1).await;
    let lease = store
        .acquire_run(run_id, "worker", T0, LEASE)
        .await
        .expect("acquire_run")
        .expect("leasable");
    let mut commit = activation(
        vec![entry(
            1,
            Event::Command(Command::ScheduleToolCall {
                seq: Seq(1),
                tool: "a".into(),
                args: json!({}).into(),
                callsite: None,
            }),
        )],
        vec![],
        RunStatus::Active,
    );
    commit.tool_calls.push(NewToolTask {
        seq: Seq(1),
        tool: "a".into(),
        args: json!({}).into(),
    });
    commit.timers.push(NewTimer {
        seq: Seq(2),
        fire_at: Timestamp(T0.0 + 10),
    });
    store
        .commit_activation(&lease, commit)
        .await
        .expect("commit");
    store
        .enqueue(run_id, Completion::TimerFired { seq: Seq(9) }, None, T0)
        .await
        .expect("enqueue");

    let done = entry(
        2,
        Event::Command(Command::CompleteRun {
            output: json!(null).into(),
        }),
    );
    store
        .commit_activation(&lease, activation(vec![done], vec![], RunStatus::Completed))
        .await
        .expect("terminal commit");
    store.release(&lease).await.expect("release");

    assert_eq!(store.next_timer().await.expect("next_timer"), None);
    assert!(
        store
            .claim_tool_tasks("worker", T0, LEASE, 10)
            .await
            .expect("claim")
            .is_empty()
    );
    assert_eq!(
        acquire(store, T0).await,
        None,
        "a terminal run is never runnable"
    );
    assert_eq!(
        store
            .acquire_run(run_id, "worker", T0, LEASE)
            .await
            .expect("acquire_run"),
        None
    );
    assert_eq!(
        store
            .get_run(run_id)
            .await
            .expect("get_run")
            .expect("exists")
            .status,
        RunStatus::Completed
    );
}

async fn enqueue_deduplicates_keys_and_refuses_ended_runs<S: JournalStore>(store: &S) {
    let run_id = start(store, 1).await;
    let signal = || Completion::SignalReceived {
        name: "approval".into(),
        payload: json!(true).into(),
    };
    let enqueue = |key: Option<&str>| store.enqueue(run_id, signal(), key.map(Into::into), T0);
    assert_eq!(
        enqueue(Some("k1")).await.expect("enqueue"),
        Enqueued::Accepted
    );
    assert_eq!(
        enqueue(Some("k1")).await.expect("enqueue"),
        Enqueued::Duplicate
    );
    assert_eq!(
        enqueue(Some("k2")).await.expect("enqueue"),
        Enqueued::Accepted
    );
    assert_eq!(enqueue(None).await.expect("enqueue"), Enqueued::Accepted);
    assert_eq!(enqueue(None).await.expect("enqueue"), Enqueued::Accepted);

    let lease = acquire(store, T0).await.expect("runnable");
    let inbox = store.take_inbox(&lease).await.expect("take_inbox");
    assert_eq!(inbox.len(), 4);
    assert!(
        inbox.windows(2).all(|pair| pair[0].id < pair[1].id),
        "arrival order"
    );

    // A key outlives the delivery of its signal.
    let consumed = inbox.iter().map(|item| item.id).collect();
    store
        .commit_activation(&lease, activation(vec![], consumed, RunStatus::Active))
        .await
        .expect("commit");
    store.release(&lease).await.expect("release");
    assert_eq!(
        enqueue(Some("k1")).await.expect("enqueue"),
        Enqueued::Duplicate
    );

    let lease = acquire(store, T0).await;
    assert_eq!(lease, None, "nothing left to deliver");
    let lease = store
        .acquire_run(run_id, "worker", T0, LEASE)
        .await
        .expect("acquire_run")
        .expect("leasable");
    let aborted = entry(
        1,
        Event::Lifecycle(Lifecycle::RunAborted(ptc_core::AbortReason {
            kind: ptc_core::AbortKind::Internal,
            message: "x".into(),
        })),
    );
    store
        .commit_activation(
            &lease,
            activation(vec![aborted], vec![], RunStatus::Aborted),
        )
        .await
        .expect("commit");
    assert_eq!(
        enqueue(Some("k3")).await.expect("enqueue"),
        Enqueued::RunEnded
    );
    assert!(matches!(
        store.enqueue(id(99), signal(), None, T0).await,
        Err(StoreError::RunNotFound(_))
    ));
}

async fn quarantine_releases_the_lease_and_hides_the_run<S: JournalStore>(store: &S) {
    let run_id = start(store, 1).await;
    store
        .enqueue(run_id, Completion::TimerFired { seq: Seq(1) }, None, T0)
        .await
        .expect("enqueue");
    let lease = acquire(store, T0).await.expect("runnable");
    store
        .quarantine(&lease, "history entry 1 is …".into())
        .await
        .expect("quarantine");
    assert_eq!(
        store.load_history(run_id, 0).await.expect("history").len(),
        1
    );
    assert_eq!(
        acquire(store, T0).await,
        None,
        "a quarantined run is never offered"
    );
    let record = store
        .get_run(run_id)
        .await
        .expect("get_run")
        .expect("exists");
    assert_eq!(record.quarantine.as_deref(), Some("history entry 1 is …"));

    // The lease was released: the run can be leased explicitly, to give it up.
    let explicit = store
        .acquire_run(run_id, "worker", T0, LEASE)
        .await
        .expect("acquire_run")
        .expect("a quarantined run can be leased explicitly");
    store.release(&explicit).await.expect("release");

    assert!(
        store
            .release_quarantine(run_id)
            .await
            .expect("release_quarantine")
    );
    assert!(
        !store
            .release_quarantine(run_id)
            .await
            .expect("release_quarantine")
    );
    let lease = acquire(store, T0).await.expect("runnable again");
    assert_eq!(lease.run_id, run_id);
}

async fn timers_fire_once<S: JournalStore>(store: &S) {
    let run_id = start(store, 1).await;
    let lease = store
        .acquire_run(run_id, "worker", T0, LEASE)
        .await
        .expect("acquire_run")
        .expect("leasable");
    let mut commit = activation(vec![], vec![], RunStatus::Active);
    for (seq, at) in [(1, 30), (2, 10), (3, 20)] {
        commit.timers.push(NewTimer {
            seq: Seq(seq),
            fire_at: Timestamp(T0.0 + at),
        });
    }
    store
        .commit_activation(&lease, commit)
        .await
        .expect("commit");
    let mut cancel = activation(vec![], vec![], RunStatus::Active);
    cancel.cancelled_timers.push(Seq(3));
    store
        .commit_activation(&lease, cancel)
        .await
        .expect("commit");
    store.release(&lease).await.expect("release");

    assert_eq!(
        store.next_timer().await.expect("next_timer"),
        Some(Timestamp(T0.0 + 10))
    );
    assert_eq!(
        store
            .fire_due_timers(Timestamp(T0.0 + 9))
            .await
            .expect("fire"),
        0
    );
    assert_eq!(
        store
            .fire_due_timers(Timestamp(T0.0 + 30))
            .await
            .expect("fire"),
        2
    );
    assert_eq!(
        store
            .fire_due_timers(Timestamp(T0.0 + 30))
            .await
            .expect("fire"),
        0
    );
    assert_eq!(store.next_timer().await.expect("next_timer"), None);

    let lease = acquire(store, T0).await.expect("runnable");
    let fired: Vec<Trigger> = store
        .take_inbox(&lease)
        .await
        .expect("take_inbox")
        .into_iter()
        .map(|item| item.trigger)
        .collect();
    assert_eq!(
        fired,
        [
            Trigger::Completion(Completion::TimerFired { seq: Seq(2) }),
            Trigger::Completion(Completion::TimerFired { seq: Seq(1) }),
        ],
        "earliest first"
    );
}

async fn runs_expire_once_and_quarantined_runs_are_reported<S: JournalStore>(store: &S) {
    let deadline = Timestamp(T0.0 + 1_000);
    let later_deadline = Timestamp(T0.0 + 1_500);
    let mut short = new_run(1);
    short.started.expires_at = deadline;
    let expiring = store
        .create_run(short, None)
        .await
        .expect("create_run")
        .run_id;
    let mut quarantined_run = new_run(2);
    quarantined_run.started.expires_at = later_deadline;
    let quarantined = store
        .create_run(quarantined_run, None)
        .await
        .expect("create_run")
        .run_id;
    create(store, 3).await;

    let lease = store
        .acquire_run(quarantined, "worker", T0, LEASE)
        .await
        .expect("acquire_run")
        .expect("leasable");
    store
        .quarantine(&lease, "mismatch".into())
        .await
        .expect("quarantine");

    assert_eq!(
        store.next_deadline().await.expect("next_deadline"),
        Some(deadline)
    );
    let none = store
        .expire_due_runs(Timestamp(deadline.0 - 1))
        .await
        .expect("expire");
    assert_eq!(none, ExpiredRuns::default());

    let expired = store.expire_due_runs(deadline).await.expect("expire");
    assert_eq!(expired.cancelled, [expiring]);
    assert!(expired.quarantined.is_empty());
    assert_eq!(
        store.next_deadline().await.expect("next_deadline"),
        Some(later_deadline),
        "a run whose expiry is enqueued has no deadline left"
    );
    let again = store.expire_due_runs(deadline).await.expect("expire");
    assert_eq!(again, ExpiredRuns::default(), "a run expires once");

    let quarantined_expired = store.expire_due_runs(later_deadline).await.expect("expire");
    assert!(
        quarantined_expired.cancelled.is_empty(),
        "nothing is enqueued for a quarantined run"
    );
    assert_eq!(quarantined_expired.quarantined, [quarantined]);
    let reported_again = store.expire_due_runs(later_deadline).await.expect("expire");
    assert_eq!(
        reported_again.quarantined,
        [quarantined],
        "until it is given up"
    );

    let lease = store
        .acquire_run(expiring, "worker", deadline, LEASE)
        .await
        .expect("acquire_run")
        .expect("leasable");
    let triggers: Vec<Trigger> = store
        .take_inbox(&lease)
        .await
        .expect("take_inbox")
        .into_iter()
        .map(|item| item.trigger)
        .collect();
    assert_eq!(
        triggers.last(),
        Some(&Trigger::Completion(Completion::CancelRequested {
            reason: CancelReason::Ttl
        }))
    );
}

async fn tool_tasks_are_claimed_reclaimed_and_completed_once<S: JournalStore>(store: &S) {
    let run_id = start(store, 1).await;
    let lease = store
        .acquire_run(run_id, "worker", T0, LEASE)
        .await
        .expect("acquire_run")
        .expect("leasable");
    let mut commit = activation(vec![], vec![], RunStatus::Active);
    for seq in [1, 2, 3] {
        commit.tool_calls.push(NewToolTask {
            seq: Seq(seq),
            tool: format!("tool_{seq}"),
            args: json!({ "seq": seq }).into(),
        });
    }
    store
        .commit_activation(&lease, commit)
        .await
        .expect("commit");
    store.release(&lease).await.expect("release");

    let first = store
        .claim_tool_tasks("a", T0, LEASE, 2)
        .await
        .expect("claim");
    assert_eq!(
        first.iter().map(|task| task.seq).collect::<Vec<_>>(),
        [Seq(1), Seq(2)]
    );
    let second = store
        .claim_tool_tasks("b", T0, LEASE, 10)
        .await
        .expect("claim");
    assert_eq!(
        second.iter().map(|task| task.seq).collect::<Vec<_>>(),
        [Seq(3)]
    );
    assert!(
        store
            .claim_tool_tasks("c", T0, LEASE, 10)
            .await
            .expect("claim")
            .is_empty()
    );

    // Task 1 completes; the claims of 2 and 3 expire and they are claimed again.
    let result = Completion::ToolCallCompleted {
        seq: Seq(1),
        result: json!("ok").into(),
    };
    store
        .complete_tool_task(&first[0], result.clone(), T0)
        .await
        .expect("complete");
    store
        .complete_tool_task(&first[0], result.clone(), T0)
        .await
        .expect("completing twice drops the second outcome");
    let later = T0.saturating_add(LEASE + Duration::from_millis(1));
    let reclaimed = store
        .claim_tool_tasks("c", later, LEASE, 10)
        .await
        .expect("claim");
    assert_eq!(
        reclaimed
            .iter()
            .map(|task| (task.seq, task.attempt))
            .collect::<Vec<_>>(),
        [(Seq(2), 2), (Seq(3), 2)]
    );

    let lease = acquire(store, later).await.expect("runnable");
    let inbox = store.take_inbox(&lease).await.expect("take_inbox");
    assert_eq!(inbox.len(), 1, "one outcome per task");
    assert_eq!(inbox[0].trigger, Trigger::Completion(result));
}

async fn list_runs_filters_by_status_and_labels<S: JournalStore>(store: &S) {
    let mut labelled = new_run(1);
    labelled.metadata.labels = BTreeMap::from([
        ("agent".to_owned(), "triage".to_owned()),
        ("conversation".to_owned(), "c1".to_owned()),
    ]);
    store.create_run(labelled, None).await.expect("create_run");
    start(store, 2).await;

    let all = store
        .list_runs(&RunFilter::default())
        .await
        .expect("list_runs");
    assert_eq!(all.len(), 2);
    let by_label = store
        .list_runs(&RunFilter {
            labels: BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
            ..RunFilter::default()
        })
        .await
        .expect("list_runs");
    assert_eq!(
        by_label.iter().map(|run| run.id).collect::<Vec<_>>(),
        [id(1)]
    );
    let active = store
        .list_runs(&RunFilter {
            status: Some(RunStatus::Active),
            ..RunFilter::default()
        })
        .await
        .expect("list_runs");
    assert_eq!(active.iter().map(|run| run.id).collect::<Vec<_>>(), [id(2)]);
}
