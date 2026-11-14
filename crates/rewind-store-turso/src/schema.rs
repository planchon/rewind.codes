//! The schema of `journal.db`, versioned: migrations are embedded in the binary, applied in
//! order when the database is opened, each in its own transaction with the new version. A
//! database whose version is above [`SCHEMA_VERSION`] was written by a newer build, and is
//! refused rather than misread.
//!
//! The tables are those of `archi/06-storage-and-scale.md`, with these differences:
//!
//! - every key starts with the tenant, except where a run id is enough: run ids are unique
//!   across tenants, so `runs` is keyed by `id` (and indexed by tenant);
//! - composite values are spread over columns (a run's quarantine, lease, trace context and
//!   fork parent; a task's route, policy and state), so that the store can query them;
//! - a run keeps its oldest inbox item (`inbox_head`, received at `runnable_at`), and a task
//!   when it can be claimed (`due_at`): the picks of `acquire_runnable` and `claim_tool_tasks`
//!   read an index instead of grouping the inbox or scanning the tasks;
//! - `data_keys` holds each tenant's data key, encrypted by the master key, apart from
//!   `tenants`: a tenant without settings (the embedded `local`) still has secrets;
//! - `tool_calls.result_shape` holds the shape of a result whose value was projected, which
//!   `observed_shapes` merges per tool;
//! - `roles.permissions` holds a role's permissions as a JSON array of strings, sorted, and
//!   `grants.principal` and `grants.granted_by` a principal's text (`user:…`, `key:…`,
//!   `invitation:…`), whose order is the order of principals.
//!
//! The read model's tables, written only through `ReadModel`:
//!
//! - `run_summaries` keeps what the entries give (status, start and end, counts, history
//!   length), the metadata of the latest `apply` (headline, labels as JSON, parent), and the
//!   kept snapshot's columns: its length (`snapshot_len`), mode and ending, the ending's
//!   `fingerprint` among them. `wakes_at` is the earliest `until` of its waits while the run is
//!   active, kept for the `wakes` order;
//! - `run_labels` holds one row per label, for equality, prefix and presence;
//! - `run_waits` holds the kept snapshot's waits by `position`, the snapshot's order, without
//!   task states;
//! - `run_texts` holds what free text is searched in, lowercase, one row per field (the run's
//!   id, the headline, each label's value, the ending's error and message): a field matches on
//!   its own, whatever characters a quoted query holds;
//! - `tool_calls.task_attempt` and `task_phase` hold the furthest state recorded for a call's
//!   task, and `run_quarantine` the latest quarantine record (`kind` null: a release), each by
//!   run id alone: both may be recorded before the run is projected.

use turso::Connection;

use crate::Error;
use crate::sql::{self, Columns, opt_text, text};

/// The version of the schema this build writes.
pub const SCHEMA_VERSION: u32 = 3;

/// The `meta` key of the schema version.
const VERSION_KEY: &str = "schema_version";

/// Migration `n` brings the schema from version `n` to `n + 1`.
const MIGRATIONS: [&str; SCHEMA_VERSION as usize] = [V1, V2, V3];

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
    attempts INTEGER NOT NULL,
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
    parent_at INTEGER,
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
    deletion_started_at INTEGER,
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
    headline TEXT NOT NULL,
    toolset TEXT NOT NULL,
    status TEXT NOT NULL,
    mode TEXT,
    labels TEXT NOT NULL,
    parent TEXT,
    started_at INTEGER,
    ended_at INTEGER,
    last_activity_at INTEGER NOT NULL,
    tool_calls INTEGER NOT NULL,
    errors INTEGER NOT NULL,
    history_len INTEGER NOT NULL,
    snapshot_len INTEGER NOT NULL,
    wakes_at INTEGER,
    ending_error TEXT,
    ending_message TEXT,
    ending_line INTEGER,
    ending_signature TEXT,
    fingerprint TEXT
) STRICT;
CREATE INDEX run_summaries_by_tenant ON run_summaries (tenant, run_id);
CREATE INDEX run_summaries_by_activity ON run_summaries (tenant, last_activity_at, run_id);

