//! `ReadModel` on Postgres (`archi/05-testing-and-observability.md`, "Read model"): the tables
//! `run_summaries`, `run_labels`, `run_waits`, `run_search`, `run_quarantine` and `tool_calls`.
//!
//! - [`apply`] projects a batch of entries with its snapshot, under the summary's row lock;
//! - [`query`] turns the typed query into SQL and reads pages of runs in the three orders;
//! - [`aggregates`] counts: statistics, failure groups, scripts, tool statistics.
//!
//! Task states and quarantine are service state, recorded here by whoever changes them, before
//! or after the run is projected: one conditional upsert each, so that the record of the highest
//! rank (a task) or of the latest time (a quarantine) is kept, whatever their order. A record for
//! a run projected under another tenant is ignored. Retention removes a run's rows from every
//! table, its records included.

mod aggregates;
mod apply;
mod query;

use std::collections::BTreeMap;

use ptc_core::{HistoryEntry, Seq, Shape, Timestamp};
use ptc_engine::ports::{
    FailureGroups, FailureWindow, ProjectedRun, QuarantineNote, ReadModel, ReadModelError,
    RunOrder, RunQuery, RunRef, RunSnapshot, RunStats, RunSummary, ScriptFilter, ScriptSummary,
    SearchCursor, StatsWindow, TaskPhase, TaskProgress, TenantId, TenantScope, ToolStats,
    ToolStatsFilter,
};

use crate::codec;
use crate::sql::{Columns, Failure, retrying, scope_clause, tenant, ts, uuid};
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

/// The step of a task's state within an attempt, in SQL: `TaskProgress::rank` is
/// `(attempt, step)`.
fn task_step(state: &str) -> String {
    let step = |phase| {
        TaskProgress {
            seq: Seq(0),
            attempt: 0,
            phase,
        }
        .rank()
        .1
    };
    format!(
        "CASE {state} WHEN 'approval' THEN {} WHEN 'claimed' THEN {} WHEN 'parked' THEN {} \
         WHEN 'deferred' THEN {} WHEN 'ready' THEN {} END",
        step(TaskPhase::Approval),
        step(TaskPhase::Claimed),
        step(TaskPhase::Parked),
        step(TaskPhase::Deferred),
        step(TaskPhase::Ready),
    )
}

/// The tasks of a batch of records, one per call: of the records of a call, the first of the
/// highest rank, as if they were recorded one at a time.
fn furthest(tasks: &[TaskProgress]) -> BTreeMap<Seq, TaskProgress> {
    let mut kept: BTreeMap<Seq, TaskProgress> = BTreeMap::new();
    for task in tasks {
        let entry = kept.entry(task.seq).or_insert(*task);
        if task.rank() > entry.rank() {
            *entry = *task;
        }
    }
    kept
}

impl ReadModel for PostgresStore {
    async fn apply(
        &self,
        run: &ProjectedRun,
        entries: &[HistoryEntry],
        snapshot: &RunSnapshot,
    ) -> Result<(), ReadModelError> {
        let attempt = || async {
            apply::apply_once(&self.pool, run, entries, snapshot)
                .await
                .map_err(Failure::<ReadModelError>::Db)
        };
        Ok(retrying(attempt).await?)
    }

    async fn search_runs(
        &self,
        query: &RunQuery,
        order: RunOrder,
        after: Option<&SearchCursor>,
        limit: usize,
    ) -> Result<Vec<RunSummary>, ReadModelError> {
        Ok(query::search_runs(&self.pool, query, order, after, limit).await?)
    }

    async fn run_stats(
        &self,
        query: &RunQuery,
        window: &StatsWindow,
    ) -> Result<RunStats, ReadModelError> {
        Ok(aggregates::run_stats(&self.pool, query, window).await?)
    }

    async fn failure_groups(
        &self,
        query: &RunQuery,
        window: &FailureWindow,
    ) -> Result<FailureGroups, ReadModelError> {
        Ok(aggregates::failure_groups(&self.pool, query, window).await?)
    }

    async fn scripts(&self, filter: &ScriptFilter) -> Result<Vec<ScriptSummary>, ReadModelError> {
        Ok(aggregates::scripts(&self.pool, filter).await?)
    }

