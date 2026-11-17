//! The conformance suite of `JournalStore`: the commit invariant and fencing, lease renewal,
//! single writers, tenants (isolation, quotas, fair draws), sticky affinity, idempotency keys,
//! epochs, failure counts and quarantine, timers, run expiry, the tool task lifecycle and its
//! concurrency limits, webhook tokens and subscriptions, payload rows and references, retention
//! and the feed; workflow versions, and the watchers that receive a run's end.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rewind_core::{
    AbortKind, AbortReason, Annotation, CancelReason, Command, Completion, Digest, Epoch, Event,
    HistoryEntry, Lifecycle, LogLevel, Payload, RunId, RunMode, RunStarted, RunStatus, Script,
    Seed, Seq, Suite, SuiteTest, Timestamp, ToolFailure, ToolFailureKind, ToolOutcome, ToolPolicy,
    ToolRoute, Toolset, Workflow, WorkflowSchemas, end_signal_name,
};
use serde_json::json;

use super::{T0, check, tenant};
use crate::ports::{
    AcquireRunnable, Activation, CompiledScript, CreateRun, Enqueued, ExpiredRuns, FeedConsumer,
    ForkParent, JournalStore, Lease, NewRun, NewTimer, NewToolTask, PAYLOAD_DELETION_TIMEOUT,
    ParkKind, ParkReason, ParkedScope, PayloadRecord, QUARANTINE_AFTER_FAILURES, QuarantineKind,
    Retention, RunCursor, RunFilter, RunMetadata, RunRef, RunWorkflow, STICKY_GRACE, Secret,
    StartedBy, StoreError, Subscription, TEST_GRACE, TaskFilter, TaskRef, TaskState, TaskToken,
    TaskUpdate, Tenant, TenantId, TenantScope, TestRecord, ToolDeclarations, TraceContext, Trigger,
    Watched, WebhookEventType, WebhookToken, WorkflowFilter, deferred_timeout_failure,
};
use crate::ports::{
    Day, DeleteNamespace, DropNamespace, Month, NamespaceCursor, NamespaceFilter, NamespaceName,
    NamespaceOrder, NamespaceWrite, QuotaAlertKey, QuotaExceeded, QuotaKind, QuotaScope, Quotas,
    Threshold, UsageDelta, UsageOf,
};

const LEASE: Duration = Duration::from_secs(30);
const EPOCHS: &[Epoch] = &[Epoch(1)];
const DAY: u64 = 86_400_000;
static ALL_TENANTS: TenantScope = TenantScope::All;

/// Runs every check, each on a new store.
pub async fn run_all<S: JournalStore>(new_store: impl Fn() -> S) {
    check!(
        "journal",
        new_store(),
        scripts_and_toolsets_are_stored_by_tenant_and_digest
    );
    check!("journal", new_store(), create_run_enqueues_run_started);
    check!("journal", new_store(), run_ids_are_unique_across_tenants);
    check!(
        "journal",
        new_store(),
        idempotency_keys_are_scoped_by_tenant
    );
    check!("journal", new_store(), quotas_limit_active_runs);
    check!("journal", new_store(), a_run_has_one_writer_at_a_time);
    check!(
        "journal",
        new_store(),
        leases_are_renewed_while_the_fence_matches
    );
    check!(
        "journal",
        new_store(),
        acquire_runnable_respects_epochs_quarantine_and_tenants
    );
    check!(
        "journal",
        new_store(),
        tenants_are_drawn_then_their_oldest_run_goes_first
    );
    check!("journal", new_store(), sticky_workers_get_their_runs_first);
    check!(
        "journal",
        new_store(),
        a_sticky_worker_that_died_holds_its_run_no_longer_than_its_lease
    );
    check!(
        "journal",
        new_store(),
        a_commit_writes_entries_inbox_effects_and_metadata
    );
    check!(
        "journal",
        new_store(),
        a_commit_with_a_stale_fence_writes_nothing
    );
    check!("journal", new_store(), a_commit_must_continue_the_history);
    check!("journal", new_store(), a_terminal_commit_clears_the_run);
    check!(
        "journal",
        new_store(),
        enqueue_deduplicates_keys_and_refuses_ended_runs
    );
    check!("journal", new_store(), runs_of_another_tenant_are_invisible);
    check!(
        "journal",
        new_store(),
        failures_are_counted_and_the_fifth_quarantines
    );
    check!(
        "journal",
        new_store(),
        activations_started_since_the_last_commit_are_counted
    );
    check!(
        "journal",
        new_store(),
        quarantine_releases_the_lease_and_hides_the_run
    );
    check!(
        "journal",
        new_store(),
        a_new_build_retries_each_quarantined_run_once
    );
    check!(
        "journal",
        new_store(),
        quarantined_runs_are_retried_by_workers_of_their_tenant
    );
    check!("journal", new_store(), timers_fire_once_earliest_first);
    check!(
        "journal",
        new_store(),
        runs_expire_once_and_quarantined_runs_are_reported
    );
    check!(
        "journal",
        new_store(),
        tool_tasks_are_claimed_and_completed_once
    );
    check!(
        "journal",
        new_store(),
        an_expired_claim_is_claimed_again_and_its_token_dies
    );
    check!(
        "journal",
        new_store(),
        expired_host_claims_that_cannot_be_handed_out_again_are_interrupted
    );
    check!(
        "journal",
        new_store(),
        transient_failures_wait_for_their_next_attempt
    );
    check!(
        "journal",
        new_store(),
        the_next_tool_attempt_is_the_earliest_retry_still_to_come
    );
    check!(
        "journal",
        new_store(),
        deferred_tasks_keep_their_token_until_answered_or_late
    );
    check!(
        "journal",
        new_store(),
        approval_and_parked_tasks_wait_for_the_harness
    );
    check!("journal", new_store(), a_token_names_its_task);
    check!(
        "journal",
        new_store(),
        renewed_credentials_resume_parked_tasks
    );
    check!("journal", new_store(), claims_respect_concurrency_limits);
    check!(
        "journal",
        new_store(),
        claims_filter_by_route_tenant_toolset_and_tool
    );
    check!(
        "journal",
        new_store(),
        webhook_tokens_resolve_to_their_run_until_it_ends
    );
    check!("journal", new_store(), subscriptions_are_stored_per_tenant);
    check!(
        "journal",
        new_store(),
        payloads_are_recorded_and_referenced_at_commit
    );
    check!(
        "journal",
        new_store(),
        unreferenced_payloads_are_forgotten_after_their_grace
    );
    check!(
        "journal",
        new_store(),
        a_payload_being_deleted_is_never_committed_without_its_value
    );
    check!("journal", new_store(), json_values_keep_nul_characters);
    check!(
        "journal",
        new_store(),
        finished_runs_are_archived_then_deleted
    );
    check!(
        "journal",
        new_store(),
        runs_with_feed_items_left_are_not_archived
    );
    check!("journal", new_store(), deleting_a_run_erases_it);
    check!("journal", new_store(), tenants_are_stored);
    check!(
        "journal",
        new_store(),
        the_feed_is_claimed_and_acknowledged_per_consumer
    );
    check!("journal", new_store(), list_runs_filters_and_pages);
    check!(
        "journal",
        new_store(),
        namespaces_are_created_at_first_use_and_keep_their_exceptions
    );
    check!(
        "journal",
        new_store(),
        workflow_versions_are_stored_by_tenant_and_listed_newest_first
    );
    check!(
        "journal",
        new_store(),
        tests_are_kept_while_a_version_references_them
    );
    check!(
        "journal",
        new_store(),
        tests_of_an_erased_namespace_go_whatever_references_them
    );
    check!(
        "journal",
        new_store(),
        idempotency_keys_are_scoped_by_namespace
    );
    check!(
        "journal",
        new_store(),
        a_run_ends_to_its_watchers_and_cancels_its_attached_runs
    );
    check!(
        "journal",
        new_store(),
        active_runs_are_limited_per_namespace_then_per_tenant
    );
    check!(
        "journal",
        new_store(),
        runs_per_month_are_counted_and_free_the_next_month
    );
    check!(
        "journal",
        new_store(),
        scripts_count_distinct_scripts_with_runs_kept
    );
    check!(
        "journal",
        new_store(),
        cpu_time_is_counted_per_day_and_alerts_once
    );
    check!(
        "journal",
        new_store(),
        quota_alerts_are_claimed_and_acknowledged
    );
    check!("journal", new_store(), namespaces_are_listed_by_their_use);
    check!(
        "journal",
        new_store(),
        an_erased_namespace_takes_no_run_and_is_dropped_once_empty
    );
    check!(
        "journal",
        new_store(),
        runs_tasks_and_parked_calls_are_filtered_by_namespace
    );
    check!(
        "journal",
        new_store(),
        watching_a_run_says_whether_it_has_ended
    );
}

// -------------------------------------------------------------------------------------------
// Helpers

fn acme() -> TenantId {
    tenant("acme")
}

fn id(n: u128) -> RunId {
    RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0000 | n)
}

fn run_ref(n: u128) -> RunRef {
    RunRef::new(acme(), id(n))
}

fn run_in(tenant: &TenantId, n: u128) -> RunRef {
    RunRef::new(tenant.clone(), id(n))
}

fn toolset_digest() -> Digest {
    Digest::of(b"toolset")
}

fn started(epoch: u32, expires_at: Timestamp) -> RunStarted {
    RunStarted {
        script: Digest::of(b"script"),
        toolset: toolset_digest(),
        input: json!({ "n": 1 }).into(),
        seed: Seed::from_bytes([9; 32]),
        epoch: Epoch(epoch),
        expires_at,
        mode: RunMode::Mock,
    }
}

fn new_run(run: RunRef) -> NewRun {
    NewRun {
        run,
        namespace: alice(),
        started: started(1, Timestamp(T0.0 + DAY)),
        metadata: RunMetadata::default(),
        created_at: T0,
    }
}

/// A run of `namespace` running `script`, created at `at`.
fn new_run_of(run: RunRef, namespace: &NamespaceName, script: &[u8], at: Timestamp) -> NewRun {
    let mut started = started(1, Timestamp(at.0 + DAY));
    started.script = Digest::of(script);
    NewRun {
        run,
        namespace: namespace.clone(),
        started,
        metadata: RunMetadata::default(),
        created_at: at,
    }
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
        Event::Lifecycle(Lifecycle::RunStarted(started(1, Timestamp(T0.0 + DAY)))),
    )
}

fn log_entry(index: u64) -> HistoryEntry {
    entry(
        index,
        Event::Annotation(Annotation::Log {
            level: LogLevel::Info,
            message: "x".into(),
            callsite: None,
        }),
    )
}

fn aborted_entry(index: u64) -> HistoryEntry {
    entry(
        index,
        Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
            kind: AbortKind::Internal,
            message: "x".into(),
        })),
    )
}

fn later(at: Timestamp, ms: u64) -> Timestamp {
    Timestamp(at.0 + ms)
}

fn host_task(seq: u32, tool: &str) -> NewToolTask {
    let route = ToolRoute::Host { group: None };
    NewToolTask {
        seq: Seq(seq),
        tool: tool.into(),
        args: json!({ "seq": seq }).into(),
        policy: ToolPolicy::default_for(&route),
        route,
        server_concurrency: None,
    }
}

fn mcp_task(seq: u32, tool: &str, server: &str, server_concurrency: u32) -> NewToolTask {
    let route = ToolRoute::Mcp {
        server: server.into(),
    };
    NewToolTask {
        seq: Seq(seq),
        tool: tool.into(),
        args: json!({ "seq": seq }).into(),
        policy: ToolPolicy::default_for(&route),
        route,
        server_concurrency: Some(server_concurrency),
    }
}

fn result(seq: u32) -> ToolOutcome {
    ToolOutcome::Result(json!({ "done": seq }))
}

fn request<'a>(owner: &'a str, at: Timestamp, draw: u64) -> AcquireRunnable<'a> {
    AcquireRunnable {
        owner,
        epochs: EPOCHS,
        tenants: &ALL_TENANTS,
        now: at,
        lease_duration: LEASE,
        sticky_grace: STICKY_GRACE,
        draw,
    }
}

async fn create<S: JournalStore>(store: &S, run: RunRef) -> RunRef {
    match store
        .create_run(new_run(run.clone()), None)
        .await
        .expect("create_run")
    {
        CreateRun::Created(created) => {
            assert!(created.created);
            assert_eq!(created.run, run);
            created.run
        }
        other => panic!("the run was not created: {other:?}"),
    }
}

async fn acquire<S: JournalStore>(store: &S, at: Timestamp) -> Option<Lease> {
    store
        .acquire_runnable(&request("worker", at, 0))
        .await
        .expect("acquire_runnable")
}

async fn lease_of<S: JournalStore>(store: &S, run: &RunRef, at: Timestamp) -> Lease {
    store
        .acquire_run(run, "worker", at, LEASE)
        .await
        .expect("acquire_run")
        .expect("the run can be leased")
}

async fn commit<S: JournalStore>(store: &S, lease: &Lease, activation: Activation, at: Timestamp) {
    store
        .commit_activation(lease, activation, at)
        .await
        .expect("commit_activation");
}

/// Creates the run and commits its `run_started`: the run is active, its lease released, and
/// `worker` is its sticky worker.
async fn start<S: JournalStore>(store: &S, run: RunRef) -> RunRef {
    create(store, run.clone()).await;
    let lease = lease_of(store, &run, T0).await;
    let consumed = inbox_ids(store, &lease).await;
    commit(
        store,
        &lease,
        Activation::new(vec![run_started_entry()], consumed, RunStatus::Active),
        T0,
    )
    .await;
    store.release(&lease).await.expect("release");
    run
}

/// Starts the run and commits these tool tasks.
async fn start_with_tasks<S: JournalStore>(
    store: &S,
    run: RunRef,
    tasks: Vec<NewToolTask>,
) -> RunRef {
    start(store, run.clone()).await;
    let lease = lease_of(store, &run, T0).await;
    let mut activation = Activation::new(vec![], vec![], RunStatus::Active);
    activation.tool_calls = tasks;
    commit(store, &lease, activation, T0).await;
    store.release(&lease).await.expect("release");
    run
}

async fn inbox_ids<S: JournalStore>(store: &S, lease: &Lease) -> Vec<u64> {
    store
        .take_inbox(lease)
        .await
        .expect("take_inbox")
        .iter()
        .map(|item| item.id)
        .collect()
}

/// The triggers in the run's inbox, read under a short lease.
async fn inbox<S: JournalStore>(store: &S, run: &RunRef, at: Timestamp) -> Vec<Trigger> {
    let lease = lease_of(store, run, at).await;
    let items = store.take_inbox(&lease).await.expect("take_inbox");
    store.release(&lease).await.expect("release");
    items.into_iter().map(|item| item.trigger).collect()
}

async fn record<S: JournalStore>(store: &S, run: &RunRef) -> crate::ports::RunRecord {
    store
        .get_run(run)
        .await
        .expect("get_run")
        .expect("the run exists")
}

async fn claim<S: JournalStore>(
    store: &S,
    owner: &str,
    filter: &TaskFilter,
    at: Timestamp,
    limit: usize,
) -> Vec<crate::ports::ClaimedTask> {
    store
        .claim_tool_tasks(owner, filter, at, limit)
        .await
        .expect("claim_tool_tasks")
}

fn seqs(claimed: &[crate::ports::ClaimedTask]) -> Vec<u32> {
    claimed.iter().map(|task| task.task.task.seq.0).collect()
}

// -------------------------------------------------------------------------------------------
// Scripts, toolsets and runs

pub async fn scripts_and_toolsets_are_stored_by_tenant_and_digest<S: JournalStore>(store: &S) {
    let other = tenant("other");
    let script = Script::new("export default () => 1");
    let compiled = CompiledScript {
        javascript: "export default () => 1".into(),
        source_map: "{}".into(),
    };
    assert_eq!(
        store
            .get_script(&acme(), script.digest())
            .await
            .expect("get_script"),
        None
    );
    store
        .put_script(&acme(), &script, &compiled)
        .await
        .expect("put_script");
    let recompiled = CompiledScript {
        javascript: "changed".into(),
        source_map: String::new(),
    };
    store
        .put_script(&acme(), &script, &recompiled)
        .await
        .expect("put_script is idempotent");
    let stored = store
        .get_script(&acme(), script.digest())
        .await
        .expect("get_script")
        .expect("stored");
    assert_eq!(
        (stored.script, stored.compiled),
        (script.clone(), compiled),
        "the first put wins"
    );
    assert_eq!(
        store
            .get_script(&other, script.digest())
            .await
            .expect("get_script"),
        None,
        "another tenant does not see it"
    );

    let toolset = Toolset::from_host_tools(["a", "b"]).expect("toolset");
    let declarations = ToolDeclarations {
        typescript: "declare module \"rewind:tools\" {}".into(),
        prompt: "a() · b()".into(),
    };
    store
        .put_toolset(&acme(), &toolset, &declarations)
        .await
        .expect("put_toolset");
    store
        .put_toolset(&acme(), &toolset, &ToolDeclarations::default())
        .await
        .expect("put_toolset is idempotent");
    let stored = store
        .get_toolset(&acme(), toolset.digest())
        .await
        .expect("get_toolset")
        .expect("stored");
    assert_eq!(stored.toolset, toolset);
    assert_eq!(stored.declarations, declarations, "the first put wins");
    assert_eq!(
        store
            .get_toolset(&other, toolset.digest())
            .await
            .expect("get_toolset"),
        None
    );

    let second = Toolset::from_host_tools(["c"]).expect("toolset");
    store
        .put_toolset(&acme(), &second, &ToolDeclarations::default())
        .await
        .expect("put_toolset");
    store
        .put_toolset(&other, &second, &ToolDeclarations::default())
        .await
        .expect("put_toolset");
    let listed = store.list_toolsets(&acme()).await.expect("list_toolsets");
    let mut expected = vec![toolset.digest(), second.digest()];
    expected.sort();
    assert_eq!(
        listed
            .iter()
            .map(|stored| stored.toolset.digest())
            .collect::<Vec<_>>(),
        expected,
        "the tenant's toolsets, by digest"
    );
    assert_eq!(
        listed
            .iter()
            .find(|stored| stored.toolset == toolset)
            .map(|stored| &stored.declarations),
        Some(&declarations)
    );
    assert_eq!(
        store
            .list_toolsets(&tenant("nobody"))
            .await
            .expect("list_toolsets"),
        []
    );
}

pub async fn create_run_enqueues_run_started<S: JournalStore>(store: &S) {
    let mut run = new_run(run_ref(1));
    run.metadata = RunMetadata {
        labels: BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
        trace_context: Some(TraceContext {
            traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
            tracestate: Some("vendor=1".into()),
        }),
        parent: Some(ForkParent {
            run: id(99),
            at: Some(7),
        }),
        workflow: Some(RunWorkflow {
            name: "triage".into(),
            version: Digest::of(b"version"),
        }),
        started_by: Some(StartedBy {
            run: id(98),
            seq: Seq(3),
            detached: true,
        }),
        scope: BTreeMap::from([("conversation".to_owned(), "c-1".to_owned())]),
    };
    let created = store
        .create_run(run.clone(), Some("call-1".into()))
        .await
        .expect("create_run");
    assert!(matches!(created, CreateRun::Created(ref c) if c.created && c.run == run_ref(1)));

    let record = record(store, &run_ref(1)).await;
    assert_eq!(record.run, run_ref(1));
    assert_eq!(record.status, RunStatus::Pending);
    assert_eq!(
        (record.script, record.toolset),
        (Digest::of(b"script"), toolset_digest())
    );
    assert_eq!(record.epoch, Epoch(1));
    assert_eq!(record.expires_at, Timestamp(T0.0 + DAY));
    assert_eq!(record.history_len, 0);
    assert_eq!(record.metadata, run.metadata);
    assert_eq!(record.idempotency_key.as_deref(), Some("call-1"));
    assert_eq!((record.created_at, record.updated_at), (T0, T0));
    assert_eq!(record.quarantine, None);
    assert_eq!((record.failures, record.attempts), (0, 0));
    assert_eq!(record.sticky_worker, None);
    assert_eq!(record.ended_at, None);
    assert_eq!(record.archive, None);

    let lease = acquire(store, T0).await.expect("a pending run is runnable");
    assert_eq!(lease.run, run_ref(1));
    let items = store.take_inbox(&lease).await.expect("take_inbox");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].trigger, Trigger::RunStarted(run.started));
    assert_eq!(items[0].received_at, T0);
    assert!(
        store
            .load_history(&run_ref(1), 0)
            .await
            .expect("load_history")
            .is_empty()
    );
}

