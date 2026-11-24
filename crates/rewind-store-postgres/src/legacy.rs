//! The runs of the schema `legacy` (migration 15), which `rewind server migrate-runs` moves onto
//! the run plane (`archi/08-decisions.md`, "Taken while writing the object-storage spec": the
//! migration): [`PostgresLegacy`], the `LegacyRuns` of the engine.
//!
//! The rows are read as they were written: events and inbox items in the stored format v1
//! (`rewind-format`), payload values in their canonical JSON, digests as their 32 bytes, labels,
//! scopes and attachments as JSON objects, times in milliseconds. A page of runs is one query for
//! the runs and one per table they hold rows in.

use std::collections::BTreeMap;

use rewind_core::{
    Attachments, Digest, Epoch, Event, HistoryEntry, JsonValue, Lifecycle, Payload, RunId,
    SecretRef, Seq,
};
use rewind_engine::ports::legacy::{
    LegacyError, LegacyHistory, LegacyRun, LegacyRuns, LegacyTask, LegacyWebhook,
};
use rewind_engine::ports::{
    ForkParent, InboxItem, NamespaceName, ParkKind, ParkReason, PayloadRecord, Quarantine,
    QuarantineKind, RunMetadata, RunRef, RunWorkflow, StartedBy, TaskState, TraceContext, Trigger,
    Watcher,
};
use sqlx::PgPool;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use crate::codec::decode_labels;
use crate::sql::{Columns, parse_digest};
use crate::{Error, PostgresStore};

/// The runs of the schema `legacy`, read once by `migrate-runs`.
#[derive(Debug, Clone)]
pub struct PostgresLegacy {
    pool: PgPool,
}

impl PostgresStore {
    /// The runs this store's database set aside in the schema `legacy`.
    pub fn legacy(&self) -> PostgresLegacy {
        PostgresLegacy {
            pool: self.pool.clone(),
        }
    }
}

impl From<Error> for LegacyError {
    fn from(error: Error) -> Self {
        match error {
            Error::Corrupt(message) => LegacyError::Corrupt(message),
            Error::Unreachable(message) => LegacyError::Unavailable(message),
            other => LegacyError::Backend(other.to_string()),
        }
    }
}

fn corrupt(message: impl Into<String>) -> Error {
    Error::Corrupt(message.into())
}

fn u32_of(value: u64) -> Result<u32, Error> {
    u32::try_from(value).map_err(|_| corrupt(format!("{value} is out of range")))
}

fn run_id(row: &PgRow, index: usize) -> Result<RunId, Error> {
    row.opt_run_id(index)?
        .ok_or_else(|| corrupt(format!("column {index} is null")))
}

fn value(json: &str) -> Result<JsonValue, Error> {
    serde_json::from_str(json).map_err(|err| corrupt(format!("stored JSON value: {err}")))
}

fn event(json: &str) -> Result<Event, Error> {
    rewind_format::decode_event(json).map_err(|err| corrupt(format!("stored event: {err}")))
}

fn trigger(json: &str) -> Result<Trigger, Error> {
    match event(json)? {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Ok(Trigger::RunStarted(started)),
        Event::Completion(completion) => Ok(Trigger::Completion(completion)),
        other => Err(corrupt(format!(
            "an inbox item holds a {} event",
            other.name()
        ))),
    }
}

fn attachments(json: &str) -> Result<Attachments, Error> {
    decode_labels(json)?
        .into_iter()
        .map(|(server, secret)| {
            let secret = SecretRef::parse(&secret)
                .map_err(|err| corrupt(format!("attachment of {server:?}: {err}")))?;
            Ok((server, secret))
        })
        .collect()
}

fn quarantine_kind(name: &str) -> Result<QuarantineKind, Error> {
    match name {
        "non_determinism" => Ok(QuarantineKind::NonDeterminism),
        "failures" => Ok(QuarantineKind::Failures),
        other => Err(corrupt(format!("unknown quarantine kind {other:?}"))),
    }
}

