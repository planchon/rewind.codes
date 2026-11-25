//! `CatalogStore` on Postgres: the control plane's catalog (`archi/07-code-architecture.md`,
//! "`CatalogStore`"; `archi/06-storage-and-scale.md`, "The control plane"). The in-memory
//! `MemoryCatalog` of `rewind-engine` is the reference of every behavior here; both pass the same
//! conformance suite.
//!
//! The catalog keeps code, action sets, workflows (`workflows`) with their versions
//! (`workflow_versions`), the codes each scope has seen (`workflow_codes`) and their suites
//! (`workflow_tests`), tests, tenants, namespaces with each one's `partition`, and subscriptions.
//! A workflow's scope is its `namespace` column, `*` for the organization's. Every write is one
//! transaction that sends what it changed with `NOTIFY` (`crate::changes`).
//!
//! A write of a workflow locks its row (`FOR UPDATE`): its versions take their numbers, and its
//! suite and scope change, one at a time. A write that adds a code to a scope (a workflow created,
//! a version) takes a transaction-scoped advisory lock on that code in that scope: a workflow
//! created without a name for a code its scope already has finds the workflow that has it. Test
//! rows are locked by tenant and digest: a write of a suite locks the rows of the tests joining it
//! (`FOR SHARE`) until it has written its references, and the cleanup marks only rows it holds
//! (`FOR UPDATE SKIP LOCKED`).
//!
//! The labels a caller is limited to are carried by a row whose labels hold each of them:
//! `labels @> $caller::jsonb`.
//!
//! `ensure_namespace` reads first and writes only a namespace it creates, or a row without a
//! partition, which it places: starting a run writes nothing here. Rows without a partition are
//! those created before the column; [`PostgresCatalog::place_namespaces`] places them all once
//! (`archi/08-decisions.md`, "Taken while building the control plane of the run plane").

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;

use rewind_core::{
    ActionSet, Code, Digest, PartitionId, RunId, Suite, Timestamp, WorkflowId, WorkflowVersion,
};
use rewind_engine::ports::catalog::{NamespaceFilter, NamespacePage};
use rewind_engine::ports::{
    ActionDeclarations, CatalogDocuments, CatalogError, CatalogMigration, CatalogStore, CodeTitle,
    CompiledCode, ControlPlaneChange, DeleteNamespace, DisplayName, DropNamespace, Labels,
    Namespace, NamespaceName, NamespaceWrite, NewVersion, NewWorkflow, Quotas, RawDocument,
    RawVersion, RunRef, ScopeFilter, Secret, StoredActionSet, StoredCode, StoredVersion,
    StoredWorkflow, Subscription, SuiteChange, SuiteMember, TEST_DELETION_TIMEOUT, TEST_GRACE,
    Tenant, TenantId, TestRecord, VersionRef, WorkflowCreated, WorkflowFilter, WorkflowPatch,
    WorkflowScope, WorkflowSource, WorkflowWrite, namespace_partition,
};
use sqlx::postgres::{PgQueryResult, PgRow};
use sqlx::{PgConnection, PgPool};

use crate::changes::notify;
use crate::codec::{decode_event_types, decode_labels, encode_event_types, encode_labels};
use crate::namespaces::{
    self as tenancy, display_name_of, name_of, quota_columns, quota_values, quotas_from_row,
};
use crate::sql::{Columns, Failure, bytes, int, limit, ms, retrying, tenant, ts};
use crate::{Error, PostgresStore};

type Fail = Failure<CatalogError>;

impl From<Fail> for CatalogError {
    fn from(failure: Fail) -> Self {
        match failure {
            Failure::Port(err) => err,
            Failure::Db(err) => err.into(),
        }
    }
}

impl From<Error> for CatalogError {
    fn from(err: Error) -> Self {
        match err {
            Error::Database {
                code: Some(ref code),
                ..
            } if code == "23505" => CatalogError::Conflict(err.to_string()),
            Error::Unreachable(_) => CatalogError::Unavailable(err.to_string()),
            other => CatalogError::Backend(other.to_string()),
        }
    }
}

impl From<CatalogError> for Fail {
    fn from(err: CatalogError) -> Self {
        Failure::Port(err)
    }
}

/// `CatalogStore` on the database of a [`PostgresStore`], whose pool it shares
/// ([`PostgresStore::catalog`]).
#[derive(Clone)]
pub struct PostgresCatalog {
    pool: PgPool,
}

impl std::fmt::Debug for PostgresCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresCatalog").finish_non_exhaustive()
    }
}

impl PostgresStore {
    /// The control plane's catalog on this store's database.
    pub fn catalog(&self) -> PostgresCatalog {
        PostgresCatalog {
            pool: self.pool.clone(),
        }
    }
}

/// The columns of a [`StoredWorkflow`] of the row `w`, read by [`workflow_from_row`].
macro_rules! workflow_columns {
    () => {
        "w.id, w.namespace, w.name, w.description, w.labels::text, w.source_workflow, \
         w.source_version, w.created_by, w.created_at, w.promoted_at, w.latest"
    };
}

/// How many columns [`workflow_columns!`] names.
const WORKFLOW_COLUMNS: usize = 11;

/// The columns of a [`StoredVersion`] of the row `v`, read by [`version_from_row`].
macro_rules! version_columns {
    () => {
        "v.workflow, v.version, v.digest, v.document, v.labels::text, v.created_at"
    };
}

/// The `namespace` of the organization's workflows, which is no namespace's name.
const ORGANIZATION: &str = "*";

/// A test (`t`) the cleanup may start deleting at `$1`: stored or reserved at least its grace
/// (`$2`) before, not being deleted, or for at least `$3`, and referenced by no suite, as a
/// current or a former member.
macro_rules! test_due {
    () => {
        "t.reserved_at <= $1 - $2 \
         AND (t.deletion_started_at IS NULL OR t.deletion_started_at <= $1 - $3) \
         AND NOT EXISTS (SELECT 1 FROM workflow_tests w \
                         WHERE w.tenant = t.tenant AND w.digest = t.digest)"
    };
}

fn workflow_id(text: String) -> Result<WorkflowId, Error> {
    text.parse()
        .map_err(|err: rewind_core::InvalidWorkflowId| Error::Corrupt(err.to_string()))
}

fn scope_of(namespace: String) -> Result<WorkflowScope, Error> {
    if namespace == ORGANIZATION {
        Ok(WorkflowScope::Organization)
    } else {
        name_of(namespace).map(WorkflowScope::Namespace)
    }
}

/// The workflow whose [`workflow_columns!`] start at column `first`.
fn workflow_from_row(row: &PgRow, first: usize) -> Result<StoredWorkflow, Error> {
    let source = match row.opt_text(first + 5)? {
        None => None,
        Some(workflow) => Some(WorkflowSource {
            workflow: workflow_id(workflow)?,
            version: row.u32(first + 6)?,
        }),
    };
    Ok(StoredWorkflow {
        id: workflow_id(row.text(first)?)?,
        scope: scope_of(row.text(first + 1)?)?,
        name: row.opt_text(first + 2)?,
        description: row.opt_text(first + 3)?,
        labels: decode_labels(&row.text(first + 4)?)?,
        source,
        created_by: row.text(first + 7)?,
        created_at: row.ts(first + 8)?,
        promoted_at: row.opt_ts(first + 9)?,
        latest: row.u32(first + 10)?,
    })
}

/// The version whose [`version_columns!`] start at column `first`. Its document hashes to its
/// digest.
fn version_from_row(row: &PgRow, first: usize) -> Result<StoredVersion, Error> {
    let workflow = workflow_id(row.text(first)?)?;
    let version = row.u32(first + 1)?;
    let digest = row.digest(first + 2)?;
    let document = rewind_format::decode_workflow_version(&row.text(first + 3)?)
        .map_err(|err| Error::Corrupt(format!("version {version} of {workflow}: {err}")))?;
    if document.digest() != digest {
        return Err(Error::Corrupt(format!(
            "version {version} of {workflow} does not hash to its digest {digest}"
        )));
    }
    Ok(StoredVersion {
        workflow,
        version,
        document,
        digest,
        labels: decode_labels(&row.text(first + 4)?)?,
        created_at: row.ts(first + 5)?,
    })
}

fn suite_member_from_row(row: &PgRow) -> Result<SuiteMember, Error> {
    Ok(SuiteMember {
        name: row.text(0)?,
        digest: row.digest(1)?,
        added_at: row.ts(2)?,
        retired_at: row.opt_ts(3)?,
        reason: row.opt_text(4)?,
    })
}

fn action_set_from_row(
    row: &PgRow,
    digest: Digest,
    first: usize,
) -> Result<StoredActionSet, Error> {
    let action_set = rewind_format::decode_action_set(&row.text(first)?)
        .map_err(|err| Error::Corrupt(format!("action set {digest}: {err}")))?;
    if action_set.digest() != digest {
        return Err(Error::Corrupt(format!(
            "action set {digest} does not hash to its key"
        )));
    }
    Ok(StoredActionSet {
        action_set,
        declarations: ActionDeclarations {
            typescript: row.text(first + 1)?,
            prompt: row.text(first + 2)?,
        },
    })
}

