//! The schema of `journal.db`, versioned: migrations are embedded in the binary, applied in
//! order when the database is opened, each in its own transaction with the new version. A
//! database whose version is above [`SCHEMA_VERSION`] was written by a newer build, and is
//! refused rather than misread.
//!
//! The tables are those of `archi/06-storage-and-scale.md`, with these differences:
//!
//! - every key starts with the tenant, except where a run id is enough: run ids are unique
//!   across tenants, so `runs` is keyed by `id` (and indexed by tenant);
//! - composite values are spread over columns (a run's quarantine, lease and trace context; a
//!   task's route, policy and state), so that the store can query them;
//! - a run keeps its oldest inbox item (`inbox_head`, received at `runnable_at`), and a task
//!   when it can be claimed (`due_at`): the picks of `acquire_runnable` and `claim_tool_tasks`
//!   read an index instead of grouping the inbox or scanning the tasks;
//! - `data_keys` holds each tenant's data key, encrypted by the master key, apart from
//!   `tenants`: a tenant without settings (the embedded `local`) still has secrets;
//! - `tool_calls.result_shape` holds the shape of a result whose value was projected, which
//!   `observed_shapes` merges per tool.

use turso::Connection;

use crate::Error;
use crate::sql::{self, Columns, opt_text, text};

/// The version of the schema this build writes.
pub const SCHEMA_VERSION: u32 = 1;

/// The `meta` key of the schema version.
const VERSION_KEY: &str = "schema_version";

/// Migration `n` brings the schema from version `n` to `n + 1`.
const MIGRATIONS: [&str; SCHEMA_VERSION as usize] = [V1];

const V1: &str = r#"
CREATE TABLE tenants (
    id TEXT PRIMARY KEY,
    active_run_quota INTEGER,
    journal_retention_ms INTEGER NOT NULL,
    archive_retention_ms INTEGER NOT NULL,
    read_model_retention_ms INTEGER NOT NULL,
    payload_grace_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE data_keys (
    tenant TEXT PRIMARY KEY,
    wrapped_key BLOB NOT NULL
) STRICT;

CREATE TABLE api_keys (
    hash TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    role TEXT NOT NULL,
    allowed_labels TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
) STRICT;

CREATE TABLE scripts (
    tenant TEXT NOT NULL,
    digest TEXT NOT NULL,
    source TEXT NOT NULL,
    transpiled TEXT NOT NULL,
    source_map TEXT NOT NULL,
    PRIMARY KEY (tenant, digest)
) STRICT;

CREATE TABLE toolsets (
    tenant TEXT NOT NULL,
    digest TEXT NOT NULL,
    spec TEXT NOT NULL,
    declarations TEXT NOT NULL,
    prompt TEXT NOT NULL,
    PRIMARY KEY (tenant, digest)
) STRICT;

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    script TEXT NOT NULL,
    toolset TEXT NOT NULL,
    status TEXT NOT NULL,
    epoch INTEGER NOT NULL,
    history_len INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    expiry_enqueued INTEGER NOT NULL,
    quarantine_kind TEXT,
    quarantine_reason TEXT,
    quarantine_since INTEGER,
    quarantine_retried_by TEXT,
    failures INTEGER NOT NULL,
    lease_owner TEXT,
    lease_until INTEGER,
    fence INTEGER NOT NULL,
    inbox_head INTEGER,
    runnable_at INTEGER,
    sticky_worker TEXT,
    labels TEXT NOT NULL,
    traceparent TEXT,
    tracestate TEXT,
    parent TEXT,
    idempotency_key TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    ended_at INTEGER,
    archive_document TEXT,
    archived_at INTEGER,
    UNIQUE (tenant, idempotency_key)
) STRICT;
CREATE INDEX runs_by_tenant ON runs (tenant, created_at, id);
CREATE INDEX runs_by_deadline ON runs (expires_at, id);
CREATE INDEX runs_by_end ON runs (ended_at, tenant, id);
CREATE INDEX runs_by_archive ON runs (archived_at, tenant, id);
CREATE INDEX runs_runnable ON runs (inbox_head);
CREATE INDEX runs_runnable_by_tenant ON runs (tenant, inbox_head);
CREATE INDEX runs_runnable_by_sticky_worker ON runs (sticky_worker, inbox_head);

CREATE TABLE history (
    run_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    recorded_at INTEGER NOT NULL,
    event TEXT NOT NULL,
    PRIMARY KEY (run_id, idx)
) STRICT;

CREATE TABLE inbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    event TEXT NOT NULL,
    received_at INTEGER NOT NULL
) STRICT;
CREATE INDEX inbox_by_run ON inbox (run_id, id);

CREATE TABLE signal_keys (
    run_id TEXT NOT NULL,
    key TEXT NOT NULL,
    PRIMARY KEY (run_id, key)
) STRICT;

CREATE TABLE timers (
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    fire_at INTEGER NOT NULL,
    PRIMARY KEY (run_id, seq)
) STRICT;
CREATE INDEX timers_by_fire_at ON timers (fire_at, run_id, seq);