    async fn tool_stats(&self, filter: &ToolStatsFilter) -> Result<Vec<ToolStats>, ReadModelError> {
        Ok(aggregates::tool_stats(&self.pool, filter).await?)
    }

    async fn observed_shapes(
        &self,
        tenant_id: &TenantId,
        tools: &[String],
    ) -> Result<BTreeMap<String, Shape>, ReadModelError> {
        if tools.is_empty() {
            return Ok(BTreeMap::new());
        }
        // Merging is commutative, associative and idempotent: each distinct shape once, in any
        // order.
        let rows = sqlx::query(
            "SELECT DISTINCT tool, result_shape FROM tool_calls \
             WHERE tenant = $1 AND tool = ANY($2) AND result_shape IS NOT NULL",
        )
        .bind(tenant(tenant_id))
        .bind(tools)
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        let mut shapes: BTreeMap<String, Shape> = BTreeMap::new();
        for row in &rows {
            let tool = row.text(0)?;
            let shape = codec::decode_shape(&row.text(1)?)?;
            let merged = shapes.remove(&tool).unwrap_or(Shape::Unknown).merge(shape);
            shapes.insert(tool, merged);
        }
        Ok(shapes)
    }

    async fn record_tasks(
        &self,
        run: &RunRef,
        tasks: &[TaskProgress],
    ) -> Result<(), ReadModelError> {
        let tasks = furthest(tasks);
        if tasks.is_empty() {
            return Ok(());
        }
        let seqs: Vec<i64> = tasks.keys().map(|seq| i64::from(seq.0)).collect();
        let attempts: Vec<i64> = tasks.values().map(|t| i64::from(t.attempt)).collect();
        let states: Vec<&str> = tasks.values().map(|t| codec::task_phase(t.phase)).collect();
        // Calls in order of seq, as every writer of `tool_calls` locks them. A higher rank
        // replaces the record kept; the update re-reads the row it waited for.
        let statement = format!(
            "INSERT INTO tool_calls (tenant, run_id, seq, attempts, task_state) \
             SELECT $1, $2, seq, attempt, state \
             FROM unnest($3::bigint[], $4::bigint[], $5::text[]) AS t(seq, attempt, state) \
             WHERE NOT EXISTS (SELECT 1 FROM run_summaries WHERE run_id = $2 AND tenant <> $1) \
             ORDER BY seq \
             ON CONFLICT (run_id, seq) DO UPDATE \
                 SET attempts = EXCLUDED.attempts, task_state = EXCLUDED.task_state \
                 WHERE (EXCLUDED.attempts, {}) \
                     > (COALESCE(tool_calls.attempts, -1), COALESCE({}, -1))",
            task_step("EXCLUDED.task_state"),
            task_step("tool_calls.task_state"),
        );
        let attempt = || async {
            sqlx::query(&statement)
                .bind(tenant(&run.tenant))
                .bind(uuid(run.id))
                .bind(&seqs)
                .bind(&attempts)
                .bind(&states)
                .execute(&self.pool)
                .await
                .map_err(|err| Failure::<ReadModelError>::Db(err.into()))
        };
        retrying(attempt).await?;
        Ok(())
    }

    async fn attempts(&self, run: &RunRef) -> Result<BTreeMap<Seq, u32>, ReadModelError> {
        let rows = sqlx::query(
            "SELECT c.seq, c.attempts FROM tool_calls c \
             JOIN run_summaries s ON s.run_id = c.run_id \
             WHERE c.run_id = $1 AND s.tenant = $2 AND c.attempts IS NOT NULL",
        )
        .bind(uuid(run.id))
        .bind(tenant(&run.tenant))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        let mut attempts = BTreeMap::new();
        for row in &rows {
            attempts.insert(Seq(row.u32(0)?), row.u32(1)?);
        }
        Ok(attempts)
    }