/// A suite given a test the catalog cannot keep for it.
fn missing_test(test: Digest) -> CatalogError {
    CatalogError::Conflict(format!(
        "the test {test} has no row, or its deletion has started: reserve it and write its blob \
         again before giving it to a suite"
    ))
}

fn no_workflow(id: &WorkflowId) -> CatalogError {
    CatalogError::Conflict(format!("the workflow {id} does not exist"))
}

fn workflow_changed(tenant_id: &TenantId, id: &WorkflowId) -> ControlPlaneChange {
    ControlPlaneChange::Workflow {
        tenant: tenant_id.clone(),
        id: id.clone(),
    }
}

/// The tenants and digests of `keys`, sorted (the order rows are locked in), to bind as arrays.
fn sorted_keys(keys: &[(TenantId, Digest)]) -> (Vec<&str>, Vec<Vec<u8>>) {
    let sorted: BTreeSet<&(TenantId, Digest)> = keys.iter().collect();
    sorted
        .into_iter()
        .map(|(owner, digest)| (tenant(owner), bytes(*digest)))
        .unzip()
}

fn test_changed(tenant_id: &TenantId, digest: Digest) -> ControlPlaneChange {
    ControlPlaneChange::Test {
        tenant: tenant_id.clone(),
        digest,
    }
}

fn namespace_changed(tenant_id: &TenantId, name: &NamespaceName) -> ControlPlaneChange {
    ControlPlaneChange::Namespace {
        tenant: tenant_id.clone(),
        name: name.clone(),
    }
}

/// The `(tenant, digest)` pairs of a statement's `RETURNING tenant, digest`.
fn keys_of(rows: &[PgRow]) -> Result<Vec<(TenantId, Digest)>, Error> {
    rows.iter()
        .map(|row| Ok((row.tenant(0)?, row.digest(1)?)))
        .collect()
}

// -------------------------------------------------------------------------------------------
// Workflows

/// The workflow, its row locked until the transaction ends.
async fn lock_workflow(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    id: &WorkflowId,
) -> Result<Option<StoredWorkflow>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        workflow_columns!(),
        " FROM workflows w WHERE w.tenant = $1 AND w.id = $2 FOR UPDATE"
    ))
    .bind(tenant(tenant_id))
    .bind(id.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| workflow_from_row(&row, 0)).transpose()
}

/// Takes the lock of `code` in `scope` until the transaction ends: the writes that add it to the
/// scope's codes, and the creations that look for it there, run one at a time.
async fn lock_code(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    scope: &WorkflowScope,
    code: Digest,
) -> Result<(), Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!(
            "rewind workflow code\u{1f}{tenant_id}\u{1f}{scope}\u{1f}{}",
            code.to_hex()
        ))
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Locks the rows of the tests joining a suite, by digest, until the transaction ends: no cleanup
/// marks them before the suite references them. A test without a row, or whose deletion has
/// started, refuses the write.
async fn lock_tests(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    tests: impl IntoIterator<Item = Digest>,
) -> Result<(), Fail> {
    let tests: BTreeSet<Digest> = tests.into_iter().collect();
    if tests.is_empty() {
        return Ok(());
    }
    let digests: Vec<Vec<u8>> = tests.iter().map(|test| bytes(*test)).collect();
    let rows = sqlx::query(
        "SELECT digest FROM tests WHERE tenant = $1 AND digest = ANY($2) \
         AND deletion_started_at IS NULL ORDER BY digest FOR SHARE",
    )
    .bind(tenant(tenant_id))
    .bind(digests)
    .fetch_all(&mut *conn)
    .await?;
    let live: BTreeSet<Digest> = rows
        .iter()
        .map(|row| row.digest(0))
        .collect::<Result<_, Error>>()?;
    match tests.iter().find(|test| !live.contains(test)) {
        Some(missing) => Err(Failure::Port(missing_test(*missing))),
        None => Ok(()),
    }
}

/// Stores version `number` of the workflow `id`.
async fn insert_version(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    id: &WorkflowId,
    number: u32,
    document: &WorkflowVersion,
    now: Timestamp,
) -> Result<StoredVersion, Error> {
    let digest = document.digest();
    sqlx::query(
        "INSERT INTO workflow_versions (tenant, workflow, version, digest, code, document, \
             labels, created_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8)",
    )
    .bind(tenant(tenant_id))
    .bind(id.as_str())
    .bind(i64::from(number))
    .bind(bytes(digest))
    .bind(bytes(document.code()))
    .bind(rewind_format::encode_workflow_version(document))
    .bind(encode_labels(document.labels()))
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    Ok(StoredVersion {
        workflow: id.clone(),
        version: number,
        document: document.clone(),
        digest,
        labels: document.labels().clone(),
        created_at: now,
    })
}

/// Adds `code` to the codes of `scope` under `labels`, as the workflow `id`'s, unless the scope
/// has it under those labels already.
async fn add_code(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    scope: &WorkflowScope,
    document: &WorkflowVersion,
    id: &WorkflowId,
    now: Timestamp,
) -> Result<(), Error> {
    sqlx::query(
        "INSERT INTO workflow_codes (tenant, namespace, labels, code, workflow, added_at) \
         VALUES ($1, $2, $3::jsonb, $4, $5, $6) ON CONFLICT DO NOTHING",
    )
    .bind(tenant(tenant_id))
    .bind(scope.to_string())
    .bind(encode_labels(document.labels()))
    .bind(bytes(document.code()))
    .bind(id.as_str())
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The workflow that first had `code` in the first of `scopes` that has it under labels carrying
/// `labels`, and its latest version with that code carrying them.
async fn with_code(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    scopes: &[String],
    code: Digest,
    labels: &Labels,
) -> Result<Option<(StoredWorkflow, StoredVersion)>, Error> {
    let row = sqlx::query(concat!(
        "SELECT ",
        workflow_columns!(),
        ", ",
        version_columns!(),
        " FROM workflow_codes c \
         JOIN workflows w ON w.tenant = c.tenant AND w.id = c.workflow \
         CROSS JOIN LATERAL ( \
             SELECT * FROM workflow_versions v \
             WHERE v.tenant = c.tenant AND v.workflow = c.workflow AND v.code = c.code \
             AND v.labels @> $4::jsonb \
             ORDER BY v.version DESC LIMIT 1) v \
         WHERE c.tenant = $1 AND c.code = $3 AND c.namespace = ANY($2) \
         AND c.labels @> $4::jsonb \
         ORDER BY array_position($2, c.namespace::text), c.ordinal LIMIT 1"
    ))
    .bind(tenant(tenant_id))
    .bind(scopes)
    .bind(bytes(code))
    .bind(encode_labels(labels))
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| {
        Ok((
            workflow_from_row(&row, 0)?,
            version_from_row(&row, WORKFLOW_COLUMNS)?,
        ))
    })
    .transpose()
}

/// Whether `scope` has a workflow named `name`.
async fn named_in(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    scope: &WorkflowScope,
    name: &str,
) -> Result<bool, Error> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workflows WHERE tenant = $1 AND namespace = $2 AND name = $3)",
    )
    .bind(tenant(tenant_id))
    .bind(scope.to_string())
    .bind(name)
    .fetch_one(&mut *conn)
    .await?)
}

/// Whether a write of a workflow's name or scope was refused because the scope has a workflow
/// of that name: the only unique key such a write can break.
fn name_taken(written: Result<PgQueryResult, sqlx::Error>) -> Result<bool, Error> {
    match written.map_err(Error::from) {
        Ok(_) => Ok(false),
        Err(err) if err.is_unique_violation() => Ok(true),
        Err(err) => Err(err),
    }
}

/// Every test a workflow's suite holds or held, by name, then when it joined.
async fn suite_of(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    id: &WorkflowId,
) -> Result<Vec<SuiteMember>, Error> {
    let rows = sqlx::query(
        "SELECT name, digest, added_at, retired_at, reason FROM workflow_tests \
         WHERE tenant = $1 AND workflow = $2 ORDER BY name, added_at, digest",
    )
    .bind(tenant(tenant_id))
    .bind(id.as_str())
    .fetch_all(&mut *conn)
    .await?;
    rows.iter().map(suite_member_from_row).collect()
}

/// The tests a suite holds now, by name and digest.
async fn current_members(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    id: &WorkflowId,
) -> Result<BTreeSet<(String, Digest)>, Error> {
    let rows = sqlx::query(
        "SELECT name, digest FROM workflow_tests \
         WHERE tenant = $1 AND workflow = $2 AND retired_at IS NULL",
    )
    .bind(tenant(tenant_id))
    .bind(id.as_str())
    .fetch_all(&mut *conn)
    .await?;
    rows.iter()
        .map(|row| Ok((row.text(0)?, row.digest(1)?)))
        .collect()
}

