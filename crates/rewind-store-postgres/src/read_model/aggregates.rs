//! What the read model counts: `run_stats` and `failure_groups` over the runs a query matches,
//! `scripts` and `tool_stats` over a tenant's runs and calls.
//!
//! Each is one statement of SQL aggregates, so that its numbers come from one snapshot of the
//! database, and only what it returns leaves it: counts per group, at most `top` values per
//! facet (`LIMIT`), at most `limit` failure groups. The runs a statement counts are materialized
//! CTEs that its parts share, each found by an index: the window's runs by their start or end,
//! the runs that may wait by the partial index of active runs. Joins to a run's rows name the
//! tenant, whose indexes and statistics they then use. A statement returning parts of several
//! shapes returns them as tagged rows (`UNION ALL`). Percentiles are `percentile_disc`, which is
//! nearest-rank: the first value whose position reaches `p × n`, rounded up.

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{Digest, RunStatus, Timestamp, ToolFailureKind};
use rewind_engine::ports::{
    FacetValue, Facets, FailureGroup, FailureGroups, FailureWindow, LabelFacet, Latency, RunQuery,
    RunStats, ScriptFilter, ScriptSummary, StatsWindow, ToolStats, ToolStatsFilter, WaitingFor,
};
use sqlx::PgPool;
use sqlx::Row as _;
use sqlx::postgres::PgRow;

use super::query::{Sql, WAIT_TASKS, WAITING_FOR, push_matching};
use crate::Error;
use crate::codec;
use crate::sql::{Columns, bytes, int, limit, tenant, ts};

/// Every status, in the order of `RunStatus`: what `run_stats` counts, zeros included.
const STATUSES: [RunStatus; 6] = [
    RunStatus::Pending,
    RunStatus::Active,
    RunStatus::Completed,
    RunStatus::Failed,
    RunStatus::Cancelled,
    RunStatus::Aborted,
];

/// Labels a failure group lists at most.
const GROUP_LABELS: i64 = 5;

/// The latency columns of a set of milliseconds `ms`: count, mean rounded down, p50, p95, p99,
/// max. All null but the count without any.
const LATENCY: &str = "count(ms), div(sum(ms), NULLIF(count(ms), 0))::bigint, \
     percentile_disc(0.5) WITHIN GROUP (ORDER BY ms), \
     percentile_disc(0.95) WITHIN GROUP (ORDER BY ms), \
     percentile_disc(0.99) WITHIN GROUP (ORDER BY ms), max(ms)";

/// The buckets of a series: from `since`, each `width` wide, up to `until`.
#[derive(Debug, Clone, Copy)]
struct Buckets {
    since: i64,
    width: i64,
    count: usize,
}

impl Buckets {
    fn new(since: Option<Timestamp>, until: Option<Timestamp>, width: Option<u64>) -> Option<Self> {
        let (since, until, width) = (since?, until?, width.filter(|width| *width > 0)?);
        let count = until.0.saturating_sub(since.0).div_ceil(width);
        Some(Self {
            since: ts(since),
            width: int(width),
            count: usize::try_from(count).unwrap_or(usize::MAX),
        })
    }

    /// The SQL of the bucket of a time column, a `bigint`.
    fn push_index(&self, sql: &mut Sql<'_>, column: &str) {
        sql.push("((")
            .push(column)
            .push(" - ")
            .push_bind(self.since)
            .push(") / ")
            .push_bind(self.width)
            .push(")");
    }
}

fn statuses() -> BTreeMap<RunStatus, u64> {
    STATUSES.iter().map(|status| (*status, 0)).collect()
}

fn parse_status(name: &str) -> Result<RunStatus, Error> {
    codec::parse_status(name)
}

fn count(value: i64) -> Result<u64, Error> {
    u64::try_from(value).map_err(|_| Error::Corrupt(format!("negative count {value}")))
}

/// The most frequent values first, then by value (byte order); at most `top`.
fn ranked(mut values: Vec<FacetValue>, top: usize) -> Vec<FacetValue> {
    values.sort_by(|a, b| b.runs.cmp(&a.runs).then_with(|| a.value.cmp(&b.value)));
    values.truncate(top);
    values
}