fn park_kind(name: &str) -> Result<ParkKind, Error> {
    match name {
        "schema_changed" => Ok(ParkKind::SchemaChanged),
        // What a 401 parked a task as before runs attached their secrets: the same refusal.
        "credentials_refused" | "credentials_expired" => Ok(ParkKind::CredentialsRefused),
        "secret_missing" => Ok(ParkKind::SecretMissing),
        other => Err(corrupt(format!("unknown park kind {other:?}"))),
    }
}

const RUN_COLUMNS: &str = "id, tenant, namespace, script, toolset, epoch, expires_at, \
    expiry_enqueued, quarantine_kind, quarantine_reason, quarantine_since, quarantine_retried_by, \
    failures, attempts, labels::text, traceparent, tracestate, parent, parent_at, \
    idempotency_key, created_at, updated_at, ended_at, archive_document, workflow, \
    workflow_version, started_by, started_seq, detached, scope::text, secrets::text, origin, \
    suffix_len::bigint";

fn run_from_row(row: &PgRow) -> Result<LegacyRun, Error> {
    let id = run_id(row, 0)?;
    let namespace = row.text(2)?;
    let quarantine = match row.opt_text(8)? {
        None => None,
        Some(kind) => Some(Quarantine {
            kind: quarantine_kind(&kind)?,
            reason: row.text(9)?,
            since: row.ts(10)?,
            retried_by: row.opt_text(11)?,
        }),
    };
    let trace_context = match row.opt_text(15)? {
        None => None,
        Some(traceparent) => Some(TraceContext {
            traceparent,
            tracestate: row.opt_text(16)?,
        }),
    };
    let parent = match row.opt_run_id(17)? {
        None => None,
        Some(run) => Some(ForkParent {
            run,
            at: row.opt_u64(18)?,
        }),
    };
    let workflow = match row.opt_text(24)? {
        None => None,
        Some(name) => Some(RunWorkflow {
            name,
            version: row.digest(25)?,
        }),
    };
    let suffix_len = row.u64(32)?;
    let started_by = match row.opt_run_id(26)? {
        None => None,
        Some(run) => Some(StartedBy {
            run,
            seq: Seq(u32_of(row.u64(27)?)?),
            detached: row.boolean(28)?,
        }),
    };
    Ok(LegacyRun {
        run: RunRef::new(row.tenant(1)?, id),
        namespace: NamespaceName::new(namespace.clone())
            .map_err(|_| corrupt(format!("invalid namespace {namespace:?}")))?,
        origin: row.text(31)?,
        suffix_len: u8::try_from(suffix_len)
            .map_err(|_| corrupt(format!("a suffix of {suffix_len} characters")))?,
        script: row.digest(3)?,
        action_set: row.digest(4)?,
        epoch: Epoch(u32_of(row.u64(5)?)?),
        expires_at: row.ts(6)?,
        expiry_enqueued: row.boolean(7)?,
        quarantine,
        failures: u32_of(row.u64(12)?)?,
        attempts: u32_of(row.u64(13)?)?,
        metadata: RunMetadata {
            labels: decode_labels(&row.text(14)?)?,
            trace_context,
            parent,
            workflow,
            started_by,
            scope: decode_labels(&row.text(29)?)?,
            secrets: attachments(&row.text(30)?)?,
        },
        idempotency_key: row.opt_text(19)?,
        created_at: row.ts(20)?,
        updated_at: row.ts(21)?,
        ended_at: row.opt_ts(22)?,
        history: match row.opt_digest(23)? {
            Some(document) => LegacyHistory::Archived(document),
            None => LegacyHistory::Entries(Vec::new()),
        },
        inbox: Vec::new(),
        signal_keys: Vec::new(),
        timers: Vec::new(),
        tasks: Vec::new(),
        webhooks: Vec::new(),
        watchers: Vec::new(),
        payloads: Vec::new(),
    })
}