pub async fn run_ids_are_unique_across_tenants<S: JournalStore>(store: &S) {
    create(store, run_ref(1)).await;
    for taken in [run_ref(1), run_in(&tenant("other"), 1)] {
        assert!(
            matches!(
                store.create_run(new_run(taken.clone()), None).await,
                Err(StoreError::Conflict(_))
            ),
            "{taken} reuses a run id"
        );
    }
}

pub async fn idempotency_keys_are_scoped_by_tenant<S: JournalStore>(store: &S) {
    let first = store
        .create_run(new_run(run_ref(1)), Some("tool-call-1".into()))
        .await
        .expect("create_run");
    let retry = store
        .create_run(new_run(run_ref(2)), Some("tool-call-1".into()))
        .await
        .expect("create_run");
    let CreateRun::Created(retry) = retry else {
        panic!("a retry is never refused");
    };
    assert!(!retry.created);
    assert_eq!(retry.run, run_ref(1));
    assert!(matches!(first, CreateRun::Created(ref c) if c.created));
    assert_eq!(store.get_run(&run_ref(2)).await.expect("get_run"), None);

    let other = run_in(&tenant("other"), 3);
    let elsewhere = store
        .create_run(new_run(other.clone()), Some("tool-call-1".into()))
        .await
        .expect("create_run");
    assert!(
        matches!(elsewhere, CreateRun::Created(ref c) if c.created && c.run == other),
        "the same key in another tenant starts another run"
    );
}

pub async fn quotas_limit_active_runs<S: JournalStore>(store: &S) {
    let mut limited = Tenant::new(acme());
    limited.quotas.active_runs = Some(2);
    store.put_tenant(&limited).await.expect("put_tenant");
    store
        .create_run(new_run(run_ref(1)), Some("k1".into()))
        .await
        .expect("create_run");
    start(store, run_ref(2)).await;
    assert_eq!(
        store
            .create_run(new_run(run_ref(3)), None)
            .await
            .expect("create_run"),
        CreateRun::QuotaExceeded(QuotaExceeded {
            quota: QuotaKind::ActiveRuns,
            scope: QuotaScope::Tenant,
            limit: 2,
            used: 2,
            resets_at: None,
        })
    );
    assert_eq!(store.get_run(&run_ref(3)).await.expect("get_run"), None);
    let retry = store
        .create_run(new_run(run_ref(4)), Some("k1".into()))
        .await
        .expect("create_run");
    assert!(
        matches!(retry, CreateRun::Created(ref c) if !c.created && c.run == run_ref(1)),
        "a retried request returns its run, even at the quota"
    );

    // A run that ends frees its slot.
    let lease = lease_of(store, &run_ref(2), T0).await;
    commit(
        store,
        &lease,
        Activation::new(vec![aborted_entry(1)], vec![], RunStatus::Aborted),
        T0,
    )
    .await;
    create(store, run_ref(3)).await;

    // A tenant without a record has no quota.
    for n in 10..15 {
        create(store, run_in(&tenant("free"), n)).await;
    }
}

pub async fn a_run_has_one_writer_at_a_time<S: JournalStore>(store: &S) {
    let run = create(store, run_ref(1)).await;
    let first = acquire(store, T0).await.expect("runnable");
    assert_eq!(
        acquire(store, T0).await,
        None,
        "a leased run is not runnable"
    );
    assert_eq!(
        store
            .acquire_run(&run, "other", T0, LEASE)
            .await
            .expect("acquire_run"),
        None
    );

    // The lease expires: another worker takes over, and the first one is fenced off.
    let expiry = T0.saturating_add(LEASE);
    assert_eq!(
        acquire(store, Timestamp(expiry.0 - 1)).await,
        None,
        "a lease lasts its duration"
    );
    let second = acquire(store, expiry)
        .await
        .expect("an expired lease can be taken over");
    assert!(second.fence > first.fence);
    assert!(matches!(
        store.take_inbox(&first).await,
        Err(StoreError::Fenced { .. })
    ));

    // Releasing a stale lease does nothing; releasing the live one frees the run.
    store.release(&first).await.expect("release");
    assert_eq!(acquire(store, expiry).await, None);
    store.release(&second).await.expect("release");
    let third = acquire(store, expiry).await.expect("released");
    assert!(third.fence > second.fence);
    assert!(matches!(
        store.acquire_run(&run_ref(9), "worker", T0, LEASE).await,
        Err(StoreError::RunNotFound(_))
    ));
}

pub async fn leases_are_renewed_while_the_fence_matches<S: JournalStore>(store: &S) {
    create(store, run_ref(1)).await;
    let lease = acquire(store, T0).await.expect("runnable");
    let renewed_at = later(T0, 20_000);
    store.renew(&lease, renewed_at, LEASE).await.expect("renew");
    let old_expiry = T0.saturating_add(LEASE);
    assert_eq!(
        acquire(store, old_expiry).await,
        None,
        "the renewed lease outlives its first term"
    );
    let new_expiry = renewed_at.saturating_add(LEASE);
    let taken = acquire(store, new_expiry)
        .await
        .expect("a lease not renewed expires");
    assert!(
        matches!(
            store.renew(&lease, new_expiry, LEASE).await,
            Err(StoreError::Fenced { .. })
        ),
        "a lost lease cannot be renewed"
    );
    store.renew(&taken, new_expiry, LEASE).await.expect("renew");
}

pub async fn acquire_runnable_respects_epochs_quarantine_and_tenants<S: JournalStore>(store: &S) {
    let mut epoch_2 = new_run(run_ref(1));
    epoch_2.started.epoch = Epoch(2);
    store.create_run(epoch_2, None).await.expect("create_run");
    assert_eq!(
        acquire(store, T0).await,
        None,
        "epoch 2 is not offered for epoch 1"
    );
    let both = [Epoch(1), Epoch(2)];
    let mut both_epochs = request("worker", T0, 0);
    both_epochs.epochs = &both;
    let lease = store
        .acquire_runnable(&both_epochs)
        .await
        .expect("acquire_runnable")
        .expect("epoch 2 is offered to a worker that supports it");
    store.release(&lease).await.expect("release");

    let other = create(store, run_in(&tenant("other"), 2)).await;
    let only_acme = TenantScope::Only(BTreeSet::from([acme()]));
    let mut scoped = request("worker", T0, 0);
    scoped.tenants = &only_acme;
    assert_eq!(
        store
            .acquire_runnable(&scoped)
            .await
            .expect("acquire_runnable"),
        None,
        "acme's only run is of epoch 2"
    );
    let except_acme = TenantScope::Except(BTreeSet::from([acme()]));
    scoped.tenants = &except_acme;
    let lease = store
        .acquire_runnable(&scoped)
        .await
        .expect("acquire_runnable")
        .expect("the other tenant's run");
    assert_eq!(lease.run, other);
    store
        .quarantine(
            &lease,
            QuarantineKind::NonDeterminism,
            "mismatch".into(),
            T0,
        )
        .await
        .expect("quarantine");
    assert_eq!(
        store
            .acquire_runnable(&scoped)
            .await
            .expect("acquire_runnable"),
        None,
        "a quarantined run is never offered"
    );
}

pub async fn tenants_are_drawn_then_their_oldest_run_goes_first<S: JournalStore>(store: &S) {
    let a = tenant("a-tenant");
    let b = tenant("b-tenant");
    // `a` has three runs, `b` one; they were created in the order a1, b1, a2, a3.
    create(store, run_in(&a, 1)).await;
    create(store, run_in(&b, 2)).await;
    create(store, run_in(&a, 3)).await;
    create(store, run_in(&a, 4)).await;

    let take = |draw: u64| async move {
        store
            .acquire_runnable(&request("w", T0, draw))
            .await
            .expect("acquire_runnable")
            .expect("runnable")
            .run
    };
    // Tenants with work, by id: [a, b]. Draw 1 is b; then a's runs, oldest first.
    assert_eq!(take(1).await, run_in(&b, 2));
    assert_eq!(
        take(1).await,
        run_in(&a, 1),
        "only a has work left: every draw is a"
    );
    assert_eq!(take(0).await, run_in(&a, 3));
    assert_eq!(take(7).await, run_in(&a, 4));
}

pub async fn sticky_workers_get_their_runs_first<S: JournalStore>(store: &S) {
    // Run 1 is committed by w1, which becomes its sticky worker.
    create(store, run_ref(1)).await;
    let lease = store
        .acquire_run(&run_ref(1), "w1", T0, LEASE)
        .await
        .expect("acquire_run")
        .expect("leasable");
    let consumed = inbox_ids(store, &lease).await;
    commit(
        store,
        &lease,
        Activation::new(vec![run_started_entry()], consumed, RunStatus::Active),
        T0,
    )
    .await;
    store.release(&lease).await.expect("release");
    assert_eq!(
        record(store, &run_ref(1)).await.sticky_worker.as_deref(),
        Some("w1")
    );

    let woken = later(T0, 1_000);
    store
        .enqueue(
            &run_ref(1),
            Completion::TimerFired { seq: Seq(1) },
            None,
            woken,
        )
        .await
        .expect("enqueue");
    let as_worker = |owner: &'static str, at: Timestamp| async move {
        store
            .acquire_runnable(&request(owner, at, 0))
            .await
            .expect("acquire_runnable")
    };
    let grace_end = woken.saturating_add(STICKY_GRACE);
    assert_eq!(
        as_worker("w2", woken).await,
        None,
        "reserved to its sticky worker"
    );
    assert_eq!(as_worker("w2", Timestamp(grace_end.0 - 1)).await, None);

    // Its sticky worker gets it first, even before an older run without a sticky worker.
    create(store, run_ref(2)).await;
    let lease = as_worker("w1", woken).await.expect("w1's run");
    assert_eq!(lease.run, run_ref(1));
    store.release(&lease).await.expect("release");

    // Other workers take it once the grace has passed.
    let lease = as_worker("w2", woken)
        .await
        .expect("the run without a sticky worker");
    assert_eq!(lease.run, run_ref(2));
    let lease = as_worker("w2", grace_end)
        .await
        .expect("w1's run, after the grace");
    assert_eq!(lease.run, run_ref(1));
    store.release(&lease).await.expect("release");

    // Forgetting the priority frees the run at once; only its sticky worker can forget it.
    store
        .forget_sticky(&run_ref(1), "w2")
        .await
        .expect("forget_sticky");
    assert_eq!(
        record(store, &run_ref(1)).await.sticky_worker.as_deref(),
        Some("w1")
    );
    store
        .forget_sticky(&run_ref(1), "w1")
        .await
        .expect("forget_sticky");
    assert_eq!(record(store, &run_ref(1)).await.sticky_worker, None);
    let lease = as_worker("w3", woken)
        .await
        .expect("no sticky worker any more");
    assert_eq!(lease.run, run_ref(1));
}

pub async fn a_sticky_worker_that_died_holds_its_run_no_longer_than_its_lease<S: JournalStore>(
    store: &S,
) {
    // w1 commits the run, and is its sticky worker. A trigger arrives; w1 leases the run, and
    // dies before it commits anything.
    let run = start(store, run_ref(1)).await;
    let woken = later(T0, 1_000);
    store
        .enqueue(&run, Completion::TimerFired { seq: Seq(1) }, None, woken)
        .await
        .expect("enqueue");
    let as_worker = |owner: &'static str, at: Timestamp| async move {
        store
            .acquire_runnable(&request(owner, at, 0))
            .await
            .expect("acquire_runnable")
    };
    let dead = as_worker("worker", woken).await.expect("its sticky worker");
    assert_eq!(dead.run, run);

    // The lease holds the run until it expires, and not a moment longer: the sticky priority
    // of the dead worker adds no grace, since its trigger has waited longer than that already.
    let expiry = woken.saturating_add(LEASE);
    assert_eq!(as_worker("w2", Timestamp(expiry.0 - 1)).await, None);
    let taken = as_worker("w2", expiry)
        .await
        .expect("another worker takes it as soon as the lease expires");
    assert_eq!(taken.run, run);
    assert!(taken.fence > dead.fence);
    assert_eq!(
        record(store, &run).await.sticky_worker.as_deref(),
        Some("worker"),
        "leasing a run does not make the worker sticky: only a commit does"
    );
}

pub async fn a_commit_writes_entries_inbox_effects_and_metadata<S: JournalStore>(store: &S) {
    let run = create(store, run_ref(1)).await;
    let lease = acquire(store, T0).await.expect("runnable");
    let inbox = store.take_inbox(&lease).await.expect("take_inbox");
    let big = Digest::of(b"big");
    let call = |seq: u32, args: Payload| Command::ScheduleToolCall {
        seq: Seq(seq),
        tool: "a".into(),
        args,
        callsite: None,
    };
    let entries = vec![
        run_started_entry(),
        entry(1, Event::Command(call(1, json!({ "x": 1 }).into()))),
        entry(2, Event::Command(call(2, Payload::Digest(big)))),
        entry(
            3,
            Event::Command(Command::StartTimer {
                seq: Seq(3),
                duration_ms: 10,
                fire_at: Timestamp(T0.0 + 10),
                callsite: None,
            }),
        ),
    ];
    let mut activation = Activation::new(entries.clone(), vec![inbox[0].id], RunStatus::Active);
    let mut approved = host_task(2, "a");
    approved.policy.approval = true;
    activation.tool_calls = vec![host_task(1, "a"), approved];
    activation.timers.push(NewTimer {
        seq: Seq(3),
        fire_at: Timestamp(T0.0 + 10),
    });
    activation.payloads.push(PayloadRecord {
        digest: big,
        size: 2_000,
        preview: "{\"x\":\"…".into(),
        value: Some(json!({ "x": "y" })),
    });
    let committed_at = later(T0, 5);
    commit(store, &lease, activation, committed_at).await;

    assert_eq!(
        store.load_history(&run, 0).await.expect("load_history"),
        entries
    );
    assert_eq!(
        store.load_history(&run, 2).await.expect("load_history"),
        entries[2..]
    );
    assert!(
        store
            .load_history(&run, 9)
            .await
            .expect("load_history")
            .is_empty()
    );
    assert!(
        store
            .take_inbox(&lease)
            .await
            .expect("take_inbox")
            .is_empty()
    );
    let record = record(store, &run).await;
    assert_eq!(record.status, RunStatus::Active);
    assert_eq!(record.history_len, 4);
    assert_eq!(record.updated_at, Timestamp(T0.0 + 3));
    assert_eq!(record.sticky_worker.as_deref(), Some("worker"));
    assert_eq!(
        store.next_timer().await.expect("next_timer"),
        Some(Timestamp(T0.0 + 10))
    );

    let tasks = store.tool_tasks(&run).await.expect("tool_tasks");
    assert_eq!(tasks.len(), 2);
    assert_eq!(
        tasks[0].task,
        TaskRef {
            run: run.clone(),
            seq: Seq(1)
        }
    );
    assert_eq!(
        tasks[0].state,
        TaskState::Ready {
            next_attempt_at: committed_at
        }
    );
    assert_eq!(tasks[0].attempt, 0);
    assert_eq!(tasks[0].toolset, toolset_digest());
    assert_eq!(tasks[0].args, Payload::from(json!({ "seq": 1 })));
    assert_eq!(tasks[0].created_at, committed_at);
    assert_eq!(
        tasks[1].state,
        TaskState::Approval,
        "the approval policy holds the task"
    );

    let payload = store
        .get_payload(&acme(), big)
        .await
        .expect("get_payload")
        .expect("recorded at commit");
    assert_eq!(payload.size, 2_000);
    assert_eq!(payload.value, Some(json!({ "x": "y" })));

    let feed = store
        .claim_feed(
            FeedConsumer::ReadModel,
            "projector",
            committed_at,
            LEASE,
            10,
        )
        .await
        .expect("claim_feed");
    assert_eq!(feed.len(), 1);
    assert_eq!(
        (feed[0].run.clone(), feed[0].fence),
        (run.clone(), lease.fence)
    );
    assert_eq!((feed[0].from_idx, feed[0].to_idx), (0, 4));
}

pub async fn a_commit_with_a_stale_fence_writes_nothing<S: JournalStore>(store: &S) {
    let run = create(store, run_ref(1)).await;
    let stale = acquire(store, T0).await.expect("runnable");
    let inbox = store.take_inbox(&stale).await.expect("take_inbox");
    let live = acquire(store, T0.saturating_add(LEASE))
        .await
        .expect("taken over");

    let mut activation = Activation::new(
        vec![run_started_entry()],
        vec![inbox[0].id],
        RunStatus::Active,
    );
    activation.tool_calls.push(host_task(1, "a"));
    activation.timers.push(NewTimer {
        seq: Seq(2),
        fire_at: T0,
    });
    let result = store.commit_activation(&stale, activation, T0).await;
    assert!(
        matches!(result, Err(StoreError::Fenced { .. })),
        "{result:?}"
    );
    assert!(
        store
            .load_history(&run, 0)
            .await
            .expect("load_history")
            .is_empty()
    );
    assert_eq!(store.take_inbox(&live).await.expect("take_inbox").len(), 1);
    assert!(store.tool_tasks(&run).await.expect("tool_tasks").is_empty());
    assert_eq!(store.next_timer().await.expect("next_timer"), None);
    assert!(
        store
            .claim_feed(FeedConsumer::Otel, "p", T0, LEASE, 10)
            .await
            .expect("claim_feed")
            .is_empty()
    );
    for result in [
        store
            .quarantine(&stale, QuarantineKind::NonDeterminism, "x".into(), T0)
            .await
            .map(|_| ()),
        store
            .record_failure(&stale, "x".into(), T0)
            .await
            .map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(StoreError::Fenced { .. })),
            "{result:?}"
        );
    }
}

pub async fn a_commit_must_continue_the_history<S: JournalStore>(store: &S) {
    let run = start(store, run_ref(1)).await;
    let lease = lease_of(store, &run, T0).await;
    for index in [0, 2] {
        let result = store
            .commit_activation(
                &lease,
                Activation::new(vec![log_entry(index)], vec![], RunStatus::Active),
                T0,
            )
            .await;
        assert!(
            matches!(result, Err(StoreError::Conflict(_))),
            "index {index}: {result:?}"
        );
    }
    let gap = vec![log_entry(1), log_entry(3)];
    let result = store
        .commit_activation(&lease, Activation::new(gap, vec![], RunStatus::Active), T0)
        .await;
    assert!(matches!(result, Err(StoreError::Conflict(_))), "{result:?}");
    assert_eq!(
        store
            .load_history(&run, 0)
            .await
            .expect("load_history")
            .len(),
        1,
        "all or nothing"
    );
}

