//! `JournalStore` on Postgres. The in-memory `MemoryJournal` of `rewind-engine` is the reference of
//! every behavior here; both pass the same conformance suite.
//!
//! Several engines share the database, so every method holds against concurrent callers:
//!
//! - Rows that workers compete for are picked with `FOR UPDATE SKIP LOCKED`: runnable runs,
//!   quarantined runs, runs past their deadline, due tool tasks, feed items. Two workers never
//!   wait for each other there, and never take the same row.
//! - Everything a run owns (its history, inbox, timers, tasks, keys and webhook tokens) is
//!   written under the run's row lock, taken first. The lock orders those writes: an item
//!   enqueued while a commit consumes the inbox is never lost, and nothing is enqueued for a run
//!   that a concurrent commit ends. Callers that lock several runs lock them by id; payload rows
//!   are locked by digest, after the run. A terminal commit enqueues into the runs its end
//!   reaches (`run_watchers`): it locks them with its run, all by id, before anything else.
//! - The concurrency limits of tool tasks hold across claimers: a claim decides for a run's tool
//!   (or a worker's MCP server) only while it holds a transaction-scoped advisory lock on it.
//! - Test rows are locked by tenant and digest. A new workflow version locks the rows of its
//!   suite (`FOR SHARE`) until it has written its references; the cleanup of tests marks only
//!   rows it holds (`FOR UPDATE SKIP LOCKED`), and checks their references again once it holds
//!   them. A row is never marked while a version that references it is being written.
//! - A new workflow version takes the next number of its workflow under a transaction-scoped
//!   advisory lock on the workflow, and a new run the suffix of its handle under its tenant's
//!   (the lock of its start): two of them never take the same.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use rewind_core::{
    CancelReason, Completion, Digest, Epoch, HistoryEntry, JsonValue, Payload, RunId, RunStatus,
    Script, SecretRef, Seq, Timestamp, ToolFailure, ToolOutcome, ToolPolicy, Toolset, Workflow,
    end_signal_name, starter_ended, suffix_len_apart, suffix_neighbours,
};
use rewind_engine::ports::{
    AcquireRunnable, Activation, Archive, ClaimedTask, CompiledScript, CreateRun, Created,
    Enqueued, ExpiredRuns, Failures, FeedConsumer, FeedItem, ForkParent, InboxItem, JournalStore,
    Lease, NewRun, PAYLOAD_DELETION_TIMEOUT, ParkReason, ParkedScope, PayloadRecord,
    QUARANTINE_AFTER_FAILURES, Quarantine, QuarantineKind, Retention, RunFilter, RunMetadata,
    RunRecord, RunRef, RunWorkflow, ScriptTitle, Secret, SecretId, SecretNamespace, SecretUsage,
    StartedBy, StoreError, StoredScript, StoredToolset, StoredWorkflow, Subscription, TEST_GRACE,
    TaskFilter, TaskRef, TaskState, TaskToken, TaskUpdate, Tenant, TenantId, TenantScope,
    TestRecord, ToolDeclarations, ToolTask, TraceContext, Trigger, Watched, WebhookTarget,
    WebhookToken, WorkflowAttaching, WorkflowFilter, deferred_timeout_failure,
};
use rewind_engine::ports::{
    DeleteNamespace, DisplayName, DropNamespace, Month, Namespace, NamespaceFilter, NamespaceName,
    NamespacePage, NamespaceWrite, QuotaAlert, QuotaAlertKey, Quotas, Usage, UsageDelta, UsageOf,
};
use sqlx::PgConnection;
use sqlx::postgres::PgRow;

use uuid::Uuid;

use crate::codec::{self, decode_labels, encode_labels};
use crate::sql::{
    Columns, Failure, bytes, int, limit, ms, opt_ts, retrying, scope_clause, tenant, ts, uuid,
};
use crate::{Error, PostgresStore, namespaces};

type Fail = Failure<StoreError>;

impl From<StoreError> for Fail {
    fn from(err: StoreError) -> Self {
        Failure::Port(err)
    }
}

impl From<Fail> for StoreError {
    fn from(failure: Fail) -> Self {
        match failure {
            Failure::Port(err) => err,
            Failure::Db(err) if err.is_unique_violation() => StoreError::Conflict(err.to_string()),
            Failure::Db(err) => StoreError::Backend(err.to_string()),
        }
    }
}

impl From<Error> for StoreError {
    fn from(err: Error) -> Self {
        StoreError::from(Fail::Db(err))
    }
}

/// The terminal statuses, for `status IN …` and `status NOT IN …`.
macro_rules! terminal {
    () => {
        "('completed', 'failed', 'cancelled', 'aborted')"
    };
}

/// The columns of a [`RunRecord`], read by [`record_from_row`].
macro_rules! run_columns {
    () => {
        "id, tenant, script, toolset, status, epoch, expires_at, history_len, quarantine_kind, \
         quarantine_reason, quarantine_since, quarantine_retried_by, failures, sticky_worker, \
         labels::text, traceparent, tracestate, parent, idempotency_key, created_at, updated_at, \
         ended_at, archive_document, archived_at, parent_at, attempts, workflow, \
         workflow_version, started_by, started_seq, detached, scope::text, namespace, \
         secrets::text, origin, suffix_len::bigint"
    };
}

/// The columns of a [`StoredWorkflow`], read by [`workflow_from_row`].
macro_rules! workflow_columns {
    () => {
        "version, document, labels::text, created_at, number"
    };
}

/// The columns of a [`ToolTask`], read by [`task_from_row`].
macro_rules! task_columns {
    () => {
        "tenant, run_id, seq, toolset, tool, route, args_digest, args, on_interruption, approval, \
         execution_timeout_ms, heartbeat_timeout_ms, concurrency, state, next_attempt_at, \
         lease_owner, lease_until, parked_kind, parked_reason, attempt, created_at, \
         server_concurrency, mcp_server, schema_change_approved, due_at"
    };
}

/// Starts a statement whose commit does not wait for its WAL to reach the disk (`FROM
/// unflushed` makes it run): lease bookkeeping whose loss in a crash of the database only makes
/// a lease end later (`release`) or sooner (`renew`), or keeps a sticky priority. The fence still
/// guards every commit, and any later durable commit flushes it too.
macro_rules! unflushed {
    () => {
        "WITH unflushed AS (SELECT set_config('synchronous_commit', 'off', true)) "
    };
}

/// A test (`t`) the cleanup may start deleting at `$1`: stored or reserved at least its grace
/// (`$2`) before, not being deleted, or for at least `$3`, referenced by no version, and whose
/// digest is not also a payload's (they share the blobs' keys).
macro_rules! test_due {
    () => {
        "t.reserved_at <= $1 - $2 \
         AND (t.deletion_started_at IS NULL OR t.deletion_started_at <= $1 - $3) \
         AND NOT EXISTS (SELECT 1 FROM workflow_tests w \
                         WHERE w.tenant = t.tenant AND w.digest = t.digest) \
         AND NOT EXISTS (SELECT 1 FROM payloads p \
                         WHERE p.tenant = t.tenant AND p.digest = t.digest)"
    };
}

/// `a + b` in milliseconds, at most `i64::MAX`, for a time computed in SQL.
macro_rules! saturating_add {
    ($a:literal, $b:literal) => {
        concat!(
            "(CASE WHEN ",
            $b,
            " > 9223372036854775807 - ",
            $a,
            " THEN 9223372036854775807 ELSE ",
            $a,
            " + ",
            $b,
            " END)"
        )
    };
}

fn record_from_row(row: &PgRow) -> Result<RunRecord, Error> {
    let quarantine = match row.opt_text(8)? {
        None => None,
        Some(kind) => Some(Quarantine {
            kind: codec::parse_quarantine_kind(&kind)?,
            reason: row.text(9)?,
            since: row.ts(10)?,
            retried_by: row.opt_text(11)?,
        }),
    };
    let trace_context = row
        .opt_text(15)?
        .map(|traceparent| -> Result<TraceContext, Error> {
            Ok(TraceContext {
                traceparent,
                tracestate: row.opt_text(16)?,
            })
        })
        .transpose()?;
    let archive = match row.opt_digest(22)? {
        None => None,
        Some(document) => Some(Archive {
            document,
            archived_at: row.ts(23)?,
        }),
    };
    let namespace = row.text(32)?;
    let suffix_len = row.u64(35)?;
    Ok(RunRecord {
        run: RunRef::new(row.tenant(1)?, row.run_id(0)?),
        namespace: NamespaceName::new(namespace.clone())
            .map_err(|_| Error::Corrupt(format!("invalid namespace {namespace:?}")))?,
        origin: row.text(34)?,
        suffix_len: u8::try_from(suffix_len)
            .map_err(|_| Error::Corrupt(format!("a suffix of {suffix_len} characters")))?,
        script: row.digest(2)?,
        toolset: row.digest(3)?,
        status: codec::parse_status(&row.text(4)?)?,
        epoch: Epoch(row.u32(5)?),
        expires_at: row.ts(6)?,
        history_len: row.u64(7)?,
        quarantine,
        failures: row.u32(12)?,
        attempts: row.u32(25)?,
        sticky_worker: row.opt_text(13)?,
        metadata: RunMetadata {
            labels: decode_labels(&row.text(14)?)?,
            trace_context,
            parent: row
                .opt_run_id(17)?
                .map(|run| -> Result<ForkParent, Error> {
                    Ok(ForkParent {
                        run,
                        at: row.opt_u64(24)?,
                    })
                })
                .transpose()?,
            workflow: row
                .opt_text(26)?
                .map(|name| -> Result<RunWorkflow, Error> {
                    Ok(RunWorkflow {
                        name,
                        version: row.digest(27)?,
                    })
                })
                .transpose()?,
            started_by: row
                .opt_run_id(28)?
                .map(|run| -> Result<StartedBy, Error> {
                    Ok(StartedBy {
                        run,
                        seq: Seq(row.u32(29)?),
                        detached: row.boolean(30)?,
                    })
                })
                .transpose()?,
            scope: decode_labels(&row.text(31)?)?,
            secrets: codec::decode_attachments(&row.text(33)?)?,
        },
        idempotency_key: row.opt_text(18)?,
        created_at: row.ts(19)?,
        updated_at: row.ts(20)?,
        ended_at: row.opt_ts(21)?,
        archive,
    })
}

/// A task as loaded, with what the claim needs besides the port's view.
struct TaskRow {
    task: ToolTask,
    server_concurrency: Option<u32>,
    mcp_server: Option<String>,
    due_at: Option<i64>,
}

fn task_from_row(row: &PgRow) -> Result<TaskRow, Error> {
    let state = match row.text(13)?.as_str() {
        "approval" => TaskState::Approval,
        "ready" => TaskState::Ready {
            next_attempt_at: row.ts(14)?,
        },
        "claimed" => TaskState::Claimed {
            owner: row.text(15)?,
            until: row.ts(16)?,
        },
        "deferred" => TaskState::Deferred { until: row.ts(16)? },
        "parked" => TaskState::Parked(ParkReason {
            kind: codec::parse_park_kind(&row.text(17)?)?,
            message: row.text(18)?,
        }),
        other => return Err(Error::Corrupt(format!("unknown task state {other:?}"))),
    };
    let args = codec::decode_payload(&row.bytes(6)?, row.opt_text(7)?.as_deref())?;
    let task = ToolTask {
        task: TaskRef {
            run: RunRef::new(row.tenant(0)?, row.run_id(1)?),
            seq: Seq(row.u32(2)?),
        },
        toolset: row.digest(3)?,
        tool: row.text(4)?,
        route: codec::decode_route(&row.text(5)?)?,
        args,
        policy: ToolPolicy {
            on_interruption: codec::parse_interruption(&row.text(8)?)?,
            approval: row.boolean(9)?,
            execution_timeout: row.opt_duration(10)?,
            heartbeat_timeout: row.duration(11)?,
            concurrency: row.u32(12)?,
        },
        state,
        attempt: row.u32(19)?,
        schema_change_approved: row.boolean(23)?,
        created_at: row.ts(20)?,
    };
    Ok(TaskRow {
        task,
        server_concurrency: row.opt_u32(21)?,
        mcp_server: row.opt_text(22)?,
        due_at: row.opt_i64(24)?,
    })
}

fn workflow_from_row(row: &PgRow) -> Result<StoredWorkflow, Error> {
    let version = row.digest(0)?;
    let workflow = rewind_format::decode_workflow(&row.text(1)?)
        .map_err(|err| Error::Corrupt(format!("workflow {version}: {err}")))?;
    if workflow.version() != version {
        return Err(Error::Corrupt(format!(
            "workflow {version} does not hash to its key"
        )));
    }
    Ok(StoredWorkflow {
        workflow,
        labels: decode_labels(&row.text(2)?)?,
        created_at: row.ts(3)?,
        number: row.u32(4)?,
    })
}

fn feed_consumer(name: &str) -> Result<FeedConsumer, Error> {
    FeedConsumer::ALL
        .into_iter()
        .find(|consumer| consumer.as_str() == name)
        .ok_or_else(|| Error::Corrupt(format!("unknown feed consumer {name:?}")))
}

fn epochs(epochs: &[Epoch]) -> Vec<i64> {
    epochs.iter().map(|epoch| i64::from(epoch.0)).collect()
}

fn seq(seq: Seq) -> i64 {
    i64::from(seq.0)
}

/// An unguessable token: `prefix` and 256 bits from the operating system's CSPRNG, in hex.
fn random_token(prefix: &str) -> Result<String, Error> {
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|err| Error::Random(err.to_string()))?;
    let mut token = String::with_capacity(prefix.len() + 64);
    token.push_str(prefix);
    for byte in random {
        token.push_str(&format!("{byte:02x}"));
    }
    Ok(token)
}

fn fenced(lease: &Lease) -> StoreError {
    StoreError::Fenced {
        run_id: lease.run.id,
        fence: lease.fence,
    }
}

/// A call with a limit of 0 does nothing, and opens no transaction.
fn nothing(limit: usize) -> bool {
    limit == 0
}

/// A version referencing a test the store cannot keep for it.
fn missing_test(test: Digest) -> StoreError {
    StoreError::Conflict(format!(
        "the test {test} has no row, or its deletion has started: reserve it and write its blob \
         again before saving the version"
    ))
}

/// The tenants and digests of `keys`, sorted (the order rows are locked in), to bind as arrays.
fn sorted_keys(keys: &[(TenantId, Digest)]) -> (Vec<&str>, Vec<Vec<u8>>) {
    let sorted: BTreeSet<&(TenantId, Digest)> = keys.iter().collect();
    sorted
        .into_iter()
        .map(|(owner, digest)| (tenant(owner), bytes(*digest)))
        .unzip()
}

/// What the lock of a run reads of it.
struct LockedRun {
    status: RunStatus,
    fence: u64,
    history_len: u64,
    lease_owner: Option<String>,
    toolset: Digest,
}

/// Locks a run of this tenant until the end of the transaction. Everything the run owns is
/// written under this lock.
async fn lock_run(conn: &mut PgConnection, run: &RunRef) -> Result<Option<LockedRun>, Error> {
    let row = sqlx::query(
        "SELECT status, fence, history_len, lease_owner, toolset FROM runs \
         WHERE tenant = $1 AND id = $2 FOR NO KEY UPDATE",
    )
    .bind(tenant(&run.tenant))
    .bind(uuid(run.id))
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| -> Result<LockedRun, Error> {
        Ok(LockedRun {
            status: codec::parse_status(&row.text(0)?)?,
            fence: row.u64(1)?,
            history_len: row.u64(2)?,
            lease_owner: row.opt_text(3)?,
            toolset: row.digest(4)?,
        })
    })
    .transpose()
}

/// Locks runs by id, in id order, until the end of the transaction: callers that lock several
/// runs take them in the same order, so they never wait for each other in a cycle.
async fn lock_runs(conn: &mut PgConnection, runs: &BTreeSet<Uuid>) -> Result<(), Error> {
    if runs.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = runs.iter().copied().collect();
    sqlx::query("SELECT id FROM runs WHERE id = ANY($1) ORDER BY id FOR NO KEY UPDATE")
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?;
    Ok(())
}