fn task_from_row(row: &PgRow) -> Result<LegacyTask, Error> {
    let state = match row.text(4)?.as_str() {
        "approval" => TaskState::Approval,
        "ready" => TaskState::Ready {
            next_delivery_at: row.opt_ts(7)?.unwrap_or(row.ts(12)?),
        },
        "claimed" => TaskState::Claimed {
            owner: row.opt_text(5)?.unwrap_or_default(),
            until: row.ts(6)?,
        },
        "deferred" => TaskState::Deferred { until: row.ts(6)? },
        "parked" => TaskState::Parked(ParkReason {
            kind: park_kind(&row.text(8)?)?,
            message: row.text(9)?,
        }),
        other => return Err(corrupt(format!("unknown task state {other:?}"))),
    };
    let args = match row.opt_text(3)? {
        Some(json) => Payload::Value(value(&json)?),
        None => Payload::Digest(row.digest(2)?),
    };
    Ok(LegacyTask {
        seq: Seq(u32_of(row.u64(0)?)?),
        action: row.text(1)?,
        args,
        state,
        attempt: u32_of(row.u64(10)?)?,
        schema_change_approved: row.boolean(11)?,
        created_at: row.ts(12)?,
    })
}

impl PostgresLegacy {
    async fn page(&self, after: Option<RunId>, limit: usize) -> Result<Vec<LegacyRun>, Error> {
        let present: bool = sqlx::query_scalar("SELECT to_regclass('legacy.runs') IS NOT NULL")
            .fetch_one(&self.pool)
            .await?;
        if !present || limit == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(&format!(
            "SELECT {RUN_COLUMNS} FROM legacy.runs WHERE $1::uuid IS NULL OR id > $1 \
             ORDER BY id LIMIT $2"
        ))
        .bind(after.map(|id| id.as_uuid()))
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;
        let mut runs = Vec::with_capacity(rows.len());
        let mut at = BTreeMap::new();
        for row in &rows {
            let run = run_from_row(row)?;
            at.insert(run.run.id, runs.len());
            runs.push(run);
        }
        let ids: Vec<Uuid> = runs.iter().map(|run| run.run.id.as_uuid()).collect();
        let index_at = |row: &PgRow, column: usize| -> Result<usize, Error> {
            let id = run_id(row, column)?;
            at.get(&id)
                .copied()
                .ok_or_else(|| corrupt(format!("a row of run {id}, not asked for")))
        };
        let index = |row: &PgRow| index_at(row, 0);

        for row in self
            .rows(
                "SELECT run_id, idx, recorded_at, event FROM legacy.history \
                 WHERE run_id = ANY($1) ORDER BY run_id, idx",
                &ids,
            )
            .await?
        {
            let run = &mut runs[index(&row)?];
            let entry = HistoryEntry {
                index: row.u64(1)?,
                recorded_at: row.ts(2)?,
                event: event(&row.text(3)?)?,
            };
            match &mut run.history {
                LegacyHistory::Entries(entries) => entries.push(entry),
                LegacyHistory::Archived(_) => {
                    return Err(corrupt(format!(
                        "run {} is archived and still has entries",
                        run.run.id
                    )));
                }
            }
        }
        for row in self
            .rows(
                "SELECT run_id, id, event, received_at FROM legacy.inbox \
                 WHERE run_id = ANY($1) ORDER BY run_id, id",
                &ids,
            )
            .await?
        {
            let item = InboxItem {
                id: row.u64(1)?,
                trigger: trigger(&row.text(2)?)?,
                received_at: row.ts(3)?,
            };
            runs[index(&row)?].inbox.push(item);
        }
        for row in self
            .rows(
                "SELECT run_id, key FROM legacy.signal_keys \
                 WHERE run_id = ANY($1) ORDER BY run_id, key",
                &ids,
            )
            .await?
        {
            let key = row.text(1)?;
            runs[index(&row)?].signal_keys.push(key);
        }
        for row in self
            .rows(
                "SELECT run_id, seq, fire_at FROM legacy.timers \
                 WHERE run_id = ANY($1) ORDER BY run_id, seq",
                &ids,
            )
            .await?
        {
            let timer = (Seq(u32_of(row.u64(1)?)?), row.ts(2)?);
            runs[index(&row)?].timers.push(timer);
        }
        for row in self
            .rows(
                "SELECT seq, tool, args_digest, args, state, lease_owner, lease_until, \
                 next_attempt_at, parked_kind, parked_reason, attempt, schema_change_approved, \
                 created_at, run_id FROM legacy.tool_tasks WHERE run_id = ANY($1) \
                 ORDER BY run_id, seq",
                &ids,
            )
            .await?
        {
            let task = task_from_row(&row)?;
            runs[index_at(&row, 13)?].tasks.push(task);
        }
        for row in self
            .rows(
                "SELECT run_id, name, token FROM legacy.webhook_tokens \
                 WHERE run_id = ANY($1) ORDER BY run_id, name",
                &ids,
            )
            .await?
        {
            let webhook = LegacyWebhook {
                name: row.text(1)?,
                hash: Digest::of(row.text(2)?.as_bytes()),
            };
            runs[index(&row)?].webhooks.push(webhook);
        }
        for row in self
            .rows(
                "SELECT run_id, watcher, seq, attached FROM legacy.run_watchers \
                 WHERE run_id = ANY($1) ORDER BY run_id, watcher",
                &ids,
            )
            .await?
        {
            let watcher = Watcher {
                run: run_id(&row, 1)?,
                seq: Seq(u32_of(row.u64(2)?)?),
                attached: row.boolean(3)?,
            };
            runs[index(&row)?].watchers.push(watcher);
        }
        for row in self
            .rows(
                "SELECT r.run_id, p.digest, p.bytes, p.preview, p.value \
                 FROM legacy.payload_refs r \
                 JOIN legacy.payloads p ON p.tenant = r.tenant AND p.digest = r.digest \
                 WHERE r.run_id = ANY($1) ORDER BY r.run_id, p.digest",
                &ids,
            )
            .await?
        {
            let payload = PayloadRecord {
                digest: parse_digest(&row.bytes(1)?)?,
                size: row.u64(2)?,
                preview: row.text(3)?,
                value: row.opt_text(4)?.map(|json| value(&json)).transpose()?,
            };
            runs[index(&row)?].payloads.push(payload);
        }
        Ok(runs)
    }

