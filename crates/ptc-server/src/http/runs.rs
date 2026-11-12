//! Runs: start, list, state, trace, entries, signals, cancellation, forks, export, quarantine,
//! credentials and erasure (`archi/03-execution.md`, `archi/07-code-architecture.md`).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use ptc_api::ProblemType;
use ptc_api::common::{Labels, Payload, timestamp};
use ptc_api::events::Entry;
use ptc_api::runs::{
    CancelRun, ForkRun, GetRunQuery, JournalExport, JournalQuery, Run, RunFilter, RunPage,
    RunSummary, SignalAccepted, StartRun, Trace, TraceFormat, TraceQuery,
};
use ptc_core::{
    Command, Digest, Event, JournalDocument, Lifecycle, Matching, RunId, RunMode, Script,
    ToolFailureKind, canonical_json,
};
use ptc_engine::ports::{
    Enqueued, JournalStore, ReadModel, RunMetadata, RunRecord, RunSearch, TraceContext,
};
use ptc_engine::trace::{TraceInput, render_trace};

use crate::app::App;
use crate::auth::{Access, Caller};
use crate::error::ApiError;
use crate::extract::{Body, Idempotency, OptionalBody, Query};
use crate::http::{refuse_nul_in_filter, toolsets};

/// Runs a page of `GET /v1/runs` holds by default, and at most.
const PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;
/// Summaries read from the read model for one page at most, filters applied after it included.
const MAX_SCANNED: usize = 2_000;

/// `POST /v1/runs`.
pub(super) async fn start(
    State(app): State<Arc<App>>,
    caller: Caller,
    Idempotency(key): Idempotency,
    Body(request): Body<StartRun>,
) -> Result<Response, ApiError> {
    caller.allow(Access::Write)?;
    let labels = caller.within(request.labels.0)?;
    let trace_context = trace_context(request.traceparent, request.tracestate)?;
    let stored = match &request.toolset {
        Some(id) => toolsets::stored(&app, &caller.tenant, id).await?,
        None => app.default_toolset(&caller.tenant).await?,
    };
    let input = request.input;
    let start = ptc_engine::StartRun {
        tenant: caller.tenant.clone(),
        script: Script::new(request.script),
        toolset: stored.toolset,
        declarations: stored.declarations,
        input: input.clone(),
        mode: RunMode::Live,
        ttl: request.ttl.map(|ttl| ttl.get()),
        metadata: RunMetadata {
            labels,
            trace_context,
            parent: None,
        },
        idempotency_key: key,
        credentials: request.credentials.map(toolsets::secrets),
    };
    let created = if request.check == Some(false) {
        app.engine.start_run(start).await?
    } else {
        app.engine.start_run_checked(start, &app.checker).await?
    };
    app.wake.bump();
    let record = app
        .wait_for_end(&created.run, request.wait.map(|wait| wait.get()))
        .await?;
    if !caller.sees(&record.metadata.labels) {
        return Err(ApiError::run_not_found(&created.run.id.to_string()));
    }
    let mut run = app.views().run(&record).await?;
    if run.input.is_none() && created.created {
        run.input = Some(Payload::of(input, ptc_api::common::PAYLOAD_VALUE_LIMIT));
    }
    let status = if created.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(run)).into_response())
}