/// A latency from its columns at `at` ([`LATENCY`]); `None` without any value.
fn latency(row: &PgRow, at: usize) -> Result<Option<Latency>, Error> {
    let count = row.u64(at)?;
    if count == 0 {
        return Ok(None);
    }
    Ok(Some(Latency {
        count,
        mean_ms: row.u64(at + 1)?,
        p50_ms: row.u64(at + 2)?,
        p95_ms: row.u64(at + 3)?,
        p99_ms: row.u64(at + 4)?,
        max_ms: row.u64(at + 5)?,
    }))
}

/// The values of an array column; none for a null.
fn array<T>(row: &PgRow, at: usize) -> Result<Vec<T>, Error>
where
    Vec<T>: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    let values: Option<Vec<T>> = row.try_get(at)?;
    Ok(values.unwrap_or_default())
}

// -------------------------------------------------------------------------------------------
// run_stats

/// Counts, series, durations and facets of the runs a query matches, in one statement of three
/// sets of runs, each found by its own index: `win` the runs started in the window (by start),
/// `active` the runs that may wait (by the partial index of active runs), and the quarantined
/// runs (by `run_quarantine`). Only a window without bounds reads every run of the tenant.
pub(super) async fn run_stats(
    pool: &PgPool,
    query: &RunQuery,
    window: &StatsWindow,
) -> Result<RunStats, Error> {
    let buckets = Buckets::new(window.since, window.until, window.bucket_ms);
    let top = window.top;
    let tenant_id = tenant(&query.tenant).to_owned();
    let mut sql = Sql::new(
        "WITH win AS MATERIALIZED (SELECT s.run_id, s.status, s.started_at, s.ended_at, \
             s.script, s.error, s.namespace FROM run_summaries s WHERE ",
    );
    push_matching(&mut sql, query);
    if window.since.is_some() || window.until.is_some() {
        sql.push(" AND s.started_at IS NOT NULL");
        if let Some(since) = window.since {
            sql.push(" AND s.started_at >= ").push_bind(ts(since));
        }
        if let Some(until) = window.until {
            sql.push(" AND s.started_at < ").push_bind(ts(until));
        }
    }
    sql.push("), active AS MATERIALIZED (SELECT s.run_id FROM run_summaries s WHERE ");
    push_matching(&mut sql, query);
    sql.push(" AND s.status = 'active'), waits AS MATERIALIZED (SELECT DISTINCT a.run_id, ")
        .push(WAITING_FOR)
        .push(" AS kind FROM active a JOIN run_waits w ON w.run_id = a.run_id AND w.tenant = ")
        .push_bind(tenant_id.clone())
        .push(" ")
        .push(WAIT_TASKS)
        .push("), durations (n, mean, p50, p95, p99, top) AS (SELECT ")
        .push(LATENCY)
        .push(
            " FROM (SELECT GREATEST(ended_at - started_at, 0) AS ms FROM win \
             WHERE started_at IS NOT NULL AND ended_at IS NOT NULL) d)",
        );
    if top > 0 {
        sql.push(
            ", keys AS (SELECT l.key, count(*) AS runs FROM win \
             JOIN run_labels l ON l.run_id = win.run_id AND l.tenant = ",
        )
        .push_bind(tenant_id.clone())
        .push(" GROUP BY l.key ORDER BY count(*) DESC, l.key LIMIT ")
        .push_bind(limit(top))
        .push(")");
    }
    sql.push(
        " SELECT 'status', win.status, NULL::text, count(*) FROM win GROUP BY win.status \
         UNION ALL SELECT 'waiting', kind, NULL, count(*) FROM waits GROUP BY kind \
         UNION ALL SELECT 'waiting_window', kind, NULL, count(*) FROM waits \
             WHERE run_id IN (SELECT run_id FROM win) GROUP BY kind \
         UNION ALL SELECT 'duration', v.name, NULL, v.ms FROM durations d CROSS JOIN LATERAL ( \
             VALUES ('0', d.n), ('1', d.mean), ('2', d.p50), ('3', d.p95), ('4', d.p99), \
                 ('5', d.top)) \
             AS v(name, ms) \
         UNION ALL SELECT 'quarantined', NULL, NULL, count(*) FROM run_summaries s \
             WHERE EXISTS (SELECT 1 FROM run_quarantine q WHERE q.run_id = s.run_id \
                 AND q.tenant = s.tenant AND q.kind IS NOT NULL) AND ",
    );
    push_matching(&mut sql, query);
    if let Some(buckets) = buckets {
        sql.push(" UNION ALL SELECT 'series', ");
        buckets.push_index(&mut sql, "win.started_at");
        sql.push("::text, win.status, count(*) FROM win GROUP BY 2, 3");
    }
    if top > 0 {
        let top = limit(top);
        sql.push(
            " UNION ALL SELECT 'label', keys.key, NULL, keys.runs FROM keys \
             UNION ALL SELECT 'label_value', v.key, v.value, v.runs FROM ( \
                 SELECT l.key, l.value, count(*) AS runs, row_number() OVER ( \
                     PARTITION BY l.key ORDER BY count(*) DESC, l.value) AS rank \
                 FROM win JOIN run_labels l ON l.run_id = win.run_id AND l.tenant = ",
        )
        .push_bind(tenant_id.clone())
        .push(
            " WHERE l.key IN (SELECT key FROM keys) GROUP BY l.key, l.value) v \
             WHERE v.rank <= ",
        )
        .push_bind(top)
        .push(
            " UNION ALL (SELECT 'tool', c.tool, NULL, count(DISTINCT c.run_id) FROM win \
                 JOIN tool_calls c ON c.run_id = win.run_id AND c.tenant = ",
        )
        .push_bind(tenant_id.clone())
        .push(
            " WHERE c.tool IS NOT NULL \
                 GROUP BY c.tool ORDER BY count(DISTINCT c.run_id) DESC, c.tool LIMIT ",
        )
        .push_bind(top)
        .push(
            ") UNION ALL (SELECT 'error', e.name, NULL, count(*) FROM ( \
                 SELECT win.run_id, win.error AS name FROM win WHERE win.error IS NOT NULL \
                 UNION SELECT c.run_id, c.error_kind FROM win \
                     JOIN tool_calls c ON c.run_id = win.run_id AND c.tenant = ",
        )
        .push_bind(tenant_id.clone())
        .push(
            " WHERE c.outcome = 'failed') e \
                 GROUP BY e.name ORDER BY count(*) DESC, e.name LIMIT ",
        )
        .push_bind(top)
        .push(
            ") UNION ALL (SELECT 'script', encode(win.script, 'hex'), NULL, count(*) FROM win \
                 GROUP BY win.script ORDER BY count(*) DESC, win.script LIMIT ",
        )
        .push_bind(top)
        .push(
            ") UNION ALL (SELECT 'namespace', win.namespace, NULL, count(*) FROM win \
                 GROUP BY win.namespace ORDER BY count(*) DESC, win.namespace LIMIT ",
        )
        .push_bind(top)
        .push(
            ") UNION ALL (SELECT 'signal', w.name, NULL, count(DISTINCT w.run_id) FROM win \
                 JOIN run_waits w ON w.run_id = win.run_id AND w.tenant = ",
        )
        .push_bind(tenant_id)
        .push(
            " WHERE win.status = 'active' AND w.kind = 'signal' AND w.name IS NOT NULL \
                 GROUP BY w.name ORDER BY count(DISTINCT w.run_id) DESC, w.name LIMIT ",
        )
        .push_bind(top)
        .push(")");
    }
    let rows = sql.build().persistent(false).fetch_all(pool).await?;

    let mut stats = RunStats {
        runs: 0,
        statuses: statuses(),
        series: vec![statuses(); buckets.map_or(0, |buckets| buckets.count)],
        durations: None,
        waiting: WaitingFor::ALL.iter().map(|kind| (*kind, 0)).collect(),
        quarantined: 0,
        facets: Facets::default(),
    };
    let mut durations = [0_u64; 6];
    let mut has_durations = false;
    let mut waiting_window: Vec<FacetValue> = Vec::new();
    let mut keys: Vec<LabelFacet> = Vec::new();
    let mut label_values: BTreeMap<String, Vec<FacetValue>> = BTreeMap::new();
    for row in &rows {
        let section = row.text(0)?;
        let name = row.opt_text(1)?;
        let runs = row.opt_i64(3)?;
        let named = || {
            name.clone()
                .ok_or_else(|| Error::Corrupt(format!("{section} without name")))
        };
        let value = |runs: u64| FacetValue {
            value: name.clone().unwrap_or_default(),
            runs,
        };
        match section.as_str() {
            "duration" => {
                let at: usize = named()?
                    .parse()
                    .map_err(|_| Error::Corrupt("a duration's column".into()))?;
                if let Some(ms) = runs {
                    has_durations |= at == 0 && ms > 0;
                    durations[at] = count(ms)?;
                }
            }
            "status" => {
                let runs = count(runs.unwrap_or(0))?;
                stats.statuses.insert(parse_status(&named()?)?, runs);
                stats.runs += runs;
                stats.facets.statuses.push(value(runs));
            }
            "waiting" => {
                let kind = codec::parse_waiting_for(&named()?)?;
                stats.waiting.insert(kind, count(runs.unwrap_or(0))?);
            }
            "waiting_window" => waiting_window.push(value(count(runs.unwrap_or(0))?)),
            "quarantined" => stats.quarantined = count(runs.unwrap_or(0))?,
            "series" => {
                let index: usize = named()?
                    .parse()
                    .map_err(|_| Error::Corrupt("a bucket".into()))?;
                let status = parse_status(&row.text(2)?)?;
                if let Some(bucket) = stats.series.get_mut(index) {
                    bucket.insert(status, count(runs.unwrap_or(0))?);
                }
            }
            "label" => keys.push(LabelFacet {
                key: named()?,
                runs: count(runs.unwrap_or(0))?,
                values: Vec::new(),
            }),
            "label_value" => label_values.entry(named()?).or_default().push(FacetValue {
                value: row.text(2)?,
                runs: count(runs.unwrap_or(0))?,
            }),
            "tool" => stats.facets.tools.push(value(count(runs.unwrap_or(0))?)),
            "error" => stats.facets.errors.push(value(count(runs.unwrap_or(0))?)),
            "script" => stats.facets.scripts.push(value(count(runs.unwrap_or(0))?)),
            "namespace" => stats
                .facets
                .namespaces
                .push(value(count(runs.unwrap_or(0))?)),
            "signal" => stats.facets.signals.push(value(count(runs.unwrap_or(0))?)),
            other => return Err(Error::Corrupt(format!("a statistic {other:?}"))),
        }
    }
    if has_durations {
        let [count, mean_ms, p50_ms, p95_ms, p99_ms, max_ms] = durations;
        stats.durations = Some(Latency {
            count,
            mean_ms,
            p50_ms,
            p95_ms,
            p99_ms,
            max_ms,
        });
    }
    keys.sort_by(|a, b| b.runs.cmp(&a.runs).then_with(|| a.key.cmp(&b.key)));
    keys.truncate(top);
    for key in &mut keys {
        key.values = ranked(label_values.remove(&key.key).unwrap_or_default(), top);
    }
    let facets = &mut stats.facets;
    facets.namespaces = ranked(std::mem::take(&mut facets.namespaces), top);
    facets.statuses = ranked(std::mem::take(&mut facets.statuses), top);
    facets.labels = keys;
    facets.tools = ranked(std::mem::take(&mut facets.tools), top);
    facets.errors = ranked(std::mem::take(&mut facets.errors), top);
    facets.scripts = ranked(std::mem::take(&mut facets.scripts), top);
    facets.waiting = ranked(waiting_window, top);
    facets.signals = ranked(std::mem::take(&mut facets.signals), top);
    Ok(stats)
}

