//! `JournalStore` on Turso. The in-memory `MemoryJournal` of `rewind-engine` is the reference of
//! every behavior here; both pass the same conformance suite.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use rewind_core::{
    CancelReason, Completion, Digest, Epoch, HistoryEntry, JsonValue, Payload, RunId, RunStatus,
    Script, Seq, Timestamp, ToolFailure, ToolOutcome, ToolPolicy, Toolset, Workflow,
    end_signal_name,
};
use rewind_engine::ports::{
    AcquireRunnable, Activation, ApiKey, Archive, ClaimedTask, CompiledScript, CreateRun, Created,
    Enqueued, ExpiredRuns, Failures, FeedConsumer, FeedItem, ForkParent, InboxItem, JournalStore,
    Lease, NewRun, PAYLOAD_DELETION_TIMEOUT, ParkKind, ParkReason, ParkedScope, PayloadRecord,
    QUARANTINE_AFTER_FAILURES, Quarantine, QuarantineKind, Retention, RunFilter, RunMetadata,
    RunRecord, RunRef, RunWorkflow, StartedBy, StoreError, StoredScript, StoredToolset,
    StoredWorkflow, Subscription, TaskFilter, TaskRef, TaskState, TaskToken, TaskUpdate, Tenant,
    TenantId, TenantScope, ToolDeclarations, ToolTask, TraceContext, Trigger, Watched,
    WebhookTarget, WebhookToken, WorkflowFilter, deferred_timeout_failure,
};
use turso::{Connection, Row, Value};

use crate::codec::{self, decode_labels, encode_labels};
use crate::sql::{
    self, Columns, boolean, digest, int, limit, ms, opt_text, opt_ts, placeholders, run_id,
    scope_clause, tenant, text, transaction, ts,
};
use crate::{Error, TursoStore};

impl From<Error> for StoreError {
    fn from(err: Error) -> Self {
        StoreError::Backend(err.to_string())
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
         labels, traceparent, tracestate, parent, idempotency_key, created_at, updated_at, \
         ended_at, archive_document, archived_at, parent_at, attempts, workflow, \
         workflow_version, started_by, started_seq, detached, scope"
    };
}

/// The columns of a [`ToolTask`] (`t` the task, `r` its run), read by [`task_from_row`].
macro_rules! task_columns {
    () => {
        "r.tenant, t.run_id, t.seq, r.toolset, t.tool, t.route, t.args_digest, t.args, \
         t.on_interruption, t.approval, t.execution_timeout_ms, t.heartbeat_timeout_ms, \
         t.concurrency, t.state, t.next_attempt_at, t.lease_owner, t.lease_until, t.parked_kind, \
         t.parked_reason, t.attempt, t.created_at, t.server_concurrency, t.mcp_server, \
         t.schema_change_approved"
    };
}

fn record_from_row(row: &Row) -> Result<RunRecord, Error> {
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
    Ok(RunRecord {
        run: RunRef::new(row.tenant(1)?, row.run_id(0)?),
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
        },
        idempotency_key: row.opt_text(18)?,
        created_at: row.ts(19)?,
        updated_at: row.ts(20)?,
        ended_at: row.opt_ts(21)?,
        archive,
    })
}

/// How many columns [`task_columns`] selects.
const TASK_COLUMNS: usize = 24;

/// A task as loaded, with what the claim needs besides the port's view.
struct TaskRow {
    task: ToolTask,
    server_concurrency: Option<u32>,
    mcp_server: Option<String>,
}

fn task_from_row(row: &Row) -> Result<TaskRow, Error> {
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
    let args = codec::decode_payload(&row.text(6)?, row.opt_text(7)?.as_deref())?;
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
    })
}

/// `column IN (…)` for epochs; `None` for an empty list, which matches nothing.
fn epochs_clause(column: &str, epochs: &[Epoch], params: &mut Vec<Value>) -> Option<String> {
    if epochs.is_empty() {
        return None;
    }
    params.extend(epochs.iter().map(|epoch| int(u64::from(epoch.0))));
    Some(format!("{column} IN ({})", placeholders(epochs.len())))
}

/// An unguessable token: `prefix` and 256 bits from the operating system's CSPRNG, in hex.
fn random_token(prefix: &str) -> Result<String, StoreError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| StoreError::from(Error::Random(err.to_string())))?;
    let mut token = String::with_capacity(prefix.len() + 64);
    token.push_str(prefix);
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    Ok(token)
}

/// What a lease check reads of a run.
struct LeasedRun {
    history_len: u64,
    lease_owner: Option<String>,
    lease_until: Option<Timestamp>,
}

/// The run, if `lease` is still its current lease: its fence matches.
async fn leased(connection: &Connection, lease: &Lease) -> Result<LeasedRun, StoreError> {
    let row = sql::query_row(
        connection,
        "SELECT fence, history_len, lease_owner, lease_until FROM runs WHERE id = ? AND tenant = ?",
        vec![run_id(lease.run.id), tenant(&lease.run.tenant)],
    )
    .await?
    .ok_or(StoreError::RunNotFound(lease.run.id))?;
    if row.u64(0)? != lease.fence {
        return Err(fenced(lease));
    }
    Ok(LeasedRun {
        history_len: row.u64(1)?,
        lease_owner: row.opt_text(2)?,
        lease_until: row.opt_ts(3)?,
    })
}

fn fenced(lease: &Lease) -> StoreError {
    StoreError::Fenced {
        run_id: lease.run.id,
        fence: lease.fence,
    }
}

/// The status of a run of this tenant, or `RunNotFound`.
async fn run_status(connection: &Connection, run: &RunRef) -> Result<RunStatus, StoreError> {
    let row = sql::query_row(
        connection,
        "SELECT status FROM runs WHERE id = ? AND tenant = ?",
        vec![run_id(run.id), tenant(&run.tenant)],
    )
    .await?
    .ok_or(StoreError::RunNotFound(run.id))?;
    Ok(codec::parse_status(&row.text(0)?)?)
}

/// Whether a lease counts as an activation started, in the run's `attempts`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Attempt {
    /// `acquire_runnable`: the lease activates the run.
    Counted,
    /// `acquire_run`, `acquire_quarantined`.
    NotCounted,
}

/// Takes a lease on a run whose fence is `fence`: the fence is incremented.
async fn take_lease(
    connection: &Connection,
    run: RunRef,
    fence: u64,
    owner: &str,
    now: Timestamp,
    duration: Duration,
    attempt: Attempt,
) -> Result<Lease, StoreError> {
    let fence = fence + 1;
    sql::execute(
        connection,
        "UPDATE runs SET fence = ?, lease_owner = ?, lease_until = ?, attempts = attempts + ? \
         WHERE id = ?",
        vec![
            int(fence),
            text(owner),
            ts(now.saturating_add(duration)),
            int(u64::from(attempt == Attempt::Counted)),
            run_id(run.id),
        ],
    )
    .await?;
    Ok(Lease { run, fence })
}

async fn push_inbox(
    connection: &Connection,
    run: RunId,
    trigger: &Trigger,
    now: Timestamp,
) -> Result<(), Error> {
    sql::execute(
        connection,
        "INSERT INTO inbox (run_id, event, received_at) VALUES (?, ?, ?)",
        vec![run_id(run), text(codec::encode_trigger(trigger)), ts(now)],
    )
    .await?;
    // The calls of a store never interleave: the last row inserted is this one.
    let item = Value::Integer(connection.last_insert_rowid());
    sql::execute(
        connection,
        "UPDATE runs SET inbox_head = ?, runnable_at = ? WHERE id = ? AND inbox_head IS NULL",
        vec![item, ts(now), run_id(run)],
    )
    .await?;
    Ok(())
}

/// Points the run at its oldest inbox item again, after items were consumed.
async fn refresh_inbox_head(connection: &Connection, run: RunId) -> Result<(), Error> {
    sql::execute(
        connection,
        "UPDATE runs SET inbox_head = (SELECT MIN(id) FROM inbox WHERE run_id = ?1), \
         runnable_at = (SELECT received_at FROM inbox WHERE run_id = ?1 ORDER BY id LIMIT 1) \
         WHERE id = ?1",
        vec![run_id(run)],
    )
    .await?;
    Ok(())
}

/// Deletes a task and enqueues its outcome.
async fn finish_task(
    connection: &Connection,
    run: RunId,
    seq: Seq,
    outcome: ToolOutcome,
    now: Timestamp,
) -> Result<(), Error> {
    sql::execute(
        connection,
        "DELETE FROM tool_tasks WHERE run_id = ? AND seq = ?",
        vec![run_id(run), int(u64::from(seq.0))],
    )
    .await?;
    let completion = outcome.into_completion(seq);
    push_inbox(connection, run, &Trigger::Completion(completion), now).await
}

/// Clears what a run that has ended no longer needs.
async fn end_run(connection: &Connection, run: RunId) -> Result<(), Error> {
    for statement in [
        "DELETE FROM timers WHERE run_id = ?",
        "DELETE FROM tool_tasks WHERE run_id = ?",
        "DELETE FROM inbox WHERE run_id = ?",
        "DELETE FROM webhook_tokens WHERE run_id = ?",
        "UPDATE runs SET quarantine_kind = NULL, quarantine_reason = NULL, \
         quarantine_since = NULL, quarantine_retried_by = NULL, inbox_head = NULL, \
         runnable_at = NULL WHERE id = ?",
    ] {
        sql::execute(connection, statement, vec![run_id(run)]).await?;
    }
    Ok(())
}

/// Enqueues a completion for a run that has not ended, unless its key was already seen.
async fn enqueue_live(
    connection: &Connection,
    run: RunId,
    completion: Completion,
    idempotency_key: Option<String>,
    now: Timestamp,
) -> Result<Enqueued, Error> {
    if let Some(key) = idempotency_key {
        let inserted = sql::execute(
            connection,
            "INSERT OR IGNORE INTO signal_keys (run_id, key) VALUES (?, ?)",
            vec![run_id(run), text(key)],
        )
        .await?;
        if inserted == 0 {
            return Ok(Enqueued::Duplicate);
        }
    }
    push_inbox(connection, run, &Trigger::Completion(completion), now).await?;
    Ok(Enqueued::Accepted)
}

