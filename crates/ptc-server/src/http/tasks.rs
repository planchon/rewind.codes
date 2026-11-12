//! Host-tool tasks: the harness's SDK worker long-polls them and answers through the claim's
//! token; the harness approves or rejects the calls that wait for it (`archi/04-tools.md`,
//! "Tool task lifecycle").

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use ptc_api::ProblemType;
use ptc_api::tasks::{
    CompleteTask, DeferTask, FailTask, HeartbeatAck, RejectTask, Task, TaskBatch, TaskQuery,
};
use ptc_core::canonical_json;
use ptc_engine::ports::{JournalStore, TaskToken};
use ptc_engine::{HostClaim, HostFailure, HostTask};

use crate::app::App;
use crate::auth::{Access, Caller};
use crate::error::ApiError;
use crate::extract::{Body, OptionalBody, Query};
use crate::http::refuse_nul_in_filter;
use crate::http::runs::check_payload_size;
use crate::http::toolsets::parse_digest;
use crate::views::{parse_task_id, task_id};
use crate::wake::{POLL, wait};

/// The most a failure holds: its message and its data as canonical JSON, together. A failure
/// stays in its journal entry, which never moves a value to the payload tiers: it is bounded by
/// the largest value a row holds next to a digest (08).
const FAILURE_LIMIT: usize = 64 * 1024;

/// How long a claim waits for a task by default, and at most.
const WAIT: Duration = Duration::from_secs(30);
const MAX_WAIT: Duration = Duration::from_secs(60);

/// `GET /v1/tasks`: claims up to `limit` ready host-tool tasks, waiting up to `wait` for one.
pub(super) async fn claim(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(query): Query<TaskQuery>,
) -> Result<Json<TaskBatch>, ApiError> {
    caller.allow(Access::Tasks)?;
    if caller.scoped() {
        return Err(ApiError::forbidden(
            "a key limited to labels cannot claim host-tool tasks: use a worker key of the tenant",
        ));
    }
    let toolset = query.toolset.as_deref().map(parse_digest).transpose()?;
    for tool in &query.tools {
        refuse_nul_in_filter("tool", tool)?;
    }
    let claim = HostClaim {
        tenant: caller.tenant.clone(),
        toolset,
        tools: query.tools.into_iter().collect::<BTreeSet<_>>(),
        limit: usize::try_from(query.limit.unwrap_or(1).clamp(1, 100)).unwrap_or(1),
    };
    let wait_for = query.wait.map_or(WAIT, |wait| wait.get()).min(MAX_WAIT);
    let deadline = tokio::time::Instant::now() + wait_for;
    let mut changes = app.wake.subscribe();
    loop {
        let claimed = app.engine.claim_host_tasks(&claim).await?;
        if !claimed.is_empty() {
            let mut tasks = Vec::with_capacity(claimed.len());
            for task in claimed {
                app.record_attempt(&task.task.run, task.task.seq, task.attempt)
                    .await;
                tasks.push(view(task));
            }
            app.wake.bump();
            return Ok(Json(TaskBatch { tasks }));
        }
        if tokio::time::Instant::now() >= deadline || app.stopping() {
            return Ok(Json(TaskBatch { tasks: Vec::new() }));
        }
        wait(&mut changes, POLL, Some(deadline)).await;
    }
}

fn view(task: HostTask) -> Task {
    let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
    Task {
        token: task.token.0,
        id: task_id(&task.task),
        run_id: task.task.run.id.to_string(),
        seq: task.task.seq.0,
        tool: task.tool,
        args: task.args,
        idempotency_key: task.idempotency_key,
        attempt: task.attempt,
        heartbeat_timeout_ms: millis(task.heartbeat_timeout),
        timeout_ms: task.execution_timeout.map(millis),
        labels: task.labels.into(),
        traceparent: task
            .trace_context
            .as_ref()
            .map(|context| context.traceparent.clone()),
        tracestate: task.trace_context.and_then(|context| context.tracestate),
    }
}

