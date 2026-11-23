//! Statistics (`archi/05-testing-and-observability.md`, "Statistics"): `GET /v1/runs/stats`,
//! `GET /v1/runs/failures`, `GET /v1/tools/stats` and `GET /v1/scripts`, from the hourly
//! counters of the partitions that hold their scope, added up at the edge.
//!
//! - The scope is the tenant, or one namespace: a token's, `Rewind-Namespace`'s, or the one `q`
//!   names with a single `namespace:` term. Any other term is refused with its position.
//! - The window is made of whole UTC hours, its bounds rounded out; a series' bucket is rounded
//!   up to whole hours.
//! - A grant or a token limited to labels reads none of them (403): counters carry no labels.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use futures_util::StreamExt;
use rewind_api::Problem;
use rewind_api::access::Permission;
use rewind_api::common::{Timestamp, timestamp};
use rewind_api::query::{self, Filter, QueryError};
use rewind_api::reading::{Latency, ToolStats, ToolStatsFilter, ToolStatsPage};
use rewind_api::runs::{
    FacetValue, Facets, FailureGroup, FailureGroupPage, FailureQuery, FailureSample, RunStats,
    RunStatsQuery, SeriesBucket, StatusCounts,
};
use rewind_api::scripts::{RunCounts, ScriptFilter, ScriptPage, ScriptSummary};
use rewind_core::{Digest, RunStatus};
use rewind_engine::ports::{
    Counters, Gauges, LatestRun, RunCounts as Counted, RunRef, Scope, Window,
};

use crate::app::App;
use crate::auth::Caller;
use crate::error::ApiError;
use crate::extract::Query;
use crate::http::refuse_nul_in_filter;
use crate::http::runs::core_time;
use crate::http::search::{durations, wait_counts};

/// Buckets of a series at most.
const MAX_BUCKETS: u64 = 500;
const HOUR_MS: u64 = 3_600_000;
const PAGE: u32 = 50;
const MAX_PAGE: u32 = 500;
/// The labels a failure group lists.
const TOP_LABELS: usize = 10;
/// The latest runs read at once for the labels of a page of failure groups.
const LABEL_READS: usize = 16;

/// The scope of a statistics request: the caller's tenant, or one namespace.
pub(crate) fn scope(caller: &Caller, q: Option<&str>) -> Result<Scope, ApiError> {
    if !caller.labels.is_empty() {
        return Err(ApiError::forbidden(
            "statistics come from counters, which know no labels: a grant or a token limited \
             to labels cannot read them. Search the runs instead (GET /v1/runs)",
        ));
    }
    let mut named = None;
    if let Some(text) = q {
        refuse_nul_in_filter("q", text)?;
        let parsed = query::parse(text)
            .map_err(|error| ApiError(Box::new(Problem::invalid_query(&error))))?;
        for term in parsed.terms {
            let refused = |message: &str| {
                ApiError(Box::new(Problem::invalid_query(&QueryError {
                    message: message.to_owned(),
                    span: term.span,
                })))
            };
            match term.filter {
                Filter::Namespace(names)
                    if !term.negated && names.len() == 1 && named.is_none() =>
                {
                    named = names.into_iter().next();
                }
                Filter::Namespace(_) => {
                    return Err(refused(
                        "statistics take one namespace: one term namespace:<name> of one value, \
                         not negated",
                    ));
                }
                _ => {
                    return Err(refused(
                        "statistics come from counters and take no term but namespace:<name>: \
                         search the runs (GET /v1/runs) to filter by anything else",
                    ));
                }
            }
        }
    }
    let named = named.map(|name| crate::views::engine_namespace(&name));
    let namespace = match (&caller.namespace, named) {
        (Some(bound), Some(named)) if *bound != named => {
            return Err(ApiError::forbidden(format!(
                "this request is bound to the namespace {bound}: it reads no other"
            )));
        }
        (Some(bound), _) => Some(bound.clone()),
        (None, named) => named,
    };
    Ok(match namespace {
        Some(namespace) => Scope::Namespace {
            tenant: caller.tenant.clone(),
            namespace,
        },
        None => Scope::Tenant(caller.tenant.clone()),
    })
}