CREATE TABLE tool_tasks (
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    tool TEXT NOT NULL,
    route TEXT NOT NULL,
    mcp_server TEXT,
    dispatched INTEGER NOT NULL,
    args_digest TEXT NOT NULL,
    args TEXT,
    on_interruption TEXT NOT NULL,
    approval INTEGER NOT NULL,
    execution_timeout_ms INTEGER,
    heartbeat_timeout_ms INTEGER NOT NULL,
    concurrency INTEGER NOT NULL,
    server_concurrency INTEGER,
    state TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    token TEXT UNIQUE,
    lease_owner TEXT,
    lease_until INTEGER,
    next_attempt_at INTEGER,
    due_at INTEGER,
    parked_kind TEXT,
    parked_reason TEXT,
    schema_change_approved INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (run_id, seq)
) STRICT;
CREATE INDEX tool_tasks_due ON tool_tasks (due_at, run_id, seq);
CREATE INDEX tool_tasks_deferred ON tool_tasks (state, lease_until, run_id, seq);
CREATE INDEX tool_tasks_by_owner ON tool_tasks (lease_owner, mcp_server);

CREATE TABLE webhook_tokens (
    token TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    name TEXT NOT NULL,
    UNIQUE (run_id, name)
) STRICT;

CREATE TABLE subscriptions (
    tenant TEXT NOT NULL,
    id TEXT NOT NULL,
    url TEXT NOT NULL,
    secret TEXT NOT NULL,
    events TEXT NOT NULL,
    labels TEXT NOT NULL,
    PRIMARY KEY (tenant, id)
) STRICT;

CREATE TABLE payloads (
    tenant TEXT NOT NULL,
    digest TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    preview TEXT NOT NULL,
    value TEXT,
    unreferenced_since INTEGER,
    PRIMARY KEY (tenant, digest)
) STRICT;

CREATE TABLE payload_refs (
    tenant TEXT NOT NULL,
    digest TEXT NOT NULL,
    run_id TEXT NOT NULL,
    PRIMARY KEY (tenant, digest, run_id)
) STRICT;
CREATE INDEX payload_refs_by_run ON payload_refs (run_id);

CREATE TABLE secrets (
    tenant TEXT NOT NULL,
    owner TEXT NOT NULL,
    ciphertext BLOB NOT NULL,
    PRIMARY KEY (tenant, owner)
) STRICT;

CREATE TABLE feed (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    consumer TEXT NOT NULL,
    tenant TEXT NOT NULL,
    run_id TEXT NOT NULL,
    fence INTEGER NOT NULL,
    from_idx INTEGER NOT NULL,
    to_idx INTEGER NOT NULL,
    lease_owner TEXT,
    lease_until INTEGER
) STRICT;
CREATE INDEX feed_by_consumer ON feed (consumer, id);
CREATE INDEX feed_by_run ON feed (run_id);

CREATE TABLE run_summaries (
    run_id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    script TEXT NOT NULL,
    toolset TEXT NOT NULL,
    status TEXT NOT NULL,
    labels TEXT NOT NULL,
    started_at INTEGER,
    ended_at INTEGER,
    last_activity_at INTEGER NOT NULL,
    tool_calls INTEGER NOT NULL,
    errors INTEGER NOT NULL,
    history_len INTEGER NOT NULL
) STRICT;
CREATE INDEX run_summaries_by_tenant ON run_summaries (tenant, run_id);

CREATE TABLE tool_calls (
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    tool TEXT,
    callsite_line INTEGER,
    scheduled_at INTEGER,
    completed_at INTEGER,
    outcome TEXT,
    error_kind TEXT,
    args_digest TEXT,
    result_digest TEXT,
    result_shape TEXT,
    PRIMARY KEY (run_id, seq)
) STRICT;
CREATE INDEX tool_calls_by_tool ON tool_calls (tool);
"#;

/// Brings the database's schema to [`SCHEMA_VERSION`], or refuses a newer one.
pub(crate) async fn migrate(connection: &Connection) -> Result<(), Error> {
    sql::execute(
        connection,
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT",
        Vec::new(),
    )
    .await?;
    let found = version(connection).await?;
    if found > SCHEMA_VERSION {
        return Err(Error::NewerSchema {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    for (from, migration) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let to = (from + 1).to_string();
        sql::transaction(connection, async {
            connection.execute_batch(migration).await?;
            set_meta(connection, VERSION_KEY, Some(&to)).await
        })
        .await?;
        tracing::info!(version = %to, "migrated the schema of the embedded store");
    }
    Ok(())
}

/// The schema version recorded in `meta`; 0 for a new database.
pub(crate) async fn version(connection: &Connection) -> Result<u32, Error> {
    match meta(connection, VERSION_KEY).await? {
        None => Ok(0),
        Some(value) => value
            .parse()
            .map_err(|_| Error::Corrupt(format!("schema version {value:?}"))),
    }
}

pub(crate) async fn meta(connection: &Connection, key: &str) -> Result<Option<String>, Error> {
    let row = sql::query_row(
        connection,
        "SELECT value FROM meta WHERE key = ?",
        vec![text(key)],
    )
    .await?;
    row.map(|row| row.text(0)).transpose()
}

pub(crate) async fn set_meta(
    connection: &Connection,
    key: &str,
    value: Option<&str>,
) -> Result<(), Error> {
    match value {
        Some(value) => {
            sql::execute(
                connection,
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)",
                vec![text(key), opt_text(Some(value))],
            )
            .await?
        }
        None => {
            sql::execute(
                connection,
                "DELETE FROM meta WHERE key = ?",
                vec![text(key)],
            )
            .await?
        }
    };
    Ok(())
}

/// The schema version is the store's own: the server cannot set it.
pub(crate) fn check_setting_name(name: &str) -> Result<(), Error> {
    if name == VERSION_KEY {
        return Err(Error::ReservedSetting(name.to_owned()));
    }
    Ok(())
}
