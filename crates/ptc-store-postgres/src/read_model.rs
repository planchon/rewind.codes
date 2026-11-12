//! `ReadModel` on Postgres: `run_summaries` and `tool_calls`, filled by the projector.
//!
//! Every write is keyed so that applying entries twice, or in any order, gives the same rows:
//! a call's columns are set by the entry that knows them (`schedule_tool_call`, its
//! completion), a summary keeps the maximum of what it has seen, and its counts are recounted
//! from `tool_calls` after each batch. Projectors may apply items of one run concurrently: the
//! summary's row, written first, serializes them.

use std::collections::{BTreeMap, BTreeSet};

use ptc_core::{
    Command, Completion, Digest, Event, HistoryEntry, Lifecycle, Payload, RunStatus, ScriptError,
    Shape, Timestamp, ToolFailureKind,
};
use ptc_engine::ports::{
    Latency, ProjectedRun, ReadModel, ReadModelError, RunRef, RunSearch, RunSummary, ScriptFilter,
    ScriptSummary, TenantId, TenantScope, ToolStats, ToolStatsFilter,
};
use sqlx::postgres::PgRow;

use crate::codec::{self, decode_labels, encode_labels};
use crate::sql::{
    Columns, Failure, bytes, int, limit, opt_ts, retrying, scope_clause, tenant, ts, uuid,
};
use crate::{Error, PostgresStore};

impl From<Error> for ReadModelError {
    fn from(err: Error) -> Self {
        ReadModelError::Backend(err.to_string())
    }
}

impl From<Failure<ReadModelError>> for ReadModelError {
    fn from(failure: Failure<ReadModelError>) -> Self {
        match failure {
            Failure::Port(err) => err,
            Failure::Db(err) => err.into(),
        }
    }
}

/// The status a terminal entry gives its run.
fn status_of(event: &Event) -> Option<RunStatus> {
    match event {
        Event::Lifecycle(Lifecycle::RunAborted(_)) => Some(RunStatus::Aborted),
        Event::Command(Command::CompleteRun { .. }) => Some(RunStatus::Completed),
        Event::Command(Command::FailRun { error }) if error.name == ScriptError::CANCELLED => {
            Some(RunStatus::Cancelled)
        }
        Event::Command(Command::FailRun { .. }) => Some(RunStatus::Failed),
        _ => None,
    }
}

/// The nearest-rank percentile `p` of sorted values.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    let n = sorted.len() as u64;
    let rank = (p * n).div_ceil(100).max(1);
    sorted[(rank - 1) as usize]
}

/// What one batch of entries says about the calls of a run, one row per call and per kind of
/// entry: the last entry of a kind wins, as if the entries were applied one at a time.
#[derive(Default)]
struct Calls {
    /// seq → (tool, callsite line, scheduled at, args digest).
    scheduled: BTreeMap<i64, (String, Option<i64>, i64, Vec<u8>)>,
    /// seq → (entry index, completed at, outcome).
    outcomes: BTreeMap<i64, (u64, i64, Outcome)>,
}

enum Outcome {
    Completed {
        digest: Vec<u8>,
        shape: Option<String>,
    },
    Failed {
        kind: &'static str,
    },
}

impl Calls {
    fn add(&mut self, entry: &HistoryEntry) {
        let at = ts(entry.recorded_at);
        let outcome = match &entry.event {
            Event::Command(Command::ScheduleToolCall {
                seq,
                tool,
                args,
                callsite,
            }) => {
                let line = callsite.map(|callsite| i64::from(callsite.line));
                let call = (tool.clone(), line, at, bytes(args.digest()));
                self.scheduled.insert(i64::from(seq.0), call);
                return;
            }
            Event::Completion(Completion::ToolCallCompleted { seq, result }) => {
                let shape = match result {
                    Payload::Value(value) => Some(codec::encode_shape(&Shape::of(value))),
                    Payload::Digest(_) => None,
                };
                let digest = bytes(result.digest());
                (seq, Outcome::Completed { digest, shape })
            }
            Event::Completion(Completion::ToolCallFailed { seq, error }) => (
                seq,
                Outcome::Failed {
                    kind: error.kind.as_str(),
                },
            ),
            _ => return,
        };
        let (seq, outcome) = outcome;
        let replace = self
            .outcomes
            .get(&i64::from(seq.0))
            .is_none_or(|(index, ..)| *index <= entry.index);
        if replace {
            self.outcomes
                .insert(i64::from(seq.0), (entry.index, at, outcome));
        }
    }
}