/// Makes each `(name, digest)` of `tests` a member of the suite: a test that left under that name
/// and digest joins again at `now`; one in it stays as it is.
async fn join(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    id: &WorkflowId,
    tests: &BTreeSet<(&str, Digest)>,
    now: Timestamp,
) -> Result<(), Error> {
    if tests.is_empty() {
        return Ok(());
    }
    let (names, digests): (Vec<&str>, Vec<Vec<u8>>) = tests
        .iter()
        .map(|(name, digest)| (*name, bytes(*digest)))
        .unzip();
    sqlx::query(
        "INSERT INTO workflow_tests (tenant, workflow, name, digest, added_at) \
         SELECT $1, $2, name, digest, $5 FROM unnest($3::text[], $4::bytea[]) AS t(name, digest) \
         ON CONFLICT (tenant, workflow, name, digest) DO UPDATE \
         SET added_at = EXCLUDED.added_at, retired_at = NULL, reason = NULL \
         WHERE workflow_tests.retired_at IS NOT NULL",
    )
    .bind(tenant(tenant_id))
    .bind(id.as_str())
    .bind(names)
    .bind(digests)
    .bind(ts(now))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Retires the members `(name, digest)` of the suite still in it at `now`, each for its reason.
async fn retire(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    id: &WorkflowId,
    leaving: &[(&str, Digest, Option<&str>)],
    now: Timestamp,
) -> Result<(), Error> {
    if leaving.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE workflow_tests w SET retired_at = $3, reason = l.reason \
         FROM unnest($4::text[], $5::bytea[], $6::text[]) AS l(name, digest, reason) \
         WHERE w.tenant = $1 AND w.workflow = $2 AND w.name = l.name AND w.digest = l.digest \
         AND w.retired_at IS NULL",
    )
    .bind(tenant(tenant_id))
    .bind(id.as_str())
    .bind(ts(now))
    .bind(leaving.iter().map(|(name, _, _)| *name).collect::<Vec<_>>())
    .bind(
        leaving
            .iter()
            .map(|(_, digest, _)| bytes(*digest))
            .collect::<Vec<_>>(),
    )
    .bind(
        leaving
            .iter()
            .map(|(_, _, reason)| *reason)
            .collect::<Vec<_>>(),
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The tests of a suite, by name and digest.
fn suite_tests(suite: &Suite) -> BTreeSet<(&str, Digest)> {
    suite
        .tests
        .iter()
        .map(|test| (test.name.as_str(), test.test))
        .collect()
}

/// Why a version's suite no longer holds its workflow's test `name`: the reason it retired it,
/// else the reason it replaced it.
fn reason_left<'a>(suite: &'a Suite, name: &str) -> Option<&'a str> {
    suite
        .retired
        .iter()
        .find(|retired| retired.name == name)
        .map(|retired| retired.reason.as_str())
        .or_else(|| {
            suite
                .replaced
                .iter()
                .find(|replaced| replaced.name == name)
                .map(|replaced| replaced.reason.as_str())
        })
}

// -------------------------------------------------------------------------------------------
// Namespaces

/// The columns of a [`Namespace`], read by [`namespace_from_row`].
fn namespace_columns() -> String {
    format!(
        "tenant, name, {}, tokens_revoked_before, created_at, last_active_at, deleting_since, \
         display_name, partition::bigint",
        quota_columns("")
    )
}

fn namespace_from_row(row: &PgRow) -> Result<Namespace, Error> {
    let name = name_of(row.text(1)?)?;
    let partition = row.opt_i64(13)?.ok_or_else(|| {
        Error::Corrupt(format!(
            "namespace {name} has no partition: \
             PostgresCatalog::place_namespaces has not placed it yet"
        ))
    })?;
    let partition = u16::try_from(partition)
        .map_err(|_| Error::Corrupt(format!("namespace {name} is in partition {partition}")))?;
    Ok(Namespace {
        tenant: row.tenant(0)?,
        name,
        partition: PartitionId::new(partition),
        display_name: row.opt_text(12)?.map(display_name_of).transpose()?,
        exceptions: quotas_from_row(row, 2)?,
        tokens_revoked_before: row.opt_ts(8)?,
        created_at: row.ts(9)?,
        last_active_at: row.ts(10)?,
        deleting_since: row.opt_ts(11)?,
    })
}

async fn find_namespace(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    lock: bool,
) -> Result<Option<Namespace>, Error> {
    let row = sqlx::query(&format!(
        "SELECT {} FROM namespaces WHERE tenant = $1 AND name = $2{}",
        namespace_columns(),
        if lock { " FOR UPDATE" } else { "" }
    ))
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    row.map(|row| namespace_from_row(&row)).transpose()
}

/// The namespace, created at `now` in its partition if it does not exist, or placed in it if its
/// row has none, its row locked for the transaction; whether it was created or placed.
async fn ensure_locked(
    conn: &mut PgConnection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    partitions: NonZeroU16,
    now: Timestamp,
) -> Result<(Namespace, bool), Error> {
    let partition = namespace_partition(tenant_id, name, partitions);
    let written = sqlx::query(
        "INSERT INTO namespaces (tenant, name, created_at, last_active_at, partition) \
         VALUES ($1, $2, $3, $3, $4) \
         ON CONFLICT (tenant, name) DO UPDATE SET partition = EXCLUDED.partition \
         WHERE namespaces.partition IS NULL",
    )
    .bind(tenant(tenant_id))
    .bind(name.as_str())
    .bind(ts(now))
    .bind(i32::from(partition.get()))
    .execute(&mut *conn)
    .await?;
    // The row is locked by the statement above, updated or not.
    match find_namespace(conn, tenant_id, name, true).await? {
        Some(namespace) => Ok((namespace, written.rows_affected() > 0)),
        // Dropped by another transaction between the two statements: a retry creates it.
        None => Err(Error::Database {
            message: format!("namespace {name} was dropped while it was created"),
            code: Some("40001".into()),
        }),
    }
}

fn written(namespace: Namespace) -> NamespaceWrite {
    if namespace.deleting_since.is_some() {
        NamespaceWrite::Deleting
    } else {
        NamespaceWrite::Written(namespace)
    }
}

impl PostgresCatalog {
    /// Places in its partition, `hash(tenant, name) mod partitions`, every namespace whose row
    /// has none; returns how many. Run once, with the deployment's number of partitions, before
    /// the catalog serves (`rewind server migrate-runs` runs it first): a namespace without a
    /// partition cannot be read.
    pub async fn place_namespaces(&self, partitions: NonZeroU16) -> Result<u64, CatalogError> {
        let mut tx = self.pool.begin().await.map_err(Error::from)?;
        let rows = sqlx::query(
            "SELECT tenant, name FROM namespaces WHERE partition IS NULL \
             ORDER BY tenant, name FOR UPDATE",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::from)?;
        let mut changes = Vec::with_capacity(rows.len());
        for row in &rows {
            let (tenant_id, name) = (row.tenant(0)?, name_of(row.text(1)?)?);
            let partition = namespace_partition(&tenant_id, &name, partitions);
            sqlx::query("UPDATE namespaces SET partition = $3 WHERE tenant = $1 AND name = $2")
                .bind(tenant(&tenant_id))
                .bind(name.as_str())
                .bind(i32::from(partition.get()))
                .execute(&mut *tx)
                .await
                .map_err(Error::from)?;
            changes.push(namespace_changed(&tenant_id, &name));
        }
        notify(&mut tx, &changes).await?;
        tx.commit().await.map_err(Error::from)?;
        Ok(rows.len() as u64)
    }