// -------------------------------------------------------------------------------------------
// failure_groups

/// The runs with an ending a query matches and that ended in the window (`m`), by fingerprint:
/// one row per group of the `limit` first (`top`), each with its latest run, its most common
/// line, its series and its labels as arrays; and the totals, on every row, or alone.
pub(super) async fn failure_groups(
    pool: &PgPool,
    query: &RunQuery,
    window: &FailureWindow,
) -> Result<FailureGroups, Error> {
    let buckets = Buckets::new(window.since, window.until, window.bucket_ms);
    let mut sql = Sql::new(
        "WITH m AS MATERIALIZED (SELECT s.run_id, s.script, s.status, s.ended_at, s.error, \
             s.message, s.line, s.signature, s.fingerprint FROM run_summaries s WHERE ",
    );
    push_matching(&mut sql, query);
    sql.push(" AND s.fingerprint IS NOT NULL AND s.ended_at IS NOT NULL");
    if let Some(since) = window.since {
        sql.push(" AND s.ended_at >= ").push_bind(ts(since));
    }
    if let Some(until) = window.until {
        sql.push(" AND s.ended_at < ").push_bind(ts(until));
    }
    sql.push(
        "), fingerprints AS (SELECT fingerprint, count(*) AS runs, \
             count(DISTINCT script) AS scripts, min(ended_at) AS first_ended_at, \
             max(ended_at) AS last_ended_at FROM m GROUP BY fingerprint), \
         top AS (SELECT * FROM fingerprints \
             ORDER BY runs DESC, last_ended_at DESC, fingerprint LIMIT ",
    )
    .push_bind(limit(window.limit))
    .push(
        "), latest AS (SELECT DISTINCT ON (m.fingerprint) m.fingerprint, m.run_id, m.status, \
             m.error, m.signature, m.message FROM m \
             WHERE m.fingerprint IN (SELECT fingerprint FROM top) \
             ORDER BY m.fingerprint, m.ended_at DESC, m.run_id DESC), \
         lines AS (SELECT DISTINCT ON (fingerprint) fingerprint, line FROM ( \
             SELECT m.fingerprint, m.line, count(*) AS runs FROM m \
             WHERE m.line IS NOT NULL AND m.fingerprint IN (SELECT fingerprint FROM top) \
             GROUP BY m.fingerprint, m.line) x \
             ORDER BY fingerprint, runs DESC, line), \
         labels AS (SELECT fingerprint, array_agg(label ORDER BY runs DESC, label) AS labels, \
             array_agg(runs ORDER BY runs DESC, label) AS runs FROM ( \
                 SELECT fingerprint, label, runs, row_number() OVER ( \
                     PARTITION BY fingerprint ORDER BY runs DESC, label) AS rank FROM ( \
                     SELECT m.fingerprint, (l.key || ':' || l.value) COLLATE \"C\" AS label, \
                         count(*) AS runs \
                     FROM m JOIN run_labels l ON l.run_id = m.run_id AND l.tenant = ",
    )
    .push_bind(tenant(&query.tenant).to_owned())
    .push(
        " WHERE m.fingerprint IN (SELECT fingerprint FROM top) \
                     GROUP BY m.fingerprint, l.key, l.value) x) y \
             WHERE rank <= ",
    )
    .push_bind(GROUP_LABELS)
    .push(" GROUP BY fingerprint)");
    if let Some(buckets) = buckets {
        sql.push(
            ", series AS (SELECT fingerprint, array_agg(bucket ORDER BY bucket) AS buckets, \
                 array_agg(runs ORDER BY bucket) AS runs FROM ( \
                     SELECT m.fingerprint, ",
        );
        buckets.push_index(&mut sql, "m.ended_at");
        sql.push(
            " AS bucket, count(*) AS runs FROM m \
                 WHERE m.fingerprint IN (SELECT fingerprint FROM top) GROUP BY 1, 2) x \
             GROUP BY fingerprint)",
        );
    } else {
        sql.push(
            ", series AS (SELECT NULL::text AS fingerprint, NULL::bigint[] AS buckets, \
                 NULL::bigint[] AS runs WHERE FALSE)",
        );
    }
    sql.push(
        " SELECT totals.runs, totals.fingerprints, top.fingerprint, top.runs, top.scripts, \
             top.first_ended_at, top.last_ended_at, latest.run_id, latest.status, latest.error, \
             latest.signature, latest.message, lines.line, series.buckets, series.runs, \
             labels.labels, labels.runs \
         FROM (SELECT count(*) AS runs, count(DISTINCT fingerprint) AS fingerprints FROM m) totals \
         LEFT JOIN top ON TRUE \
         LEFT JOIN latest ON latest.fingerprint = top.fingerprint \
         LEFT JOIN lines ON lines.fingerprint = top.fingerprint \
         LEFT JOIN series ON series.fingerprint = top.fingerprint \
         LEFT JOIN labels ON labels.fingerprint = top.fingerprint \
         ORDER BY top.runs DESC, top.last_ended_at DESC, top.fingerprint",
    );
    let rows = sql.build().persistent(false).fetch_all(pool).await?;

    let mut groups = FailureGroups {
        runs: 0,
        groups: 0,
        top: Vec::new(),
    };
    for row in &rows {
        groups.runs = row.u64(0)?;
        groups.groups = row.u64(1)?;
        let Some(fingerprint) = row.opt_text(2)? else {
            continue;
        };
        let mut series = vec![0; buckets.map_or(0, |buckets| buckets.count)];
        let indexes: Vec<i64> = array(row, 13)?;
        let counts: Vec<i64> = array(row, 14)?;
        for (index, runs) in indexes.into_iter().zip(counts) {
            if let Some(bucket) = usize::try_from(index).ok().and_then(|i| series.get_mut(i)) {
                *bucket = count(runs)?;
            }
        }
        let labels: Vec<String> = array(row, 15)?;
        let label_runs: Vec<i64> = array(row, 16)?;
        let labels = labels
            .into_iter()
            .zip(label_runs)
            .map(|(value, runs)| {
                Ok(FacetValue {
                    value,
                    runs: count(runs)?,
                })
            })
            .collect::<Result<_, Error>>()?;
        groups.top.push(FailureGroup {
            fingerprint,
            signature: row.text(10)?,
            error: row.text(9)?,
            runs: row.u64(3)?,
            series,
            scripts: row.u64(4)?,
            first_ended_at: row.ts(5)?,
            last_ended_at: row.ts(6)?,
            line: row.opt_u32(12)?,
            labels,
            latest: row.run_id(7)?,
            latest_status: parse_status(&row.text(8)?)?,
            latest_message: row.text(11)?,
        });
    }
    Ok(groups)
}

