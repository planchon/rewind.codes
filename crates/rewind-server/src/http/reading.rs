//! Reading across runs, for the dashboard: the scripts executed, tool statistics, payloads
//! (`archi/05-testing-and-observability.md`, "Read model" and "Dashboard").

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use rewind_api::access::Permission;
use rewind_api::common::{RunStatus, Timestamp, timestamp};
use rewind_api::reading::{Latency, PayloadQuery, ToolStats, ToolStatsFilter, ToolStatsPage};
use rewind_api::scripts::{RunCounts, Script, ScriptFilter, ScriptPage, ScriptSummary};
use rewind_core::{Digest, canonical_json};
use rewind_engine::ports::{JournalStore, ReadModel};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::Query;
use crate::http::refuse_nul_in_filter;
use crate::http::runs::core_time;

const PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;

/// The read model's filter of scripts, from the API's, within the caller's labels.
fn filter(
    caller: &Caller,
    digest: Option<&str>,
    labels: &[String],
    tool: Option<String>,
) -> Result<Option<rewind_engine::ports::ScriptFilter>, ApiError> {
    let mut wanted = BTreeMap::new();
    for label in labels {
        refuse_nul_in_filter("label", label)?;
        let (key, value) = label
            .split_once(':')
            .ok_or_else(|| ApiError::invalid(format!("label {label:?}: expected key:value")))?;
        wanted.insert(key.to_owned(), value.to_owned());
    }
    if let Some(tool) = &tool {
        refuse_nul_in_filter("tool", tool)?;
    }
    let Ok(labels) = caller.within(wanted) else {
        return Ok(None);
    };
    let digest = match digest {
        Some(digest) => Some(
            Digest::from_hex(digest)
                .map_err(|_| ApiError::invalid(format!("digest {digest:?}: expected a SHA-256")))?,
        ),
        None => None,
    };
    let mut filter = rewind_engine::ports::ScriptFilter::new(caller.tenant.clone());
    filter.digest = digest;
    filter.labels = labels;
    filter.tool = tool;
    Ok(Some(filter))
}

fn counts(statuses: &BTreeMap<rewind_core::RunStatus, u64>) -> RunCounts {
    let mut counts = RunCounts::default();
    for (status, count) in statuses {
        let slot = match RunStatus::from(*status) {
            RunStatus::Pending => &mut counts.pending,
            RunStatus::Active => &mut counts.active,
            RunStatus::Completed => &mut counts.completed,
            RunStatus::Failed => &mut counts.failed,
            RunStatus::Cancelled => &mut counts.cancelled,
            RunStatus::Aborted => &mut counts.aborted,
        };
        *slot += count;
    }
    counts
}

/// `GET /v1/scripts`: every distinct script, most recently run first.
pub(super) async fn scripts(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(query): Query<ScriptFilter>,
) -> Result<Json<ScriptPage>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let limit = usize::try_from(query.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
    let Some(filter) = filter(&caller, query.digest.as_deref(), &query.labels, query.tool)? else {
        return Ok(Json(ScriptPage {
            scripts: Vec::new(),
            next_cursor: None,
        }));
    };
    let mut scripts = app.store.scripts(&filter).await?;
    // Most recently run first; the digest breaks ties. The cursor is the last one sent.
    let key = |summary: &rewind_engine::ports::ScriptSummary| {
        (
            std::cmp::Reverse(summary.last_started_at.map_or(0, |at| at.0)),
            summary.script,
        )
    };
    scripts.sort_by_key(key);
    if let Some(cursor) = &query.cursor {
        let (at, digest) = cursor
            .split_once('.')
            .and_then(|(at, digest)| {
                Some((at.parse::<u64>().ok()?, Digest::from_hex(digest).ok()?))
            })
            .ok_or_else(|| ApiError::invalid(format!("cursor {cursor:?} is not valid")))?;
        let after = (std::cmp::Reverse(at), digest);
        scripts.retain(|summary| key(summary) > after);
    }
    let next_cursor = (scripts.len() > limit).then(|| {
        let last = &scripts[limit - 1];
        format!(
            "{}.{}",
            last.last_started_at.map_or(0, |at| at.0),
            last.script.to_hex()
        )
    });
    scripts.truncate(limit);
    let mut page = Vec::with_capacity(scripts.len());
    for summary in scripts {
        page.push(ScriptSummary {
            digest: summary.script.to_hex(),
            headline: app.headline(&caller.tenant, summary.script).await,
            first_run_at: summary
                .first_started_at
                .map_or(Timestamp::UNIX_EPOCH, timestamp),
            last_run_at: summary
                .last_started_at
                .map_or(Timestamp::UNIX_EPOCH, timestamp),
            runs: counts(&summary.statuses),
            tools: summary.tools.into_iter().collect(),
            labels: summary.labels,
        });
    }
    Ok(Json(ScriptPage {
        scripts: page,
        next_cursor,
    }))
}