/// What a run's end does to the runs around it, in its terminal commit: its end to each of its
/// watchers that has not ended, and a cancellation to each run it watches attached and that has
/// not ended.
async fn propagate_end(
    connection: &Connection,
    run: RunId,
    end: Option<&JsonValue>,
    now: Timestamp,
) -> Result<(), Error> {
    if let Some(end) = end {
        let name = end_signal_name(run);
        let watchers = sql::query(
            connection,
            concat!(
                "SELECT w.watcher FROM run_watchers w JOIN runs r ON r.id = w.watcher \
                 WHERE w.run_id = ? AND r.status NOT IN ",
                terminal!(),
                " ORDER BY w.watcher"
            ),
            vec![run_id(run)],
        )
        .await?;
        for row in &watchers {
            let completion = Completion::SignalReceived {
                name: name.clone(),
                payload: Payload::Value(end.clone()),
            };
            enqueue_live(
                connection,
                row.run_id(0)?,
                completion,
                Some(name.clone()),
                now,
            )
            .await?;
        }
    }
    let attached = sql::query(
        connection,
        concat!(
            "SELECT w.run_id FROM run_watchers w JOIN runs r ON r.id = w.run_id \
             WHERE w.watcher = ? AND w.attached = 1 AND r.status NOT IN ",
            terminal!(),
            " ORDER BY w.run_id"
        ),
        vec![run_id(run)],
    )
    .await?;
    for row in &attached {
        let completion = Completion::CancelRequested {
            reason: CancelReason::Requested(Some(format!("the run {run} that started it ended"))),
        };
        enqueue_live(connection, row.run_id(0)?, completion, None, now).await?;
    }
    Ok(())
}

/// The columns of a [`StoredWorkflow`], read by [`workflow_from_row`].
macro_rules! workflow_columns {
    () => {
        "version, document, labels, created_at"
    };
}

fn workflow_from_row(row: &Row) -> Result<StoredWorkflow, Error> {
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
    })
}

/// Whether a version's labels hold all of `required`.
fn carries(labels: &BTreeMap<String, String>, required: &BTreeMap<String, String>) -> bool {
    required
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value))
}

/// The task holding `token`: a claimed task, or a deferred one.
async fn task_by_token(
    connection: &Connection,
    token: &TaskToken,
) -> Result<Option<TaskRow>, Error> {
    let row = sql::query_row(
        connection,
        concat!(
            "SELECT ",
            task_columns!(),
            " FROM tool_tasks t JOIN runs r ON r.id = t.run_id WHERE t.token = ?"
        ),
        vec![text(token.0.as_str())],
    )
    .await?;
    row.map(|row| task_from_row(&row)).transpose()
}

/// The task `seq` of a run of this tenant.
async fn task_by_ref(connection: &Connection, task: &TaskRef) -> Result<Option<TaskRow>, Error> {
    let row = sql::query_row(
        connection,
        concat!(
            "SELECT ",
            task_columns!(),
            " FROM tool_tasks t JOIN runs r ON r.id = t.run_id \
             WHERE t.run_id = ? AND t.seq = ? AND r.tenant = ?"
        ),
        vec![
            run_id(task.run.id),
            int(u64::from(task.seq.0)),
            tenant(&task.run.tenant),
        ],
    )
    .await?;
    row.map(|row| task_from_row(&row)).transpose()
}

/// The calls of a tool in flight in a run: claimed tasks whose claim has not expired, and
/// deferred tasks.
async fn tool_in_flight(
    connection: &Connection,
    (run, tool): &(RunId, String),
    now: Timestamp,
) -> Result<u32, Error> {
    let row = sql::query_row(
        connection,
        "SELECT COUNT(*) FROM tool_tasks WHERE run_id = ? AND tool = ? \
         AND ((state = 'claimed' AND lease_until > ?) OR state = 'deferred')",
        vec![run_id(*run), text(tool.as_str()), ts(now)],
    )
    .await?;
    row.map_or(Ok(0), |row| row.u32(0))
}

/// The calls in flight to an MCP server of a tenant under `owner`: its claims not expired.
async fn server_in_flight(
    connection: &Connection,
    (owner_tenant, server): &(TenantId, String),
    owner: &str,
    now: Timestamp,
) -> Result<u32, Error> {
    let row = sql::query_row(
        connection,
        "SELECT COUNT(*) FROM tool_tasks t JOIN runs r ON r.id = t.run_id \
         WHERE t.lease_owner = ? AND t.mcp_server = ? AND t.state = 'claimed' \
         AND t.lease_until > ? AND r.tenant = ?",
        vec![
            text(owner),
            text(server.as_str()),
            ts(now),
            tenant(owner_tenant),
        ],
    )
    .await?;
    row.map_or(Ok(0), |row| row.u32(0))
}

/// When a task in `state` can be claimed: a ready task at its next attempt, a claimed one once
/// its claim expires.
fn due_at(state: &TaskState) -> Option<Timestamp> {
    match state {
        TaskState::Ready { next_attempt_at } => Some(*next_attempt_at),
        TaskState::Claimed { until, .. } => Some(*until),
        TaskState::Approval | TaskState::Deferred { .. } | TaskState::Parked(_) => None,
    }
}

/// Sets a task's state, with the columns that go with it.
async fn set_task_state(
    connection: &Connection,
    task: &TaskRef,
    state: &TaskState,
    token: Option<&str>,
) -> Result<(), Error> {
    let (next_attempt_at, owner, until, parked_kind, parked_reason) = match state {
        TaskState::Approval => (None, None, None, None, None),
        TaskState::Ready { next_attempt_at } => (Some(*next_attempt_at), None, None, None, None),
        TaskState::Claimed { owner, until } => {
            (None, Some(owner.as_str()), Some(*until), None, None)
        }
        TaskState::Deferred { until } => (None, None, Some(*until), None, None),
        TaskState::Parked(reason) => (
            None,
            None,
            None,
            Some(codec::park_kind(reason.kind)),
            Some(reason.message.as_str()),
        ),
    };
    sql::execute(
        connection,
        "UPDATE tool_tasks SET state = ?, token = ?, next_attempt_at = ?, lease_owner = ?, \
         lease_until = ?, due_at = ?, parked_kind = ?, parked_reason = ? \
         WHERE run_id = ? AND seq = ?",
        vec![
            text(state.name()),
            opt_text(token),
            opt_ts(next_attempt_at),
            opt_text(owner),
            opt_ts(until),
            opt_ts(due_at(state)),
            opt_text(parked_kind),
            opt_text(parked_reason),
            run_id(task.run.id),
            int(u64::from(task.seq.0)),
        ],
    )
    .await?;
    Ok(())
}

fn tenant_from_row(row: &Row) -> Result<Tenant, Error> {
    Ok(Tenant {
        id: row.tenant(0)?,
        active_run_quota: row.opt_u32(1)?,
        retention: Retention {
            journal: row.duration(2)?,
            archive: row.duration(3)?,
            read_model: row.duration(4)?,
            payload_grace: row.duration(5)?,
        },
    })
}

fn api_key_from_row(row: &Row) -> Result<ApiKey, Error> {
    Ok(ApiKey {
        hash: row.digest(0)?,
        tenant: row.tenant(1)?,
        role: row.text(2)?,
        allowed_labels: decode_labels(&row.text(3)?)?,
        created_at: row.ts(4)?,
        revoked_at: row.opt_ts(5)?,
    })
}

fn feed_consumer(name: &str) -> Result<FeedConsumer, Error> {
    FeedConsumer::ALL
        .into_iter()
        .find(|consumer| consumer.as_str() == name)
        .ok_or_else(|| Error::Corrupt(format!("unknown feed consumer {name:?}")))
}

/// A call with a limit of 0 does nothing, and opens no transaction.
/// The conflict of a commit referencing a blob that may be gone.
fn unreserved(payload: Digest) -> StoreError {
    StoreError::Conflict(format!(
        "payload {} has no reserved row, or is being deleted: its blob may be gone",
        payload.short()
    ))
}

fn nothing(limit: usize) -> bool {
    limit == 0
}

impl JournalStore for TursoStore {
    // ---------------------------------------------------------------------------------------
    // Scripts and toolsets