pub async fn a_terminal_commit_clears_the_run<S: JournalStore>(store: &S) {
    let run = start_with_tasks(store, run_ref(1), vec![host_task(1, "a")]).await;
    let lease = lease_of(store, &run, T0).await;
    let mut activation = Activation::new(vec![log_entry(1)], vec![], RunStatus::Active);
    activation.timers.push(NewTimer {
        seq: Seq(2),
        fire_at: Timestamp(T0.0 + 10),
    });
    commit(store, &lease, activation, T0).await;
    store
        .enqueue(&run, Completion::TimerFired { seq: Seq(9) }, None, T0)
        .await
        .expect("enqueue");
    let token = store
        .create_webhook_token(&run, "pr_merged")
        .await
        .expect("create_webhook_token");

    let done = entry(
        2,
        Event::Command(Command::CompleteRun {
            output: json!(null).into(),
        }),
    );
    let ended = Timestamp(T0.0 + 2);
    commit(
        store,
        &lease,
        Activation::new(vec![done], vec![], RunStatus::Completed),
        T0,
    )
    .await;
    store.release(&lease).await.expect("release");

    assert_eq!(store.next_timer().await.expect("next_timer"), None);
    assert!(store.tool_tasks(&run).await.expect("tool_tasks").is_empty());
    assert!(claim(store, "w", &TaskFilter::All, T0, 10).await.is_empty());
    assert_eq!(
        acquire(store, T0).await,
        None,
        "a terminal run is never runnable"
    );
    assert_eq!(
        store
            .acquire_run(&run, "worker", T0, LEASE)
            .await
            .expect("acquire_run"),
        None
    );
    assert_eq!(
        store.resolve_webhook_token(&token).await.expect("resolve"),
        None
    );
    let record = record(store, &run).await;
    assert_eq!(record.status, RunStatus::Completed);
    assert_eq!(record.ended_at, Some(ended));
    assert_eq!(store.next_deadline().await.expect("next_deadline"), None);
}

pub async fn enqueue_deduplicates_keys_and_refuses_ended_runs<S: JournalStore>(store: &S) {
    let run = start(store, run_ref(1)).await;
    let signal = || Completion::SignalReceived {
        name: "approval".into(),
        payload: json!(true).into(),
    };
    let enqueue = |key: Option<&str>| store.enqueue(&run, signal(), key.map(Into::into), T0);
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
    let items = store.take_inbox(&lease).await.expect("take_inbox");
    assert_eq!(items.len(), 4);
    assert!(
        items.windows(2).all(|pair| pair[0].id < pair[1].id),
        "arrival order"
    );

    // A key outlives the delivery of its signal.
    let consumed = items.iter().map(|item| item.id).collect();
    commit(
        store,
        &lease,
        Activation::new(vec![], consumed, RunStatus::Active),
        T0,
    )
    .await;
    store.release(&lease).await.expect("release");
    assert_eq!(
        enqueue(Some("k1")).await.expect("enqueue"),
        Enqueued::Duplicate
    );
    assert_eq!(acquire(store, T0).await, None, "nothing left to deliver");

    let lease = lease_of(store, &run, T0).await;
    commit(
        store,
        &lease,
        Activation::new(vec![aborted_entry(1)], vec![], RunStatus::Aborted),
        T0,
    )
    .await;
    assert_eq!(
        enqueue(Some("k3")).await.expect("enqueue"),
        Enqueued::RunEnded
    );
    assert!(matches!(
        store.enqueue(&run_ref(99), signal(), None, T0).await,
        Err(StoreError::RunNotFound(_))
    ));
}

pub async fn runs_of_another_tenant_are_invisible<S: JournalStore>(store: &S) {
    let run = start_with_tasks(store, run_ref(1), vec![host_task(1, "a")]).await;
    let intruder = run_in(&tenant("intruder"), 1);
    assert_eq!(store.get_run(&intruder).await.expect("get_run"), None);
    assert!(matches!(
        store
            .enqueue(&intruder, Completion::TimerFired { seq: Seq(1) }, None, T0)
            .await,
        Err(StoreError::RunNotFound(_))
    ));
    assert!(matches!(
        store.load_history(&intruder, 0).await,
        Err(StoreError::RunNotFound(_))
    ));
    assert!(matches!(
        store.acquire_run(&intruder, "w", T0, LEASE).await,
        Err(StoreError::RunNotFound(_))
    ));
    assert!(matches!(
        store.tool_tasks(&intruder).await,
        Err(StoreError::RunNotFound(_))
    ));
    let task = TaskRef {
        run: intruder.clone(),
        seq: Seq(1),
    };
    assert_eq!(
        store.approve_tool_task(&task, T0).await.expect("approve"),
        TaskUpdate::NotFound
    );
    assert_eq!(
        store
            .reject_tool_task(
                &task,
                ToolFailure {
                    kind: ToolFailureKind::Error,
                    message: "no".into(),
                    data: None
                },
                T0
            )
            .await
            .expect("reject"),
        TaskUpdate::NotFound
    );
    assert!(
        store
            .list_runs(&RunFilter::new(tenant("intruder")))
            .await
            .expect("list_runs")
            .is_empty()
    );
    assert!(
        !store
            .release_quarantine(&run)
            .await
            .expect("release_quarantine")
    );
    assert!(!store.delete_run(&intruder, T0).await.expect("delete_run"));
    assert!(store.get_run(&run).await.expect("get_run").is_some());
}

// -------------------------------------------------------------------------------------------
// Failures and quarantine

pub async fn failures_are_counted_and_the_fifth_quarantines<S: JournalStore>(store: &S) {
    let run = start(store, run_ref(1)).await;
    store
        .enqueue(&run, Completion::TimerFired { seq: Seq(1) }, None, T0)
        .await
        .expect("enqueue");
    let mut at = T0;
    for count in 1..QUARANTINE_AFTER_FAILURES {
        let lease = acquire(store, at)
            .await
            .expect("runnable once the lease expired");
        let failures = store
            .record_failure(&lease, format!("blob missing ({count})"), at)
            .await
            .expect("record_failure");
        assert_eq!((failures.count, failures.quarantined), (count, false));
        assert_eq!(
            acquire(store, at).await,
            None,
            "the lease is kept until it expires"
        );
        assert_eq!(record(store, &run).await.failures, count);
        at = at.saturating_add(LEASE);
    }
    let lease = acquire(store, at).await.expect("runnable");
    let failures = store
        .record_failure(&lease, "blob missing (5)".into(), at)
        .await
        .expect("record_failure");
    assert_eq!(
        (failures.count, failures.quarantined),
        (QUARANTINE_AFTER_FAILURES, true)
    );
    let quarantine = record(store, &run).await.quarantine.expect("quarantined");
    assert_eq!(quarantine.kind, QuarantineKind::Failures);
    assert_eq!(quarantine.reason, "blob missing (5)");
    assert_eq!(quarantine.since, at);
    assert_eq!(
        acquire(store, at.saturating_add(LEASE)).await,
        None,
        "never offered"
    );
    let explicit = lease_of(store, &run, at).await;
    store.release(&explicit).await.expect("release");

    // Releasing the quarantine resets the count.
    assert!(
        store
            .release_quarantine(&run)
            .await
            .expect("release_quarantine")
    );
    assert_eq!(record(store, &run).await.failures, 0);
    let lease = acquire(store, at).await.expect("runnable again");
    let failures = store
        .record_failure(&lease, "x".into(), at)
        .await
        .expect("record_failure");
    assert_eq!(failures.count, 1);

    // A commit resets it too.
    let lease = lease_of(store, &run, at.saturating_add(LEASE)).await;
    let consumed = inbox_ids(store, &lease).await;
    commit(
        store,
        &lease,
        Activation::new(vec![], consumed, RunStatus::Active),
        at,
    )
    .await;
    assert_eq!(record(store, &run).await.failures, 0);
}

pub async fn activations_started_since_the_last_commit_are_counted<S: JournalStore>(store: &S) {
    let run = start(store, run_ref(1)).await;
    assert_eq!(record(store, &run).await.attempts, 0);
    store
        .enqueue(&run, Completion::TimerFired { seq: Seq(1) }, None, T0)
        .await
        .expect("enqueue");

    // Each lease `acquire_runnable` hands out counts, whether the activation fails, records
    // its failure, or never ends because its worker died.
    let mut at = T0;
    for attempts in 1..=3 {
        acquire(store, at)
            .await
            .expect("runnable once the lease expired");
        let found = record(store, &run).await;
        assert_eq!((found.attempts, found.failures), (attempts, 0));
        at = at.saturating_add(LEASE);
    }
    let lease = acquire(store, at).await.expect("runnable");
    let failures = store
        .record_failure(&lease, "blob missing".into(), at)
        .await
        .expect("record_failure");
    assert_eq!(failures.count, 1, "record_failure counts what it records");
    let found = record(store, &run).await;
    assert_eq!((found.attempts, found.failures), (4, 1));

    // Leasing a given run, or a quarantined one, activates nothing that counts.
    at = at.saturating_add(LEASE);
    let explicit = lease_of(store, &run, at).await;
    store.release(&explicit).await.expect("release");
    assert_eq!(record(store, &run).await.attempts, 4);

    // A commit resets the count.
    let lease = acquire(store, at).await.expect("runnable");
    assert_eq!(record(store, &run).await.attempts, 5);
    let consumed = inbox_ids(store, &lease).await;
    commit(
        store,
        &lease,
        Activation::new(vec![], consumed, RunStatus::Active),
        at,
    )
    .await;
    store.release(&lease).await.expect("release");
    let found = record(store, &run).await;
    assert_eq!((found.attempts, found.failures), (0, 0));

    // The engine quarantines a run whose activations never finished, with the kind
    // `failures`; releasing the quarantine resets the count.
    store
        .enqueue(&run, Completion::TimerFired { seq: Seq(2) }, None, at)
        .await
        .expect("enqueue");
    acquire(store, at).await.expect("runnable");
    at = at.saturating_add(LEASE);
    let lease = acquire(store, at)
        .await
        .expect("runnable once the lease expired");
    assert_eq!(record(store, &run).await.attempts, 2);
    store
        .quarantine(
            &lease,
            QuarantineKind::Failures,
            "the activation never finished".into(),
            at,
        )
        .await
        .expect("quarantine");
    let quarantine = record(store, &run).await.quarantine.expect("quarantined");
    assert_eq!(quarantine.kind, QuarantineKind::Failures);
    assert_eq!(quarantine.reason, "the activation never finished");
    assert_eq!(acquire(store, at).await, None, "never offered");
    let retried = store
        .acquire_quarantined("w", "build-2", EPOCHS, &ALL_TENANTS, at, LEASE)
        .await
        .expect("acquire_quarantined")
        .expect("a new build retries it");
    store.release(&retried).await.expect("release");
    assert_eq!(record(store, &run).await.attempts, 2);
    assert!(
        store
            .release_quarantine(&run)
            .await
            .expect("release_quarantine")
    );
    assert_eq!(record(store, &run).await.attempts, 0);
    acquire(store, at).await.expect("runnable again");
    assert_eq!(record(store, &run).await.attempts, 1);
}

pub async fn quarantine_releases_the_lease_and_hides_the_run<S: JournalStore>(store: &S) {
    let run = start(store, run_ref(1)).await;
    store
        .enqueue(&run, Completion::TimerFired { seq: Seq(1) }, None, T0)
        .await
        .expect("enqueue");
    let lease = acquire(store, T0).await.expect("runnable");
    let at = later(T0, 7);
    store
        .quarantine(
            &lease,
            QuarantineKind::NonDeterminism,
            "history entry 1 is …".into(),
            at,
        )
        .await
        .expect("quarantine");
    assert_eq!(store.load_history(&run, 0).await.expect("history").len(), 1);
    assert_eq!(
        acquire(store, T0).await,
        None,
        "a quarantined run is never offered"
    );
    let quarantine = record(store, &run).await.quarantine.expect("quarantined");
    assert_eq!(quarantine.kind, QuarantineKind::NonDeterminism);
    assert_eq!(quarantine.reason, "history entry 1 is …");
    assert_eq!(quarantine.since, at);
    assert_eq!(quarantine.retried_by, None);

    // The lease was released: the run can be leased explicitly, to give it up.
    let explicit = lease_of(store, &run, T0).await;
    store.release(&explicit).await.expect("release");

    assert!(
        store
            .release_quarantine(&run)
            .await
            .expect("release_quarantine")
    );
    assert!(
        !store
            .release_quarantine(&run)
            .await
            .expect("release_quarantine")
    );
    assert_eq!(acquire(store, T0).await.expect("runnable again").run, run);
}

pub async fn a_new_build_retries_each_quarantined_run_once<S: JournalStore>(store: &S) {
    let run = start(store, run_ref(1)).await;
    let mut epoch_2 = new_run(run_ref(2));
    epoch_2.started.epoch = Epoch(2);
    store.create_run(epoch_2, None).await.expect("create_run");
    start(store, run_ref(3)).await;
    for quarantined in [&run, &run_ref(2)] {
        let lease = lease_of(store, quarantined, T0).await;
        store
            .quarantine(
                &lease,
                QuarantineKind::NonDeterminism,
                "mismatch".into(),
                T0,
            )
            .await
            .expect("quarantine");
    }

    let retry = |build: &'static str, at: Timestamp| async move {
        store
            .acquire_quarantined("w", build, EPOCHS, &ALL_TENANTS, at, LEASE)
            .await
            .expect("acquire_quarantined")
    };
    let lease = retry("build-2", T0)
        .await
        .expect("the quarantined run of epoch 1");
    assert_eq!(lease.run, run);
    assert_eq!(
        record(store, &run)
            .await
            .quarantine
            .expect("still quarantined")
            .retried_by
            .as_deref(),
        Some("build-2")
    );
    // The retry fails again: the run stays quarantined, and build-2 never retries it again.
    store
        .quarantine(
            &lease,
            QuarantineKind::NonDeterminism,
            "mismatch again".into(),
            T0,
        )
        .await
        .expect("quarantine");
    let quarantine = record(store, &run).await.quarantine.expect("quarantined");
    assert_eq!(quarantine.reason, "mismatch again");
    assert_eq!(quarantine.retried_by.as_deref(), Some("build-2"));
    assert_eq!(
        retry("build-2", T0).await,
        None,
        "once per build; epoch 2 is not supported"
    );
    let lease = retry("build-3", T0)
        .await
        .expect("a newer build retries it");
    assert_eq!(lease.run, run);
    assert_eq!(
        retry("build-4", T0).await,
        None,
        "a leased run is not offered"
    );
}

pub async fn quarantined_runs_are_retried_by_workers_of_their_tenant<S: JournalStore>(store: &S) {
    let globex = tenant("globex");
    let run = start(store, run_ref(1)).await;
    let lease = lease_of(store, &run, T0).await;
    store
        .quarantine(
            &lease,
            QuarantineKind::NonDeterminism,
            "mismatch".into(),
            T0,
        )
        .await
        .expect("quarantine");

    let retry = |scope: TenantScope| async move {
        store
            .acquire_quarantined("w", "build-2", EPOCHS, &scope, T0, LEASE)
            .await
            .expect("acquire_quarantined")
    };
    for scope in [
        TenantScope::Only(BTreeSet::from([globex.clone()])),
        TenantScope::Except(BTreeSet::from([acme()])),
        TenantScope::Only(BTreeSet::new()),
    ] {
        assert_eq!(retry(scope.clone()).await, None, "{scope:?}");
    }
    let quarantine = record(store, &run).await.quarantine.expect("quarantined");
    assert_eq!(
        quarantine.retried_by, None,
        "a worker of another pool does not spend the build's retry"
    );
    let unleased = store
        .acquire_run(&run, "other", T0, LEASE)
        .await
        .expect("acquire_run")
        .expect("the run was not leased");
    store.release(&unleased).await.expect("release");

    let lease = retry(TenantScope::Only(BTreeSet::from([globex, acme()])))
        .await
        .expect("a worker of acme's pool retries it");
    assert_eq!(lease.run, run);
    let quarantine = record(store, &run).await.quarantine.expect("quarantined");
    assert_eq!(quarantine.retried_by.as_deref(), Some("build-2"));
}

// -------------------------------------------------------------------------------------------
// Time

pub async fn timers_fire_once_earliest_first<S: JournalStore>(store: &S) {
    let run = start(store, run_ref(1)).await;
    let lease = lease_of(store, &run, T0).await;
    let mut activation = Activation::new(vec![], vec![], RunStatus::Active);
    for (seq, at) in [(1, 30), (2, 10), (3, 20), (4, 25)] {
        activation.timers.push(NewTimer {
            seq: Seq(seq),
            fire_at: Timestamp(T0.0 + at),
        });
    }
    commit(store, &lease, activation, T0).await;
    let mut cancel = Activation::new(vec![], vec![], RunStatus::Active);
    cancel.cancelled_timers.push(Seq(3));
    commit(store, &lease, cancel, T0).await;
    store.release(&lease).await.expect("release");

    assert_eq!(
        store.next_timer().await.expect("next_timer"),
        Some(Timestamp(T0.0 + 10))
    );
    let fire = |at: u64, limit| store.fire_due_timers(Timestamp(T0.0 + at), limit);
    assert_eq!(fire(9, 10).await.expect("fire"), 0);
    assert_eq!(fire(30, 1).await.expect("fire"), 1, "up to the limit");
    assert_eq!(
        store.next_timer().await.expect("next_timer"),
        Some(Timestamp(T0.0 + 25))
    );
    assert_eq!(fire(30, 10).await.expect("fire"), 2);
    assert_eq!(fire(30, 10).await.expect("fire"), 0, "a timer fires once");
    assert_eq!(store.next_timer().await.expect("next_timer"), None);

    assert_eq!(
        inbox(store, &run, T0).await,
        [2, 4, 1].map(|seq| Trigger::Completion(Completion::TimerFired { seq: Seq(seq) })),
        "earliest first"
    );
}

pub async fn runs_expire_once_and_quarantined_runs_are_reported<S: JournalStore>(store: &S) {
    let deadline = Timestamp(T0.0 + 1_000);
    let later_deadline = Timestamp(T0.0 + 1_500);
    let mut short = new_run(run_ref(1));
    short.started.expires_at = deadline;
    store.create_run(short, None).await.expect("create_run");
    let mut quarantined_run = new_run(run_ref(2));
    quarantined_run.started.expires_at = later_deadline;
    store
        .create_run(quarantined_run, None)
        .await
        .expect("create_run");
    create(store, run_ref(3)).await;
    let lease = lease_of(store, &run_ref(2), T0).await;
    store
        .quarantine(
            &lease,
            QuarantineKind::NonDeterminism,
            "mismatch".into(),
            T0,
        )
        .await
        .expect("quarantine");

    assert_eq!(
        store.next_deadline().await.expect("next_deadline"),
        Some(deadline)
    );
    let none = store
        .expire_due_runs(Timestamp(deadline.0 - 1), 10)
        .await
        .expect("expire");
    assert_eq!(none, ExpiredRuns::default());

    let expired = store.expire_due_runs(deadline, 10).await.expect("expire");
    assert_eq!(expired.cancelled, [run_ref(1)]);
    assert!(expired.quarantined.is_empty());
    assert_eq!(
        store.next_deadline().await.expect("next_deadline"),
        Some(later_deadline),
        "a run whose expiry is enqueued has no deadline left"
    );
    assert_eq!(
        store.expire_due_runs(deadline, 10).await.expect("expire"),
        ExpiredRuns::default()
    );

    let reported = store
        .expire_due_runs(later_deadline, 10)
        .await
        .expect("expire");
    assert!(
        reported.cancelled.is_empty(),
        "nothing is enqueued for a quarantined run"
    );
    assert_eq!(reported.quarantined, [run_ref(2)]);
    let again = store
        .expire_due_runs(later_deadline, 10)
        .await
        .expect("expire");
    assert_eq!(again.quarantined, [run_ref(2)], "until it is given up");

    assert_eq!(
        inbox(store, &run_ref(1), deadline).await.last(),
        Some(&Trigger::Completion(Completion::CancelRequested {
            reason: CancelReason::Ttl
        }))
    );

    // The limit bounds what one call does.
    let mut a = new_run(run_ref(4));
    a.started.expires_at = deadline;
    let mut b = new_run(run_ref(5));
    b.started.expires_at = deadline;
    store.create_run(a, None).await.expect("create_run");
    store.create_run(b, None).await.expect("create_run");
    let first = store.expire_due_runs(deadline, 1).await.expect("expire");
    assert_eq!(first.cancelled.len() + first.quarantined.len(), 1);
}

