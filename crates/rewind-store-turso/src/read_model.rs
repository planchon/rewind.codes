//! `ReadModel` on Turso: `run_summaries` with `run_labels`, `run_waits` and `run_texts`, and
//! `tool_calls`, filled by the projector; task states and quarantine, recorded by whoever
//! changes them (`archi/05-testing-and-observability.md`, "Read model").
//!
//! Every write is keyed so that applying entries twice, or in any order, gives the same rows:
//! a call's columns are set by the entry that knows them (`schedule_tool_call`, its
//! completion), a summary keeps the maximum of what it has seen and recounts its calls after
//! each batch, and the snapshot's columns and waits are replaced only by the snapshot of a
//! longer history. The columns derived from both (`wakes_at`, `run_texts`) are computed again
//! at the end of each `apply`.
//!
//! Task states and quarantine are kept by run id alone, so they can be recorded before the run
//! is projected; what is recorded for a run projected under another tenant is ignored.
//!
//! Every write that changes a summary stamps its `changed_at` with the system clock, at least a
//! millisecond after its previous stamp, for the change stream ([`ReadModel::changed_runs`]).
//!
//! A query is a `WHERE` clause over the tenant's summaries ([`query`]); statistics, facets and
//! failure groups are counted in Rust over the runs it matches ([`stats`]).

mod query;
mod stats;

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{
    Callsite, Command, Completion, Digest, Event, HistoryEntry, Lifecycle, Payload, RunId,
    RunStatus, ScriptError, Seq, Shape, Timestamp, ToolFailureKind,
};
use rewind_engine::adapters::SystemClock;
use rewind_engine::ports::{
    ChangedRun, Clock, Ending, FailureGroups, FailureWindow, ProjectedRun, QuarantineNote,
    ReadModel, ReadModelError, RunOrder, RunQuery, RunRef, RunSnapshot, RunStats, RunSummary,
    ScriptFilter, ScriptSummary, SearchCursor, StatsWindow, TaskProgress, TenantId, TenantScope,
    ToolStats, ToolStatsFilter, Wait, WaitKind,
};
use turso::{Connection, Row, Value};

use crate::codec::{self, decode_labels, encode_labels};
use crate::sql::{
    self, Columns, digest, int, limit, opt_text, opt_ts, placeholders, run_id, scope_clause,
    tenant, text, transaction, ts,
};
use crate::{Error, TursoStore};

use query::Sql;
use stats::Found;

impl From<Error> for ReadModelError {
    fn from(err: Error) -> Self {
        ReadModelError::Backend(err.to_string())
    }
}

/// Run ids bound in one `IN (…)` list at most.
const IDS_PER_STATEMENT: usize = 500;

/// The tables that hold something of a run, besides `run_summaries`.
const RUN_TABLES: [&str; 5] = [
    "tool_calls",
    "run_labels",
    "run_waits",
    "run_texts",
    "run_quarantine",
];

/// The columns a summary is read from, in the order of [`summary_of`], then when it last
/// changed ([`CHANGED_AT`]).
const SUMMARY_COLUMNS: &str = "s.run_id, s.tenant, s.script, s.headline, s.toolset, s.status, \
     s.mode, s.labels, s.parent, s.workflow, s.started_by, s.started_at, s.ended_at, \
     s.last_activity_at, s.tool_calls, s.errors, s.history_len, s.wakes_at, s.ending_error, \
     s.ending_message, s.ending_line, s.ending_signature, s.fingerprint, q.kind, q.reason, \
     q.since, s.namespace, s.origin, s.suffix_len, s.changed_at \
     FROM run_summaries s LEFT JOIN run_quarantine q ON q.run_id = s.run_id";

/// The column of [`SUMMARY_COLUMNS`] that holds `changed_at`.
const CHANGED_AT: usize = 29;

/// Now, as `changed_at` is stamped: the system clock, never virtual time.
fn now_ms() -> Value {
    int(SystemClock.now().as_millis())
}