    async fn create_workflow_once(
        &self,
        tenant_id: &TenantId,
        workflow: &NewWorkflow,
        now: Timestamp,
    ) -> Result<WorkflowCreated, Fail> {
        let version = &workflow.version;
        let mut tx = self.pool.begin().await?;
        // Without a name, a workflow is created only for a code its scope does not have yet.
        lock_code(&mut tx, tenant_id, &workflow.scope, version.code()).await?;
        match &workflow.name {
            Some(name) => {
                if named_in(&mut tx, tenant_id, &workflow.scope, name).await? {
                    return Ok(WorkflowCreated::NameTaken);
                }
            }
            None => {
                let scopes = [workflow.scope.to_string()];
                if let Some((existing, _)) = with_code(
                    &mut tx,
                    tenant_id,
                    &scopes,
                    version.code(),
                    version.labels(),
                )
                .await?
                {
                    return Ok(WorkflowCreated::Existing(existing));
                }
            }
        }
        lock_tests(
            &mut tx,
            tenant_id,
            version.suite().tests.iter().map(|test| test.test),
        )
        .await?;
        let source = workflow.source.as_ref();
        let inserted = sqlx::query(
            "INSERT INTO workflows (tenant, id, namespace, name, description, labels, \
                 source_workflow, source_version, created_by, created_at, latest) \
             VALUES ($1, $2, $3, $4, $5, $6::jsonb, $7, $8, $9, $10, 1) \
             ON CONFLICT (tenant, namespace, name) DO NOTHING",
        )
        .bind(tenant(tenant_id))
        .bind(workflow.id.as_str())
        .bind(workflow.scope.to_string())
        .bind(workflow.name.as_deref())
        .bind(workflow.description.as_deref())
        .bind(encode_labels(&workflow.labels))
        .bind(source.map(|source| source.workflow.as_str()))
        .bind(source.map(|source| i64::from(source.version)))
        .bind(workflow.created_by.as_str())
        .bind(ts(now))
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            // Its scope took the name since it was looked for.
            return Ok(WorkflowCreated::NameTaken);
        }
        insert_version(&mut tx, tenant_id, &workflow.id, 1, version, now).await?;
        add_code(
            &mut tx,
            tenant_id,
            &workflow.scope,
            version,
            &workflow.id,
            now,
        )
        .await?;
        join(
            &mut tx,
            tenant_id,
            &workflow.id,
            &suite_tests(version.suite()),
            now,
        )
        .await?;
        notify(&mut tx, &[workflow_changed(tenant_id, &workflow.id)]).await?;
        tx.commit().await?;
        Ok(WorkflowCreated::Created(StoredWorkflow {
            id: workflow.id.clone(),
            scope: workflow.scope.clone(),
            name: workflow.name.clone(),
            description: workflow.description.clone(),
            labels: workflow.labels.clone(),
            source: workflow.source.clone(),
            created_by: workflow.created_by.clone(),
            created_at: now,
            promoted_at: None,
            latest: 1,
        }))
    }

    async fn update_workflow_once(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        patch: &WorkflowPatch,
    ) -> Result<WorkflowWrite, Fail> {
        let mut tx = self.pool.begin().await?;
        let Some(workflow) = lock_workflow(&mut tx, tenant_id, id).await? else {
            return Ok(WorkflowWrite::NotFound);
        };
        let name = patch.name.clone().unwrap_or(workflow.name);
        let description = patch.description.clone().unwrap_or(workflow.description);
        let updated = sqlx::query(
            "UPDATE workflows SET name = $3, description = $4 WHERE tenant = $1 AND id = $2",
        )
        .bind(tenant(tenant_id))
        .bind(id.as_str())
        .bind(name.as_deref())
        .bind(description.as_deref())
        .execute(&mut *tx)
        .await;
        if name_taken(updated)? {
            return Ok(WorkflowWrite::NameTaken);
        }
        notify(&mut tx, &[workflow_changed(tenant_id, id)]).await?;
        tx.commit().await?;
        Ok(WorkflowWrite::Written(StoredWorkflow {
            name,
            description,
            ..workflow
        }))
    }

    async fn put_version_once(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        version: &NewVersion,
        now: Timestamp,
    ) -> Result<StoredVersion, Fail> {
        let document = &version.document;
        let suite = document.suite();
        let mut tx = self.pool.begin().await?;
        // The workflow's lock orders its versions: each takes the number after its latest.
        let Some(workflow) = lock_workflow(&mut tx, tenant_id, id).await? else {
            return Err(Failure::Port(no_workflow(id)));
        };
        lock_tests(&mut tx, tenant_id, suite.tests.iter().map(|test| test.test)).await?;
        let number = workflow.latest.checked_add(1).ok_or_else(|| {
            CatalogError::Conflict(format!("the workflow {id} has no version number left"))
        })?;
        let stored = insert_version(&mut tx, tenant_id, id, number, document, now).await?;
        lock_code(&mut tx, tenant_id, &workflow.scope, document.code()).await?;
        add_code(&mut tx, tenant_id, &workflow.scope, document, id, now).await?;
        // The suite is the version's: the tests it no longer has leave with the reason it gives.
        let current = current_members(&mut tx, tenant_id, id).await?;
        let leaving: Vec<(&str, Digest, Option<&str>)> = current
            .iter()
            .filter(|(name, digest)| suite.test(name) != Some(*digest))
            .map(|(name, digest)| (name.as_str(), *digest, reason_left(suite, name)))
            .collect();
        retire(&mut tx, tenant_id, id, &leaving, now).await?;
        join(&mut tx, tenant_id, id, &suite_tests(suite), now).await?;
        // A fork that took its source's version follows it from there.
        sqlx::query(
            "UPDATE workflows SET latest = $3, source_version = CASE \
                 WHEN source_workflow IS NOT NULL THEN COALESCE($4, source_version) END \
             WHERE tenant = $1 AND id = $2",
        )
        .bind(tenant(tenant_id))
        .bind(id.as_str())
        .bind(i64::from(number))
        .bind(version.pulled.map(i64::from))
        .execute(&mut *tx)
        .await?;
        notify(&mut tx, &[workflow_changed(tenant_id, id)]).await?;
        tx.commit().await?;
        Ok(stored)
    }

    async fn change_suite_once(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        change: &SuiteChange,
        now: Timestamp,
    ) -> Result<Vec<SuiteMember>, Fail> {
        let mut tx = self.pool.begin().await?;
        if lock_workflow(&mut tx, tenant_id, id).await?.is_none() {
            return Err(Failure::Port(no_workflow(id)));
        }
        let joining: BTreeSet<(&str, Digest)> = change
            .replaced
            .iter()
            .map(|replaced| (replaced.name.as_str(), replaced.new))
            .chain(
                change
                    .added
                    .iter()
                    .map(|test| (test.name.as_str(), test.test)),
            )
            .collect();
        lock_tests(
            &mut tx,
            tenant_id,
            joining.iter().map(|(_, digest)| *digest),
        )
        .await?;
        let leaving: Vec<(&str, Digest, Option<&str>)> = change
            .retired
            .iter()
            .map(|retired| {
                (
                    retired.name.as_str(),
                    retired.test,
                    Some(retired.reason.as_str()),
                )
            })
            .chain(change.replaced.iter().map(|replaced| {
                (
                    replaced.name.as_str(),
                    replaced.old,
                    Some(replaced.reason.as_str()),
                )
            }))
            .collect();
        let current = current_members(&mut tx, tenant_id, id).await?;
        if let Some((name, digest, _)) = leaving
            .iter()
            .find(|(name, digest, _)| !current.contains(&((*name).to_owned(), *digest)))
        {
            return Err(Failure::Port(CatalogError::Conflict(format!(
                "the suite of {id} has no test {name:?} of digest {digest}"
            ))));
        }
        retire(&mut tx, tenant_id, id, &leaving, now).await?;
        join(&mut tx, tenant_id, id, &joining, now).await?;
        let suite = suite_of(&mut tx, tenant_id, id).await?;
        notify(&mut tx, &[workflow_changed(tenant_id, id)]).await?;
        tx.commit().await?;
        Ok(suite)
    }

    /// Moves a workflow into `to`, with its codes: a code `to` has already under the same labels
    /// keeps its workflow there. `promoted_at`, for a promotion to the organization.
    async fn move_workflow_once(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        to: &WorkflowScope,
        promoted_at: Option<Timestamp>,
    ) -> Result<WorkflowWrite, Fail> {
        let mut tx = self.pool.begin().await?;
        let Some(workflow) = lock_workflow(&mut tx, tenant_id, id).await? else {
            return Ok(WorkflowWrite::NotFound);
        };
        if workflow.scope == *to {
            return Ok(WorkflowWrite::Written(workflow));
        }
        let moved = sqlx::query(
            "UPDATE workflows SET namespace = $3, promoted_at = COALESCE($4, promoted_at) \
             WHERE tenant = $1 AND id = $2",
        )
        .bind(tenant(tenant_id))
        .bind(id.as_str())
        .bind(to.to_string())
        .bind(promoted_at.map(ts))
        .execute(&mut *tx)
        .await;
        if name_taken(moved)? {
            return Ok(WorkflowWrite::NameTaken);
        }
        sqlx::query(
            "DELETE FROM workflow_codes c \
             WHERE c.tenant = $1 AND c.namespace = $2 AND c.workflow = $3 \
             AND EXISTS (SELECT 1 FROM workflow_codes o WHERE o.tenant = c.tenant \
                         AND o.namespace = $4 AND o.labels = c.labels AND o.code = c.code)",
        )
        .bind(tenant(tenant_id))
        .bind(workflow.scope.to_string())
        .bind(id.as_str())
        .bind(to.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE workflow_codes SET namespace = $4 \
             WHERE tenant = $1 AND namespace = $2 AND workflow = $3",
        )
        .bind(tenant(tenant_id))
        .bind(workflow.scope.to_string())
        .bind(id.as_str())
        .bind(to.to_string())
        .execute(&mut *tx)
        .await?;
        notify(&mut tx, &[workflow_changed(tenant_id, id)]).await?;
        tx.commit().await?;
        Ok(WorkflowWrite::Written(StoredWorkflow {
            scope: to.clone(),
            promoted_at: promoted_at.or(workflow.promoted_at),
            ..workflow
        }))
    }

    async fn reserve_tests_once(
        &self,
        tenant_id: &TenantId,
        tests: &[TestRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, Fail> {
        // One row per digest, in digest order: the order every writer locks test rows in.
        let by_digest: BTreeMap<Digest, &TestRecord> =
            tests.iter().map(|test| (test.digest, test)).collect();
        let digests: Vec<Vec<u8>> = by_digest.keys().map(|digest| bytes(*digest)).collect();
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            "SELECT digest, deletion_started_at IS NOT NULL FROM tests \
             WHERE tenant = $1 AND digest = ANY($2) ORDER BY digest FOR UPDATE",
        )
        .bind(tenant(tenant_id))
        .bind(digests)
        .fetch_all(&mut *tx)
        .await?;
        let mut deleting = BTreeSet::new();
        for row in &rows {
            if row.boolean(1)? {
                deleting.insert(row.digest(0)?);
            }
        }
        let kept: Vec<&TestRecord> = by_digest
            .values()
            .filter(|test| !deleting.contains(&test.digest))
            .copied()
            .collect();
        if !kept.is_empty() {
            // A missing row is created; an existing one starts its grace again. A row created
            // since the lock above is not being deleted either.
            let written = sqlx::query(
                "INSERT INTO tests (tenant, digest, namespace, size, reserved_at) \
                 SELECT $1, digest, namespace, size, $5 \
                 FROM unnest($2::bytea[], $3::text[], $4::bigint[]) \
                     WITH ORDINALITY AS t(digest, namespace, size, n) \
                 ORDER BY n \
                 ON CONFLICT (tenant, digest) DO UPDATE SET reserved_at = $5 \
                 WHERE tests.deletion_started_at IS NULL \
                 RETURNING tenant, digest",
            )
            .bind(tenant(tenant_id))
            .bind(
                kept.iter()
                    .map(|test| bytes(test.digest))
                    .collect::<Vec<_>>(),
            )
            .bind(
                kept.iter()
                    .map(|test| test.namespace.as_ref().map(NamespaceName::as_str))
                    .collect::<Vec<_>>(),
            )
            .bind(kept.iter().map(|test| int(test.size)).collect::<Vec<_>>())
            .bind(ts(now))
            .fetch_all(&mut *tx)
            .await?;
            let changes: Vec<ControlPlaneChange> = keys_of(&written)?
                .iter()
                .map(|(owner, digest)| test_changed(owner, *digest))
                .collect();
            notify(&mut tx, &changes).await?;
        }
        tx.commit().await?;
        // In the order asked.
        Ok(tests
            .iter()
            .map(|test| test.digest)
            .filter(|digest| deleting.contains(digest))
            .collect())
    }

    async fn unreferenced_tests_once(
        &self,
        now: Timestamp,
        max: usize,
    ) -> Result<Vec<(TenantId, Digest)>, Fail> {
        let mut tx = self.pool.begin().await?;
        // The rows due, but those a writer or another cleanup holds.
        let rows = sqlx::query(concat!(
            "SELECT t.tenant, t.digest FROM tests t WHERE ",
            test_due!(),
            " ORDER BY t.tenant, t.digest LIMIT $4 FOR UPDATE SKIP LOCKED"
        ))
        .bind(ts(now))
        .bind(ms(TEST_GRACE))
        .bind(ms(TEST_DELETION_TIMEOUT))
        .bind(limit(max))
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let picked = keys_of(&rows)?;
        let (tenants, digests) = sorted_keys(&picked);
        // Checked again under the locks, by a statement that sees what committed before them: a
        // suite that referenced a row before it was locked keeps it.
        let marked = sqlx::query(concat!(
            "UPDATE tests t SET deletion_started_at = $1 \
             FROM unnest($4::text[], $5::bytea[]) AS f(tenant, digest) \
             WHERE t.tenant = f.tenant AND t.digest = f.digest AND ",
            test_due!(),
            " RETURNING t.tenant, t.digest"
        ))
        .bind(ts(now))
        .bind(ms(TEST_GRACE))
        .bind(ms(TEST_DELETION_TIMEOUT))
        .bind(tenants)
        .bind(digests)
        .fetch_all(&mut *tx)
        .await?;
        let marked: BTreeSet<(TenantId, Digest)> = keys_of(&marked)?.into_iter().collect();
        let changes: Vec<ControlPlaneChange> = marked
            .iter()
            .map(|(owner, digest)| test_changed(owner, *digest))
            .collect();
        notify(&mut tx, &changes).await?;
        tx.commit().await?;
        Ok(marked.into_iter().collect())
    }

    async fn drop_tests_once(
        &self,
        tests: &[(TenantId, Digest)],
        erased: bool,
    ) -> Result<usize, Fail> {
        let (tenants, digests) = sorted_keys(tests);
        let mut tx = self.pool.begin().await?;
        let dropped = if erased {
            // A namespace's erasure: the rows go whatever references them, then the suites'
            // references, read after the rows' locks: those of a suite written meanwhile go too.
            let dropped = sqlx::query(
                "DELETE FROM tests t USING ( \
                     SELECT tenant, digest FROM unnest($1::text[], $2::bytea[]) \
                         AS f(tenant, digest) \
                     ORDER BY tenant, digest) f \
                 WHERE t.tenant = f.tenant AND t.digest = f.digest \
                 RETURNING t.tenant, t.digest",
            )
            .bind(&tenants)
            .bind(&digests)
            .fetch_all(&mut *tx)
            .await?;
            sqlx::query(
                "DELETE FROM workflow_tests w \
                 USING unnest($1::text[], $2::bytea[]) AS f(tenant, digest) \
                 WHERE w.tenant = f.tenant AND w.digest = f.digest",
            )
            .bind(&tenants)
            .bind(&digests)
            .execute(&mut *tx)
            .await?;
            dropped
        } else {
            sqlx::query(
                "DELETE FROM tests t USING ( \
                     SELECT tenant, digest FROM unnest($1::text[], $2::bytea[]) \
                         AS f(tenant, digest) \
                     ORDER BY tenant, digest) f \
                 WHERE t.tenant = f.tenant AND t.digest = f.digest \
                 AND t.deletion_started_at IS NOT NULL \
                 AND NOT EXISTS (SELECT 1 FROM workflow_tests w \
                                 WHERE w.tenant = t.tenant AND w.digest = t.digest) \
                 RETURNING t.tenant, t.digest",
            )
            .bind(&tenants)
            .bind(&digests)
            .fetch_all(&mut *tx)
            .await?
        };
        let changes: Vec<ControlPlaneChange> = keys_of(&dropped)?
            .iter()
            .map(|(owner, digest)| test_changed(owner, *digest))
            .collect();
        notify(&mut tx, &changes).await?;
        tx.commit().await?;
        Ok(dropped.len())
    }

    async fn ensure_once(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> Result<NamespaceWrite, Fail> {
        let mut tx = self.pool.begin().await?;
        let (namespace, changed) = ensure_locked(&mut tx, tenant_id, name, partitions, now).await?;
        if changed {
            notify(&mut tx, &[namespace_changed(tenant_id, name)]).await?;
        }
        tx.commit().await?;
        Ok(written(namespace))
    }

    async fn put_namespace_once(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        exceptions: Option<&Quotas>,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> Result<NamespaceWrite, Fail> {
        let mut tx = self.pool.begin().await?;
        let (namespace, changed) = ensure_locked(&mut tx, tenant_id, name, partitions, now).await?;
        if namespace.deleting_since.is_some() {
            return Ok(NamespaceWrite::Deleting);
        }
        let namespace = match exceptions {
            Some(exceptions) => {
                let [cpu, active, runs, scripts, ttl, lines] = quota_values(exceptions);
                sqlx::query(
                    "UPDATE namespaces SET cpu_per_month_ms = $3, active_runs = $4, \
                     runs_per_month = $5, scripts = $6, max_ttl_ms = $7, max_script_lines = $8 \
                     WHERE tenant = $1 AND name = $2",
                )
                .bind(tenant(tenant_id))
                .bind(name.as_str())
                .bind(cpu)
                .bind(active)
                .bind(runs)
                .bind(scripts)
                .bind(ttl)
                .bind(lines)
                .execute(&mut *tx)
                .await?;
                Namespace {
                    exceptions: exceptions.clone(),
                    ..namespace
                }
            }
            None => namespace,
        };
        if changed || exceptions.is_some() {
            notify(&mut tx, &[namespace_changed(tenant_id, name)]).await?;
        }
        tx.commit().await?;
        Ok(NamespaceWrite::Written(namespace))
    }

    async fn set_display_name_once(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        display_name: Option<&DisplayName>,
    ) -> Result<Option<NamespaceWrite>, Fail> {
        let mut tx = self.pool.begin().await?;
        let Some(namespace) = find_namespace(&mut tx, tenant_id, name, true).await? else {
            return Ok(None);
        };
        if namespace.deleting_since.is_some() {
            return Ok(Some(NamespaceWrite::Deleting));
        }
        sqlx::query(
            "UPDATE namespaces SET display_name = $3, display_key = $4 \
             WHERE tenant = $1 AND name = $2",
        )
        .bind(tenant(tenant_id))
        .bind(name.as_str())
        .bind(display_name.map(DisplayName::as_str))
        .bind(display_name.map(DisplayName::key))
        .execute(&mut *tx)
        .await?;
        notify(&mut tx, &[namespace_changed(tenant_id, name)]).await?;
        tx.commit().await?;
        Ok(Some(NamespaceWrite::Written(Namespace {
            display_name: display_name.cloned(),
            ..namespace
        })))
    }

    async fn drop_namespace_once(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
    ) -> Result<DropNamespace, Fail> {
        let mut tx = self.pool.begin().await?;
        let dropped = sqlx::query(
            "DELETE FROM namespaces WHERE tenant = $1 AND name = $2 AND deleting_since IS NOT NULL",
        )
        .bind(tenant(tenant_id))
        .bind(name.as_str())
        .execute(&mut *tx)
        .await?;
        if dropped.rows_affected() == 0 {
            return Ok(DropNamespace::NotFound);
        }
        // Its workflows go with it, with their versions, suites and codes; the organization's
        // stay.
        let workflows: Vec<String> = sqlx::query_scalar(
            "DELETE FROM workflows WHERE tenant = $1 AND namespace = $2 RETURNING id",
        )
        .bind(tenant(tenant_id))
        .bind(name.as_str())
        .fetch_all(&mut *tx)
        .await?;
        for table in ["workflow_versions", "workflow_tests"] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE tenant = $1 AND workflow = ANY($2)"
            ))
            .bind(tenant(tenant_id))
            .bind(&workflows)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("DELETE FROM workflow_codes WHERE tenant = $1 AND namespace = $2")
            .bind(tenant(tenant_id))
            .bind(name.as_str())
            .execute(&mut *tx)
            .await?;
        let mut ids = workflows
            .into_iter()
            .map(workflow_id)
            .collect::<Result<Vec<_>, Error>>()?;
        ids.sort();
        let changes: Vec<ControlPlaneChange> = std::iter::once(namespace_changed(tenant_id, name))
            .chain(ids.iter().map(|id| workflow_changed(tenant_id, id)))
            .collect();
        notify(&mut tx, &changes).await?;
        tx.commit().await?;
        Ok(DropNamespace::Dropped)
    }

    /// Runs one write of a namespace's row and sends its change when it changed a row.
    async fn namespace_update(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        sql: &str,
        now: Timestamp,
    ) -> Result<bool, CatalogError> {
        let mut tx = self.pool.begin().await.map_err(Error::from)?;
        let changed = sqlx::query(sql)
            .bind(tenant(tenant_id))
            .bind(name.as_str())
            .bind(ts(now))
            .execute(&mut *tx)
            .await
            .map_err(Error::from)?;
        if changed.rows_affected() == 0 {
            return Ok(false);
        }
        notify(&mut tx, &[namespace_changed(tenant_id, name)]).await?;
        tx.commit().await.map_err(Error::from)?;
        Ok(true)
    }
}