// -------------------------------------------------------------------------------------------
// Tool tasks

pub async fn tool_tasks_are_claimed_and_completed_once<S: JournalStore>(store: &S) {
    let run = start_with_tasks(
        store,
        run_ref(1),
        vec![host_task(1, "t1"), host_task(2, "t2"), host_task(3, "t3")],
    )
    .await;
    let first = claim(store, "a", &TaskFilter::All, T0, 2).await;
    assert_eq!(seqs(&first), [1, 2]);
    assert_eq!(first[0].task.attempt, 1);
    assert!(!first[0].previous_claim_expired);
    let heartbeat = ToolPolicy::default_for(&ToolRoute::Host { group: None }).heartbeat_timeout;
    assert_eq!(
        first[0].task.state,
        TaskState::Claimed {
            owner: "a".into(),
            until: T0.saturating_add(heartbeat)
        }
    );
    assert_ne!(first[0].token, first[1].token);
    assert_eq!(
        seqs(&claim(store, "b", &TaskFilter::All, T0, 10).await),
        [3]
    );
    assert!(claim(store, "c", &TaskFilter::All, T0, 10).await.is_empty());

    assert_eq!(
        store
            .complete_tool_task(&first[0].token, result(1), T0)
            .await
            .expect("complete"),
        TaskUpdate::Applied
    );
    assert_eq!(
        store
            .complete_tool_task(&first[0].token, result(1), T0)
            .await
            .expect("complete"),
        TaskUpdate::NotFound,
        "a second outcome is dropped"
    );
    let failure = ToolFailure {
        kind: ToolFailureKind::Error,
        message: "boom".into(),
        data: Some(json!({ "code": 7 })),
    };
    store
        .complete_tool_task(&first[1].token, ToolOutcome::Failure(failure.clone()), T0)
        .await
        .expect("complete");
    assert_eq!(
        inbox(store, &run, T0).await,
        [
            Trigger::Completion(result(1).into_completion(Seq(1))),
            Trigger::Completion(ToolOutcome::Failure(failure).into_completion(Seq(2))),
        ]
    );
    let left: Vec<u32> = store
        .tool_tasks(&run)
        .await
        .expect("tool_tasks")
        .iter()
        .map(|t| t.task.seq.0)
        .collect();
    assert_eq!(left, [3]);
    assert_eq!(
        store
            .complete_tool_task(
                &crate::ports::TaskToken("rewind_tt_unknown".into()),
                result(9),
                T0
            )
            .await
            .expect("complete"),
        TaskUpdate::NotFound
    );
}

pub async fn an_expired_claim_is_claimed_again_and_its_token_dies<S: JournalStore>(store: &S) {
    let run = start_with_tasks(store, run_ref(1), vec![host_task(1, "t")]).await;
    let first = claim(store, "a", &TaskFilter::All, T0, 10).await.remove(0);
    let heartbeat = first.task.policy.heartbeat_timeout;

    // A heartbeat keeps the claim alive.
    let beat = later(T0, 50_000);
    assert_eq!(
        store
            .heartbeat_tool_task(&first.token, beat)
            .await
            .expect("heartbeat"),
        Some(beat.saturating_add(heartbeat))
    );
    assert!(
        claim(
            store,
            "b",
            &TaskFilter::All,
            T0.saturating_add(heartbeat),
            10
        )
        .await
        .is_empty(),
        "the heartbeat extended the claim"
    );

    // Without heartbeats the claim expires and the task is claimed again, attempt 2.
    let expired = beat.saturating_add(heartbeat);
    let second = claim(store, "b", &TaskFilter::All, expired, 10)
        .await
        .remove(0);
    assert_eq!(second.task.attempt, 2);
    assert!(second.previous_claim_expired);
    assert_ne!(second.token, first.token);
    assert_eq!(
        store
            .heartbeat_tool_task(&first.token, expired)
            .await
            .expect("heartbeat"),
        None
    );
    assert_eq!(
        store
            .complete_tool_task(&first.token, result(1), expired)
            .await
            .expect("complete"),
        TaskUpdate::NotFound,
        "the token of an expired claim taken again is dead"
    );
    assert_eq!(
        store
            .complete_tool_task(&second.token, result(1), expired)
            .await
            .expect("complete"),
        TaskUpdate::Applied
    );
    assert_eq!(inbox(store, &run, expired).await.len(), 1);
}

pub async fn expired_host_claims_that_cannot_be_handed_out_again_are_interrupted<
    S: JournalStore,
>(
    store: &S,
) {
    let other = tenant("other");
    let at_most_once = |mut task: NewToolTask| {
        task.policy.on_interruption = rewind_core::Interruption::AtMostOnce;
        task
    };
    start_with_tasks(
        store,
        run_ref(1),
        vec![
            at_most_once(host_task(1, "pay")),
            host_task(2, "lookup"),
            at_most_once(mcp_task(3, "github.a", "github", 8)),
        ],
    )
    .await;
    start_with_tasks(
        store,
        run_in(&other, 2),
        vec![at_most_once(host_task(1, "pay"))],
    )
    .await;
    let interrupted = TaskFilter::Interrupted {
        tenants: TenantScope::Only(BTreeSet::from([acme()])),
        max_attempts: 2,
    };
    assert!(
        claim(store, "s", &interrupted, T0, 10).await.is_empty(),
        "a task never claimed was not interrupted"
    );
    let first = claim(store, "a", &TaskFilter::All, T0, 10).await;
    assert_eq!(first.len(), 4);
    let heartbeat = first[0].task.policy.heartbeat_timeout;
    assert!(
        claim(store, "s", &interrupted, T0, 10).await.is_empty(),
        "the claims are alive"
    );

    // The claims expire: only acme's at_most_once host task cannot be handed out again.
    let expired = T0.saturating_add(heartbeat);
    let swept = claim(store, "s", &interrupted, expired, 10).await;
    assert_eq!(seqs(&swept), [1]);
    assert_eq!(swept[0].task.task.run.tenant, acme());
    assert!(swept[0].previous_claim_expired);
    assert_eq!(swept[0].task.attempt, 2);
    assert_eq!(
        store
            .complete_tool_task(&swept[0].token, result(1), expired)
            .await
            .expect("complete"),
        TaskUpdate::Applied
    );

    // The retry task is claimed again, its second attempt, and that claim expires too: it has
    // had its attempts.
    let host = TaskFilter::Host {
        tenant: acme(),
        namespace: None,
        toolset: None,
        tools: BTreeSet::new(),
    };
    let second = claim(store, "a", &host, expired, 10).await;
    assert_eq!(seqs(&second), [2]);
    assert_eq!(second[0].task.attempt, 2);
    let expired_again = expired.saturating_add(heartbeat);
    let swept = claim(store, "s", &interrupted, expired_again, 10).await;
    assert_eq!(seqs(&swept), [2]);
    assert_eq!(swept[0].task.attempt, 3);
}

pub async fn transient_failures_wait_for_their_next_attempt<S: JournalStore>(store: &S) {
    let run = start_with_tasks(store, run_ref(1), vec![host_task(1, "t")]).await;
    let first = claim(store, "a", &TaskFilter::All, T0, 10).await.remove(0);
    let next_attempt_at = later(T0, 2_000);
    assert_eq!(
        store
            .retry_tool_task(&first.token, next_attempt_at)
            .await
            .expect("retry"),
        TaskUpdate::Applied
    );
    assert_eq!(
        store.tool_tasks(&run).await.expect("tool_tasks")[0].state,
        TaskState::Ready { next_attempt_at }
    );
    assert_eq!(
        store
            .retry_tool_task(&first.token, next_attempt_at)
            .await
            .expect("retry"),
        TaskUpdate::NotFound,
        "the claim's token is spent"
    );
    assert!(
        claim(
            store,
            "a",
            &TaskFilter::All,
            Timestamp(next_attempt_at.0 - 1),
            10
        )
        .await
        .is_empty()
    );
    let second = claim(store, "a", &TaskFilter::All, next_attempt_at, 10)
        .await
        .remove(0);
    assert_eq!(second.task.attempt, 2);
    assert!(
        !second.previous_claim_expired,
        "a retry is not an interruption"
    );
}

pub async fn the_next_tool_attempt_is_the_earliest_retry_still_to_come<S: JournalStore>(store: &S) {
    let next = |after: Timestamp| async move {
        store
            .next_tool_attempt(after)
            .await
            .expect("next_tool_attempt")
    };
    assert_eq!(next(T0).await, None, "no task");
    let other = tenant("other");
    start_with_tasks(
        store,
        run_ref(1),
        vec![host_task(1, "t"), host_task(2, "u")],
    )
    .await;
    start_with_tasks(store, run_in(&other, 2), vec![host_task(1, "t")]).await;
    assert_eq!(
        next(T0).await,
        None,
        "a task ready now is claimable: it is no wake-up"
    );

    let claimed = claim(store, "a", &TaskFilter::All, T0, 10).await;
    assert_eq!(claimed.len(), 3);
    assert_eq!(next(T0).await, None, "a claimed task is no retry");
    let retry_at = |task: &crate::ports::ClaimedTask| match task.task.task.seq.0 {
        1 if task.task.task.run.tenant == other => later(T0, 1_000),
        1 => later(T0, 5_000),
        _ => later(T0, 3_000),
    };
    for task in &claimed {
        assert_eq!(
            store
                .retry_tool_task(&task.token, retry_at(task))
                .await
                .expect("retry"),
            TaskUpdate::Applied
        );
    }
    assert_eq!(
        next(T0).await,
        Some(later(T0, 1_000)),
        "the earliest retry, whatever its tenant"
    );
    assert_eq!(
        next(later(T0, 1_000)).await,
        Some(later(T0, 3_000)),
        "only retries later than `after`"
    );
    assert_eq!(next(later(T0, 4_000)).await, Some(later(T0, 5_000)));
    assert_eq!(next(later(T0, 5_000)).await, None);

    // Claimed again, a task no longer waits.
    let again = claim(store, "a", &TaskFilter::All, later(T0, 3_000), 10).await;
    assert_eq!(again.len(), 2);
    assert_eq!(next(T0).await, Some(later(T0, 5_000)));
}

pub async fn deferred_tasks_keep_their_token_until_answered_or_late<S: JournalStore>(store: &S) {
    let run = start_with_tasks(
        store,
        run_ref(1),
        vec![host_task(1, "t"), host_task(2, "u")],
    )
    .await;
    let claimed = claim(store, "a", &TaskFilter::All, T0, 10).await;
    let until = later(T0, 3 * DAY);
    for task in &claimed {
        assert_eq!(
            store
                .defer_tool_task(&task.token, until)
                .await
                .expect("defer"),
            TaskUpdate::Applied
        );
    }
    assert_eq!(
        store.tool_tasks(&run).await.expect("tool_tasks")[0].state,
        TaskState::Deferred { until }
    );
    assert_eq!(
        store
            .heartbeat_tool_task(&claimed[0].token, later(T0, DAY))
            .await
            .expect("heartbeat"),
        Some(until),
        "a deferred task's deadline does not move"
    );
    let far = later(T0, 2 * DAY);
    assert!(
        claim(store, "b", &TaskFilter::All, far, 10)
            .await
            .is_empty(),
        "never claimed again"
    );
    assert_eq!(
        store
            .retry_tool_task(&claimed[0].token, far)
            .await
            .expect("retry"),
        TaskUpdate::InvalidState(TaskState::Deferred { until })
    );

    // Task 1 is answered days later, through its token; task 2 never is.
    assert_eq!(
        store
            .complete_tool_task(&claimed[0].token, result(1), far)
            .await
            .expect("complete"),
        TaskUpdate::Applied
    );
    assert_eq!(
        store
            .expire_deferred_tool_tasks(Timestamp(until.0 - 1), 10)
            .await
            .expect("expire"),
        0
    );
    assert_eq!(
        store
            .expire_deferred_tool_tasks(until, 10)
            .await
            .expect("expire"),
        1
    );
    assert_eq!(
        store
            .expire_deferred_tool_tasks(until, 10)
            .await
            .expect("expire"),
        0
    );
    assert_eq!(
        inbox(store, &run, until).await,
        [
            Trigger::Completion(result(1).into_completion(Seq(1))),
            Trigger::Completion(
                ToolOutcome::Failure(deferred_timeout_failure()).into_completion(Seq(2))
            ),
        ]
    );
    assert_eq!(
        store
            .complete_tool_task(&claimed[1].token, result(2), until)
            .await
            .expect("complete"),
        TaskUpdate::NotFound,
        "too late"
    );
}

pub async fn approval_and_parked_tasks_wait_for_the_harness<S: JournalStore>(store: &S) {
    let mut approval = host_task(1, "t");
    approval.policy.approval = true;
    let mut rejected = host_task(2, "t");
    rejected.policy.approval = true;
    let run = start_with_tasks(
        store,
        run_ref(1),
        vec![approval, rejected, host_task(3, "t")],
    )
    .await;
    let task = |seq| TaskRef {
        run: run.clone(),
        seq: Seq(seq),
    };

    let first = claim(store, "a", &TaskFilter::All, T0, 10).await;
    assert_eq!(seqs(&first), [3], "approval tasks wait");
    assert!(!first[0].task.schema_change_approved);
    let approved_at = later(T0, 100);
    assert_eq!(
        store
            .approve_tool_task(&task(1), approved_at)
            .await
            .expect("approve"),
        TaskUpdate::Applied
    );
    let approved = store.tool_tasks(&run).await.expect("tool_tasks").remove(0);
    assert_eq!(
        approved.state,
        TaskState::Ready {
            next_attempt_at: approved_at
        }
    );
    assert!(
        !approved.schema_change_approved,
        "an approval is not a schema change"
    );
    let rejection = ToolFailure {
        kind: ToolFailureKind::Error,
        message: "the harness refused the call".into(),
        data: None,
    };
    assert_eq!(
        store
            .reject_tool_task(&task(2), rejection.clone(), approved_at)
            .await
            .expect("reject"),
        TaskUpdate::Applied
    );
    assert_eq!(
        inbox(store, &run, approved_at).await,
        [Trigger::Completion(
            ToolOutcome::Failure(rejection.clone()).into_completion(Seq(2))
        )]
    );

    // A schema change parks a claimed task; approved, it goes on against the new schema.
    let claimed = claim(store, "a", &TaskFilter::All, approved_at, 10)
        .await
        .remove(0);
    assert_eq!(claimed.task.task.seq, Seq(1));
    let reason = ParkReason {
        kind: ParkKind::SchemaChanged,
        message: "`repo` became `repository`".into(),
    };
    assert_eq!(
        store
            .park_tool_task(&claimed.token, reason.clone())
            .await
            .expect("park"),
        TaskUpdate::Applied
    );
    assert_eq!(
        store.tool_tasks(&run).await.expect("tool_tasks")[0].state,
        TaskState::Parked(reason)
    );
    assert!(
        claim(store, "a", &TaskFilter::All, approved_at, 10)
            .await
            .is_empty(),
        "parked tasks wait"
    );
    assert_eq!(
        store
            .complete_tool_task(&claimed.token, result(1), approved_at)
            .await
            .expect("complete"),
        TaskUpdate::NotFound,
        "parking spends the token"
    );
    assert_eq!(
        store
            .approve_tool_task(&task(1), approved_at)
            .await
            .expect("approve"),
        TaskUpdate::Applied
    );
    let again = claim(store, "a", &TaskFilter::All, approved_at, 10)
        .await
        .remove(0);
    assert_eq!(again.task.task.seq, Seq(1));
    assert!(
        again.task.schema_change_approved,
        "approved after a schema change, the call goes on against the new schema"
    );
    assert_eq!(
        store
            .approve_tool_task(&task(1), approved_at)
            .await
            .expect("approve"),
        TaskUpdate::InvalidState(again.task.state.clone()),
        "only approval and parked tasks are approved"
    );
    assert_eq!(
        store
            .reject_tool_task(&task(1), rejection, approved_at)
            .await
            .expect("reject"),
        TaskUpdate::InvalidState(again.task.state)
    );
    assert_eq!(
        store
            .approve_tool_task(&task(9), approved_at)
            .await
            .expect("approve"),
        TaskUpdate::NotFound
    );
}

pub async fn a_token_names_its_task<S: JournalStore>(store: &S) {
    let run = start_with_tasks(
        store,
        run_ref(1),
        vec![
            host_task(1, "t"),
            host_task(2, "t"),
            host_task(3, "t"),
            host_task(4, "t"),
        ],
    )
    .await;
    let claimed = claim(store, "a", &TaskFilter::All, T0, 10).await;
    assert_eq!(seqs(&claimed), [1, 2, 3, 4]);
    let task_of = |token| async move { store.tool_task(token).await.expect("tool_task") };

    // A live claim names its task, as `tool_tasks` shows it.
    let found = task_of(&claimed[0].token).await.expect("the claim's task");
    assert_eq!(found, store.tool_tasks(&run).await.expect("tool_tasks")[0]);
    assert_eq!(found.task, claimed[0].task.task);
    assert_eq!(found.attempt, 1);
    assert!(matches!(found.state, TaskState::Claimed { .. }));

    // A deferred task keeps its token.
    let until = later(T0, DAY);
    store
        .defer_tool_task(&claimed[0].token, until)
        .await
        .expect("defer");
    assert_eq!(
        task_of(&claimed[0].token).await.map(|task| task.state),
        Some(TaskState::Deferred { until })
    );

    // Retried, parked, completed: the token holds nothing any more.
    store
        .retry_tool_task(&claimed[1].token, later(T0, 1_000))
        .await
        .expect("retry");
    assert_eq!(task_of(&claimed[1].token).await, None);
    let reason = ParkReason {
        kind: ParkKind::CredentialsExpired,
        message: "401".into(),
    };
    store
        .park_tool_task(&claimed[2].token, reason)
        .await
        .expect("park");
    assert_eq!(task_of(&claimed[2].token).await, None);
    store
        .complete_tool_task(&claimed[3].token, result(4), T0)
        .await
        .expect("complete");
    assert_eq!(task_of(&claimed[3].token).await, None);

    // Claimed again after its claim expired: the old token is dead, the new one names it.
    let again = claim(store, "b", &TaskFilter::All, later(T0, 1_000), 10).await;
    assert_eq!(seqs(&again), [2]);
    assert_eq!(task_of(&claimed[1].token).await, None);
    assert_eq!(
        task_of(&again[0].token).await.map(|task| task.attempt),
        Some(2)
    );
    let expired = later(T0, 1_000).saturating_add(again[0].task.policy.heartbeat_timeout);
    let third = claim(store, "c", &TaskFilter::All, expired, 10).await;
    assert_eq!(seqs(&third), [2]);
    assert_eq!(task_of(&again[0].token).await, None);
    assert_eq!(task_of(&TaskToken("rewind_tt_unknown".into())).await, None);
}