/// `GET /v1/runs`: the read model's runs, newest first, with what the journal's records add
/// (quarantine, the parent of a fork, the error that ended the run).
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(filter): Query<RunFilter>,
) -> Result<Json<RunPage>, ApiError> {
    caller.allow(Access::Read)?;
    let limit = usize::try_from(filter.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
    let mut labels = BTreeMap::new();
    for label in &filter.labels {
        refuse_nul_in_filter("label", label)?;
        let (key, value) = label
            .split_once(':')
            .ok_or_else(|| ApiError::invalid(format!("label {label:?}: expected key:value")))?;
        labels.insert(key.to_owned(), value.to_owned());
    }
    if let Some(tool) = &filter.tool {
        refuse_nul_in_filter("tool", tool)?;
    }
    let Ok(labels) = caller.within(labels) else {
        // A filter outside the key's labels matches nothing it may see.
        return Ok(Json(RunPage {
            runs: Vec::new(),
            next_cursor: None,
        }));
    };
    let mut search = RunSearch::new(caller.tenant.clone(), limit);
    search.labels = labels;
    search.status = filter.status.map(super::runs_status);
    search.tool = filter.tool.clone();
    search.started_after = filter.started_after.map(core_time);
    search.started_before = filter.started_before.map(core_time);
    if let Some(script) = &filter.script {
        search.script = Some(
            Digest::from_hex(script)
                .map_err(|_| ApiError::invalid(format!("script {script:?}: expected a digest")))?,
        );
    }
    let error_kind = filter.error.as_deref().and_then(failure_kind);
    search.error_kind = error_kind;
    let error_name = filter.error.clone().filter(|_| error_kind.is_none());
    if let Some(cursor) = &filter.cursor {
        search.before = Some(
            cursor
                .parse::<RunId>()
                .map_err(|_| ApiError::invalid(format!("cursor {cursor:?} is not valid")))?,
        );
    }
    let parent = match &filter.parent {
        Some(parent) => Some(
            parent
                .parse::<RunId>()
                .map_err(|_| ApiError::invalid(format!("parent {parent:?}: expected a run id")))?,
        ),
        None => None,
    };

    let mut runs = Vec::new();
    let mut scanned = 0;
    let mut next_cursor = None;
    search.limit = limit.max(100);
    'pages: loop {
        let batch = app.store.search_runs(&search).await?;
        let full = batch.len() == search.limit;
        for summary in batch {
            scanned += 1;
            search.before = Some(summary.run.id);
            let Some(record) = app.store.get_run(&summary.run).await? else {
                continue;
            };
            if filter
                .quarantined
                .is_some_and(|quarantined| quarantined != record.quarantine.is_some())
                || parent
                    .is_some_and(|parent| record.metadata.parent.is_none_or(|of| of.run != parent))
            {
                continue;
            }
            let error = ended_with(&app, &record).await?;
            if let Some(name) = &error_name
                && error.as_ref() != Some(name)
            {
                continue;
            }
            runs.push(RunSummary {
                id: record.run.id.to_string(),
                status: record.status.into(),
                quarantined: record.quarantine.is_some(),
                script: record.script.to_hex(),
                headline: app.headline(&record.run.tenant, record.script).await,
                toolset: record.toolset.to_hex(),
                labels: Labels::from(record.metadata.labels.clone()),
                started_at: timestamp(summary.started_at.unwrap_or(record.created_at)),
                ended_at: record.ended_at.or(summary.ended_at).map(timestamp),
                last_activity_at: timestamp(summary.last_activity_at.max(record.updated_at)),
                tool_calls: summary.tool_calls,
                errors: summary.errors,
                history_len: summary.history_len.max(record.history_len),
                error,
                parent: record.metadata.parent.map(|parent| parent.run.to_string()),
            });
            if runs.len() == limit {
                next_cursor = Some(record.run.id.to_string());
                break 'pages;
            }
        }
        if !full {
            break;
        }
        if scanned >= MAX_SCANNED {
            next_cursor = search.before.map(|id| id.to_string());
            break;
        }
    }
    Ok(Json(RunPage { runs, next_cursor }))
}

/// `GET /v1/runs/{id}`.
pub(super) async fn get(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<GetRunQuery>,
) -> Result<Json<Run>, ApiError> {
    caller.allow(Access::Read)?;
    let record = app.run(&caller, &id).await?;
    let record = app
        .wait_for_end(&record.run, query.wait.map(|wait| wait.get()))
        .await?;
    Ok(Json(app.views().run(&record).await?))
}