// -------------------------------------------------------------------------------------------
// scripts

/// The tenant's scripts, over the runs the filter counts (`r`): runs per status with the first
/// and last start, the tools called, the labels carried. A script whose runs did not call the
/// filter's tool is left out.
pub(super) async fn scripts(
    pool: &PgPool,
    filter: &ScriptFilter,
) -> Result<Vec<ScriptSummary>, Error> {
    let mut sql = Sql::new(
        "WITH counted AS MATERIALIZED (SELECT s.run_id, s.script, s.status, s.started_at \
         FROM run_summaries s WHERE s.tenant = ",
    );
    sql.push_bind(tenant(&filter.tenant).to_owned());
    if let Some(namespace) = &filter.namespace {
        sql.push(" AND s.namespace = ")
            .push_bind(namespace.as_str().to_owned());
    }
    if let Some(since) = filter.since {
        sql.push(" AND s.started_at >= ").push_bind(ts(since));
    }
    if let Some(digest) = filter.digest {
        sql.push(" AND s.script = ").push_bind(bytes(digest));
    }
    for (key, value) in &filter.labels {
        sql.push(" AND EXISTS (SELECT 1 FROM run_labels l WHERE l.run_id = s.run_id AND l.key = ")
            .push_bind(key.clone())
            .push(" AND l.value = ")
            .push_bind(value.clone())
            .push(")");
    }
    sql.push("), r AS (SELECT * FROM counted");
    if let Some(tool) = &filter.tool {
        sql.push(
            " WHERE script IN (SELECT counted.script FROM counted \
             JOIN tool_calls c ON c.run_id = counted.run_id WHERE c.tool = ",
        )
        .push_bind(tool.clone())
        .push(")");
    }
    sql.push(
        ") SELECT 'status', r.script, r.status, count(*), min(r.started_at), max(r.started_at) \
             FROM r GROUP BY r.script, r.status \
         UNION ALL SELECT DISTINCT 'tool', r.script, c.tool, NULL::bigint, NULL::bigint, \
             NULL::bigint FROM r JOIN tool_calls c ON c.run_id = r.run_id \
             WHERE c.tool IS NOT NULL \
         UNION ALL SELECT 'label', r.script, l.key || ':' || l.value, count(*), NULL, NULL \
             FROM r JOIN run_labels l ON l.run_id = r.run_id GROUP BY r.script, l.key, l.value",
    );
    let rows = sql.build().persistent(false).fetch_all(pool).await?;
    let mut scripts: BTreeMap<Digest, ScriptSummary> = BTreeMap::new();
    for row in &rows {
        let digest = row.digest(1)?;
        let script = scripts.entry(digest).or_insert_with(|| ScriptSummary {
            script: digest,
            runs: 0,
            statuses: BTreeMap::new(),
            tools: BTreeSet::new(),
            labels: BTreeMap::new(),
            first_started_at: None,
            last_started_at: None,
        });
        match row.text(0)?.as_str() {
            "status" => {
                let runs = row.u64(3)?;
                script.runs += runs;
                *script
                    .statuses
                    .entry(parse_status(&row.text(2)?)?)
                    .or_default() += runs;
                if let Some(first) = row.opt_ts(4)? {
                    let earliest = script.first_started_at.map_or(first, |at| at.min(first));
                    script.first_started_at = Some(earliest);
                }
                if let Some(last) = row.opt_ts(5)? {
                    let latest = script.last_started_at.map_or(last, |at| at.max(last));
                    script.last_started_at = Some(latest);
                }
            }
            "tool" => {
                script.tools.insert(row.text(2)?);
            }
            "label" => {
                *script.labels.entry(row.text(2)?).or_default() += row.u64(3)?;
            }
            other => return Err(Error::Corrupt(format!("a script's {other:?}"))),
        }
    }
    Ok(scripts.into_values().collect())
}