/// `GET /v1/scripts/{digest}`: the source, and what the runs the caller sees did with it.
pub(super) async fn script(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(digest): Path<String>,
) -> Result<Json<Script>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let missing = || ApiError::not_found(format!("no script {digest} for this tenant"));
    let parsed = Digest::from_hex(&digest).map_err(|_| missing())?;
    let stored = app
        .store
        .get_script(&caller.tenant, parsed)
        .await?
        .ok_or_else(missing)?;
    let summary = match filter(&caller, Some(&digest), &[], None)? {
        Some(filter) => app.store.scripts(&filter).await?.into_iter().next(),
        None => None,
    };
    if caller.scoped() && summary.is_none() {
        // A key limited to labels reads the scripts of the runs it sees, once they are listed.
        return Err(missing());
    }
    let source = stored.script.source().to_owned();
    Ok(Json(Script {
        digest: parsed.to_hex(),
        headline: rewind_api::scripts::headline(&source),
        source,
        runs: summary
            .as_ref()
            .map_or_else(RunCounts::default, |summary| counts(&summary.statuses)),
        tools: summary
            .as_ref()
            .map(|summary| summary.tools.iter().cloned().collect())
            .unwrap_or_default(),
        first_run_at: summary
            .as_ref()
            .and_then(|summary| summary.first_started_at)
            .map(timestamp),
        last_run_at: summary
            .as_ref()
            .and_then(|summary| summary.last_started_at)
            .map(timestamp),
    }))
}

/// `GET /v1/tools/stats`: per tool, over the calls scheduled in the window (the last 24 hours by
/// default).
pub(super) async fn tool_stats(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(query): Query<ToolStatsFilter>,
) -> Result<Json<ToolStatsPage>, ApiError> {
    caller.require(Permission::RunsRead)?;
    if caller.scoped() {
        return Err(ApiError::forbidden(
            "tool statistics span every run of the tenant: a key limited to labels cannot read \
             them",
        ));
    }
    let until = query.until.map_or_else(|| app.now(), core_time);
    let since = query.since.map_or_else(
        || rewind_core::Timestamp(until.0.saturating_sub(86_400_000)),
        core_time,
    );
    let toolset =
        match &query.toolset {
            Some(toolset) => Some(Digest::from_hex(toolset).map_err(|_| {
                ApiError::invalid(format!("toolset {toolset:?}: expected a digest"))
            })?),
            None => None,
        };
    let stats = app
        .store
        .tool_stats(&rewind_engine::ports::ToolStatsFilter {
            tenant: caller.tenant.clone(),
            namespace: None,
            toolset,
            since: Some(since),
            until: Some(until),
        })
        .await?;
    let tools = stats
        .into_iter()
        .filter(|stats| query.tool.as_ref().is_none_or(|tool| *tool == stats.tool))
        .map(|stats| ToolStats {
            tool: stats.tool,
            calls: stats.calls,
            errors: stats.failed,
            error_kinds: stats
                .errors_by_kind
                .into_iter()
                .map(|(kind, count)| (kind.as_str().to_owned(), count))
                .collect(),
            retries: stats.retries,
            latency_ms: stats.latency.map_or(
                Latency {
                    p50: 0,
                    p95: 0,
                    p99: 0,
                    max: 0,
                },
                |latency| Latency {
                    p50: latency.p50_ms,
                    p95: latency.p95_ms,
                    p99: latency.p99_ms,
                    max: latency.max_ms,
                },
            ),
        })
        .collect();
    Ok(Json(ToolStatsPage {
        since: timestamp(since),
        until: timestamp(until),
        tools,
    }))
}

/// `GET /v1/payloads/{digest}`: the value, as canonical JSON. Through `run` when it is given,
/// which a key limited to labels must do: the run must be one the caller sees, and its journal
/// must hold the payload.
pub(super) async fn payload(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(digest): Path<String>,
    Query(query): Query<PayloadQuery>,
) -> Result<Response, ApiError> {
    caller.require(Permission::RunsRead)?;
    let missing = || ApiError::not_found(format!("no payload {digest} for this tenant"));
    let parsed = Digest::from_hex(&digest).map_err(|_| missing())?;
    match &query.run {
        Some(run) => {
            let record = app.run(&caller, run).await?;
            let history = app.views().history(&record).await?;
            let held = history.iter().any(|entry| {
                crate::views::payload_of(&entry.event)
                    .is_some_and(|payload| payload.digest() == parsed)
            });
            if !held {
                return Err(ApiError::not_found(format!(
                    "no payload {digest} in the journal of run {run}"
                )));
            }
        }
        None if caller.scoped() => {
            return Err(ApiError::forbidden(format!(
                "a key limited to labels reads the payloads of the runs it sees: name the run \
                 (GET /v1/payloads/{digest}?run=<id>)"
            )));
        }
        None => {}
    }
    let value = app
        .views()
        .value(&caller.tenant, parsed)
        .await?
        .ok_or_else(missing)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/json"),
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable",
            ),
        ],
        canonical_json(&value),
    )
        .into_response())
}