    async fn record_quarantine(
        &self,
        run: &RunRef,
        quarantine: Option<&QuarantineNote>,
        at: Timestamp,
    ) -> Result<(), ReadModelError> {
        // The latest record wins; at the same time, a quarantine wins over a release.
        sqlx::query(
            "INSERT INTO run_quarantine (run_id, tenant, recorded_at, kind, reason, since) \
             SELECT $1, $2, $3, $4, $5, $6 \
             WHERE NOT EXISTS (SELECT 1 FROM run_summaries WHERE run_id = $1 AND tenant <> $2) \
             ON CONFLICT (run_id) DO UPDATE SET recorded_at = EXCLUDED.recorded_at, \
                 kind = EXCLUDED.kind, reason = EXCLUDED.reason, since = EXCLUDED.since \
                 WHERE EXCLUDED.recorded_at > run_quarantine.recorded_at \
                     OR (EXCLUDED.recorded_at = run_quarantine.recorded_at \
                         AND run_quarantine.kind IS NULL AND EXCLUDED.kind IS NOT NULL)",
        )
        .bind(uuid(run.id))
        .bind(tenant(&run.tenant))
        .bind(ts(at))
        .bind(quarantine.map(|note| codec::quarantine_kind(note.kind)))
        .bind(quarantine.map(|note| note.reason.as_str()))
        .bind(quarantine.map(|note| ts(note.since)))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
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
             {} \
             SELECT count(*) FROM purged",
            remove_records("SELECT run_id FROM purged"),
        );
        let attempt = || async {
            let mut query = sqlx::query_scalar::<_, i64>(&statement).bind(ts(ended_before));
            if let Some(tenants) = &scope_tenants {
                query = query.bind(tenants);
            }
            query
                .fetch_one(&self.pool)
                .await
                .map_err(|err| Failure::<ReadModelError>::Db(err.into()))
        };
        let purged = retrying(attempt).await?;
        Ok(usize::try_from(purged).unwrap_or(usize::MAX))
    }

    async fn delete_run(&self, run: &RunRef) -> Result<bool, ReadModelError> {
        // Another tenant's run keeps its rows; the records of a run never projected go.
        let statement = format!(
            "WITH elsewhere AS (SELECT 1 FROM run_summaries WHERE run_id = $1 AND tenant <> $2), \
                 summary AS (DELETE FROM run_summaries WHERE run_id = $1 AND tenant = $2 \
                     RETURNING run_id), \
                 {} \
             SELECT count(*) FROM summary",
            remove_records("SELECT $1::uuid WHERE NOT EXISTS (SELECT 1 FROM elsewhere)"),
        );
        let attempt = || async {
            sqlx::query_scalar::<_, i64>(&statement)
                .bind(uuid(run.id))
                .bind(tenant(&run.tenant))
                .fetch_one(&self.pool)
                .await
                .map_err(|err| Failure::<ReadModelError>::Db(err.into()))
        };
        let deleted = retrying(attempt).await?;
        Ok(deleted > 0)
    }
}

/// The CTEs that delete everything of the runs `runs` selects but their summary.
fn remove_records(runs: &str) -> String {
    [
        "tool_calls",
        "run_labels",
        "run_waits",
        "run_search",
        "run_quarantine",
    ]
    .iter()
    .map(|table| format!("removed_{table} AS (DELETE FROM {table} WHERE run_id IN ({runs}))"))
    .collect::<Vec<_>>()
    .join(", ")
}

#[cfg(test)]
mod tests {
    use super::{furthest, task_step};
    use ptc_core::Seq;
    use ptc_engine::ports::{TaskPhase, TaskProgress};

    fn task(seq: u32, attempt: u32, phase: TaskPhase) -> TaskProgress {
        TaskProgress {
            seq: Seq(seq),
            attempt,
            phase,
        }
    }

    #[test]
    fn a_batch_keeps_the_first_record_of_the_highest_rank() {
        let kept = furthest(&[
            task(1, 1, TaskPhase::Claimed),
            task(1, 1, TaskPhase::Ready),
            task(1, 1, TaskPhase::Parked),
            task(2, 0, TaskPhase::Approval),
            task(2, 0, TaskPhase::Claimed),
        ]);
        assert_eq!(kept[&Seq(1)].phase, TaskPhase::Ready);
        assert_eq!(
            kept[&Seq(2)].phase,
            TaskPhase::Approval,
            "a tie keeps the first"
        );
    }

    #[test]
    fn steps_follow_the_rank() {
        assert_eq!(
            task_step("x"),
            "CASE x WHEN 'approval' THEN 0 WHEN 'claimed' THEN 0 WHEN 'parked' THEN 1 \
             WHEN 'deferred' THEN 1 WHEN 'ready' THEN 2 END"
        );
    }
}
