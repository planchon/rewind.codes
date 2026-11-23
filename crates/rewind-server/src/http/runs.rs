//! Runs: start, list, state, trace, entries, signals, cancellation, forks, export, quarantine,
//! the secrets they attach and erasure (`archi/03-execution.md`, `archi/07-code-architecture.md`).
//! Each goes through the edge to the owner of the run's partition.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::access::Permission;
use rewind_api::common::Payload;
use rewind_api::events::Entry;
use rewind_api::runs::{
    CancelRun, ForkRun, GetRunQuery, JournalExport, JournalQuery, Run, RunFilter, RunPage,
    SignalAccepted, StartRun, Trace, TraceFormat, TraceQuery,
};
use rewind_core::{Attachments, Matching, RunMode, Script, SecretRef, canonical_json};
use rewind_engine::attachments::AttachRights;
use rewind_engine::ports::link::RunView;
use rewind_engine::ports::{Enqueued, Page, RunMetadata, SCAN_BUDGET, TraceContext};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::{Body, Idempotency, OptionalBody, Query};
use crate::http::{search, toolsets, workflows};
use crate::titles::Title;

/// Runs a page of `GET /v1/runs` holds by default, and at most.
const PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;

/// `POST /v1/runs`.
pub(super) async fn start(
    State(app): State<Arc<App>>,
    caller: Caller,
    Idempotency(key): Idempotency,
    Body(request): Body<StartRun>,
) -> Result<Response, ApiError> {
    caller.require(Permission::RunsStart)?;
    let namespace = caller.namespace_required("start a run")?;
    let labels = caller.within(request.labels.0)?;
    let trace_context = trace_context(request.traceparent, request.tracestate)?;
    let input = request.input;
    let secrets = attachments(&request.secrets)?;
    let metadata = RunMetadata {
        labels,
        trace_context,
        // What the run's scripts reach: the runs carrying the key's labels.
        scope: caller.labels.clone(),
        secrets,
        ..RunMetadata::default()
    };
    let mut gateway = app.gateway();
    let created = match request.workflow {
        Some(reference) => {
            if !request.secrets.is_empty() {
                return Err(ApiError::invalid(
                    "secrets: a run of a workflow reads the secrets its version attaches; give \
                     none",
                ));
            }
            if !request.script.is_empty() || request.toolset.is_some() {
                return Err(ApiError::invalid(
                    "give either a workflow, or a script and its toolset: a run of a workflow \
                     executes the workflow's script with its toolset",
                ));
            }
            let stored =
                workflows::resolve(&app, &caller.tenant, &caller.labels, &reference).await?;
            app.edge
                .start_workflow(
                    &mut gateway,
                    rewind_engine::StartWorkflow {
                        tenant: caller.tenant.clone(),
                        namespace,
                        version: stored.workflow.version(),
                        input: input.clone(),
                        ttl: request.ttl.map(|ttl| ttl.get()),
                        metadata,
                        idempotency_key: key,
                    },
                )
                .await?
        }
        None => {
            let stored = match &request.toolset {
                Some(id) => toolsets::stored(&app, &caller.tenant, id).await?,
                None => app.default_toolset(&caller.tenant).await?,
            };
            let start = rewind_engine::StartRun {
                tenant: caller.tenant.clone(),
                namespace,
                script: Script::new(request.script),
                toolset: stored.toolset,
                declarations: stored.declarations,
                input: input.clone(),
                mode: RunMode::Live,
                ttl: request.ttl.map(|ttl| ttl.get()),
                metadata,
                idempotency_key: key,
                attach: attach_rights(&caller),
            };
            if request.check == Some(false) {
                app.edge.start_run(&mut gateway, start).await?
            } else {
                app.edge
                    .start_run_checked(&mut gateway, start, &app.checker)
                    .await?
            }
        }
    };
    let view = app
        .wait_for_end(&created.run, request.wait.map(|wait| wait.get()))
        .await?;
    if !caller.sees_run(&view.head) {
        return Err(ApiError::run_not_found(&created.run.id.to_string()));
    }
    let mut run = app.views().run(&view).await?;
    if run.input.is_none() && created.created {
        run.input = Some(Payload::of(input, rewind_api::common::PAYLOAD_VALUE_LIMIT));
    }
    let status = if created.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(run)).into_response())
}