CREATE TABLE run_labels (
    tenant TEXT NOT NULL,
    run_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (run_id, key)
) STRICT;
CREATE INDEX run_labels_by_value ON run_labels (tenant, key, value);

CREATE TABLE run_waits (
    run_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    kind TEXT NOT NULL,
    seq INTEGER,
    name TEXT,
    since INTEGER NOT NULL,
    until INTEGER,
    callsite_line INTEGER,
    callsite_column INTEGER,
    PRIMARY KEY (run_id, position)
) STRICT;

CREATE TABLE run_texts (
    run_id TEXT NOT NULL,
    field INTEGER NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY (run_id, field)
) STRICT;

CREATE TABLE run_quarantine (
    run_id TEXT PRIMARY KEY,
    recorded_at INTEGER NOT NULL,
    kind TEXT,
    reason TEXT,
    since INTEGER
) STRICT;

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
    task_attempt INTEGER,
    task_phase TEXT,
    PRIMARY KEY (run_id, seq)
) STRICT;
CREATE INDEX tool_calls_by_tool ON tool_calls (tool);
"#;

/// Access (`AccessStore`): API keys gain a name and their creator, and what a key may do moves
/// to its grant, held by the principal `key:<hash>`. Each key of version 1 gets a grant of its
/// role (`admin`, the operator's before roles became data, becomes `operator`) with its allowed
/// labels, granted when the key was created, and the name `unnamed <role> key`.
const V2: &str = r#"
CREATE TABLE roles (
    tenant TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    permissions TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, name)
) STRICT;

CREATE TABLE grants (
    tenant TEXT NOT NULL,
    principal TEXT NOT NULL,
    role TEXT NOT NULL,
    allowed_labels TEXT NOT NULL,
    granted_by TEXT,
    granted_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, principal)
) STRICT;
CREATE INDEX grants_by_role ON grants (tenant, role);

CREATE TABLE access_settings (
    tenant TEXT PRIMARY KEY,
    default_role TEXT NOT NULL
) STRICT;

INSERT INTO grants (tenant, principal, role, allowed_labels, granted_by, granted_at)
SELECT tenant, 'key:' || hash, CASE role WHEN 'admin' THEN 'operator' ELSE role END,
       allowed_labels, NULL, created_at
FROM api_keys;

ALTER TABLE api_keys RENAME TO api_keys_v1;

CREATE TABLE api_keys (
    hash TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    name TEXT NOT NULL,
    created_by TEXT,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
) STRICT;
CREATE INDEX api_keys_by_tenant ON api_keys (tenant, created_at, hash);

INSERT INTO api_keys (hash, tenant, name, created_by, created_at, revoked_at)
SELECT hash, tenant, 'unnamed ' || CASE role WHEN 'admin' THEN 'operator' ELSE role END || ' key',
       NULL, created_at, revoked_at
FROM api_keys_v1;

DROP TABLE api_keys_v1;
"#;

/// Namespaces (`archi/06-storage-and-scale.md`, "Namespaces"): the end user a run belongs to. The
/// runs that existed before namespaces belong to `migrated`.
const V3: &str = r#"
ALTER TABLE runs ADD COLUMN namespace TEXT NOT NULL DEFAULT 'migrated';
ALTER TABLE run_summaries ADD COLUMN namespace TEXT NOT NULL DEFAULT 'migrated';
"#;

/// Brings the database's schema to [`SCHEMA_VERSION`], or refuses a newer one.
pub(crate) async fn migrate(connection: &Connection) -> Result<(), Error> {
    migrate_to(connection, SCHEMA_VERSION).await
}

