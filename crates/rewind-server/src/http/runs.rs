//! Runs: start, list, state, trace, entries, signals, cancellation, forks, export, quarantine,
//! credentials and erasure (`archi/03-execution.md`, `archi/07-code-architecture.md`).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rewind_api::ProblemType;
use rewind_api::common::{Payload, timestamp};
use rewind_api::events::Entry;
use rewind_api::runs::{
    CancelRun, FailureGroupPage, FailureQuery, ForkRun, GetRunQuery, JournalExport, JournalQuery,
    Run, RunFilter, RunPage, RunStats, RunStatsQuery, SignalAccepted, StartRun, Trace, TraceFormat,
    TraceQuery,
};
use rewind_core::{
    Command, Event, JournalDocument, Lifecycle, Matching, RunMode, Script, canonical_json,
};
use rewind_engine::ports::{
    Enqueued, FailureWindow, JournalStore, ReadModel, RunMetadata, RunRecord, StatsWindow,
    TraceContext,
};
use rewind_engine::trace::{TraceInput, render_trace};

use crate::app::App;
use crate::auth::{Access, Caller};
use crate::error::ApiError;
use crate::extract::{Body, Idempotency, OptionalBody, Query};
use crate::http::{search, toolsets, workflows};

/// Runs a page of `GET /v1/runs` holds by default, and at most; failure groups likewise.
const PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;

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
    let input = request.input;
    let metadata = RunMetadata {
        labels,
        trace_context,
        // What the run's scripts reach: the runs carrying the key's labels.
        scope: caller.labels.clone(),
        ..RunMetadata::default()
    };
    let created = match request.workflow {
        Some(reference) => {
            if !request.script.is_empty() || request.toolset.is_some() {
                return Err(ApiError::invalid(
                    "give either a workflow, or a script and its toolset: a run of a workflow                      executes the workflow's script with its toolset",
                ));
            }
            let stored =
                workflows::resolve(&app, &caller.tenant, &caller.labels, &reference).await?;
            app.engine
                .start_workflow(rewind_engine::StartWorkflow {
                    tenant: caller.tenant.clone(),
                    version: stored.workflow.version(),
                    input: input.clone(),
                    ttl: request.ttl.map(|ttl| ttl.get()),
                    metadata,
                    idempotency_key: key,
                    credentials: request.credentials.map(toolsets::secrets),
                })
                .await?
        }
        None => {
            let stored = match &request.toolset {
                Some(id) => toolsets::stored(&app, &caller.tenant, id).await?,
                None => app.default_toolset(&caller.tenant).await?,
            };
            let start = rewind_engine::StartRun {
                tenant: caller.tenant.clone(),
                script: Script::new(request.script),
                toolset: stored.toolset,
                declarations: stored.declarations,
                input: input.clone(),
                mode: RunMode::Live,
                ttl: request.ttl.map(|ttl| ttl.get()),
                metadata,
                idempotency_key: key,
                credentials: request.credentials.map(toolsets::secrets),
            };
            if request.check == Some(false) {
                app.engine.start_run(start).await?
            } else {
                app.engine.start_run_checked(start, &app.checker).await?
            }
        }
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
        run.input = Some(Payload::of(input, rewind_api::common::PAYLOAD_VALUE_LIMIT));
    }
    let status = if created.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(run)).into_response())
}