/// Enqueues triggers, in order, in the inboxes of runs the transaction has locked or is about
/// to lock: the run row is always updated, so its lock orders this write with a commit that
/// consumes the inbox. A run points at its oldest item.
async fn push_inbox(
    conn: &mut PgConnection,
    items: &[(RunId, Trigger)],
    now: Timestamp,
) -> Result<(), Error> {
    if items.is_empty() {
        return Ok(());
    }
    let runs: Vec<Uuid> = items.iter().map(|(run, _)| uuid(*run)).collect();
    let events: Vec<String> = items
        .iter()
        .map(|(_, trigger)| codec::encode_trigger(trigger))
        .collect();
    sqlx::query(
        "WITH items AS ( \
             INSERT INTO inbox (run_id, event, received_at) \
             SELECT run_id, event, $3 FROM unnest($1::uuid[], $2::text[]) \
                 WITH ORDINALITY AS item(run_id, event, n) \
             ORDER BY n \
             RETURNING run_id, id), \
         heads AS (SELECT run_id, min(id) AS id FROM items GROUP BY run_id) \
         UPDATE runs SET inbox_head = COALESCE(runs.inbox_head, heads.id), \
             runnable_at = COALESCE(runs.runnable_at, $3) \
         FROM heads WHERE runs.id = heads.run_id",
    )
    .bind(runs)
    .bind(events)
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The runs a run's end reaches, among those that have not ended.
#[derive(Default)]
struct Reach {
    /// The run's watchers, which receive its end.
    watchers: BTreeSet<Uuid>,
    /// The runs the run watches attached, which its end cancels.
    attached: BTreeSet<Uuid>,
}

impl Reach {
    fn runs(&self) -> BTreeSet<Uuid> {
        self.watchers.union(&self.attached).copied().collect()
    }
}

/// The runs the end of `run` reaches and that have not ended: its watchers when it has an end to
/// give them, and the runs it watches attached. Read without locks: a status read is only stable
/// for a run the transaction has locked.
async fn reach_of_end(
    conn: &mut PgConnection,
    run: Uuid,
    with_watchers: bool,
) -> Result<Reach, Error> {
    let rows = sqlx::query(concat!(
        "SELECT w.watcher, false FROM run_watchers w JOIN runs r ON r.id = w.watcher \
         WHERE w.run_id = $1 AND $2::boolean AND r.status NOT IN ",
        terminal!(),
        " UNION ALL \
         SELECT w.run_id, true FROM run_watchers w JOIN runs r ON r.id = w.run_id \
         WHERE w.watcher = $1 AND w.attached AND r.status NOT IN ",
        terminal!()
    ))
    .bind(run)
    .bind(with_watchers)
    .fetch_all(&mut *conn)
    .await?;
    let mut reach = Reach::default();
    for row in &rows {
        let id = uuid(row.run_id(0)?);
        if row.boolean(1)? {
            reach.attached.insert(id);
        } else {
            reach.watchers.insert(id);
        }
    }
    Ok(reach)
}

/// What the end of a run does to the runs around it, in its terminal commit: the end (when it
/// has one) to each of its watchers that has not ended, as the signal `rewind:ended:<run>` under
/// that name as idempotency key, and a cancellation to each run it watches attached that has not
/// ended. `locked` are the runs the commit locked with its run; a run watched since then (started
/// by this run meanwhile) is locked now, out of id order: a deadlock this rare race may cause is
/// retried.
async fn propagate_end(
    conn: &mut PgConnection,
    run_id: RunId,
    end: Option<&JsonValue>,
    mut locked: BTreeSet<Uuid>,
    now: Timestamp,
) -> Result<(), Error> {
    let id = uuid(run_id);
    let reach = loop {
        let reach = reach_of_end(conn, id, end.is_some()).await?;
        let unlocked: BTreeSet<Uuid> = reach.runs().difference(&locked).copied().collect();
        if unlocked.is_empty() {
            break reach;
        }
        // Their status is read again once they are locked.
        lock_runs(conn, &unlocked).await?;
        locked.extend(unlocked);
    };
    let mut items: Vec<(RunId, Trigger)> = Vec::new();
    if let Some(end) = end
        && !reach.watchers.is_empty()
    {
        let name = end_signal_name(run_id);
        // A watcher whose inbox already received this end (its key is known) does not again.
        let mut fresh: Vec<Uuid> = sqlx::query_scalar(
            "INSERT INTO signal_keys (run_id, key) \
             SELECT watcher, $2 FROM unnest($1::uuid[]) AS w(watcher) \
             ON CONFLICT DO NOTHING RETURNING run_id",
        )
        .bind(reach.watchers.iter().copied().collect::<Vec<_>>())
        .bind(name.as_str())
        .fetch_all(&mut *conn)
        .await?;
        fresh.sort_unstable();
        for watcher in fresh {
            let ended = Completion::SignalReceived {
                name: name.clone(),
                payload: Payload::Value(end.clone()),
            };
            items.push((RunId::from_uuid(watcher), Trigger::Completion(ended)));
        }
    }
    for attached in &reach.attached {
        let cancel = Completion::CancelRequested {
            reason: starter_ended(run_id),
        };
        items.push((RunId::from_uuid(*attached), Trigger::Completion(cancel)));
    }
    push_inbox(conn, &items, now).await
}

/// The task holding `token`: a claimed task, or a deferred one.
async fn task_by_token(
    conn: &mut PgConnection,
    token: &TaskToken,
) -> Result<Option<TaskRow>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        task_columns!(),
        " FROM tool_tasks WHERE token = $1"
    ))
    .bind(token.0.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| task_from_row(&row)).transpose()
}

/// The task `seq` of a run of this tenant.
async fn task_by_ref(conn: &mut PgConnection, task: &TaskRef) -> Result<Option<TaskRow>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        task_columns!(),
        " FROM tool_tasks WHERE run_id = $1 AND seq = $2 AND tenant = $3"
    ))
    .bind(uuid(task.run.id))
    .bind(seq(task.seq))
    .bind(tenant(&task.run.tenant))
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| task_from_row(&row)).transpose()
}

/// What an update of a task that did not apply means: the task is gone, or in another state.
fn not_applied(found: Option<TaskRow>) -> TaskUpdate {
    match found {
        None => TaskUpdate::NotFound,
        Some(found) => TaskUpdate::InvalidState(found.task.state),
    }
}

/// Takes an advisory lock for the transaction on each key that no other transaction holds, and
/// returns the keys it got. A claim decides on a concurrency limit only under its key's lock.
async fn try_lock_keys(conn: &mut PgConnection, keys: Vec<String>) -> Result<Vec<String>, Error> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT key FROM unnest($1::text[]) WITH ORDINALITY AS k(key, n) \
         WHERE pg_try_advisory_xact_lock(hashtextextended(key, 0)) ORDER BY n",
    )
    .bind(keys)
    .fetch_all(&mut *conn)
    .await?;
    rows.iter().map(|row| row.text(0)).collect()
}

/// The advisory lock of a run's tool. Parts are separated by U+001F, which Postgres text can
/// hold (unlike NUL) and names do not.
fn tool_key(run: RunId, tool: &str) -> String {
    format!("rewind tool\u{1f}{run}\u{1f}{tool}")
}

/// The advisory lock of an MCP server of a tenant, under one worker.
fn server_key(owner: &str, tenant_id: &TenantId, server: &str) -> String {
    format!("rewind server\u{1f}{owner}\u{1f}{tenant_id}\u{1f}{server}")
}

/// The advisory lock of a tenant's workflow, under which its new versions take their numbers.
fn workflow_key(tenant_id: &TenantId, name: &str) -> String {
    format!("rewind workflow\u{1f}{tenant_id}\u{1f}{name}")
}

/// The runs `acquire_runnable` may lease, as a condition on `runs r`, with its parameters from
/// `$base`: now, the epochs, and the scope's tenants when it lists some. `next` is the first
/// parameter after them.
struct Runnable {
    eligible: String,
    tenants: Option<Vec<String>>,
    next: usize,
}

impl Runnable {
    fn new(base: usize, scope: &TenantScope) -> Self {
        let (scope_clause, tenants) = scope_clause("r.tenant", scope, base + 2);
        let next = base + 2 + usize::from(tenants.is_some());
        let eligible = format!(
            "r.inbox_head IS NOT NULL AND r.status NOT IN {terminal} \
             AND r.quarantine_kind IS NULL AND (r.lease_until IS NULL OR r.lease_until <= ${now}) \
             AND r.epoch = ANY(${epochs}){scope_clause}",
            terminal = terminal!(),
            now = base,
            epochs = base + 1,
        );
        Self {
            eligible,
            tenants,
            next,
        }
    }

    /// Binds the condition's parameters, after those already bound.
    fn bind<'q, O>(&self, query: O, now: i64, epochs: &[i64]) -> O
    where
        O: Bind<'q>,
    {
        let query = query.bind_value(now).bind_value(epochs.to_vec());
        match &self.tenants {
            Some(tenants) => query.bind_value(tenants.clone()),
            None => query,
        }
    }
}

/// `bind` on both kinds of query, for [`Runnable::bind`].
trait Bind<'q>: Sized {
    fn bind_value<T>(self, value: T) -> Self
    where
        T: 'q + sqlx::Encode<'q, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send;
}

impl<'q> Bind<'q> for sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    fn bind_value<T>(self, value: T) -> Self
    where
        T: 'q + sqlx::Encode<'q, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send,
    {
        self.bind(value)
    }
}

impl<'q, O> Bind<'q>
    for sqlx::query::QueryScalar<'q, sqlx::Postgres, O, sqlx::postgres::PgArguments>
{
    fn bind_value<T>(self, value: T) -> Self
    where
        T: 'q + sqlx::Encode<'q, sqlx::Postgres> + sqlx::Type<sqlx::Postgres> + Send,
    {
        self.bind(value)
    }
}

/// Leases the run `pick` selects (`tenant, id` of one run locked with `SKIP LOCKED`) to activate
/// it, its fence and its attempts incremented: `$1` is the owner and `$2` the lease's end.
fn lease_statement(pick: &str) -> String {
    format!(
        "UPDATE runs SET fence = fence + 1, lease_owner = $1, lease_until = $2, \
             attempts = attempts + 1 \
         WHERE (tenant, id) = ({pick}) RETURNING tenant, id, fence"
    )
}

fn lease_from_row(row: &PgRow) -> Result<Lease, Error> {
    Ok(Lease {
        run: RunRef::new(row.tenant(0)?, row.run_id(1)?),
        fence: row.u64(2)?,
    })
}

/// The host-tool selection of a claim: tenant, toolset, tools, namespace, and the labels its
/// runs carry (JSON).
type HostSelection<'a> = (
    &'a TenantId,
    Option<Digest>,
    Vec<String>,
    Option<&'a str>,
    String,
);

/// What a claim knows of a concurrency slot: another claim is deciding on it, or this many
/// calls are in flight.
#[derive(Clone, Copy)]
enum Slot {
    Busy,
    InFlight(u32),
}

impl PostgresStore {
    // ---------------------------------------------------------------------------------------
    // One attempt of each method that runs a transaction. A method runs its attempt again after
    // a deadlock or a serialization failure.

