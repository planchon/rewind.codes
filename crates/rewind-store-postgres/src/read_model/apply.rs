//! `apply`: a batch of a run's entries and the snapshot of its history up to the last of them,
//! in one transaction.
//!
//! The transaction first takes the summary's row (`FOR UPDATE`, or by inserting it), and decides
//! under that lock: projectors applying batches of one run at once take turns, so "the snapshot
//! of the longest history wins" holds whatever their order. The row keeps the length of its
//! snapshot (`snapshot_len`): a longer one replaces the mode, the ending and the waits, wholesale;
//! a shorter one, or one as long (equal by the contract), changes nothing.
//!
//! What comes from the entries is keyed so that applying them twice, or in any order, gives the
//! same rows: a call's columns are set by the entry that knows them (`schedule_tool_call`, its
//! completion), the summary keeps the maximum of the times and lengths it has seen, and its
//! counts are recounted from `tool_calls`. Every batch stamps the summary as changed
//! ([`STAMP`]).

use std::collections::BTreeMap;

use rewind_core::{
    Command, Completion, Event, HistoryEntry, Lifecycle, Payload, RunStatus, ScriptError, Shape,
    Timestamp,
};
use rewind_engine::ports::{ProjectedRun, RunSnapshot};
use sqlx::{PgConnection, PgPool};

use super::STAMP;
use super::query::SEARCH_SEPARATOR;
use crate::Error;
use crate::codec::{self, decode_labels, encode_labels};
use crate::sql::{Columns, bytes, int, opt_ts, tenant, ts, uuid};

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

/// What free text is searched in (`run_search.search_text`): the run's id, its headline, its
/// ending's error and message, its labels' values, each in lowercase, joined by
/// [`SEARCH_SEPARATOR`]. Each field is lowercased alone, as the read model's contract reads
/// them: a final sigma is one at the end of its field.
pub(super) fn search_text(
    run: &ProjectedRun,
    error: Option<&str>,
    message: Option<&str>,
) -> String {
    let mut fields = vec![run.run.id.to_string(), run.headline.to_lowercase()];
    fields.extend(error.map(str::to_lowercase));
    fields.extend(message.map(str::to_lowercase));
    fields.extend(run.labels.values().map(|value| value.to_lowercase()));
    let mut separator = [0; 4];
    fields.join(SEARCH_SEPARATOR.encode_utf8(&mut separator))
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
        let (seq, outcome) = match &entry.event {
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
        let replace = self
            .outcomes
            .get(&i64::from(seq.0))
            .is_none_or(|(index, ..)| *index <= entry.index);
        if replace {
            self.outcomes
                .insert(i64::from(seq.0), (entry.index, at, outcome));
        }
    }

    fn is_empty(&self) -> bool {
        self.scheduled.is_empty() && self.outcomes.is_empty()
    }
}

/// The summary's row as the transaction found it, locked.
struct Locked {
    /// `None`: the transaction inserted the row.
    tenant: Option<String>,
    /// `-1` for a row just inserted: any snapshot is longer.
    snapshot_len: i64,
    labels: Option<String>,
    error: Option<String>,
    message: Option<String>,
    search_text: Option<String>,
}