/// The whole hours from `since` (the epoch without it) to `until` (now without it), and the
/// bucket of a series in whole hours, which needs `since`.
fn window(
    since: Option<jiff::Timestamp>,
    until: Option<jiff::Timestamp>,
    bucket: Option<rewind_api::common::Duration>,
    now: rewind_core::Timestamp,
) -> Result<(Window, Option<u32>), ApiError> {
    let since = since.map(core_time);
    let until = until.map_or(now, core_time);
    if since.is_some_and(|since| since >= until) {
        return Err(ApiError::invalid(
            "since must be before until (now by default)",
        ));
    }
    let window = Window::covering(since.unwrap_or(rewind_core::Timestamp(0)), until);
    let bucket = match (bucket, since) {
        (None, _) => None,
        (Some(_), None) => {
            return Err(ApiError::invalid(
                "bucket needs since: the series starts there",
            ));
        }
        (Some(bucket), Some(_)) => {
            let hours = bucket.as_millis().div_ceil(HOUR_MS).max(1);
            let span = u64::from(window.to.0.saturating_sub(window.from.0));
            if span.div_ceil(hours) > MAX_BUCKETS {
                return Err(ApiError::invalid(format!(
                    "{bucket} buckets from since to until make more than {MAX_BUCKETS}: widen \
                     the bucket or narrow the window"
                )));
            }
            Some(u32::try_from(hours).unwrap_or(u32::MAX))
        }
    };
    Ok((window, bucket))
}

/// The counters of each bucket of `hours` hours over the window: one request per partition.
async fn series(
    app: &App,
    scope: &Scope,
    window: Window,
    hours: Option<u32>,
) -> Result<Vec<(Window, Counters)>, ApiError> {
    let Some(hours) = hours.and_then(NonZeroU32::new) else {
        return Ok(Vec::new());
    };
    Ok(app
        .edge
        .series(&mut app.gateway(), scope, window, hours)
        .await?)
}

/// The runs of the window by status: those that ended, by their end; the active ones now.
fn statuses(runs: &Counted, gauges: &Gauges) -> StatusCounts {
    let mut counts = StatusCounts {
        active: gauges.active_runs,
        ..StatusCounts::default()
    };
    for (status, count) in &runs.ended {
        match status {
            RunStatus::Completed => counts.completed += count,
            RunStatus::Failed => counts.failed += count,
            RunStatus::Cancelled => counts.cancelled += count,
            RunStatus::Aborted => counts.aborted += count,
            RunStatus::Pending | RunStatus::Active => {}
        }
    }
    counts
}

fn ended(runs: &Counted) -> StatusCounts {
    statuses(runs, &Gauges::default())
}

/// The values of a facet, most runs first, at most `top`.
fn top(values: impl IntoIterator<Item = (String, u64)>, top: usize) -> Vec<FacetValue> {
    let mut values: Vec<FacetValue> = values
        .into_iter()
        .filter(|(_, runs)| *runs > 0)
        .map(|(value, runs)| FacetValue { value, runs })
        .collect();
    values.sort_by(|a, b| b.runs.cmp(&a.runs).then_with(|| a.value.cmp(&b.value)));
    values.truncate(top);
    values
}