    async fn create_run_once(
        &self,
        run: &NewRun,
        idempotency_key: Option<&str>,
    ) -> Result<CreateRun, Fail> {
        let mut tx = self.pool.begin().await?;
        let tenant_id = &run.run.tenant;
        // The tenant's lock serializes its starts: two starts never both see the last free
        // place of a quota, of the namespace or of the tenant.
        namespaces::lock_tenant(&mut tx, tenant_id).await?;
        let existing = |id: Uuid| {
            CreateRun::Created(Created {
                run: RunRef::new(tenant_id.clone(), RunId::from_uuid(id)),
                created: false,
            })
        };
        if let Some(key) = idempotency_key {
            let found: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM runs WHERE tenant = $1 AND namespace = $2 AND idempotency_key = $3",
            )
            .bind(tenant(tenant_id))
            .bind(run.namespace.as_str())
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(id) = found {
                return Ok(existing(id));
            }
        }
        let admitted = match namespaces::admit_run(&mut tx, run).await? {
            Ok(admitted) => admitted,
            Err(refused) => {
                // The namespace created and the alert of a refusal are kept.
                tx.commit().await?;
                return Ok(refused);
            }
        };
        // The handle's suffix, long enough to name no other run of the tenant whose id shares
        // its first 64 bits; under the tenant's lock, no other start takes it meanwhile.
        let (low, high) = suffix_neighbours(run.run.id);
        let near: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM runs WHERE tenant = $1 AND id BETWEEN $2 AND $3")
                .bind(tenant(tenant_id))
                .bind(uuid(low))
                .bind(uuid(high))
                .fetch_all(&mut *tx)
                .await?;
        let suffix_len = suffix_len_apart(run.run.id, near.into_iter().map(RunId::from_uuid));
        let trace = run.metadata.trace_context.as_ref();
        let parent = run.metadata.parent;
        let workflow = run.metadata.workflow.as_ref();
        let started_by = run.metadata.started_by;
        // A run id already used, in any tenant, violates `runs`' keys: a conflict.
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO runs (tenant, id, script, toolset, status, epoch, expires_at, labels, \
             traceparent, tracestate, parent, parent_at, idempotency_key, created_at, updated_at, \
             workflow, workflow_version, started_by, started_seq, detached, scope, namespace, \
             secrets, origin, suffix_len) \
             VALUES ($1, $2, $3, $4, 'pending', $5, $6, $7::jsonb, $8, $9, $10, $11, $12, $13, $13, \
                 $14, $15, $16, $17, $18, $19::jsonb, $20, $21::jsonb, $22, $23) \
             ON CONFLICT (tenant, namespace, idempotency_key) DO NOTHING RETURNING id",
        )
        .bind(tenant(tenant_id))
        .bind(uuid(run.run.id))
        .bind(bytes(run.started.script))
        .bind(bytes(run.started.toolset))
        .bind(i64::from(run.started.epoch.0))
        .bind(ts(run.started.expires_at))
        .bind(encode_labels(&run.metadata.labels))
        .bind(trace.map(|trace| trace.traceparent.as_str()))
        .bind(trace.and_then(|trace| trace.tracestate.as_deref()))
        .bind(parent.map(|parent| uuid(parent.run)))
        .bind(parent.and_then(|parent| parent.at).map(int))
        .bind(idempotency_key)
        .bind(ts(run.created_at))
        .bind(workflow.map(|workflow| workflow.name.as_str()))
        .bind(workflow.map(|workflow| bytes(workflow.version)))
        .bind(started_by.map(|started_by| uuid(started_by.run)))
        .bind(started_by.map(|started_by| seq(started_by.seq)))
        .bind(started_by.map(|started_by| started_by.detached))
        .bind(encode_labels(&run.metadata.scope))
        .bind(run.namespace.as_str())
        .bind(codec::encode_attachments(&run.metadata.secrets))
        .bind(run.origin.as_str())
        .bind(i16::try_from(suffix_len).unwrap_or(i16::MAX))
        .fetch_optional(&mut *tx)
        .await?;
        if inserted.is_none() {
            // A concurrent start with the same key committed first.
            let id: Uuid = sqlx::query_scalar(
                "SELECT id FROM runs WHERE tenant = $1 AND namespace = $2 AND idempotency_key = $3",
            )
            .bind(tenant(tenant_id))
            .bind(run.namespace.as_str())
            .bind(idempotency_key)
            .fetch_one(&mut *tx)
            .await?;
            return Ok(existing(id));
        }
        // The run that started this one watches it: it receives its end, and cancels it when it
        // ends first unless it started it detached.
        if let Some(started_by) = started_by {
            sqlx::query(
                "INSERT INTO run_watchers (run_id, watcher, seq, attached) VALUES ($1, $2, $3, $4)",
            )
            .bind(uuid(run.run.id))
            .bind(uuid(started_by.run))
            .bind(seq(started_by.seq))
            .bind(!started_by.detached)
            .execute(&mut *tx)
            .await?;
        }
        let started = Trigger::RunStarted(run.started.clone());
        push_inbox(&mut tx, &[(run.run.id, started)], run.created_at).await?;
        namespaces::count_run(&mut tx, run, admitted).await?;
        tx.commit().await?;
        Ok(CreateRun::Created(Created {
            run: run.run.clone(),
            created: true,
        }))
    }

    async fn enqueue_once(
        &self,
        run: &RunRef,
        completion: &Completion,
        idempotency_key: Option<&str>,
        now: Timestamp,
    ) -> Result<Enqueued, Fail> {
        let mut tx = self.pool.begin().await?;
        let Some(locked) = lock_run(&mut tx, run).await? else {
            return Err(StoreError::RunNotFound(run.id).into());
        };
        if locked.status.is_terminal() {
            return Ok(Enqueued::RunEnded);
        }
        if let Some(key) = idempotency_key {
            let inserted = sqlx::query(
                "INSERT INTO signal_keys (run_id, key) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(uuid(run.id))
            .bind(key)
            .execute(&mut *tx)
            .await?;
            if inserted.rows_affected() == 0 {
                return Ok(Enqueued::Duplicate);
            }
        }
        let trigger = Trigger::Completion(completion.clone());
        push_inbox(&mut tx, &[(run.id, trigger)], now).await?;
        tx.commit().await?;
        Ok(Enqueued::Accepted)
    }

    async fn watch_run_once(
        &self,
        run: &RunRef,
        watcher: RunId,
        seq_started: Seq,
    ) -> Result<Option<Watched>, Fail> {
        let mut tx = self.pool.begin().await?;
        // The run's lock orders the watch with its terminal commit: a watcher added before it
        // receives the end, one added after learns that the run has ended.
        let Some(locked) = lock_run(&mut tx, run).await? else {
            return Ok(None);
        };
        sqlx::query(
            "INSERT INTO run_watchers (run_id, watcher, seq, attached) \
             VALUES ($1, $2, $3, false) ON CONFLICT DO NOTHING",
        )
        .bind(uuid(run.id))
        .bind(uuid(watcher))
        .bind(seq(seq_started))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(Watched {
            ended: locked.status.is_terminal(),
        }))
    }

    async fn commit_once(
        &self,
        lease: &Lease,
        activation: &Activation,
        now: Timestamp,
    ) -> Result<(), Fail> {
        let mut tx = self.pool.begin().await?;
        let id = uuid(lease.run.id);
        let terminal = activation.status.is_terminal();
        // A terminal commit enqueues into the runs its end reaches: they are locked with the run,
        // all by id, so that two related runs ending at once never wait for each other in a
        // cycle. A run already ended is not locked: it receives nothing.
        let mut locked = BTreeSet::new();
        if terminal {
            locked = reach_of_end(&mut tx, id, activation.end.is_some())
                .await?
                .runs();
            if !locked.is_empty() {
                locked.insert(id);
                lock_runs(&mut tx, &locked).await?;
            }
        }
        let Some(run) = lock_run(&mut tx, &lease.run).await? else {
            return Err(StoreError::RunNotFound(lease.run.id).into());
        };
        if run.fence != lease.fence {
            return Err(fenced(lease).into());
        }
        let from_idx = run.history_len;
        for (offset, entry) in activation.entries.iter().enumerate() {
            let expected = from_idx + offset as u64;
            if entry.index != expected {
                return Err(StoreError::Conflict(format!(
                    "entry index {} does not continue the history of run {} \
                     (next index {expected})",
                    entry.index, lease.run.id
                ))
                .into());
            }
        }
        if activation
            .payloads
            .iter()
            .any(|payload| payload.value.is_none())
        {
            check_reserved(&mut tx, &lease.run, &activation.payloads).await?;
        }
        let to_idx = from_idx + activation.entries.len() as u64;
        let last = activation.entries.last().map(|entry| entry.recorded_at);

        // The entries, the feed items, the consumed inbox items and the run, at once.
        let indexes: Vec<i64> = activation.entries.iter().map(|e| int(e.index)).collect();
        let recorded: Vec<i64> = activation
            .entries
            .iter()
            .map(|e| ts(e.recorded_at))
            .collect();
        let events: Vec<String> = activation
            .entries
            .iter()
            .map(|entry| rewind_format::encode_event(&entry.event))
            .collect();
        let consumers: Vec<&str> = if activation.entries.is_empty() {
            Vec::new()
        } else {
            FeedConsumer::ALL.iter().map(|c| c.as_str()).collect()
        };
        let consumed: Vec<i64> = activation.consumed.iter().map(|id| int(*id)).collect();
        sqlx::query(
            "WITH entries AS ( \
                 INSERT INTO history (run_id, idx, recorded_at, event) \
                 SELECT $1, idx, recorded_at, event \
                 FROM unnest($3::bigint[], $4::bigint[], $5::text[]) AS e(idx, recorded_at, event)), \
             feed_items AS ( \
                 INSERT INTO feed (consumer, tenant, run_id, fence, from_idx, to_idx) \
                 SELECT consumer, $2, $1, $6, $7, $8 \
                 FROM unnest($9::text[]) WITH ORDINALITY AS c(consumer, n) ORDER BY n), \
             consumed AS (DELETE FROM inbox WHERE run_id = $1 AND id = ANY($10)), \
             head AS (SELECT id, received_at FROM inbox \
                      WHERE run_id = $1 AND NOT (id = ANY($10)) ORDER BY id LIMIT 1) \
             UPDATE runs SET history_len = $8, status = $11, failures = 0, attempts = 0, \
                 updated_at = COALESCE($12, updated_at), ended_at = COALESCE($13, ended_at), \
                 sticky_worker = COALESCE($14, sticky_worker), \
                 inbox_head = (SELECT id FROM head), runnable_at = (SELECT received_at FROM head) \
             WHERE tenant = $2 AND id = $1",
        )
        .bind(id)
        .bind(tenant(&lease.run.tenant))
        .bind(indexes)
        .bind(recorded)
        .bind(events)
        .bind(int(lease.fence))
        .bind(int(from_idx))
        .bind(int(to_idx))
        .bind(consumers)
        .bind(consumed)
        .bind(activation.status.as_str())
        .bind(opt_ts(last))
        .bind(opt_ts(last.filter(|_| terminal)))
        .bind(run.lease_owner.as_deref())
        .execute(&mut *tx)
        .await?;

        if !activation.tool_calls.is_empty() {
            insert_tool_tasks(&mut tx, &lease.run, run.toolset, activation, now).await?;
        }
        if !activation.timers.is_empty() {
            let seqs: Vec<i64> = activation.timers.iter().map(|t| seq(t.seq)).collect();
            let fire_at: Vec<i64> = activation.timers.iter().map(|t| ts(t.fire_at)).collect();
            sqlx::query(
                "INSERT INTO timers (run_id, seq, fire_at) \
                 SELECT $1, seq, fire_at FROM unnest($2::bigint[], $3::bigint[]) AS t(seq, fire_at) \
                 ON CONFLICT (run_id, seq) DO UPDATE SET fire_at = EXCLUDED.fire_at",
            )
            .bind(id)
            .bind(seqs)
            .bind(fire_at)
            .execute(&mut *tx)
            .await?;
        }
        // After the new timers: one started and cancelled in the same activation never fires.
        if !activation.cancelled_timers.is_empty() {
            let cancelled: Vec<i64> = activation
                .cancelled_timers
                .iter()
                .map(|s| seq(*s))
                .collect();
            sqlx::query("DELETE FROM timers WHERE run_id = $1 AND seq = ANY($2)")
                .bind(id)
                .bind(cancelled)
                .execute(&mut *tx)
                .await?;
        }
        if !activation.payloads.is_empty() {
            insert_payloads(&mut tx, &lease.run, &activation.payloads).await?;
        }
        if terminal {
            sqlx::query(
                "WITH timers_gone AS (DELETE FROM timers WHERE run_id = $1), \
                 tasks_gone AS (DELETE FROM tool_tasks WHERE run_id = $1), \
                 inbox_gone AS (DELETE FROM inbox WHERE run_id = $1), \
                 tokens_gone AS (DELETE FROM webhook_tokens WHERE run_id = $1) \
                 UPDATE runs SET quarantine_kind = NULL, quarantine_reason = NULL, \
                     quarantine_since = NULL, quarantine_retried_by = NULL, inbox_head = NULL, \
                     runnable_at = NULL \
                 WHERE id = $1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            propagate_end(&mut tx, lease.run.id, activation.end.as_ref(), locked, now).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn record_failure_once(
        &self,
        lease: &Lease,
        error: &str,
        now: Timestamp,
    ) -> Result<Failures, Fail> {
        let row = sqlx::query(
            "UPDATE runs SET failures = failures + 1, \
                 quarantine_kind = CASE WHEN failures + 1 >= $4 THEN 'failures' \
                                        ELSE quarantine_kind END, \
                 quarantine_reason = CASE WHEN failures + 1 >= $4 THEN $5 \
                                          ELSE quarantine_reason END, \
                 quarantine_since = CASE WHEN failures + 1 >= $4 THEN $6 \
                                         ELSE quarantine_since END, \
                 lease_owner = CASE WHEN failures + 1 >= $4 THEN NULL ELSE lease_owner END, \
                 lease_until = CASE WHEN failures + 1 >= $4 THEN NULL ELSE lease_until END \
             WHERE tenant = $1 AND id = $2 AND fence = $3 \
             RETURNING failures",
        )
        .bind(tenant(&lease.run.tenant))
        .bind(uuid(lease.run.id))
        .bind(int(lease.fence))
        .bind(i64::from(QUARANTINE_AFTER_FAILURES))
        .bind(error)
        .bind(ts(now))
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Err(self.lost_lease(lease).await.into());
        };
        let count = u32::try_from(row.u64(0)?).unwrap_or(u32::MAX);
        Ok(Failures {
            count,
            quarantined: count >= QUARANTINE_AFTER_FAILURES,
        })
    }

    /// Why a statement guarded by a lease's fence changed nothing: the run is gone, or the
    /// lease is lost.
    async fn lost_lease(&self, lease: &Lease) -> StoreError {
        let exists =
            sqlx::query_scalar::<_, i32>("SELECT 1 FROM runs WHERE tenant = $1 AND id = $2")
                .bind(tenant(&lease.run.tenant))
                .bind(uuid(lease.run.id))
                .fetch_optional(&self.pool)
                .await;
        match exists {
            Ok(Some(_)) => fenced(lease),
            Ok(None) => StoreError::RunNotFound(lease.run.id),
            Err(err) => Error::from(err).into(),
        }
    }

    async fn fire_due_timers_once(&self, now: Timestamp, max: usize) -> Result<usize, Fail> {
        // Read without locks; each run is then locked, by id, before its timers are deleted.
        let due = sqlx::query(
            "SELECT run_id FROM timers WHERE fire_at <= $1 ORDER BY fire_at, run_id, seq LIMIT $2",
        )
        .bind(ts(now))
        .bind(limit(max))
        .fetch_all(&self.pool)
        .await?;
        if due.is_empty() {
            return Ok(0);
        }
        let runs = due
            .iter()
            .map(|row| {
                row.opt_uuid(0)?
                    .ok_or_else(|| Error::Corrupt("a timer without run".into()))
            })
            .collect::<Result<BTreeSet<Uuid>, Error>>()?;
        let mut tx = self.pool.begin().await?;
        lock_runs(&mut tx, &runs).await?;
        // A timer fired or cancelled since it was read is gone: it is not fired again.
        let fired = sqlx::query(
            "DELETE FROM timers WHERE (run_id, seq) IN ( \
                 SELECT run_id, seq FROM timers WHERE run_id = ANY($1) AND fire_at <= $2 \
                 ORDER BY fire_at, run_id, seq LIMIT $3) \
             RETURNING run_id, seq, fire_at",
        )
        .bind(runs.iter().copied().collect::<Vec<_>>())
        .bind(ts(now))
        .bind(limit(max))
        .fetch_all(&mut *tx)
        .await?;
        let mut fired = fired
            .iter()
            .map(|row| Ok((row.i64(2)?, row.run_id(0)?, Seq(row.u32(1)?))))
            .collect::<Result<Vec<_>, Error>>()?;
        fired.sort_unstable();
        let items: Vec<(RunId, Trigger)> = fired
            .iter()
            .map(|(_, run, seq)| {
                (
                    *run,
                    Trigger::Completion(Completion::TimerFired { seq: *seq }),
                )
            })
            .collect();
        push_inbox(&mut tx, &items, now).await?;
        tx.commit().await?;
        Ok(items.len())
    }

    async fn expire_due_runs_once(&self, now: Timestamp, max: usize) -> Result<ExpiredRuns, Fail> {
        let mut tx = self.pool.begin().await?;
        // Skipping locked runs never waits, so it never waits in a cycle with callers that lock
        // runs by id; a run skipped now expires at the next call.
        let due = sqlx::query(concat!(
            "SELECT tenant, id, quarantine_kind IS NOT NULL FROM runs WHERE status NOT IN ",
            terminal!(),
            " AND expires_at <= $1 AND (quarantine_kind IS NOT NULL OR NOT expiry_enqueued) \
             ORDER BY expires_at, id LIMIT $2 FOR NO KEY UPDATE SKIP LOCKED"
        ))
        .bind(ts(now))
        .bind(limit(max))
        .fetch_all(&mut *tx)
        .await?;
        let mut expired = ExpiredRuns::default();
        for row in &due {
            let run = RunRef::new(row.tenant(0)?, row.run_id(1)?);
            if row.boolean(2)? {
                expired.quarantined.push(run);
            } else {
                expired.cancelled.push(run);
            }
        }
        if !expired.cancelled.is_empty() {
            let ids: Vec<Uuid> = expired.cancelled.iter().map(|run| uuid(run.id)).collect();
            sqlx::query("UPDATE runs SET expiry_enqueued = true WHERE id = ANY($1)")
                .bind(ids)
                .execute(&mut *tx)
                .await?;
            let items: Vec<(RunId, Trigger)> = expired
                .cancelled
                .iter()
                .map(|run| {
                    let cancel = Completion::CancelRequested {
                        reason: CancelReason::Ttl,
                    };
                    (run.id, Trigger::Completion(cancel))
                })
                .collect();
            push_inbox(&mut tx, &items, now).await?;
        }
        tx.commit().await?;
        Ok(expired)
    }

    async fn claim_once(
        &self,
        owner: &str,
        filter: &TaskFilter,
        now: Timestamp,
        max: usize,
    ) -> Result<Vec<ClaimedTask>, Fail> {
        let mut tx = self.pool.begin().await?;
        // $1 now, $2 page size, $3–$5 the last task read; the filter's values from $6.
        let mut selection = String::new();
        let mut max_attempts = None;
        let mut scope_tenants = None;
        let mut host: Option<HostSelection<'_>> = None;
        match filter {
            TaskFilter::All => {}
            TaskFilter::Dispatched { tenants } => {
                selection.push_str(" AND dispatched");
                let (clause, bound) = scope_clause("tenant", tenants, 6);
                selection.push_str(&clause);
                scope_tenants = bound;
            }
            TaskFilter::Interrupted {
                tenants,
                max_attempts: attempts,
            } => {
                // Due and claimed: the claim expired.
                selection.push_str(
                    " AND NOT dispatched AND state = 'claimed' \
                     AND (on_interruption = 'at_most_once' OR attempt >= $6)",
                );
                max_attempts = Some(i64::from(*attempts));
                let (clause, bound) = scope_clause("tenant", tenants, 7);
                selection.push_str(&clause);
                scope_tenants = bound;
            }
            TaskFilter::Host {
                tenant: host_tenant,
                namespace,
                labels,
                toolset,
                tools,
            } => {
                // A run's namespace and labels are read by its key, a task at a time: the due
                // tasks are few, and a label's runs may be many.
                selection.push_str(
                    " AND NOT dispatched AND tenant = $6 \
                     AND ($7::bytea IS NULL OR toolset = $7) \
                     AND (cardinality($8::text[]) = 0 OR tool = ANY($8)) \
                     AND (($9::text IS NULL AND $10::jsonb = '{}'::jsonb) OR EXISTS \
                         (SELECT 1 FROM runs WHERE runs.tenant = $6 \
                          AND runs.id = tool_tasks.run_id \
                          AND ($9::text IS NULL OR runs.namespace = $9) \
                          AND runs.labels @> $10::jsonb))",
                );
                host = Some((
                    host_tenant,
                    *toolset,
                    tools.iter().cloned().collect(),
                    namespace.as_ref().map(NamespaceName::as_str),
                    encode_labels(labels),
                ));
            }
        }
        // The pages after the first start after the last task read, `$3`–`$5`.
        let first_page = format!(
            "SELECT {columns} FROM tool_tasks WHERE due_at <= $1{selection} \
             ORDER BY due_at, run_id, seq LIMIT $2 FOR UPDATE SKIP LOCKED",
            columns = task_columns!(),
        );
        let next_page = format!(
            "SELECT {columns} FROM tool_tasks WHERE due_at <= $1{selection} \
             AND (due_at, run_id, seq) > ($3, $4, $5) \
             ORDER BY due_at, run_id, seq LIMIT $2 FOR UPDATE SKIP LOCKED",
            columns = task_columns!(),
        );
        // Due tasks are read a page at a time, in the order they became due: a task without a
        // free concurrency slot is skipped, so a claim may read past `max`.
        let page = max.saturating_mul(2).clamp(16, 256);
        let mut per_tool: HashMap<String, Slot> = HashMap::new();
        let mut per_server: HashMap<String, Slot> = HashMap::new();
        let mut claimed: Vec<ClaimedTask> = Vec::new();
        let mut after: Option<(i64, Uuid, i64)> = None;
        'pages: loop {
            let query = if after.is_some() {
                &next_page
            } else {
                &first_page
            };
            let mut statement = sqlx::query(query)
                .bind(ts(now))
                .bind(limit(page))
                .bind(after.map(|(due_at, ..)| due_at))
                .bind(after.map(|(_, run, _)| run))
                .bind(after.map(|(.., seq)| seq));
            if let Some(attempts) = max_attempts {
                statement = statement.bind(attempts);
            }
            if let Some(tenants) = &scope_tenants {
                statement = statement.bind(tenants.clone());
            }
            if let Some((host_tenant, toolset, tools, namespace, labels)) = &host {
                statement = statement
                    .bind(tenant(host_tenant))
                    .bind(toolset.map(bytes))
                    .bind(tools.clone())
                    .bind(*namespace)
                    .bind(labels.as_str());
            }
            let rows = statement.fetch_all(&mut *tx).await?;
            let tasks = rows
                .iter()
                .map(task_from_row)
                .collect::<Result<Vec<TaskRow>, Error>>()?;
            if let Some(last) = tasks.last() {
                let due_at = last
                    .due_at
                    .ok_or_else(|| Error::Corrupt("a due task without due_at".into()))?;
                after = Some((due_at, uuid(last.task.task.run.id), seq(last.task.task.seq)));
            }

            // The slots this page needs that no earlier page counted: locked, then counted.
            let tool_keys: BTreeSet<String> = tasks
                .iter()
                .map(|row| tool_key(row.task.task.run.id, &row.task.tool))
                .filter(|key| !per_tool.contains_key(key))
                .collect();
            let server_keys: BTreeSet<String> = tasks
                .iter()
                .filter(|row| row.server_concurrency.is_some())
                .filter_map(|row| {
                    let server = row.mcp_server.as_deref()?;
                    Some(server_key(owner, &row.task.task.run.tenant, server))
                })
                .filter(|key| !per_server.contains_key(key))
                .collect();
            Self::count_tool_slots(&mut tx, &tasks, tool_keys, &mut per_tool, now).await?;
            Self::count_server_slots(&mut tx, owner, &tasks, server_keys, &mut per_server, now)
                .await?;

            for row in &tasks {
                if claimed.len() >= max {
                    break 'pages;
                }
                let key = tool_key(row.task.task.run.id, &row.task.tool);
                let Some(Slot::InFlight(in_flight)) = per_tool.get(&key).copied() else {
                    continue;
                };
                if in_flight >= row.task.policy.concurrency {
                    continue;
                }
                let server = match (&row.mcp_server, row.server_concurrency) {
                    (Some(server), Some(max_in_flight)) => {
                        let key = server_key(owner, &row.task.task.run.tenant, server);
                        let Some(Slot::InFlight(in_flight)) = per_server.get(&key).copied() else {
                            continue;
                        };
                        if in_flight >= max_in_flight {
                            continue;
                        }
                        Some(key)
                    }
                    _ => None,
                };
                per_tool.insert(key, Slot::InFlight(in_flight + 1));
                if let Some(key) = server
                    && let Some(Slot::InFlight(count)) = per_server.get_mut(&key)
                {
                    *count += 1;
                }

                let mut task = row.task.clone();
                let previous_claim_expired = matches!(task.state, TaskState::Claimed { .. });
                task.attempt += 1;
                task.state = TaskState::Claimed {
                    owner: owner.to_owned(),
                    until: now.saturating_add(task.policy.heartbeat_timeout),
                };
                claimed.push(ClaimedTask {
                    task,
                    token: TaskToken(random_token("rewind_tt_")?),
                    previous_claim_expired,
                });
            }
            if rows.len() < page {
                break;
            }
        }

        if !claimed.is_empty() {
            let runs: Vec<Uuid> = claimed.iter().map(|c| uuid(c.task.task.run.id)).collect();
            let seqs: Vec<i64> = claimed.iter().map(|c| seq(c.task.task.seq)).collect();
            let tokens: Vec<&str> = claimed.iter().map(|c| c.token.0.as_str()).collect();
            let untils: Vec<i64> = claimed
                .iter()
                .map(|c| match &c.task.state {
                    TaskState::Claimed { until, .. } => ts(*until),
                    _ => ts(now),
                })
                .collect();
            sqlx::query(
                "UPDATE tool_tasks t SET state = 'claimed', token = c.token, \
                     attempt = t.attempt + 1, lease_owner = $5, lease_until = c.until, \
                     due_at = c.until, next_attempt_at = NULL, parked_kind = NULL, \
                     parked_reason = NULL \
                 FROM unnest($1::uuid[], $2::bigint[], $3::text[], $4::bigint[]) \
                     AS c(run_id, seq, token, until) \
                 WHERE t.run_id = c.run_id AND t.seq = c.seq",
            )
            .bind(runs)
            .bind(seqs)
            .bind(tokens)
            .bind(untils)
            .bind(owner)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(claimed)
    }

    /// Locks the per-tool slots of `keys` and counts their calls in flight: claimed tasks whose
    /// claim has not expired, and deferred tasks. A slot another claim holds is busy.
    async fn count_tool_slots(
        conn: &mut PgConnection,
        tasks: &[TaskRow],
        keys: BTreeSet<String>,
        slots: &mut HashMap<String, Slot>,
        now: Timestamp,
    ) -> Result<(), Error> {
        if keys.is_empty() {
            return Ok(());
        }
        let locked: BTreeSet<String> = try_lock_keys(conn, keys.iter().cloned().collect())
            .await?
            .into_iter()
            .collect();
        let mut pairs: BTreeMap<String, (Uuid, String)> = BTreeMap::new();
        for row in tasks {
            let key = tool_key(row.task.task.run.id, &row.task.tool);
            if locked.contains(&key) {
                pairs.insert(key, (uuid(row.task.task.run.id), row.task.tool.clone()));
            }
        }
        for key in &keys {
            slots.insert(
                key.clone(),
                if locked.contains(key) {
                    Slot::InFlight(0)
                } else {
                    Slot::Busy
                },
            );
        }
        if pairs.is_empty() {
            return Ok(());
        }
        let runs: Vec<Uuid> = pairs.values().map(|(run, _)| *run).collect();
        let tools: Vec<String> = pairs.values().map(|(_, tool)| tool.clone()).collect();
        let counts = sqlx::query(
            "SELECT t.run_id, t.tool, count(*) FROM tool_tasks t \
             JOIN unnest($1::uuid[], $2::text[]) AS k(run_id, tool) \
                 ON t.run_id = k.run_id AND t.tool = k.tool \
             WHERE (t.state = 'claimed' AND t.lease_until > $3) OR t.state = 'deferred' \
             GROUP BY t.run_id, t.tool",
        )
        .bind(runs)
        .bind(tools)
        .bind(ts(now))
        .fetch_all(&mut *conn)
        .await?;
        for row in &counts {
            let key = tool_key(row.run_id(0)?, &row.text(1)?);
            let count = u32::try_from(row.u64(2)?).unwrap_or(u32::MAX);
            slots.insert(key, Slot::InFlight(count));
        }
        Ok(())
    }

    /// Locks the per-server slots of `keys` under `owner` and counts their calls in flight:
    /// claims of the owner, not expired, of the server's tenant and name.
    async fn count_server_slots(
        conn: &mut PgConnection,
        owner: &str,
        tasks: &[TaskRow],
        keys: BTreeSet<String>,
        slots: &mut HashMap<String, Slot>,
        now: Timestamp,
    ) -> Result<(), Error> {
        if keys.is_empty() {
            return Ok(());
        }
        let locked: BTreeSet<String> = try_lock_keys(conn, keys.iter().cloned().collect())
            .await?
            .into_iter()
            .collect();
        let mut pairs: BTreeMap<String, (String, String)> = BTreeMap::new();
        for row in tasks {
            if let Some(server) = &row.mcp_server {
                let key = server_key(owner, &row.task.task.run.tenant, server);
                if locked.contains(&key) {
                    let tenant_id = row.task.task.run.tenant.as_str().to_owned();
                    pairs.insert(key, (tenant_id, server.clone()));
                }
            }
        }
        for key in &keys {
            slots.insert(
                key.clone(),
                if locked.contains(key) {
                    Slot::InFlight(0)
                } else {
                    Slot::Busy
                },
            );
        }
        if pairs.is_empty() {
            return Ok(());
        }
        let tenants: Vec<String> = pairs.values().map(|(t, _)| t.clone()).collect();
        let servers: Vec<String> = pairs.values().map(|(_, s)| s.clone()).collect();
        let counts = sqlx::query(
            "SELECT t.tenant, t.mcp_server, count(*) FROM tool_tasks t \
             JOIN unnest($2::text[], $3::text[]) AS k(tenant, server) \
                 ON t.tenant = k.tenant AND t.mcp_server = k.server \
             WHERE t.lease_owner = $1 AND t.state = 'claimed' AND t.lease_until > $4 \
             GROUP BY t.tenant, t.mcp_server",
        )
        .bind(owner)
        .bind(tenants)
        .bind(servers)
        .bind(ts(now))
        .fetch_all(&mut *conn)
        .await?;
        for row in &counts {
            let key = server_key(owner, &row.tenant(0)?, &row.text(1)?);
            let count = u32::try_from(row.u64(2)?).unwrap_or(u32::MAX);
            slots.insert(key, Slot::InFlight(count));
        }
        Ok(())
    }

    /// Enqueues the outcomes of tasks just deleted, in runs the transaction has locked.
    async fn finish_tasks(
        conn: &mut PgConnection,
        done: &[(RunId, Seq, ToolOutcome)],
        now: Timestamp,
    ) -> Result<(), Error> {
        let items: Vec<(RunId, Trigger)> = done
            .iter()
            .map(|(run, seq, outcome)| {
                (
                    *run,
                    Trigger::Completion(outcome.clone().into_completion(*seq)),
                )
            })
            .collect();
        push_inbox(conn, &items, now).await
    }

    async fn complete_once(
        &self,
        token: &TaskToken,
        outcome: &ToolOutcome,
        now: Timestamp,
    ) -> Result<TaskUpdate, Fail> {
        let mut tx = self.pool.begin().await?;
        let task: Option<(Uuid, String)> =
            sqlx::query_as("SELECT run_id, tenant FROM tool_tasks WHERE token = $1")
                .bind(token.0.as_str())
                .fetch_optional(&mut *tx)
                .await?;
        let Some((run_id, tenant_id)) = task else {
            return Ok(TaskUpdate::NotFound);
        };
        let tenant_id = TenantId::new(tenant_id).map_err(|err| Error::Corrupt(err.to_string()))?;
        let run = RunRef::new(tenant_id, RunId::from_uuid(run_id));
        if lock_run(&mut tx, &run).await?.is_none() {
            return Ok(TaskUpdate::NotFound);
        }
        // Claimed again, or ended, while the run was being locked: the token holds nothing.
        let seq_done: Option<i64> = sqlx::query_scalar(
            "DELETE FROM tool_tasks WHERE run_id = $1 AND token = $2 RETURNING seq",
        )
        .bind(run_id)
        .bind(token.0.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(seq_done) = seq_done else {
            return Ok(TaskUpdate::NotFound);
        };
        let seq_done =
            Seq(u32::try_from(seq_done).map_err(|_| Error::Corrupt(format!("seq {seq_done}")))?);
        Self::finish_tasks(&mut tx, &[(run.id, seq_done, outcome.clone())], now).await?;
        tx.commit().await?;
        Ok(TaskUpdate::Applied)
    }

    async fn reject_once(
        &self,
        task: &TaskRef,
        failure: &ToolFailure,
        now: Timestamp,
    ) -> Result<TaskUpdate, Fail> {
        let mut tx = self.pool.begin().await?;
        if lock_run(&mut tx, &task.run).await?.is_none() {
            return Ok(TaskUpdate::NotFound);
        }
        let deleted = sqlx::query(
            "DELETE FROM tool_tasks WHERE run_id = $1 AND seq = $2 AND tenant = $3 \
             AND state IN ('approval', 'parked')",
        )
        .bind(uuid(task.run.id))
        .bind(seq(task.seq))
        .bind(tenant(&task.run.tenant))
        .execute(&mut *tx)
        .await?;
        if deleted.rows_affected() == 0 {
            return Ok(not_applied(task_by_ref(&mut tx, task).await?));
        }
        let outcome = ToolOutcome::Failure(failure.clone());
        Self::finish_tasks(&mut tx, &[(task.run.id, task.seq, outcome)], now).await?;
        tx.commit().await?;
        Ok(TaskUpdate::Applied)
    }

    async fn resume_parked_once(&self, scope: &ParkedScope, now: Timestamp) -> Result<usize, Fail> {
        let ParkedScope::Secret {
            tenant: tenant_id,
            secret,
        } = scope;
        let (attached, namespace) = attached_as(secret);
        // The tasks parked by a refusal or by a missing secret (`credentials_expired` before
        // version 9), whose run attaches the secret for the task's server or API.
        let parked = "t.tenant = $1 AND t.state = 'parked' \
             AND t.parked_kind IN ('credentials_refused', 'secret_missing', 'credentials_expired') \
             AND r.tenant = t.tenant AND r.id = t.run_id \
             AND r.secrets ->> t.mcp_server = $2 AND ($3::text IS NULL OR r.namespace = $3)";
        let mut tx = self.pool.begin().await?;
        let runs: Vec<Uuid> = sqlx::query_scalar(&format!(
            "SELECT DISTINCT t.run_id FROM tool_tasks t, runs r WHERE {parked}"
        ))
        .bind(tenant(tenant_id))
        .bind(attached.as_str())
        .bind(namespace)
        .fetch_all(&mut *tx)
        .await?;
        if runs.is_empty() {
            return Ok(0);
        }
        // The tasks of the runs locked only: a run's row is locked before its tasks, in the
        // order a commit locks them.
        lock_runs(&mut tx, &runs.iter().copied().collect()).await?;
        let resumed = sqlx::query(&format!(
            "UPDATE tool_tasks t SET state = 'ready', token = NULL, next_attempt_at = $4, \
                 due_at = $4, lease_owner = NULL, lease_until = NULL, parked_kind = NULL, \
                 parked_reason = NULL \
             FROM runs r WHERE {parked} AND t.run_id = ANY($5)"
        ))
        .bind(tenant(tenant_id))
        .bind(attached.as_str())
        .bind(namespace)
        .bind(ts(now))
        .bind(&runs)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(usize::try_from(resumed.rows_affected()).unwrap_or(usize::MAX))
    }

    async fn expire_deferred_once(&self, now: Timestamp, max: usize) -> Result<usize, Fail> {
        let due: Vec<Uuid> = sqlx::query_scalar(
            "SELECT run_id FROM tool_tasks WHERE state = 'deferred' AND lease_until <= $1 \
             ORDER BY lease_until, run_id, seq LIMIT $2",
        )
        .bind(ts(now))
        .bind(limit(max))
        .fetch_all(&self.pool)
        .await?;
        if due.is_empty() {
            return Ok(0);
        }
        let runs: BTreeSet<Uuid> = due.into_iter().collect();
        let mut tx = self.pool.begin().await?;
        lock_runs(&mut tx, &runs).await?;
        let ended = sqlx::query(
            "DELETE FROM tool_tasks WHERE (run_id, seq) IN ( \
                 SELECT run_id, seq FROM tool_tasks \
                 WHERE run_id = ANY($1) AND state = 'deferred' AND lease_until <= $2 \
                 ORDER BY lease_until, run_id, seq LIMIT $3) \
             RETURNING lease_until, run_id, seq",
        )
        .bind(runs.into_iter().collect::<Vec<_>>())
        .bind(ts(now))
        .bind(limit(max))
        .fetch_all(&mut *tx)
        .await?;
        let mut ended = ended
            .iter()
            .map(|row| Ok((row.i64(0)?, row.run_id(1)?, Seq(row.u32(2)?))))
            .collect::<Result<Vec<_>, Error>>()?;
        ended.sort_unstable();
        let done: Vec<(RunId, Seq, ToolOutcome)> = ended
            .iter()
            .map(|(_, run, seq)| (*run, *seq, ToolOutcome::Failure(deferred_timeout_failure())))
            .collect();
        Self::finish_tasks(&mut tx, &done, now).await?;
        tx.commit().await?;
        Ok(done.len())
    }

    async fn create_webhook_token_once(
        &self,
        run: &RunRef,
        name: &str,
    ) -> Result<WebhookToken, Fail> {
        let mut tx = self.pool.begin().await?;
        let Some(locked) = lock_run(&mut tx, run).await? else {
            return Err(StoreError::RunNotFound(run.id).into());
        };
        if locked.status.is_terminal() {
            return Err(StoreError::Conflict(format!("run {} has ended", run.id)).into());
        }
        let existing: Option<String> =
            sqlx::query_scalar("SELECT token FROM webhook_tokens WHERE run_id = $1 AND name = $2")
                .bind(uuid(run.id))
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(token) = existing {
            return Ok(WebhookToken(token));
        }
        let token = random_token("rewind_wh_")?;
        sqlx::query("INSERT INTO webhook_tokens (token, run_id, name) VALUES ($1, $2, $3)")
            .bind(token.as_str())
            .bind(uuid(run.id))
            .bind(name)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(WebhookToken(token))
    }

    async fn put_workflow_once(
        &self,
        tenant_id: &TenantId,
        workflow: &Workflow,
        now: Timestamp,
    ) -> Result<Option<StoredWorkflow>, Fail> {
        let version = workflow.version();
        let mut tx = self.pool.begin().await?;
        // The workflow's lock orders its new versions: each takes the number after the highest
        // stored, which no concurrent put takes too.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(workflow_key(tenant_id, workflow.name()))
            .execute(&mut *tx)
            .await?;
        let inserted = sqlx::query(concat!(
            "INSERT INTO workflows (tenant, version, name, document, labels, created_at, number) \
             SELECT $1, $2, $3, $4, $5::jsonb, $6, COALESCE(max(number), 0) + 1 \
             FROM workflows WHERE tenant = $1 AND name = $3 \
             ON CONFLICT (tenant, version) DO NOTHING RETURNING ",
            workflow_columns!()
        ))
        .bind(tenant(tenant_id))
        .bind(bytes(version))
        .bind(workflow.name())
        .bind(rewind_format::encode_workflow(workflow))
        .bind(encode_labels(workflow.labels()))
        .bind(ts(now))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = inserted else {
            // Stored before: nothing changes.
            return Ok(None);
        };
        let tests: BTreeSet<Digest> = workflow.suite().tests.iter().map(|t| t.test).collect();
        if !tests.is_empty() {
            let digests: Vec<Vec<u8>> = tests.iter().map(|test| bytes(*test)).collect();
            // The suite's rows not being deleted, locked by digest until the commit: no cleanup
            // marks them before the references below are written.
            let rows = sqlx::query(
                "SELECT digest FROM tests WHERE tenant = $1 AND digest = ANY($2) \
                 AND deletion_started_at IS NULL ORDER BY digest FOR SHARE",
            )
            .bind(tenant(tenant_id))
            .bind(&digests)
            .fetch_all(&mut *tx)
            .await?;
            let live: BTreeSet<Digest> = rows
                .iter()
                .map(|row| row.digest(0))
                .collect::<Result<_, Error>>()?;
            if let Some(missing) = workflow
                .suite()
                .tests
                .iter()
                .find(|test| !live.contains(&test.test))
            {
                return Err(missing_test(missing.test).into());
            }
            sqlx::query(
                "INSERT INTO workflow_tests (tenant, version, digest) \
                 SELECT $1, $2, digest FROM unnest($3::bytea[]) AS t(digest) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(tenant(tenant_id))
            .bind(bytes(version))
            .bind(digests)
            .execute(&mut *tx)
            .await?;
        }
        let stored = workflow_from_row(&row)?;
        tx.commit().await?;
        Ok(Some(stored))
    }

    async fn reserve_tests_once(
        &self,
        tenant_id: &TenantId,
        tests: &[TestRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, Fail> {
        // One row per digest, in digest order: the order every writer locks test rows in.
        let by_digest: BTreeMap<Digest, &TestRecord> =
            tests.iter().map(|test| (test.digest, test)).collect();
        let digests: Vec<Vec<u8>> = by_digest.keys().map(|digest| bytes(*digest)).collect();
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            "SELECT digest, deletion_started_at IS NOT NULL FROM tests \
             WHERE tenant = $1 AND digest = ANY($2) ORDER BY digest FOR UPDATE",
        )
        .bind(tenant(tenant_id))
        .bind(digests)
        .fetch_all(&mut *tx)
        .await?;
        let mut deleting = BTreeSet::new();
        for row in &rows {
            if row.boolean(1)? {
                deleting.insert(row.digest(0)?);
            }
        }
        let kept: Vec<&TestRecord> = by_digest
            .values()
            .filter(|test| !deleting.contains(&test.digest))
            .copied()
            .collect();
        if !kept.is_empty() {
            // A missing row is created; an existing one starts its grace again. A row created
            // since the lock above is not being deleted either.
            sqlx::query(
                "INSERT INTO tests (tenant, digest, namespace, size, reserved_at) \
                 SELECT $1, digest, namespace, size, $5 \
                 FROM unnest($2::bytea[], $3::text[], $4::bigint[]) \
                     WITH ORDINALITY AS t(digest, namespace, size, n) \
                 ORDER BY n \
                 ON CONFLICT (tenant, digest) DO UPDATE SET reserved_at = $5 \
                 WHERE tests.deletion_started_at IS NULL",
            )
            .bind(tenant(tenant_id))
            .bind(
                kept.iter()
                    .map(|test| bytes(test.digest))
                    .collect::<Vec<_>>(),
            )
            .bind(
                kept.iter()
                    .map(|test| test.namespace.as_ref().map(NamespaceName::as_str))
                    .collect::<Vec<_>>(),
            )
            .bind(kept.iter().map(|test| int(test.size)).collect::<Vec<_>>())
            .bind(ts(now))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        // In the order asked.
        Ok(tests
            .iter()
            .map(|test| test.digest)
            .filter(|digest| deleting.contains(digest))
            .collect())
    }

    async fn unreferenced_tests_once(
        &self,
        now: Timestamp,
        max: usize,
    ) -> Result<Vec<(TenantId, Digest)>, Fail> {
        let mut tx = self.pool.begin().await?;
        // The rows due, but those a writer or another cleanup holds.
        let rows = sqlx::query(concat!(
            "SELECT t.tenant, t.digest FROM tests t WHERE ",
            test_due!(),
            " ORDER BY t.tenant, t.digest LIMIT $4 FOR UPDATE SKIP LOCKED"
        ))
        .bind(ts(now))
        .bind(ms(TEST_GRACE))
        .bind(ms(PAYLOAD_DELETION_TIMEOUT))
        .bind(limit(max))
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let picked = rows
            .iter()
            .map(|row| Ok((row.tenant(0)?, row.digest(1)?)))
            .collect::<Result<Vec<_>, Error>>()?;
        let (tenants, digests) = sorted_keys(&picked);
        // Checked again under the locks, by a statement that sees what committed before them: a
        // version that referenced a row before it was locked keeps it.
        let marked = sqlx::query(concat!(
            "UPDATE tests t SET deletion_started_at = $1 \
             FROM unnest($4::text[], $5::bytea[]) AS f(tenant, digest) \
             WHERE t.tenant = f.tenant AND t.digest = f.digest AND ",
            test_due!(),
            " RETURNING t.tenant, t.digest"
        ))
        .bind(ts(now))
        .bind(ms(TEST_GRACE))
        .bind(ms(PAYLOAD_DELETION_TIMEOUT))
        .bind(tenants)
        .bind(digests)
        .fetch_all(&mut *tx)
        .await?;
        let marked: BTreeSet<(TenantId, Digest)> = marked
            .iter()
            .map(|row| Ok((row.tenant(0)?, row.digest(1)?)))
            .collect::<Result<_, Error>>()?;
        tx.commit().await?;
        Ok(marked.into_iter().collect())
    }

    async fn drop_tests_once(
        &self,
        tests: &[(TenantId, Digest)],
        erased: bool,
    ) -> Result<usize, Fail> {
        let (tenants, digests) = sorted_keys(tests);
        if !erased {
            let dropped = sqlx::query(
                "DELETE FROM tests t USING ( \
                     SELECT tenant, digest FROM unnest($1::text[], $2::bytea[]) \
                         AS f(tenant, digest) \
                     ORDER BY tenant, digest) f \
                 WHERE t.tenant = f.tenant AND t.digest = f.digest \
                 AND t.deletion_started_at IS NOT NULL \
                 AND NOT EXISTS (SELECT 1 FROM workflow_tests w \
                                 WHERE w.tenant = t.tenant AND w.digest = t.digest)",
            )
            .bind(tenants)
            .bind(digests)
            .execute(&self.pool)
            .await?;
            return Ok(usize::try_from(dropped.rows_affected()).unwrap_or(usize::MAX));
        }
        // A namespace's erasure: the rows go whatever references them, then the references,
        // read after the rows' locks: those of a version written meanwhile go too.
        let mut tx = self.pool.begin().await?;
        let dropped = sqlx::query(
            "DELETE FROM tests t USING ( \
                 SELECT tenant, digest FROM unnest($1::text[], $2::bytea[]) AS f(tenant, digest) \
                 ORDER BY tenant, digest) f \
             WHERE t.tenant = f.tenant AND t.digest = f.digest",
        )
        .bind(&tenants)
        .bind(&digests)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM workflow_tests w USING unnest($1::text[], $2::bytea[]) AS f(tenant, digest) \
             WHERE w.tenant = f.tenant AND w.digest = f.digest",
        )
        .bind(&tenants)
        .bind(&digests)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(usize::try_from(dropped.rows_affected()).unwrap_or(usize::MAX))
    }

    async fn reserve_payloads_once(
        &self,
        tenant_id: &TenantId,
        payloads: &[PayloadRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, Fail> {
        // One row per digest, in digest order: the order every writer locks payload rows in.
        let by_digest: BTreeMap<Digest, &PayloadRecord> = payloads
            .iter()
            .map(|payload| (payload.digest, payload))
            .collect();
        let digests: Vec<Vec<u8>> = by_digest.keys().map(|digest| bytes(*digest)).collect();
        let mut tx = self.pool.begin().await?;
        // The rows there, locked in digest order, as every writer locks them.
        let rows = sqlx::query(
            "SELECT digest, deletion_started_at IS NOT NULL FROM payloads \
             WHERE tenant = $1 AND digest = ANY($2) ORDER BY digest FOR UPDATE",
        )
        .bind(tenant(tenant_id))
        .bind(digests)
        .fetch_all(&mut *tx)
        .await?;
        let mut deleting = BTreeSet::new();
        for row in &rows {
            if row.boolean(1)? {
                deleting.insert(row.digest(0)?);
            }
        }
        let kept: Vec<&PayloadRecord> = by_digest
            .values()
            .filter(|payload| !deleting.contains(&payload.digest))
            .copied()
            .collect();
        if !kept.is_empty() {
            // A missing row is created; a row without references starts its grace again. A row
            // created since the lock above is not being deleted either.
            sqlx::query(
                "INSERT INTO payloads (tenant, digest, bytes, preview, value, unreferenced_since) \
                 SELECT $1, digest, bytes, preview, value, $6 \
                 FROM unnest($2::bytea[], $3::bigint[], $4::text[], $5::text[]) \
                     WITH ORDINALITY AS p(digest, bytes, preview, value, n) \
                 ORDER BY n \
                 ON CONFLICT (tenant, digest) DO UPDATE SET \
                     unreferenced_since = GREATEST(payloads.unreferenced_since, $6) \
                 WHERE payloads.unreferenced_since IS NOT NULL \
                     AND payloads.deletion_started_at IS NULL",
            )
            .bind(tenant(tenant_id))
            .bind(
                kept.iter()
                    .map(|payload| bytes(payload.digest))
                    .collect::<Vec<_>>(),
            )
            .bind(
                kept.iter()
                    .map(|payload| int(payload.size))
                    .collect::<Vec<_>>(),
            )
            .bind(
                kept.iter()
                    .map(|payload| payload.preview.as_str())
                    .collect::<Vec<_>>(),
            )
            .bind(
                kept.iter()
                    .map(|payload| payload.value.as_ref().map(codec::encode_value))
                    .collect::<Vec<_>>(),
            )
            .bind(ts(now))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        // In the order asked.
        Ok(payloads
            .iter()
            .map(|payload| payload.digest)
            .filter(|digest| deleting.contains(digest))
            .collect())
    }

    async fn forget_payloads_once(
        &self,
        payloads: &[(TenantId, Digest)],
        now: Timestamp,
    ) -> Result<Vec<(TenantId, Digest)>, Fail> {
        // Payload rows are locked by digest, as commits lock them.
        let sorted: BTreeSet<(TenantId, Digest)> = payloads.iter().cloned().collect();
        let tenants: Vec<&str> = sorted.iter().map(|(owner, _)| tenant(owner)).collect();
        let digests: Vec<Vec<u8>> = sorted.iter().map(|(_, digest)| bytes(*digest)).collect();
        let default_grace = ms(Retention::default().payload_grace);
        let rows = sqlx::query(
            "UPDATE payloads p SET deletion_started_at = $3 FROM ( \
                 SELECT tenant, digest FROM unnest($1::text[], $2::bytea[]) AS f(tenant, digest) \
                 ORDER BY tenant, digest) f \
             WHERE p.tenant = f.tenant AND p.digest = f.digest \
             AND p.unreferenced_since IS NOT NULL \
             AND p.unreferenced_since <= $3 - COALESCE( \
                 (SELECT payload_grace_ms FROM tenants WHERE id = p.tenant), $4) \
             AND (p.deletion_started_at IS NULL OR p.deletion_started_at <= $3 - $5) \
             AND NOT EXISTS (SELECT 1 FROM payload_refs r \
                             WHERE r.tenant = p.tenant AND r.digest = p.digest) \
             AND NOT EXISTS (SELECT 1 FROM tests x \
                             WHERE x.tenant = p.tenant AND x.digest = p.digest) \
             RETURNING p.tenant, p.digest",
        )
        .bind(tenants)
        .bind(digests)
        .bind(ts(now))
        .bind(default_grace)
        .bind(ms(PAYLOAD_DELETION_TIMEOUT))
        .fetch_all(&self.pool)
        .await?;
        let forgotten: BTreeSet<(TenantId, Digest)> = rows
            .iter()
            .map(|row| Ok((row.tenant(0)?, row.digest(1)?)))
            .collect::<Result<_, Error>>()?;
        // In the order asked.
        Ok(payloads
            .iter()
            .filter(|payload| forgotten.contains(payload))
            .cloned()
            .collect())
    }

    async fn drop_payloads_once(
        &self,
        payloads: &[(TenantId, Digest)],
        started: Timestamp,
    ) -> Result<usize, Fail> {
        // Payload rows are locked by digest, as commits lock them.
        let sorted: BTreeSet<(TenantId, Digest)> = payloads.iter().cloned().collect();
        let tenants: Vec<&str> = sorted.iter().map(|(owner, _)| tenant(owner)).collect();
        let digests: Vec<Vec<u8>> = sorted.iter().map(|(_, digest)| bytes(*digest)).collect();
        let dropped = sqlx::query(
            "DELETE FROM payloads p USING ( \
                 SELECT tenant, digest FROM unnest($1::text[], $2::bytea[]) AS f(tenant, digest) \
                 ORDER BY tenant, digest) f \
             WHERE p.tenant = f.tenant AND p.digest = f.digest \
             AND p.deletion_started_at = $3 \
             AND NOT EXISTS (SELECT 1 FROM payload_refs r \
                             WHERE r.tenant = p.tenant AND r.digest = p.digest)",
        )
        .bind(tenants)
        .bind(digests)
        .bind(ts(started))
        .execute(&self.pool)
        .await?;
        Ok(usize::try_from(dropped.rows_affected()).unwrap_or(usize::MAX))
    }

    async fn archive_once(
        &self,
        run: &RunRef,
        document: Digest,
        now: Timestamp,
    ) -> Result<bool, Fail> {
        let mut tx = self.pool.begin().await?;
        let Some(locked) = lock_run(&mut tx, run).await? else {
            return Ok(false);
        };
        // A run with feed items left keeps its history until they are applied.
        let (archived, pending): (bool, bool) = sqlx::query_as(
            "SELECT archive_document IS NOT NULL, \
             EXISTS (SELECT 1 FROM feed WHERE run_id = runs.id) FROM runs WHERE id = $1",
        )
        .bind(uuid(run.id))
        .fetch_one(&mut *tx)
        .await?;
        if !locked.status.is_terminal() || archived || pending {
            return Ok(false);
        }
        sqlx::query(
            "WITH history_gone AS (DELETE FROM history WHERE run_id = $1), \
             inbox_gone AS (DELETE FROM inbox WHERE run_id = $1) \
             UPDATE runs SET archive_document = $2, archived_at = $3, inbox_head = NULL, \
                 runnable_at = NULL \
             WHERE id = $1",
        )
        .bind(uuid(run.id))
        .bind(bytes(document))
        .bind(ts(now))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn delete_run_once(&self, run: &RunRef, now: Timestamp) -> Result<bool, Fail> {
        let mut tx = self.pool.begin().await?;
        if lock_run(&mut tx, run).await?.is_none() {
            return Ok(false);
        }
        let id = uuid(run.id);
        let (namespace, script): (String, Vec<u8>) =
            sqlx::query_as("SELECT namespace, script FROM runs WHERE id = $1")
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        namespaces::uncount_run(&mut tx, &run.tenant, &namespace, &script).await?;
        let mut referenced: Vec<Vec<u8>> = sqlx::query_scalar(
            "DELETE FROM payload_refs WHERE tenant = $1 AND run_id = $2 RETURNING digest",
        )
        .bind(tenant(&run.tenant))
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        referenced.sort_unstable();
        if !referenced.is_empty() {
            // A payload left without references starts its grace now. Rows locked by digest.
            sqlx::query(
                "UPDATE payloads p SET unreferenced_since = $3 FROM ( \
                     SELECT digest FROM unnest($2::bytea[]) AS d(digest) ORDER BY digest) d \
                 WHERE p.tenant = $1 AND p.digest = d.digest \
                 AND NOT EXISTS (SELECT 1 FROM payload_refs r \
                                 WHERE r.tenant = p.tenant AND r.digest = p.digest)",
            )
            .bind(tenant(&run.tenant))
            .bind(referenced)
            .bind(ts(now))
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "WITH history_gone AS (DELETE FROM history WHERE run_id = $1), \
             inbox_gone AS (DELETE FROM inbox WHERE run_id = $1), \
             keys_gone AS (DELETE FROM signal_keys WHERE run_id = $1), \
             timers_gone AS (DELETE FROM timers WHERE run_id = $1), \
             tasks_gone AS (DELETE FROM tool_tasks WHERE run_id = $1), \
             tokens_gone AS (DELETE FROM webhook_tokens WHERE run_id = $1), \
             feed_gone AS (DELETE FROM feed WHERE run_id = $1), \
             watchers_gone AS (DELETE FROM run_watchers WHERE run_id = $1 OR watcher = $1) \
             DELETE FROM runs WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }
}

/// Inserts the tool tasks of an activation: `approval` when their policy asks for it, else
/// `ready` and due at `now`.
async fn insert_tool_tasks(
    conn: &mut PgConnection,
    run: &RunRef,
    toolset: Digest,
    activation: &Activation,
    now: Timestamp,
) -> Result<(), Error> {
    let tasks = &activation.tool_calls;
    let mut seqs = Vec::with_capacity(tasks.len());
    let mut tools = Vec::with_capacity(tasks.len());
    let mut routes = Vec::with_capacity(tasks.len());
    let mut servers: Vec<Option<String>> = Vec::with_capacity(tasks.len());
    let mut dispatched = Vec::with_capacity(tasks.len());
    let mut args_digests = Vec::with_capacity(tasks.len());
    let mut args: Vec<Option<String>> = Vec::with_capacity(tasks.len());
    let mut interruptions = Vec::with_capacity(tasks.len());
    let mut approvals = Vec::with_capacity(tasks.len());
    let mut execution_timeouts: Vec<Option<i64>> = Vec::with_capacity(tasks.len());
    let mut heartbeat_timeouts = Vec::with_capacity(tasks.len());
    let mut concurrencies = Vec::with_capacity(tasks.len());
    let mut server_concurrencies: Vec<Option<i64>> = Vec::with_capacity(tasks.len());
    for task in tasks {
        let (digest, value) = codec::payload_columns(&task.args);
        seqs.push(seq(task.seq));
        tools.push(task.tool.clone());
        routes.push(codec::encode_route(&task.route));
        servers.push(codec::mcp_server(&task.route).map(str::to_owned));
        dispatched.push(task.route.is_dispatched());
        args_digests.push(digest);
        args.push(value);
        interruptions.push(task.policy.on_interruption.as_str().to_owned());
        approvals.push(task.policy.approval);
        execution_timeouts.push(task.policy.execution_timeout.map(ms));
        heartbeat_timeouts.push(ms(task.policy.heartbeat_timeout));
        concurrencies.push(i64::from(task.policy.concurrency));
        server_concurrencies.push(task.server_concurrency.map(i64::from));
    }
    sqlx::query(
        "INSERT INTO tool_tasks (tenant, run_id, toolset, seq, tool, route, mcp_server, \
             dispatched, args_digest, args, on_interruption, approval, execution_timeout_ms, \
             heartbeat_timeout_ms, concurrency, server_concurrency, state, attempt, token, \
             lease_owner, lease_until, next_attempt_at, due_at, parked_kind, parked_reason, \
             schema_change_approved, created_at) \
         SELECT $1, $2, $3, t.seq, t.tool, t.route, t.mcp_server, t.dispatched, t.args_digest, \
             t.args, t.on_interruption, t.approval, t.execution_timeout_ms, \
             t.heartbeat_timeout_ms, t.concurrency, t.server_concurrency, \
             CASE WHEN t.approval THEN 'approval' ELSE 'ready' END, 0, NULL, NULL, NULL, \
             CASE WHEN t.approval THEN NULL ELSE $4::bigint END, \
             CASE WHEN t.approval THEN NULL ELSE $4::bigint END, NULL, NULL, false, $4 \
         FROM unnest($5::bigint[], $6::text[], $7::text[], $8::text[], $9::boolean[], \
             $10::bytea[], $11::text[], $12::text[], $13::boolean[], $14::bigint[], \
             $15::bigint[], $16::bigint[], $17::bigint[]) \
             AS t(seq, tool, route, mcp_server, dispatched, args_digest, args, on_interruption, \
                  approval, execution_timeout_ms, heartbeat_timeout_ms, concurrency, \
                  server_concurrency) \
         ON CONFLICT (run_id, seq) DO UPDATE SET tenant = EXCLUDED.tenant, \
             toolset = EXCLUDED.toolset, tool = EXCLUDED.tool, route = EXCLUDED.route, \
             mcp_server = EXCLUDED.mcp_server, dispatched = EXCLUDED.dispatched, \
             args_digest = EXCLUDED.args_digest, args = EXCLUDED.args, \
             on_interruption = EXCLUDED.on_interruption, approval = EXCLUDED.approval, \
             execution_timeout_ms = EXCLUDED.execution_timeout_ms, \
             heartbeat_timeout_ms = EXCLUDED.heartbeat_timeout_ms, \
             concurrency = EXCLUDED.concurrency, server_concurrency = EXCLUDED.server_concurrency, \
             state = EXCLUDED.state, attempt = 0, token = NULL, lease_owner = NULL, \
             lease_until = NULL, next_attempt_at = EXCLUDED.next_attempt_at, \
             due_at = EXCLUDED.due_at, parked_kind = NULL, parked_reason = NULL, \
             schema_change_approved = false, created_at = EXCLUDED.created_at",
    )
    .bind(tenant(&run.tenant))
    .bind(uuid(run.id))
    .bind(bytes(toolset))
    .bind(ts(now))
    .bind(seqs)
    .bind(tools)
    .bind(routes)
    .bind(servers)
    .bind(dispatched)
    .bind(args_digests)
    .bind(args)
    .bind(interruptions)
    .bind(approvals)
    .bind(execution_timeouts)
    .bind(heartbeat_timeouts)
    .bind(concurrencies)
    .bind(server_concurrencies)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Locks the existing payload rows of an activation, by digest, and checks that each payload of
/// the blob tier has a row, reserved before its blob was written, whose deletion has not
/// started: its blob may be gone otherwise.
async fn check_reserved(
    conn: &mut PgConnection,
    run: &RunRef,
    payloads: &[PayloadRecord],
) -> Result<(), Fail> {
    let digests: BTreeSet<Digest> = payloads.iter().map(|payload| payload.digest).collect();
    let digests: Vec<Vec<u8>> = digests.into_iter().map(bytes).collect();
    let rows = sqlx::query(
        "SELECT digest, deletion_started_at IS NULL FROM payloads \
         WHERE tenant = $1 AND digest = ANY($2) ORDER BY digest FOR UPDATE",
    )
    .bind(tenant(&run.tenant))
    .bind(digests)
    .fetch_all(&mut *conn)
    .await?;
    let mut usable = BTreeSet::new();
    for row in &rows {
        if row.boolean(1)? {
            usable.insert(row.digest(0)?);
        }
    }
    match payloads
        .iter()
        .find(|payload| payload.value.is_none() && !usable.contains(&payload.digest))
    {
        Some(payload) => Err(StoreError::Conflict(format!(
            "payload {} has no reserved row, or is being deleted: its blob may be gone",
            payload.digest.short()
        ))
        .into()),
        None => Ok(()),
    }
}

/// Writes the payload rows of an activation, by digest, and the run's references to them. A row
/// already there keeps its value, gets one if it had none, is referenced again, and is no longer
/// being deleted.
async fn insert_payloads(
    conn: &mut PgConnection,
    run: &RunRef,
    payloads: &[PayloadRecord],
) -> Result<(), Error> {
    // One row per digest, in digest order: the order every writer locks payload rows in.
    let mut by_digest: BTreeMap<Digest, &PayloadRecord> = BTreeMap::new();
    for payload in payloads {
        let kept = by_digest.entry(payload.digest).or_insert(payload);
        if kept.value.is_none() && payload.value.is_some() {
            *kept = payload;
        }
    }
    let digests: Vec<Vec<u8>> = by_digest.keys().map(|digest| bytes(*digest)).collect();
    let sizes: Vec<i64> = by_digest.values().map(|p| int(p.size)).collect();
    let previews: Vec<&str> = by_digest.values().map(|p| p.preview.as_str()).collect();
    let values: Vec<Option<String>> = by_digest
        .values()
        .map(|p| p.value.as_ref().map(codec::encode_value))
        .collect();
    sqlx::query(
        "WITH rows AS ( \
             INSERT INTO payloads (tenant, digest, bytes, preview, value) \
             SELECT $1, digest, bytes, preview, value \
             FROM unnest($3::bytea[], $4::bigint[], $5::text[], $6::text[]) \
                 WITH ORDINALITY AS p(digest, bytes, preview, value, n) \
             ORDER BY n \
             ON CONFLICT (tenant, digest) DO UPDATE SET \
                 value = COALESCE(payloads.value, EXCLUDED.value), unreferenced_since = NULL, \
                 deletion_started_at = NULL \
             WHERE payloads.unreferenced_since IS NOT NULL \
                 OR payloads.deletion_started_at IS NOT NULL \
                 OR (payloads.value IS NULL AND EXCLUDED.value IS NOT NULL)) \
         INSERT INTO payload_refs (tenant, digest, run_id) \
         SELECT $1, digest, $2 FROM unnest($3::bytea[]) AS r(digest) \
         ON CONFLICT DO NOTHING",
    )
    .bind(tenant(&run.tenant))
    .bind(uuid(run.id))
    .bind(digests)
    .bind(sizes)
    .bind(previews)
    .bind(values)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

impl JournalStore for PostgresStore {
    // ---------------------------------------------------------------------------------------
    // Scripts and toolsets

    async fn put_script(
        &self,
        tenant_id: &TenantId,
        script: &Script,
        compiled: &CompiledScript,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO scripts (tenant, digest, source, transpiled, source_map) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(script.digest()))
        .bind(script.source())
        .bind(compiled.javascript.as_str())
        .bind(compiled.source_map.as_str())
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    async fn get_script(
        &self,
        tenant_id: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredScript>, StoreError> {
        let row = sqlx::query(
            "SELECT source, transpiled, source_map FROM scripts WHERE tenant = $1 AND digest = $2",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(digest))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let script = Script::new(row.text(0)?);
        if script.digest() != digest {
            return Err(Error::Corrupt(format!("script {digest} does not hash to its key")).into());
        }
        Ok(Some(StoredScript {
            script,
            compiled: CompiledScript {
                javascript: row.text(1)?,
                source_map: row.text(2)?,
            },
        }))
    }

    async fn put_toolset(
        &self,
        tenant_id: &TenantId,
        toolset: &Toolset,
        declarations: &ToolDeclarations,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO toolsets (tenant, digest, spec, declarations, prompt) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(toolset.digest()))
        .bind(rewind_format::encode_toolset(toolset))
        .bind(declarations.typescript.as_str())
        .bind(declarations.prompt.as_str())
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    async fn get_toolset(
        &self,
        tenant_id: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredToolset>, StoreError> {
        let row = sqlx::query(
            "SELECT spec, declarations, prompt FROM toolsets WHERE tenant = $1 AND digest = $2",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(digest))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let toolset = rewind_format::decode_toolset(&row.text(0)?)
            .map_err(|err| Error::Corrupt(format!("toolset {digest}: {err}")))?;
        if toolset.digest() != digest {
            return Err(
                Error::Corrupt(format!("toolset {digest} does not hash to its key")).into(),
            );
        }
        Ok(Some(StoredToolset {
            toolset,
            declarations: ToolDeclarations {
                typescript: row.text(1)?,
                prompt: row.text(2)?,
            },
        }))
    }

    async fn set_script_title(
        &self,
        tenant_id: &TenantId,
        script: Digest,
        title: Option<&ScriptTitle>,
    ) -> Result<bool, StoreError> {
        // A tenant's scripts are never deleted: whether it has the script needs no lock.
        let Some(title) = title else {
            let stored: bool = sqlx::query_scalar(
                "WITH removed AS (DELETE FROM script_titles WHERE tenant = $1 AND digest = $2) \
                 SELECT EXISTS (SELECT 1 FROM scripts WHERE tenant = $1 AND digest = $2)",
            )
            .bind(tenant(tenant_id))
            .bind(bytes(script))
            .fetch_one(&self.pool)
            .await
            .map_err(Error::from)?;
            return Ok(stored);
        };
        let written = sqlx::query(
            "INSERT INTO script_titles (tenant, digest, title, description, updated_by, \
                 updated_at) \
             SELECT tenant, digest, $3, $4, $5, $6 FROM scripts WHERE tenant = $1 AND digest = $2 \
             ON CONFLICT (tenant, digest) DO UPDATE SET title = EXCLUDED.title, \
                 description = EXCLUDED.description, updated_by = EXCLUDED.updated_by, \
                 updated_at = EXCLUDED.updated_at",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(script))
        .bind(title.title.as_str())
        .bind(title.description.as_deref())
        .bind(title.updated_by.as_str())
        .bind(ts(title.updated_at))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(written.rows_affected() > 0)
    }

    async fn script_titles(
        &self,
        tenant_id: &TenantId,
        scripts: &[Digest],
    ) -> Result<Vec<(Digest, ScriptTitle)>, StoreError> {
        if scripts.is_empty() {
            return Ok(Vec::new());
        }
        let digests: Vec<Vec<u8>> = scripts.iter().map(|digest| bytes(*digest)).collect();
        // In the order of `scripts`.
        let rows = sqlx::query(
            "SELECT t.digest, t.title, t.description, t.updated_by, t.updated_at \
             FROM unnest($2::bytea[]) WITH ORDINALITY AS d(digest, n) \
             JOIN script_titles t ON t.tenant = $1 AND t.digest = d.digest ORDER BY d.n",
        )
        .bind(tenant(tenant_id))
        .bind(digests)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        let mut titles = Vec::with_capacity(rows.len());
        for row in &rows {
            titles.push((
                row.digest(0)?,
                ScriptTitle {
                    title: row.text(1)?,
                    description: row.opt_text(2)?,
                    updated_by: row.text(3)?,
                    updated_at: row.ts(4)?,
                },
            ));
        }
        Ok(titles)
    }

    async fn list_toolsets(&self, tenant_id: &TenantId) -> Result<Vec<StoredToolset>, StoreError> {
        let rows = sqlx::query(
            "SELECT digest, spec, declarations, prompt FROM toolsets WHERE tenant = $1 \
             ORDER BY digest",
        )
        .bind(tenant(tenant_id))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        let mut toolsets = Vec::with_capacity(rows.len());
        for row in &rows {
            let digest = row.digest(0)?;
            let toolset = rewind_format::decode_toolset(&row.text(1)?)
                .map_err(|err| Error::Corrupt(format!("toolset {digest}: {err}")))?;
            if toolset.digest() != digest {
                return Err(
                    Error::Corrupt(format!("toolset {digest} does not hash to its key")).into(),
                );
            }
            toolsets.push(StoredToolset {
                toolset,
                declarations: ToolDeclarations {
                    typescript: row.text(2)?,
                    prompt: row.text(3)?,
                },
            });
        }
        Ok(toolsets)
    }

    // ---------------------------------------------------------------------------------------
    // Workflows

    async fn put_workflow(
        &self,
        tenant_id: &TenantId,
        workflow: &Workflow,
        now: Timestamp,
    ) -> Result<StoredWorkflow, StoreError> {
        let version = workflow.version();
        let inserted = retrying(|| self.put_workflow_once(tenant_id, workflow, now)).await?;
        if let Some(stored) = inserted {
            return Ok(stored);
        }
        // Stored before: the version as it was stored first.
        let stored = self.get_workflow(tenant_id, version).await?;
        stored.ok_or_else(|| {
            StoreError::Backend(format!("workflow {version} was neither stored nor found"))
        })
    }

    async fn get_workflow(
        &self,
        tenant_id: &TenantId,
        version: Digest,
    ) -> Result<Option<StoredWorkflow>, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows WHERE tenant = $1 AND version = $2"
        ))
        .bind(tenant(tenant_id))
        .bind(bytes(version))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(row.map(|row| workflow_from_row(&row)).transpose()?)
    }

    async fn workflow_number(
        &self,
        tenant_id: &TenantId,
        name: &str,
        number: u32,
    ) -> Result<Option<StoredWorkflow>, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows WHERE tenant = $1 AND name = $2 AND number = $3"
        ))
        .bind(tenant(tenant_id))
        .bind(name)
        .bind(i64::from(number))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(row.map(|row| workflow_from_row(&row)).transpose()?)
    }

    async fn workflow_versions(
        &self,
        tenant_id: &TenantId,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows WHERE tenant = $1 AND name = $2 AND labels @> $3::jsonb \
             ORDER BY created_at DESC, ordinal DESC"
        ))
        .bind(tenant(tenant_id))
        .bind(name)
        .bind(encode_labels(labels))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(workflow_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn workflows_with_script(
        &self,
        tenant_id: &TenantId,
        script: Digest,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, StoreError> {
        // The script is read from the version's document: a tenant has few versions, and the
        // index on (tenant, name, …) bounds the scan to them.
        let rows = sqlx::query(concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows WHERE tenant = $1 AND labels @> $2::jsonb \
             AND (document::jsonb ->> 'script') = $3 \
             ORDER BY created_at DESC, ordinal DESC"
        ))
        .bind(tenant(tenant_id))
        .bind(encode_labels(labels))
        .bind(script.to_hex())
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(workflow_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn latest_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<Vec<StoredWorkflow>, StoreError> {
        let rows = sqlx::query(concat!(
            "SELECT DISTINCT ON (name) ",
            workflow_columns!(),
            " FROM workflows WHERE tenant = $1 AND labels @> $2::jsonb \
             AND ($3::text IS NULL OR name > $3) \
             ORDER BY name, created_at DESC, ordinal DESC LIMIT $4"
        ))
        .bind(tenant(&filter.tenant))
        .bind(encode_labels(&filter.labels))
        .bind(filter.after.as_deref())
        .bind(filter.limit.map(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(workflow_from_row)
            .collect::<Result<_, Error>>()?)
    }

    // ---------------------------------------------------------------------------------------
    // The way in

    async fn create_run(
        &self,
        run: NewRun,
        idempotency_key: Option<String>,
    ) -> Result<CreateRun, StoreError> {
        let key = idempotency_key.as_deref();
        Ok(retrying(|| self.create_run_once(&run, key)).await?)
    }

    async fn enqueue(
        &self,
        run: &RunRef,
        completion: Completion,
        idempotency_key: Option<String>,
        now: Timestamp,
    ) -> Result<Enqueued, StoreError> {
        let key = idempotency_key.as_deref();
        Ok(retrying(|| self.enqueue_once(run, &completion, key, now)).await?)
    }

    async fn get_run(&self, run: &RunRef) -> Result<Option<RunRecord>, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            run_columns!(),
            " FROM runs WHERE tenant = $1 AND id = $2"
        ))
        .bind(tenant(&run.tenant))
        .bind(uuid(run.id))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(row.map(|row| record_from_row(&row)).transpose()?)
    }

    async fn runs_in_range(
        &self,
        tenant_id: &TenantId,
        low: RunId,
        high: RunId,
        max: usize,
    ) -> Result<Vec<RunRecord>, StoreError> {
        if nothing(max) || low > high {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(concat!(
            "SELECT ",
            run_columns!(),
            " FROM runs WHERE tenant = $1 AND id BETWEEN $2 AND $3 ORDER BY id LIMIT $4"
        ))
        .bind(tenant(tenant_id))
        .bind(uuid(low))
        .bind(uuid(high))
        .bind(limit(max))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(record_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn watch_run(
        &self,
        run: &RunRef,
        watcher: RunId,
        seq: Seq,
    ) -> Result<Option<Watched>, StoreError> {
        Ok(retrying(|| self.watch_run_once(run, watcher, seq)).await?)
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<Vec<RunRecord>, StoreError> {
        let quarantined = match filter.quarantined {
            Some(true) => " AND quarantine_kind IS NOT NULL",
            Some(false) => " AND quarantine_kind IS NULL",
            None => "",
        };
        let rows = sqlx::query(&format!(
            "SELECT {columns} FROM runs WHERE tenant = $1 \
             AND ($2::text IS NULL OR status = $2) \
             AND labels @> $3::jsonb{quarantined} \
             AND ($4::bigint IS NULL OR (created_at, id) > ($4, $5)) \
             AND ($7::text IS NULL OR namespace = $7) \
             ORDER BY created_at, id LIMIT $6",
            columns = run_columns!(),
        ))
        .bind(tenant(&filter.tenant))
        .bind(filter.status.map(|status| status.as_str()))
        .bind(encode_labels(&filter.labels))
        .bind(filter.after.map(|after| ts(after.created_at)))
        .bind(filter.after.map(|after| uuid(after.id)))
        .bind(filter.limit.map(limit))
        .bind(filter.namespace.as_ref().map(NamespaceName::as_str))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(record_from_row)
            .collect::<Result<_, Error>>()?)
    }

    // ---------------------------------------------------------------------------------------
    // Leases and activations

    async fn acquire_runnable(
        &self,
        request: &AcquireRunnable<'_>,
    ) -> Result<Option<Lease>, StoreError> {
        if request.epochs.is_empty() {
            return Ok(None);
        }
        let now = ts(request.now);
        let until = ts(request.now.saturating_add(request.lease_duration));
        let epochs = epochs(request.epochs);
        let grace_end = now.saturating_sub(ms(request.sticky_grace));
        let owner = request.owner;

        // The caller's own runs first, as their sticky worker: oldest inbox item first.
        let runnable = Runnable::new(3, request.tenants);
        let sticky = lease_statement(&format!(
            "SELECT r.tenant, r.id FROM runs r WHERE {eligible} AND r.sticky_worker = ${n} \
             ORDER BY r.inbox_head LIMIT 1 FOR NO KEY UPDATE SKIP LOCKED",
            eligible = runnable.eligible,
            n = runnable.next,
        ));
        let query = runnable.bind(sqlx::query(&sticky).bind(owner).bind(until), now, &epochs);
        let row = query
            .bind(owner)
            .fetch_optional(&self.pool)
            .await
            .map_err(Error::from)?;
        if let Some(row) = row {
            return Ok(Some(lease_from_row(&row)?));
        }

        // Then the runs without a sticky worker, or whose oldest item waited the grace: a tenant
        // drawn among those with such runs, and its oldest one. The distinct tenants are read
        // one index probe each, through the index of runnable runs.
        let runnable = Runnable::new(1, request.tenants);
        let open = format!(
            "{eligible} AND (r.sticky_worker IS NULL OR r.runnable_at <= ${n})",
            eligible = runnable.eligible,
            n = runnable.next,
        );
        let tenants_query = format!(
            "WITH RECURSIVE drawn(tenant) AS ( \
                 (SELECT r.tenant FROM runs r WHERE {open} ORDER BY r.tenant LIMIT 1) \
               UNION ALL \
                 SELECT (SELECT r.tenant FROM runs r WHERE {open} AND r.tenant > d.tenant \
                         ORDER BY r.tenant LIMIT 1) \
                 FROM drawn d WHERE d.tenant IS NOT NULL) \
             SELECT tenant FROM drawn WHERE tenant IS NOT NULL"
        );
        let query = runnable.bind(sqlx::query_scalar(&tenants_query), now, &epochs);
        let mut tenants: Vec<String> = query
            .bind(grace_end)
            .fetch_all(&self.pool)
            .await
            .map_err(Error::from)?;
        if tenants.is_empty() {
            return Ok(None);
        }
        tenants.sort_unstable();
        let drawn = (request.draw % tenants.len() as u64) as usize;

        let runnable = Runnable::new(3, request.tenants);
        let pick = lease_statement(&format!(
            "SELECT r.tenant, r.id FROM runs r WHERE {eligible} \
             AND (r.sticky_worker IS NULL OR r.runnable_at <= ${n}) AND r.tenant = ${t} \
             ORDER BY r.inbox_head LIMIT 1 FOR NO KEY UPDATE SKIP LOCKED",
            eligible = runnable.eligible,
            n = runnable.next,
            t = runnable.next + 1,
        ));
        // The drawn tenant first; if concurrent workers hold all its runs, the next one.
        for offset in 0..tenants.len() {
            let candidate = &tenants[(drawn + offset) % tenants.len()];
            let query = runnable.bind(sqlx::query(&pick).bind(owner).bind(until), now, &epochs);
            let row = query
                .bind(grace_end)
                .bind(candidate.as_str())
                .fetch_optional(&self.pool)
                .await
                .map_err(Error::from)?;
            if let Some(row) = row {
                return Ok(Some(lease_from_row(&row)?));
            }
        }
        Ok(None)
    }

    async fn acquire_run(
        &self,
        run: &RunRef,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        let fence: Option<i64> = sqlx::query_scalar(concat!(
            "UPDATE runs SET fence = fence + 1, lease_owner = $3, lease_until = $4 \
             WHERE tenant = $1 AND id = $2 AND status NOT IN ",
            terminal!(),
            " AND (lease_until IS NULL OR lease_until <= $5) RETURNING fence"
        ))
        .bind(tenant(&run.tenant))
        .bind(uuid(run.id))
        .bind(owner)
        .bind(ts(now.saturating_add(lease_duration)))
        .bind(ts(now))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        if let Some(fence) = fence {
            return Ok(Some(Lease {
                run: run.clone(),
                fence: u64::try_from(fence)
                    .map_err(|_| Error::Corrupt(format!("fence {fence}")))?,
            }));
        }
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM runs WHERE tenant = $1 AND id = $2")
                .bind(tenant(&run.tenant))
                .bind(uuid(run.id))
                .fetch_optional(&self.pool)
                .await
                .map_err(Error::from)?;
        match exists {
            Some(_) => Ok(None),
            None => Err(StoreError::RunNotFound(run.id)),
        }
    }

    async fn acquire_quarantined(
        &self,
        owner: &str,
        build: &str,
        epoch_list: &[Epoch],
        tenants: &TenantScope,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        if epoch_list.is_empty() {
            return Ok(None);
        }
        let (scope, scoped) = scope_clause("tenant", tenants, 6);
        let statement = format!(
            "UPDATE runs SET fence = fence + 1, lease_owner = $1, lease_until = $2, \
                 quarantine_retried_by = $3 \
             WHERE (tenant, id) = ( \
                 SELECT tenant, id FROM runs \
                 WHERE quarantine_kind IS NOT NULL AND status NOT IN {terminal} \
                 AND (lease_until IS NULL OR lease_until <= $4) \
                 AND (quarantine_retried_by IS NULL OR quarantine_retried_by <> $3) \
                 AND epoch = ANY($5){scope} \
                 ORDER BY quarantine_since, tenant, id LIMIT 1 FOR NO KEY UPDATE SKIP LOCKED) \
             RETURNING tenant, id, fence",
            terminal = terminal!(),
        );
        let query = sqlx::query(&statement)
            .bind(owner)
            .bind(ts(now.saturating_add(lease_duration)))
            .bind(build)
            .bind(ts(now))
            .bind(epochs(epoch_list));
        let query = match scoped {
            Some(scoped) => query.bind(scoped),
            None => query,
        };
        let row = query
            .fetch_optional(&self.pool)
            .await
            .map_err(Error::from)?;
        Ok(row.map(|row| lease_from_row(&row)).transpose()?)
    }

    async fn renew(
        &self,
        lease: &Lease,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<(), StoreError> {
        let renewed = sqlx::query(concat!(
            unflushed!(),
            "UPDATE runs SET lease_until = $4 FROM unflushed \
             WHERE tenant = $1 AND id = $2 AND fence = $3 AND lease_until IS NOT NULL"
        ))
        .bind(tenant(&lease.run.tenant))
        .bind(uuid(lease.run.id))
        .bind(int(lease.fence))
        .bind(ts(now.saturating_add(lease_duration)))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        if renewed.rows_affected() == 0 {
            return Err(self.lost_lease(lease).await);
        }
        Ok(())
    }

    async fn load_history(&self, run: &RunRef, from: u64) -> Result<Vec<HistoryEntry>, StoreError> {
        // One statement: the run and its entries are read from the same snapshot.
        let rows = sqlx::query(
            "SELECT r.archive_document IS NOT NULL, h.idx, h.recorded_at, h.event FROM runs r \
             LEFT JOIN history h ON h.run_id = r.id AND h.idx >= $3 \
             WHERE r.tenant = $1 AND r.id = $2 ORDER BY h.idx",
        )
        .bind(tenant(&run.tenant))
        .bind(uuid(run.id))
        .bind(int(from))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        let Some(first) = rows.first() else {
            return Err(StoreError::RunNotFound(run.id));
        };
        if first.boolean(0)? {
            return Err(StoreError::Archived(run.id));
        }
        let mut history = Vec::with_capacity(rows.len());
        for row in &rows {
            let Some(index) = row.opt_u64(1)? else {
                continue;
            };
            history.push(HistoryEntry {
                index,
                recorded_at: row.ts(2)?,
                event: codec::decode_event(&row.text(3)?)?,
            });
        }
        Ok(history)
    }

    async fn take_inbox(&self, lease: &Lease) -> Result<Vec<InboxItem>, StoreError> {
        let rows = sqlx::query(
            "SELECT r.fence, i.id, i.event, i.received_at FROM runs r \
             LEFT JOIN inbox i ON i.run_id = r.id \
             WHERE r.tenant = $1 AND r.id = $2 ORDER BY i.id",
        )
        .bind(tenant(&lease.run.tenant))
        .bind(uuid(lease.run.id))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        let Some(first) = rows.first() else {
            return Err(StoreError::RunNotFound(lease.run.id));
        };
        if first.u64(0)? != lease.fence {
            return Err(fenced(lease));
        }
        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            let Some(id) = row.opt_u64(1)? else {
                continue;
            };
            items.push(InboxItem {
                id,
                trigger: codec::decode_trigger(&row.text(2)?)?,
                received_at: row.ts(3)?,
            });
        }
        Ok(items)
    }

    async fn commit_activation(
        &self,
        lease: &Lease,
        activation: Activation,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        Ok(retrying(|| self.commit_once(lease, &activation, now)).await?)
    }

    async fn release(&self, lease: &Lease) -> Result<(), StoreError> {
        sqlx::query(concat!(
            unflushed!(),
            "UPDATE runs SET lease_owner = NULL, lease_until = NULL FROM unflushed \
             WHERE tenant = $1 AND id = $2 AND fence = $3"
        ))
        .bind(tenant(&lease.run.tenant))
        .bind(uuid(lease.run.id))
        .bind(int(lease.fence))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    async fn forget_sticky(&self, run: &RunRef, owner: &str) -> Result<(), StoreError> {
        sqlx::query(concat!(
            unflushed!(),
            "UPDATE runs SET sticky_worker = NULL FROM unflushed \
             WHERE tenant = $1 AND id = $2 AND sticky_worker = $3"
        ))
        .bind(tenant(&run.tenant))
        .bind(uuid(run.id))
        .bind(owner)
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    async fn record_failure(
        &self,
        lease: &Lease,
        error: String,
        now: Timestamp,
    ) -> Result<Failures, StoreError> {
        Ok(retrying(|| self.record_failure_once(lease, &error, now)).await?)
    }

    async fn quarantine(
        &self,
        lease: &Lease,
        kind: QuarantineKind,
        reason: String,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let quarantined = sqlx::query(
            "UPDATE runs SET quarantine_kind = $4, quarantine_reason = $5, quarantine_since = $6, \
                 lease_owner = NULL, lease_until = NULL \
             WHERE tenant = $1 AND id = $2 AND fence = $3",
        )
        .bind(tenant(&lease.run.tenant))
        .bind(uuid(lease.run.id))
        .bind(int(lease.fence))
        .bind(codec::quarantine_kind(kind))
        .bind(reason)
        .bind(ts(now))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        if quarantined.rows_affected() == 0 {
            return Err(self.lost_lease(lease).await);
        }
        Ok(())
    }

    async fn release_quarantine(&self, run: &RunRef) -> Result<bool, StoreError> {
        let was: Option<bool> = sqlx::query_scalar(
            "WITH old AS (SELECT quarantine_kind IS NOT NULL AS quarantined FROM runs \
                          WHERE tenant = $1 AND id = $2 FOR NO KEY UPDATE) \
             UPDATE runs SET failures = 0, attempts = 0, quarantine_kind = NULL, \
                 quarantine_reason = NULL, quarantine_since = NULL, quarantine_retried_by = NULL \
             FROM old WHERE tenant = $1 AND id = $2 RETURNING old.quarantined",
        )
        .bind(tenant(&run.tenant))
        .bind(uuid(run.id))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        was.ok_or(StoreError::RunNotFound(run.id))
    }

    // ---------------------------------------------------------------------------------------
    // Time

    async fn fire_due_timers(&self, now: Timestamp, max: usize) -> Result<usize, StoreError> {
        if nothing(max) {
            return Ok(0);
        }
        Ok(retrying(|| self.fire_due_timers_once(now, max)).await?)
    }

    async fn next_timer(&self) -> Result<Option<Timestamp>, StoreError> {
        let next: Option<i64> = sqlx::query_scalar("SELECT min(fire_at) FROM timers")
            .fetch_one(&self.pool)
            .await
            .map_err(Error::from)?;
        Ok(next.map(|at| Timestamp(u64::try_from(at).unwrap_or(0))))
    }

    async fn expire_due_runs(&self, now: Timestamp, max: usize) -> Result<ExpiredRuns, StoreError> {
        if nothing(max) {
            return Ok(ExpiredRuns::default());
        }
        Ok(retrying(|| self.expire_due_runs_once(now, max)).await?)
    }

    async fn next_deadline(&self) -> Result<Option<Timestamp>, StoreError> {
        let next: Option<i64> = sqlx::query_scalar(concat!(
            "SELECT min(expires_at) FROM runs WHERE status NOT IN ",
            terminal!(),
            " AND NOT expiry_enqueued"
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(next.map(|at| Timestamp(u64::try_from(at).unwrap_or(0))))
    }

    // ---------------------------------------------------------------------------------------
    // Tool tasks

    async fn claim_tool_tasks(
        &self,
        owner: &str,
        filter: &TaskFilter,
        now: Timestamp,
        max: usize,
    ) -> Result<Vec<ClaimedTask>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        Ok(retrying(|| self.claim_once(owner, filter, now, max)).await?)
    }

    async fn heartbeat_tool_task(
        &self,
        token: &TaskToken,
        now: Timestamp,
    ) -> Result<Option<Timestamp>, StoreError> {
        let until: Option<i64> = sqlx::query_scalar(concat!(
            "UPDATE tool_tasks SET \
                 lease_until = CASE WHEN state = 'claimed' THEN ",
            saturating_add!("$2", "heartbeat_timeout_ms"),
            " ELSE lease_until END, \
                 due_at = CASE WHEN state = 'claimed' THEN ",
            saturating_add!("$2", "heartbeat_timeout_ms"),
            " ELSE due_at END \
             WHERE token = $1 AND state IN ('claimed', 'deferred') RETURNING lease_until"
        ))
        .bind(token.0.as_str())
        .bind(ts(now))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(until.map(|at| Timestamp(u64::try_from(at).unwrap_or(0))))
    }

    async fn next_tool_attempt(&self, after: Timestamp) -> Result<Option<Timestamp>, StoreError> {
        // A ready task is due at its next attempt: `tool_tasks_due` holds it.
        let next: Option<i64> = sqlx::query_scalar(
            "SELECT min(due_at) FROM tool_tasks WHERE state = 'ready' AND due_at > $1",
        )
        .bind(ts(after))
        .fetch_one(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(next.map(|at| Timestamp(u64::try_from(at).unwrap_or(0))))
    }

    async fn retry_tool_task(
        &self,
        token: &TaskToken,
        next_attempt_at: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let retried = sqlx::query(
            "UPDATE tool_tasks SET state = 'ready', token = NULL, next_attempt_at = $2, \
                 lease_owner = NULL, lease_until = NULL, due_at = $2 \
             WHERE token = $1 AND state = 'claimed'",
        )
        .bind(token.0.as_str())
        .bind(ts(next_attempt_at))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        self.token_update(token, retried.rows_affected()).await
    }

    async fn defer_tool_task(
        &self,
        token: &TaskToken,
        until: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let deferred = sqlx::query(
            "UPDATE tool_tasks SET state = 'deferred', next_attempt_at = NULL, \
                 lease_owner = NULL, lease_until = $2, due_at = NULL \
             WHERE token = $1 AND state = 'claimed'",
        )
        .bind(token.0.as_str())
        .bind(ts(until))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        self.token_update(token, deferred.rows_affected()).await
    }

    async fn park_tool_task(
        &self,
        token: &TaskToken,
        reason: ParkReason,
    ) -> Result<TaskUpdate, StoreError> {
        let parked = sqlx::query(
            "UPDATE tool_tasks SET state = 'parked', token = NULL, next_attempt_at = NULL, \
                 lease_owner = NULL, lease_until = NULL, due_at = NULL, parked_kind = $2, \
                 parked_reason = $3 \
             WHERE token = $1 AND state = 'claimed'",
        )
        .bind(token.0.as_str())
        .bind(codec::park_kind(reason.kind))
        .bind(reason.message.as_str())
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        self.token_update(token, parked.rows_affected()).await
    }

    async fn complete_tool_task(
        &self,
        token: &TaskToken,
        outcome: ToolOutcome,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        Ok(retrying(|| self.complete_once(token, &outcome, now)).await?)
    }

    async fn approve_tool_task(
        &self,
        task: &TaskRef,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        // Approving a task parked by a schema change lets the call go on against the tool's
        // current schema.
        let approved = sqlx::query(
            "UPDATE tool_tasks SET state = 'ready', token = NULL, next_attempt_at = $4, \
                 lease_owner = NULL, lease_until = NULL, due_at = $4, \
                 schema_change_approved = schema_change_approved \
                     OR (state = 'parked' AND parked_kind = 'schema_changed'), \
                 parked_kind = NULL, parked_reason = NULL \
             WHERE run_id = $1 AND seq = $2 AND tenant = $3 AND state IN ('approval', 'parked')",
        )
        .bind(uuid(task.run.id))
        .bind(seq(task.seq))
        .bind(tenant(&task.run.tenant))
        .bind(ts(now))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        if approved.rows_affected() > 0 {
            return Ok(TaskUpdate::Applied);
        }
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(not_applied(task_by_ref(&mut conn, task).await?))
    }

    async fn reject_tool_task(
        &self,
        task: &TaskRef,
        failure: ToolFailure,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        Ok(retrying(|| self.reject_once(task, &failure, now)).await?)
    }

    async fn resume_parked(
        &self,
        scope: &ParkedScope,
        now: Timestamp,
    ) -> Result<usize, StoreError> {
        Ok(retrying(|| self.resume_parked_once(scope, now)).await?)
    }

    async fn secret_usage(
        &self,
        tenant_id: &TenantId,
        secret: &SecretId,
        labels: &BTreeMap<String, String>,
        max: usize,
    ) -> Result<SecretUsage, StoreError> {
        let (attached, namespace) = attached_as(secret);
        let labels =
            serde_json::to_string(labels).map_err(|error| Error::Corrupt(error.to_string()))?;
        // The active runs that attach the secret, and those of them with a call to a server or
        // an API it serves that is parked or not yet sent.
        let active = concat!(
            "r.tenant = $1 AND r.status NOT IN ",
            terminal!(),
            " AND ($3::text IS NULL OR r.namespace = $3) AND r.labels @> $5::jsonb \
             AND EXISTS (SELECT 1 FROM jsonb_each_text(r.secrets) a WHERE a.value = $2)"
        );
        let waiting = " AND EXISTS (SELECT 1 FROM tool_tasks t \
             WHERE t.run_id = r.id AND t.tenant = r.tenant \
             AND t.state IN ('approval', 'ready', 'parked') AND r.secrets ->> t.mcp_server = $2)";
        let mut usage = SecretUsage::default();
        for (filter, runs, total) in [
            (
                active.to_owned(),
                &mut usage.active_runs,
                &mut usage.active_total,
            ),
            (
                format!("{active}{waiting}"),
                &mut usage.waiting_runs,
                &mut usage.waiting_total,
            ),
        ] {
            let ids: Vec<Uuid> = sqlx::query_scalar(&format!(
                "SELECT r.id FROM runs r WHERE {filter} ORDER BY r.created_at, r.id LIMIT $4"
            ))
            .bind(tenant(tenant_id))
            .bind(attached.as_str())
            .bind(namespace)
            .bind(limit(max))
            .bind(labels.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(Error::from)?;
            let count: i64 =
                sqlx::query_scalar(&format!("SELECT count(*) FROM runs r WHERE {filter}"))
                    .bind(tenant(tenant_id))
                    .bind(attached.as_str())
                    .bind(namespace)
                    .bind(None::<i64>)
                    .bind(labels.as_str())
                    .fetch_one(&self.pool)
                    .await
                    .map_err(Error::from)?;
            *runs = ids
                .into_iter()
                .map(|id| RunRef::new(tenant_id.clone(), RunId::from_uuid(id)))
                .collect();
            *total = u64::try_from(count).unwrap_or_default();
        }
        // A tenant has few workflows: the attachments are read from their latest versions.
        let latest = JournalStore::latest_workflows(
            self,
            &WorkflowFilter {
                tenant: tenant_id.clone(),
                labels: BTreeMap::new(),
                after: None,
                limit: None,
            },
        )
        .await?;
        usage.versions = workflows_attaching(&latest, &secret.to_ref());
        Ok(usage)
    }

    async fn expire_deferred_tool_tasks(
        &self,
        now: Timestamp,
        max: usize,
    ) -> Result<usize, StoreError> {
        if nothing(max) {
            return Ok(0);
        }
        Ok(retrying(|| self.expire_deferred_once(now, max)).await?)
    }

    async fn tool_task(&self, token: &TaskToken) -> Result<Option<ToolTask>, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(task_by_token(&mut conn, token)
            .await?
            .map(|found| found.task))
    }

    async fn tool_tasks(&self, run: &RunRef) -> Result<Vec<ToolTask>, StoreError> {
        let exists: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM runs WHERE tenant = $1 AND id = $2")
                .bind(tenant(&run.tenant))
                .bind(uuid(run.id))
                .fetch_optional(&self.pool)
                .await
                .map_err(Error::from)?;
        if exists.is_none() {
            return Err(StoreError::RunNotFound(run.id));
        }
        let rows = sqlx::query(concat!(
            "SELECT ",
            task_columns!(),
            " FROM tool_tasks WHERE run_id = $1 AND tenant = $2 ORDER BY seq"
        ))
        .bind(uuid(run.id))
        .bind(tenant(&run.tenant))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| task_from_row(row).map(|found| found.task))
            .collect::<Result<_, Error>>()?)
    }

    // ---------------------------------------------------------------------------------------
    // Webhooks

    async fn create_webhook_token(
        &self,
        run: &RunRef,
        name: &str,
    ) -> Result<WebhookToken, StoreError> {
        Ok(retrying(|| self.create_webhook_token_once(run, name)).await?)
    }

    async fn resolve_webhook_token(
        &self,
        token: &WebhookToken,
    ) -> Result<Option<WebhookTarget>, StoreError> {
        let row = sqlx::query(concat!(
            "SELECT r.tenant, r.id, w.name FROM webhook_tokens w JOIN runs r ON r.id = w.run_id \
             WHERE w.token = $1 AND r.status NOT IN ",
            terminal!()
        ))
        .bind(token.0.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(WebhookTarget {
            run: RunRef::new(row.tenant(0)?, row.run_id(1)?),
            name: row.text(2)?,
        }))
    }

    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO subscriptions (tenant, id, url, secret, events, labels) \
             VALUES ($1, $2, $3, $4, $5, $6::jsonb) \
             ON CONFLICT (tenant, id) DO UPDATE SET url = EXCLUDED.url, \
                 secret = EXCLUDED.secret, events = EXCLUDED.events, labels = EXCLUDED.labels",
        )
        .bind(tenant(&subscription.tenant))
        .bind(subscription.id.as_str())
        .bind(subscription.url.as_str())
        .bind(subscription.secret.expose())
        .bind(codec::encode_event_types(&subscription.events))
        .bind(encode_labels(&subscription.labels))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    async fn subscriptions(&self, tenant_id: &TenantId) -> Result<Vec<Subscription>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, url, secret, events, labels::text FROM subscriptions \
             WHERE tenant = $1 ORDER BY id",
        )
        .bind(tenant(tenant_id))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| {
                Ok(Subscription {
                    tenant: tenant_id.clone(),
                    id: row.text(0)?,
                    url: row.text(1)?,
                    secret: Secret::new(row.text(2)?),
                    events: codec::decode_event_types(&row.text(3)?)?,
                    labels: decode_labels(&row.text(4)?)?,
                })
            })
            .collect::<Result<_, Error>>()?)
    }

    async fn delete_subscription(
        &self,
        tenant_id: &TenantId,
        id: &str,
    ) -> Result<bool, StoreError> {
        let deleted = sqlx::query("DELETE FROM subscriptions WHERE tenant = $1 AND id = $2")
            .bind(tenant(tenant_id))
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(Error::from)?;
        Ok(deleted.rows_affected() > 0)
    }

    // ---------------------------------------------------------------------------------------
    // Tests

    async fn reserve_tests(
        &self,
        tenant_id: &TenantId,
        tests: &[TestRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, StoreError> {
        if tests.is_empty() {
            return Ok(Vec::new());
        }
        Ok(retrying(|| self.reserve_tests_once(tenant_id, tests, now)).await?)
    }

    async fn get_test(
        &self,
        tenant_id: &TenantId,
        digest: Digest,
    ) -> Result<Option<TestRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT namespace, size FROM tests \
             WHERE tenant = $1 AND digest = $2 AND deletion_started_at IS NULL",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(digest))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(TestRecord {
            digest,
            namespace: row
                .opt_text(0)?
                .map(|name| {
                    NamespaceName::new(name.clone())
                        .map_err(|_| Error::Corrupt(format!("invalid namespace {name:?}")))
                })
                .transpose()?,
            size: row.u64(1)?,
        }))
    }

    async fn namespace_tests(
        &self,
        tenant_id: &TenantId,
        namespace: &NamespaceName,
        max: usize,
    ) -> Result<Vec<Digest>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT digest FROM tests WHERE tenant = $1 AND namespace = $2 \
             ORDER BY digest LIMIT $3",
        )
        .bind(tenant(tenant_id))
        .bind(namespace.as_str())
        .bind(limit(max))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| row.digest(0))
            .collect::<Result<_, Error>>()?)
    }

    async fn unreferenced_tests(
        &self,
        now: Timestamp,
        max: usize,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        Ok(retrying(|| self.unreferenced_tests_once(now, max)).await?)
    }

    async fn drop_tests(
        &self,
        tests: &[(TenantId, Digest)],
        erased: bool,
    ) -> Result<usize, StoreError> {
        if tests.is_empty() {
            return Ok(0);
        }
        Ok(retrying(|| self.drop_tests_once(tests, erased)).await?)
    }

    // ---------------------------------------------------------------------------------------
    // Payloads

    async fn get_payload(
        &self,
        tenant_id: &TenantId,
        digest: Digest,
    ) -> Result<Option<PayloadRecord>, StoreError> {
        let row = sqlx::query(
            "SELECT bytes, preview, value FROM payloads WHERE tenant = $1 AND digest = $2",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(digest))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(PayloadRecord {
            digest,
            size: row.u64(0)?,
            preview: row.text(1)?,
            value: row
                .opt_text(2)?
                .map(|json| codec::decode_value(&json))
                .transpose()?,
        }))
    }

    async fn reserve_payloads(
        &self,
        tenant_id: &TenantId,
        payloads: &[PayloadRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, StoreError> {
        if payloads.is_empty() {
            return Ok(Vec::new());
        }
        Ok(retrying(|| self.reserve_payloads_once(tenant_id, payloads, now)).await?)
    }

    async fn unreferenced_payloads(
        &self,
        now: Timestamp,
        max: usize,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT p.tenant, p.digest FROM payloads p LEFT JOIN tenants t ON t.id = p.tenant \
             WHERE p.unreferenced_since IS NOT NULL \
             AND p.unreferenced_since <= $1 - COALESCE(t.payload_grace_ms, $2) \
             AND (p.deletion_started_at IS NULL OR p.deletion_started_at <= $1 - $4) \
             AND NOT EXISTS (SELECT 1 FROM payload_refs r \
                             WHERE r.tenant = p.tenant AND r.digest = p.digest) \
             AND NOT EXISTS (SELECT 1 FROM tests x \
                             WHERE x.tenant = p.tenant AND x.digest = p.digest) \
             ORDER BY p.tenant, p.digest LIMIT $3",
        )
        .bind(ts(now))
        .bind(ms(Retention::default().payload_grace))
        .bind(limit(max))
        .bind(ms(PAYLOAD_DELETION_TIMEOUT))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| Ok((row.tenant(0)?, row.digest(1)?)))
            .collect::<Result<_, Error>>()?)
    }

    async fn forget_payloads(
        &self,
        payloads: &[(TenantId, Digest)],
        now: Timestamp,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        if payloads.is_empty() {
            return Ok(Vec::new());
        }
        Ok(retrying(|| self.forget_payloads_once(payloads, now)).await?)
    }

    async fn drop_payloads(
        &self,
        payloads: &[(TenantId, Digest)],
        started: Timestamp,
    ) -> Result<usize, StoreError> {
        if payloads.is_empty() {
            return Ok(0);
        }
        Ok(retrying(|| self.drop_payloads_once(payloads, started)).await?)
    }

    // ---------------------------------------------------------------------------------------
    // Retention

    async fn runs_to_archive(&self, now: Timestamp, max: usize) -> Result<Vec<RunRef>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(concat!(
            "SELECT r.tenant, r.id FROM runs r LEFT JOIN tenants t ON t.id = r.tenant \
             WHERE r.status IN ",
            terminal!(),
            " AND r.archive_document IS NULL AND r.ended_at IS NOT NULL \
             AND r.ended_at <= $1 - COALESCE(t.journal_retention_ms, $2) \
             AND NOT EXISTS (SELECT 1 FROM feed f WHERE f.run_id = r.id) \
             ORDER BY r.ended_at, r.tenant, r.id LIMIT $3"
        ))
        .bind(ts(now))
        .bind(ms(Retention::default().journal))
        .bind(limit(max))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| Ok(RunRef::new(row.tenant(0)?, row.run_id(1)?)))
            .collect::<Result<_, Error>>()?)
    }

    async fn archive_run(
        &self,
        run: &RunRef,
        document: Digest,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        Ok(retrying(|| self.archive_once(run, document, now)).await?)
    }

    async fn runs_to_delete(&self, now: Timestamp, max: usize) -> Result<Vec<RunRef>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT r.tenant, r.id FROM runs r LEFT JOIN tenants t ON t.id = r.tenant \
             WHERE r.archive_document IS NOT NULL \
             AND r.archived_at <= $1 - COALESCE(t.archive_retention_ms, $2) \
             ORDER BY r.archived_at, r.tenant, r.id LIMIT $3",
        )
        .bind(ts(now))
        .bind(ms(Retention::default().archive))
        .bind(limit(max))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| Ok(RunRef::new(row.tenant(0)?, row.run_id(1)?)))
            .collect::<Result<_, Error>>()?)
    }

    async fn delete_run(&self, run: &RunRef, now: Timestamp) -> Result<bool, StoreError> {
        Ok(retrying(|| self.delete_run_once(run, now)).await?)
    }

    // ---------------------------------------------------------------------------------------
    // Tenants

    async fn put_tenant(&self, record: &Tenant) -> Result<(), StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::put_tenant(&mut conn, record).await?)
    }

    async fn get_tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::get_tenant(&mut conn, id).await?)
    }

    async fn list_tenants(&self) -> Result<Vec<Tenant>, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::list_tenants(&mut conn).await?)
    }

    // ---------------------------------------------------------------------------------------
    // Namespaces, their use and their alerts (`crate::namespaces`)

    async fn put_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        exceptions: Option<&Quotas>,
        now: Timestamp,
    ) -> Result<NamespaceWrite, StoreError> {
        Ok(retrying(|| async {
            let mut tx = self.pool.begin().await?;
            let written =
                namespaces::put_namespace(&mut tx, tenant_id, name, exceptions, now).await?;
            tx.commit().await?;
            Ok::<_, Fail>(written)
        })
        .await?)
    }

    async fn get_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
    ) -> Result<Option<Namespace>, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::get_namespace(&mut conn, tenant_id, name).await?)
    }

    async fn set_namespace_display_name(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        display_name: Option<&DisplayName>,
    ) -> Result<Option<NamespaceWrite>, StoreError> {
        Ok(retrying(|| async {
            let mut tx = self.pool.begin().await?;
            let written =
                namespaces::set_display_name(&mut tx, tenant_id, name, display_name).await?;
            tx.commit().await?;
            Ok::<_, Fail>(written)
        })
        .await?)
    }

    async fn namespace_display_names(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<(NamespaceName, DisplayName)>, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::display_names(&mut conn, tenant_id).await?)
    }

    async fn list_namespaces(&self, filter: &NamespaceFilter) -> Result<NamespacePage, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::list_namespaces(&mut conn, filter).await?)
    }

    async fn namespace_usage(
        &self,
        tenant_id: &TenantId,
        of: &UsageOf,
        month: Month,
    ) -> Result<Usage, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::namespace_usage(&mut conn, tenant_id, of, month).await?)
    }

    async fn namespace_has_script(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        script: Digest,
    ) -> Result<bool, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::namespace_has_script(&mut conn, tenant_id, name, script).await?)
    }

    async fn revoke_namespace_tokens(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::revoke_namespace_tokens(&mut conn, tenant_id, name, now).await?)
    }

    async fn delete_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<DeleteNamespace, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::delete_namespace(&mut conn, tenant_id, name, now).await?)
    }

    async fn namespaces_to_erase(
        &self,
        max: usize,
    ) -> Result<Vec<(TenantId, NamespaceName)>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::namespaces_to_erase(&mut conn, max).await?)
    }

    async fn drop_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
    ) -> Result<DropNamespace, StoreError> {
        Ok(retrying(|| async {
            let mut tx = self.pool.begin().await?;
            let dropped = namespaces::drop_namespace(&mut tx, tenant_id, name).await?;
            tx.commit().await?;
            Ok::<_, Fail>(dropped)
        })
        .await?)
    }

    async fn record_usage(
        &self,
        deltas: &[UsageDelta],
        now: Timestamp,
    ) -> Result<Vec<QuotaAlert>, StoreError> {
        if deltas.is_empty() {
            return Ok(Vec::new());
        }
        Ok(retrying(|| async {
            let mut tx = self.pool.begin().await?;
            let raised = namespaces::record_usage(&mut tx, deltas, now).await?;
            tx.commit().await?;
            Ok::<_, Fail>(raised)
        })
        .await?)
    }

    async fn quota_alerts(
        &self,
        tenant_id: &TenantId,
        namespace: Option<&NamespaceName>,
        month: Month,
    ) -> Result<Vec<QuotaAlert>, StoreError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::quota_alerts(&mut conn, tenant_id, namespace, month).await?)
    }

    async fn claim_quota_alerts(
        &self,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
        max: usize,
    ) -> Result<Vec<QuotaAlert>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::claim_quota_alerts(&mut conn, owner, now, lease_duration, max).await?)
    }

    async fn ack_quota_alerts(
        &self,
        keys: &[QuotaAlertKey],
        now: Timestamp,
    ) -> Result<(), StoreError> {
        if keys.is_empty() {
            return Ok(());
        }
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(namespaces::ack_quota_alerts(&mut conn, keys, now).await?)
    }

    // ---------------------------------------------------------------------------------------
    // The feed

    async fn claim_feed(
        &self,
        consumer: FeedConsumer,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
        max: usize,
    ) -> Result<Vec<FeedItem>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "UPDATE feed f SET lease_owner = $2, lease_until = $3 FROM ( \
                 SELECT id FROM feed WHERE consumer = $1 \
                 AND (lease_until IS NULL OR lease_until <= $4) \
                 ORDER BY id LIMIT $5 FOR UPDATE SKIP LOCKED) c \
             WHERE f.id = c.id \
             RETURNING f.id, f.consumer, f.tenant, f.run_id, f.fence, f.from_idx, f.to_idx",
        )
        .bind(consumer.as_str())
        .bind(owner)
        .bind(ts(now.saturating_add(lease_duration)))
        .bind(ts(now))
        .bind(limit(max))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        let mut items = rows
            .iter()
            .map(|row| {
                Ok(FeedItem {
                    id: row.u64(0)?,
                    consumer: feed_consumer(&row.text(1)?)?,
                    run: RunRef::new(row.tenant(2)?, row.run_id(3)?),
                    fence: row.u64(4)?,
                    from_idx: row.u64(5)?,
                    to_idx: row.u64(6)?,
                })
            })
            .collect::<Result<Vec<_>, Error>>()?;
        items.sort_unstable_by_key(|item| item.id);
        Ok(items)
    }

    async fn ack_feed(&self, ids: &[u64]) -> Result<(), StoreError> {
        if ids.is_empty() {
            return Ok(());
        }
        let ids: Vec<i64> = ids.iter().map(|id| int(*id)).collect();
        sqlx::query("DELETE FROM feed WHERE id = ANY($1)")
            .bind(ids)
            .execute(&self.pool)
            .await
            .map_err(Error::from)?;
        Ok(())
    }
}