/// Brings the database's schema to `target`, at most [`SCHEMA_VERSION`], or refuses a newer one.
async fn migrate_to(connection: &Connection, target: u32) -> Result<(), Error> {
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
    let migrations = MIGRATIONS.iter().take(target as usize);
    for (from, migration) in migrations.enumerate().skip(found as usize) {
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rewind_core::{Digest, Timestamp};
    use rewind_engine::ports::{AccessStore, ApiKey, Grant, Principal, TenantId};

    use super::*;
    use crate::codec::encode_labels;
    use crate::sql::{digest, opt_ts, ts};
    use crate::{MasterKey, TursoStore};

    const T0: Timestamp = Timestamp(1_767_225_600_000);

    fn at(seconds: u64) -> Timestamp {
        Timestamp(T0.0 + seconds * 1_000)
    }

    fn tenant_id(id: &str) -> TenantId {
        TenantId::new(id).expect("tenant")
    }

    /// A key as version 1 kept it: its role and allowed labels on the key itself.
    struct KeyOfVersion1 {
        secret: &'static str,
        tenant: &'static str,
        role: &'static str,
        labels: &'static [(&'static str, &'static str)],
        created_at: Timestamp,
        revoked_at: Option<Timestamp>,
    }

    fn labels(labels: &[(&str, &str)]) -> BTreeMap<String, String> {
        labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn keys_of_version_1_become_keys_and_grants() {
        let keys = [
            KeyOfVersion1 {
                secret: "rewind_sk_admin",
                tenant: "acme",
                role: "admin",
                labels: &[],
                created_at: T0,
                revoked_at: None,
            },
            KeyOfVersion1 {
                secret: "rewind_sk_agent",
                tenant: "acme",
                role: "harness",
                labels: &[("conversation", "c-1")],
                created_at: at(1),
                revoked_at: None,
            },
            KeyOfVersion1 {
                secret: "rewind_sk_gone",
                tenant: "beta",
                role: "reader",
                labels: &[],
                created_at: at(2),
                revoked_at: Some(at(3)),
            },
        ];
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let path = dir.path().join("journal.db");
        {
            let database = turso::Builder::new_local(path.to_str().expect("UTF-8 path"))
                .build()
                .await
                .expect("open the file");
            let connection = database.connect().expect("connect");
            migrate_to(&connection, 1).await.expect("version 1");
            for key in &keys {
                sql::execute(
                    &connection,
                    "INSERT INTO api_keys (hash, tenant, role, allowed_labels, created_at, \
                     revoked_at) VALUES (?, ?, ?, ?, ?, ?)",
                    vec![
                        digest(Digest::of(key.secret.as_bytes())),
                        text(key.tenant),
                        text(key.role),
                        text(encode_labels(&labels(key.labels))),
                        ts(key.created_at),
                        opt_ts(key.revoked_at),
                    ],
                )
                .await
                .expect("a key of version 1");
            }
        }

        let store = TursoStore::open(&path, MasterKey::from_bytes([1; 32]))
            .await
            .expect("migrate to the current version");
        assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
        for (key, role) in keys.iter().zip(["operator", "harness", "reader"]) {
            let hash = Digest::of(key.secret.as_bytes());
            let tenant = tenant_id(key.tenant);
            assert_eq!(
                store.find_key(hash).await.expect("find_key"),
                Some(ApiKey {
                    hash,
                    tenant: tenant.clone(),
                    name: format!("unnamed {role} key"),
                    created_by: None,
                    created_at: key.created_at,
                    revoked_at: key.revoked_at,
                }),
                "{}",
                key.secret
            );
            assert_eq!(
                store
                    .grant(&tenant, &Principal::Key(hash))
                    .await
                    .expect("grant"),
                Some(Grant {
                    tenant,
                    principal: Principal::Key(hash),
                    role: role.to_owned(),
                    allowed_labels: labels(key.labels),
                    granted_by: None,
                    granted_at: key.created_at,
                }),
                "{}",
                key.secret
            );
        }
        let acme: Vec<Digest> = store
            .list_keys(&tenant_id("acme"))
            .await
            .expect("list_keys")
            .into_iter()
            .map(|key| key.hash)
            .collect();
        assert_eq!(
            acme,
            [
                Digest::of(b"rewind_sk_admin"),
                Digest::of(b"rewind_sk_agent")
            ]
        );
    }
}