/// Takes the summary's row, inserting it if the run was never projected. A concurrent insert
/// is waited for, then its row is taken.
async fn lock_summary(conn: &mut PgConnection, run: &ProjectedRun) -> Result<Locked, Error> {
    let id = uuid(run.run.id);
    loop {
        let row = sqlx::query(
            "SELECT s.tenant, s.snapshot_len, s.labels::text, s.error, s.message, t.search_text \
             FROM run_summaries s LEFT JOIN run_search t ON t.run_id = s.run_id \
             WHERE s.run_id = $1 FOR UPDATE OF s",
        )
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(row) = row {
            return Ok(Locked {
                tenant: Some(row.text(0)?),
                snapshot_len: row.i64(1)?,
                labels: Some(row.text(2)?),
                error: row.opt_text(3)?,
                message: row.opt_text(4)?,
                search_text: row.opt_text(5)?,
            });
        }
        let inserted = sqlx::query(
            "INSERT INTO run_summaries (run_id, tenant, script, toolset, headline, labels, \
                 status, last_activity_at, tool_calls, errors, history_len, snapshot_len, \
                 namespace) \
             VALUES ($1, $2, $3, $4, '', '{}', 'active', 0, 0, 0, 0, -1, $5) \
             ON CONFLICT (run_id) DO NOTHING",
        )
        .bind(id)
        .bind(tenant(&run.run.tenant))
        .bind(bytes(run.script))
        .bind(bytes(run.toolset))
        .bind(run.namespace.as_str())
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if inserted == 1 {
            return Ok(Locked {
                tenant: None,
                snapshot_len: -1,
                labels: None,
                error: None,
                message: None,
                search_text: None,
            });
        }
    }
}

