//! `CatalogStore` on Postgres: the control plane's catalog (`archi/07-code-architecture.md`,
//! "`CatalogStore`"; `archi/06-storage-and-scale.md`, "The control plane"). The in-memory
//! `MemoryCatalog` of `rewind-engine` is the reference of every behavior here; both pass the same
//! conformance suite.
//!
//! The catalog keeps scripts, toolsets, workflows, tests, workflow_tests, tenants, namespaces
//! with each one's `partition`, and subscriptions. Every write is one transaction that sends what
//! it changed with `NOTIFY` (`crate::changes`); a write that changes nothing sends nothing. Test
//! rows are locked by tenant and digest: a new version locks the rows of its suite (`FOR SHARE`)
//! until it has written its references, and the cleanup marks only rows it holds
//! (`FOR UPDATE SKIP LOCKED`).
//!
//! `ensure_namespace` reads first and writes only a namespace it creates, or a row without a
//! partition, which it places: starting a run writes nothing here. Rows without a partition are
//! those created before the column; [`PostgresCatalog::place_namespaces`] places them all once
//! (`archi/08-decisions.md`, "Taken while building the control plane of the run plane").

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;

use rewind_core::{Digest, PartitionId, RunId, Script, Timestamp, Toolset, Workflow};
use rewind_engine::ports::catalog::{NamespaceFilter, NamespacePage};
use rewind_engine::ports::{
    CatalogError, CatalogStore, CompiledScript, ControlPlaneChange, DeleteNamespace, DisplayName,
    DropNamespace, Namespace, NamespaceName, NamespaceWrite, Quotas, RunRef, Secret, StoredScript,
    StoredToolset, StoredWorkflow, Subscription, TEST_DELETION_TIMEOUT, TEST_GRACE, Tenant,
    TenantId, TestRecord, ToolDeclarations, WorkflowFilter, namespace_partition,
};
use sqlx::postgres::PgRow;
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

/// The columns of a [`StoredWorkflow`], read by [`workflow_from_row`].
macro_rules! workflow_columns {
    () => {
        "version, document, labels::text, created_at"
    };
}

/// A test (`t`) the cleanup may start deleting at `$1`: stored or reserved at least its grace
/// (`$2`) before, not being deleted, or for at least `$3`, and referenced by no version.
macro_rules! test_due {
    () => {
        "t.reserved_at <= $1 - $2 \
         AND (t.deletion_started_at IS NULL OR t.deletion_started_at <= $1 - $3) \
         AND NOT EXISTS (SELECT 1 FROM workflow_tests w \
                         WHERE w.tenant = t.tenant AND w.digest = t.digest)"
    };
}

fn workflow_from_row(row: &PgRow) -> Result<StoredWorkflow, Error> {
    let version = row.digest(0)?;
    let workflow = rewind_format::decode_workflow(&row.text(1)?)
        .map_err(|err| Error::Corrupt(format!("workflow {version}: {err}")))?;
    if workflow.version() != version {
        return Err(Error::Corrupt(format!(
            "workflow {version} does not hash to its key"
        )));
    }
    Ok(StoredWorkflow {
        workflow,
        labels: decode_labels(&row.text(2)?)?,
        created_at: row.ts(3)?,
    })
}

fn toolset_from_row(row: &PgRow, digest: Digest, first: usize) -> Result<StoredToolset, Error> {
    let toolset = rewind_format::decode_toolset(&row.text(first)?)
        .map_err(|err| Error::Corrupt(format!("toolset {digest}: {err}")))?;
    if toolset.digest() != digest {
        return Err(Error::Corrupt(format!(
            "toolset {digest} does not hash to its key"
        )));
    }
    Ok(StoredToolset {
        toolset,
        declarations: ToolDeclarations {
            typescript: row.text(first + 1)?,
            prompt: row.text(first + 2)?,
        },
    })
}