impl CatalogStore for PostgresCatalog {
    // ---------------------------------------------------------------------------------------
    // Workflows and action sets

    async fn put_code(
        &self,
        tenant_id: &TenantId,
        code: &Code,
        compiled: &CompiledCode,
    ) -> Result<(), CatalogError> {
        // A code is content under its digest, which never changes: no change is sent.
        sqlx::query(
            "INSERT INTO code (tenant, digest, source, transpiled, source_map) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(code.digest()))
        .bind(code.source())
        .bind(compiled.javascript.as_str())
        .bind(compiled.source_map.as_str())
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    async fn get_code(
        &self,
        tenant_id: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredCode>, CatalogError> {
        let row = sqlx::query(
            "SELECT source, transpiled, source_map FROM code WHERE tenant = $1 AND digest = $2",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(digest))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let code = Code::new(row.text(0)?);
        if code.digest() != digest {
            return Err(Error::Corrupt(format!("code {digest} does not hash to its key")).into());
        }
        Ok(Some(StoredCode {
            code,
            compiled: CompiledCode {
                javascript: row.text(1)?,
                source_map: row.text(2)?,
            },
        }))
    }

    async fn put_action_set(
        &self,
        tenant_id: &TenantId,
        action_set: &ActionSet,
        declarations: &ActionDeclarations,
    ) -> Result<(), CatalogError> {
        let mut tx = self.pool.begin().await.map_err(Error::from)?;
        let inserted = sqlx::query(
            "INSERT INTO action_sets (tenant, digest, spec, declarations, prompt) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(action_set.digest()))
        .bind(rewind_format::encode_action_set(action_set))
        .bind(declarations.typescript.as_str())
        .bind(declarations.prompt.as_str())
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
        if inserted.rows_affected() > 0 {
            let change = ControlPlaneChange::ActionSet {
                tenant: tenant_id.clone(),
                digest: action_set.digest(),
            };
            notify(&mut tx, &[change]).await?;
        }
        tx.commit().await.map_err(Error::from)?;
        Ok(())
    }

    async fn get_action_set(
        &self,
        tenant_id: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredActionSet>, CatalogError> {
        let row = sqlx::query(
            "SELECT spec, declarations, prompt FROM action_sets WHERE tenant = $1 AND digest = $2",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(digest))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(row
            .map(|row| action_set_from_row(&row, digest, 0))
            .transpose()?)
    }

    async fn list_action_sets(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<StoredActionSet>, CatalogError> {
        let rows = sqlx::query(
            "SELECT digest, spec, declarations, prompt FROM action_sets WHERE tenant = $1 \
             ORDER BY digest",
        )
        .bind(tenant(tenant_id))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| action_set_from_row(row, row.digest(0)?, 1))
            .collect::<Result<_, Error>>()?)
    }

    // ---------------------------------------------------------------------------------------
    // Workflows

    async fn create_workflow(
        &self,
        tenant_id: &TenantId,
        workflow: &NewWorkflow,
        now: Timestamp,
    ) -> Result<WorkflowCreated, CatalogError> {
        Ok(retrying(|| self.create_workflow_once(tenant_id, workflow, now)).await?)
    }

    async fn workflow(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows w WHERE w.tenant = $1 AND w.id = $2"
        ))
        .bind(tenant(tenant_id))
        .bind(id.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(row.map(|row| workflow_from_row(&row, 0)).transpose()?)
    }

    async fn workflow_named(
        &self,
        tenant_id: &TenantId,
        namespace: Option<&NamespaceName>,
        name: &str,
        labels: &Labels,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        // The namespace's first: it shadows the organization's.
        let scopes: Vec<String> = namespace
            .map(NamespaceName::to_string)
            .into_iter()
            .chain([ORGANIZATION.to_owned()])
            .collect();
        let row = sqlx::query(concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows w \
             WHERE w.tenant = $1 AND w.namespace = ANY($2) AND w.name = $3 \
             AND EXISTS (SELECT 1 FROM workflow_versions v \
                         WHERE v.tenant = w.tenant AND v.workflow = w.id \
                         AND v.labels @> $4::jsonb) \
             ORDER BY array_position($2, w.namespace::text) LIMIT 1"
        ))
        .bind(tenant(tenant_id))
        .bind(scopes)
        .bind(name)
        .bind(encode_labels(labels))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(row.map(|row| workflow_from_row(&row, 0)).transpose()?)
    }

    async fn workflow_with_code(
        &self,
        tenant_id: &TenantId,
        namespace: &NamespaceName,
        code: Digest,
        labels: &Labels,
    ) -> Result<Option<(StoredWorkflow, StoredVersion)>, CatalogError> {
        let scopes = [namespace.to_string(), ORGANIZATION.to_owned()];
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(with_code(&mut conn, tenant_id, &scopes, code, labels).await?)
    }

    async fn list_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        // The scopes listed; `None`, every one.
        let scopes: Option<Vec<String>> = match &filter.scope {
            ScopeFilter::All => None,
            ScopeFilter::Visible(namespace) => {
                Some(vec![namespace.to_string(), ORGANIZATION.to_owned()])
            }
            ScopeFilter::Organization => Some(vec![ORGANIZATION.to_owned()]),
            ScopeFilter::Namespace(namespace) => Some(vec![namespace.to_string()]),
        };
        let rows = sqlx::query(concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows w \
             WHERE w.tenant = $1 AND ($2::text[] IS NULL OR w.namespace = ANY($2)) \
             AND ($3::text IS NULL OR w.id > $3) \
             AND EXISTS (SELECT 1 FROM workflow_versions v \
                         WHERE v.tenant = w.tenant AND v.workflow = w.id \
                         AND v.labels @> $4::jsonb AND ($5::bytea IS NULL OR v.code = $5)) \
             ORDER BY w.id LIMIT $6"
        ))
        .bind(tenant(&filter.tenant))
        .bind(scopes)
        .bind(filter.after.as_ref().map(WorkflowId::as_str))
        .bind(encode_labels(&filter.labels))
        .bind(filter.code.map(bytes))
        .bind(limit(filter.limit))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| workflow_from_row(row, 0))
            .collect::<Result<_, Error>>()?)
    }

    async fn update_workflow(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        patch: &WorkflowPatch,
        _now: Timestamp,
    ) -> Result<WorkflowWrite, CatalogError> {
        Ok(retrying(|| self.update_workflow_once(tenant_id, id, patch)).await?)
    }

    async fn put_version(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        version: &NewVersion,
        now: Timestamp,
    ) -> Result<StoredVersion, CatalogError> {
        Ok(retrying(|| self.put_version_once(tenant_id, id, version, now)).await?)
    }

    async fn versions(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        labels: &Labels,
    ) -> Result<Vec<StoredVersion>, CatalogError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            version_columns!(),
            " FROM workflow_versions v \
             WHERE v.tenant = $1 AND v.workflow = $2 AND v.labels @> $3::jsonb \
             ORDER BY v.version DESC"
        ))
        .bind(tenant(tenant_id))
        .bind(id.as_str())
        .bind(encode_labels(labels))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| version_from_row(row, 0))
            .collect::<Result<_, Error>>()?)
    }

    async fn version(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        version: VersionRef,
    ) -> Result<Option<StoredVersion>, CatalogError> {
        let (number, digest) = match version {
            VersionRef::Number(number) => (Some(i64::from(number)), None),
            VersionRef::Digest(digest) => (None, Some(bytes(digest))),
        };
        // By its digest, the latest version that has it.
        let row = sqlx::query(concat!(
            "SELECT ",
            version_columns!(),
            " FROM workflow_versions v \
             WHERE v.tenant = $1 AND v.workflow = $2 \
             AND ($3::bigint IS NULL OR v.version = $3) AND ($4::bytea IS NULL OR v.digest = $4) \
             ORDER BY v.version DESC LIMIT 1"
        ))
        .bind(tenant(tenant_id))
        .bind(id.as_str())
        .bind(number)
        .bind(digest)
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(row.map(|row| version_from_row(&row, 0)).transpose()?)
    }

    async fn change_suite(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        change: &SuiteChange,
        now: Timestamp,
    ) -> Result<Vec<SuiteMember>, CatalogError> {
        Ok(retrying(|| self.change_suite_once(tenant_id, id, change, now)).await?)
    }

    async fn suite(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
    ) -> Result<Vec<SuiteMember>, CatalogError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(suite_of(&mut conn, tenant_id, id).await?)
    }

    async fn promote_workflow(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        now: Timestamp,
    ) -> Result<WorkflowWrite, CatalogError> {
        let promote =
            || self.move_workflow_once(tenant_id, id, &WorkflowScope::Organization, Some(now));
        Ok(retrying(promote).await?)
    }

    // ---------------------------------------------------------------------------------------
    // Tests

    async fn reserve_tests(
        &self,
        tenant_id: &TenantId,
        tests: &[TestRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, CatalogError> {
        if tests.is_empty() {
            return Ok(Vec::new());
        }
        Ok(retrying(|| self.reserve_tests_once(tenant_id, tests, now)).await?)
    }

    async fn get_test(
        &self,
        tenant_id: &TenantId,
        digest: Digest,
    ) -> Result<Option<TestRecord>, CatalogError> {
        let row = sqlx::query(
            "SELECT namespace, size FROM tests \
             WHERE tenant = $1 AND digest = $2 AND deletion_started_at IS NULL",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(digest))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(TestRecord {
            digest,
            namespace: row.opt_text(0)?.map(name_of).transpose()?,
            size: row.u64(1)?,
        }))
    }

    async fn namespace_tests(
        &self,
        tenant_id: &TenantId,
        namespace: &NamespaceName,
        max: usize,
    ) -> Result<Vec<Digest>, CatalogError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT digest FROM tests WHERE tenant = $1 AND namespace = $2 \
             ORDER BY digest LIMIT $3",
        )
        .bind(tenant(tenant_id))
        .bind(namespace.as_str())
        .bind(limit(max))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| row.digest(0))
            .collect::<Result<_, Error>>()?)
    }

    async fn unreferenced_tests(
        &self,
        now: Timestamp,
        max: usize,
    ) -> Result<Vec<(TenantId, Digest)>, CatalogError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        Ok(retrying(|| self.unreferenced_tests_once(now, max)).await?)
    }

    async fn drop_tests(
        &self,
        tests: &[(TenantId, Digest)],
        erased: bool,
    ) -> Result<usize, CatalogError> {
        if tests.is_empty() {
            return Ok(0);
        }
        Ok(retrying(|| self.drop_tests_once(tests, erased)).await?)
    }

    // ---------------------------------------------------------------------------------------
    // Subscriptions

    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), CatalogError> {
        let mut tx = self.pool.begin().await.map_err(Error::from)?;
        sqlx::query(
            "INSERT INTO subscriptions (tenant, id, url, secret, types, labels) \
             VALUES ($1, $2, $3, $4, $5, $6::jsonb) \
             ON CONFLICT (tenant, id) DO UPDATE SET url = EXCLUDED.url, \
                 secret = EXCLUDED.secret, types = EXCLUDED.types, labels = EXCLUDED.labels",
        )
        .bind(tenant(&subscription.tenant))
        .bind(subscription.id.as_str())
        .bind(subscription.url.as_str())
        .bind(subscription.secret.expose())
        .bind(encode_event_types(&subscription.types))
        .bind(encode_labels(&subscription.labels))
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
        let change = ControlPlaneChange::Subscription {
            tenant: subscription.tenant.clone(),
            id: subscription.id.clone(),
        };
        notify(&mut tx, &[change]).await?;
        tx.commit().await.map_err(Error::from)?;
        Ok(())
    }

    async fn subscriptions(&self, tenant_id: &TenantId) -> Result<Vec<Subscription>, CatalogError> {
        let rows = sqlx::query(
            "SELECT id, url, secret, types, labels::text FROM subscriptions \
             WHERE tenant = $1 ORDER BY id",
        )
        .bind(tenant(tenant_id))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| {
                Ok(Subscription {
                    tenant: tenant_id.clone(),
                    id: row.text(0)?,
                    url: row.text(1)?,
                    secret: Secret::new(row.text(2)?),
                    types: decode_event_types(&row.text(3)?)?,
                    labels: decode_labels(&row.text(4)?)?,
                })
            })
            .collect::<Result<_, Error>>()?)
    }

    async fn delete_subscription(
        &self,
        tenant_id: &TenantId,
        id: &str,
    ) -> Result<bool, CatalogError> {
        let mut tx = self.pool.begin().await.map_err(Error::from)?;
        let deleted = sqlx::query("DELETE FROM subscriptions WHERE tenant = $1 AND id = $2")
            .bind(tenant(tenant_id))
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(Error::from)?;
        if deleted.rows_affected() == 0 {
            return Ok(false);
        }
        let change = ControlPlaneChange::Subscription {
            tenant: tenant_id.clone(),
            id: id.to_owned(),
        };
        notify(&mut tx, &[change]).await?;
        tx.commit().await.map_err(Error::from)?;
        Ok(true)
    }

    // ---------------------------------------------------------------------------------------
    // Tenants

    async fn put_tenant(&self, record: &Tenant) -> Result<(), CatalogError> {
        let mut tx = self.pool.begin().await.map_err(Error::from)?;
        tenancy::put_tenant(&mut tx, record).await?;
        let change = ControlPlaneChange::Tenant {
            tenant: record.id.clone(),
        };
        notify(&mut tx, &[change]).await?;
        tx.commit().await.map_err(Error::from)?;
        Ok(())
    }

    async fn get_tenant(&self, id: &TenantId) -> Result<Option<Tenant>, CatalogError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(tenancy::get_tenant(&mut conn, id).await?)
    }

    async fn list_tenants(&self) -> Result<Vec<Tenant>, CatalogError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(tenancy::list_tenants(&mut conn).await?)
    }

    async fn legacy_partition(&self, run: RunId) -> Result<Option<PartitionId>, CatalogError> {
        let partition: Option<i32> =
            sqlx::query_scalar("SELECT partition FROM legacy_runs WHERE id = $1")
                .bind(run.as_uuid())
                .fetch_optional(&self.pool)
                .await
                .map_err(Error::from)?;
        partition
            .map(|partition| {
                u16::try_from(partition).map(PartitionId::new).map_err(|_| {
                    CatalogError::Backend(format!(
                        "the partition {partition} of the moved run {run} is out of range"
                    ))
                })
            })
            .transpose()
    }

    async fn put_legacy_run(
        &self,
        run: &RunRef,
        partition: PartitionId,
    ) -> Result<(), CatalogError> {
        sqlx::query(
            "INSERT INTO legacy_runs (id, tenant, partition) VALUES ($1, $2, $3) \
             ON CONFLICT (id) DO UPDATE SET partition = EXCLUDED.partition",
        )
        .bind(run.id.as_uuid())
        .bind(tenant(&run.tenant))
        .bind(i32::from(partition.get()))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // Namespaces

    async fn ensure_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> Result<NamespaceWrite, CatalogError> {
        // A namespace in use is read, not written.
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        let row = sqlx::query(&format!(
            "SELECT {} FROM namespaces WHERE tenant = $1 AND name = $2 AND partition IS NOT NULL",
            namespace_columns()
        ))
        .bind(tenant(tenant_id))
        .bind(name.as_str())
        .fetch_optional(&mut *conn)
        .await
        .map_err(Error::from)?;
        drop(conn);
        if let Some(row) = row {
            return Ok(written(namespace_from_row(&row)?));
        }
        Ok(retrying(|| self.ensure_once(tenant_id, name, partitions, now)).await?)
    }

    async fn put_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        exceptions: Option<&Quotas>,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> Result<NamespaceWrite, CatalogError> {
        let put = || self.put_namespace_once(tenant_id, name, exceptions, partitions, now);
        Ok(retrying(put).await?)
    }

    async fn get_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
    ) -> Result<Option<Namespace>, CatalogError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(find_namespace(&mut conn, tenant_id, name, false).await?)
    }

    async fn set_namespace_display_name(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        display_name: Option<&DisplayName>,
    ) -> Result<Option<NamespaceWrite>, CatalogError> {
        Ok(retrying(|| self.set_display_name_once(tenant_id, name, display_name)).await?)
    }

    async fn namespace_display_names(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<(NamespaceName, DisplayName)>, CatalogError> {
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(tenancy::display_names(&mut conn, tenant_id).await?)
    }

    async fn list_namespaces(
        &self,
        filter: &NamespaceFilter,
    ) -> Result<NamespacePage, CatalogError> {
        const MATCHING: &str = "tenant = $1 AND ($2::text IS NULL OR name = $2) \
             AND ($3::text IS NULL OR starts_with(name, $3) OR starts_with(display_key, $4))";
        let prefix = filter.prefix.as_deref();
        let folded = prefix.map(DisplayName::fold);
        let mut tx = self.pool.begin().await.map_err(Error::from)?;
        // Both reads see one snapshot: the total counts the page's namespaces.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(Error::from)?;
        let total: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM namespaces WHERE {MATCHING}"))
                .bind(tenant(&filter.tenant))
                .bind(filter.name.as_ref().map(NamespaceName::as_str))
                .bind(prefix)
                .bind(folded.as_deref())
                .fetch_one(&mut *tx)
                .await
                .map_err(Error::from)?;
        let rows = sqlx::query(&format!(
            "SELECT {} FROM namespaces WHERE {MATCHING} AND ($5::text IS NULL OR name > $5) \
             ORDER BY name LIMIT $6",
            namespace_columns()
        ))
        .bind(tenant(&filter.tenant))
        .bind(filter.name.as_ref().map(NamespaceName::as_str))
        .bind(prefix)
        .bind(folded.as_deref())
        .bind(filter.after.as_ref().map(NamespaceName::as_str))
        .bind(limit(filter.limit))
        .fetch_all(&mut *tx)
        .await
        .map_err(Error::from)?;
        tx.commit().await.map_err(Error::from)?;
        Ok(NamespacePage {
            namespaces: rows
                .iter()
                .map(namespace_from_row)
                .collect::<Result<_, Error>>()?,
            total: u64::try_from(total).unwrap_or(0),
        })
    }

    async fn revoke_namespace_tokens(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<bool, CatalogError> {
        self.namespace_update(
            tenant_id,
            name,
            "UPDATE namespaces SET tokens_revoked_before = $3 WHERE tenant = $1 AND name = $2",
            now,
        )
        .await
    }

    async fn delete_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<DeleteNamespace, CatalogError> {
        let marked = self
            .namespace_update(
                tenant_id,
                name,
                "UPDATE namespaces SET deleting_since = COALESCE(deleting_since, $3) \
                 WHERE tenant = $1 AND name = $2",
                now,
            )
            .await?;
        Ok(if marked {
            DeleteNamespace::Started
        } else {
            DeleteNamespace::NotFound
        })
    }

    async fn namespaces_to_erase(
        &self,
        max: usize,
    ) -> Result<Vec<(TenantId, NamespaceName)>, CatalogError> {
        if max == 0 {
            return Ok(Vec::new());
        }
        let mut conn = self.pool.acquire().await.map_err(Error::from)?;
        Ok(tenancy::namespaces_to_erase(&mut conn, max).await?)
    }

    async fn drop_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
    ) -> Result<DropNamespace, CatalogError> {
        Ok(retrying(|| self.drop_namespace_once(tenant_id, name)).await?)
    }
}