    async fn put_script(
        &self,
        tenant_id: &TenantId,
        script: &Script,
        compiled: &CompiledScript,
    ) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        sql::execute(
            &connection,
            "INSERT OR IGNORE INTO scripts (tenant, digest, source, transpiled, source_map) \
             VALUES (?, ?, ?, ?, ?)",
            vec![
                tenant(tenant_id),
                digest(script.digest()),
                text(script.source()),
                text(compiled.javascript.as_str()),
                text(compiled.source_map.as_str()),
            ],
        )
        .await?;
        Ok(())
    }

    async fn get_script(
        &self,
        tenant_id: &TenantId,
        digest_of: Digest,
    ) -> Result<Option<StoredScript>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT source, transpiled, source_map FROM scripts WHERE tenant = ? AND digest = ?",
            vec![tenant(tenant_id), digest(digest_of)],
        )
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let script = Script::new(row.text(0)?);
        if script.digest() != digest_of {
            return Err(
                Error::Corrupt(format!("script {digest_of} does not hash to its key")).into(),
            );
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
        let connection = self.connection().await?;
        sql::execute(
            &connection,
            "INSERT OR IGNORE INTO toolsets (tenant, digest, spec, declarations, prompt) \
             VALUES (?, ?, ?, ?, ?)",
            vec![
                tenant(tenant_id),
                digest(toolset.digest()),
                text(rewind_format::encode_toolset(toolset)),
                text(declarations.typescript.as_str()),
                text(declarations.prompt.as_str()),
            ],
        )
        .await?;
        Ok(())
    }

    async fn get_toolset(
        &self,
        tenant_id: &TenantId,
        digest_of: Digest,
    ) -> Result<Option<StoredToolset>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT spec, declarations, prompt FROM toolsets WHERE tenant = ? AND digest = ?",
            vec![tenant(tenant_id), digest(digest_of)],
        )
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let toolset = rewind_format::decode_toolset(&row.text(0)?)
            .map_err(|err| Error::Corrupt(format!("toolset {digest_of}: {err}")))?;
        if toolset.digest() != digest_of {
            return Err(
                Error::Corrupt(format!("toolset {digest_of} does not hash to its key")).into(),
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

    async fn list_toolsets(&self, tenant_id: &TenantId) -> Result<Vec<StoredToolset>, StoreError> {
        let connection = self.connection().await?;
        let rows = sql::query_dynamic(
            &connection,
            "SELECT digest, spec, declarations, prompt FROM toolsets WHERE tenant = ? \
             ORDER BY digest",
            vec![tenant(tenant_id)],
        )
        .await?;
        let mut toolsets = Vec::with_capacity(rows.len());
        for row in &rows {
            let key = row.digest(0)?;
            let toolset = rewind_format::decode_toolset(&row.text(1)?)
                .map_err(|err| Error::Corrupt(format!("toolset {key}: {err}")))?;
            if toolset.digest() != key {
                return Err(
                    Error::Corrupt(format!("toolset {key} does not hash to its key")).into(),
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
        labels: &BTreeMap<String, String>,
        now: Timestamp,
    ) -> Result<StoredWorkflow, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let version = workflow.version();
            let existing = sql::query_row(
                connection,
                concat!(
                    "SELECT ",
                    workflow_columns!(),
                    " FROM workflows WHERE tenant = ? AND version = ?"
                ),
                vec![tenant(tenant_id), digest(version)],
            )
            .await?;
            if let Some(existing) = existing {
                return Ok(workflow_from_row(&existing)?);
            }
            // Only the versions of one workflow are compared by the order they were stored in.
            let order = sql::query_row(
                connection,
                "SELECT COALESCE(MAX(stored_order), 0) + 1 FROM workflows \
                 WHERE tenant = ? AND name = ?",
                vec![tenant(tenant_id), text(workflow.name())],
            )
            .await?
            .map_or(Ok(1), |row| row.u64(0))?;
            sql::execute(
                connection,
                "INSERT INTO workflows (tenant, version, name, document, labels, created_at, \
                 stored_order) VALUES (?, ?, ?, ?, ?, ?, ?)",
                vec![
                    tenant(tenant_id),
                    digest(version),
                    text(workflow.name()),
                    text(rewind_format::encode_workflow(workflow)),
                    text(encode_labels(labels)),
                    ts(now),
                    int(order),
                ],
            )
            .await?;
            Ok(StoredWorkflow {
                workflow: workflow.clone(),
                labels: labels.clone(),
                created_at: now,
            })
        })
        .await
    }

    async fn get_workflow(
        &self,
        tenant_id: &TenantId,
        version: Digest,
    ) -> Result<Option<StoredWorkflow>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            concat!(
                "SELECT ",
                workflow_columns!(),
                " FROM workflows WHERE tenant = ? AND version = ?"
            ),
            vec![tenant(tenant_id), digest(version)],
        )
        .await?;
        Ok(row.map(|row| workflow_from_row(&row)).transpose()?)
    }

    async fn workflow_versions(
        &self,
        tenant_id: &TenantId,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, StoreError> {
        let connection = self.connection().await?;
        let rows = sql::query(
            &connection,
            concat!(
                "SELECT ",
                workflow_columns!(),
                " FROM workflows WHERE tenant = ? AND name = ? \
                 ORDER BY created_at DESC, stored_order DESC"
            ),
            vec![tenant(tenant_id), text(name)],
        )
        .await?;
        let mut versions = Vec::new();
        for row in &rows {
            // Labels are filtered here: a version's labels are one JSON column.
            if carries(&decode_labels(&row.text(2)?)?, labels) {
                versions.push(workflow_from_row(row)?);
            }
        }
        Ok(versions)
    }

    async fn latest_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<Vec<StoredWorkflow>, StoreError> {
        if filter.limit.is_some_and(nothing) {
            return Ok(Vec::new());
        }
        let connection = self.connection().await?;
        let mut query = concat!(
            "SELECT ",
            workflow_columns!(),
            ", name FROM workflows WHERE tenant = ?"
        )
        .to_owned();
        let mut params = vec![tenant(&filter.tenant)];
        if let Some(after) = &filter.after {
            query.push_str(" AND name > ?");
            params.push(text(after.as_str()));
        }
        // Each workflow's versions, newest first: the first carrying the labels is its latest.
        query.push_str(" ORDER BY name, created_at DESC, stored_order DESC");
        let rows = sql::query_dynamic(&connection, &query, params).await?;
        let mut latest: Vec<StoredWorkflow> = Vec::new();
        for row in &rows {
            if filter.limit.is_some_and(|max| latest.len() >= max) {
                break;
            }
            let name = row.text(4)?;
            let found = latest
                .last()
                .is_some_and(|known| known.workflow.name() == name);
            if !found && carries(&decode_labels(&row.text(2)?)?, &filter.labels) {
                latest.push(workflow_from_row(row)?);
            }
        }
        Ok(latest)
    }

    // ---------------------------------------------------------------------------------------
    // The way in

    async fn create_run(
        &self,
        run: NewRun,
        idempotency_key: Option<String>,
    ) -> Result<CreateRun, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let tenant_id = &run.run.tenant;
            if let Some(key) = &idempotency_key {
                let existing = sql::query_row(
                    connection,
                    "SELECT id FROM runs WHERE tenant = ? AND idempotency_key = ?",
                    vec![tenant(tenant_id), text(key.as_str())],
                )
                .await?;
                if let Some(existing) = existing {
                    return Ok(CreateRun::Created(Created {
                        run: RunRef::new(tenant_id.clone(), existing.run_id(0)?),
                        created: false,
                    }));
                }
            }
            let taken = sql::query_row(
                connection,
                "SELECT 1 FROM runs WHERE id = ?",
                vec![run_id(run.run.id)],
            )
            .await?;
            if taken.is_some() {
                return Err(StoreError::Conflict(format!(
                    "run {} already exists",
                    run.run.id
                )));
            }
            let quota = sql::query_row(
                connection,
                "SELECT active_run_quota FROM tenants WHERE id = ?",
                vec![tenant(tenant_id)],
            )
            .await?
            .map(|row| row.opt_u32(0))
            .transpose()?
            .flatten();
            if let Some(quota) = quota {
                let active = sql::query_row(
                    connection,
                    concat!(
                        "SELECT COUNT(*) FROM runs WHERE tenant = ? AND status NOT IN ",
                        terminal!()
                    ),
                    vec![tenant(tenant_id)],
                )
                .await?
                .map(|row| row.u64(0))
                .transpose()?
                .unwrap_or(0);
                if active >= u64::from(quota) {
                    return Ok(CreateRun::QuotaExceeded { quota });
                }
            }
            let metadata = &run.metadata;
            let trace = metadata.trace_context.as_ref();
            let workflow = metadata.workflow.as_ref();
            let started_by = metadata.started_by;
            sql::execute(
                connection,
                "INSERT INTO runs (id, tenant, script, toolset, status, epoch, history_len, \
                 expires_at, expiry_enqueued, failures, attempts, fence, labels, traceparent, \
                 tracestate, parent, parent_at, idempotency_key, created_at, updated_at, \
                 workflow, workflow_version, started_by, started_seq, detached, scope) \
                 VALUES (?, ?, ?, ?, 'pending', ?, 0, ?, 0, 0, 0, 0, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
                 ?, ?, ?, ?)",
                vec![
                    run_id(run.run.id),
                    tenant(tenant_id),
                    digest(run.started.script),
                    digest(run.started.toolset),
                    int(u64::from(run.started.epoch.0)),
                    ts(run.started.expires_at),
                    text(encode_labels(&metadata.labels)),
                    opt_text(trace.map(|trace| trace.traceparent.as_str())),
                    opt_text(trace.and_then(|trace| trace.tracestate.as_deref())),
                    opt_text(metadata.parent.map(|parent| parent.run.to_string())),
                    metadata
                        .parent
                        .and_then(|parent| parent.at)
                        .map_or(Value::Null, int),
                    opt_text(idempotency_key.as_deref()),
                    ts(run.created_at),
                    ts(run.created_at),
                    opt_text(workflow.map(|workflow| workflow.name.as_str())),
                    workflow.map_or(Value::Null, |workflow| digest(workflow.version)),
                    opt_text(started_by.map(|started_by| started_by.run.to_string())),
                    started_by.map_or(Value::Null, |started_by| int(u64::from(started_by.seq.0))),
                    started_by.map_or(Value::Null, |started_by| boolean(started_by.detached)),
                    text(encode_labels(&metadata.scope)),
                ],
            )
            .await?;
            // The run that started this one watches its end, attached unless it is detached.
            if let Some(started_by) = started_by {
                sql::execute(
                    connection,
                    "INSERT OR REPLACE INTO run_watchers (run_id, watcher, seq, attached) \
                     VALUES (?, ?, ?, ?)",
                    vec![
                        run_id(run.run.id),
                        run_id(started_by.run),
                        int(u64::from(started_by.seq.0)),
                        boolean(!started_by.detached),
                    ],
                )
                .await?;
            }
            let started = Trigger::RunStarted(run.started.clone());
            push_inbox(connection, run.run.id, &started, run.created_at).await?;
            Ok(CreateRun::Created(Created {
                run: run.run.clone(),
                created: true,
            }))
        })
        .await
    }

    async fn enqueue(
        &self,
        run: &RunRef,
        completion: Completion,
        idempotency_key: Option<String>,
        now: Timestamp,
    ) -> Result<Enqueued, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            if run_status(connection, run).await?.is_terminal() {
                return Ok(Enqueued::RunEnded);
            }
            Ok(enqueue_live(connection, run.id, completion, idempotency_key, now).await?)
        })
        .await
    }

    async fn get_run(&self, run: &RunRef) -> Result<Option<RunRecord>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            concat!(
                "SELECT ",
                run_columns!(),
                " FROM runs WHERE id = ? AND tenant = ?"
            ),
            vec![run_id(run.id), tenant(&run.tenant)],
        )
        .await?;
        Ok(row.map(|row| record_from_row(&row)).transpose()?)
    }

    async fn watch_run(
        &self,
        run: &RunRef,
        watcher: RunId,
        seq: Seq,
    ) -> Result<Option<Watched>, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let row = sql::query_row(
                connection,
                "SELECT status FROM runs WHERE id = ? AND tenant = ?",
                vec![run_id(run.id), tenant(&run.tenant)],
            )
            .await?;
            let Some(row) = row else {
                return Ok(None);
            };
            let ended = codec::parse_status(&row.text(0)?)?.is_terminal();
            // A fork's watch is never attached; a watch already there is kept as it is.
            sql::execute(
                connection,
                "INSERT OR IGNORE INTO run_watchers (run_id, watcher, seq, attached) \
                 VALUES (?, ?, ?, 0)",
                vec![run_id(run.id), run_id(watcher), int(u64::from(seq.0))],
            )
            .await?;
            Ok(Some(Watched { ended }))
        })
        .await
    }

    async fn list_runs(&self, filter: &RunFilter) -> Result<Vec<RunRecord>, StoreError> {
        let connection = self.connection().await?;
        let mut query =
            concat!("SELECT ", run_columns!(), " FROM runs WHERE tenant = ?").to_owned();
        let mut params = vec![tenant(&filter.tenant)];
        if let Some(status) = filter.status {
            query.push_str(" AND status = ?");
            params.push(text(status.as_str()));
        }
        match filter.quarantined {
            Some(true) => query.push_str(" AND quarantine_kind IS NOT NULL"),
            Some(false) => query.push_str(" AND quarantine_kind IS NULL"),
            None => {}
        }
        if let Some(after) = filter.after {
            query.push_str(" AND (created_at > ? OR (created_at = ? AND id > ?))");
            params.extend([ts(after.created_at), ts(after.created_at), run_id(after.id)]);
        }
        query.push_str(" ORDER BY created_at, id");
        // Labels are filtered here, so the limit can only be applied in SQL without them.
        if let (Some(max), true) = (filter.limit, filter.labels.is_empty()) {
            query.push_str(" LIMIT ?");
            params.push(limit(max));
        }
        let rows = sql::query_dynamic(&connection, &query, params).await?;
        let mut runs = Vec::new();
        for row in &rows {
            if filter.limit.is_some_and(|max| runs.len() >= max) {
                break;
            }
            let record = record_from_row(row)?;
            let labelled = filter
                .labels
                .iter()
                .all(|(key, value)| record.metadata.labels.get(key) == Some(value));
            if labelled {
                runs.push(record);
            }
        }
        Ok(runs)
    }

    // ---------------------------------------------------------------------------------------
    // Leases and activations

    async fn acquire_runnable(
        &self,
        request: &AcquireRunnable<'_>,
    ) -> Result<Option<Lease>, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let now = request.now;
            let mut eligible_params = vec![ts(now)];
            let Some(epochs) = epochs_clause("epoch", request.epochs, &mut eligible_params) else {
                return Ok(None);
            };
            let scope = scope_clause("tenant", request.tenants, &mut eligible_params);
            let eligible = format!(
                "inbox_head > 0 AND status NOT IN {terminal} AND quarantine_kind IS NULL \
                 AND (lease_until IS NULL OR lease_until <= ?) AND {epochs} AND {scope}",
                terminal = terminal!(),
            );
            let pick = |rows: Vec<Row>| -> Result<Option<(RunRef, u64)>, Error> {
                rows.first()
                    .map(|row| Ok((RunRef::new(row.tenant(0)?, row.run_id(1)?), row.u64(2)?)))
                    .transpose()
            };

            // The caller's own runs first, as their sticky worker: oldest inbox item first.
            let mut params = eligible_params.clone();
            params.push(text(request.owner));
            let query = format!(
                "SELECT tenant, id, fence FROM runs WHERE {eligible} AND sticky_worker = ? \
                 ORDER BY inbox_head LIMIT 1"
            );
            let mut chosen = pick(sql::query_dynamic(connection, &query, params).await?)?;

            if chosen.is_none() {
                // Then the runs without a sticky worker, or whose oldest item waited the grace:
                // a tenant drawn among those with such runs, and its oldest one.
                let grace_end =
                    sql::millis(now).saturating_sub(sql::duration_ms(request.sticky_grace));
                let open = format!("{eligible} AND (sticky_worker IS NULL OR runnable_at <= ?)");
                let mut params = eligible_params.clone();
                params.push(Value::Integer(grace_end));
                // Read through the index of runnable runs, not the whole table; ordered here.
                let query = format!(
                    "SELECT DISTINCT tenant FROM runs INDEXED BY runs_runnable WHERE {open}"
                );
                let mut tenants = sql::query_dynamic(connection, &query, params.clone())
                    .await?
                    .iter()
                    .map(|row| row.text(0))
                    .collect::<Result<Vec<_>, Error>>()?;
                tenants.sort_unstable();
                if !tenants.is_empty() {
                    let drawn = (request.draw % tenants.len() as u64) as usize;
                    params.push(text(tenants.swap_remove(drawn)));
                    let query = format!(
                        "SELECT tenant, id, fence FROM runs WHERE {open} AND tenant = ? \
                         ORDER BY inbox_head LIMIT 1"
                    );
                    chosen = pick(sql::query_dynamic(connection, &query, params).await?)?;
                }
            }
            let Some((run, fence)) = chosen else {
                return Ok(None);
            };
            let lease = take_lease(
                connection,
                run,
                fence,
                request.owner,
                now,
                request.lease_duration,
                Attempt::Counted,
            )
            .await?;
            Ok(Some(lease))
        })
        .await
    }

    async fn acquire_run(
        &self,
        run: &RunRef,
        owner: &str,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let row = sql::query_row(
                connection,
                "SELECT status, lease_until, fence FROM runs WHERE id = ? AND tenant = ?",
                vec![run_id(run.id), tenant(&run.tenant)],
            )
            .await?
            .ok_or(StoreError::RunNotFound(run.id))?;
            let terminal = codec::parse_status(&row.text(0)?)?.is_terminal();
            let leased = row.opt_ts(1)?.is_some_and(|until| until > now);
            if terminal || leased {
                return Ok(None);
            }
            let lease = take_lease(
                connection,
                run.clone(),
                row.u64(2)?,
                owner,
                now,
                lease_duration,
                Attempt::NotCounted,
            )
            .await?;
            Ok(Some(lease))
        })
        .await
    }

    async fn acquire_quarantined(
        &self,
        owner: &str,
        build: &str,
        epochs: &[Epoch],
        tenants: &TenantScope,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<Option<Lease>, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let mut params = vec![ts(now), text(build)];
            let Some(epochs) = epochs_clause("epoch", epochs, &mut params) else {
                return Ok(None);
            };
            let scope = scope_clause("tenant", tenants, &mut params);
            let query = format!(
                "SELECT tenant, id, fence FROM runs \
                 WHERE status NOT IN {terminal} AND (lease_until IS NULL OR lease_until <= ?) \
                 AND quarantine_kind IS NOT NULL \
                 AND (quarantine_retried_by IS NULL OR quarantine_retried_by <> ?) AND {epochs} \
                 AND {scope} ORDER BY quarantine_since, tenant, id LIMIT 1",
                terminal = terminal!(),
            );
            let Some(row) = sql::query_dynamic(connection, &query, params)
                .await?
                .into_iter()
                .next()
            else {
                return Ok(None);
            };
            let run = RunRef::new(row.tenant(0)?, row.run_id(1)?);
            sql::execute(
                connection,
                "UPDATE runs SET quarantine_retried_by = ? WHERE id = ?",
                vec![text(build), run_id(run.id)],
            )
            .await?;
            let lease = take_lease(
                connection,
                run,
                row.u64(2)?,
                owner,
                now,
                lease_duration,
                Attempt::NotCounted,
            )
            .await?;
            Ok(Some(lease))
        })
        .await
    }

    async fn renew(
        &self,
        lease: &Lease,
        now: Timestamp,
        lease_duration: Duration,
    ) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let run = leased(connection, lease).await?;
            if run.lease_until.is_none() {
                return Err(fenced(lease));
            }
            sql::execute(
                connection,
                "UPDATE runs SET lease_until = ? WHERE id = ?",
                vec![ts(now.saturating_add(lease_duration)), run_id(lease.run.id)],
            )
            .await?;
            Ok(())
        })
        .await
    }

    async fn load_history(&self, run: &RunRef, from: u64) -> Result<Vec<HistoryEntry>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT archive_document IS NOT NULL FROM runs WHERE id = ? AND tenant = ?",
            vec![run_id(run.id), tenant(&run.tenant)],
        )
        .await?
        .ok_or(StoreError::RunNotFound(run.id))?;
        if row.boolean(0)? {
            return Err(StoreError::Archived(run.id));
        }
        let rows = sql::query(
            &connection,
            "SELECT idx, recorded_at, event FROM history WHERE run_id = ? AND idx >= ? ORDER BY idx",
            vec![run_id(run.id), int(from)],
        )
        .await?;
        let history = rows
            .iter()
            .map(|row| {
                Ok(HistoryEntry {
                    index: row.u64(0)?,
                    recorded_at: row.ts(1)?,
                    event: codec::decode_event(&row.text(2)?)?,
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok(history)
    }

    async fn take_inbox(&self, lease: &Lease) -> Result<Vec<InboxItem>, StoreError> {
        let connection = self.connection().await?;
        leased(&connection, lease).await?;
        let rows = sql::query(
            &connection,
            "SELECT id, event, received_at FROM inbox WHERE run_id = ? ORDER BY id",
            vec![run_id(lease.run.id)],
        )
        .await?;
        let items = rows
            .iter()
            .map(|row| {
                Ok(InboxItem {
                    id: row.u64(0)?,
                    trigger: codec::decode_trigger(&row.text(1)?)?,
                    received_at: row.ts(2)?,
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok(items)
    }

    async fn commit_activation(
        &self,
        lease: &Lease,
        activation: Activation,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let run = leased(connection, lease).await?;
            let id = lease.run.id;
            let from_idx = run.history_len;
            for (offset, entry) in activation.entries.iter().enumerate() {
                let expected = from_idx + offset as u64;
                if entry.index != expected {
                    return Err(StoreError::Conflict(format!(
                        "entry index {} does not continue the history of run {id} \
                         (next index {expected})",
                        entry.index
                    )));
                }
            }
            // A payload of the blob tier needs a reserved row whose deletion has not started:
            // its blob may be gone otherwise.
            for payload in activation.payloads.iter().filter(|p| p.value.is_none()) {
                let reserved = sql::query_row(
                    connection,
                    "SELECT 1 FROM payloads WHERE tenant = ? AND digest = ? \
                     AND deletion_started_at IS NULL",
                    vec![tenant(&lease.run.tenant), digest(payload.digest)],
                )
                .await?;
                if reserved.is_none() {
                    return Err(unreserved(payload.digest));
                }
            }

            for entry in &activation.entries {
                sql::execute(
                    connection,
                    "INSERT INTO history (run_id, idx, recorded_at, event) VALUES (?, ?, ?, ?)",
                    vec![
                        run_id(id),
                        int(entry.index),
                        ts(entry.recorded_at),
                        text(rewind_format::encode_event(&entry.event)),
                    ],
                )
                .await?;
            }
            let to_idx = from_idx + activation.entries.len() as u64;
            let terminal = activation.status.is_terminal();
            let last = activation.entries.last().map(|entry| entry.recorded_at);
            sql::execute(
                connection,
                "UPDATE runs SET history_len = ?, status = ?, failures = 0, attempts = 0, \
                 updated_at = COALESCE(?, updated_at), ended_at = COALESCE(?, ended_at), \
                 sticky_worker = COALESCE(?, sticky_worker) WHERE id = ?",
                vec![
                    int(to_idx),
                    text(activation.status.as_str()),
                    opt_ts(last),
                    opt_ts(last.filter(|_| terminal)),
                    opt_text(run.lease_owner.as_deref()),
                    run_id(id),
                ],
            )
            .await?;
            for consumed in &activation.consumed {
                sql::execute(
                    connection,
                    "DELETE FROM inbox WHERE run_id = ? AND id = ?",
                    vec![run_id(id), int(*consumed)],
                )
                .await?;
            }
            if !activation.consumed.is_empty() {
                refresh_inbox_head(connection, id).await?;
            }
            for task in &activation.tool_calls {
                let (args_digest, args) = codec::payload_columns(&task.args);
                // A task waits for its approval, or is ready, and due, at once.
                let (state, ready_at) = if task.policy.approval {
                    ("approval", Value::Null)
                } else {
                    ("ready", ts(now))
                };
                sql::execute(
                    connection,
                    "INSERT OR REPLACE INTO tool_tasks (run_id, seq, tool, route, mcp_server, \
                     dispatched, args_digest, args, on_interruption, approval, \
                     execution_timeout_ms, heartbeat_timeout_ms, concurrency, server_concurrency, \
                     state, attempt, token, lease_owner, lease_until, next_attempt_at, due_at, \
                     parked_kind, parked_reason, schema_change_approved, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, NULL, NULL, NULL, ?, ?, \
                     NULL, NULL, 0, ?)",
                    vec![
                        run_id(id),
                        int(u64::from(task.seq.0)),
                        text(task.tool.as_str()),
                        text(codec::encode_route(&task.route)),
                        opt_text(codec::mcp_server(&task.route)),
                        boolean(task.route.is_dispatched()),
                        text(args_digest),
                        opt_text(args),
                        text(task.policy.on_interruption.as_str()),
                        boolean(task.policy.approval),
                        task.policy.execution_timeout.map_or(Value::Null, ms),
                        ms(task.policy.heartbeat_timeout),
                        int(u64::from(task.policy.concurrency)),
                        task.server_concurrency
                            .map_or(Value::Null, |max| int(u64::from(max))),
                        text(state),
                        ready_at.clone(),
                        ready_at,
                        ts(now),
                    ],
                )
                .await?;
            }
            for timer in &activation.timers {
                sql::execute(
                    connection,
                    "INSERT OR REPLACE INTO timers (run_id, seq, fire_at) VALUES (?, ?, ?)",
                    vec![run_id(id), int(u64::from(timer.seq.0)), ts(timer.fire_at)],
                )
                .await?;
            }
            for seq in &activation.cancelled_timers {
                sql::execute(
                    connection,
                    "DELETE FROM timers WHERE run_id = ? AND seq = ?",
                    vec![run_id(id), int(u64::from(seq.0))],
                )
                .await?;
            }
            for payload in &activation.payloads {
                sql::execute(
                    connection,
                    "INSERT INTO payloads (tenant, digest, bytes, preview, value, \
                     unreferenced_since, deletion_started_at) VALUES (?, ?, ?, ?, ?, NULL, NULL) \
                     ON CONFLICT (tenant, digest) DO UPDATE SET \
                     value = COALESCE(payloads.value, excluded.value), unreferenced_since = NULL, \
                     deletion_started_at = NULL",
                    vec![
                        tenant(&lease.run.tenant),
                        digest(payload.digest),
                        int(payload.size),
                        text(payload.preview.as_str()),
                        opt_text(payload.value.as_ref().map(codec::encode_value)),
                    ],
                )
                .await?;
                sql::execute(
                    connection,
                    "INSERT OR IGNORE INTO payload_refs (tenant, digest, run_id) VALUES (?, ?, ?)",
                    vec![tenant(&lease.run.tenant), digest(payload.digest), run_id(id)],
                )
                .await?;
            }
            if !activation.entries.is_empty() {
                for consumer in FeedConsumer::ALL {
                    sql::execute(
                        connection,
                        "INSERT INTO feed (consumer, tenant, run_id, fence, from_idx, to_idx) \
                         VALUES (?, ?, ?, ?, ?, ?)",
                        vec![
                            text(consumer.as_str()),
                            tenant(&lease.run.tenant),
                            run_id(id),
                            int(lease.fence),
                            int(from_idx),
                            int(to_idx),
                        ],
                    )
                    .await?;
                }
            }
            if terminal {
                end_run(connection, id).await?;
                propagate_end(connection, id, activation.end.as_ref(), now).await?;
            }
            Ok(())
        })
        .await
    }

    async fn release(&self, lease: &Lease) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        sql::execute(
            &connection,
            "UPDATE runs SET lease_owner = NULL, lease_until = NULL \
             WHERE id = ? AND tenant = ? AND fence = ?",
            vec![
                run_id(lease.run.id),
                tenant(&lease.run.tenant),
                int(lease.fence),
            ],
        )
        .await?;
        Ok(())
    }

    async fn forget_sticky(&self, run: &RunRef, owner: &str) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        sql::execute(
            &connection,
            "UPDATE runs SET sticky_worker = NULL WHERE id = ? AND tenant = ? AND sticky_worker = ?",
            vec![run_id(run.id), tenant(&run.tenant), text(owner)],
        )
        .await?;
        Ok(())
    }

    async fn record_failure(
        &self,
        lease: &Lease,
        error: String,
        now: Timestamp,
    ) -> Result<Failures, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            leased(connection, lease).await?;
            let row = sql::query_row(
                connection,
                "SELECT failures FROM runs WHERE id = ?",
                vec![run_id(lease.run.id)],
            )
            .await?
            .ok_or(StoreError::RunNotFound(lease.run.id))?;
            let count = row.u32(0)?.saturating_add(1);
            let quarantined = count >= QUARANTINE_AFTER_FAILURES;
            if quarantined {
                sql::execute(
                    connection,
                    "UPDATE runs SET failures = ?, quarantine_kind = ?, quarantine_reason = ?, \
                     quarantine_since = ?, lease_owner = NULL, lease_until = NULL WHERE id = ?",
                    vec![
                        int(u64::from(count)),
                        text(codec::quarantine_kind(QuarantineKind::Failures)),
                        text(error),
                        ts(now),
                        run_id(lease.run.id),
                    ],
                )
                .await?;
            } else {
                sql::execute(
                    connection,
                    "UPDATE runs SET failures = ? WHERE id = ?",
                    vec![int(u64::from(count)), run_id(lease.run.id)],
                )
                .await?;
            }
            Ok(Failures { count, quarantined })
        })
        .await
    }

    async fn quarantine(
        &self,
        lease: &Lease,
        kind: QuarantineKind,
        reason: String,
        now: Timestamp,
    ) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            leased(connection, lease).await?;
            sql::execute(
                connection,
                "UPDATE runs SET quarantine_kind = ?, quarantine_reason = ?, quarantine_since = ?, \
                 lease_owner = NULL, lease_until = NULL WHERE id = ?",
                vec![
                    text(codec::quarantine_kind(kind)),
                    text(reason),
                    ts(now),
                    run_id(lease.run.id),
                ],
            )
            .await?;
            Ok(())
        })
        .await
    }

    async fn release_quarantine(&self, run: &RunRef) -> Result<bool, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let row = sql::query_row(
                connection,
                "SELECT quarantine_kind IS NOT NULL FROM runs WHERE id = ? AND tenant = ?",
                vec![run_id(run.id), tenant(&run.tenant)],
            )
            .await?
            .ok_or(StoreError::RunNotFound(run.id))?;
            sql::execute(
                connection,
                "UPDATE runs SET failures = 0, attempts = 0, quarantine_kind = NULL, \
                 quarantine_reason = NULL, quarantine_since = NULL, quarantine_retried_by = NULL \
                 WHERE id = ?",
                vec![run_id(run.id)],
            )
            .await?;
            Ok(row.boolean(0)?)
        })
        .await
    }

    // ---------------------------------------------------------------------------------------
    // Time

    async fn fire_due_timers(&self, now: Timestamp, max: usize) -> Result<usize, StoreError> {
        if nothing(max) {
            return Ok(0);
        }
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let due = sql::query(
                connection,
                "SELECT run_id, seq FROM timers WHERE fire_at <= ? \
                 ORDER BY fire_at, run_id, seq LIMIT ?",
                vec![ts(now), limit(max)],
            )
            .await?;
            for row in &due {
                let (run, seq) = (row.run_id(0)?, Seq(row.u32(1)?));
                sql::execute(
                    connection,
                    "DELETE FROM timers WHERE run_id = ? AND seq = ?",
                    vec![run_id(run), int(u64::from(seq.0))],
                )
                .await?;
                let fired = Trigger::Completion(Completion::TimerFired { seq });
                push_inbox(connection, run, &fired, now).await?;
            }
            Ok(due.len())
        })
        .await
    }

    async fn next_timer(&self) -> Result<Option<Timestamp>, StoreError> {
        let connection = self.connection().await?;
        let row =
            sql::query_row(&connection, "SELECT MIN(fire_at) FROM timers", Vec::new()).await?;
        Ok(row.map(|row| row.opt_ts(0)).transpose()?.flatten())
    }

    async fn expire_due_runs(&self, now: Timestamp, max: usize) -> Result<ExpiredRuns, StoreError> {
        if nothing(max) {
            return Ok(ExpiredRuns::default());
        }
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let due = sql::query(
                connection,
                concat!(
                    "SELECT tenant, id, quarantine_kind IS NOT NULL FROM runs WHERE status NOT IN ",
                    terminal!(),
                    " AND expires_at <= ? AND (quarantine_kind IS NOT NULL OR expiry_enqueued = 0) \
                     ORDER BY expires_at, id LIMIT ?"
                ),
                vec![ts(now), limit(max)],
            )
            .await?;
            let mut expired = ExpiredRuns::default();
            for row in &due {
                let run = RunRef::new(row.tenant(0)?, row.run_id(1)?);
                if row.boolean(2)? {
                    expired.quarantined.push(run);
                    continue;
                }
                sql::execute(
                    connection,
                    "UPDATE runs SET expiry_enqueued = 1 WHERE id = ?",
                    vec![run_id(run.id)],
                )
                .await?;
                let cancel = Trigger::Completion(Completion::CancelRequested {
                    reason: CancelReason::Ttl,
                });
                push_inbox(connection, run.id, &cancel, now).await?;
                expired.cancelled.push(run);
            }
            Ok(expired)
        })
        .await
    }

    async fn next_deadline(&self) -> Result<Option<Timestamp>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            concat!(
                "SELECT MIN(expires_at) FROM runs WHERE status NOT IN ",
                terminal!(),
                " AND expiry_enqueued = 0"
            ),
            Vec::new(),
        )
        .await?;
        Ok(row.map(|row| row.opt_ts(0)).transpose()?.flatten())
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
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let mut filter_params = Vec::new();
            let selection = match filter {
                TaskFilter::All => "1 = 1".to_owned(),
                TaskFilter::Dispatched { tenants } => format!(
                    "t.dispatched = 1 AND {}",
                    scope_clause("r.tenant", tenants, &mut filter_params)
                ),
                TaskFilter::Host {
                    tenant: host_tenant,
                    toolset,
                    tools,
                } => {
                    let mut clause = "t.dispatched = 0 AND r.tenant = ?".to_owned();
                    filter_params.push(tenant(host_tenant));
                    if let Some(toolset) = toolset {
                        clause.push_str(" AND r.toolset = ?");
                        filter_params.push(digest(*toolset));
                    }
                    if !tools.is_empty() {
                        clause.push_str(&format!(" AND t.tool IN ({})", placeholders(tools.len())));
                        filter_params.extend(tools.iter().map(|tool| text(tool.as_str())));
                    }
                    clause
                }
            };
            // Due tasks are read a page at a time, in the order they became due: a task
            // without a free concurrency slot is skipped, so a claim may read past `max`.
            let page = max.saturating_mul(2).max(32);
            let first_page = format!(
                "SELECT {columns}, t.due_at FROM tool_tasks t JOIN runs r ON r.id = t.run_id \
                 WHERE t.due_at <= ? AND r.status NOT IN {terminal} AND {selection} \
                 ORDER BY t.due_at, t.run_id, t.seq LIMIT ?",
                columns = task_columns!(),
                terminal = terminal!(),
            );
            let next_page = format!(
                "SELECT {columns}, t.due_at FROM tool_tasks t JOIN runs r ON r.id = t.run_id \
                 WHERE t.due_at <= ? AND r.status NOT IN {terminal} AND {selection} \
                 AND (t.due_at > ? OR (t.due_at = ? AND (t.run_id > ? \
                      OR (t.run_id = ? AND t.seq > ?)))) \
                 ORDER BY t.due_at, t.run_id, t.seq LIMIT ?",
                columns = task_columns!(),
                terminal = terminal!(),
            );

            // Calls in flight, per (run, tool) and per (tenant, MCP server) under this owner,
            // counted the first time a due task needs them, then kept up to date here.
            let mut per_tool: HashMap<(RunId, String), u32> = HashMap::new();
            let mut per_server: HashMap<(TenantId, String), u32> = HashMap::new();
            let mut claimed = Vec::new();
            let mut after: Option<(Value, RunId, Seq)> = None;
            'pages: loop {
                let mut params = vec![ts(now)];
                params.extend(filter_params.iter().cloned());
                let query = match &after {
                    None => &first_page,
                    Some((due_at, run, seq)) => {
                        params.extend([
                            due_at.clone(),
                            due_at.clone(),
                            run_id(*run),
                            run_id(*run),
                            int(u64::from(seq.0)),
                        ]);
                        &next_page
                    }
                };
                params.push(limit(page));
                let due = sql::query_dynamic(connection, query, params).await?;
                for row in &due {
                    if claimed.len() >= max {
                        break 'pages;
                    }
                    let TaskRow {
                        mut task,
                        server_concurrency,
                        mcp_server,
                    } = task_from_row(row)?;
                    after = Some((row.column(TASK_COLUMNS)?, task.task.run.id, task.task.seq));

                    let tool_key = (task.task.run.id, task.tool.clone());
                    let in_flight = match per_tool.get(&tool_key) {
                        Some(count) => *count,
                        None => {
                            let count = tool_in_flight(connection, &tool_key, now).await?;
                            per_tool.insert(tool_key.clone(), count);
                            count
                        }
                    };
                    if in_flight >= task.policy.concurrency {
                        continue;
                    }
                    let server_key =
                        mcp_server.map(|server| (task.task.run.tenant.clone(), server));
                    if let (Some(key), Some(max_in_flight)) = (&server_key, server_concurrency) {
                        let in_flight = match per_server.get(key) {
                            Some(count) => *count,
                            None => {
                                let count = server_in_flight(connection, key, owner, now).await?;
                                per_server.insert(key.clone(), count);
                                count
                            }
                        };
                        if in_flight >= max_in_flight {
                            continue;
                        }
                    }
                    *per_tool.entry(tool_key).or_default() += 1;
                    if let Some(key) = server_key {
                        *per_server.entry(key).or_default() += 1;
                    }

                    let previous_claim_expired = matches!(task.state, TaskState::Claimed { .. });
                    let token = random_token("rewind_tt_")?;
                    let until = now.saturating_add(task.policy.heartbeat_timeout);
                    task.attempt += 1;
                    task.state = TaskState::Claimed {
                        owner: owner.to_owned(),
                        until,
                    };
                    sql::execute(
                        connection,
                        "UPDATE tool_tasks SET state = 'claimed', token = ?, attempt = ?, \
                         lease_owner = ?, lease_until = ?, due_at = ?, next_attempt_at = NULL, \
                         parked_kind = NULL, parked_reason = NULL WHERE run_id = ? AND seq = ?",
                        vec![
                            text(token.as_str()),
                            int(u64::from(task.attempt)),
                            text(owner),
                            ts(until),
                            ts(until),
                            run_id(task.task.run.id),
                            int(u64::from(task.task.seq.0)),
                        ],
                    )
                    .await?;
                    claimed.push(ClaimedTask {
                        task,
                        token: TaskToken(token),
                        previous_claim_expired,
                    });
                }
                if due.len() < page {
                    break;
                }
            }
            Ok(claimed)
        })
        .await
    }

    async fn heartbeat_tool_task(
        &self,
        token: &TaskToken,
        now: Timestamp,
    ) -> Result<Option<Timestamp>, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let Some(found) = task_by_token(connection, token).await? else {
                return Ok(None);
            };
            match found.task.state {
                TaskState::Claimed { owner, .. } => {
                    let until = now.saturating_add(found.task.policy.heartbeat_timeout);
                    let state = TaskState::Claimed { owner, until };
                    set_task_state(connection, &found.task.task, &state, Some(&token.0)).await?;
                    Ok(Some(until))
                }
                TaskState::Deferred { until } => Ok(Some(until)),
                _ => Ok(None),
            }
        })
        .await
    }

    async fn next_tool_attempt(&self, after: Timestamp) -> Result<Option<Timestamp>, StoreError> {
        let connection = self.connection().await?;
        // A ready task is due at its next attempt: `tool_tasks_due` holds it.
        let row = sql::query_row(
            &connection,
            "SELECT MIN(due_at) FROM tool_tasks WHERE state = 'ready' AND due_at > ?",
            vec![ts(after)],
        )
        .await?;
        Ok(row.map(|row| row.opt_ts(0)).transpose()?.flatten())
    }

    async fn retry_tool_task(
        &self,
        token: &TaskToken,
        next_attempt_at: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let Some(found) = task_by_token(connection, token).await? else {
                return Ok(TaskUpdate::NotFound);
            };
            if !matches!(found.task.state, TaskState::Claimed { .. }) {
                return Ok(TaskUpdate::InvalidState(found.task.state));
            }
            let state = TaskState::Ready { next_attempt_at };
            set_task_state(connection, &found.task.task, &state, None).await?;
            Ok(TaskUpdate::Applied)
        })
        .await
    }

    async fn defer_tool_task(
        &self,
        token: &TaskToken,
        until: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let Some(found) = task_by_token(connection, token).await? else {
                return Ok(TaskUpdate::NotFound);
            };
            if !matches!(found.task.state, TaskState::Claimed { .. }) {
                return Ok(TaskUpdate::InvalidState(found.task.state));
            }
            let state = TaskState::Deferred { until };
            set_task_state(connection, &found.task.task, &state, Some(&token.0)).await?;
            Ok(TaskUpdate::Applied)
        })
        .await
    }

    async fn park_tool_task(
        &self,
        token: &TaskToken,
        reason: ParkReason,
    ) -> Result<TaskUpdate, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let Some(found) = task_by_token(connection, token).await? else {
                return Ok(TaskUpdate::NotFound);
            };
            if !matches!(found.task.state, TaskState::Claimed { .. }) {
                return Ok(TaskUpdate::InvalidState(found.task.state));
            }
            let state = TaskState::Parked(reason);
            set_task_state(connection, &found.task.task, &state, None).await?;
            Ok(TaskUpdate::Applied)
        })
        .await
    }

    async fn complete_tool_task(
        &self,
        token: &TaskToken,
        outcome: ToolOutcome,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let Some(found) = task_by_token(connection, token).await? else {
                return Ok(TaskUpdate::NotFound);
            };
            let task = &found.task.task;
            finish_task(connection, task.run.id, task.seq, outcome, now).await?;
            Ok(TaskUpdate::Applied)
        })
        .await
    }

    async fn approve_tool_task(
        &self,
        task: &TaskRef,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let Some(found) = task_by_ref(connection, task).await? else {
                return Ok(TaskUpdate::NotFound);
            };
            let schema_changed = match &found.task.state {
                TaskState::Approval => false,
                TaskState::Parked(reason) => reason.kind == ParkKind::SchemaChanged,
                other => return Ok(TaskUpdate::InvalidState(other.clone())),
            };
            let state = TaskState::Ready {
                next_attempt_at: now,
            };
            set_task_state(connection, task, &state, None).await?;
            if schema_changed {
                // The harness let the call go on against the tool's current schema.
                sql::execute(
                    connection,
                    "UPDATE tool_tasks SET schema_change_approved = 1 WHERE run_id = ? AND seq = ?",
                    vec![run_id(task.run.id), int(u64::from(task.seq.0))],
                )
                .await?;
            }
            Ok(TaskUpdate::Applied)
        })
        .await
    }

    async fn reject_tool_task(
        &self,
        task: &TaskRef,
        failure: ToolFailure,
        now: Timestamp,
    ) -> Result<TaskUpdate, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let Some(found) = task_by_ref(connection, task).await? else {
                return Ok(TaskUpdate::NotFound);
            };
            match found.task.state {
                TaskState::Approval | TaskState::Parked(_) => {
                    let outcome = ToolOutcome::Failure(failure);
                    finish_task(connection, task.run.id, task.seq, outcome, now).await?;
                    Ok(TaskUpdate::Applied)
                }
                other => Ok(TaskUpdate::InvalidState(other)),
            }
        })
        .await
    }

    async fn resume_parked(
        &self,
        scope: &ParkedScope,
        now: Timestamp,
    ) -> Result<usize, StoreError> {
        let connection = self.connection().await?;
        let (runs, mut params) = match scope {
            ParkedScope::Run(run) => (
                "SELECT id FROM runs WHERE tenant = ? AND id = ?",
                vec![tenant(&run.tenant), run_id(run.id)],
            ),
            ParkedScope::Toolset {
                tenant: owner,
                toolset,
            } => (
                "SELECT id FROM runs WHERE tenant = ? AND toolset = ?",
                vec![tenant(owner), digest(*toolset)],
            ),
        };
        params.splice(0..0, [ts(now), ts(now)]);
        let query = format!(
            "UPDATE tool_tasks SET state = 'ready', next_attempt_at = ?, due_at = ?, \
             parked_kind = NULL, parked_reason = NULL \
             WHERE state = 'parked' AND parked_kind = 'credentials_expired' AND run_id IN ({runs})"
        );
        let resumed = sql::execute(&connection, &query, params).await?;
        Ok(usize::try_from(resumed).unwrap_or(usize::MAX))
    }

    async fn expire_deferred_tool_tasks(
        &self,
        now: Timestamp,
        max: usize,
    ) -> Result<usize, StoreError> {
        if nothing(max) {
            return Ok(0);
        }
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let due = sql::query(
                connection,
                "SELECT run_id, seq FROM tool_tasks WHERE state = 'deferred' AND lease_until <= ? \
                 ORDER BY lease_until, run_id, seq LIMIT ?",
                vec![ts(now), limit(max)],
            )
            .await?;
            for row in &due {
                let failure = ToolOutcome::Failure(deferred_timeout_failure());
                finish_task(connection, row.run_id(0)?, Seq(row.u32(1)?), failure, now).await?;
            }
            Ok(due.len())
        })
        .await
    }

    async fn tool_task(&self, token: &TaskToken) -> Result<Option<ToolTask>, StoreError> {
        let connection = self.connection().await?;
        Ok(task_by_token(&connection, token)
            .await?
            .map(|found| found.task))
    }

    async fn tool_tasks(&self, run: &RunRef) -> Result<Vec<ToolTask>, StoreError> {
        let connection = self.connection().await?;
        run_status(&connection, run).await?;
        let rows = sql::query(
            &connection,
            concat!(
                "SELECT ",
                task_columns!(),
                " FROM tool_tasks t JOIN runs r ON r.id = t.run_id WHERE t.run_id = ? \
                 ORDER BY t.seq"
            ),
            vec![run_id(run.id)],
        )
        .await?;
        let tasks = rows
            .iter()
            .map(|row| task_from_row(row).map(|found| found.task))
            .collect::<Result<_, Error>>()?;
        Ok(tasks)
    }

    // ---------------------------------------------------------------------------------------
    // Webhooks

    async fn create_webhook_token(
        &self,
        run: &RunRef,
        name: &str,
    ) -> Result<WebhookToken, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            if run_status(connection, run).await?.is_terminal() {
                return Err(StoreError::Conflict(format!("run {} has ended", run.id)));
            }
            let existing = sql::query_row(
                connection,
                "SELECT token FROM webhook_tokens WHERE run_id = ? AND name = ?",
                vec![run_id(run.id), text(name)],
            )
            .await?;
            if let Some(row) = existing {
                return Ok(WebhookToken(row.text(0)?));
            }
            let token = random_token("rewind_wh_")?;
            sql::execute(
                connection,
                "INSERT INTO webhook_tokens (token, run_id, name) VALUES (?, ?, ?)",
                vec![text(token.as_str()), run_id(run.id), text(name)],
            )
            .await?;
            Ok(WebhookToken(token))
        })
        .await
    }

    async fn resolve_webhook_token(
        &self,
        token: &WebhookToken,
    ) -> Result<Option<WebhookTarget>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            concat!(
                "SELECT r.tenant, r.id, w.name FROM webhook_tokens w JOIN runs r ON r.id = w.run_id \
                 WHERE w.token = ? AND r.status NOT IN ",
                terminal!()
            ),
            vec![text(token.0.as_str())],
        )
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(WebhookTarget {
            run: RunRef::new(row.tenant(0)?, row.run_id(1)?),
            name: row.text(2)?,
        }))
    }

    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        sql::execute(
            &connection,
            "INSERT OR REPLACE INTO subscriptions (tenant, id, url, secret, events, labels) \
             VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                tenant(&subscription.tenant),
                text(subscription.id.as_str()),
                text(subscription.url.as_str()),
                text(subscription.secret.expose()),
                text(codec::encode_event_types(&subscription.events)),
                text(encode_labels(&subscription.labels)),
            ],
        )
        .await?;
        Ok(())
    }

    async fn subscriptions(&self, tenant_id: &TenantId) -> Result<Vec<Subscription>, StoreError> {
        let connection = self.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT id, url, secret, events, labels FROM subscriptions WHERE tenant = ? \
             ORDER BY id",
            vec![tenant(tenant_id)],
        )
        .await?;
        let subscriptions = rows
            .iter()
            .map(|row| {
                Ok(Subscription {
                    tenant: tenant_id.clone(),
                    id: row.text(0)?,
                    url: row.text(1)?,
                    secret: rewind_engine::ports::Secret::new(row.text(2)?),
                    events: codec::decode_event_types(&row.text(3)?)?,
                    labels: decode_labels(&row.text(4)?)?,
                })
            })
            .collect::<Result<_, Error>>()?;
        Ok(subscriptions)
    }

    async fn delete_subscription(
        &self,
        tenant_id: &TenantId,
        id: &str,
    ) -> Result<bool, StoreError> {
        let connection = self.connection().await?;
        let deleted = sql::execute(
            &connection,
            "DELETE FROM subscriptions WHERE tenant = ? AND id = ?",
            vec![tenant(tenant_id), text(id)],
        )
        .await?;
        Ok(deleted > 0)
    }

    // ---------------------------------------------------------------------------------------
    // Payloads

    async fn get_payload(
        &self,
        tenant_id: &TenantId,
        digest_of: Digest,
    ) -> Result<Option<PayloadRecord>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT bytes, preview, value FROM payloads WHERE tenant = ? AND digest = ?",
            vec![tenant(tenant_id), digest(digest_of)],
        )
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(PayloadRecord {
            digest: digest_of,
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
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let mut deleting = Vec::new();
            for payload in payloads {
                let row = sql::query_row(
                    connection,
                    "SELECT deletion_started_at IS NOT NULL FROM payloads \
                     WHERE tenant = ? AND digest = ?",
                    vec![tenant(tenant_id), digest(payload.digest)],
                )
                .await?;
                match row {
                    Some(row) if row.boolean(0)? => deleting.push(payload.digest),
                    // A row without references starts its grace again.
                    Some(_) => {
                        sql::execute(
                            connection,
                            "UPDATE payloads SET unreferenced_since = MAX(unreferenced_since, ?) \
                             WHERE tenant = ? AND digest = ? AND unreferenced_since IS NOT NULL",
                            vec![ts(now), tenant(tenant_id), digest(payload.digest)],
                        )
                        .await?;
                    }
                    None => {
                        sql::execute(
                            connection,
                            "INSERT INTO payloads (tenant, digest, bytes, preview, value, \
                             unreferenced_since, deletion_started_at) \
                             VALUES (?, ?, ?, ?, ?, ?, NULL)",
                            vec![
                                tenant(tenant_id),
                                digest(payload.digest),
                                int(payload.size),
                                text(payload.preview.as_str()),
                                opt_text(payload.value.as_ref().map(codec::encode_value)),
                                ts(now),
                            ],
                        )
                        .await?;
                    }
                }
            }
            Ok(deleting)
        })
        .await
    }

    async fn unreferenced_payloads(
        &self,
        now: Timestamp,
        max: usize,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        let connection = self.connection().await?;
        let default_grace = sql::duration_ms(Retention::default().payload_grace);
        let rows = sql::query(
            &connection,
            "SELECT p.tenant, p.digest FROM payloads p LEFT JOIN tenants t ON t.id = p.tenant \
             WHERE p.unreferenced_since IS NOT NULL \
             AND p.unreferenced_since <= ? - COALESCE(t.payload_grace_ms, ?) \
             AND (p.deletion_started_at IS NULL OR p.deletion_started_at <= ? - ?) \
             AND NOT EXISTS (SELECT 1 FROM payload_refs r \
                             WHERE r.tenant = p.tenant AND r.digest = p.digest) \
             ORDER BY p.tenant, p.digest LIMIT ?",
            vec![
                ts(now),
                Value::Integer(default_grace),
                ts(now),
                ms(PAYLOAD_DELETION_TIMEOUT),
                limit(max),
            ],
        )
        .await?;
        let payloads = rows
            .iter()
            .map(|row| Ok((row.tenant(0)?, row.digest(1)?)))
            .collect::<Result<_, Error>>()?;
        Ok(payloads)
    }

    async fn forget_payloads(
        &self,
        payloads: &[(TenantId, Digest)],
        now: Timestamp,
    ) -> Result<Vec<(TenantId, Digest)>, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        let default_grace = sql::duration_ms(Retention::default().payload_grace);
        transaction(connection, async {
            let mut forgotten = Vec::new();
            for (owner, payload) in payloads {
                let started = sql::execute(
                    connection,
                    "UPDATE payloads SET deletion_started_at = ? WHERE tenant = ? AND digest = ? \
                     AND unreferenced_since IS NOT NULL \
                     AND unreferenced_since <= ? - COALESCE( \
                         (SELECT payload_grace_ms FROM tenants WHERE id = payloads.tenant), ?) \
                     AND (deletion_started_at IS NULL OR deletion_started_at <= ? - ?) \
                     AND NOT EXISTS (SELECT 1 FROM payload_refs r \
                                     WHERE r.tenant = payloads.tenant \
                                     AND r.digest = payloads.digest)",
                    vec![
                        ts(now),
                        tenant(owner),
                        digest(*payload),
                        ts(now),
                        Value::Integer(default_grace),
                        ts(now),
                        ms(PAYLOAD_DELETION_TIMEOUT),
                    ],
                )
                .await?;
                if started > 0 {
                    forgotten.push((owner.clone(), *payload));
                }
            }
            Ok(forgotten)
        })
        .await
    }

    async fn drop_payloads(
        &self,
        payloads: &[(TenantId, Digest)],
        started: Timestamp,
    ) -> Result<usize, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let mut dropped = 0;
            for (owner, payload) in payloads {
                let deleted = sql::execute(
                    connection,
                    "DELETE FROM payloads WHERE tenant = ? AND digest = ? \
                     AND deletion_started_at = ? \
                     AND NOT EXISTS (SELECT 1 FROM payload_refs r \
                                     WHERE r.tenant = payloads.tenant \
                                     AND r.digest = payloads.digest)",
                    vec![tenant(owner), digest(*payload), ts(started)],
                )
                .await?;
                dropped += usize::try_from(deleted).unwrap_or(usize::MAX);
            }
            Ok(dropped)
        })
        .await
    }

    // ---------------------------------------------------------------------------------------
    // Retention

    async fn runs_to_archive(&self, now: Timestamp, max: usize) -> Result<Vec<RunRef>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        let connection = self.connection().await?;
        let default_journal = sql::duration_ms(Retention::default().journal);
        let rows = sql::query(
            &connection,
            concat!(
                "SELECT r.tenant, r.id FROM runs r LEFT JOIN tenants t ON t.id = r.tenant \
                 WHERE r.status IN ",
                terminal!(),
                " AND r.archive_document IS NULL AND r.ended_at IS NOT NULL \
                 AND r.ended_at <= ? - COALESCE(t.journal_retention_ms, ?) \
                 AND NOT EXISTS (SELECT 1 FROM feed f WHERE f.run_id = r.id) \
                 ORDER BY r.ended_at, r.tenant, r.id LIMIT ?"
            ),
            vec![ts(now), Value::Integer(default_journal), limit(max)],
        )
        .await?;
        let runs = rows
            .iter()
            .map(|row| Ok(RunRef::new(row.tenant(0)?, row.run_id(1)?)))
            .collect::<Result<_, Error>>()?;
        Ok(runs)
    }

    async fn archive_run(
        &self,
        run: &RunRef,
        document: Digest,
        now: Timestamp,
    ) -> Result<bool, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let row = sql::query_row(
                connection,
                "SELECT status, archive_document IS NOT NULL, \
                 EXISTS (SELECT 1 FROM feed WHERE run_id = runs.id) \
                 FROM runs WHERE id = ? AND tenant = ?",
                vec![run_id(run.id), tenant(&run.tenant)],
            )
            .await?;
            let Some(row) = row else {
                return Ok(false);
            };
            // A run with feed items left keeps its history until they are applied.
            if !codec::parse_status(&row.text(0)?)?.is_terminal()
                || row.boolean(1)?
                || row.boolean(2)?
            {
                return Ok(false);
            }
            for statement in [
                "DELETE FROM history WHERE run_id = ?",
                "DELETE FROM inbox WHERE run_id = ?",
            ] {
                sql::execute(connection, statement, vec![run_id(run.id)]).await?;
            }
            sql::execute(
                connection,
                "UPDATE runs SET archive_document = ?, archived_at = ?, inbox_head = NULL, \
                 runnable_at = NULL WHERE id = ?",
                vec![digest(document), ts(now), run_id(run.id)],
            )
            .await?;
            Ok(true)
        })
        .await
    }

    async fn runs_to_delete(&self, now: Timestamp, max: usize) -> Result<Vec<RunRef>, StoreError> {
        if nothing(max) {
            return Ok(Vec::new());
        }
        let connection = self.connection().await?;
        let default_archive = sql::duration_ms(Retention::default().archive);
        let rows = sql::query(
            &connection,
            "SELECT r.tenant, r.id FROM runs r LEFT JOIN tenants t ON t.id = r.tenant \
             WHERE r.archive_document IS NOT NULL \
             AND r.archived_at <= ? - COALESCE(t.archive_retention_ms, ?) \
             ORDER BY r.archived_at, r.tenant, r.id LIMIT ?",
            vec![ts(now), Value::Integer(default_archive), limit(max)],
        )
        .await?;
        let runs = rows
            .iter()
            .map(|row| Ok(RunRef::new(row.tenant(0)?, row.run_id(1)?)))
            .collect::<Result<_, Error>>()?;
        Ok(runs)
    }

    async fn delete_run(&self, run: &RunRef, now: Timestamp) -> Result<bool, StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let exists = sql::query_row(
                connection,
                "SELECT 1 FROM runs WHERE id = ? AND tenant = ?",
                vec![run_id(run.id), tenant(&run.tenant)],
            )
            .await?;
            if exists.is_none() {
                return Ok(false);
            }
            let referenced = sql::query(
                connection,
                "SELECT digest FROM payload_refs WHERE tenant = ? AND run_id = ?",
                vec![tenant(&run.tenant), run_id(run.id)],
            )
            .await?;
            sql::execute(
                connection,
                "DELETE FROM payload_refs WHERE tenant = ? AND run_id = ?",
                vec![tenant(&run.tenant), run_id(run.id)],
            )
            .await?;
            for row in &referenced {
                // A payload left without references starts its grace now.
                sql::execute(
                    connection,
                    "UPDATE payloads SET unreferenced_since = ? WHERE tenant = ? AND digest = ? \
                     AND NOT EXISTS (SELECT 1 FROM payload_refs r \
                                     WHERE r.tenant = payloads.tenant \
                                     AND r.digest = payloads.digest)",
                    vec![ts(now), tenant(&run.tenant), text(row.text(0)?)],
                )
                .await?;
            }
            for statement in [
                "DELETE FROM history WHERE run_id = ?",
                "DELETE FROM inbox WHERE run_id = ?",
                "DELETE FROM signal_keys WHERE run_id = ?",
                "DELETE FROM timers WHERE run_id = ?",
                "DELETE FROM tool_tasks WHERE run_id = ?",
                "DELETE FROM webhook_tokens WHERE run_id = ?",
                "DELETE FROM feed WHERE run_id = ?",
                "DELETE FROM run_watchers WHERE run_id = ?",
                "DELETE FROM run_watchers WHERE watcher = ?",
                "DELETE FROM runs WHERE id = ?",
            ] {
                sql::execute(connection, statement, vec![run_id(run.id)]).await?;
            }
            Ok(true)
        })
        .await
    }

    // ---------------------------------------------------------------------------------------
    // Tenants and API keys

    async fn put_tenant(&self, record: &Tenant) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        sql::execute(
            &connection,
            "INSERT OR REPLACE INTO tenants (id, active_run_quota, journal_retention_ms, \
             archive_retention_ms, read_model_retention_ms, payload_grace_ms) \
             VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                tenant(&record.id),
                record
                    .active_run_quota
                    .map_or(Value::Null, |quota| int(u64::from(quota))),
                ms(record.retention.journal),
                ms(record.retention.archive),
                ms(record.retention.read_model),
                ms(record.retention.payload_grace),
            ],
        )
        .await?;
        Ok(())
    }

    async fn get_tenant(&self, id: &TenantId) -> Result<Option<Tenant>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT id, active_run_quota, journal_retention_ms, archive_retention_ms, \
             read_model_retention_ms, payload_grace_ms FROM tenants WHERE id = ?",
            vec![tenant(id)],
        )
        .await?;
        Ok(row.map(|row| tenant_from_row(&row)).transpose()?)
    }

    async fn list_tenants(&self) -> Result<Vec<Tenant>, StoreError> {
        let connection = self.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT id, active_run_quota, journal_retention_ms, archive_retention_ms, \
             read_model_retention_ms, payload_grace_ms FROM tenants ORDER BY id",
            Vec::new(),
        )
        .await?;
        Ok(rows
            .iter()
            .map(tenant_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn put_api_key(&self, key: &ApiKey) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let exists = sql::query_row(
                connection,
                "SELECT 1 FROM api_keys WHERE hash = ?",
                vec![digest(key.hash)],
            )
            .await?;
            if exists.is_some() {
                return Err(StoreError::Conflict(format!(
                    "an API key with hash {} already exists",
                    key.hash.short()
                )));
            }
            sql::execute(
                connection,
                "INSERT INTO api_keys (hash, tenant, role, allowed_labels, created_at, revoked_at) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                vec![
                    digest(key.hash),
                    tenant(&key.tenant),
                    text(key.role.as_str()),
                    text(encode_labels(&key.allowed_labels)),
                    ts(key.created_at),
                    opt_ts(key.revoked_at),
                ],
            )
            .await?;
            Ok(())
        })
        .await
    }

    async fn find_api_key(&self, hash: Digest) -> Result<Option<ApiKey>, StoreError> {
        let connection = self.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT hash, tenant, role, allowed_labels, created_at, revoked_at FROM api_keys \
             WHERE hash = ?",
            vec![digest(hash)],
        )
        .await?;
        Ok(row.map(|row| api_key_from_row(&row)).transpose()?)
    }

    async fn revoke_api_key(&self, hash: Digest, now: Timestamp) -> Result<bool, StoreError> {
        let connection = self.connection().await?;
        let revoked = sql::execute(
            &connection,
            "UPDATE api_keys SET revoked_at = ? WHERE hash = ? AND revoked_at IS NULL",
            vec![ts(now), digest(hash)],
        )
        .await?;
        Ok(revoked > 0)
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
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let rows = sql::query(
                connection,
                "SELECT id, consumer, tenant, run_id, fence, from_idx, to_idx FROM feed \
                 WHERE consumer = ? AND (lease_until IS NULL OR lease_until <= ?) \
                 ORDER BY id LIMIT ?",
                vec![text(consumer.as_str()), ts(now), limit(max)],
            )
            .await?;
            let until = now.saturating_add(lease_duration);
            let mut items = Vec::with_capacity(rows.len());
            for row in &rows {
                let item = FeedItem {
                    id: row.u64(0)?,
                    consumer: feed_consumer(&row.text(1)?)?,
                    run: RunRef::new(row.tenant(2)?, row.run_id(3)?),
                    fence: row.u64(4)?,
                    from_idx: row.u64(5)?,
                    to_idx: row.u64(6)?,
                };
                sql::execute(
                    connection,
                    "UPDATE feed SET lease_owner = ?, lease_until = ? WHERE id = ?",
                    vec![text(owner), ts(until), int(item.id)],
                )
                .await?;
                items.push(item);
            }
            Ok(items)
        })
        .await
    }

    async fn ack_feed(&self, ids: &[u64]) -> Result<(), StoreError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            for id in ids {
                sql::execute(connection, "DELETE FROM feed WHERE id = ?", vec![int(*id)]).await?;
            }
            Ok(())
        })
        .await
    }
}