impl PostgresStore {
    async fn apply_once(
        &self,
        run: &ProjectedRun,
        entries: &[HistoryEntry],
    ) -> Result<(), Failure<ReadModelError>> {
        let mut tx = self.pool.begin().await?;
        let id = uuid(run.run.id);
        let tenant_id = tenant(&run.run.tenant);
        sqlx::query(
            "INSERT INTO run_summaries (run_id, tenant, script, toolset, status, labels, \
                 last_activity_at, tool_calls, errors, history_len) \
             VALUES ($1, $2, $3, $4, 'active', $5::jsonb, 0, 0, 0, 0) \
             ON CONFLICT (run_id) DO UPDATE SET tenant = EXCLUDED.tenant, \
                 script = EXCLUDED.script, toolset = EXCLUDED.toolset, labels = EXCLUDED.labels",
        )
        .bind(id)
        .bind(tenant_id)
        .bind(bytes(run.script))
        .bind(bytes(run.toolset))
        .bind(encode_labels(&run.labels))
        .execute(&mut *tx)
        .await?;

        let mut last_activity = Timestamp(0);
        let mut history_len = 0;
        let mut started_at = None;
        // (index, status, recorded_at) of the last terminal entry of the batch.
        let mut ended: Option<(u64, RunStatus, Timestamp)> = None;
        let mut calls = Calls::default();
        for entry in entries {
            last_activity = last_activity.max(entry.recorded_at);
            history_len = history_len.max(entry.index + 1);
            if let Event::Lifecycle(Lifecycle::RunStarted(_)) = &entry.event {
                started_at = Some(entry.recorded_at);
            }
            if let Some(status) = status_of(&entry.event)
                && ended.is_none_or(|(index, ..)| index < entry.index)
            {
                ended = Some((entry.index, status, entry.recorded_at));
            }
            calls.add(entry);
        }

        if !calls.scheduled.is_empty() {
            let seqs: Vec<i64> = calls.scheduled.keys().copied().collect();
            let tools: Vec<&str> = calls.scheduled.values().map(|c| c.0.as_str()).collect();
            let lines: Vec<Option<i64>> = calls.scheduled.values().map(|c| c.1).collect();
            let at: Vec<i64> = calls.scheduled.values().map(|c| c.2).collect();
            let args: Vec<&[u8]> = calls.scheduled.values().map(|c| c.3.as_slice()).collect();
            sqlx::query(
                "INSERT INTO tool_calls (tenant, run_id, seq, tool, callsite_line, scheduled_at, \
                     args_digest) \
                 SELECT $1, $2, seq, tool, line, at, args \
                 FROM unnest($3::bigint[], $4::text[], $5::bigint[], $6::bigint[], $7::bytea[]) \
                     AS c(seq, tool, line, at, args) \
                 ON CONFLICT (run_id, seq) DO UPDATE SET tool = EXCLUDED.tool, \
                     callsite_line = EXCLUDED.callsite_line, \
                     scheduled_at = EXCLUDED.scheduled_at, args_digest = EXCLUDED.args_digest",
            )
            .bind(tenant_id)
            .bind(id)
            .bind(seqs)
            .bind(tools)
            .bind(lines)
            .bind(at)
            .bind(args)
            .execute(&mut *tx)
            .await?;
        }
        let mut completed = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut failed = (Vec::new(), Vec::new(), Vec::new());
        for (seq, (_, at, outcome)) in &calls.outcomes {
            match outcome {
                Outcome::Completed { digest, shape } => {
                    completed.0.push(*seq);
                    completed.1.push(*at);
                    completed.2.push(digest.as_slice());
                    completed.3.push(shape.as_deref());
                }
                Outcome::Failed { kind } => {
                    failed.0.push(*seq);
                    failed.1.push(*at);
                    failed.2.push(*kind);
                }
            }
        }
        if !completed.0.is_empty() {
            sqlx::query(
                "INSERT INTO tool_calls (tenant, run_id, seq, completed_at, outcome, \
                     result_digest, result_shape) \
                 SELECT $1, $2, seq, at, 'completed', digest, shape \
                 FROM unnest($3::bigint[], $4::bigint[], $5::bytea[], $6::text[]) \
                     AS c(seq, at, digest, shape) \
                 ON CONFLICT (run_id, seq) DO UPDATE SET completed_at = EXCLUDED.completed_at, \
                     outcome = 'completed', error_kind = NULL, \
                     result_digest = EXCLUDED.result_digest, \
                     result_shape = EXCLUDED.result_shape",
            )
            .bind(tenant_id)
            .bind(id)
            .bind(completed.0)
            .bind(completed.1)
            .bind(completed.2)
            .bind(completed.3)
            .execute(&mut *tx)
            .await?;
        }
        if !failed.0.is_empty() {
            sqlx::query(
                "INSERT INTO tool_calls (tenant, run_id, seq, completed_at, outcome, error_kind) \
                 SELECT $1, $2, seq, at, 'failed', kind \
                 FROM unnest($3::bigint[], $4::bigint[], $5::text[]) AS c(seq, at, kind) \
                 ON CONFLICT (run_id, seq) DO UPDATE SET completed_at = EXCLUDED.completed_at, \
                     outcome = 'failed', error_kind = EXCLUDED.error_kind, \
                     result_digest = NULL, result_shape = NULL",
            )
            .bind(tenant_id)
            .bind(id)
            .bind(failed.0)
            .bind(failed.1)
            .bind(failed.2)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            "UPDATE run_summaries SET last_activity_at = GREATEST(last_activity_at, $2), \
                 history_len = GREATEST(history_len, $3), \
                 started_at = COALESCE($4, started_at), status = COALESCE($5, status), \
                 ended_at = COALESCE($6, ended_at), \
                 tool_calls = (SELECT count(*) FROM tool_calls \
                               WHERE run_id = $1 AND scheduled_at IS NOT NULL), \
                 errors = (SELECT count(*) FROM tool_calls \
                           WHERE run_id = $1 AND outcome = 'failed') \
             WHERE run_id = $1",
        )
        .bind(id)
        .bind(ts(last_activity))
        .bind(int(history_len))
        .bind(opt_ts(started_at))
        .bind(ended.map(|(_, status, _)| status.as_str()))
        .bind(opt_ts(ended.map(|(.., at)| at)))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

fn summary_from_row(row: &PgRow) -> Result<RunSummary, Error> {
    Ok(RunSummary {
        run: RunRef::new(row.tenant(1)?, row.run_id(0)?),
        script: row.digest(2)?,
        toolset: row.digest(3)?,
        status: codec::parse_status(&row.text(4)?)?,
        labels: decode_labels(&row.text(5)?)?,
        started_at: row.opt_ts(6)?,
        ended_at: row.opt_ts(7)?,
        last_activity_at: row.ts(8)?,
        tool_calls: row.u64(9)?,
        errors: row.u64(10)?,
        history_len: row.u64(11)?,
    })
}

impl ReadModel for PostgresStore {
    async fn apply(
        &self,
        run: &ProjectedRun,
        entries: &[HistoryEntry],
    ) -> Result<(), ReadModelError> {
        Ok(retrying(|| self.apply_once(run, entries)).await?)
    }

    async fn search_runs(&self, search: &RunSearch) -> Result<Vec<RunSummary>, ReadModelError> {
        let rows = sqlx::query(
            "SELECT run_id, tenant, script, toolset, status, labels::text, started_at, ended_at, \
                 last_activity_at, tool_calls, errors, history_len \
             FROM run_summaries s WHERE tenant = $1 \
             AND ($2::uuid IS NULL OR run_id < $2) \
             AND ($3::bytea IS NULL OR script = $3) \
             AND ($4::bytea IS NULL OR toolset = $4) \
             AND ($5::text IS NULL OR status = $5) \
             AND ($6::bigint IS NULL OR started_at >= $6) \
             AND ($7::bigint IS NULL OR started_at < $7) \
             AND ($8::text IS NULL OR EXISTS ( \
                 SELECT 1 FROM tool_calls c WHERE c.run_id = s.run_id AND c.tool = $8)) \
             AND ($9::text IS NULL OR EXISTS ( \
                 SELECT 1 FROM tool_calls c WHERE c.run_id = s.run_id AND c.error_kind = $9)) \
             AND labels @> $10::jsonb \
             ORDER BY run_id DESC LIMIT $11",
        )
        .persistent(false)
        .bind(tenant(&search.tenant))
        .bind(search.before.map(uuid))
        .bind(search.script.map(bytes))
        .bind(search.toolset.map(bytes))
        .bind(search.status.map(|status| status.as_str()))
        .bind(search.started_after.map(ts))
        .bind(search.started_before.map(ts))
        .bind(search.tool.as_deref())
        .bind(search.error_kind.map(|kind| kind.as_str()))
        .bind(encode_labels(&search.labels))
        .bind(limit(search.limit))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(summary_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn scripts(&self, filter: &ScriptFilter) -> Result<Vec<ScriptSummary>, ReadModelError> {
        let since = filter.since.map(ts);
        let runs = sqlx::query(
            "SELECT script, status, started_at FROM run_summaries \
             WHERE tenant = $1 AND ($2::bigint IS NULL OR started_at >= $2)",
        )
        .bind(tenant(&filter.tenant))
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        let mut scripts: BTreeMap<Digest, ScriptSummary> = BTreeMap::new();
        for row in &runs {
            let script = row.digest(0)?;
            let summary = scripts.entry(script).or_insert_with(|| ScriptSummary {
                script,
                runs: 0,
                statuses: BTreeMap::new(),
                tools: BTreeSet::new(),
                first_started_at: None,
                last_started_at: None,
            });
            summary.runs += 1;
            *summary
                .statuses
                .entry(codec::parse_status(&row.text(1)?)?)
                .or_default() += 1;
            if let Some(at) = row.opt_ts(2)? {
                summary.first_started_at = Some(summary.first_started_at.map_or(at, |t| t.min(at)));
                summary.last_started_at = Some(summary.last_started_at.map_or(at, |t| t.max(at)));
            }
        }
        let tools = sqlx::query(
            "SELECT DISTINCT s.script, c.tool FROM tool_calls c \
             JOIN run_summaries s ON s.run_id = c.run_id \
             WHERE s.tenant = $1 AND ($2::bigint IS NULL OR s.started_at >= $2) \
             AND c.tool IS NOT NULL",
        )
        .bind(tenant(&filter.tenant))
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        for row in &tools {
            if let Some(summary) = scripts.get_mut(&row.digest(0)?) {
                summary.tools.insert(row.text(1)?);
            }
        }
        Ok(scripts.into_values().collect())
    }

    async fn tool_stats(&self, filter: &ToolStatsFilter) -> Result<Vec<ToolStats>, ReadModelError> {
        let rows = sqlx::query(
            "SELECT c.tool, c.scheduled_at, c.completed_at, c.outcome, c.error_kind \
             FROM tool_calls c JOIN run_summaries s ON s.run_id = c.run_id \
             WHERE c.tenant = $1 AND c.tool IS NOT NULL AND c.scheduled_at IS NOT NULL \
             AND ($2::bytea IS NULL OR s.toolset = $2) \
             AND ($3::bigint IS NULL OR c.scheduled_at >= $3) \
             AND ($4::bigint IS NULL OR c.scheduled_at < $4)",
        )
        .persistent(false)
        .bind(tenant(&filter.tenant))
        .bind(filter.toolset.map(bytes))
        .bind(filter.since.map(ts))
        .bind(filter.until.map(ts))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        let mut stats: BTreeMap<String, (ToolStats, Vec<u64>)> = BTreeMap::new();
        for row in &rows {
            let tool = row.text(0)?;
            let scheduled_at = row.ts(1)?;
            let (entry, latencies) = stats.entry(tool.clone()).or_insert_with(|| {
                let stats = ToolStats {
                    tool,
                    calls: 0,
                    completed: 0,
                    failed: 0,
                    errors_by_kind: BTreeMap::new(),
                    latency: None,
                };
                (stats, Vec::new())
            });
            entry.calls += 1;
            match row.opt_text(3)?.as_deref() {
                Some("completed") => entry.completed += 1,
                Some("failed") => {
                    let kind: ToolFailureKind = codec::parse_failure_kind(&row.text(4)?)?;
                    entry.failed += 1;
                    *entry.errors_by_kind.entry(kind).or_default() += 1;
                }
                Some(other) => {
                    return Err(Error::Corrupt(format!("unknown call outcome {other:?}")).into());
                }
                None => {}
            }
            if let Some(completed_at) = row.opt_ts(2)? {
                latencies.push(completed_at.0.saturating_sub(scheduled_at.0));
            }
        }
        Ok(stats
            .into_values()
            .map(|(mut entry, mut latencies)| {
                latencies.sort_unstable();
                if !latencies.is_empty() {
                    let count = latencies.len() as u64;
                    entry.latency = Some(Latency {
                        count,
                        mean_ms: latencies.iter().sum::<u64>() / count,
                        p50_ms: percentile(&latencies, 50),
                        p95_ms: percentile(&latencies, 95),
                        max_ms: latencies[latencies.len() - 1],
                    });
                }
                entry
            })
            .collect())
    }

    async fn observed_shapes(
        &self,
        tenant_id: &TenantId,
        tools: &[String],
    ) -> Result<BTreeMap<String, Shape>, ReadModelError> {
        if tools.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows = sqlx::query(
            "SELECT tool, result_shape FROM tool_calls \
             WHERE tenant = $1 AND tool = ANY($2) AND result_shape IS NOT NULL",
        )
        .bind(tenant(tenant_id))
        .bind(tools)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        // Merging is commutative, associative and idempotent: the order of the rows is free.
        let mut shapes: BTreeMap<String, Shape> = BTreeMap::new();
        for row in &rows {
            let tool = row.text(0)?;
            let shape = codec::decode_shape(&row.text(1)?)?;
            let merged = shapes.remove(&tool).unwrap_or(Shape::Unknown).merge(shape);
            shapes.insert(tool, merged);
        }
        Ok(shapes)
    }

    async fn purge(
        &self,
        ended_before: Timestamp,
        tenants: &TenantScope,
    ) -> Result<usize, ReadModelError> {
        let (scope, scope_tenants) = scope_clause("tenant", tenants, 2);
        let statement = format!(
            "WITH purged AS ( \
                 DELETE FROM run_summaries \
                 WHERE ended_at IS NOT NULL AND ended_at < $1{scope} RETURNING run_id), \
             calls AS (DELETE FROM tool_calls WHERE run_id IN (SELECT run_id FROM purged)) \
             SELECT count(*) FROM purged"
        );
        let mut query = sqlx::query_scalar::<_, i64>(&statement).bind(ts(ended_before));
        if let Some(tenants) = scope_tenants {
            query = query.bind(tenants);
        }
        let purged = query.fetch_one(&self.pool).await.map_err(Error::from)?;
        Ok(usize::try_from(purged).unwrap_or(usize::MAX))
    }

    async fn delete_run(&self, run: &RunRef) -> Result<bool, ReadModelError> {
        let deleted: i64 = sqlx::query_scalar(
            "WITH summary AS ( \
                 DELETE FROM run_summaries WHERE run_id = $1 AND tenant = $2 RETURNING run_id), \
             calls AS (DELETE FROM tool_calls WHERE run_id IN (SELECT run_id FROM summary)) \
             SELECT count(*) FROM summary",
        )
        .bind(uuid(run.id))
        .bind(tenant(&run.tenant))
        .fetch_one(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(deleted > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentiles_are_nearest_rank() {
        let values = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&values, 50), 50);
        assert_eq!(percentile(&values, 95), 100);
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[1, 2, 3], 50), 2);
    }
}
