//! `ReadModel` on Turso: `run_summaries` and `tool_calls`, filled by the projector.
//!
//! Every write is keyed so that applying entries twice, or in any order, gives the same rows:
//! a call's columns are set by the entry that knows them (`schedule_tool_call`, its
//! completion), a summary keeps the maximum of what it has seen, and its counts are recounted
//! from `tool_calls` after each batch.

use std::collections::{BTreeMap, BTreeSet};

use ptc_core::{
    Command, Completion, Digest, Event, HistoryEntry, Lifecycle, Payload, RunStatus, ScriptError,
    Seq, Shape, Timestamp, ToolFailureKind,
};
use ptc_engine::ports::{
    Latency, ProjectedRun, ReadModel, ReadModelError, RunRef, RunSearch, RunSummary, ScriptFilter,
    ScriptSummary, TenantId, TenantScope, ToolStats, ToolStatsFilter,
};
use turso::{Connection, Value};

use crate::codec::{self, decode_labels, encode_labels};
use crate::sql::{
    self, Columns, digest, int, limit, opt_text, opt_ts, placeholders, run_id, scope_clause,
    tenant, text, transaction, ts,
};
use crate::{Error, TursoStore};

impl From<Error> for ReadModelError {
    fn from(err: Error) -> Self {
        ReadModelError::Backend(err.to_string())
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

/// Projects one entry onto `tool_calls`.
async fn project_call(
    connection: &Connection,
    run: &RunRef,
    entry: &HistoryEntry,
) -> Result<(), Error> {
    let at = ts(entry.recorded_at);
    match &entry.event {
        Event::Command(Command::ScheduleToolCall {
            seq,
            tool,
            args,
            callsite,
        }) => {
            sql::execute(
                connection,
                "INSERT INTO tool_calls (run_id, seq, tool, callsite_line, scheduled_at, \
                 args_digest) VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT (run_id, seq) DO UPDATE SET tool = excluded.tool, \
                 callsite_line = excluded.callsite_line, scheduled_at = excluded.scheduled_at, \
                 args_digest = excluded.args_digest",
                vec![
                    run_id(run.id),
                    int(u64::from(seq.0)),
                    text(tool.as_str()),
                    callsite.map_or(Value::Null, |callsite| int(u64::from(callsite.line))),
                    at,
                    digest(args.digest()),
                ],
            )
            .await?;
        }
        Event::Completion(Completion::ToolCallCompleted { seq, result }) => {
            let shape = match result {
                Payload::Value(value) => Some(codec::encode_shape(&Shape::of(value))),
                Payload::Digest(_) => None,
            };
            sql::execute(
                connection,
                "INSERT INTO tool_calls (run_id, seq, completed_at, outcome, result_digest, \
                 result_shape) VALUES (?, ?, ?, 'completed', ?, ?) \
                 ON CONFLICT (run_id, seq) DO UPDATE SET completed_at = excluded.completed_at, \
                 outcome = 'completed', error_kind = NULL, \
                 result_digest = excluded.result_digest, result_shape = excluded.result_shape",
                vec![
                    run_id(run.id),
                    int(u64::from(seq.0)),
                    at,
                    digest(result.digest()),
                    opt_text(shape),
                ],
            )
            .await?;
        }
        Event::Completion(Completion::ToolCallFailed { seq, error }) => {
            sql::execute(
                connection,
                "INSERT INTO tool_calls (run_id, seq, completed_at, outcome, error_kind) \
                 VALUES (?, ?, ?, 'failed', ?) \
                 ON CONFLICT (run_id, seq) DO UPDATE SET completed_at = excluded.completed_at, \
                 outcome = 'failed', error_kind = excluded.error_kind, result_digest = NULL, \
                 result_shape = NULL",
                vec![
                    run_id(run.id),
                    int(u64::from(seq.0)),
                    at,
                    text(error.kind.as_str()),
                ],
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

impl ReadModel for TursoStore {
    async fn apply(
        &self,
        run: &ProjectedRun,
        entries: &[HistoryEntry],
    ) -> Result<(), ReadModelError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            sql::execute(
                connection,
                "INSERT INTO run_summaries (run_id, tenant, script, toolset, status, labels, \
                 last_activity_at, tool_calls, errors, history_len) \
                 VALUES (?, ?, ?, ?, 'active', ?, 0, 0, 0, 0) \
                 ON CONFLICT (run_id) DO UPDATE SET tenant = excluded.tenant, \
                 script = excluded.script, toolset = excluded.toolset, labels = excluded.labels",
                vec![
                    run_id(run.run.id),
                    tenant(&run.run.tenant),
                    digest(run.script),
                    digest(run.toolset),
                    text(encode_labels(&run.labels)),
                ],
            )
            .await?;

            let mut last_activity = Timestamp(0);
            let mut history_len = 0;
            let mut started_at = None;
            // (index, status, recorded_at) of the last terminal entry of the batch.
            let mut ended: Option<(u64, RunStatus, Timestamp)> = None;
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
                project_call(connection, &run.run, entry).await?;
            }
            let id = run_id(run.run.id);
            sql::execute(
                connection,
                "UPDATE run_summaries SET last_activity_at = MAX(last_activity_at, ?), \
                 history_len = MAX(history_len, ?), started_at = COALESCE(?, started_at), \
                 status = COALESCE(?, status), ended_at = COALESCE(?, ended_at), \
                 tool_calls = (SELECT COUNT(*) FROM tool_calls \
                               WHERE run_id = ? AND scheduled_at IS NOT NULL), \
                 errors = (SELECT COUNT(*) FROM tool_calls WHERE run_id = ? AND outcome = 'failed') \
                 WHERE run_id = ?",
                vec![
                    ts(last_activity),
                    int(history_len),
                    opt_ts(started_at),
                    opt_text(ended.map(|(_, status, _)| status.as_str())),
                    opt_ts(ended.map(|(.., at)| at)),
                    id.clone(),
                    id.clone(),
                    id,
                ],
            )
            .await?;
            Ok(())
        })
        .await
    }

    async fn search_runs(&self, search: &RunSearch) -> Result<Vec<RunSummary>, ReadModelError> {
        let connection = self.connection().await?;
        let mut query = "SELECT run_id, tenant, script, toolset, status, labels, started_at, \
                         ended_at, last_activity_at, tool_calls, errors, history_len \
                         FROM run_summaries s WHERE tenant = ?"
            .to_owned();
        let mut params = vec![tenant(&search.tenant)];
        if let Some(before) = search.before {
            query.push_str(" AND run_id < ?");
            params.push(run_id(before));
        }
        if let Some(script) = search.script {
            query.push_str(" AND script = ?");
            params.push(digest(script));
        }
        if let Some(toolset) = search.toolset {
            query.push_str(" AND toolset = ?");
            params.push(digest(toolset));
        }
        if let Some(status) = search.status {
            query.push_str(" AND status = ?");
            params.push(text(status.as_str()));
        }
        if let Some(after) = search.started_after {
            query.push_str(" AND started_at >= ?");
            params.push(ts(after));
        }
        if let Some(before) = search.started_before {
            query.push_str(" AND started_at < ?");
            params.push(ts(before));
        }
        if let Some(tool) = &search.tool {
            query.push_str(
                " AND EXISTS (SELECT 1 FROM tool_calls c WHERE c.run_id = s.run_id AND c.tool = ?)",
            );
            params.push(text(tool.as_str()));
        }
        if let Some(kind) = search.error_kind {
            query.push_str(
                " AND EXISTS (SELECT 1 FROM tool_calls c \
                 WHERE c.run_id = s.run_id AND c.error_kind = ?)",
            );
            params.push(text(kind.as_str()));
        }
        query.push_str(" ORDER BY run_id DESC");
        // Labels are filtered here, so the limit can only be applied in SQL without them.
        if search.labels.is_empty() {
            query.push_str(" LIMIT ?");
            params.push(limit(search.limit));
        }
        let rows = sql::query_dynamic(&connection, &query, params).await?;
        let mut found = Vec::new();
        for row in &rows {
            if found.len() >= search.limit {
                break;
            }
            let labels = decode_labels(&row.text(5)?)?;
            if !search
                .labels
                .iter()
                .all(|(key, value)| labels.get(key) == Some(value))
            {
                continue;
            }
            found.push(RunSummary {
                run: RunRef::new(row.tenant(1)?, row.run_id(0)?),
                script: row.digest(2)?,
                toolset: row.digest(3)?,
                status: codec::parse_status(&row.text(4)?)?,
                labels,
                started_at: row.opt_ts(6)?,
                ended_at: row.opt_ts(7)?,
                last_activity_at: row.ts(8)?,
                tool_calls: row.u64(9)?,
                errors: row.u64(10)?,
                history_len: row.u64(11)?,
            });
        }
        Ok(found)
    }

    async fn scripts(&self, filter: &ScriptFilter) -> Result<Vec<ScriptSummary>, ReadModelError> {
        let connection = self.connection().await?;
        let mut clauses = String::new();
        let mut params = vec![tenant(&filter.tenant)];
        if let Some(since) = filter.since {
            clauses.push_str(" AND s.started_at >= ?");
            params.push(ts(since));
        }
        if let Some(script) = filter.digest {
            clauses.push_str(" AND s.script = ?");
            params.push(digest(script));
        }
        let runs = sql::query_dynamic(
            &connection,
            &format!(
                "SELECT s.script, s.status, s.started_at, s.labels, s.run_id                  FROM run_summaries s WHERE s.tenant = ?{clauses}"
            ),
            params.clone(),
        )
        .await?;
        let mut scripts: BTreeMap<Digest, ScriptSummary> = BTreeMap::new();
        let mut counted = BTreeSet::new();
        for row in &runs {
            let labels = decode_labels(&row.text(3)?)?;
            if !filter
                .labels
                .iter()
                .all(|(key, value)| labels.get(key) == Some(value))
            {
                continue;
            }
            counted.insert(row.text(4)?);
            let script = row.digest(0)?;
            let summary = scripts.entry(script).or_insert_with(|| ScriptSummary {
                script,
                runs: 0,
                statuses: BTreeMap::new(),
                tools: BTreeSet::new(),
                labels: BTreeMap::new(),
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
            for (key, value) in labels {
                *summary.labels.entry(format!("{key}:{value}")).or_default() += 1;
            }
        }
        let tools = sql::query_dynamic(
            &connection,
            &format!(
                "SELECT DISTINCT s.script, c.tool, s.run_id FROM tool_calls c \
                 JOIN run_summaries s ON s.run_id = c.run_id \
                 WHERE s.tenant = ?{clauses} AND c.tool IS NOT NULL"
            ),
            params,
        )
        .await?;
        for row in &tools {
            if !counted.contains(&row.text(2)?) {
                continue;
            }
            if let Some(summary) = scripts.get_mut(&row.digest(0)?) {
                summary.tools.insert(row.text(1)?);
            }
        }
        Ok(scripts
            .into_values()
            .filter(|script| {
                filter
                    .tool
                    .as_ref()
                    .is_none_or(|tool| script.tools.contains(tool))
            })
            .collect())
    }

    async fn tool_stats(&self, filter: &ToolStatsFilter) -> Result<Vec<ToolStats>, ReadModelError> {
        let connection = self.connection().await?;
        let mut query = "SELECT c.tool, c.scheduled_at, c.completed_at, c.outcome, c.error_kind, \
                         c.attempts FROM tool_calls c JOIN run_summaries s ON s.run_id = c.run_id \
                         WHERE s.tenant = ? AND c.tool IS NOT NULL AND c.scheduled_at IS NOT NULL"
            .to_owned();
        let mut params = vec![tenant(&filter.tenant)];
        if let Some(toolset) = filter.toolset {
            query.push_str(" AND s.toolset = ?");
            params.push(digest(toolset));
        }
        if let Some(since) = filter.since {
            query.push_str(" AND c.scheduled_at >= ?");
            params.push(ts(since));
        }
        if let Some(until) = filter.until {
            query.push_str(" AND c.scheduled_at < ?");
            params.push(ts(until));
        }
        let rows = sql::query_dynamic(&connection, &query, params).await?;
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
                    retries: 0,
                    latency: None,
                };
                (stats, Vec::new())
            });
            entry.calls += 1;
            if let Some(attempts) = row.opt_u64(5)? {
                entry.retries += attempts.saturating_sub(1);
            }
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
                        p99_ms: percentile(&latencies, 99),
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
        let connection = self.connection().await?;
        let mut params = vec![tenant(tenant_id)];
        params.extend(tools.iter().map(|tool| text(tool.as_str())));
        let rows = sql::query_dynamic(
            &connection,
            &format!(
                "SELECT c.tool, c.result_shape FROM tool_calls c \
                 JOIN run_summaries s ON s.run_id = c.run_id \
                 WHERE s.tenant = ? AND c.result_shape IS NOT NULL AND c.tool IN ({})",
                placeholders(tools.len())
            ),
            params,
        )
        .await?;
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
        let connection = self.connection().await?;
        let connection = &*connection;
        let mut scope_params = Vec::new();
        let scope = scope_clause("tenant", tenants, &mut scope_params);
        let mut params = vec![ts(ended_before)];
        params.extend(scope_params);
        let ended = format!("ended_at IS NOT NULL AND ended_at < ? AND {scope}");
        transaction(connection, async {
            sql::execute_dynamic(
                connection,
                &format!(
                    "DELETE FROM tool_calls WHERE run_id IN \
                     (SELECT run_id FROM run_summaries WHERE {ended})"
                ),
                params.clone(),
            )
            .await?;
            let purged = sql::execute_dynamic(
                connection,
                &format!("DELETE FROM run_summaries WHERE {ended}"),
                params,
            )
            .await?;
            Ok(usize::try_from(purged).unwrap_or(usize::MAX))
        })
        .await
    }

    async fn record_attempt(
        &self,
        run: &RunRef,
        seq: Seq,
        attempt: u32,
    ) -> Result<(), ReadModelError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let owner = sql::query_row(
                connection,
                "SELECT tenant FROM run_summaries WHERE run_id = ?",
                vec![run_id(run.id)],
            )
            .await?;
            if let Some(owner) = owner
                && owner.text(0)? != run.tenant.as_str()
            {
                return Ok(());
            }
            sql::execute(
                connection,
                "INSERT INTO tool_calls (run_id, seq, attempts) VALUES (?, ?, ?) \
                 ON CONFLICT (run_id, seq) DO UPDATE SET \
                 attempts = MAX(COALESCE(attempts, 0), excluded.attempts)",
                vec![
                    run_id(run.id),
                    int(u64::from(seq.0)),
                    int(u64::from(attempt)),
                ],
            )
            .await?;
            Ok(())
        })
        .await
    }

    async fn attempts(&self, run: &RunRef) -> Result<BTreeMap<Seq, u32>, ReadModelError> {
        let connection = self.connection().await?;
        let rows = sql::query_dynamic(
            &connection,
            "SELECT c.seq, c.attempts FROM tool_calls c \
             JOIN run_summaries s ON s.run_id = c.run_id \
             WHERE c.run_id = ? AND s.tenant = ? AND c.attempts IS NOT NULL",
            vec![run_id(run.id), tenant(&run.tenant)],
        )
        .await?;
        let mut attempts = BTreeMap::new();
        for row in &rows {
            let seq = u32::try_from(row.u64(0)?)
                .map_err(|_| Error::Corrupt("a call's seq is out of range".into()))?;
            let count = u32::try_from(row.u64(1)?)
                .map_err(|_| Error::Corrupt("a call's attempts are out of range".into()))?;
            attempts.insert(Seq(seq), count);
        }
        Ok(attempts)
    }

    async fn delete_run(&self, run: &RunRef) -> Result<bool, ReadModelError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let projected = sql::query_row(
                connection,
                "SELECT 1 FROM run_summaries WHERE run_id = ? AND tenant = ?",
                vec![run_id(run.id), tenant(&run.tenant)],
            )
            .await?;
            if projected.is_none() {
                return Ok(false);
            }
            for statement in [
                "DELETE FROM tool_calls WHERE run_id = ?",
                "DELETE FROM run_summaries WHERE run_id = ?",
            ] {
                sql::execute(connection, statement, vec![run_id(run.id)]).await?;
            }
            Ok(true)
        })
        .await
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