impl PostgresStore {
    /// What a token update that changed nothing means.
    async fn token_update(
        &self,
        token: &TaskToken,
        changed: u64,
    ) -> Result<TaskUpdate, StoreError> {
        if changed > 0 {
            return Ok(TaskUpdate::Applied);
        }
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(not_applied(task_by_token(&mut conn, token).await?))
    }
}

/// How a run's attachments name `secret`, and the namespace whose runs may attach it: `org/<name>`
/// in every namespace, `ns/<name>` in the secret's own.
fn attached_as(secret: &SecretId) -> (String, Option<&str>) {
    let namespace = match &secret.namespace {
        SecretNamespace::Organization => None,
        SecretNamespace::Namespace(namespace) => Some(namespace.as_str()),
    };
    (secret.to_ref().to_string(), namespace)
}

/// The versions of `latest` that attach `secret`, with the servers and APIs they attach it for.
fn workflows_attaching(latest: &[StoredWorkflow], secret: &SecretRef) -> Vec<WorkflowAttaching> {
    latest
        .iter()
        .filter_map(|stored| {
            let servers: Vec<String> = stored
                .workflow
                .secrets()
                .servers_reading(secret)
                .map(str::to_owned)
                .collect();
            (!servers.is_empty()).then(|| WorkflowAttaching {
                name: stored.workflow.name().to_owned(),
                version: stored.workflow.version(),
                servers,
            })
        })
        .collect()
}
