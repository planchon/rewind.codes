//! The runs of the tables `legacy_*` (schema version 13), which `rewind server migrate-runs`
//! moves onto the run plane (`archi/08-decisions.md`, "Taken while writing the object-storage
//! spec": the migration): [`TursoLegacy`], the `LegacyRuns` of the engine.
//!
//! The rows are read as they were written: events and inbox items in the stored format v1
//! (`rewind-format`), payload values in their canonical JSON, digests in hex, labels, scopes and
//! attachments as JSON objects, booleans as integers, times in milliseconds.

use rewind_core::{
    Attachments, Digest, Epoch, Event, HistoryEvent, JsonValue, Lifecycle, Payload, RunId,
    SecretRef, Seq,
};
use rewind_engine::ports::legacy::{
    LegacyError, LegacyHistory, LegacyRun, LegacyRuns, LegacyTask, LegacyWebhook,
};
use rewind_engine::ports::{
    ForkSource, InboxItem, NamespaceName, ParkKind, ParkReason, PayloadRecord, Quarantine,
    QuarantineKind, RunMetadata, RunRef, RunWorkflow, StartedBy, TaskState, TraceContext, Trigger,
    Watcher,
};
use turso::{Connection, Row, Value};

use crate::codec::{decode_labels, decode_value};
use crate::sql::{self, Columns, limit, run_id};
use crate::{Error, TursoStore};

/// The runs of the tables `legacy_*`, read once by `migrate-runs`.
#[derive(Debug, Clone)]
pub struct TursoLegacy {
    store: TursoStore,
}

impl TursoStore {
    /// The runs this store's database set aside in the tables `legacy_*`.
    pub fn legacy(&self) -> TursoLegacy {
        TursoLegacy {
            store: self.clone(),
        }
    }
}

/// The tables version 13 set aside, in an order that drops nothing another still names.
const TABLES: [&str; 20] = [
    "legacy_journal_runs",
    "legacy_history",
    "legacy_inbox",
    "legacy_signal_keys",
    "legacy_timers",
    "legacy_tool_tasks",
    "legacy_webhook_tokens",
    "legacy_run_watchers",
    "legacy_payloads",
    "legacy_payload_refs",
    "legacy_feed",
    "legacy_run_summaries",
    "legacy_run_labels",
    "legacy_run_waits",
    "legacy_run_texts",
    "legacy_run_quarantine",
    "legacy_tool_calls",
    "legacy_usage",
    "legacy_quota_alerts",
    "legacy_namespace_scripts",
];