// -------------------------------------------------------------------------------------------
// tool_stats

/// The calls of the tenant's runs scheduled in the window (`calls`), per tool: one row of
/// counts and latency per tool, and one row per tool and kind of failure.
pub(super) async fn tool_stats(
    pool: &PgPool,
    filter: &ToolStatsFilter,
) -> Result<Vec<ToolStats>, Error> {
    let mut sql = Sql::new(
        "WITH calls AS MATERIALIZED (SELECT c.tool, c.outcome, c.error_kind, c.attempts, \
             CASE WHEN c.completed_at IS NOT NULL \
                 THEN GREATEST(c.completed_at - c.scheduled_at, 0) END AS ms \
         FROM tool_calls c JOIN run_summaries s ON s.run_id = c.run_id \
         WHERE c.tool IS NOT NULL AND c.scheduled_at IS NOT NULL AND c.tenant = ",
    );
    let tenant_id = tenant(&filter.tenant).to_owned();
    sql.push_bind(tenant_id.clone())
        .push(" AND s.tenant = ")
        .push_bind(tenant_id);
    if let Some(namespace) = &filter.namespace {
        sql.push(" AND s.namespace = ")
            .push_bind(namespace.as_str().to_owned());
    }
    if let Some(toolset) = filter.toolset {
        sql.push(" AND s.toolset = ").push_bind(bytes(toolset));
    }
    if let Some(since) = filter.since {
        sql.push(" AND c.scheduled_at >= ").push_bind(ts(since));
    }
    if let Some(until) = filter.until {
        sql.push(" AND c.scheduled_at < ").push_bind(ts(until));
    }
    sql.push(
        ") SELECT tool, NULL::text, count(*), count(*) FILTER (WHERE outcome = 'completed'), \
             count(*) FILTER (WHERE outcome = 'failed'), \
             COALESCE(sum(GREATEST(COALESCE(attempts, 0) - 1, 0)), 0)::bigint, ",
    )
    .push(LATENCY)
    .push(
        " FROM calls GROUP BY tool \
         UNION ALL SELECT tool, error_kind, count(*), NULL, NULL, NULL, \
             0, NULL, NULL, NULL, NULL, NULL \
         FROM calls WHERE outcome = 'failed' GROUP BY tool, error_kind",
    );
    let rows = sql.build().persistent(false).fetch_all(pool).await?;
    let mut stats: BTreeMap<String, ToolStats> = BTreeMap::new();
    for row in &rows {
        let tool = row.text(0)?;
        let entry = stats.entry(tool.clone()).or_insert_with(|| ToolStats {
            tool,
            calls: 0,
            completed: 0,
            failed: 0,
            errors_by_kind: BTreeMap::new(),
            retries: 0,
            latency: None,
        });
        match row.opt_text(1)? {
            Some(kind) => {
                let kind: ToolFailureKind = codec::parse_failure_kind(&kind)?;
                entry.errors_by_kind.insert(kind, row.u64(2)?);
            }
            None => {
                entry.calls = row.u64(2)?;
                entry.completed = row.u64(3)?;
                entry.failed = row.u64(4)?;
                entry.retries = row.u64(5)?;
                entry.latency = latency(row, 6)?;
            }
        }
    }
    Ok(stats.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::Buckets;
    use rewind_core::Timestamp;

    #[test]
    fn buckets_start_at_since_and_cover_until() {
        let buckets = Buckets::new(Some(Timestamp(1_000)), Some(Timestamp(3_500)), Some(1_000));
        assert_eq!(buckets.map(|buckets| buckets.count), Some(3));
        assert!(Buckets::new(None, Some(Timestamp(1)), Some(1)).is_none());
        assert!(Buckets::new(Some(Timestamp(0)), Some(Timestamp(1)), Some(0)).is_none());
        assert!(Buckets::new(Some(Timestamp(0)), None, Some(1)).is_none());
    }
}