/// `GET /v1/runs/stats`.
pub(super) async fn runs(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(request): Query<RunStatsQuery>,
) -> Result<Json<RunStats>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let scope = scope(&caller, request.q.as_deref())?;
    let (window, hours) = window(request.since, request.until, request.bucket, app.now())?;
    let top_n = usize::try_from(request.top.unwrap_or(10).clamp(1, 50)).unwrap_or(10);
    let (counters, gauges) = app
        .edge
        .statistics(&mut app.gateway(), &scope, window)
        .await?;
    let series = series(&app, &scope, window, hours).await?;
    let counts = statuses(&counters.runs, &gauges);
    let facets = Facets {
        statuses: top(
            [
                ("active", counts.active),
                ("completed", counts.completed),
                ("failed", counts.failed),
                ("cancelled", counts.cancelled),
                ("aborted", counts.aborted),
            ]
            .map(|(status, runs)| (status.to_owned(), runs)),
            top_n,
        ),
        workflows: top(
            counters
                .workflows
                .iter()
                .map(|(name, runs)| (name.clone(), runs.started)),
            top_n,
        ),
        tools: top(
            counters
                .tools
                .iter()
                .map(|(tool, calls)| (tool.clone(), calls.calls)),
            top_n,
        ),
        errors: top(failure_errors(&counters), top_n),
        scripts: top(
            counters
                .scripts
                .iter()
                .map(|(digest, script)| (digest.to_hex(), script.runs.started)),
            top_n,
        ),
        waiting: top(
            gauges
                .waiting
                .iter()
                .map(|(kind, runs)| (crate::http::search::wait_name(*kind).to_owned(), *runs)),
            top_n,
        ),
        // What erased namespaces consumed counts in the totals, under no namespace.
        namespaces: top(
            counters
                .namespaces
                .iter()
                .filter(|(namespace, _)| !namespace.is_erased())
                .map(|(namespace, runs)| (namespace.to_string(), runs.started)),
            top_n,
        ),
        ..Facets::default()
    };
    Ok(Json(RunStats {
        since: request.since.map(|_| timestamp(window.start())),
        until: timestamp(window.end()),
        bucket: hours.map(|hours| {
            rewind_api::common::Duration::from(std::time::Duration::from_secs(
                u64::from(hours) * 3_600,
            ))
        }),
        runs: counters.runs.started,
        statuses: counts,
        series: series
            .iter()
            .map(|(bucket, counters)| SeriesBucket {
                at: timestamp(bucket.start()),
                statuses: ended(&counters.runs),
            })
            .collect(),
        durations: durations(counters.durations.latency()),
        waiting: wait_counts(&gauges.waiting),
        quarantined: gauges.quarantined,
        facets,
    }))
}