pub async fn renewed_credentials_resume_parked_tasks<S: JournalStore>(store: &S) {
    let run = start_with_tasks(
        store,
        run_ref(1),
        vec![host_task(1, "t"), host_task(2, "u")],
    )
    .await;
    let other = start_with_tasks(store, run_ref(2), vec![host_task(1, "t")]).await;
    let claimed = claim(store, "a", &TaskFilter::All, T0, 10).await;
    assert_eq!(claimed.len(), 3);
    let expired = ParkReason {
        kind: ParkKind::CredentialsExpired,
        message: "401".into(),
    };
    let changed = ParkReason {
        kind: ParkKind::SchemaChanged,
        message: "renamed".into(),
    };
    for (task, reason) in claimed.iter().zip([&expired, &changed, &expired]) {
        store
            .park_tool_task(&task.token, reason.clone())
            .await
            .expect("park");
    }
    let at = later(T0, 10);
    assert_eq!(
        store
            .resume_parked(&ParkedScope::Run(run.clone()), at)
            .await
            .expect("resume"),
        1
    );
    let states: Vec<TaskState> = store
        .tool_tasks(&run)
        .await
        .expect("tool_tasks")
        .into_iter()
        .map(|t| t.state)
        .collect();
    assert_eq!(
        states,
        [
            TaskState::Ready {
                next_attempt_at: at
            },
            TaskState::Parked(changed)
        ],
        "a schema change is not resumed by credentials"
    );
    let scope = ParkedScope::Toolset {
        tenant: acme(),
        toolset: toolset_digest(),
    };
    assert_eq!(
        store.resume_parked(&scope, at).await.expect("resume"),
        1,
        "the other run, by its toolset"
    );
    assert_eq!(
        store.tool_tasks(&other).await.expect("tool_tasks")[0].state,
        TaskState::Ready {
            next_attempt_at: at
        }
    );
    let foreign = ParkedScope::Toolset {
        tenant: tenant("other"),
        toolset: toolset_digest(),
    };
    assert_eq!(store.resume_parked(&foreign, at).await.expect("resume"), 0);
}

pub async fn claims_respect_concurrency_limits<S: JournalStore>(store: &S) {
    let limited = |seq| {
        let mut task = host_task(seq, "ask_user");
        task.policy.concurrency = 2;
        task
    };
    start_with_tasks(
        store,
        run_ref(1),
        vec![limited(1), limited(2), limited(3), host_task(4, "other")],
    )
    .await;
    let first = claim(store, "a", &TaskFilter::All, T0, 10).await;
    assert_eq!(
        seqs(&first),
        [1, 2, 4],
        "two calls of ask_user in flight at most"
    );
    assert!(claim(store, "a", &TaskFilter::All, T0, 10).await.is_empty());
    store
        .defer_tool_task(&first[0].token, later(T0, DAY))
        .await
        .expect("defer");
    assert!(
        claim(store, "a", &TaskFilter::All, T0, 10).await.is_empty(),
        "a deferred call is in flight"
    );
    store
        .complete_tool_task(&first[1].token, result(2), T0)
        .await
        .expect("complete");
    assert_eq!(
        seqs(&claim(store, "a", &TaskFilter::All, T0, 10).await),
        [3],
        "a slot is free"
    );

    // Another run has its own slots.
    start_with_tasks(store, run_ref(2), vec![limited(1)]).await;
    assert_eq!(claim(store, "a", &TaskFilter::All, T0, 10).await.len(), 1);

    // Calls to one MCP server are limited per worker.
    let mcp = start_with_tasks(
        store,
        run_ref(3),
        vec![
            mcp_task(1, "github.a", "github", 1),
            mcp_task(2, "github.b", "github", 1),
            mcp_task(3, "slack.c", "slack", 1),
        ],
    )
    .await;
    let a = claim(store, "a", &TaskFilter::All, T0, 10).await;
    assert_eq!(
        seqs(&a),
        [1, 3],
        "one call to github in flight under worker a"
    );
    let b = claim(store, "b", &TaskFilter::All, T0, 10).await;
    assert_eq!(seqs(&b), [2], "worker b has its own slot");
    assert_eq!(b[0].task.task.run, mcp);

    // An expired claim no longer takes a slot: a new call to github goes to worker a.
    let fresh = start_with_tasks(
        store,
        run_ref(4),
        vec![mcp_task(1, "github.a", "github", 1)],
    )
    .await;
    let expired = T0.saturating_add(a[0].task.policy.heartbeat_timeout);
    let again = claim(store, "a", &TaskFilter::All, expired, 100).await;
    let github: Vec<&RunRef> = again
        .iter()
        .filter(|task| {
            task.task.route
                == ToolRoute::Mcp {
                    server: "github".into(),
                }
        })
        .map(|task| &task.task.task.run)
        .collect();
    assert_eq!(github, [&fresh], "the call due first takes the one slot");
}

pub async fn claims_filter_by_route_tenant_toolset_and_tool<S: JournalStore>(store: &S) {
    let other = tenant("other");
    let webhook = {
        let route = ToolRoute::Builtin(rewind_core::Builtin::Webhook);
        NewToolTask {
            seq: Seq(3),
            tool: "webhook".into(),
            args: json!({}).into(),
            policy: ToolPolicy::default_for(&route),
            route,
            server_concurrency: None,
        }
    };
    start_with_tasks(
        store,
        run_ref(1),
        vec![
            host_task(1, "ask_user"),
            mcp_task(2, "github.a", "github", 8),
            webhook,
            host_task(4, "notify"),
        ],
    )
    .await;
    start_with_tasks(
        store,
        run_in(&other, 2),
        vec![
            host_task(1, "ask_user"),
            mcp_task(2, "github.a", "github", 8),
        ],
    )
    .await;
    let summary = |claimed: Vec<crate::ports::ClaimedTask>| -> Vec<(String, u32)> {
        claimed
            .iter()
            .map(|task| (task.task.task.run.tenant.to_string(), task.task.task.seq.0))
            .collect()
    };

    let only_acme = TaskFilter::Dispatched {
        tenants: TenantScope::Only(BTreeSet::from([acme()])),
    };
    assert_eq!(
        summary(claim(store, "d", &only_acme, T0, 10).await),
        [("acme".to_owned(), 2), ("acme".to_owned(), 3)],
        "MCP tools and built-ins, never host tools"
    );
    let host = TaskFilter::Host {
        tenant: acme(),
        namespace: None,
        toolset: Some(Digest::of(b"another")),
        tools: BTreeSet::new(),
    };
    assert!(
        claim(store, "h", &host, T0, 10).await.is_empty(),
        "another toolset"
    );
    let host = TaskFilter::Host {
        tenant: acme(),
        namespace: None,
        toolset: Some(toolset_digest()),
        tools: BTreeSet::from(["ask_user".to_owned()]),
    };
    assert_eq!(
        summary(claim(store, "h", &host, T0, 10).await),
        [("acme".to_owned(), 1)]
    );
    let host = TaskFilter::Host {
        tenant: acme(),
        namespace: None,
        toolset: None,
        tools: BTreeSet::new(),
    };
    assert_eq!(
        summary(claim(store, "h", &host, T0, 10).await),
        [("acme".to_owned(), 4)]
    );
    let everything = claim(store, "x", &TaskFilter::All, T0, 10).await;
    assert_eq!(
        summary(everything),
        [("other".to_owned(), 1), ("other".to_owned(), 2)]
    );
}

// -------------------------------------------------------------------------------------------
// Webhooks

pub async fn webhook_tokens_resolve_to_their_run_until_it_ends<S: JournalStore>(store: &S) {
    let run = start(store, run_ref(1)).await;
    let merged = store
        .create_webhook_token(&run, "pr_merged")
        .await
        .expect("create_webhook_token");
    assert_eq!(
        store
            .create_webhook_token(&run, "pr_merged")
            .await
            .expect("create_webhook_token"),
        merged,
        "one token per run and signal name"
    );
    let closed = store
        .create_webhook_token(&run, "pr_closed")
        .await
        .expect("create_webhook_token");
    assert_ne!(closed, merged);
    assert!(merged.0.len() >= 32, "unguessable");
    let target = store
        .resolve_webhook_token(&merged)
        .await
        .expect("resolve")
        .expect("bound");
    assert_eq!(
        (target.run, target.name.as_str()),
        (run.clone(), "pr_merged")
    );
    assert_eq!(
        store
            .resolve_webhook_token(&WebhookToken("rewind_wh_guess".into()))
            .await
            .expect("resolve"),
        None
    );

    let lease = lease_of(store, &run, T0).await;
    commit(
        store,
        &lease,
        Activation::new(vec![aborted_entry(1)], vec![], RunStatus::Aborted),
        T0,
    )
    .await;
    assert_eq!(
        store.resolve_webhook_token(&closed).await.expect("resolve"),
        None,
        "the run has ended"
    );
    assert!(matches!(
        store.create_webhook_token(&run, "late").await,
        Err(StoreError::Conflict(_))
    ));
}

pub async fn subscriptions_are_stored_per_tenant<S: JournalStore>(store: &S) {
    let subscription = |tenant: TenantId, id: &str, url: &str| Subscription {
        tenant,
        id: id.into(),
        url: url.into(),
        secret: Secret::new("whsec_1"),
        events: BTreeSet::from([WebhookEventType::RunCompleted]),
        labels: BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
    };
    store
        .put_subscription(&subscription(acme(), "s2", "https://b"))
        .await
        .expect("put");
    store
        .put_subscription(&subscription(acme(), "s1", "https://a"))
        .await
        .expect("put");
    store
        .put_subscription(&subscription(tenant("other"), "s1", "https://c"))
        .await
        .expect("put");
    store
        .put_subscription(&subscription(acme(), "s2", "https://b2"))
        .await
        .expect("put replaces");
    let mine = store.subscriptions(&acme()).await.expect("subscriptions");
    assert_eq!(
        mine,
        [
            subscription(acme(), "s1", "https://a"),
            subscription(acme(), "s2", "https://b2")
        ]
    );
    assert!(
        store
            .delete_subscription(&acme(), "s1")
            .await
            .expect("delete")
    );
    assert!(
        !store
            .delete_subscription(&acme(), "s1")
            .await
            .expect("delete")
    );
    assert_eq!(
        store
            .subscriptions(&acme())
            .await
            .expect("subscriptions")
            .len(),
        1
    );
    assert_eq!(
        store
            .subscriptions(&tenant("other"))
            .await
            .expect("subscriptions")
            .len(),
        1
    );
}

// -------------------------------------------------------------------------------------------
// Payloads and retention

fn payload(digest: Digest, value: Option<serde_json::Value>) -> PayloadRecord {
    PayloadRecord {
        digest,
        size: 70_000,
        preview: "[1,2,3,…".into(),
        value,
    }
}

/// Commits one log entry referencing these payloads, those of the blob tier reserved first, as
/// the engine reserves them before writing their blobs.
async fn reference<S: JournalStore>(
    store: &S,
    run: &RunRef,
    payloads: Vec<PayloadRecord>,
    at: Timestamp,
) {
    let large: Vec<PayloadRecord> = payloads
        .iter()
        .filter(|payload| payload.value.is_none())
        .cloned()
        .collect();
    let deleting = store
        .reserve_payloads(&run.tenant, &large, at)
        .await
        .expect("reserve_payloads");
    assert_eq!(deleting, [], "no deletion started");
    let result = commit_payloads(store, run, payloads, at).await;
    result.expect("commit_activation");
}

/// Commits one log entry referencing these payloads, as they are.
async fn commit_payloads<S: JournalStore>(
    store: &S,
    run: &RunRef,
    payloads: Vec<PayloadRecord>,
    at: Timestamp,
) -> Result<(), StoreError> {
    let lease = lease_of(store, run, at).await;
    let next = record(store, run).await.history_len;
    let mut activation = Activation::new(vec![log_entry(next)], vec![], RunStatus::Active);
    activation.payloads = payloads;
    let committed = store.commit_activation(&lease, activation, at).await;
    store.release(&lease).await.expect("release");
    committed
}

pub async fn payloads_are_recorded_and_referenced_at_commit<S: JournalStore>(store: &S) {
    let (blob, table) = (Digest::of(b"blob"), Digest::of(b"table"));
    let run = start(store, run_ref(1)).await;
    // A payload of the blob tier is committed only once its row is reserved: its blob may be
    // gone otherwise.
    let unreserved = commit_payloads(store, &run, vec![payload(blob, None)], T0).await;
    assert!(
        matches!(unreserved, Err(StoreError::Conflict(_))),
        "{unreserved:?}"
    );
    assert_eq!(record(store, &run).await.history_len, 1, "nothing written");
    assert_eq!(
        store.get_payload(&acme(), blob).await.expect("get_payload"),
        None
    );
    reference(
        store,
        &run,
        vec![payload(blob, None), payload(table, Some(json!([1, 2, 3])))],
        T0,
    )
    .await;
    assert_eq!(
        store.get_payload(&acme(), blob).await.expect("get_payload"),
        Some(payload(blob, None))
    );
    assert_eq!(
        store
            .get_payload(&acme(), table)
            .await
            .expect("get_payload"),
        Some(payload(table, Some(json!([1, 2, 3]))))
    );
    assert_eq!(
        store
            .get_payload(&tenant("other"), table)
            .await
            .expect("get_payload"),
        None
    );
    assert_eq!(
        store
            .get_payload(&acme(), Digest::of(b"none"))
            .await
            .expect("get_payload"),
        None
    );

    // Payloads are deduplicated within a tenant: a second run references the same row.
    let second = start(store, run_ref(2)).await;
    reference(
        store,
        &second,
        vec![payload(table, Some(json!([1, 2, 3])))],
        T0,
    )
    .await;
    let grace = Retention::default().payload_grace;
    store.delete_run(&run, T0).await.expect("delete_run");
    assert!(
        store
            .unreferenced_payloads(T0.saturating_add(grace), 10)
            .await
            .expect("unreferenced")
            .contains(&(acme(), blob)),
        "only the deleted run referenced the blob"
    );
    assert!(
        !store
            .unreferenced_payloads(T0.saturating_add(grace), 10)
            .await
            .expect("unreferenced")
            .contains(&(acme(), table)),
        "the second run still references it"
    );
}

pub async fn unreferenced_payloads_are_forgotten_after_their_grace<S: JournalStore>(store: &S) {
    let (a, b) = (Digest::of(b"a"), Digest::of(b"b"));
    let mut quick = Tenant::new(tenant("quick"));
    quick.retention.payload_grace = Duration::from_secs(60);
    store.put_tenant(&quick).await.expect("put_tenant");
    let run = start(store, run_ref(1)).await;
    let fast = start(store, run_in(&tenant("quick"), 2)).await;
    reference(store, &run, vec![payload(a, None), payload(b, None)], T0).await;
    reference(store, &fast, vec![payload(a, None)], T0).await;

    let deleted_at = later(T0, 1_000);
    assert!(
        store
            .delete_run(&run, deleted_at)
            .await
            .expect("delete_run")
    );
    assert!(
        store
            .delete_run(&fast, deleted_at)
            .await
            .expect("delete_run")
    );
    let grace = Retention::default().payload_grace;
    let unreferenced = |at: Timestamp| async move {
        store
            .unreferenced_payloads(at, 10)
            .await
            .expect("unreferenced")
    };
    assert_eq!(unreferenced(later(deleted_at, 59_999)).await, []);
    assert_eq!(
        unreferenced(later(deleted_at, 60_000)).await,
        [(tenant("quick"), a)],
        "each tenant's grace applies"
    );
    let due = deleted_at.saturating_add(grace);
    let all = unreferenced(due).await;
    assert_eq!(all.len(), 3);
    assert_eq!(
        store
            .unreferenced_payloads(due, 2)
            .await
            .expect("unreferenced")
            .len(),
        2,
        "up to the limit"
    );

    // `b` is referenced again before it is forgotten: it is kept.
    let again = start(store, run_ref(3)).await;
    reference(store, &again, vec![payload(b, None)], due).await;
    let mut forgotten = store
        .forget_payloads(&all, due)
        .await
        .expect("forget_payloads");
    forgotten.sort();
    let mut expected = vec![(acme(), a), (tenant("quick"), a)];
    expected.sort();
    assert_eq!(forgotten, expected);
    assert_eq!(
        store
            .forget_payloads(&all, due)
            .await
            .expect("forget_payloads"),
        [],
        "a deletion starts once"
    );
    assert_eq!(unreferenced(due).await, [], "deletions under way");

    // Their blobs are deleted, then their rows.
    assert!(
        store
            .get_payload(&acme(), a)
            .await
            .expect("get_payload")
            .is_some(),
        "until it is dropped"
    );
    assert_eq!(
        store
            .drop_payloads(&forgotten, later(due, 1))
            .await
            .expect("drop_payloads"),
        0,
        "only the deletion started at that time"
    );
    assert_eq!(
        store
            .drop_payloads(&forgotten, due)
            .await
            .expect("drop_payloads"),
        2
    );
    assert_eq!(
        store.get_payload(&acme(), a).await.expect("get_payload"),
        None
    );
    assert!(
        store
            .get_payload(&acme(), b)
            .await
            .expect("get_payload")
            .is_some()
    );
    assert_eq!(unreferenced(due).await, []);
}

pub async fn a_payload_being_deleted_is_never_committed_without_its_value<S: JournalStore>(
    store: &S,
) {
    let blob = Digest::of(b"blob");
    let grace = Retention::default().payload_grace;
    let unreferenced = |at: Timestamp| async move {
        store
            .unreferenced_payloads(at, 10)
            .await
            .expect("unreferenced")
    };
    let reserve = |at: Timestamp| async move {
        store
            .reserve_payloads(&acme(), &[payload(blob, None)], at)
            .await
            .expect("reserve_payloads")
    };
    let forget = |at: Timestamp| async move {
        store
            .forget_payloads(&[(acme(), blob)], at)
            .await
            .expect("forget_payloads")
    };
    let drop = |started: Timestamp| async move {
        store
            .drop_payloads(&[(acme(), blob)], started)
            .await
            .expect("drop_payloads")
    };
    let first = start(store, run_ref(1)).await;
    reference(store, &first, vec![payload(blob, None)], T0).await;
    store.delete_run(&first, T0).await.expect("delete_run");
    let due = T0.saturating_add(grace);
    assert_eq!(unreferenced(due).await, [(acme(), blob)]);

    // Reserved before the cleanup starts: its grace starts again, so the cleanup keeps it and
    // the commit finds it.
    let writer = start(store, run_ref(2)).await;
    let reserved_at = later(due, 1);
    assert_eq!(reserve(reserved_at).await, []);
    assert_eq!(unreferenced(reserved_at).await, []);
    assert_eq!(forget(reserved_at).await, [], "reserved: kept");
    commit_payloads(store, &writer, vec![payload(blob, None)], reserved_at)
        .await
        .expect("commit_activation");
    store
        .delete_run(&writer, reserved_at)
        .await
        .expect("delete_run");

    // The cleanup started before the reservation: the payload is reported as being deleted and
    // left as it is. Its value is committed in its row, which stops the deletion.
    let due = reserved_at.saturating_add(grace);
    assert_eq!(forget(due).await, [(acme(), blob)]);
    let writer = start(store, run_ref(3)).await;
    assert_eq!(reserve(due).await, [blob]);
    let refused = commit_payloads(store, &writer, vec![payload(blob, None)], due).await;
    assert!(
        matches!(refused, Err(StoreError::Conflict(_))),
        "its blob may be gone: {refused:?}"
    );
    let value = json!({ "large": true });
    commit_payloads(
        store,
        &writer,
        vec![payload(blob, Some(value.clone()))],
        due,
    )
    .await
    .expect("commit_activation");
    assert_eq!(drop(due).await, 0, "taken back by the commit");
    assert_eq!(
        store
            .get_payload(&acme(), blob)
            .await
            .expect("get_payload")
            .and_then(|record| record.value),
        Some(value),
        "the row holds the value"
    );
    store.delete_run(&writer, due).await.expect("delete_run");

    // Reserved, then the writer stalls past the grace: the cleanup deletes the payload, and the
    // commit that relied on the blob is refused, the row being deleted or gone.
    let writer = start(store, run_ref(4)).await;
    let reserved_at = later(due, 1);
    assert_eq!(reserve(reserved_at).await, []);
    let stalled = reserved_at.saturating_add(grace);
    assert_eq!(forget(stalled).await, [(acme(), blob)]);
    let refused = commit_payloads(store, &writer, vec![payload(blob, None)], stalled).await;
    assert!(
        matches!(refused, Err(StoreError::Conflict(_))),
        "{refused:?}"
    );
    assert_eq!(drop(stalled).await, 1);
    let refused = commit_payloads(store, &writer, vec![payload(blob, None)], stalled).await;
    assert!(
        matches!(refused, Err(StoreError::Conflict(_))),
        "no row: {refused:?}"
    );
    assert_eq!(
        record(store, &writer).await.history_len,
        1,
        "nothing written"
    );

    // A deletion that stopped halfway is started again after the timeout, by another cleanup
    // that then owns the row.
    reference(store, &writer, vec![payload(blob, None)], stalled).await;
    store
        .delete_run(&writer, stalled)
        .await
        .expect("delete_run");
    let due = stalled.saturating_add(grace);
    assert_eq!(forget(due).await, [(acme(), blob)]);
    let timeout = due.saturating_add(PAYLOAD_DELETION_TIMEOUT);
    assert_eq!(unreferenced(Timestamp(timeout.0 - 1)).await, []);
    assert_eq!(forget(Timestamp(timeout.0 - 1)).await, []);
    assert_eq!(unreferenced(timeout).await, [(acme(), blob)]);
    assert_eq!(forget(timeout).await, [(acme(), blob)]);
    assert_eq!(
        drop(due).await,
        0,
        "the first deletion no longer owns the row"
    );
    assert_eq!(drop(timeout).await, 1);
    assert_eq!(
        store.get_payload(&acme(), blob).await.expect("get_payload"),
        None
    );
}

