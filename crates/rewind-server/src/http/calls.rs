//! Harness calls: the harness's SDK worker holds a call stream open and is handed the calls, the
//! tasks that carry them, as they become ready, and answers each through its token; the harness
//! approves or rejects the calls that wait for it (`archi/04-tools.md`, "Call lifecycle").
//!
//! ```text
//!  GET /v1/calls/stream ── attach_stream ──► organization actor of the tenant (its home owner)
//!        ▲ call, gone                          hands each task to a stream with a free slot
//!        └──────────────── this edge ◄──────── EdgeNotice::HandOut (here, or through the link)
//!  POST /v1/calls/{token}/… ── the token names its run ──► the owner of the run's partition
//! ```
//!
//! The open stream is the liveness of the calls handed on it: nothing is written while a call is
//! in flight. A worker reaches the calls of the runs its caller sees: an end user's token those
//! of its namespace, a grant limited to labels those of the runs carrying them
//! (`archi/06-storage-and-scale.md`, "Tenancy, namespaces, identity and access").

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use rewind_api::ProblemType;
use rewind_api::access::Permission;
use rewind_api::calls::{
    CallGone, CallStreamMessage, CallStreamQuery, CompleteCall, DeferCall, FailCall, HandedCall,
    RejectCall,
};
use rewind_core::canonical_json;
use rewind_engine::ports::TaskToken;
use rewind_engine::ports::link::{
    HarnessFailure, HarnessTask, StreamAttach, StreamEvent, StreamId,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::{App, Node};
use crate::auth::{Caller, TokenExpiry};
use crate::error::ApiError;
use crate::extract::{Body, OptionalBody, Query};
use crate::http::action_sets::parse_digest;
use crate::http::refuse_nul_in_filter;
use crate::http::runs::check_payload_size;
use crate::idle::Busy;
use crate::streams::Rights;
use crate::views::{call_id, parse_call_id};

/// The most a failure holds: its message and its data as canonical JSON, together. A failure
/// stays in its journal entry, which never moves a value to the payload tiers: it is bounded by
/// the largest value a row holds next to a digest (08).
const FAILURE_LIMIT: usize = 64 * 1024;

/// The tasks a stream holds at once, by default and at most.
const SLOTS: u32 = 1;
const MAX_SLOTS: u32 = 100;

/// `GET /v1/calls/stream`: the harness calls handed to this worker as they become ready.
pub(super) async fn stream(
    State(app): State<Arc<App>>,
    caller: Caller,
    TokenExpiry(expiry): TokenExpiry,
    Query(query): Query<CallStreamQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    caller.require(Permission::CallsWork)?;
    let named = query.namespace.as_ref().map(crate::views::engine_namespace);
    let namespace = match (&caller.namespace, named) {
        (Some(bound), Some(named)) if *bound != named => {
            return Err(ApiError::forbidden(format!(
                "this request is bound to the namespace {bound}: it receives the tasks of no \
                 other"
            )));
        }
        (Some(bound), _) => Some(bound.clone()),
        (None, named) => named,
    };
    let action_set = query.action_set.as_deref().map(parse_digest).transpose()?;
    let mut actions = BTreeSet::new();
    for action in query.actions {
        refuse_nul_in_filter("actions", &action)?;
        actions.insert(action);
    }
    let slots = query.slots.unwrap_or(SLOTS);
    if slots > MAX_SLOTS {
        return Err(ApiError::invalid(format!(
            "slots is {slots}: a stream holds at most {MAX_SLOTS} tasks at once"
        )));
    }
    let worker = match query.worker {
        Some(worker) => {
            let visible = |c: char| c.is_ascii_graphic();
            if worker.is_empty() || worker.len() > 128 || !worker.chars().all(visible) {
                return Err(ApiError::invalid(
                    "worker: 1 to 128 visible ASCII characters, drawn once by the worker (a \
                     UUID) and sent again at each reconnection",
                ));
            }
            worker
        }
        None => uuid::Uuid::now_v7().to_string(),
    };
    let stream = StreamId(format!("{}/{}", app.node.node(), uuid::Uuid::now_v7()));
    let tasks = app
        .node
        .attach_stream(
            caller.tenant.clone(),
            StreamAttach {
                stream: stream.clone(),
                worker,
                node: app.node.node().clone(),
                namespace,
                labels: caller.labels.clone(),
                action_set,
                actions,
                slots,
            },
        )
        .map_err(|error| ApiError::unavailable(error.to_string()))?;
    let held = Held {
        rights: Rights::new(&app, caller, Permission::CallsWork, expiry),
        attached: Attached {
            node: app.node.clone(),
            stream,
        },
        _busy: app.idle.busy(),
        tasks,
    };
    let events = futures_util::stream::unfold(held, |mut held| async move {
        held.next_event().await.map(|event| (Ok(event), held))
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

/// A stream attached at this edge: detached when the response it feeds ends.
struct Attached {
    node: Node,
    stream: StreamId,
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.node.detach_stream(self.stream.clone());
    }
}

/// Where a call stream stands.
struct Held {
    rights: Rights,
    #[allow(dead_code)]
    attached: Attached,
    /// A stream held open keeps the embedded server from stopping on idle.
    _busy: Busy,
    tasks: UnboundedReceiver<StreamEvent>,
}

impl Held {
    async fn next_event(&mut self) -> Option<Event> {
        tokio::select! {
            () = self.rights.lost() => None,
            event = self.tasks.recv() => event.map(|event| match event {
                StreamEvent::Task(task) => sse(&CallStreamMessage::Call(Box::new(handed(task)))),
                StreamEvent::Gone(token) => sse(&CallStreamMessage::Gone(CallGone { token: token.0 })),
            }),
        }
    }
}

/// A task as the SDK worker receives it.
fn handed(task: HarnessTask) -> HandedCall {
    HandedCall {
        token: task.token.0,
        id: call_id(&task.task),
        run_id: task.task.run.id.to_string(),
        namespace: crate::views::namespace(&task.namespace),
        seq: task.task.seq.0,
        action: task.action,
        args: task.args,
        idempotency_key: task.idempotency_key,
        delivery: task.delivery,
        timeout_ms: task
            .execution_timeout
            .map(|timeout| u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
        labels: task.labels.into(),
        traceparent: task
            .trace_context
            .as_ref()
            .map(|context| context.traceparent.clone()),
        tracestate: task.trace_context.and_then(|context| context.tracestate),
    }
}

fn sse(event: &CallStreamMessage) -> Event {
    let data = match event {
        CallStreamMessage::Call(task) => serde_json::to_string(task),
        CallStreamMessage::Gone(gone) => serde_json::to_string(gone),
    }
    .unwrap_or_default();
    Event::default().event(event.sse_event()).data(data)
}

/// `POST /v1/calls/{token}/defer`.
pub(super) async fn defer(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(token): Path<String>,
    OptionalBody(request): OptionalBody<DeferCall>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::CallsWork)?;
    let token = TaskToken(token);
    held(&app, &caller, &token).await?;
    app.edge
        .defer_task(
            &mut app.gateway(),
            &caller.tenant,
            &token,
            request.timeout.map(|timeout| timeout.get()),
        )
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/calls/{token}/complete`.
pub(super) async fn complete(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(token): Path<String>,
    Body(request): Body<CompleteCall>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::CallsWork)?;
    check_payload_size(&app, &request.result)?;
    let token = TaskToken(token);
    held(&app, &caller, &token).await?;
    app.edge
        .complete_task(&mut app.gateway(), &caller.tenant, &token, request.result)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/calls/{token}/fail`.
pub(super) async fn fail(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(token): Path<String>,
    Body(request): Body<FailCall>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::CallsWork)?;
    check_failure_size(&request.message, request.data.as_ref())?;
    let failure = HarnessFailure {
        message: request.message,
        data: request.data,
        retryable: request.retryable,
    };
    let token = TaskToken(token);
    held(&app, &caller, &token).await?;
    app.edge
        .fail_task(&mut app.gateway(), &caller.tenant, &token, failure)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/calls/{id}/approve`.
pub(super) async fn approve(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::CallsApprove)?;
    let task = task(&app, &caller, &id).await?;
    app.edge.approve_task(&mut app.gateway(), &task).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/calls/{id}/reject`.
pub(super) async fn reject(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    OptionalBody(request): OptionalBody<RejectCall>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::CallsApprove)?;
    if let Some(message) = &request.message {
        check_failure_size(message, None)?;
    }
    let task = task(&app, &caller, &id).await?;
    app.edge
        .reject_task(&mut app.gateway(), &task, request.message)
        .await?;
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

/// Refuses (410, as a call that no longer exists) the token of a call whose run the caller does
/// not see: a worker acting for an end user answers its calls only, and one whose grant is
/// limited to labels the calls of the runs carrying them. The token names its run.
async fn held(app: &App, caller: &Caller, token: &TaskToken) -> Result<(), ApiError> {
    if caller.namespace.is_none() && !caller.scoped() {
        return Ok(());
    }
    let gone = || ApiError::from(rewind_engine::ports::link::Refusal::TaskGone);
    let Ok((run, _)) = app.edge.partition_of_token(&token.0).await else {
        return Err(gone());
    };
    let run = rewind_engine::ports::RunRef::new(caller.tenant.clone(), run);
    match app.read_run(&run).await? {
        Some(view) if caller.sees_run(&view.head) => Ok(()),
        _ => Err(gone()),
    }
}

/// A task of a run the caller sees.
async fn task(
    app: &App,
    caller: &Caller,
    id: &str,
) -> Result<rewind_engine::ports::TaskRef, ApiError> {
    let missing = || {
        ApiError::not_found(format!(
            "no call {id}: a call's id is in the run's state (waitingFor) or in the webhook"
        ))
    };
    let task = parse_call_id(&caller.tenant, id).ok_or_else(missing)?;
    match app.read_run(&task.run).await? {
        Some(view) if caller.sees_run(&view.head) => Ok(task),
        _ => Err(missing()),
    }
}