/// Stamps the run's summary as changed now, at least a millisecond after its previous stamp: a
/// run not projected yet has none to stamp.
async fn stamp(connection: &Connection, run: RunId) -> Result<(), Error> {
    sql::execute(
        connection,
        "UPDATE run_summaries SET changed_at = MAX(?, changed_at + 1) WHERE run_id = ?",
        vec![now_ms(), run_id(run)],
    )
    .await?;
    Ok(())
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

// -------------------------------------------------------------------------------------------
// Writes

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

/// Replaces the snapshot's columns and waits with `snapshot`'s. A wait's task state is not
/// kept: the waits read it from the task records.
async fn keep_snapshot(
    connection: &Connection,
    run: RunId,
    snapshot: &RunSnapshot,
) -> Result<(), Error> {
    let ending = snapshot.ending.as_ref();
    sql::execute(
        connection,
        "UPDATE run_summaries SET snapshot_len = ?, mode = ?, ending_error = ?, \
         ending_message = ?, ending_line = ?, ending_signature = ?, fingerprint = ? \
         WHERE run_id = ?",
        vec![
            int(snapshot.history_len),
            opt_text(snapshot.mode.map(codec::run_mode)),
            opt_text(ending.map(|ending| ending.error.as_str())),
            opt_text(ending.map(|ending| ending.message.as_str())),
            ending
                .and_then(|ending| ending.line)
                .map_or(Value::Null, |line| int(u64::from(line))),
            opt_text(ending.map(|ending| ending.signature.as_str())),
            opt_text(ending.map(|ending| ending.fingerprint.as_str())),
            run_id(run),
        ],
    )
    .await?;
    sql::execute(
        connection,
        "DELETE FROM run_waits WHERE run_id = ?",
        vec![run_id(run)],
    )
    .await?;
    for (position, wait) in snapshot.waits.iter().enumerate() {
        sql::execute(
            connection,
            "INSERT INTO run_waits (run_id, position, kind, seq, name, since, until, \
             callsite_line, callsite_column) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                run_id(run),
                int(position as u64),
                text(codec::wait_kind(wait.kind)),
                wait.seq.map_or(Value::Null, |seq| int(u64::from(seq.0))),
                opt_text(wait.name.as_deref()),
                ts(wait.since),
                opt_ts(wait.until),
                wait.callsite
                    .map_or(Value::Null, |callsite| int(u64::from(callsite.line))),
                wait.callsite
                    .map_or(Value::Null, |callsite| int(u64::from(callsite.column))),
            ],
        )
        .await?;
    }
    Ok(())
}

/// Replaces the run's labels with those of its metadata.
async fn replace_labels(connection: &Connection, run: &ProjectedRun) -> Result<(), Error> {
    let id = run.run.id;
    sql::execute(
        connection,
        "DELETE FROM run_labels WHERE run_id = ?",
        vec![run_id(id)],
    )
    .await?;
    for (key, value) in &run.labels {
        sql::execute(
            connection,
            "INSERT INTO run_labels (tenant, run_id, key, value) VALUES (?, ?, ?, ?)",
            vec![
                tenant(&run.run.tenant),
                run_id(id),
                text(key.as_str()),
                text(value.as_str()),
            ],
        )
        .await?;
    }
    Ok(())
}

/// Computes again what derives from both the entries and the kept snapshot: `wakes_at`, the
/// earliest `until` of the waits while the run is active, and the texts free text is searched
/// in, lowercased here with Unicode's rules (SQLite's `lower` only knows ASCII).
async fn derive(connection: &Connection, run: &ProjectedRun) -> Result<(), Error> {
    let id = run.run.id;
    let row = sql::query_row(
        connection,
        "SELECT status, ending_error, ending_message FROM run_summaries WHERE run_id = ?",
        vec![run_id(id)],
    )
    .await?
    .ok_or_else(|| Error::Corrupt(format!("the summary of run {id} is missing")))?;
    let status = codec::parse_status(&row.text(0)?)?;
    let wakes_at = if status == RunStatus::Active {
        sql::query_row(
            connection,
            "SELECT MIN(until) FROM run_waits WHERE run_id = ?",
            vec![run_id(id)],
        )
        .await?
        .map(|row| row.opt_ts(0))
        .transpose()?
        .flatten()
    } else {
        None
    };
    sql::execute(
        connection,
        "UPDATE run_summaries SET wakes_at = ? WHERE run_id = ?",
        vec![opt_ts(wakes_at), run_id(id)],
    )
    .await?;

    let mut texts = vec![id.to_string(), run.headline.to_lowercase()];
    texts.extend(run.labels.values().map(|value| value.to_lowercase()));
    texts.extend(row.opt_text(1)?.map(|error| error.to_lowercase()));
    texts.extend(row.opt_text(2)?.map(|message| message.to_lowercase()));
    sql::execute(
        connection,
        "DELETE FROM run_texts WHERE run_id = ?",
        vec![run_id(id)],
    )
    .await?;
    for (field, value) in texts.into_iter().enumerate() {
        sql::execute(
            connection,
            "INSERT INTO run_texts (run_id, field, text) VALUES (?, ?, ?)",
            vec![run_id(id), int(field as u64), text(value)],
        )
        .await?;
    }
    Ok(())
}