/// Applies every feed item: claims and acknowledges each consumer's items.
async fn project_all<S: JournalStore>(store: &S, at: Timestamp) {
    for consumer in FeedConsumer::ALL {
        let ids: Vec<u64> = store
            .claim_feed(consumer, "projector", at, LEASE, 1_000)
            .await
            .expect("claim_feed")
            .iter()
            .map(|item| item.id)
            .collect();
        store.ack_feed(&ids).await.expect("ack_feed");
    }
}

/// Values are JSON in the stored format, where U+0000 is escaped: every store keeps them as they
/// are, Postgres included. Text outside JSON never holds one: the engine refuses it.
pub async fn json_values_keep_nul_characters<S: JournalStore>(store: &S) {
    let value = json!({ "text": "a\u{0}b", "key\u{0}": ["\u{0}"] });
    let signal = Completion::SignalReceived {
        name: "go".into(),
        payload: value.clone().into(),
    };
    let run = start(store, run_ref(1)).await;
    store
        .enqueue(&run, signal.clone(), None, T0)
        .await
        .expect("enqueue");
    assert_eq!(
        inbox(store, &run, T0).await,
        [Trigger::Completion(signal.clone())]
    );

    let lease = lease_of(store, &run, T0).await;
    let consumed = inbox_ids(store, &lease).await;
    let delivered = entry(1, Event::Completion(signal));
    let mut activation = Activation::new(vec![delivered.clone()], consumed, RunStatus::Active);
    let digest = Digest::of(b"a value with a NUL character");
    activation.payloads = vec![payload(digest, Some(value.clone()))];
    let mut task = host_task(2, "t");
    task.args = value.clone().into();
    activation.tool_calls = vec![task];
    commit(store, &lease, activation, T0).await;
    store.release(&lease).await.expect("release");

    assert_eq!(
        store.load_history(&run, 1).await.expect("load_history"),
        [delivered]
    );
    assert_eq!(
        store
            .get_payload(&acme(), digest)
            .await
            .expect("get_payload")
            .and_then(|record| record.value),
        Some(value.clone())
    );
    let tasks = store.tool_tasks(&run).await.expect("tool_tasks");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].args, value.into());
}

pub async fn finished_runs_are_archived_then_deleted<S: JournalStore>(store: &S) {
    let mut short = Tenant::new(tenant("short"));
    short.retention.journal = Duration::from_secs(3_600);
    short.retention.archive = Duration::from_secs(7_200);
    store.put_tenant(&short).await.expect("put_tenant");
    let run = start(store, run_ref(1)).await;
    let brief = start(store, run_in(&tenant("short"), 2)).await;
    start(store, run_ref(3)).await;
    for ended in [&run, &brief] {
        let lease = lease_of(store, ended, T0).await;
        commit(
            store,
            &lease,
            Activation::new(vec![aborted_entry(1)], vec![], RunStatus::Aborted),
            T0,
        )
        .await;
    }
    let ended_at = Timestamp(T0.0 + 1);
    let retention = Retention::default();
    project_all(store, ended_at).await;

    assert_eq!(
        store
            .runs_to_archive(Timestamp(ended_at.0 + 3_599_999), 10)
            .await
            .expect("to archive"),
        []
    );
    assert_eq!(
        store
            .runs_to_archive(ended_at.saturating_add(Duration::from_secs(3_600)), 10)
            .await
            .expect("to archive"),
        std::slice::from_ref(&brief)
    );
    let due = ended_at.saturating_add(retention.journal);
    assert_eq!(
        store.runs_to_archive(due, 10).await.expect("to archive"),
        [run.clone(), brief.clone()],
        "the active run is never archived"
    );
    assert_eq!(
        store
            .runs_to_archive(due, 1)
            .await
            .expect("to archive")
            .len(),
        1
    );

    let document = Digest::of(b"journal document");
    assert!(
        !store
            .archive_run(&run_ref(3), document, due)
            .await
            .expect("archive_run"),
        "an active run"
    );
    assert!(
        store
            .archive_run(&run, document, due)
            .await
            .expect("archive_run")
    );
    assert!(
        !store
            .archive_run(&run, document, due)
            .await
            .expect("archive_run"),
        "once"
    );
    assert!(matches!(
        store.load_history(&run, 0).await,
        Err(StoreError::Archived(_))
    ));
    let record = record(store, &run).await;
    assert_eq!(
        record
            .archive
            .map(|archive| (archive.document, archive.archived_at)),
        Some((document, due))
    );
    assert_eq!(record.status, RunStatus::Aborted, "the record stays");
    assert_eq!(
        store.runs_to_archive(due, 10).await.expect("to archive"),
        std::slice::from_ref(&brief)
    );
    assert!(
        store
            .archive_run(&brief, document, due)
            .await
            .expect("archive_run")
    );

    assert_eq!(store.runs_to_delete(due, 10).await.expect("to delete"), []);
    let brief_due = due.saturating_add(Duration::from_secs(7_200));
    assert_eq!(
        store
            .runs_to_delete(brief_due, 10)
            .await
            .expect("to delete"),
        [brief]
    );
    assert_eq!(
        store
            .runs_to_delete(due.saturating_add(retention.archive), 10)
            .await
            .expect("to delete")
            .len(),
        2
    );
}

pub async fn runs_with_feed_items_left_are_not_archived<S: JournalStore>(store: &S) {
    let run = start(store, run_ref(1)).await;
    let lease = lease_of(store, &run, T0).await;
    commit(
        store,
        &lease,
        Activation::new(vec![aborted_entry(1)], vec![], RunStatus::Aborted),
        T0,
    )
    .await;
    let due = Timestamp(T0.0 + 1).saturating_add(Retention::default().journal);
    let document = Digest::of(b"journal document");
    let archivable = || async {
        store
            .runs_to_archive(due, 10)
            .await
            .expect("to archive")
            .contains(&run)
    };

    // The projector has not applied the run's last entries: its history stays in the store.
    assert!(!archivable().await);
    assert!(
        !store
            .archive_run(&run, document, due)
            .await
            .expect("archive_run")
    );
    // The read model applied them, the other consumers not yet.
    let applied: Vec<u64> = store
        .claim_feed(FeedConsumer::ReadModel, "projector", due, LEASE, 10)
        .await
        .expect("claim_feed")
        .iter()
        .map(|item| item.id)
        .collect();
    assert_eq!(applied.len(), 2);
    store.ack_feed(&applied).await.expect("ack_feed");
    assert!(!archivable().await);
    assert!(
        !store
            .archive_run(&run, document, due)
            .await
            .expect("archive_run")
    );

    project_all(store, due).await;
    assert!(archivable().await);
    assert!(
        store
            .archive_run(&run, document, due)
            .await
            .expect("archive_run")
    );
}

pub async fn deleting_a_run_erases_it<S: JournalStore>(store: &S) {
    store
        .create_run(new_run(run_ref(1)), Some("key".into()))
        .await
        .expect("create_run");
    let lease = lease_of(store, &run_ref(1), T0).await;
    let consumed = inbox_ids(store, &lease).await;
    let mut activation = Activation::new(vec![run_started_entry()], consumed, RunStatus::Active);
    activation.tool_calls.push(host_task(1, "t"));
    activation.timers.push(NewTimer {
        seq: Seq(2),
        fire_at: later(T0, 10),
    });
    commit(store, &lease, activation, T0).await;
    let run = run_ref(1);
    store
        .create_webhook_token(&run, "hook")
        .await
        .expect("create_webhook_token");

    assert!(store.delete_run(&run, T0).await.expect("delete_run"));
    assert!(!store.delete_run(&run, T0).await.expect("delete_run"));
    assert_eq!(store.get_run(&run).await.expect("get_run"), None);
    assert!(matches!(
        store.load_history(&run, 0).await,
        Err(StoreError::RunNotFound(_))
    ));
    assert_eq!(store.next_timer().await.expect("next_timer"), None);
    assert!(claim(store, "w", &TaskFilter::All, T0, 10).await.is_empty());
    for consumer in FeedConsumer::ALL {
        assert!(
            store
                .claim_feed(consumer, "p", T0, LEASE, 10)
                .await
                .expect("claim_feed")
                .is_empty()
        );
    }
    let reused = store
        .create_run(new_run(run_ref(2)), Some("key".into()))
        .await
        .expect("create_run");
    assert!(
        matches!(reused, CreateRun::Created(ref c) if c.created),
        "its idempotency key is gone"
    );
}

pub async fn tenants_are_stored<S: JournalStore>(store: &S) {
    assert_eq!(store.get_tenant(&acme()).await.expect("get_tenant"), None);
    let mut tenant_record = Tenant::new(acme());
    tenant_record.quotas = Quotas {
        cpu_per_month: Some(Duration::from_secs(3_600)),
        active_runs: Some(100),
        runs_per_month: Some(10_000),
        scripts: Some(500),
        max_ttl: Some(Duration::from_secs(7 * 86_400)),
        max_script_lines: Some(2_000),
    };
    tenant_record.namespace_defaults = Quotas {
        active_runs: Some(10),
        max_script_lines: Some(300),
        ..Quotas::default()
    };
    tenant_record.retention.read_model = Duration::from_secs(86_400);
    store.put_tenant(&tenant_record).await.expect("put_tenant");
    store
        .put_tenant(&Tenant::new(tenant("beta")))
        .await
        .expect("put_tenant");
    assert_eq!(
        store.get_tenant(&acme()).await.expect("get_tenant"),
        Some(tenant_record.clone())
    );
    tenant_record.quotas = Quotas::default();
    store
        .put_tenant(&tenant_record)
        .await
        .expect("put replaces");
    let ids: Vec<String> = store
        .list_tenants()
        .await
        .expect("list_tenants")
        .iter()
        .map(|t| t.id.to_string())
        .collect();
    assert_eq!(ids, ["acme", "beta"]);
}

pub async fn the_feed_is_claimed_and_acknowledged_per_consumer<S: JournalStore>(store: &S) {
    let run = start(store, run_ref(1)).await;
    let lease = lease_of(store, &run, T0).await;
    commit(
        store,
        &lease,
        Activation::new(vec![log_entry(1), log_entry(2)], vec![], RunStatus::Active),
        T0,
    )
    .await;
    commit(
        store,
        &lease,
        Activation::new(vec![], vec![], RunStatus::Active),
        T0,
    )
    .await;

    for consumer in FeedConsumer::ALL {
        let items = store
            .claim_feed(consumer, "p1", T0, LEASE, 10)
            .await
            .expect("claim_feed");
        assert_eq!(
            items.len(),
            2,
            "{consumer}: one item per commit with entries"
        );
        assert!(
            items
                .iter()
                .all(|item| item.consumer == consumer && item.run == run)
        );
        assert_eq!((items[0].from_idx, items[0].to_idx), (0, 1));
        assert_eq!((items[1].from_idx, items[1].to_idx), (1, 3));
        assert_eq!(items[1].fence, lease.fence);
    }
    assert!(
        store
            .claim_feed(FeedConsumer::Otel, "p2", T0, LEASE, 10)
            .await
            .expect("claim_feed")
            .is_empty(),
        "claimed items are not handed out again while claimed"
    );
    let expired = T0.saturating_add(LEASE);
    let again = store
        .claim_feed(FeedConsumer::Otel, "p2", expired, LEASE, 1)
        .await
        .expect("claim_feed");
    assert_eq!(again.len(), 1, "up to the limit, once the claim expired");
    store
        .ack_feed(&[again[0].id, 999_999])
        .await
        .expect("ack_feed");
    let rest = store
        .claim_feed(
            FeedConsumer::Otel,
            "p3",
            later(expired, 1).saturating_add(LEASE),
            LEASE,
            10,
        )
        .await
        .expect("claim_feed");
    assert_eq!(rest.len(), 1);
    assert_ne!(rest[0].id, again[0].id, "acknowledged items are gone");
    let read_model = store
        .claim_feed(FeedConsumer::ReadModel, "p", expired, LEASE, 10)
        .await
        .expect("claim_feed");
    assert_eq!(read_model.len(), 2, "each consumer has its own items");
}