    async fn rows(&self, sql: &str, ids: &[Uuid]) -> Result<Vec<PgRow>, Error> {
        Ok(sqlx::query(sql).bind(ids).fetch_all(&self.pool).await?)
    }
}

impl LegacyRuns for PostgresLegacy {
    async fn runs(
        &self,
        after: Option<RunId>,
        limit: usize,
    ) -> Result<Vec<LegacyRun>, LegacyError> {
        Ok(self.page(after, limit).await?)
    }

    async fn drop_tables(&self) -> Result<(), LegacyError> {
        sqlx::query("DROP SCHEMA IF EXISTS legacy CASCADE")
            .execute(&self.pool)
            .await
            .map_err(Error::from)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn park_kinds_read_the_refusal_of_a_401_as_one() {
        assert_eq!(
            park_kind("credentials_expired"),
            Ok(ParkKind::CredentialsRefused)
        );
        assert_eq!(park_kind("secret_missing"), Ok(ParkKind::SecretMissing));
        assert!(park_kind("lost").is_err());
        assert_eq!(quarantine_kind("failures"), Ok(QuarantineKind::Failures));
    }

    #[test]
    fn an_inbox_item_holds_a_trigger() {
        let fired =
            rewind_format::encode_event(&Event::Completion(rewind_core::Completion::TimerFired {
                seq: Seq(1),
            }));
        assert!(matches!(trigger(&fired), Ok(Trigger::Completion(_))));
    }
}