/// The tenant a run is projected under, if it is.
async fn owner(connection: &Connection, run: RunId) -> Result<Option<String>, Error> {
    sql::query_row(
        connection,
        "SELECT tenant FROM run_summaries WHERE run_id = ?",
        vec![run_id(run)],
    )
    .await?
    .map(|row| row.text(0))
    .transpose()
}

/// Whether the run is projected under another tenant than `run`'s: what is recorded for it is
/// ignored.
async fn foreign(connection: &Connection, run: &RunRef) -> Result<bool, Error> {
    Ok(owner(connection, run.id)
        .await?
        .is_some_and(|owner| owner != run.tenant.as_str()))
}

// -------------------------------------------------------------------------------------------
// Reads

/// A summary from a row of [`SUMMARY_COLUMNS`], without its waits.
fn summary_of(row: &Row) -> Result<RunSummary, Error> {
    let ending = match row.opt_text(18)? {
        Some(error) => Some(Ending {
            error,
            message: row.text(19)?,
            line: row.opt_u32(20)?,
            signature: row.text(21)?,
            fingerprint: row.text(22)?,
        }),
        None => None,
    };
    let quarantine = match row.opt_text(23)? {
        Some(kind) => Some(QuarantineNote {
            kind: codec::parse_quarantine_kind(&kind)?,
            reason: row.text(24)?,
            since: row.ts(25)?,
        }),
        None => None,
    };
    let namespace = row.text(26)?;
    Ok(RunSummary {
        run: RunRef::new(row.tenant(1)?, row.run_id(0)?),
        namespace: rewind_engine::ports::NamespaceName::new(namespace.clone())
            .map_err(|_| Error::Corrupt(format!("invalid namespace {namespace:?}")))?,
        origin: row.text(27)?,
        suffix_len: row.suffix_len(28)?,
        script: row.digest(2)?,
        headline: row.text(3)?,
        toolset: row.digest(4)?,
        status: codec::parse_status(&row.text(5)?)?,
        mode: row
            .opt_text(6)?
            .map(|mode| codec::parse_run_mode(&mode))
            .transpose()?,
        labels: decode_labels(&row.text(7)?)?,
        parent: row.opt_run_id(8)?,
        workflow: row.opt_text(9)?,
        started_by: row.opt_run_id(10)?,
        started_at: row.opt_ts(11)?,
        ended_at: row.opt_ts(12)?,
        last_activity_at: row.ts(13)?,
        tool_calls: row.u64(14)?,
        errors: row.u64(15)?,
        history_len: row.u64(16)?,
        waits: Vec::new(),
        wakes_at: row.opt_ts(17)?,
        ending,
        quarantine,
    })
}

/// The summaries of the runs `filter` matches, in `order` if any, at most `limit`, with their
/// waits.
async fn summaries(
    connection: &Connection,
    filter: Sql,
    order: Option<RunOrder>,
    count: Option<usize>,
) -> Result<Vec<RunSummary>, Error> {
    let mut statement = format!("SELECT {SUMMARY_COLUMNS} WHERE {}", filter.text);
    let mut params = filter.params;
    if let Some(order) = order {
        statement.push_str(" ORDER BY ");
        statement.push_str(&query::order_by(order));
    }
    if let Some(count) = count {
        statement.push_str(" LIMIT ?");
        params.push(limit(count));
    }
    let rows = sql::query_dynamic(connection, &statement, params).await?;
    let mut found = rows.iter().map(summary_of).collect::<Result<Vec<_>, _>>()?;
    attach_waits(connection, &mut found).await?;
    Ok(found)
}