/// A version referencing a test the catalog cannot keep for it.
fn missing_test(test: Digest) -> CatalogError {
    CatalogError::Conflict(format!(
        "the test {test} has no row, or its deletion has started: reserve it and write its blob \
         again before saving the version"
    ))
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

    async fn put_workflow_once(
        &self,
        tenant_id: &TenantId,
        workflow: &Workflow,
        now: Timestamp,
    ) -> Result<Option<StoredWorkflow>, Fail> {
        let version = workflow.version();
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(concat!(
            "INSERT INTO workflows (tenant, version, name, document, labels, created_at) \
             VALUES ($1, $2, $3, $4, $5::jsonb, $6) ON CONFLICT DO NOTHING RETURNING ",
            workflow_columns!()
        ))
        .bind(tenant(tenant_id))
        .bind(bytes(version))
        .bind(workflow.name())
        .bind(rewind_format::encode_workflow(workflow))
        .bind(encode_labels(workflow.labels()))
        .bind(ts(now))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = inserted else {
            // Stored before: nothing changes.
            return Ok(None);
        };
        let tests: BTreeSet<Digest> = workflow.suite().tests.iter().map(|t| t.test).collect();
        if !tests.is_empty() {
            let digests: Vec<Vec<u8>> = tests.iter().map(|test| bytes(*test)).collect();
            // The suite's rows not being deleted, locked by digest until the commit: no cleanup
            // marks them before the references below are written.
            let rows = sqlx::query(
                "SELECT digest FROM tests WHERE tenant = $1 AND digest = ANY($2) \
                 AND deletion_started_at IS NULL ORDER BY digest FOR SHARE",
            )
            .bind(tenant(tenant_id))
            .bind(&digests)
            .fetch_all(&mut *tx)
            .await?;
            let live: BTreeSet<Digest> = rows
                .iter()
                .map(|row| row.digest(0))
                .collect::<Result<_, Error>>()?;
            if let Some(missing) = tests.iter().find(|test| !live.contains(test)) {
                return Err(Failure::Port(missing_test(*missing)));
            }
            sqlx::query(
                "INSERT INTO workflow_tests (tenant, version, digest) \
                 SELECT $1, $2, digest FROM unnest($3::bytea[]) AS t(digest) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(tenant(tenant_id))
            .bind(bytes(version))
            .bind(digests)
            .execute(&mut *tx)
            .await?;
        }
        let stored = workflow_from_row(&row)?;
        let change = ControlPlaneChange::Workflow {
            tenant: tenant_id.clone(),
            name: workflow.name().to_owned(),
            version,
        };
        notify(&mut tx, &[change]).await?;
        tx.commit().await?;
        Ok(Some(stored))
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
        // version that referenced a row before it was locked keeps it.
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
            // A namespace's erasure: the rows go whatever references them, then the
            // references, read after the rows' locks: those of a version written meanwhile go
            // too.
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
        notify(&mut tx, &[namespace_changed(tenant_id, name)]).await?;
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
    // Scripts and toolsets

    async fn put_script(
        &self,
        tenant_id: &TenantId,
        script: &Script,
        compiled: &CompiledScript,
    ) -> Result<(), CatalogError> {
        let mut tx = self.pool.begin().await.map_err(Error::from)?;
        let inserted = sqlx::query(
            "INSERT INTO scripts (tenant, digest, source, transpiled, source_map) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(script.digest()))
        .bind(script.source())
        .bind(compiled.javascript.as_str())
        .bind(compiled.source_map.as_str())
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
        if inserted.rows_affected() > 0 {
            let change = ControlPlaneChange::Script {
                tenant: tenant_id.clone(),
                digest: script.digest(),
            };
            notify(&mut tx, &[change]).await?;
        }
        tx.commit().await.map_err(Error::from)?;
        Ok(())
    }

    async fn get_script(
        &self,
        tenant_id: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredScript>, CatalogError> {
        let row = sqlx::query(
            "SELECT source, transpiled, source_map FROM scripts WHERE tenant = $1 AND digest = $2",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(digest))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let script = Script::new(row.text(0)?);
        if script.digest() != digest {
            return Err(Error::Corrupt(format!("script {digest} does not hash to its key")).into());
        }
        Ok(Some(StoredScript {
            script,
            compiled: CompiledScript {
                javascript: row.text(1)?,
                source_map: row.text(2)?,
            },
        }))
    }

    async fn put_toolset(
        &self,
        tenant_id: &TenantId,
        toolset: &Toolset,
        declarations: &ToolDeclarations,
    ) -> Result<(), CatalogError> {
        let mut tx = self.pool.begin().await.map_err(Error::from)?;
        let inserted = sqlx::query(
            "INSERT INTO toolsets (tenant, digest, spec, declarations, prompt) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(toolset.digest()))
        .bind(rewind_format::encode_toolset(toolset))
        .bind(declarations.typescript.as_str())
        .bind(declarations.prompt.as_str())
        .execute(&mut *tx)
        .await
        .map_err(Error::from)?;
        if inserted.rows_affected() > 0 {
            let change = ControlPlaneChange::Toolset {
                tenant: tenant_id.clone(),
                digest: toolset.digest(),
            };
            notify(&mut tx, &[change]).await?;
        }
        tx.commit().await.map_err(Error::from)?;
        Ok(())
    }

    async fn get_toolset(
        &self,
        tenant_id: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredToolset>, CatalogError> {
        let row = sqlx::query(
            "SELECT spec, declarations, prompt FROM toolsets WHERE tenant = $1 AND digest = $2",
        )
        .bind(tenant(tenant_id))
        .bind(bytes(digest))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(row
            .map(|row| toolset_from_row(&row, digest, 0))
            .transpose()?)
    }

    async fn list_toolsets(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<StoredToolset>, CatalogError> {
        let rows = sqlx::query(
            "SELECT digest, spec, declarations, prompt FROM toolsets WHERE tenant = $1 \
             ORDER BY digest",
        )
        .bind(tenant(tenant_id))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(|row| toolset_from_row(row, row.digest(0)?, 1))
            .collect::<Result<_, Error>>()?)
    }

    // ---------------------------------------------------------------------------------------
    // Workflows

    async fn put_workflow(
        &self,
        tenant_id: &TenantId,
        workflow: &Workflow,
        now: Timestamp,
    ) -> Result<StoredWorkflow, CatalogError> {
        let version = workflow.version();
        if let Some(stored) = retrying(|| self.put_workflow_once(tenant_id, workflow, now)).await? {
            return Ok(stored);
        }
        // Stored before: the version as it was stored first.
        let stored = self.get_workflow(tenant_id, version).await?;
        stored.ok_or_else(|| {
            CatalogError::Backend(format!("workflow {version} was neither stored nor found"))
        })
    }

    async fn get_workflow(
        &self,
        tenant_id: &TenantId,
        version: Digest,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        let row = sqlx::query(concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows WHERE tenant = $1 AND version = $2"
        ))
        .bind(tenant(tenant_id))
        .bind(bytes(version))
        .fetch_optional(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(row.map(|row| workflow_from_row(&row)).transpose()?)
    }

    async fn workflow_versions(
        &self,
        tenant_id: &TenantId,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        let rows = sqlx::query(concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows WHERE tenant = $1 AND name = $2 AND labels @> $3::jsonb \
             ORDER BY created_at DESC, ordinal DESC"
        ))
        .bind(tenant(tenant_id))
        .bind(name)
        .bind(encode_labels(labels))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(workflow_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn workflows_with_script(
        &self,
        tenant_id: &TenantId,
        script: Digest,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        // The script is read from the version's document: a tenant has few versions.
        let rows = sqlx::query(concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows WHERE tenant = $1 AND labels @> $2::jsonb \
             AND (document::jsonb ->> 'script') = $3 \
             ORDER BY created_at DESC, ordinal DESC"
        ))
        .bind(tenant(tenant_id))
        .bind(encode_labels(labels))
        .bind(script.to_hex())
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(workflow_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn latest_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        let rows = sqlx::query(concat!(
            "SELECT DISTINCT ON (name) ",
            workflow_columns!(),
            " FROM workflows WHERE tenant = $1 AND labels @> $2::jsonb \
             AND ($3::text IS NULL OR name > $3) \
             ORDER BY name, created_at DESC, ordinal DESC LIMIT $4"
        ))
        .bind(tenant(&filter.tenant))
        .bind(encode_labels(&filter.labels))
        .bind(filter.after.as_deref())
        .bind(filter.limit.map(limit))
        .fetch_all(&self.pool)
        .await
        .map_err(Error::from)?;
        Ok(rows
            .iter()
            .map(workflow_from_row)
            .collect::<Result<_, Error>>()?)
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
            "INSERT INTO subscriptions (tenant, id, url, secret, events, labels) \
             VALUES ($1, $2, $3, $4, $5, $6::jsonb) \
             ON CONFLICT (tenant, id) DO UPDATE SET url = EXCLUDED.url, \
                 secret = EXCLUDED.secret, events = EXCLUDED.events, labels = EXCLUDED.labels",
        )
        .bind(tenant(&subscription.tenant))
        .bind(subscription.id.as_str())
        .bind(subscription.url.as_str())
        .bind(subscription.secret.expose())
        .bind(encode_event_types(&subscription.events))
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
            "SELECT id, url, secret, events, labels::text FROM subscriptions \
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
                    events: decode_event_types(&row.text(3)?)?,
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