pub(super) async fn apply_once(
    pool: &PgPool,
    run: &ProjectedRun,
    entries: &[HistoryEntry],
    snapshot: &RunSnapshot,
) -> Result<(), Error> {
    let mut tx = pool.begin().await?;
    let id = uuid(run.run.id);
    let tenant_id = tenant(&run.run.tenant);
    let locked = lock_summary(&mut tx, run).await?;

    // Records made before the run was projected, or under another tenant, now name the run's.
    if locked.tenant.as_deref() != Some(tenant_id) {
        sqlx::query(
            "WITH calls AS (UPDATE tool_calls SET tenant = $2 WHERE run_id = $1 AND tenant <> $2), \
                 quarantine AS (UPDATE run_quarantine SET tenant = $2 \
                     WHERE run_id = $1 AND tenant <> $2), \
                 labels AS (UPDATE run_labels SET tenant = $2 WHERE run_id = $1 AND tenant <> $2), \
                 waits AS (UPDATE run_waits SET tenant = $2 WHERE run_id = $1 AND tenant <> $2) \
             UPDATE run_search SET tenant = $2 WHERE run_id = $1 AND tenant <> $2",
        )
        .bind(id)
        .bind(tenant_id)
        .execute(&mut *tx)
        .await?;
    }

    let labels_changed = match &locked.labels {
        Some(labels) => decode_labels(labels)? != run.labels,
        None => true,
    };
    if labels_changed {
        sqlx::query("DELETE FROM run_labels WHERE run_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if !run.labels.is_empty() {
            let keys: Vec<&str> = run.labels.keys().map(String::as_str).collect();
            let values: Vec<&str> = run.labels.values().map(String::as_str).collect();
            sqlx::query(
                "INSERT INTO run_labels (run_id, key, value, tenant) \
                 SELECT $1, key, value, $2 FROM unnest($3::text[], $4::text[]) AS l(key, value)",
            )
            .bind(id)
            .bind(tenant_id)
            .bind(keys)
            .bind(values)
            .execute(&mut *tx)
            .await?;
        }
    }

    // The snapshot of the longest history wins; one as long changes nothing.
    let won = int(snapshot.history_len) > locked.snapshot_len;
    if won {
        sqlx::query("DELETE FROM run_waits WHERE run_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if !snapshot.waits.is_empty() {
            let waits = &snapshot.waits;
            let positions: Vec<i32> = (0..).take(waits.len()).collect();
            let kinds: Vec<&str> = waits.iter().map(|w| codec::wait_kind(w.kind)).collect();
            let seqs: Vec<Option<i64>> = waits
                .iter()
                .map(|w| w.seq.map(|seq| i64::from(seq.0)))
                .collect();
            let names: Vec<Option<&str>> = waits.iter().map(|w| w.name.as_deref()).collect();
            let since: Vec<i64> = waits.iter().map(|w| ts(w.since)).collect();
            let until: Vec<Option<i64>> = waits.iter().map(|w| opt_ts(w.until)).collect();
            let lines: Vec<Option<i64>> = waits
                .iter()
                .map(|w| w.callsite.map(|callsite| i64::from(callsite.line)))
                .collect();
            let columns: Vec<Option<i64>> = waits
                .iter()
                .map(|w| w.callsite.map(|callsite| i64::from(callsite.column)))
                .collect();
            sqlx::query(
                "INSERT INTO run_waits (run_id, position, tenant, kind, seq, name, since, until, \
                     callsite_line, callsite_column) \
                 SELECT $1, position, $2, kind, seq, name, since, until, line, col \
                 FROM unnest($3::integer[], $4::text[], $5::bigint[], $6::text[], $7::bigint[], \
                     $8::bigint[], $9::bigint[], $10::bigint[]) \
                     AS w(position, kind, seq, name, since, until, line, col)",
            )
            .bind(id)
            .bind(tenant_id)
            .bind(positions)
            .bind(kinds)
            .bind(seqs)
            .bind(names)
            .bind(since)
            .bind(until)
            .bind(lines)
            .bind(columns)
            .execute(&mut *tx)
            .await?;
        }
    }

    // The search text, from the metadata just given and the ending kept.
    let (error, message) = match (&snapshot.ending, won) {
        (Some(ending), true) => (Some(ending.error.as_str()), Some(ending.message.as_str())),
        (None, true) => (None, None),
        (_, false) => (locked.error.as_deref(), locked.message.as_deref()),
    };
    let text = search_text(run, error, message);
    if locked.search_text.as_deref() != Some(text.as_str()) {
        sqlx::query(
            "INSERT INTO run_search (run_id, tenant, search_text) VALUES ($1, $2, $3) \
             ON CONFLICT (run_id) DO UPDATE SET tenant = EXCLUDED.tenant, \
                 search_text = EXCLUDED.search_text",
        )
        .bind(id)
        .bind(tenant_id)
        .bind(&text)
        .execute(&mut *tx)
        .await?;
    }

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
    write_calls(&mut tx, tenant_id, id, &calls).await?;

    let ending = snapshot.ending.as_ref();
    let statement = format!(
        "UPDATE run_summaries SET tenant = $2, script = $3, toolset = $4, headline = $5, \
             parent = $6, workflow = $7, started_by = $8, labels = $9::jsonb, \
             last_activity_at = GREATEST(last_activity_at, $10), \
             history_len = GREATEST(history_len, $11), started_at = COALESCE($12, started_at), \
             status = COALESCE($13, status), ended_at = COALESCE($14, ended_at), \
             tool_calls = CASE WHEN $15 THEN (SELECT count(*) FROM tool_calls \
                 WHERE run_id = $1 AND scheduled_at IS NOT NULL) ELSE tool_calls END, \
             errors = CASE WHEN $15 THEN (SELECT count(*) FROM tool_calls \
                 WHERE run_id = $1 AND outcome = 'failed') ELSE errors END, \
             snapshot_len = CASE WHEN $16 THEN $17 ELSE snapshot_len END, \
             mode = CASE WHEN $16 THEN $18 ELSE mode END, \
             error = CASE WHEN $16 THEN $19 ELSE error END, \
             message = CASE WHEN $16 THEN $20 ELSE message END, \
             line = CASE WHEN $16 THEN $21 ELSE line END, \
             signature = CASE WHEN $16 THEN $22 ELSE signature END, \
             fingerprint = CASE WHEN $16 THEN $23 ELSE fingerprint END, \
             wakes_at = CASE WHEN COALESCE($13, status) = 'active' \
                 THEN (SELECT min(until) FROM run_waits WHERE run_id = $1) END, {STAMP} \
         WHERE run_id = $1",
    );
    sqlx::query(&statement)
        .bind(id)
        .bind(tenant_id)
        .bind(bytes(run.script))
        .bind(bytes(run.toolset))
        .bind(&run.headline)
        .bind(run.parent.map(uuid))
        .bind(run.workflow.as_deref())
        .bind(run.started_by.map(uuid))
        .bind(encode_labels(&run.labels))
        .bind(ts(last_activity))
        .bind(int(history_len))
        .bind(opt_ts(started_at))
        .bind(ended.map(|(_, status, _)| status.as_str()))
        .bind(opt_ts(ended.map(|(.., at)| at)))
        .bind(!calls.is_empty())
        .bind(won)
        .bind(int(snapshot.history_len))
        .bind(snapshot.mode.map(codec::mode))
        .bind(ending.map(|ending| ending.error.as_str()))
        .bind(ending.map(|ending| ending.message.as_str()))
        .bind(ending.and_then(|ending| ending.line).map(i64::from))
        .bind(ending.map(|ending| ending.signature.as_str()))
        .bind(ending.map(|ending| ending.fingerprint.as_str()))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Writes what a batch says of its run's calls: each column by the entry that knows it.
async fn write_calls(
    conn: &mut PgConnection,
    tenant_id: &str,
    id: uuid::Uuid,
    calls: &Calls,
) -> Result<(), Error> {
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
             ON CONFLICT (run_id, seq) DO UPDATE SET tenant = EXCLUDED.tenant, \
                 tool = EXCLUDED.tool, callsite_line = EXCLUDED.callsite_line, \
                 scheduled_at = EXCLUDED.scheduled_at, args_digest = EXCLUDED.args_digest",
        )
        .bind(tenant_id)
        .bind(id)
        .bind(seqs)
        .bind(tools)
        .bind(lines)
        .bind(at)
        .bind(args)
        .execute(&mut *conn)
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
             ON CONFLICT (run_id, seq) DO UPDATE SET tenant = EXCLUDED.tenant, \
                 completed_at = EXCLUDED.completed_at, outcome = 'completed', error_kind = NULL, \
                 result_digest = EXCLUDED.result_digest, result_shape = EXCLUDED.result_shape",
        )
        .bind(tenant_id)
        .bind(id)
        .bind(completed.0)
        .bind(completed.1)
        .bind(completed.2)
        .bind(completed.3)
        .execute(&mut *conn)
        .await?;
    }
    if !failed.0.is_empty() {
        sqlx::query(
            "INSERT INTO tool_calls (tenant, run_id, seq, completed_at, outcome, error_kind) \
             SELECT $1, $2, seq, at, 'failed', kind \
             FROM unnest($3::bigint[], $4::bigint[], $5::text[]) AS c(seq, at, kind) \
             ON CONFLICT (run_id, seq) DO UPDATE SET tenant = EXCLUDED.tenant, \
                 completed_at = EXCLUDED.completed_at, outcome = 'failed', \
                 error_kind = EXCLUDED.error_kind, result_digest = NULL, result_shape = NULL",
        )
        .bind(tenant_id)
        .bind(id)
        .bind(failed.0)
        .bind(failed.1)
        .bind(failed.2)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rewind_core::{Digest, RunId};
    use rewind_engine::ports::{ProjectedRun, RunRef, TenantId};

    use super::search_text;

    #[test]
    fn search_text_holds_each_field_in_lowercase_apart() {
        let run = ProjectedRun {
            run: RunRef::new(TenantId::local(), RunId::from_u128(4)),
            namespace: rewind_engine::ports::NamespaceName::migrated(),
            script: Digest::of(b"s"),
            headline: "// Close THE Issues".into(),
            toolset: Digest::of(b"t"),
            labels: BTreeMap::from([("a".to_owned(), "Triage".to_owned())]),
            parent: None,
            workflow: None,
            started_by: None,
        };
        let text = search_text(&run, Some("TypeError"), Some("ΟΔΟΣ"));
        assert_eq!(
            text, "00000000-0000-0000-0000-000000000004A// close the issuesAtypeerrorAοδοςAtriage",
            "each field alone, its final sigma included"
        );
    }
}