/// Gives the active runs among `summaries` their waits: only an active run waits.
async fn attach_waits(connection: &Connection, summaries: &mut [RunSummary]) -> Result<(), Error> {
    let active: Vec<RunId> = summaries
        .iter()
        .filter(|summary| summary.status == RunStatus::Active)
        .map(|summary| summary.run.id)
        .collect();
    let mut waits = waits_of(connection, &active).await?;
    for summary in summaries {
        if let Some(run_waits) = waits.remove(&summary.run.id) {
            summary.waits = run_waits;
        }
    }
    Ok(())
}

/// The waits of these runs, in their snapshot's order, each call's with its task's furthest
/// state.
async fn waits_of(
    connection: &Connection,
    runs: &[RunId],
) -> Result<BTreeMap<RunId, Vec<Wait>>, Error> {
    let mut waits: BTreeMap<RunId, Vec<Wait>> = BTreeMap::new();
    for chunk in runs.chunks(IDS_PER_STATEMENT) {
        let rows = sql::query_dynamic(
            connection,
            &format!(
                "SELECT w.run_id, w.kind, w.seq, w.name, w.since, w.until, w.callsite_line, \
                 w.callsite_column, c.task_phase FROM run_waits w \
                 LEFT JOIN tool_calls c ON c.run_id = w.run_id AND c.seq = w.seq \
                 WHERE w.run_id IN ({}) ORDER BY w.run_id, w.position",
                placeholders(chunk.len())
            ),
            chunk.iter().map(|id| run_id(*id)).collect(),
        )
        .await?;
        for row in &rows {
            let kind = codec::parse_wait_kind(&row.text(1)?)?;
            let task = match kind {
                WaitKind::Call | WaitKind::Host => row
                    .opt_text(8)?
                    .map(|phase| codec::parse_task_phase(&phase))
                    .transpose()?,
                WaitKind::Signal | WaitKind::Timer => None,
            };
            let callsite = match (row.opt_u32(6)?, row.opt_u32(7)?) {
                (Some(line), Some(column)) => Some(Callsite { line, column }),
                _ => None,
            };
            waits.entry(row.run_id(0)?).or_default().push(Wait {
                kind,
                seq: row.opt_u32(2)?.map(Seq),
                name: row.opt_text(3)?,
                since: row.ts(4)?,
                until: row.opt_ts(5)?,
                callsite,
                task,
            });
        }
    }
    Ok(waits)
}

/// The runs a query matches, with the tools they called and the kinds their calls failed with.
async fn matching(connection: &Connection, query: &RunQuery) -> Result<Vec<Found>, Error> {
    let summaries = summaries(connection, query::filter(query), None, None).await?;
    let ids: Vec<RunId> = summaries.iter().map(|summary| summary.run.id).collect();
    let mut calls: BTreeMap<RunId, (BTreeSet<String>, BTreeSet<ToolFailureKind>)> = BTreeMap::new();
    for chunk in ids.chunks(IDS_PER_STATEMENT) {
        let rows = sql::query_dynamic(
            connection,
            &format!(
                "SELECT run_id, tool, error_kind FROM tool_calls WHERE run_id IN ({}) \
                 AND (tool IS NOT NULL OR error_kind IS NOT NULL)",
                placeholders(chunk.len())
            ),
            chunk.iter().map(|id| run_id(*id)).collect(),
        )
        .await?;
        for row in &rows {
            let (tools, failed) = calls.entry(row.run_id(0)?).or_default();
            tools.extend(row.opt_text(1)?);
            if let Some(kind) = row.opt_text(2)? {
                failed.insert(codec::parse_failure_kind(&kind)?);
            }
        }
    }
    Ok(summaries
        .into_iter()
        .map(|summary| {
            let (tools, failed) = calls.remove(&summary.run.id).unwrap_or_default();
            Found {
                summary,
                tools,
                failed,
            }
        })
        .collect())
}