pub async fn list_runs_filters_and_pages<S: JournalStore>(store: &S) {
    let mut labelled = new_run(run_ref(1));
    labelled.metadata.labels = BTreeMap::from([
        ("agent".to_owned(), "triage".to_owned()),
        ("conversation".to_owned(), "c1".to_owned()),
    ]);
    store.create_run(labelled, None).await.expect("create_run");
    start(store, run_ref(2)).await;
    let mut late = new_run(run_ref(3));
    late.created_at = later(T0, 10);
    store.create_run(late, None).await.expect("create_run");
    let lease = lease_of(store, &run_ref(3), T0).await;
    store
        .quarantine(&lease, QuarantineKind::NonDeterminism, "m".into(), T0)
        .await
        .expect("quarantine");
    create(store, run_in(&tenant("other"), 4)).await;

    let list = |filter: RunFilter| async move {
        store
            .list_runs(&filter)
            .await
            .expect("list_runs")
            .into_iter()
            .map(|record| record.run.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(list(RunFilter::new(acme())).await, [id(1), id(2), id(3)]);
    let mut filter = RunFilter::new(acme());
    filter.labels = BTreeMap::from([("agent".to_owned(), "triage".to_owned())]);
    assert_eq!(list(filter.clone()).await, [id(1)]);
    filter.limit = Some(0);
    assert!(list(filter).await.is_empty(), "a limit of 0 lists nothing");
    let mut filter = RunFilter::new(acme());
    filter.status = Some(RunStatus::Active);
    assert_eq!(list(filter).await, [id(2)]);
    let mut filter = RunFilter::new(acme());
    filter.quarantined = Some(true);
    assert_eq!(list(filter).await, [id(3)]);
    let mut filter = RunFilter::new(acme());
    filter.quarantined = Some(false);
    assert_eq!(list(filter).await, [id(1), id(2)]);

    let mut page = RunFilter::new(acme());
    page.limit = Some(2);
    let first = store.list_runs(&page).await.expect("list_runs");
    assert_eq!(first.len(), 2);
    page.after = Some(RunCursor::from(&first[1]));
    assert_eq!(list(page).await, [id(3)]);
}

// -------------------------------------------------------------------------------------------
// Namespaces, their quotas, use and alerts (`archi/06-storage-and-scale.md`, "Namespaces",
// "Quotas")

/// 2026-02-01T00:00:00Z: the month after `T0`'s.
const FEBRUARY: Timestamp = Timestamp(1_769_904_000_000);

async fn put_defaults<S: JournalStore>(store: &S, defaults: Quotas, ceiling: Quotas) {
    let mut record = Tenant::new(acme());
    record.namespace_defaults = defaults;
    record.quotas = ceiling;
    store.put_tenant(&record).await.expect("put_tenant");
}

async fn create_in<S: JournalStore>(
    store: &S,
    n: u128,
    namespace: &NamespaceName,
    script: &[u8],
    at: Timestamp,
) -> CreateRun {
    store
        .create_run(new_run_of(run_ref(n), namespace, script, at), None)
        .await
        .expect("create_run")
}

fn created(outcome: &CreateRun) -> bool {
    matches!(outcome, CreateRun::Created(created) if created.created)
}

/// Commits `run_started`, then a terminal entry: the run ends `aborted`.
async fn abort<S: JournalStore>(store: &S, run: &RunRef, at: Timestamp) {
    let lease = lease_of(store, run, at).await;
    let consumed = inbox_ids(store, &lease).await;
    commit(
        store,
        &lease,
        Activation::new(
            vec![run_started_entry(), aborted_entry(1)],
            consumed,
            RunStatus::Aborted,
        ),
        at,
    )
    .await;
}

fn alert_key(
    namespace: Option<NamespaceName>,
    quota: QuotaKind,
    period: Month,
    threshold: Threshold,
) -> QuotaAlertKey {
    QuotaAlertKey {
        tenant: acme(),
        namespace,
        quota,
        period,
        threshold,
    }
}

async fn alert_keys<S: JournalStore>(
    store: &S,
    namespace: Option<&NamespaceName>,
    month: Month,
) -> BTreeSet<QuotaAlertKey> {
    store
        .quota_alerts(&acme(), namespace, month)
        .await
        .expect("quota_alerts")
        .into_iter()
        .map(|alert| alert.key)
        .collect()
}

pub async fn namespaces_are_created_at_first_use_and_keep_their_exceptions<S: JournalStore>(
    store: &S,
) {
    assert_eq!(
        store
            .get_namespace(&acme(), &alice())
            .await
            .expect("get_namespace"),
        None
    );
    let written = store
        .put_namespace(&acme(), &alice(), None, T0)
        .await
        .expect("put_namespace");
    let NamespaceWrite::Written(first) = written else {
        panic!("a new namespace is written: {written:?}");
    };
    assert_eq!(first.name, alice());
    assert_eq!(first.exceptions, Quotas::default());
    assert_eq!(first.created_at, T0);
    let exceptions = Quotas {
        active_runs: Some(50),
        cpu_per_month: Some(Duration::from_secs(36_000)),
        ..Quotas::default()
    };
    let changed = store
        .put_namespace(&acme(), &alice(), Some(&exceptions), later(T0, 1))
        .await
        .expect("put_namespace");
    assert!(
        matches!(changed, NamespaceWrite::Written(ref n) if n.exceptions == exceptions && n.created_at == T0),
        "exceptions are replaced, the creation kept: {changed:?}"
    );
    let kept = store
        .put_namespace(&acme(), &alice(), None, later(T0, 2))
        .await
        .expect("put_namespace");
    assert!(
        matches!(kept, NamespaceWrite::Written(ref n) if n.exceptions == exceptions),
        "without exceptions, they are left as they are"
    );
    assert!(created(&create_in(store, 1, &bob(), b"script", T0).await));
    let bob_record = store
        .get_namespace(&acme(), &bob())
        .await
        .expect("get_namespace")
        .expect("a run creates its namespace");
    assert_eq!(bob_record.exceptions, Quotas::default());
    assert_eq!(record(store, &run_ref(1)).await.namespace, bob());
    assert_eq!(
        store
            .get_namespace(&tenant("other"), &alice())
            .await
            .expect("get_namespace"),
        None,
        "namespaces are per tenant"
    );
    assert!(
        store
            .revoke_namespace_tokens(&acme(), &alice(), later(T0, 5))
            .await
            .expect("revoke_namespace_tokens")
    );
    assert_eq!(
        store
            .get_namespace(&acme(), &alice())
            .await
            .expect("get_namespace")
            .expect("alice")
            .tokens_revoked_before,
        Some(later(T0, 5))
    );
    assert!(
        !store
            .revoke_namespace_tokens(&acme(), &namespace("nobody"), T0)
            .await
            .expect("revoke_namespace_tokens")
    );
}

pub async fn idempotency_keys_are_scoped_by_namespace<S: JournalStore>(store: &S) {
    let in_alice = store
        .create_run(
            new_run_of(run_ref(1), &alice(), b"script", T0),
            Some("k".into()),
        )
        .await
        .expect("create_run");
    let in_bob = store
        .create_run(
            new_run_of(run_ref(2), &bob(), b"script", T0),
            Some("k".into()),
        )
        .await
        .expect("create_run");
    assert!(created(&in_alice) && created(&in_bob), "two runs");
    let retried = store
        .create_run(
            new_run_of(run_ref(3), &alice(), b"script", T0),
            Some("k".into()),
        )
        .await
        .expect("create_run");
    assert!(
        matches!(retried, CreateRun::Created(ref c) if !c.created && c.run == run_ref(1)),
        "the same key in the same namespace returns its run"
    );
}

pub async fn active_runs_are_limited_per_namespace_then_per_tenant<S: JournalStore>(store: &S) {
    put_defaults(
        store,
        Quotas {
            active_runs: Some(1),
            ..Quotas::default()
        },
        Quotas {
            active_runs: Some(3),
            ..Quotas::default()
        },
    )
    .await;
    assert!(created(&create_in(store, 1, &alice(), b"s", T0).await));
    let refused = create_in(store, 2, &alice(), b"s", T0).await;
    assert_eq!(
        refused,
        CreateRun::QuotaExceeded(QuotaExceeded {
            quota: QuotaKind::ActiveRuns,
            scope: QuotaScope::Namespace,
            limit: 1,
            used: 1,
            resets_at: None,
        })
    );
    assert_eq!(store.get_run(&run_ref(2)).await.expect("get_run"), None);
    // A second refusal records nothing new.
    create_in(store, 3, &alice(), b"s", T0).await;
    let january = Month::of(T0);
    assert_eq!(
        alert_keys(store, Some(&alice()), january).await,
        BTreeSet::from([alert_key(
            Some(alice()),
            QuotaKind::ActiveRuns,
            january,
            Threshold::Reached
        )]),
        "the first refusal of the month is an alert"
    );
    assert!(created(&create_in(store, 4, &bob(), b"s", T0).await));
    store
        .put_namespace(
            &acme(),
            &alice(),
            Some(&Quotas {
                active_runs: Some(10),
                ..Quotas::default()
            }),
            T0,
        )
        .await
        .expect("put_namespace");
    assert!(
        created(&create_in(store, 5, &alice(), b"s", T0).await),
        "an exception raises the namespace's limit"
    );
    assert_eq!(
        create_in(store, 6, &alice(), b"s", T0).await,
        CreateRun::QuotaExceeded(QuotaExceeded {
            quota: QuotaKind::ActiveRuns,
            scope: QuotaScope::Tenant,
            limit: 3,
            used: 3,
            resets_at: None,
        }),
        "the tenant's ceiling bounds the sum"
    );
    abort(store, &run_ref(1), later(T0, 10)).await;
    assert!(
        created(&create_in(store, 7, &alice(), b"s", later(T0, 20)).await),
        "an ended run frees its place"
    );
}

pub async fn runs_per_month_are_counted_and_free_the_next_month<S: JournalStore>(store: &S) {
    put_defaults(
        store,
        Quotas {
            runs_per_month: Some(5),
            ..Quotas::default()
        },
        Quotas::default(),
    )
    .await;
    for n in 1..=5 {
        assert!(created(&create_in(store, n, &alice(), b"s", T0).await));
    }
    let january = Month::of(T0);
    assert_eq!(
        create_in(store, 6, &alice(), b"s", T0).await,
        CreateRun::QuotaExceeded(QuotaExceeded {
            quota: QuotaKind::RunsPerMonth,
            scope: QuotaScope::Namespace,
            limit: 5,
            used: 5,
            resets_at: Some(FEBRUARY),
        })
    );
    assert_eq!(
        alert_keys(store, Some(&alice()), january).await,
        BTreeSet::from([
            alert_key(
                Some(alice()),
                QuotaKind::RunsPerMonth,
                january,
                Threshold::Near
            ),
            alert_key(
                Some(alice()),
                QuotaKind::RunsPerMonth,
                january,
                Threshold::Reached
            ),
        ]),
        "80 % at the fourth run, 100 % at the fifth"
    );
    let usage = store
        .namespace_usage(&acme(), &UsageOf::Namespace(alice()), january)
        .await
        .expect("namespace_usage");
    assert_eq!(usage.runs_started, 5);
    assert_eq!(
        usage.days,
        [(
            Day::of(T0),
            crate::ports::DayUsage {
                cpu: Duration::ZERO,
                runs_started: 5
            }
        )]
    );
    let tenant_usage = store
        .namespace_usage(&acme(), &UsageOf::Tenant, january)
        .await
        .expect("namespace_usage");
    assert_eq!(tenant_usage.runs_started, 5);
    assert!(
        created(&create_in(store, 7, &alice(), b"s", FEBRUARY).await),
        "a new month starts again"
    );
    assert_eq!(
        store
            .namespace_usage(&acme(), &UsageOf::Namespace(alice()), Month::of(FEBRUARY))
            .await
            .expect("namespace_usage")
            .runs_started,
        1
    );
}

pub async fn scripts_count_distinct_scripts_with_runs_kept<S: JournalStore>(store: &S) {
    put_defaults(
        store,
        Quotas {
            scripts: Some(2),
            ..Quotas::default()
        },
        Quotas::default(),
    )
    .await;
    assert!(created(&create_in(store, 1, &alice(), b"a", T0).await));
    assert!(created(&create_in(store, 2, &alice(), b"a", T0).await));
    assert!(created(&create_in(store, 3, &alice(), b"b", T0).await));
    assert!(
        created(&create_in(store, 4, &alice(), b"b", T0).await),
        "a script already counted passes"
    );
    assert_eq!(
        create_in(store, 5, &alice(), b"c", T0).await,
        CreateRun::QuotaExceeded(QuotaExceeded {
            quota: QuotaKind::Scripts,
            scope: QuotaScope::Namespace,
            limit: 2,
            used: 2,
            resets_at: None,
        })
    );
    assert!(
        created(&create_in(store, 6, &bob(), b"c", T0).await),
        "scripts are counted per namespace"
    );
    assert!(
        store
            .namespace_has_script(&acme(), &alice(), Digest::of(b"a"))
            .await
            .expect("namespace_has_script")
    );
    assert!(
        !store
            .namespace_has_script(&acme(), &alice(), Digest::of(b"c"))
            .await
            .expect("namespace_has_script")
    );
    for n in [1, 2] {
        assert!(store.delete_run(&run_ref(n), T0).await.expect("delete_run"));
    }
    assert!(
        !store
            .namespace_has_script(&acme(), &alice(), Digest::of(b"a"))
            .await
            .expect("namespace_has_script"),
        "erasing the last run of a script uncounts it"
    );
    assert!(
        created(&create_in(store, 7, &alice(), b"c", T0).await),
        "its place is free"
    );
    assert_eq!(
        store
            .namespace_usage(&acme(), &UsageOf::Tenant, Month::of(T0))
            .await
            .expect("namespace_usage")
            .scripts,
        3,
        "the tenant's count is the sum of its namespaces'"
    );
}

pub async fn cpu_time_is_counted_per_day_and_alerts_once<S: JournalStore>(store: &S) {
    put_defaults(
        store,
        Quotas {
            cpu_per_month: Some(Duration::from_secs(10)),
            ..Quotas::default()
        },
        Quotas {
            cpu_per_month: Some(Duration::from_secs(20)),
            ..Quotas::default()
        },
    )
    .await;
    store
        .put_namespace(&acme(), &alice(), None, T0)
        .await
        .expect("put_namespace");
    store
        .put_namespace(&acme(), &bob(), None, T0)
        .await
        .expect("put_namespace");
    let day = Day::of(T0);
    let next_day = Day::of(later(T0, DAY));
    let delta = |namespace: &NamespaceName, day: Day, secs: u64| UsageDelta {
        tenant: acme(),
        namespace: namespace.clone(),
        day,
        cpu: Duration::from_secs(secs),
    };
    let raised = store
        .record_usage(&[delta(&alice(), day, 7)], later(T0, 1))
        .await
        .expect("record_usage");
    assert!(raised.is_empty(), "70 % of the namespace's: {raised:?}");
    let raised = store
        .record_usage(&[delta(&alice(), next_day, 2)], later(T0, DAY))
        .await
        .expect("record_usage");
    let january = Month::of(T0);
    assert_eq!(
        raised
            .iter()
            .map(|alert| alert.key.clone())
            .collect::<Vec<_>>(),
        [alert_key(
            Some(alice()),
            QuotaKind::CpuPerMonth,
            january,
            Threshold::Near
        )]
    );
    assert_eq!(raised[0].used, 9_000);
    assert_eq!(raised[0].limit, 10_000);
    let raised = store
        .record_usage(
            &[delta(&alice(), next_day, 5), delta(&bob(), day, 4)],
            later(T0, DAY + 1),
        )
        .await
        .expect("record_usage");
    let keys: BTreeSet<QuotaAlertKey> = raised.into_iter().map(|alert| alert.key).collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            alert_key(
                Some(alice()),
                QuotaKind::CpuPerMonth,
                january,
                Threshold::Reached
            ),
            alert_key(None, QuotaKind::CpuPerMonth, january, Threshold::Near),
        ]),
        "alice passes 100 % and is not refused; the tenant reaches 80 % (18 s of 20)"
    );
    let raised = store
        .record_usage(&[delta(&alice(), next_day, 30)], later(T0, DAY + 2))
        .await
        .expect("record_usage");
    assert_eq!(
        raised
            .iter()
            .map(|alert| alert.key.clone())
            .collect::<Vec<_>>(),
        [alert_key(
            None,
            QuotaKind::CpuPerMonth,
            january,
            Threshold::Reached
        )],
        "each threshold once"
    );
    let usage = store
        .namespace_usage(&acme(), &UsageOf::Namespace(alice()), january)
        .await
        .expect("namespace_usage");
    assert_eq!(usage.cpu, Duration::from_secs(44));
    assert_eq!(
        usage
            .days
            .iter()
            .map(|(day, used)| (*day, used.cpu))
            .collect::<Vec<_>>(),
        [
            (day, Duration::from_secs(7)),
            (next_day, Duration::from_secs(37))
        ]
    );
    assert_eq!(
        store
            .namespace_usage(&acme(), &UsageOf::Tenant, january)
            .await
            .expect("namespace_usage")
            .cpu,
        Duration::from_secs(48)
    );
    store
        .record_usage(&[delta(&namespace("gone"), day, 3)], later(T0, DAY + 3))
        .await
        .expect("record_usage");
    assert_eq!(
        store
            .namespace_usage(&acme(), &UsageOf::Tenant, january)
            .await
            .expect("namespace_usage")
            .cpu,
        Duration::from_secs(51),
        "a namespace that no longer exists still counts for its tenant"
    );
    assert_eq!(
        store
            .namespace_usage(&acme(), &UsageOf::Namespace(namespace("gone")), january)
            .await
            .expect("namespace_usage")
            .cpu,
        Duration::ZERO
    );
    assert_eq!(
        store
            .get_namespace(&acme(), &alice())
            .await
            .expect("get_namespace")
            .expect("alice")
            .last_active_at,
        later(T0, DAY + 2)
    );
    assert_eq!(
        alert_keys(store, None, january).await.len(),
        4,
        "every alert of the tenant, its own included"
    );
    assert!(
        alert_keys(store, Some(&alice()), Month::of(FEBRUARY))
            .await
            .is_empty()
    );
}

pub async fn quota_alerts_are_claimed_and_acknowledged<S: JournalStore>(store: &S) {
    put_defaults(
        store,
        Quotas {
            active_runs: Some(1),
            ..Quotas::default()
        },
        Quotas::default(),
    )
    .await;
    create_in(store, 1, &alice(), b"s", T0).await;
    create_in(store, 2, &alice(), b"s", T0).await;
    create_in(store, 3, &bob(), b"s", T0).await;
    create_in(store, 4, &bob(), b"s", later(T0, 1)).await;
    let claimed = store
        .claim_quota_alerts("w", later(T0, 2), LEASE, 1)
        .await
        .expect("claim_quota_alerts");
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].key.namespace, Some(alice()), "the oldest first");
    let rest = store
        .claim_quota_alerts("v", later(T0, 2), LEASE, 10)
        .await
        .expect("claim_quota_alerts");
    assert_eq!(rest.len(), 1, "a claimed alert is not claimed again");
    assert_eq!(rest[0].key.namespace, Some(bob()));
    store
        .ack_quota_alerts(&[claimed[0].key.clone()], later(T0, 3))
        .await
        .expect("ack_quota_alerts");
    let after_lease = later(later(T0, 2), LEASE.as_millis() as u64);
    let again = store
        .claim_quota_alerts("w", after_lease, LEASE, 10)
        .await
        .expect("claim_quota_alerts");
    assert_eq!(
        again
            .iter()
            .map(|alert| alert.key.clone())
            .collect::<Vec<_>>(),
        [rest[0].key.clone()],
        "an expired claim is claimed again; a sent alert never"
    );
    let listed = store
        .quota_alerts(&acme(), Some(&alice()), Month::of(T0))
        .await
        .expect("quota_alerts");
    assert_eq!(listed[0].sent_at, Some(later(T0, 3)));
}

pub async fn namespaces_are_listed_by_their_use<S: JournalStore>(store: &S) {
    put_defaults(
        store,
        Quotas {
            runs_per_month: Some(10),
            ..Quotas::default()
        },
        Quotas::default(),
    )
    .await;
    let names = ["user-a", "user-b", "user-c", "other"];
    let mut n = 0;
    for (name, runs) in names.iter().zip([1, 3, 8, 2]) {
        for _ in 0..runs {
            n += 1;
            create_in(store, n, &namespace(name), b"s", T0).await;
        }
    }
    let filter = |order, prefix: Option<&str>, above, after, limit| NamespaceFilter {
        tenant: acme(),
        prefix: prefix.map(str::to_owned),
        above_percent: above,
        order,
        month: Month::of(T0),
        after,
        limit,
    };
    let names_of = |page: crate::ports::NamespacePage| -> Vec<String> {
        page.namespaces
            .into_iter()
            .map(|listed| listed.namespace.name.to_string())
            .collect()
    };
    let by_runs = store
        .list_namespaces(&filter(NamespaceOrder::Runs, None, None, None, 10))
        .await
        .expect("list_namespaces");
    assert_eq!(by_runs.total, 4);
    assert_eq!(by_runs.namespaces[0].usage.runs_started, 8);
    assert_eq!(by_runs.namespaces[0].limits.runs_per_month, Some(10));
    assert_eq!(names_of(by_runs), ["user-c", "user-b", "other", "user-a"]);
    let prefixed = store
        .list_namespaces(&filter(NamespaceOrder::Name, Some("user-"), None, None, 10))
        .await
        .expect("list_namespaces");
    assert_eq!(
        prefixed.total, 3,
        "the total counts what the prefix matches"
    );
    assert_eq!(names_of(prefixed), ["user-a", "user-b", "user-c"]);
    let above = store
        .list_namespaces(&filter(NamespaceOrder::Runs, None, Some(80), None, 10))
        .await
        .expect("list_namespaces");
    assert_eq!(above.total, 1, "the total counts what the share matches");
    assert_eq!(names_of(above), ["user-c"], "8 runs of 10");
    let first = store
        .list_namespaces(&filter(NamespaceOrder::Runs, None, None, None, 2))
        .await
        .expect("list_namespaces");
    assert_eq!(first.total, 4, "the total ignores the limit");
    let last = first.namespaces.last().expect("two");
    let after = NamespaceCursor {
        key: last.usage.runs_started,
        name: last.namespace.name.clone(),
    };
    let rest = store
        .list_namespaces(&filter(NamespaceOrder::Runs, None, None, Some(after), 10))
        .await
        .expect("list_namespaces");
    assert_eq!(rest.total, 4, "the total ignores the cursor");
    assert_eq!(names_of(rest), ["other", "user-a"]);
    let past = NamespaceCursor {
        key: 0,
        name: namespace("zzz"),
    };
    let empty = store
        .list_namespaces(&filter(NamespaceOrder::Runs, None, None, Some(past), 10))
        .await
        .expect("list_namespaces");
    assert!(empty.namespaces.is_empty());
    assert_eq!(empty.total, 4, "an empty page still has its total");
    let none = store
        .list_namespaces(&filter(
            NamespaceOrder::Name,
            Some("nobody"),
            None,
            None,
            10,
        ))
        .await
        .expect("list_namespaces");
    assert_eq!(none, crate::ports::NamespacePage::default());
}

pub async fn an_erased_namespace_takes_no_run_and_is_dropped_once_empty<S: JournalStore>(
    store: &S,
) {
    start(store, run_ref(1)).await;
    let cpu = UsageDelta {
        tenant: acme(),
        namespace: alice(),
        day: Day::of(T0),
        cpu: Duration::from_secs(2),
    };
    store
        .record_usage(&[cpu], later(T0, 1))
        .await
        .expect("record_usage");
    assert_eq!(
        store
            .delete_namespace(&acme(), &alice(), later(T0, 2))
            .await
            .expect("delete_namespace"),
        DeleteNamespace::Started
    );
    assert_eq!(
        store
            .delete_namespace(&acme(), &namespace("nobody"), T0)
            .await
            .expect("delete_namespace"),
        DeleteNamespace::NotFound
    );
    assert_eq!(
        create_in(store, 2, &alice(), b"s", later(T0, 3)).await,
        CreateRun::NamespaceDeleting
    );
    assert_eq!(
        store
            .put_namespace(&acme(), &alice(), Some(&Quotas::default()), later(T0, 3))
            .await
            .expect("put_namespace"),
        NamespaceWrite::Deleting
    );
    assert_eq!(
        store
            .namespaces_to_erase(10)
            .await
            .expect("namespaces_to_erase"),
        [(acme(), alice())]
    );
    assert_eq!(
        store
            .drop_namespace(&acme(), &alice())
            .await
            .expect("drop_namespace"),
        DropNamespace::RunsLeft
    );
    let mut of_alice = RunFilter::new(acme());
    of_alice.namespace = Some(alice());
    let runs = store.list_runs(&of_alice).await.expect("list_runs");
    assert_eq!(runs.len(), 1);
    for run in runs {
        store
            .delete_run(&run.run, later(T0, 4))
            .await
            .expect("delete_run");
    }
    assert_eq!(
        store
            .drop_namespace(&acme(), &alice())
            .await
            .expect("drop_namespace"),
        DropNamespace::Dropped
    );
    assert_eq!(
        store
            .get_namespace(&acme(), &alice())
            .await
            .expect("get_namespace"),
        None
    );
    assert!(
        store
            .namespaces_to_erase(10)
            .await
            .expect("namespaces_to_erase")
            .is_empty()
    );
    let january = Month::of(T0);
    assert_eq!(
        store
            .namespace_usage(&acme(), &UsageOf::Namespace(alice()), january)
            .await
            .expect("namespace_usage")
            .cpu,
        Duration::ZERO
    );
    let kept = store
        .namespace_usage(&acme(), &UsageOf::Tenant, january)
        .await
        .expect("namespace_usage");
    assert_eq!(
        (kept.cpu, kept.runs_started),
        (Duration::from_secs(2), 1),
        "the tenant keeps what the namespace consumed"
    );
    assert!(
        created(&create_in(store, 3, &alice(), b"s", later(T0, 5)).await),
        "the name can be used again"
    );
}