impl From<Error> for LegacyError {
    fn from(error: Error) -> Self {
        match error {
            Error::Corrupt(message) => LegacyError::Corrupt(message),
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

fn value(json: &str) -> Result<JsonValue, Error> {
    decode_value(json)
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
    failures, attempts, labels, traceparent, tracestate, parent, parent_at, idempotency_key, \
    created_at, updated_at, ended_at, archive_document, workflow, workflow_version, started_by, \
    started_seq, detached, scope, secrets, origin, suffix_len";

fn run_from_row(row: &Row) -> Result<LegacyRun, Error> {
    let id = sql::parse_run_id(&row.text(0)?)?;
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
        Some(run) => Some(ForkSource {
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
        suffix_len: row.suffix_len(32)?,
        code: row.digest(3)?,
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
            source: parent,
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
            None => LegacyHistory::Events(Vec::new()),
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

fn task_from_row(row: &Row) -> Result<LegacyTask, Error> {
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

/// Fills a run with the rows of the tables it holds rows in.
async fn fill(connection: &Connection, run: &mut LegacyRun) -> Result<(), Error> {
    let id = || vec![run_id(run.run.id)];
    let rows = sql::query(
        connection,
        "SELECT idx, recorded_at, event FROM legacy_history WHERE run_id = ? ORDER BY idx",
        id(),
    )
    .await?;
    if !rows.is_empty() {
        let LegacyHistory::Events(events) = &mut run.history else {
            return Err(corrupt(format!(
                "run {} is archived and still has entries",
                run.run.id
            )));
        };
        for row in rows {
            events.push(HistoryEvent {
                index: row.u64(0)?,
                recorded_at: row.ts(1)?,
                event: event(&row.text(2)?)?,
            });
        }
    }
    for row in sql::query(
        connection,
        "SELECT id, event, received_at FROM legacy_inbox WHERE run_id = ? ORDER BY id",
        id(),
    )
    .await?
    {
        run.inbox.push(InboxItem {
            id: row.u64(0)?,
            trigger: trigger(&row.text(1)?)?,
            received_at: row.ts(2)?,
        });
    }
    for row in sql::query(
        connection,
        "SELECT key FROM legacy_signal_keys WHERE run_id = ? ORDER BY key",
        id(),
    )
    .await?
    {
        run.signal_keys.push(row.text(0)?);
    }
    for row in sql::query(
        connection,
        "SELECT seq, fire_at FROM legacy_timers WHERE run_id = ? ORDER BY seq",
        id(),
    )
    .await?
    {
        run.timers.push((Seq(u32_of(row.u64(0)?)?), row.ts(1)?));
    }
    for row in sql::query(
        connection,
        "SELECT seq, tool, args_digest, args, state, lease_owner, lease_until, next_attempt_at, \
         parked_kind, parked_reason, attempt, schema_change_approved, created_at \
         FROM legacy_tool_tasks WHERE run_id = ? ORDER BY seq",
        id(),
    )
    .await?
    {
        run.tasks.push(task_from_row(&row)?);
    }
    for row in sql::query(
        connection,
        "SELECT name, token FROM legacy_webhook_tokens WHERE run_id = ? ORDER BY name",
        id(),
    )
    .await?
    {
        run.webhooks.push(LegacyWebhook {
            name: row.text(0)?,
            hash: Digest::of(row.text(1)?.as_bytes()),
        });
    }
    for row in sql::query(
        connection,
        "SELECT watcher, seq, attached FROM legacy_run_watchers WHERE run_id = ? \
         ORDER BY watcher",
        id(),
    )
    .await?
    {
        run.watchers.push(Watcher {
            run: sql::parse_run_id(&row.text(0)?)?,
            seq: Seq(u32_of(row.u64(1)?)?),
            attached: row.boolean(2)?,
        });
    }
    for row in sql::query(
        connection,
        "SELECT p.digest, p.bytes, p.preview, p.value FROM legacy_payload_refs r \
         JOIN legacy_payloads p ON p.tenant = r.tenant AND p.digest = r.digest \
         WHERE r.run_id = ? ORDER BY p.digest",
        id(),
    )
    .await?
    {
        run.payloads.push(PayloadRecord {
            digest: row.digest(0)?,
            size: row.u64(1)?,
            preview: row.text(2)?,
            value: row.opt_text(3)?.map(|json| value(&json)).transpose()?,
        });
    }
    Ok(())
}

impl TursoLegacy {
    async fn page(&self, after: Option<RunId>, count: usize) -> Result<Vec<LegacyRun>, Error> {
        let connection = self.store.connection().await?;
        let present = sql::query_row(
            &connection,
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'legacy_journal_runs'",
            Vec::new(),
        )
        .await?
        .is_some();
        if !present || count == 0 {
            return Ok(Vec::new());
        }
        let rows = sql::query(
            &connection,
            &format!(
                "SELECT {RUN_COLUMNS} FROM legacy_journal_runs WHERE ? IS NULL OR id > ? \
                 ORDER BY id LIMIT ?"
            ),
            vec![
                after.map_or(Value::Null, run_id),
                after.map_or(Value::Null, run_id),
                limit(count),
            ],
        )
        .await?;
        let mut runs = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut run = run_from_row(row)?;
            fill(&connection, &mut run).await?;
            runs.push(run);
        }
        Ok(runs)
    }

    async fn drop_all(&self) -> Result<(), Error> {
        let connection = self.store.connection().await?;
        sql::transaction(&connection, async {
            for table in TABLES {
                sql::execute(
                    &connection,
                    &format!("DROP TABLE IF EXISTS {table}"),
                    Vec::new(),
                )
                .await?;
            }
            Ok::<_, Error>(())
        })
        .await
    }
}

impl LegacyRuns for TursoLegacy {
    async fn runs(
        &self,
        after: Option<RunId>,
        limit: usize,
    ) -> Result<Vec<LegacyRun>, LegacyError> {
        Ok(self.page(after, limit).await?)
    }

    async fn drop_tables(&self) -> Result<(), LegacyError> {
        Ok(self.drop_all().await?)
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
        assert!(park_kind("lost").is_err());
        assert_eq!(
            quarantine_kind("non_determinism"),
            Ok(QuarantineKind::NonDeterminism)
        );
    }

    #[tokio::test]
    async fn a_store_without_runs_reads_none_and_drops_its_tables_twice() {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let store = TursoStore::open(
            dir.path().join("control.db"),
            crate::MasterKey::from_bytes([1; 32]),
        )
        .await
        .expect("open");
        let legacy = store.legacy();
        assert_eq!(legacy.runs(None, 10).await, Ok(Vec::new()));
        legacy.drop_tables().await.expect("drop");
        legacy.drop_tables().await.expect("drop again");
        assert_eq!(legacy.runs(None, 10).await, Ok(Vec::new()));
    }
}