/// `POST /v1/tasks/{token}/heartbeat`.
pub(super) async fn heartbeat(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(token): Path<String>,
) -> Result<Json<HeartbeatAck>, ApiError> {
    caller.allow(Access::Tasks)?;
    let token = TaskToken(token);
    let until = app.engine.heartbeat_task(&caller.tenant, &token).await?;
    // A deferred task's deadline may be far: the worker sends heartbeats it does not need.
    let left = until.0.saturating_sub(app.now().0);
    Ok(Json(HeartbeatAck {
        heartbeat_timeout_ms: left,
    }))
}

/// `POST /v1/tasks/{token}/defer`.
pub(super) async fn defer(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(token): Path<String>,
    OptionalBody(request): OptionalBody<DeferTask>,
) -> Result<StatusCode, ApiError> {
    caller.allow(Access::Tasks)?;
    app.engine
        .defer_task(
            &caller.tenant,
            &TaskToken(token),
            request.timeout.map(|timeout| timeout.get()),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/tasks/{token}/complete`.
pub(super) async fn complete(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(token): Path<String>,
    Body(request): Body<CompleteTask>,
) -> Result<StatusCode, ApiError> {
    caller.allow(Access::Tasks)?;
    check_payload_size(&app, &request.result)?;
    app.engine
        .complete_task(&caller.tenant, &TaskToken(token), request.result)
        .await?;
    app.wake.bump();
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/tasks/{token}/fail`.
pub(super) async fn fail(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(token): Path<String>,
    Body(request): Body<FailTask>,
) -> Result<StatusCode, ApiError> {
    caller.allow(Access::Tasks)?;
    check_failure_size(&request.message, request.data.as_ref())?;
    let failure = HostFailure {
        message: request.message,
        data: request.data,
        retryable: request.retryable,
    };
    app.engine
        .fail_task(&caller.tenant, &TaskToken(token), failure)
        .await?;
    app.wake.bump();
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/tasks/{id}/approve`.
pub(super) async fn approve(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.allow(Access::Write)?;
    let task = task(&app, &caller, &id).await?;
    app.engine.approve_task(&task).await?;
    app.wake.bump();
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/tasks/{id}/reject`.
pub(super) async fn reject(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    OptionalBody(request): OptionalBody<RejectTask>,
) -> Result<StatusCode, ApiError> {
    caller.allow(Access::Write)?;
    if let Some(message) = &request.message {
        check_failure_size(message, None)?;
    }
    let task = task(&app, &caller, &id).await?;
    app.engine.reject_task(&task, request.message).await?;
    app.wake.bump();
    Ok(StatusCode::NO_CONTENT)
}

/// Refuses a failure above [`FAILURE_LIMIT`].
fn check_failure_size(message: &str, data: Option<&serde_json::Value>) -> Result<(), ApiError> {
    let size = message.len() + data.map_or(0, |data| canonical_json(data).len());
    if size > FAILURE_LIMIT {
        return Err(ApiError::new(
            ProblemType::PayloadTooLarge,
            format!(
                "the failure is {size} bytes (its message, and its data as canonical JSON), above \
                 the limit of {FAILURE_LIMIT}: a failure stays in its journal entry; give large \
                 details as a result"
            ),
        ));
    }
    Ok(())
}

/// A task of a run the caller sees.
async fn task(
    app: &App,
    caller: &Caller,
    id: &str,
) -> Result<ptc_engine::ports::TaskRef, ApiError> {
    let missing = || {
        ApiError::not_found(format!(
            "no task {id}: a task's id is in the run's state (waitingFor) or in the webhook"
        ))
    };
    let task = parse_task_id(&caller.tenant, id).ok_or_else(missing)?;
    match app.store.get_run(&task.run).await? {
        Some(record) if caller.sees(&record.metadata.labels) => Ok(task),
        _ => Err(missing()),
    }
}