pub async fn runs_tasks_and_parked_calls_are_filtered_by_namespace<S: JournalStore>(store: &S) {
    // `start_with_tasks` creates its runs in alice; bob's run is created by hand.
    start_with_tasks(store, run_ref(1), vec![host_task(1, "ask_user")]).await;
    store
        .create_run(new_run_of(run_ref(2), &bob(), b"script", T0), None)
        .await
        .expect("create_run");
    let lease = lease_of(store, &run_ref(2), T0).await;
    let consumed = inbox_ids(store, &lease).await;
    let mut activation = Activation::new(vec![run_started_entry()], consumed, RunStatus::Active);
    activation.tool_calls.push(host_task(1, "ask_user"));
    commit(store, &lease, activation, T0).await;
    store.release(&lease).await.expect("release");

    let mut of_bob = RunFilter::new(acme());
    of_bob.namespace = Some(bob());
    let listed: Vec<RunRef> = store
        .list_runs(&of_bob)
        .await
        .expect("list_runs")
        .into_iter()
        .map(|record| record.run)
        .collect();
    assert_eq!(listed, [run_ref(2)]);

    let host = TaskFilter::Host {
        tenant: acme(),
        namespace: Some(bob()),
        toolset: None,
        tools: BTreeSet::new(),
    };
    let claimed = claim(store, "h", &host, T0, 10).await;
    assert_eq!(
        claimed
            .iter()
            .map(|task| task.task.task.run.clone())
            .collect::<Vec<_>>(),
        [run_ref(2)],
        "a worker of one namespace claims its tasks only"
    );
    let expired = ParkReason {
        kind: ParkKind::CredentialsExpired,
        message: "401".into(),
    };
    store
        .park_tool_task(&claimed[0].token, expired.clone())
        .await
        .expect("park");
    let all = claim(store, "a", &TaskFilter::All, T0, 10).await;
    assert_eq!(all.len(), 1, "alice's task");
    store
        .park_tool_task(&all[0].token, expired)
        .await
        .expect("park");
    let scope = ParkedScope::Namespace {
        tenant: acme(),
        namespace: bob(),
        toolset: toolset_digest(),
    };
    let at = later(T0, 10);
    assert_eq!(
        store.resume_parked(&scope, at).await.expect("resume"),
        1,
        "bob's call only"
    );
    assert_eq!(
        store.tool_tasks(&run_ref(2)).await.expect("tool_tasks")[0].state,
        TaskState::Ready {
            next_attempt_at: at
        }
    );
    assert!(matches!(
        store.tool_tasks(&run_ref(1)).await.expect("tool_tasks")[0].state,
        TaskState::Parked(_)
    ));
}

// Workflows and watchers

fn workflow(name: &str, script: &str) -> Workflow {
    Workflow::new(
        name,
        Some(format!("The {name} workflow")),
        Digest::of(script.as_bytes()),
        toolset_digest(),
        WorkflowSchemas {
            input: json!({ "type": "object" }),
            output: json!({}),
            signals: BTreeMap::from([("approval".to_owned(), json!({ "type": "boolean" }))]),
        },
    )
    .expect("a valid workflow")
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

pub async fn workflow_versions_are_stored_by_tenant_and_listed_newest_first<S: JournalStore>(
    store: &S,
) {
    let none = BTreeMap::new();
    let scoped = labels(&[("conversation", "c-1")]);
    let first = workflow("triage", "v1");
    let second = workflow("triage", "v2");
    let third = workflow("triage", "v3");
    let digest = workflow("digest", "d1");
    let put = |workflow: Workflow, labels: BTreeMap<String, String>, at: Timestamp| async move {
        store
            .put_workflow(&acme(), &workflow, &labels, at)
            .await
            .expect("put_workflow")
    };
    let stored = put(first.clone(), none.clone(), T0).await;
    assert_eq!(
        (&stored.workflow, &stored.labels, stored.created_at),
        (&first, &none, T0)
    );
    put(second.clone(), scoped.clone(), later(T0, 10)).await;
    put(digest.clone(), scoped.clone(), later(T0, 20)).await;
    put(third.clone(), none.clone(), later(T0, 30)).await;
    // A version is immutable: a second put changes nothing.
    let again = put(first.clone(), scoped.clone(), later(T0, 40)).await;
    assert_eq!((&again.labels, again.created_at), (&none, T0));
    store
        .put_workflow(
            &tenant("other"),
            &workflow("triage", "elsewhere"),
            &none,
            later(T0, 50),
        )
        .await
        .expect("put_workflow");

    let found = store
        .get_workflow(&acme(), second.version())
        .await
        .expect("get_workflow")
        .expect("the version is stored");
    assert_eq!(
        (&found.workflow, &found.labels, found.created_at),
        (&second, &scoped, later(T0, 10))
    );
    assert_eq!(
        store
            .get_workflow(&tenant("other"), second.version())
            .await
            .expect("get_workflow"),
        None,
        "another tenant's version"
    );

    let versions = |labels: BTreeMap<String, String>| async move {
        store
            .workflow_versions(&acme(), "triage", &labels)
            .await
            .expect("workflow_versions")
            .into_iter()
            .map(|stored| stored.workflow)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        versions(none.clone()).await,
        [third.clone(), second.clone(), first.clone()],
        "newest first"
    );
    assert_eq!(
        versions(scoped.clone()).await,
        std::slice::from_ref(&second),
        "only the versions carrying the labels"
    );
    assert_eq!(
        store
            .workflow_versions(&acme(), "nothing", &none)
            .await
            .expect("workflow_versions"),
        []
    );

    let latest = |labels: BTreeMap<String, String>, after: Option<String>, limit: Option<usize>| async move {
        store
            .latest_workflows(&WorkflowFilter {
                tenant: acme(),
                labels,
                after,
                limit,
            })
            .await
            .expect("latest_workflows")
            .into_iter()
            .map(|stored| stored.workflow)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        latest(none.clone(), None, None).await,
        [digest.clone(), third.clone()],
        "the latest version of each workflow, by name"
    );
    assert_eq!(
        latest(scoped.clone(), None, None).await,
        [digest.clone(), second.clone()],
        "the latest among the versions carrying the labels"
    );
    assert_eq!(
        latest(none.clone(), None, Some(1)).await,
        std::slice::from_ref(&digest)
    );
    assert_eq!(
        latest(none.clone(), Some("digest".into()), None).await,
        std::slice::from_ref(&third)
    );

    // A script is the script of the versions that name it, whatever their workflow.
    let shared = workflow("digest", "v3");
    put(shared.clone(), scoped.clone(), later(T0, 60)).await;
    let with_script = |script: &str, labels: BTreeMap<String, String>| {
        let script = Digest::of(script.as_bytes());
        async move {
            store
                .workflows_with_script(&acme(), script, &labels)
                .await
                .expect("workflows_with_script")
                .into_iter()
                .map(|stored| stored.workflow)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        with_script("v3", none.clone()).await,
        [shared.clone(), third],
        "newest first, across workflows"
    );
    assert_eq!(
        with_script("v3", scoped).await,
        [shared],
        "carrying the labels"
    );
    assert_eq!(with_script("v1", none.clone()).await, [first]);
    assert_eq!(with_script("nothing", none).await, []);
}

/// Creates a run that `starter` started with the call `seq`.
async fn create_started<S: JournalStore>(
    store: &S,
    run: RunRef,
    starter: RunId,
    seq: u32,
    detached: bool,
) -> RunRef {
    let mut new = new_run(run.clone());
    new.metadata.started_by = Some(StartedBy {
        run: starter,
        seq: Seq(seq),
        detached,
    });
    store.create_run(new, None).await.expect("create_run");
    let lease = lease_of(store, &run, T0).await;
    let consumed = inbox_ids(store, &lease).await;
    commit(
        store,
        &lease,
        Activation::new(vec![run_started_entry()], consumed, RunStatus::Active),
        T0,
    )
    .await;
    store.release(&lease).await.expect("release");
    run
}

/// Ends an active run whose history holds only its `run_started`, with this end.
async fn end<S: JournalStore>(
    store: &S,
    run: &RunRef,
    end: Option<serde_json::Value>,
    at: Timestamp,
) {
    let lease = lease_of(store, run, at).await;
    let consumed = inbox_ids(store, &lease).await;
    let mut activation = Activation::new(vec![aborted_entry(1)], consumed, RunStatus::Aborted);
    activation.end = end;
    commit(store, &lease, activation, at).await;
}

pub async fn a_run_ends_to_its_watchers_and_cancels_its_attached_runs<S: JournalStore>(store: &S) {
    let parent = start(store, run_ref(1)).await;
    let attached = create_started(store, run_ref(2), parent.id, 1, false).await;
    let detached = create_started(store, run_ref(3), parent.id, 2, true).await;
    let finished = create_started(store, run_ref(4), parent.id, 3, false).await;
    let fork = start(store, run_ref(5)).await;
    let ended_fork = start(store, run_ref(6)).await;
    for watcher in [fork.id, ended_fork.id] {
        assert_eq!(
            store
                .watch_run(&attached, watcher, Seq(1))
                .await
                .expect("watch_run"),
            Some(Watched { ended: false })
        );
    }
    end(store, &ended_fork, None, later(T0, 1)).await;

    // A run's end reaches each watcher that has not ended, once, under its name as key.
    let outcome = json!({ "status": "completed", "output": { "closed": 3 } });
    end(store, &attached, Some(outcome.clone()), later(T0, 2)).await;
    let ended = Trigger::Completion(Completion::SignalReceived {
        name: end_signal_name(attached.id),
        payload: Payload::Value(outcome),
    });
    assert_eq!(
        inbox(store, &parent, later(T0, 3)).await,
        std::slice::from_ref(&ended)
    );
    assert_eq!(inbox(store, &fork, later(T0, 3)).await, [ended]);
    assert_eq!(
        store
            .enqueue(
                &parent,
                Completion::SignalReceived {
                    name: "late".into(),
                    payload: json!(1).into(),
                },
                Some(end_signal_name(attached.id)),
                later(T0, 4),
            )
            .await
            .expect("enqueue"),
        Enqueued::Duplicate,
        "the end's key is the signal's name"
    );

    // A run that ended without an end to give reaches no one; its parent still knows nothing.
    end(store, &finished, None, later(T0, 5)).await;
    assert_eq!(inbox(store, &parent, later(T0, 6)).await.len(), 1);

    // The parent's end cancels its attached runs still running, not its detached ones.
    let still_attached = create_started(store, run_ref(7), parent.id, 4, false).await;
    end(
        store,
        &parent,
        Some(json!({ "status": "aborted" })),
        later(T0, 7),
    )
    .await;
    let cancelled = inbox(store, &still_attached, later(T0, 8)).await;
    assert!(
        matches!(
            cancelled.as_slice(),
            [Trigger::Completion(Completion::CancelRequested { reason })]
                if *reason == rewind_core::starter_ended(parent.id)
        ),
        "{cancelled:?}"
    );
    assert_eq!(inbox(store, &detached, later(T0, 8)).await, []);

    // The detached run's end has no watcher left that runs: it goes nowhere.
    end(
        store,
        &detached,
        Some(json!({ "status": "completed", "output": 1 })),
        later(T0, 9),
    )
    .await;
    assert_eq!(record(store, &parent).await.status, RunStatus::Aborted);
}

pub async fn watching_a_run_says_whether_it_has_ended<S: JournalStore>(store: &S) {
    let run = start(store, run_ref(1)).await;
    let watcher = start(store, run_ref(2)).await;
    assert_eq!(
        store
            .watch_run(&run_ref(9), watcher.id, Seq(1))
            .await
            .expect("watch_run"),
        None,
        "an unknown run"
    );
    assert_eq!(
        store
            .watch_run(&run_in(&tenant("other"), 1), watcher.id, Seq(1))
            .await
            .expect("watch_run"),
        None,
        "another tenant's run"
    );
    end(
        store,
        &run,
        Some(json!({ "status": "completed", "output": 1 })),
        later(T0, 1),
    )
    .await;
    for _ in 0..2 {
        assert_eq!(
            store
                .watch_run(&run, watcher.id, Seq(1))
                .await
                .expect("watch_run"),
            Some(Watched { ended: true })
        );
    }
    assert_eq!(
        inbox(store, &watcher, later(T0, 2)).await,
        [],
        "the end of a run that had ended is the caller's to enqueue"
    );
}

// Tests (`archi/05-testing-and-observability.md`, "Where tests are stored")

fn test_record(name: &str, namespace: Option<&str>) -> TestRecord {
    TestRecord {
        digest: Digest::of(name.as_bytes()),
        namespace: namespace.map(self::namespace),
        size: 100,
    }
}

fn with_tests(workflow: Workflow, tests: &[&TestRecord]) -> Workflow {
    workflow
        .with_suite(Suite {
            parent: None,
            tests: tests
                .iter()
                .enumerate()
                .map(|(position, test)| SuiteTest {
                    name: format!("test {position}"),
                    test: test.digest,
                })
                .collect(),
            retired: Vec::new(),
            updated: Vec::new(),
        })
        .expect("a valid suite")
}

pub async fn tests_are_kept_while_a_version_references_them<S: JournalStore>(store: &S) {
    let referenced = test_record("referenced", None);
    let recorded = test_record("recorded", Some("alice"));
    let late = test_record("late", None);
    assert_eq!(
        store
            .reserve_tests(&acme(), &[referenced.clone(), recorded.clone()], T0)
            .await
            .expect("reserve_tests"),
        Vec::<Digest>::new()
    );
    assert_eq!(
        store
            .get_test(&acme(), recorded.digest)
            .await
            .expect("get_test"),
        Some(recorded.clone())
    );
    assert_eq!(
        store
            .get_test(&tenant("other"), recorded.digest)
            .await
            .expect("get_test"),
        None,
        "another tenant's test"
    );

    // A version referencing a test without a row is refused.
    let refused = store
        .put_workflow(
            &acme(),
            &with_tests(workflow("triage", "v0"), &[&referenced, &late]),
            &BTreeMap::new(),
            T0,
        )
        .await;
    assert!(
        matches!(refused, Err(StoreError::Conflict(_))),
        "{refused:?}"
    );
    store
        .put_workflow(
            &acme(),
            &with_tests(workflow("triage", "v1"), &[&referenced]),
            &BTreeMap::new(),
            T0,
        )
        .await
        .expect("put_workflow");

    // Within their grace, no test is cleaned up.
    let grace = T0.saturating_add(TEST_GRACE);
    assert_eq!(
        store
            .unreferenced_tests(later(T0, 1), 10)
            .await
            .expect("unreferenced_tests"),
        []
    );
    // After it, the test no version references is marked, and only once.
    assert_eq!(
        store
            .unreferenced_tests(grace, 10)
            .await
            .expect("unreferenced_tests"),
        [(acme(), recorded.digest)]
    );
    assert_eq!(
        store
            .unreferenced_tests(grace, 10)
            .await
            .expect("unreferenced_tests"),
        []
    );
    // A test being deleted cannot be read, nor reserved again until its row is gone.
    assert_eq!(
        store
            .get_test(&acme(), recorded.digest)
            .await
            .expect("get_test"),
        None
    );
    assert_eq!(
        store
            .reserve_tests(&acme(), std::slice::from_ref(&recorded), grace)
            .await
            .expect("reserve_tests"),
        [recorded.digest]
    );
    assert_eq!(
        store
            .drop_tests(
                &[(acme(), recorded.digest), (acme(), referenced.digest)],
                false
            )
            .await
            .expect("drop_tests"),
        1,
        "only the marked row goes"
    );
    assert_eq!(
        store
            .reserve_tests(&acme(), std::slice::from_ref(&recorded), grace)
            .await
            .expect("reserve_tests"),
        Vec::<Digest>::new(),
        "a dropped test is stored anew"
    );
    assert_eq!(
        store
            .get_test(&acme(), referenced.digest)
            .await
            .expect("get_test"),
        Some(referenced.clone())
    );

    // Reserving a test starts its grace again.
    let renewed = test_record("renewed", None);
    store
        .reserve_tests(&acme(), std::slice::from_ref(&renewed), T0)
        .await
        .expect("reserve_tests");
    store
        .reserve_tests(
            &acme(),
            std::slice::from_ref(&renewed),
            later(T0, 3_600_000),
        )
        .await
        .expect("reserve_tests");
    assert!(
        !store
            .unreferenced_tests(grace, 10)
            .await
            .expect("unreferenced_tests")
            .contains(&(acme(), renewed.digest))
    );
}

pub async fn tests_of_an_erased_namespace_go_whatever_references_them<S: JournalStore>(store: &S) {
    let alice = test_record("alice's", Some("alice"));
    let bob = test_record("bob's", Some("bob"));
    let written = test_record("written", None);
    store
        .reserve_tests(&acme(), &[alice.clone(), bob.clone(), written.clone()], T0)
        .await
        .expect("reserve_tests");
    store
        .put_workflow(
            &acme(),
            &with_tests(workflow("triage", "v1"), &[&alice, &written]),
            &BTreeMap::new(),
            T0,
        )
        .await
        .expect("put_workflow");
    assert_eq!(
        store
            .namespace_tests(&acme(), &namespace("alice"), 10)
            .await
            .expect("namespace_tests"),
        [alice.digest]
    );
    assert_eq!(
        store
            .namespace_tests(&tenant("other"), &namespace("alice"), 10)
            .await
            .expect("namespace_tests"),
        []
    );
    // Not marked, and referenced: an erasure drops it anyway.
    assert_eq!(
        store
            .drop_tests(&[(acme(), alice.digest)], true)
            .await
            .expect("drop_tests"),
        1
    );
    assert_eq!(
        store
            .get_test(&acme(), alice.digest)
            .await
            .expect("get_test"),
        None
    );
    assert_eq!(
        store
            .namespace_tests(&acme(), &namespace("alice"), 10)
            .await
            .expect("namespace_tests"),
        []
    );
    // The version keeps the digest in its document, and is read as it was.
    let stored = store
        .get_workflow(
            &acme(),
            with_tests(workflow("triage", "v1"), &[&alice, &written]).version(),
        )
        .await
        .expect("get_workflow")
        .expect("the version is still stored");
    assert_eq!(stored.workflow.suite().tests.len(), 2);
    // The version's other test is still referenced.
    assert_eq!(
        store
            .unreferenced_tests(T0.saturating_add(TEST_GRACE), 10)
            .await
            .expect("unreferenced_tests"),
        [(acme(), bob.digest)]
    );
}