/// `GET /v1/runs`: a page of the read model's runs, in the order of `sort`. The run's own
/// record, which the read model follows by a moment, gives each its status, quarantine and
/// times as they are now.
pub(super) async fn list(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(filter): Query<RunFilter>,
) -> Result<Json<RunPage>, ApiError> {
    caller.allow(Access::Read)?;
    let limit = usize::try_from(filter.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
    let query = search::run_query(&caller, filter.q.as_deref(), Some(&filter), app.now())?;
    let order = search::order(filter.sort);
    let after = filter
        .cursor
        .as_deref()
        .map(|cursor| search::decode_cursor(order, cursor))
        .transpose()?;
    let found = app
        .store
        .search_runs(&query, order, after.as_ref(), limit)
        .await?;
    let next_cursor = (found.len() == limit)
        .then(|| found.last())
        .flatten()
        .map(|last| search::encode_cursor(order, &last.cursor(order)));
    let mut runs = Vec::with_capacity(found.len());
    for found in &found {
        let mut summary = search::summary(found);
        if let Some(record) = app.store.get_run(&found.run).await? {
            summary.status = record.status.into();
            summary.quarantined = record.quarantine.is_some();
            summary.history_len = summary.history_len.max(record.history_len);
            summary.last_activity_at = summary.last_activity_at.max(timestamp(record.updated_at));
            if let Some(ended_at) = record.ended_at {
                summary.ended_at = Some(timestamp(ended_at));
            }
            if record.status.is_terminal() {
                summary.waiting_for.clear();
                summary.wakes_at = None;
                if summary.error.is_none() {
                    summary.error = ended_with(&app, &record).await?;
                }
            }
        }
        runs.push(summary);
    }
    Ok(Json(RunPage { runs, next_cursor }))
}

/// `GET /v1/runs/stats`.
pub(super) async fn stats(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(request): Query<RunStatsQuery>,
) -> Result<Json<RunStats>, ApiError> {
    caller.allow(Access::Read)?;
    let now = app.now();
    let query = search::run_query(&caller, request.q.as_deref(), None, now)?;
    let (since, until, bucket) = window(request.since, request.until, request.bucket, now)?;
    let top = usize::try_from(request.top.unwrap_or(10).clamp(1, 50)).unwrap_or(10);
    let stats = app
        .store
        .run_stats(
            &query,
            &StatsWindow {
                since,
                until: Some(until),
                bucket_ms: bucket,
                top,
            },
        )
        .await?;
    Ok(Json(RunStats {
        since: since.map(timestamp),
        until: timestamp(until),
        bucket: request.bucket,
        runs: stats.runs,
        statuses: search::status_counts(&stats.statuses),
        series: search::series(&stats.series, since, bucket),
        durations: search::durations(stats.durations),
        waiting: search::wait_counts(&stats.waiting),
        quarantined: stats.quarantined,
        facets: search::facets(&stats.facets),
    }))
}

/// `GET /v1/runs/failures`.
pub(super) async fn failures(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(request): Query<FailureQuery>,
) -> Result<Json<FailureGroupPage>, ApiError> {
    caller.allow(Access::Read)?;
    let now = app.now();
    let query = search::run_query(&caller, request.q.as_deref(), None, now)?;
    let (since, until, bucket) = window(request.since, request.until, request.bucket, now)?;
    let limit = usize::try_from(request.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
    let groups = app
        .store
        .failure_groups(
            &query,
            &FailureWindow {
                since,
                until: Some(until),
                bucket_ms: bucket,
                limit,
            },
        )
        .await?;
    Ok(Json(FailureGroupPage {
        since: since.map(timestamp),
        until: timestamp(until),
        bucket: request.bucket,
        runs: groups.runs,
        total_groups: groups.groups,
        groups: groups.top.iter().map(search::failure_group).collect(),
    }))
}

/// Buckets of a series at most.
const MAX_BUCKETS: u64 = 500;

/// The window of the statistics and the failure groups: `until` is now by default, and a
/// bucket needs `since`.
fn window(
    since: Option<jiff::Timestamp>,
    until: Option<jiff::Timestamp>,
    bucket: Option<rewind_api::common::Duration>,
    now: rewind_core::Timestamp,
) -> Result<
    (
        Option<rewind_core::Timestamp>,
        rewind_core::Timestamp,
        Option<u64>,
    ),
    ApiError,
> {
    let since = since.map(core_time);
    let until = until.map_or(now, core_time);
    if since.is_some_and(|since| since >= until) {
        return Err(ApiError::invalid(
            "since must be before until (now by default)",
        ));
    }
    let bucket = match (bucket, since) {
        (None, _) => None,
        (Some(_), None) => {
            return Err(ApiError::invalid(
                "bucket needs since: the series starts there",
            ));
        }
        (Some(bucket), Some(since)) => {
            let width = bucket.as_millis();
            if width == 0 {
                return Err(ApiError::invalid("bucket must be longer than 0ms"));
            }
            let span = until.as_millis().saturating_sub(since.as_millis());
            if span.div_ceil(width) > MAX_BUCKETS {
                return Err(ApiError::invalid(format!(
                    "{bucket} buckets from since to until make more than {MAX_BUCKETS}: widen the bucket or narrow the window"
                )));
            }
            Some(width)
        }
    };
    Ok((since, until, bucket))
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
    let fork = rewind_engine::ForkRun {
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
            rewind_format::encode_journal(&JournalDocument {
                run: record.run.id,
                script,
                toolset,
                entries: history,
            })
        }
        JournalExport::Cassette => {
            let cassette = rewind_core::Cassette::from_history(&history, Matching::Sequential)
                .map_err(ApiError::internal)?;
            rewind_format::encode_cassette(&cassette)
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
    app.record_quarantine(&record.run).await;
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
    app.record_quarantine(&record.run).await;
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
    Body(credentials): Body<rewind_api::toolsets::Credentials>,
) -> Result<StatusCode, ApiError> {
    caller.allow(Access::Write)?;
    let record = app.run(&caller, &id).await?;
    app.engine
        .set_run_credentials(&record.run, &toolsets::secrets(credentials))
        .await?;
    app.record_tasks(&record.run).await;
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