impl ReadModel for TursoStore {
    async fn apply(
        &self,
        run: &ProjectedRun,
        entries: &[HistoryEntry],
        snapshot: &RunSnapshot,
    ) -> Result<(), ReadModelError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let id = run_id(run.run.id);
            let kept_len = sql::query_row(
                connection,
                "SELECT snapshot_len FROM run_summaries WHERE run_id = ?",
                vec![id.clone()],
            )
            .await?
            .map(|row| row.u64(0))
            .transpose()?;
            sql::execute(
                connection,
                "INSERT INTO run_summaries (run_id, tenant, script, headline, toolset, status, \
                 labels, parent, workflow, started_by, last_activity_at, tool_calls, errors, \
                 history_len, snapshot_len, namespace, origin, suffix_len, changed_at) \
                 VALUES (?, ?, ?, ?, ?, 'active', ?, ?, ?, ?, 0, 0, 0, 0, 0, ?, ?, ?, ?) \
                 ON CONFLICT (run_id) DO UPDATE SET tenant = excluded.tenant, \
                 script = excluded.script, headline = excluded.headline, \
                 toolset = excluded.toolset, labels = excluded.labels, parent = excluded.parent, \
                 workflow = excluded.workflow, started_by = excluded.started_by, \
                 namespace = excluded.namespace, origin = excluded.origin, \
                 suffix_len = excluded.suffix_len, \
                 changed_at = MAX(excluded.changed_at, run_summaries.changed_at + 1)",
                vec![
                    id.clone(),
                    tenant(&run.run.tenant),
                    digest(run.script),
                    text(run.headline.as_str()),
                    digest(run.toolset),
                    text(encode_labels(&run.labels)),
                    run.parent.map_or(Value::Null, run_id),
                    opt_text(run.workflow.as_deref()),
                    run.started_by.map_or(Value::Null, run_id),
                    text(run.namespace.as_str()),
                    text(run.origin.as_str()),
                    int(u64::from(run.suffix_len)),
                    now_ms(),
                ],
            )
            .await?;
            // Of equal lengths, the snapshots are equal: the first is kept.
            if kept_len.is_none_or(|kept| snapshot.history_len > kept) {
                keep_snapshot(connection, run.run.id, snapshot).await?;
            }
            replace_labels(connection, run).await?;

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
            Ok(derive(connection, run).await?)
        })
        .await
    }

    async fn search_runs(
        &self,
        query: &RunQuery,
        order: RunOrder,
        after: Option<&SearchCursor>,
        count: usize,
    ) -> Result<Vec<RunSummary>, ReadModelError> {
        let connection = self.connection().await?;
        let mut filter = query::filter(query);
        if let Some(cursor) = after {
            query::after(&mut filter, order, cursor);
        }
        Ok(summaries(&connection, filter, Some(order), Some(count)).await?)
    }

    async fn run_stats(
        &self,
        query: &RunQuery,
        window: &StatsWindow,
    ) -> Result<RunStats, ReadModelError> {
        let connection = self.connection().await?;
        let found = matching(&connection, query).await?;
        Ok(stats::run_stats(&found, window))
    }

    async fn failure_groups(
        &self,
        query: &RunQuery,
        window: &FailureWindow,
    ) -> Result<FailureGroups, ReadModelError> {
        let connection = self.connection().await?;
        let found = matching(&connection, query).await?;
        Ok(stats::failure_groups(&found, window))
    }

    async fn scripts(&self, filter: &ScriptFilter) -> Result<Vec<ScriptSummary>, ReadModelError> {
        let connection = self.connection().await?;
        let mut clauses = String::new();
        let mut params = vec![tenant(&filter.tenant)];
        if let Some(namespace) = &filter.namespace {
            clauses.push_str(" AND s.namespace = ?");
            params.push(text(namespace.as_str()));
        }
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
                "SELECT s.script, s.status, s.started_at, s.labels, s.run_id \
                 FROM run_summaries s WHERE s.tenant = ?{clauses}"
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
                         c.task_attempt FROM tool_calls c \
                         JOIN run_summaries s ON s.run_id = c.run_id \
                         WHERE s.tenant = ? AND c.tool IS NOT NULL AND c.scheduled_at IS NOT NULL"
            .to_owned();
        let mut params = vec![tenant(&filter.tenant)];
        if let Some(namespace) = &filter.namespace {
            query.push_str(" AND s.namespace = ?");
            params.push(text(namespace.as_str()));
        }
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
            if let Some(attempt) = row.opt_u64(5)? {
                entry.retries += attempt.saturating_sub(1);
            }
            match row.opt_text(3)?.as_deref() {
                Some("completed") => entry.completed += 1,
                Some("failed") => {
                    let kind = codec::parse_failure_kind(&row.text(4)?)?;
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
            .map(|(mut entry, latencies)| {
                entry.latency = stats::latency(latencies);
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

    async fn record_tasks(
        &self,
        run: &RunRef,
        tasks: &[TaskProgress],
    ) -> Result<(), ReadModelError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            if foreign(connection, run).await? {
                return Ok(());
            }
            let mut changed = false;
            for task in tasks {
                let seq = int(u64::from(task.seq.0));
                let kept = sql::query_row(
                    connection,
                    "SELECT task_attempt, task_phase FROM tool_calls \
                     WHERE run_id = ? AND seq = ? AND task_attempt IS NOT NULL",
                    vec![run_id(run.id), seq.clone()],
                )
                .await?
                .map(|row| -> Result<TaskProgress, Error> {
                    Ok(TaskProgress {
                        seq: task.seq,
                        attempt: row.u32(0)?,
                        phase: codec::parse_task_phase(&row.text(1)?)?,
                    })
                })
                .transpose()?;
                if kept.is_some_and(|kept| task.rank() <= kept.rank()) {
                    continue;
                }
                sql::execute(
                    connection,
                    "INSERT INTO tool_calls (run_id, seq, task_attempt, task_phase) \
                     VALUES (?, ?, ?, ?) ON CONFLICT (run_id, seq) DO UPDATE SET \
                     task_attempt = excluded.task_attempt, task_phase = excluded.task_phase",
                    vec![
                        run_id(run.id),
                        seq,
                        int(u64::from(task.attempt)),
                        text(codec::task_phase(task.phase)),
                    ],
                )
                .await?;
                changed = true;
            }
            if changed {
                stamp(connection, run.id).await?;
            }
            Ok(())
        })
        .await
    }

    async fn attempts(&self, run: &RunRef) -> Result<BTreeMap<Seq, u32>, ReadModelError> {
        let connection = self.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT c.seq, c.task_attempt FROM tool_calls c \
             JOIN run_summaries s ON s.run_id = c.run_id \
             WHERE c.run_id = ? AND s.tenant = ? AND c.task_attempt IS NOT NULL",
            vec![run_id(run.id), tenant(&run.tenant)],
        )
        .await?;
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
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            if foreign(connection, run).await? {
                return Ok(());
            }
            let kept = sql::query_row(
                connection,
                "SELECT recorded_at, kind IS NOT NULL FROM run_quarantine WHERE run_id = ?",
                vec![run_id(run.id)],
            )
            .await?
            .map(|row| -> Result<(Timestamp, bool), Error> { Ok((row.ts(0)?, row.boolean(1)?)) })
            .transpose()?;
            // The latest record; at the same time, a quarantine over a release.
            let newer = kept.is_none_or(|(kept_at, quarantined)| {
                at > kept_at || (at == kept_at && !quarantined && quarantine.is_some())
            });
            if !newer {
                return Ok(());
            }
            sql::execute(
                connection,
                "INSERT INTO run_quarantine (run_id, recorded_at, kind, reason, since) \
                 VALUES (?, ?, ?, ?, ?) ON CONFLICT (run_id) DO UPDATE SET \
                 recorded_at = excluded.recorded_at, kind = excluded.kind, \
                 reason = excluded.reason, since = excluded.since",
                vec![
                    run_id(run.id),
                    ts(at),
                    opt_text(quarantine.map(|note| codec::quarantine_kind(note.kind))),
                    opt_text(quarantine.map(|note| note.reason.as_str())),
                    opt_ts(quarantine.map(|note| note.since)),
                ],
            )
            .await?;
            stamp(connection, run.id).await?;
            Ok(())
        })
        .await
    }

    async fn changed_runs(
        &self,
        tenants: &[TenantId],
        since: u64,
        count: usize,
    ) -> Result<Vec<ChangedRun>, ReadModelError> {
        if tenants.is_empty() || count == 0 {
            return Ok(Vec::new());
        }
        let connection = self.connection().await?;
        let mut params: Vec<Value> = tenants.iter().map(tenant).collect();
        params.push(int(since));
        params.push(limit(count));
        let rows = sql::query_dynamic(
            &connection,
            &format!(
                "SELECT {SUMMARY_COLUMNS} WHERE s.tenant IN ({}) AND s.changed_at >= ? \
                 ORDER BY s.changed_at, s.run_id LIMIT ?",
                placeholders(tenants.len())
            ),
            params,
        )
        .await?;
        let mut found = Vec::with_capacity(rows.len());
        let mut stamps = Vec::with_capacity(rows.len());
        for row in &rows {
            // A row that cannot be read is left out: one bad summary must not hide the others.
            match summary_of(row).and_then(|summary| Ok((summary, row.u64(CHANGED_AT)?))) {
                Ok((summary, stamp)) => {
                    found.push(summary);
                    stamps.push(stamp);
                }
                Err(error) => {
                    tracing::warn!(%error, "a changed run cannot be read: it is left out");
                }
            }
        }
        attach_waits(&connection, &mut found).await?;
        Ok(found
            .into_iter()
            .zip(stamps)
            .map(|(summary, changed_at)| ChangedRun {
                summary,
                changed_at,
            })
            .collect())
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
            for table in RUN_TABLES {
                sql::execute_dynamic(
                    connection,
                    &format!(
                        "DELETE FROM {table} WHERE run_id IN \
                         (SELECT run_id FROM run_summaries WHERE {ended})"
                    ),
                    params.clone(),
                )
                .await?;
            }
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

    async fn delete_run(&self, run: &RunRef) -> Result<bool, ReadModelError> {
        let connection = self.connection().await?;
        let connection = &*connection;
        transaction(connection, async {
            let projected = match owner(connection, run.id).await? {
                Some(owner) if owner != run.tenant.as_str() => return Ok(false),
                owner => owner.is_some(),
            };
            // Task states and quarantine recorded before the run was projected go too.
            for table in RUN_TABLES.into_iter().chain(["run_summaries"]) {
                sql::execute_dynamic(
                    connection,
                    &format!("DELETE FROM {table} WHERE run_id = ?"),
                    vec![run_id(run.id)],
                )
                .await?;
            }
            Ok(projected)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rewind_core::{Digest, RunId, RunStatus};
    use rewind_engine::ports::{ProjectedRun, ReadModel, RunRef, RunSnapshot, TenantId};

    use crate::sql::{self, run_id};
    use crate::{MasterKey, TursoStore};

    fn projected(n: u128) -> ProjectedRun {
        ProjectedRun {
            run: RunRef::new(TenantId::local(), RunId::from_u128(n)),
            namespace: rewind_engine::ports::NamespaceName::migrated(),
            origin: "script".into(),
            suffix_len: 12,
            script: Digest::of(b"s"),
            headline: String::new(),
            toolset: Digest::of(b"t"),
            labels: BTreeMap::new(),
            parent: None,
            workflow: None,
            started_by: None,
        }
    }

    #[tokio::test]
    async fn a_changed_run_that_cannot_be_read_is_left_out() {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let store = TursoStore::open(
            dir.path().join("journal.db"),
            MasterKey::from_bytes([1; 32]),
        )
        .await
        .expect("open");
        let snapshot = RunSnapshot {
            history_len: 0,
            status: RunStatus::Active,
            mode: None,
            waits: Vec::new(),
            ending: None,
        };
        for n in 1..=3 {
            store
                .apply(&projected(n), &[], &snapshot)
                .await
                .expect("apply");
        }
        {
            let connection = store.connection().await.expect("connection");
            sql::execute(
                &connection,
                "UPDATE run_summaries SET namespace = 'Not a namespace!' WHERE run_id = ?",
                vec![run_id(RunId::from_u128(2))],
            )
            .await
            .expect("a summary that cannot be read");
        }
        let listed: Vec<RunId> = store
            .changed_runs(&[TenantId::local()], 0, 10)
            .await
            .expect("changed_runs")
            .into_iter()
            .map(|change| change.summary.run.id)
            .collect();
        assert_eq!(listed, [RunId::from_u128(1), RunId::from_u128(3)]);
    }
}