/// `GET /v1/runs`: a page of the runs' summaries, in the order of `sort`, merged from the
/// partitions of the scope. A partition that stopped at its scan budget cuts the page short,
/// with a cursor.
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(mut filter): Query<RunFilter>,
) -> Result<Json<RunPage>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let parent = by_id(&app, &caller, &mut filter.parent).await?;
    let started_by = by_id(&app, &caller, &mut filter.started_by).await?;
    if !parent || !started_by {
        return Ok(Json(RunPage {
            runs: Vec::new(),
            next_cursor: None,
        }));
    }
    let limit = usize::try_from(filter.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
    let query = search::run_query(&caller, filter.q.as_deref(), Some(&filter), app.now())?;
    let order = search::order(filter.sort);
    let cursor = filter
        .cursor
        .as_deref()
        .map(|cursor| search::decode_cursor(order, cursor))
        .transpose()?;
    let scope = search::scope_of(&query);
    let found = app
        .edge
        .search(
            &mut app.gateway(),
            &scope,
            &query,
            order,
            &Page {
                cursor,
                limit,
                budget: SCAN_BUDGET,
            },
        )
        .await?;
    let next_cursor = found
        .cursor
        .as_ref()
        .map(|cursor| search::encode_cursor(order, cursor));
    let runs = found.summaries.iter().map(search::summary).collect();
    Ok(Json(RunPage { runs, next_cursor }))
}

/// `GET /v1/runs/{id}`.
pub(super) async fn get(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<GetRunQuery>,
) -> Result<Json<Run>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let view = app.run(&caller, &id).await?;
    let view = app
        .wait_for_end(&view.head.run, query.wait.map(|wait| wait.get()))
        .await?;
    Ok(Json(app.views().run(&view).await?))
}