/// The labels of each failure group's latest runs, as `key:value`, most runs first: read from
/// the runs' owners, as many at once as [`LABEL_READS`]. A run that cannot be read (erased since,
/// its partition moving) adds none.
async fn latest_labels<'a>(
    app: &App,
    caller: &Caller,
    groups: impl Iterator<Item = &'a Vec<LatestRun>>,
) -> Vec<Vec<FacetValue>> {
    let runs: Vec<(usize, RunRef)> = groups
        .enumerate()
        .flat_map(|(group, latest)| {
            latest
                .iter()
                .map(move |run| (group, RunRef::new(caller.tenant.clone(), run.run)))
        })
        .collect();
    let groups = runs.last().map_or(0, |(group, _)| group + 1);
    let mut counts = vec![BTreeMap::<String, u64>::new(); groups];
    let mut reads = futures_util::stream::iter(runs)
        .map(|(group, run)| async move { (group, app.read_run(&run).await) })
        .buffer_unordered(LABEL_READS);
    while let Some((group, view)) = reads.next().await {
        let Ok(Some(view)) = view else {
            continue;
        };
        for (key, value) in &view.head.metadata.labels {
            *counts[group].entry(format!("{key}:{value}")).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .map(|counts| top(counts, TOP_LABELS))
        .collect()
}

/// The runs of each error name of the failure groups.
fn failure_errors(counters: &Counters) -> BTreeMap<String, u64> {
    let mut errors = BTreeMap::new();
    for group in counters.failures.values() {
        *errors.entry(group.error.clone()).or_insert(0) += group.runs;
    }
    errors
}

/// `GET /v1/runs/failures`: the failure groups of the window, most runs first.
pub(super) async fn failures(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(request): Query<FailureQuery>,
) -> Result<Json<FailureGroupPage>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let scope = scope(&caller, request.q.as_deref())?;
    let (window, hours) = window(request.since, request.until, request.bucket, app.now())?;
    let limit = usize::try_from(request.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
    let (counters, _) = app
        .edge
        .statistics(&mut app.gateway(), &scope, window)
        .await?;
    let series = series(&app, &scope, window, hours).await?;
    let mut groups: Vec<_> = counters.failures.iter().collect();
    groups.sort_by(|(a, x), (b, y)| y.runs.cmp(&x.runs).then_with(|| a.cmp(b)));
    let total_groups = groups.len() as u64;
    let runs = groups.iter().map(|(_, group)| group.runs).sum();
    let page: Vec<_> = groups.into_iter().take(limit).collect();
    let labels = latest_labels(&app, &caller, page.iter().map(|(_, group)| &group.latest)).await;
    let groups = page
        .into_iter()
        .zip(labels)
        .filter_map(|((fingerprint, group), labels)| {
            let latest = group.latest.first()?;
            let ends = group.latest.iter().map(|run| run.ended_at);
            Some(FailureGroup {
                fingerprint: fingerprint.clone(),
                signature: group.signature.clone(),
                error: group.error.clone(),
                runs: group.runs,
                series: series
                    .iter()
                    .map(|(_, counters)| {
                        counters
                            .failures
                            .get(fingerprint)
                            .map_or(0, |group| group.runs)
                    })
                    .collect(),
                scripts: group.scripts.len() as u64,
                first_ended_at: timestamp(ends.clone().min().unwrap_or(latest.ended_at)),
                last_ended_at: timestamp(ends.max().unwrap_or(latest.ended_at)),
                line: group
                    .lines
                    .iter()
                    .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                    .map(|(line, _)| *line),
                labels,
                latest: FailureSample {
                    id: latest.run.to_string(),
                    status: latest.status.into(),
                    message: latest.message.clone(),
                },
            })
        })
        .collect();
    Ok(Json(FailureGroupPage {
        since: request.since.map(|_| timestamp(window.start())),
        until: timestamp(window.end()),
        bucket: hours.map(|hours| {
            rewind_api::common::Duration::from(std::time::Duration::from_secs(
                u64::from(hours) * 3_600,
            ))
        }),
        runs,
        total_groups,
        groups,
    }))
}

/// `GET /v1/tools/stats`: per tool, over the calls scheduled in the window (the last 24 hours
/// by default).
pub(super) async fn tools(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(filter): Query<ToolStatsFilter>,
) -> Result<Json<ToolStatsPage>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let scope = scope(&caller, filter.q.as_deref())?;
    if filter.toolset.is_some() {
        return Err(ApiError::invalid(
            "toolset: tool statistics are per tool, without a per-toolset breakdown",
        ));
    }
    for text in [&filter.prefix, &filter.tool].into_iter().flatten() {
        refuse_nul_in_filter("tool", text)?;
    }
    let now = app.now();
    let until = filter.until.map_or(now, core_time);
    let since = filter.since.map_or_else(
        || rewind_core::Timestamp::from_millis(until.as_millis().saturating_sub(24 * HOUR_MS)),
        core_time,
    );
    if since >= until {
        return Err(ApiError::invalid(
            "since must be before until (now by default)",
        ));
    }
    let window = Window::covering(since, until);
    let (counters, _) = app
        .edge
        .statistics(&mut app.gateway(), &scope, window)
        .await?;
    let tools = counters
        .tools
        .iter()
        .filter(|(tool, _)| {
            filter
                .prefix
                .as_ref()
                .is_none_or(|prefix| tool.starts_with(prefix.as_str()))
                && filter.tool.as_ref().is_none_or(|only| *tool == only)
        })
        .map(|(tool, calls)| {
            let latency = calls.latency.latency();
            ToolStats {
                tool: tool.clone(),
                calls: calls.calls,
                errors: calls.failed,
                error_kinds: calls
                    .errors
                    .iter()
                    .map(|(kind, count)| (kind.as_str().to_owned(), *count))
                    .collect(),
                retries: calls.retries,
                latency_ms: Latency {
                    p50: latency.map_or(0, |latency| latency.p50_ms),
                    p95: latency.map_or(0, |latency| latency.p95_ms),
                    p99: latency.map_or(0, |latency| latency.p99_ms),
                    max: latency.map_or(0, |latency| latency.max_ms),
                },
            }
        })
        .collect();
    Ok(Json(ToolStatsPage {
        since: timestamp(window.start()),
        until: timestamp(window.end()),
        tools,
    }))
}

/// A script's runs by status, as the API counts them: those started and not ended are active.
pub(crate) fn run_counts(runs: &Counted) -> RunCounts {
    let ended = ended(runs);
    let finished = ended.completed + ended.failed + ended.cancelled + ended.aborted;
    RunCounts {
        pending: 0,
        active: runs.started.saturating_sub(finished),
        completed: ended.completed,
        failed: ended.failed,
        cancelled: ended.cancelled,
        aborted: ended.aborted,
    }
}

/// `GET /v1/scripts`: every distinct script the scope's runs ran in the window, most recently
/// run first.
pub(super) async fn scripts(
    State(app): State<Arc<App>>,
    caller: Caller,
    Query(query): Query<ScriptFilter>,
) -> Result<Json<ScriptPage>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let scope = scope(&caller, query.q.as_deref())?;
    let limit = usize::try_from(query.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE)).unwrap_or(50);
    if let Some(tool) = &query.tool {
        refuse_nul_in_filter("tool", tool)?;
    }
    let digest = match &query.digest {
        Some(digest) => Some(
            Digest::from_hex(digest)
                .map_err(|_| ApiError::invalid(format!("digest {digest:?}: expected a SHA-256")))?,
        ),
        None => None,
    };
    let (window, _) = window(query.since, query.until, None, app.now())?;
    let (counters, _) = app
        .edge
        .statistics(&mut app.gateway(), &scope, window)
        .await?;
    let mut scripts: Vec<_> = counters
        .scripts
        .iter()
        .filter(|(script, counts)| {
            digest.is_none_or(|digest| **script == digest)
                && query
                    .tool
                    .as_ref()
                    .is_none_or(|tool| counts.tools.contains(tool))
        })
        .collect();
    // Most recently run first; the digest breaks ties. The cursor is the last one sent.
    let key = |(script, counts): &(&Digest, &rewind_engine::ports::ScriptCounts)| {
        (
            std::cmp::Reverse(counts.last_started_at.map_or(0, |at| at.0)),
            **script,
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
        scripts.retain(|entry| key(entry) > after);
    }
    let next_cursor = (scripts.len() > limit).then(|| {
        let (script, counts) = scripts[limit - 1];
        format!(
            "{}.{}",
            counts.last_started_at.map_or(0, |at| at.0),
            script.to_hex()
        )
    });
    scripts.truncate(limit);
    let mut page = Vec::with_capacity(scripts.len());
    for (script, counts) in scripts {
        page.push(ScriptSummary {
            digest: script.to_hex(),
            headline: app.headline(&caller.tenant, *script).await,
            first_run_at: counts
                .first_started_at
                .map_or(Timestamp::UNIX_EPOCH, timestamp),
            last_run_at: counts
                .last_started_at
                .map_or(Timestamp::UNIX_EPOCH, timestamp),
            runs: run_counts(&counts.runs),
            tools: counts.tools.iter().cloned().collect(),
        });
    }
    Ok(Json(ScriptPage {
        scripts: page,
        next_cursor,
    }))
}