/// What `rewind server rename-data` needs beyond the catalog: the titles people gave codes,
/// which the schema migration of the workflow model kept in `code_titles`, and the move of an
/// organization's workflow into the namespace that ran it.
impl CatalogMigration for PostgresCatalog {
    async fn code_titles(&self) -> Result<Vec<CodeTitle>, CatalogError> {
        let rows = sqlx::query(
            "SELECT tenant, digest, title, description FROM code_titles ORDER BY tenant, digest",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| {
                Ok(CodeTitle {
                    tenant: row.tenant(0)?,
                    code: row.digest(1)?,
                    title: row.text(2)?,
                    description: row.opt_text(3)?,
                })
            })
            .collect::<Result<_, Error>>()?)
    }

    async fn scope_workflow(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        namespace: &NamespaceName,
        _now: Timestamp,
    ) -> Result<WorkflowWrite, CatalogError> {
        let scope = WorkflowScope::Namespace(namespace.clone());
        Ok(retrying(|| self.move_workflow_once(tenant_id, id, &scope, None)).await?)
    }

    async fn drop_code_titles(&self) -> Result<(), CatalogError> {
        sqlx::query("DELETE FROM code_titles")
            .execute(&self.pool)
            .await
            .map_err(Error::from)?;
        Ok(())
    }
}