/// `DELETE /v1/runs/{id}`: the run's sealed journals and blobs, then its records, summary and
/// counts, by its owner.
pub(super) async fn erase(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.require(Permission::RunsDelete)?;
    let view = app.run(&caller, &id).await?;
    if !app.edge.erase(&mut app.gateway(), &view.head.run).await? {
        return Err(ApiError::run_not_found(&id));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/runs/{id}/trace`.
pub(super) async fn trace(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<TraceQuery>,
) -> Result<Json<Trace>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let view = app.run(&caller, &id).await?;
    let text = app.edge.trace(&mut app.gateway(), &view.head.run).await?;
    Ok(Json(Trace {
        run_id: view.head.run.id.to_string(),
        format: query.format.unwrap_or(TraceFormat::Llm),
        text,
    }))
}

/// `GET /v1/runs/{id}/entries/{idx}`: the entry in full, its payload's value included.
pub(super) async fn entry(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path((id, idx)): Path<(String, String)>,
) -> Result<Json<Entry>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let view = app.run(&caller, &id).await?;
    let missing = || {
        ApiError::not_found(format!(
            "run {id} has no entry {idx}: its entries are numbered from 0 to {}",
            view.head.history_len.saturating_sub(1)
        ))
    };
    let index: u64 = idx.parse().map_err(|_| missing())?;
    let views = app.views();
    let history = views.history_from(&view.head.run, index).await?;
    let entry = history
        .into_iter()
        .find(|entry| entry.index == index)
        .ok_or_else(missing)?;
    let mut entries = views
        .entries(&view.head.run, std::slice::from_ref(&entry), u64::MAX)
        .await?;
    entries.pop().map(Json).ok_or_else(missing)
}

/// `POST /v1/runs/{id}/signals/{name}`.
pub(super) async fn signal(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path((id, name)): Path<(String, String)>,
    Idempotency(key): Idempotency,
    OptionalBody(payload): OptionalBody<serde_json::Value>,
) -> Result<Response, ApiError> {
    caller.require(Permission::RunsControl)?;
    let view = app.run(&caller, &id).await?;
    check_payload_size(&app, &payload)?;
    let enqueued = app
        .edge
        .signal(
            &mut app.gateway(),
            &view.head.run,
            name.clone(),
            payload,
            key,
        )
        .await;
    accepted_signal(&view, &name, enqueued)
}

/// The answer to a signal, from the API or an inbound webhook.
pub(crate) fn accepted_signal(
    view: &RunView,
    name: &str,
    enqueued: Result<Enqueued, rewind_engine::edge::EdgeError>,
) -> Result<Response, ApiError> {
    use rewind_engine::edge::EdgeError;
    use rewind_engine::ports::link::Refusal;
    let duplicate = match enqueued {
        Ok(Enqueued::Accepted) => false,
        Ok(Enqueued::Duplicate) => true,
        Ok(Enqueued::RunEnded) | Err(EdgeError::Refused(Refusal::RunEnded(_))) => {
            return Err(run_ended(view, "a signal"));
        }
        Err(error) => return Err(error.into()),
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(SignalAccepted {
            run_id: view.head.run.id.to_string(),
            name: name.to_owned(),
            duplicate,
        }),
    )
        .into_response())
}

pub(crate) fn check_payload_size(app: &App, payload: &serde_json::Value) -> Result<(), ApiError> {
    let size = canonical_json(payload).len();
    let max = app.config.limits.max_payload_bytes;
    if size > max {
        return Err(ApiError::new(
            ProblemType::PayloadTooLarge,
            format!("the payload is {size} bytes as canonical JSON, above the limit of {max}"),
        ));
    }
    Ok(())
}

fn run_ended(view: &RunView, what: &str) -> ApiError {
    ApiError::new(
        ProblemType::RunEnded,
        format!(
            "run {} has ended ({}): {what} can only be sent to a run that is still active",
            view.head.run.id,
            view.head.status.as_str()
        ),
    )
}

/// `POST /v1/runs/{id}/cancel`.
pub(super) async fn cancel(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    OptionalBody(request): OptionalBody<CancelRun>,
) -> Result<Response, ApiError> {
    use rewind_engine::edge::EdgeError;
    use rewind_engine::ports::link::Refusal;
    caller.require(Permission::RunsControl)?;
    let view = app.run(&caller, &id).await?;
    match app
        .edge
        .cancel(&mut app.gateway(), &view.head.run, request.reason)
        .await
    {
        Ok(Enqueued::RunEnded) | Err(EdgeError::Refused(Refusal::RunEnded(_))) => {
            return Err(run_ended(&view, "a cancellation"));
        }
        Ok(Enqueued::Accepted | Enqueued::Duplicate) => {}
        Err(error) => return Err(error.into()),
    }
    let view = app
        .wait_for_end(&view.head.run, request.wait.map(|wait| wait.get()))
        .await?;
    let run = app.views().run(&view).await?;
    Ok((StatusCode::ACCEPTED, Json(run)).into_response())
}

/// `POST /v1/runs/{id}/fork`.
pub(super) async fn fork(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Idempotency(key): Idempotency,
    Body(request): Body<ForkRun>,
) -> Result<Response, ApiError> {
    caller.require(Permission::RunsStart)?;
    let parent = app.run(&caller, &id).await?;
    let labels = match request.labels {
        Some(labels) => Some(caller.within(labels.0)?),
        // The parent's: the caller sees them.
        None => None,
    };
    let script = Script::new(request.script);
    super::testing::gate_fork(&app, &parent, &script).await?;
    let fork = rewind_engine::ForkRun {
        parent: parent.head.run.clone(),
        script,
        at: request.at,
        input: request.input,
        ttl: request.ttl.map(|ttl| ttl.get()),
        labels,
        trace_context: trace_context(request.traceparent, request.tracestate)?,
        idempotency_key: key,
        secrets: request.secrets.as_ref().map(attachments).transpose()?,
        attach: attach_rights(&caller),
    };
    let mut gateway = app.gateway();
    let created = if request.check == Some(false) {
        app.edge.fork(&mut gateway, fork).await?
    } else {
        app.edge
            .fork_checked(&mut gateway, fork, &app.checker)
            .await?
    };
    let view = app
        .wait_for_end(&created.run, request.wait.map(|wait| wait.get()))
        .await?;
    if !caller.sees_run(&view.head) {
        return Err(ApiError::run_not_found(&created.run.id.to_string()));
    }
    let run = app.views().run(&view).await?;
    let status = if created.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(run)).into_response())
}

/// `GET /v1/runs/{id}/journal`: the journal document, self-contained (every payload's value),
/// or the cassette of the run, in the stored format v1.
pub(super) async fn journal(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<JournalQuery>,
) -> Result<Response, ApiError> {
    caller.require(Permission::RunsRead)?;
    let view = app.run(&caller, &id).await?;
    if view.head.history_len == 0 && !view.head.is_ended() {
        return Err(ApiError::invalid(format!(
            "run {id} has not started yet: its journal is empty"
        )));
    }
    let document = app
        .edge
        .export_journal(&mut app.gateway(), &view.head.run)
        .await?;
    if document.entries.is_empty() {
        return Err(ApiError::invalid(format!(
            "run {id} has not started yet: its journal is empty"
        )));
    }
    let body = match query.export_as.unwrap_or_default() {
        JournalExport::Journal => rewind_format::encode_journal(&document),
        JournalExport::Cassette => {
            let cassette =
                rewind_core::Cassette::from_history(&document.entries, Matching::Sequential)
                    .map_err(ApiError::internal)?;
            rewind_format::encode_cassette(&cassette)
        }
    };
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

/// `POST /v1/runs/{id}/release`.
pub(super) async fn release(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Run>, ApiError> {
    caller.require(Permission::RunsQuarantine)?;
    let view = app.run(&caller, &id).await?;
    if !app.edge.release(&mut app.gateway(), &view.head.run).await? {
        return Err(not_quarantined(&id));
    }
    let view = app.run(&caller, &id).await?;
    Ok(Json(app.views().run(&view).await?))
}

/// `POST /v1/runs/{id}/give-up`.
pub(super) async fn give_up(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Run>, ApiError> {
    caller.require(Permission::RunsQuarantine)?;
    let view = app.run(&caller, &id).await?;
    if view.head.quarantine.is_none() {
        return Err(not_quarantined(&id));
    }
    if !app.edge.give_up(&mut app.gateway(), &view.head.run).await? {
        return Err(not_quarantined(&id));
    }
    let view = app
        .wait_for_end(&view.head.run, Some(std::time::Duration::from_secs(10)))
        .await?;
    Ok(Json(app.views().run(&view).await?))
}

fn not_quarantined(id: &str) -> ApiError {
    ApiError::new(
        ProblemType::NotQuarantined,
        format!("run {id} is not quarantined: only a quarantined run is released or given up"),
    )
}

/// The attachments of a request (`org/<name>`, `ns/<name>`, by server or API), read.
pub(crate) fn attachments(
    secrets: &rewind_api::secrets::Attachments,
) -> Result<Attachments, ApiError> {
    secrets
        .iter()
        .map(|(server, secret)| {
            let secret = SecretRef::parse(secret)
                .map_err(|error| ApiError::invalid(format!("secrets.{server}: {error}")))?;
            Ok((server.clone(), secret))
        })
        .collect()
}

/// What the caller may attach (`archi/06-storage-and-scale.md`, "What the caller may do"): its
/// namespace's secrets always; the organization's carrying its labels with `secrets:use`.
pub(crate) fn attach_rights(caller: &Caller) -> AttachRights {
    if caller.require(Permission::SecretsUse).is_ok() {
        AttachRights::Organization {
            labels: caller.labels.clone(),
        }
    } else {
        AttachRights::NamespaceOnly
    }
}

pub(crate) fn core_time(time: jiff::Timestamp) -> rewind_core::Timestamp {
    rewind_core::Timestamp(u64::try_from(time.as_millisecond()).unwrap_or(0))
}

/// A W3C trace context: `traceparent` is `00-<32 hex>-<16 hex>-<2 hex>`.
fn trace_context(
    traceparent: Option<String>,
    tracestate: Option<String>,
) -> Result<Option<TraceContext>, ApiError> {
    let Some(traceparent) = traceparent else {
        return Ok(None);
    };
    let parts: Vec<&str> = traceparent.split('-').collect();
    let hex =
        |part: &str, len: usize| part.len() == len && part.bytes().all(|b| b.is_ascii_hexdigit());
    let valid = parts.len() == 4
        && hex(parts[0], 2)
        && hex(parts[1], 32)
        && hex(parts[2], 16)
        && hex(parts[3], 2);
    if !valid {
        return Err(ApiError::invalid(format!(
            "traceparent {traceparent:?}: expected the W3C form 00-<trace id>-<span id>-<flags>"
        )));
    }
    Ok(Some(TraceContext {
        traceparent,
        tracestate,
    }))
}