/// `DELETE /v1/runs/{id}`.
pub(super) async fn erase(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    caller.allow(Access::Write)?;
    let record = app.run(&caller, &id).await?;
    if !app.housekeeper().delete_run(&record.run, app.now()).await? {
        return Err(ApiError::run_not_found(&id));
    }
    app.wake.bump();
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /v1/runs/{id}/trace`.
pub(super) async fn trace(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<TraceQuery>,
) -> Result<Json<Trace>, ApiError> {
    caller.allow(Access::Read)?;
    let record = app.run(&caller, &id).await?;
    let views = app.views();
    let history = views.history(&record).await?;
    let payloads = views.payload_rows(&record.run.tenant, &history).await?;
    let text = render_trace(TraceInput {
        record: &record,
        history: &history,
        payloads: &payloads,
        now: app.now(),
    });
    Ok(Json(Trace {
        run_id: record.run.id.to_string(),
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
    caller.allow(Access::Read)?;
    let record = app.run(&caller, &id).await?;
    let missing = || {
        ApiError::not_found(format!(
            "run {id} has no entry {idx}: its entries are numbered from 0 to {}",
            record.history_len.saturating_sub(1)
        ))
    };
    let index: u64 = idx.parse().map_err(|_| missing())?;
    let views = app.views();
    let history = views.history_from(&record, index).await?;
    let entry = history
        .into_iter()
        .find(|entry| entry.index == index)
        .ok_or_else(missing)?;
    let mut entries = views
        .entries(&record.run.tenant, std::slice::from_ref(&entry), u64::MAX)
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
    caller.allow(Access::Write)?;
    let record = app.run(&caller, &id).await?;
    check_payload_size(&app, &payload)?;
    let enqueued = app
        .engine
        .signal(&record.run, name.clone(), payload, key)
        .await?;
    accepted_signal(&app, &record, &name, enqueued)
}

/// The answer to a signal, from the API or an inbound webhook.
pub(crate) fn accepted_signal(
    app: &App,
    record: &RunRecord,
    name: &str,
    enqueued: Enqueued,
) -> Result<Response, ApiError> {
    let duplicate = match enqueued {
        Enqueued::Accepted => false,
        Enqueued::Duplicate => true,
        Enqueued::RunEnded => return Err(run_ended(record, "a signal")),
    };
    app.wake.bump();
    Ok((
        StatusCode::ACCEPTED,
        Json(SignalAccepted {
            run_id: record.run.id.to_string(),
            name: name.to_owned(),
            duplicate,
        }),
    )
        .into_response())
}

pub(crate) fn check_payload_size(app: &App, payload: &serde_json::Value) -> Result<(), ApiError> {
    let size = canonical_json(payload).len();
    let max = app.engine.config().limits.max_payload_bytes;
    if size > max {
        return Err(ApiError::new(
            ProblemType::PayloadTooLarge,
            format!("the payload is {size} bytes as canonical JSON, above the limit of {max}"),
        ));
    }
    Ok(())
}

fn run_ended(record: &RunRecord, what: &str) -> ApiError {
    ApiError::new(
        ProblemType::RunEnded,
        format!(
            "run {} has ended ({}): {what} can only be sent to a run that is still active",
            record.run.id,
            record.status.as_str()
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
    caller.allow(Access::Write)?;
    let record = app.run(&caller, &id).await?;
    match app.engine.cancel(&record.run, request.reason).await? {
        Enqueued::RunEnded => return Err(run_ended(&record, "a cancellation")),
        Enqueued::Accepted | Enqueued::Duplicate => app.wake.bump(),
    }
    let record = app
        .wait_for_end(&record.run, request.wait.map(|wait| wait.get()))
        .await?;
    let run = app.views().run(&record).await?;
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
    caller.allow(Access::Write)?;
    let parent = app.run(&caller, &id).await?;
    let labels = match request.labels {
        Some(labels) => Some(caller.within(labels.0)?),
        // The parent's: the caller sees them.
        None => None,
    };
    let fork = ptc_engine::ForkRun {
        parent: parent.run.clone(),
        script: Script::new(request.script),
        at: request.at,
        input: request.input,
        ttl: request.ttl.map(|ttl| ttl.get()),
        labels,
        trace_context: trace_context(request.traceparent, request.tracestate)?,
        idempotency_key: key,
    };
    let created = if request.check == Some(false) {
        app.engine.fork(fork).await?
    } else {
        app.engine.fork_checked(fork, &app.checker).await?
    };
    app.wake.bump();
    let record = app
        .wait_for_end(&created.run, request.wait.map(|wait| wait.get()))
        .await?;
    if !caller.sees(&record.metadata.labels) {
        return Err(ApiError::run_not_found(&created.run.id.to_string()));
    }
    let run = app.views().run(&record).await?;
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
    caller.allow(Access::Read)?;
    let record = app.run(&caller, &id).await?;
    let views = app.views();
    let tenant = &record.run.tenant;
    let mut history = views.history(&record).await?;
    if history.is_empty() {
        return Err(ApiError::invalid(format!(
            "run {id} has not started yet: its journal is empty"
        )));
    }
    views.inline(tenant, &mut history).await?;
    let document = match query.export_as.unwrap_or_default() {
        JournalExport::Journal => {
            let script = app
                .store
                .get_script(tenant, record.script)
                .await?
                .ok_or_else(|| ApiError::internal(format!("script {} is missing", record.script)))?
                .script;
            let toolset = app
                .store
                .get_toolset(tenant, record.toolset)
                .await?
                .ok_or_else(|| {
                    ApiError::internal(format!("toolset {} is missing", record.toolset))
                })?
                .toolset;
            ptc_format::encode_journal(&JournalDocument {
                run: record.run.id,
                script,
                toolset,
                entries: history,
            })
        }
        JournalExport::Cassette => {
            let cassette = ptc_core::Cassette::from_history(&history, Matching::Sequential)
                .map_err(ApiError::internal)?;
            ptc_format::encode_cassette(&cassette)
        }
    };
    Ok(([(header::CONTENT_TYPE, "application/json")], document).into_response())
}

/// `POST /v1/runs/{id}/release`.
pub(super) async fn release(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Run>, ApiError> {
    caller.allow(Access::Write)?;
    let record = app.run(&caller, &id).await?;
    if !app.engine.release_quarantine(&record.run).await? {
        return Err(not_quarantined(&id));
    }
    app.wake.bump();
    let record = app.run(&caller, &id).await?;
    Ok(Json(app.views().run(&record).await?))
}

/// `POST /v1/runs/{id}/give-up`.
pub(super) async fn give_up(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Run>, ApiError> {
    caller.allow(Access::Write)?;
    let record = app.run(&caller, &id).await?;
    if record.quarantine.is_none() {
        return Err(not_quarantined(&id));
    }
    if !app.engine.give_up(&record.run).await? {
        return Err(ApiError::unavailable(format!(
            "run {id} is being activated: give it up again in a few seconds"
        )));
    }
    app.wake.bump();
    let record = app.run(&caller, &id).await?;
    Ok(Json(app.views().run(&record).await?))
}

fn not_quarantined(id: &str) -> ApiError {
    ApiError::new(
        ProblemType::NotQuarantined,
        format!("run {id} is not quarantined: only a quarantined run is released or given up"),
    )
}

/// `PUT /v1/runs/{id}/credentials`.
pub(super) async fn credentials(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Body(credentials): Body<ptc_api::toolsets::Credentials>,
) -> Result<StatusCode, ApiError> {
    caller.allow(Access::Write)?;
    let record = app.run(&caller, &id).await?;
    app.engine
        .set_run_credentials(&record.run, &toolsets::secrets(credentials))
        .await?;
    app.wake.bump();
    Ok(StatusCode::NO_CONTENT)
}

/// What ended a run: the name of the script's error, or the kind of the abort.
async fn ended_with(app: &App, record: &RunRecord) -> Result<Option<String>, ApiError> {
    if !record.status.is_terminal() || record.archive.is_some() || record.history_len == 0 {
        return Ok(None);
    }
    let last = app
        .store
        .load_history(&record.run, record.history_len - 1)
        .await
        .unwrap_or_default();
    Ok(last.last().and_then(|entry| match &entry.event {
        Event::Command(Command::FailRun { error }) => Some(error.name.clone()),
        Event::Lifecycle(Lifecycle::RunAborted(reason)) => Some(reason.kind.as_str().to_owned()),
        _ => None,
    }))
}

fn failure_kind(text: &str) -> Option<ToolFailureKind> {
    [
        ToolFailureKind::Error,
        ToolFailureKind::Unavailable,
        ToolFailureKind::Timeout,
        ToolFailureKind::OutcomeUnknown,
    ]
    .into_iter()
    .find(|kind| kind.as_str() == text)
}

pub(crate) fn core_time(time: jiff::Timestamp) -> ptc_core::Timestamp {
    ptc_core::Timestamp(u64::try_from(time.as_millisecond()).unwrap_or(0))
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