impl CatalogDocuments for PostgresCatalog {
    async fn raw_action_sets(&self) -> Result<Vec<RawDocument>, CatalogError> {
        let rows =
            sqlx::query("SELECT tenant, digest, spec FROM action_sets ORDER BY tenant, digest")
                .fetch_all(&self.pool)
                .await
                .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| {
                Ok(RawDocument {
                    tenant: row.tenant(0)?,
                    digest: row.digest(1)?,
                    json: row.text(2)?,
                })
            })
            .collect::<Result<_, Error>>()?)
    }

    async fn replace_action_set(
        &self,
        tenant_id: &TenantId,
        old: Digest,
        action_set: &ActionSet,
        declarations: &ActionDeclarations,
    ) -> Result<(), CatalogError> {
        let mut tx = self.pool.begin().await.map_err(Error::from)?;
        sqlx::query(
            "INSERT INTO action_sets (tenant, digest, spec, declarations, prompt) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (tenant, digest) DO UPDATE \
             SET spec = EXCLUDED.spec, declarations = EXCLUDED.declarations, \
                 prompt = EXCLUDED.prompt",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(action_set.digest()))
        .bind(rewind_format::encode_action_set(action_set))
        .bind(declarations.typescript.as_str())
        .bind(declarations.prompt.as_str())
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
        if old != action_set.digest() {
            sqlx::query("DELETE FROM action_sets WHERE tenant = $1 AND digest = $2")
                .bind(tenant(tenant_id))
                .bind(bytes(old))
                .execute(&mut *tx)
                .await
                .map_err(Error::from)?;
        }
        tx.commit().await.map_err(Error::from)?;
        Ok(())
    }

    async fn raw_versions(&self) -> Result<Vec<RawVersion>, CatalogError> {
        let rows = sqlx::query(
            "SELECT tenant, workflow, version, digest, document FROM workflow_versions \
             ORDER BY tenant, workflow, version",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| {
                Ok(RawVersion {
                    tenant: row.tenant(0)?,
                    workflow: workflow_id(row.text(1)?)?,
                    version: row.u64(2)?,
                    digest: row.digest(3)?,
                    json: row.text(4)?,
                })
            })
            .collect::<Result<_, Error>>()?)
    }

    async fn replace_version(
        &self,
        tenant_id: &TenantId,
        workflow: &WorkflowId,
        version: u64,
        document: &WorkflowVersion,
    ) -> Result<(), CatalogError> {
        sqlx::query(
            "UPDATE workflow_versions SET digest = $4, document = $5 \
             WHERE tenant = $1 AND workflow = $2 AND version = $3",
        )
        .bind(tenant(tenant_id))
        .bind(workflow.as_str())
        .bind(int(version))
        .bind(bytes(document.digest()))
        .bind(rewind_format::encode_workflow_version(document))
        .execute(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(())
    }

    async fn raw_tests(&self) -> Result<Vec<(TenantId, Digest)>, CatalogError> {
        let rows = sqlx::query("SELECT tenant, digest FROM tests ORDER BY tenant, digest")
            .fetch_all(&self.pool)
            .await
            .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| Ok((row.tenant(0)?, row.digest(1)?)))
            .collect::<Result<_, Error>>()?)
    }

    async fn replace_test(
        &self,
        tenant_id: &TenantId,
        old: Digest,
        new: Digest,
        size: u64,
    ) -> Result<(), CatalogError> {
        if old == new {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(Error::from)?;
        sqlx::query(
            "INSERT INTO tests (tenant, digest, namespace, size, reserved_at, deletion_started_at) \
             SELECT tenant, $3, namespace, $4, reserved_at, deletion_started_at FROM tests \
             WHERE tenant = $1 AND digest = $2 ON CONFLICT (tenant, digest) DO NOTHING",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(old))
        .bind(bytes(new))
        .bind(int(size))
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
        sqlx::query("UPDATE workflow_tests SET digest = $3 WHERE tenant = $1 AND digest = $2")
            .bind(tenant(tenant_id))
            .bind(bytes(old))
            .bind(bytes(new))
            .execute(&mut *tx)
            .await
            .map_err(Error::from)?;
        sqlx::query("DELETE FROM tests WHERE tenant = $1 AND digest = $2")
            .bind(tenant(tenant_id))
            .bind(bytes(old))
            .execute(&mut *tx)
            .await
            .map_err(Error::from)?;
        tx.commit().await.map_err(Error::from)?;
        Ok(())
    }
}
