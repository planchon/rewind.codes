//! `CatalogStore` on Turso: the embedded control plane's catalog
//! (`archi/07-code-architecture.md`, "`CatalogStore`"; `archi/06-storage-and-scale.md`, "The
//! control plane"). The in-memory `MemoryCatalog` of `rewind-engine` is the reference of every
//! behavior here; both pass the same conformance suite.
//!
//! The catalog keeps scripts, toolsets, workflows, tests, workflow_tests, tenants, namespaces
//! with each one's `partition`, and subscriptions. Every call holds the store's one connection;
//! a write that changes something sends its change once it committed, while it still holds the
//! connection, so changes leave in commit order ([`TursoStore::changes`]). A write that changes
//! nothing sends nothing. Rows of namespaces created before the `partition` column have none:
//! `ensure_namespace` places the one it is asked for, [`TursoCatalog::place_namespaces`] all of
//! them.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;

use rewind_core::{Digest, PartitionId, RunId, Script, Timestamp, Toolset, Workflow};
use rewind_engine::ports::catalog::{NamespaceFilter, NamespacePage};
use rewind_engine::ports::{
    CatalogError, CatalogStore, CompiledScript, ControlPlaneChange, DeleteNamespace, DisplayName,
    DropNamespace, Namespace, NamespaceName, NamespaceWrite, Quotas, RunRef, ScriptTitle, Secret,
    StoredScript, StoredToolset, StoredWorkflow, Subscription, TEST_DELETION_TIMEOUT, TEST_GRACE,
    Tenant, TenantId, TestRecord, ToolDeclarations, WorkflowFilter, namespace_partition,
};
use turso::{Connection, Row, Value};

use crate::codec::{decode_event_types, decode_labels, encode_event_types, encode_labels};
use crate::namespaces::{
    self as tenancy, display_name_of, quota_columns, quota_values, quotas_from_row,
};
use crate::sql::{self, Columns, digest, int, limit, ms, opt_text, tenant, text, transaction, ts};
use crate::{Error, TursoStore};

impl From<Error> for CatalogError {
    fn from(err: Error) -> Self {
        CatalogError::Backend(err.to_string())
    }
}

/// `CatalogStore` on the database of a [`TursoStore`], whose connection and changes it shares
/// ([`TursoStore::catalog`]).
#[derive(Clone, Debug)]
pub struct TursoCatalog {
    store: TursoStore,
}

impl TursoStore {
    /// The control plane's catalog on this store's database.
    pub fn catalog(&self) -> TursoCatalog {
        TursoCatalog {
            store: self.clone(),
        }
    }
}

/// The columns of a [`StoredWorkflow`], read by [`workflow_from_row`].
macro_rules! workflow_columns {
    () => {
        "version, document, labels, created_at, number"
    };
}

fn workflow_from_row(row: &Row) -> Result<StoredWorkflow, Error> {
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
        number: row.u32(4)?,
    })
}

/// Whether a version's labels hold all of `required`.
fn carries(labels: &BTreeMap<String, String>, required: &BTreeMap<String, String>) -> bool {
    required
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value))
}

fn toolset_from_row(row: &Row, key: Digest, first: usize) -> Result<StoredToolset, Error> {
    let toolset = rewind_format::decode_toolset(&row.text(first)?)
        .map_err(|err| Error::Corrupt(format!("toolset {key}: {err}")))?;
    if toolset.digest() != key {
        return Err(Error::Corrupt(format!(
            "toolset {key} does not hash to its key"
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

fn name_of(name: String) -> Result<NamespaceName, Error> {
    NamespaceName::new(name.clone())
        .map_err(|_| Error::Corrupt(format!("invalid namespace {name:?}")))
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

// -------------------------------------------------------------------------------------------
// Namespaces

/// The columns of a [`Namespace`], read by [`namespace_from_row`].
fn namespace_columns() -> String {
    format!(
        "tenant, name, {}, tokens_revoked_before, created_at, last_active_at, deleting_since, \
         display_name, partition",
        quota_columns("")
    )
}

fn namespace_from_row(row: &Row) -> Result<Namespace, Error> {
    let name = name_of(row.text(1)?)?;
    let partition = row.opt_int(13)?.ok_or_else(|| {
        Error::Corrupt(format!(
            "namespace {name} has no partition: \
             TursoCatalog::place_namespaces has not placed it yet"
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

/// The row of a namespace, partition or not.
async fn namespace_row(
    connection: &Connection,
    tenant_id: &TenantId,
    name: &NamespaceName,
) -> Result<Option<Row>, Error> {
    sql::query_row(
        connection,
        &format!(
            "SELECT {} FROM namespaces WHERE tenant = ? AND name = ?",
            namespace_columns()
        ),
        vec![tenant(tenant_id), text(name.as_str())],
    )
    .await
}

async fn find_namespace(
    connection: &Connection,
    tenant_id: &TenantId,
    name: &NamespaceName,
) -> Result<Option<Namespace>, Error> {
    namespace_row(connection, tenant_id, name)
        .await?
        .map(|row| namespace_from_row(&row))
        .transpose()
}

/// The namespace, created at `now` in its partition if it does not exist, or placed in it if its
/// row has none; whether it was created or placed.
async fn ensure(
    connection: &Connection,
    tenant_id: &TenantId,
    name: &NamespaceName,
    partitions: NonZeroU16,
    now: Timestamp,
) -> Result<(Namespace, bool), Error> {
    let partition = int(u64::from(
        namespace_partition(tenant_id, name, partitions).get(),
    ));
    let changed = match namespace_row(connection, tenant_id, name).await? {
        Some(row) if row.opt_int(13)?.is_some() => return Ok((namespace_from_row(&row)?, false)),
        Some(_) => {
            sql::execute(
                connection,
                "UPDATE namespaces SET partition = ? WHERE tenant = ? AND name = ?",
                vec![partition, tenant(tenant_id), text(name.as_str())],
            )
            .await?
        }
        None => {
            sql::execute(
                connection,
                "INSERT INTO namespaces (tenant, name, created_at, last_active_at, partition) \
                 VALUES (?, ?, ?, ?, ?)",
                vec![
                    tenant(tenant_id),
                    text(name.as_str()),
                    ts(now),
                    ts(now),
                    partition,
                ],
            )
            .await?
        }
    };
    let namespace = find_namespace(connection, tenant_id, name)
        .await?
        .ok_or_else(|| Error::Corrupt(format!("namespace {name} vanished")))?;
    Ok((namespace, changed > 0))
}

fn written(namespace: Namespace) -> NamespaceWrite {
    if namespace.deleting_since.is_some() {
        NamespaceWrite::Deleting
    } else {
        NamespaceWrite::Written(namespace)
    }
}

impl TursoCatalog {
    fn send(&self, changes: impl IntoIterator<Item = ControlPlaneChange>) {
        self.store.send(changes);
    }

    /// Places in its partition, `hash(tenant, name) mod partitions`, every namespace whose row
    /// has none; returns how many. Run once, with the deployment's number of partitions, before
    /// the catalog serves: a namespace without a partition cannot be read.
    pub async fn place_namespaces(&self, partitions: NonZeroU16) -> Result<u64, CatalogError> {
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let placed = transaction(connection, async {
            let rows = sql::query(
                connection,
                "SELECT tenant, name FROM namespaces WHERE partition IS NULL ORDER BY tenant, name",
                Vec::new(),
            )
            .await?;
            let mut placed = Vec::with_capacity(rows.len());
            for row in &rows {
                let (tenant_id, name) = (row.tenant(0)?, name_of(row.text(1)?)?);
                let partition = namespace_partition(&tenant_id, &name, partitions);
                sql::execute(
                    connection,
                    "UPDATE namespaces SET partition = ? WHERE tenant = ? AND name = ?",
                    vec![
                        int(u64::from(partition.get())),
                        tenant(&tenant_id),
                        text(name.as_str()),
                    ],
                )
                .await?;
                placed.push(namespace_changed(&tenant_id, &name));
            }
            Ok::<_, Error>(placed)
        })
        .await?;
        let count = placed.len() as u64;
        self.send(placed);
        Ok(count)
    }

    /// Runs one write of a namespace's row and sends its change when it changed a row.
    async fn namespace_update(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        statement: &str,
        now: Timestamp,
    ) -> Result<bool, CatalogError> {
        let connection = self.store.connection().await?;
        let changed = sql::execute(
            &connection,
            statement,
            vec![ts(now), tenant(tenant_id), text(name.as_str())],
        )
        .await?;
        if changed == 0 {
            return Ok(false);
        }
        self.send([namespace_changed(tenant_id, name)]);
        Ok(true)
    }
}

impl CatalogStore for TursoCatalog {
    // ---------------------------------------------------------------------------------------
    // Scripts and toolsets

    async fn put_script(
        &self,
        tenant_id: &TenantId,
        script: &Script,
        compiled: &CompiledScript,
    ) -> Result<(), CatalogError> {
        let connection = self.store.connection().await?;
        let inserted = sql::execute(
            &connection,
            "INSERT OR IGNORE INTO scripts (tenant, digest, source, transpiled, source_map) \
             VALUES (?, ?, ?, ?, ?)",
            vec![
                tenant(tenant_id),
                digest(script.digest()),
                text(script.source()),
                text(compiled.javascript.as_str()),
                text(compiled.source_map.as_str()),
            ],
        )
        .await?;
        if inserted > 0 {
            self.send([ControlPlaneChange::Script {
                tenant: tenant_id.clone(),
                digest: script.digest(),
            }]);
        }
        Ok(())
    }

    async fn get_script(
        &self,
        tenant_id: &TenantId,
        key: Digest,
    ) -> Result<Option<StoredScript>, CatalogError> {
        let connection = self.store.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT source, transpiled, source_map FROM scripts WHERE tenant = ? AND digest = ?",
            vec![tenant(tenant_id), digest(key)],
        )
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let script = Script::new(row.text(0)?);
        if script.digest() != key {
            return Err(Error::Corrupt(format!("script {key} does not hash to its key")).into());
        }
        Ok(Some(StoredScript {
            script,
            compiled: CompiledScript {
                javascript: row.text(1)?,
                source_map: row.text(2)?,
            },
        }))
    }

    async fn set_script_title(
        &self,
        tenant_id: &TenantId,
        script: Digest,
        title: Option<&ScriptTitle>,
    ) -> Result<bool, CatalogError> {
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let stored = transaction(connection, async {
            let stored = sql::query_row(
                connection,
                "SELECT 1 FROM scripts WHERE tenant = ? AND digest = ?",
                vec![tenant(tenant_id), digest(script)],
            )
            .await?;
            if stored.is_none() {
                return Ok::<_, CatalogError>(false);
            }
            match title {
                Some(title) => {
                    sql::execute(
                        connection,
                        "INSERT OR REPLACE INTO script_titles (tenant, digest, title, \
                         description, updated_by, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
                        vec![
                            tenant(tenant_id),
                            digest(script),
                            text(title.title.as_str()),
                            opt_text(title.description.as_deref()),
                            text(title.updated_by.as_str()),
                            ts(title.updated_at),
                        ],
                    )
                    .await?
                }
                None => {
                    sql::execute(
                        connection,
                        "DELETE FROM script_titles WHERE tenant = ? AND digest = ?",
                        vec![tenant(tenant_id), digest(script)],
                    )
                    .await?
                }
            };
            Ok(true)
        })
        .await?;
        if stored {
            self.send([ControlPlaneChange::Script {
                tenant: tenant_id.clone(),
                digest: script,
            }]);
        }
        Ok(stored)
    }

    async fn script_titles(
        &self,
        tenant_id: &TenantId,
        scripts: &[Digest],
    ) -> Result<Vec<(Digest, ScriptTitle)>, CatalogError> {
        let connection = self.store.connection().await?;
        let mut titles = BTreeMap::new();
        for script in scripts.iter().collect::<BTreeSet<_>>() {
            let row = sql::query_row(
                &connection,
                "SELECT title, description, updated_by, updated_at FROM script_titles \
                 WHERE tenant = ? AND digest = ?",
                vec![tenant(tenant_id), digest(*script)],
            )
            .await?;
            if let Some(row) = row {
                titles.insert(
                    *script,
                    ScriptTitle {
                        title: row.text(0)?,
                        description: row.opt_text(1)?,
                        updated_by: row.text(2)?,
                        updated_at: row.ts(3)?,
                    },
                );
            }
        }
        Ok(titles.into_iter().collect())
    }

    async fn put_toolset(
        &self,
        tenant_id: &TenantId,
        toolset: &Toolset,
        declarations: &ToolDeclarations,
    ) -> Result<(), CatalogError> {
        let connection = self.store.connection().await?;
        let inserted = sql::execute(
            &connection,
            "INSERT OR IGNORE INTO toolsets (tenant, digest, spec, declarations, prompt) \
             VALUES (?, ?, ?, ?, ?)",
            vec![
                tenant(tenant_id),
                digest(toolset.digest()),
                text(rewind_format::encode_toolset(toolset)),
                text(declarations.typescript.as_str()),
                text(declarations.prompt.as_str()),
            ],
        )
        .await?;
        if inserted > 0 {
            self.send([ControlPlaneChange::Toolset {
                tenant: tenant_id.clone(),
                digest: toolset.digest(),
            }]);
        }
        Ok(())
    }

    async fn get_toolset(
        &self,
        tenant_id: &TenantId,
        key: Digest,
    ) -> Result<Option<StoredToolset>, CatalogError> {
        let connection = self.store.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT spec, declarations, prompt FROM toolsets WHERE tenant = ? AND digest = ?",
            vec![tenant(tenant_id), digest(key)],
        )
        .await?;
        Ok(row.map(|row| toolset_from_row(&row, key, 0)).transpose()?)
    }

    async fn list_toolsets(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<StoredToolset>, CatalogError> {
        let connection = self.store.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT digest, spec, declarations, prompt FROM toolsets WHERE tenant = ? \
             ORDER BY digest",
            vec![tenant(tenant_id)],
        )
        .await?;
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
        let labels = workflow.labels();
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let version = workflow.version();
        let (stored, inserted) = transaction(connection, async {
            let existing = sql::query_row(
                connection,
                concat!(
                    "SELECT ",
                    workflow_columns!(),
                    " FROM workflows WHERE tenant = ? AND version = ?"
                ),
                vec![tenant(tenant_id), digest(version)],
            )
            .await?;
            if let Some(existing) = existing {
                return Ok((workflow_from_row(&existing)?, false));
            }
            // A new version references only tests whose blobs are kept: rows not being deleted.
            let tests: BTreeSet<Digest> = workflow.suite().tests.iter().map(|t| t.test).collect();
            for test in &tests {
                let live = sql::query_row(
                    connection,
                    "SELECT 1 FROM tests WHERE tenant = ? AND digest = ? \
                     AND deletion_started_at IS NULL",
                    vec![tenant(tenant_id), digest(*test)],
                )
                .await?;
                if live.is_none() {
                    return Err(missing_test(*test));
                }
            }
            for test in &tests {
                sql::execute(
                    connection,
                    "INSERT OR IGNORE INTO workflow_tests (tenant, version, digest) \
                     VALUES (?, ?, ?)",
                    vec![tenant(tenant_id), digest(version), digest(*test)],
                )
                .await?;
            }
            // Only the versions of one workflow are compared by the order they were stored in,
            // and numbered: a new version takes the next number of its workflow.
            let (order, number) = match sql::query_row(
                connection,
                "SELECT COALESCE(MAX(stored_order), 0) + 1, COALESCE(MAX(number), 0) + 1 \
                 FROM workflows WHERE tenant = ? AND name = ?",
                vec![tenant(tenant_id), text(workflow.name())],
            )
            .await?
            {
                Some(row) => (row.u64(0)?, row.u32(1)?),
                None => (1, 1),
            };
            sql::execute(
                connection,
                "INSERT INTO workflows (tenant, version, name, document, labels, created_at, \
                 stored_order, number) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    tenant(tenant_id),
                    digest(version),
                    text(workflow.name()),
                    text(rewind_format::encode_workflow(workflow)),
                    text(encode_labels(labels)),
                    ts(now),
                    int(order),
                    int(u64::from(number)),
                ],
            )
            .await?;
            let stored = StoredWorkflow {
                workflow: workflow.clone(),
                labels: labels.clone(),
                created_at: now,
                number,
            };
            Ok::<_, CatalogError>((stored, true))
        })
        .await?;
        if inserted {
            self.send([ControlPlaneChange::Workflow {
                tenant: tenant_id.clone(),
                name: workflow.name().to_owned(),
                version,
            }]);
        }
        Ok(stored)
    }

    async fn get_workflow(
        &self,
        tenant_id: &TenantId,
        version: Digest,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        let connection = self.store.connection().await?;
        let row = sql::query_row(
            &connection,
            concat!(
                "SELECT ",
                workflow_columns!(),
                " FROM workflows WHERE tenant = ? AND version = ?"
            ),
            vec![tenant(tenant_id), digest(version)],
        )
        .await?;
        Ok(row.map(|row| workflow_from_row(&row)).transpose()?)
    }

    async fn workflow_number(
        &self,
        tenant_id: &TenantId,
        name: &str,
        number: u32,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        let connection = self.store.connection().await?;
        let row = sql::query_row(
            &connection,
            concat!(
                "SELECT ",
                workflow_columns!(),
                " FROM workflows WHERE tenant = ? AND name = ? AND number = ?"
            ),
            vec![tenant(tenant_id), text(name), int(u64::from(number))],
        )
        .await?;
        Ok(row.map(|row| workflow_from_row(&row)).transpose()?)
    }

    async fn workflow_versions(
        &self,
        tenant_id: &TenantId,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        let connection = self.store.connection().await?;
        let rows = sql::query(
            &connection,
            concat!(
                "SELECT ",
                workflow_columns!(),
                " FROM workflows WHERE tenant = ? AND name = ? \
                 ORDER BY created_at DESC, stored_order DESC"
            ),
            vec![tenant(tenant_id), text(name)],
        )
        .await?;
        let mut versions = Vec::new();
        for row in &rows {
            // Labels are filtered here: a version's labels are one JSON column.
            if carries(&decode_labels(&row.text(2)?)?, labels) {
                versions.push(workflow_from_row(row)?);
            }
        }
        Ok(versions)
    }

    async fn workflows_with_script(
        &self,
        tenant_id: &TenantId,
        script: Digest,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        let connection = self.store.connection().await?;
        // The script is read from the version's document: a tenant has few versions.
        let rows = sql::query(
            &connection,
            concat!(
                "SELECT ",
                workflow_columns!(),
                " FROM workflows WHERE tenant = ? AND json_extract(document, '$.script') = ? \
                 ORDER BY created_at DESC, stored_order DESC"
            ),
            vec![tenant(tenant_id), text(script.to_hex())],
        )
        .await?;
        let mut versions = Vec::new();
        for row in &rows {
            if carries(&decode_labels(&row.text(2)?)?, labels) {
                versions.push(workflow_from_row(row)?);
            }
        }
        Ok(versions)
    }

    async fn latest_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        if filter.limit == Some(0) {
            return Ok(Vec::new());
        }
        let connection = self.store.connection().await?;
        let mut query = concat!(
            "SELECT ",
            workflow_columns!(),
            ", name FROM workflows WHERE tenant = ?"
        )
        .to_owned();
        let mut params = vec![tenant(&filter.tenant)];
        if let Some(after) = &filter.after {
            query.push_str(" AND name > ?");
            params.push(text(after.as_str()));
        }
        // Each workflow's versions, newest first: the first carrying the labels is its latest.
        query.push_str(" ORDER BY name, created_at DESC, stored_order DESC");
        let rows = sql::query_dynamic(&connection, &query, params).await?;
        let mut latest: Vec<StoredWorkflow> = Vec::new();
        let mut seen: Option<String> = None;
        for row in &rows {
            if filter.limit.is_some_and(|max| latest.len() >= max) {
                break;
            }
            let name = row.text(5)?;
            if seen.as_deref() == Some(name.as_str()) {
                continue;
            }
            if carries(&decode_labels(&row.text(2)?)?, &filter.labels) {
                latest.push(workflow_from_row(row)?);
                seen = Some(name);
            }
        }
        Ok(latest)
    }

    // ---------------------------------------------------------------------------------------
    // Tests

    async fn reserve_tests(
        &self,
        tenant_id: &TenantId,
        tests: &[TestRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, CatalogError> {
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let mut reserved = Vec::new();
        let deleting = transaction(connection, async {
            let mut deleting = Vec::new();
            for record in tests {
                let row = sql::query_row(
                    connection,
                    "SELECT deletion_started_at IS NOT NULL FROM tests \
                     WHERE tenant = ? AND digest = ?",
                    vec![tenant(tenant_id), digest(record.digest)],
                )
                .await?;
                match row {
                    Some(row) if row.boolean(0)? => {
                        deleting.push(record.digest);
                        continue;
                    }
                    // An existing row starts its grace again.
                    Some(_) => {
                        sql::execute(
                            connection,
                            "UPDATE tests SET reserved_at = ? WHERE tenant = ? AND digest = ?",
                            vec![ts(now), tenant(tenant_id), digest(record.digest)],
                        )
                        .await?;
                    }
                    None => {
                        sql::execute(
                            connection,
                            "INSERT INTO tests (tenant, digest, namespace, size, reserved_at, \
                             deletion_started_at) VALUES (?, ?, ?, ?, ?, NULL)",
                            vec![
                                tenant(tenant_id),
                                digest(record.digest),
                                opt_text(record.namespace.as_ref().map(NamespaceName::as_str)),
                                int(record.size),
                                ts(now),
                            ],
                        )
                        .await?;
                    }
                }
                reserved.push(test_changed(tenant_id, record.digest));
            }
            Ok::<_, Error>(deleting)
        })
        .await?;
        self.send(reserved);
        Ok(deleting)
    }

    async fn get_test(
        &self,
        tenant_id: &TenantId,
        key: Digest,
    ) -> Result<Option<TestRecord>, CatalogError> {
        let connection = self.store.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT namespace, size FROM tests \
             WHERE tenant = ? AND digest = ? AND deletion_started_at IS NULL",
            vec![tenant(tenant_id), digest(key)],
        )
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(TestRecord {
            digest: key,
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
        let connection = self.store.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT digest FROM tests WHERE tenant = ? AND namespace = ? ORDER BY digest LIMIT ?",
            vec![tenant(tenant_id), text(namespace.as_str()), limit(max)],
        )
        .await?;
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
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let due = transaction(connection, async {
            let rows = sql::query(
                connection,
                "SELECT t.tenant, t.digest FROM tests t \
                 WHERE t.reserved_at <= ? - ? \
                 AND (t.deletion_started_at IS NULL OR t.deletion_started_at <= ? - ?) \
                 AND NOT EXISTS (SELECT 1 FROM workflow_tests w \
                                 WHERE w.tenant = t.tenant AND w.digest = t.digest) \
                 ORDER BY t.tenant, t.digest LIMIT ?",
                vec![
                    ts(now),
                    ms(TEST_GRACE),
                    ts(now),
                    ms(TEST_DELETION_TIMEOUT),
                    limit(max),
                ],
            )
            .await?;
            let due = rows
                .iter()
                .map(|row| Ok((row.tenant(0)?, row.digest(1)?)))
                .collect::<Result<Vec<_>, Error>>()?;
            for (owner, test) in &due {
                sql::execute(
                    connection,
                    "UPDATE tests SET deletion_started_at = ? WHERE tenant = ? AND digest = ?",
                    vec![ts(now), tenant(owner), digest(*test)],
                )
                .await?;
            }
            Ok::<_, Error>(due)
        })
        .await?;
        self.send(due.iter().map(|(owner, test)| test_changed(owner, *test)));
        Ok(due)
    }

    async fn drop_tests(
        &self,
        tests: &[(TenantId, Digest)],
        erased: bool,
    ) -> Result<usize, CatalogError> {
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let dropped = transaction(connection, async {
            let mut dropped = Vec::new();
            for (owner, test) in tests {
                let deleted = if erased {
                    // A namespace's erasure: whatever references the test.
                    sql::execute(
                        connection,
                        "DELETE FROM workflow_tests WHERE tenant = ? AND digest = ?",
                        vec![tenant(owner), digest(*test)],
                    )
                    .await?;
                    sql::execute(
                        connection,
                        "DELETE FROM tests WHERE tenant = ? AND digest = ?",
                        vec![tenant(owner), digest(*test)],
                    )
                    .await?
                } else {
                    sql::execute(
                        connection,
                        "DELETE FROM tests WHERE tenant = ? AND digest = ? \
                         AND deletion_started_at IS NOT NULL \
                         AND NOT EXISTS (SELECT 1 FROM workflow_tests w \
                                         WHERE w.tenant = tests.tenant \
                                         AND w.digest = tests.digest)",
                        vec![tenant(owner), digest(*test)],
                    )
                    .await?
                };
                if deleted > 0 {
                    dropped.push(test_changed(owner, *test));
                }
            }
            Ok::<_, Error>(dropped)
        })
        .await?;
        let count = dropped.len();
        self.send(dropped);
        Ok(count)
    }

    // ---------------------------------------------------------------------------------------
    // Subscriptions

    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), CatalogError> {
        let connection = self.store.connection().await?;
        sql::execute(
            &connection,
            "INSERT OR REPLACE INTO subscriptions (tenant, id, url, secret, events, labels) \
             VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                tenant(&subscription.tenant),
                text(subscription.id.as_str()),
                text(subscription.url.as_str()),
                text(subscription.secret.expose()),
                text(encode_event_types(&subscription.events)),
                text(encode_labels(&subscription.labels)),
            ],
        )
        .await?;
        self.send([ControlPlaneChange::Subscription {
            tenant: subscription.tenant.clone(),
            id: subscription.id.clone(),
        }]);
        Ok(())
    }

    async fn subscriptions(&self, tenant_id: &TenantId) -> Result<Vec<Subscription>, CatalogError> {
        let connection = self.store.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT id, url, secret, events, labels FROM subscriptions WHERE tenant = ? \
             ORDER BY id",
            vec![tenant(tenant_id)],
        )
        .await?;
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
        let connection = self.store.connection().await?;
        let deleted = sql::execute(
            &connection,
            "DELETE FROM subscriptions WHERE tenant = ? AND id = ?",
            vec![tenant(tenant_id), text(id)],
        )
        .await?;
        if deleted == 0 {
            return Ok(false);
        }
        self.send([ControlPlaneChange::Subscription {
            tenant: tenant_id.clone(),
            id: id.to_owned(),
        }]);
        Ok(true)
    }

    // ---------------------------------------------------------------------------------------
    // Tenants

    async fn put_tenant(&self, record: &Tenant) -> Result<(), CatalogError> {
        let connection = self.store.connection().await?;
        tenancy::put_tenant(&connection, record).await?;
        self.send([ControlPlaneChange::Tenant {
            tenant: record.id.clone(),
        }]);
        Ok(())
    }

    async fn get_tenant(&self, id: &TenantId) -> Result<Option<Tenant>, CatalogError> {
        let connection = self.store.connection().await?;
        Ok(tenancy::get_tenant(&connection, id).await?)
    }

    async fn list_tenants(&self) -> Result<Vec<Tenant>, CatalogError> {
        let connection = self.store.connection().await?;
        Ok(tenancy::list_tenants(&connection).await?)
    }

    async fn legacy_partition(&self, run: RunId) -> Result<Option<PartitionId>, CatalogError> {
        let connection = self.store.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT partition FROM legacy_runs WHERE id = ?",
            vec![sql::run_id(run)],
        )
        .await?;
        row.map(|row| {
            let partition = row.int(0)?;
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
        let connection = self.store.connection().await?;
        sql::execute(
            &connection,
            "INSERT OR REPLACE INTO legacy_runs (id, tenant, partition) VALUES (?, ?, ?)",
            vec![
                sql::run_id(run.id),
                sql::tenant(&run.tenant),
                sql::int(u64::from(partition.get())),
            ],
        )
        .await?;
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
        let connection = self.store.connection().await?;
        let (namespace, changed) = ensure(&connection, tenant_id, name, partitions, now).await?;
        if changed {
            self.send([namespace_changed(tenant_id, name)]);
        }
        Ok(written(namespace))
    }

    async fn put_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        exceptions: Option<&Quotas>,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> Result<NamespaceWrite, CatalogError> {
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let (namespace, changed) = transaction(connection, async {
            let (namespace, created) = ensure(connection, tenant_id, name, partitions, now).await?;
            if namespace.deleting_since.is_some() {
                return Ok((None, created));
            }
            let Some(exceptions) = exceptions else {
                return Ok((Some(namespace), created));
            };
            let mut params = quota_values(exceptions).to_vec();
            params.extend([tenant(tenant_id), text(name.as_str())]);
            sql::execute(
                connection,
                "UPDATE namespaces SET cpu_per_month_ms = ?, active_runs = ?, \
                 runs_per_month = ?, scripts = ?, max_ttl_ms = ?, max_script_lines = ? \
                 WHERE tenant = ? AND name = ?",
                params,
            )
            .await?;
            let namespace = Namespace {
                exceptions: exceptions.clone(),
                ..namespace
            };
            Ok::<_, Error>((Some(namespace), true))
        })
        .await?;
        if changed {
            self.send([namespace_changed(tenant_id, name)]);
        }
        Ok(namespace.map_or(NamespaceWrite::Deleting, NamespaceWrite::Written))
    }

    async fn get_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
    ) -> Result<Option<Namespace>, CatalogError> {
        let connection = self.store.connection().await?;
        Ok(find_namespace(&connection, tenant_id, name).await?)
    }

    async fn set_namespace_display_name(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
        display_name: Option<&DisplayName>,
    ) -> Result<Option<NamespaceWrite>, CatalogError> {
        let connection = self.store.connection().await?;
        let Some(namespace) = find_namespace(&connection, tenant_id, name).await? else {
            return Ok(None);
        };
        if namespace.deleting_since.is_some() {
            return Ok(Some(NamespaceWrite::Deleting));
        }
        sql::execute(
            &connection,
            "UPDATE namespaces SET display_name = ? WHERE tenant = ? AND name = ?",
            vec![
                opt_text(display_name.map(DisplayName::as_str)),
                tenant(tenant_id),
                text(name.as_str()),
            ],
        )
        .await?;
        self.send([namespace_changed(tenant_id, name)]);
        Ok(Some(NamespaceWrite::Written(Namespace {
            display_name: display_name.cloned(),
            ..namespace
        })))
    }

    async fn namespace_display_names(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<(NamespaceName, DisplayName)>, CatalogError> {
        let connection = self.store.connection().await?;
        Ok(tenancy::display_names(&connection, tenant_id).await?)
    }

    async fn list_namespaces(
        &self,
        filter: &NamespaceFilter,
    ) -> Result<NamespacePage, CatalogError> {
        let connection = self.store.connection().await?;
        let mut statement = format!(
            "SELECT {} FROM namespaces WHERE tenant = ?",
            namespace_columns()
        );
        let mut params: Vec<Value> = vec![tenant(&filter.tenant)];
        if let Some(only) = &filter.name {
            statement.push_str(" AND name = ?");
            params.push(text(only.as_str()));
        }
        statement.push_str(" ORDER BY name");
        let rows = sql::query_dynamic(&connection, &statement, params).await?;
        let mut matching = Vec::new();
        for row in &rows {
            let namespace = namespace_from_row(row)?;
            let matches = filter.prefix.as_deref().is_none_or(|prefix| {
                namespace.name.as_str().starts_with(prefix)
                    || namespace
                        .display_name
                        .as_ref()
                        .is_some_and(|display| display.starts_with(prefix))
            });
            if matches {
                matching.push(namespace);
            }
        }
        let total = matching.len() as u64;
        let namespaces = matching
            .into_iter()
            .filter(|namespace| {
                filter
                    .after
                    .as_ref()
                    .is_none_or(|after| namespace.name > *after)
            })
            .take(filter.limit)
            .collect();
        Ok(NamespacePage { namespaces, total })
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
            "UPDATE namespaces SET tokens_revoked_before = ? WHERE tenant = ? AND name = ?",
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
                "UPDATE namespaces SET deleting_since = COALESCE(deleting_since, ?) \
                 WHERE tenant = ? AND name = ?",
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
        let connection = self.store.connection().await?;
        Ok(tenancy::namespaces_to_erase(&connection, max).await?)
    }

    async fn drop_namespace(
        &self,
        tenant_id: &TenantId,
        name: &NamespaceName,
    ) -> Result<DropNamespace, CatalogError> {
        let connection = self.store.connection().await?;
        let dropped = sql::execute(
            &connection,
            "DELETE FROM namespaces WHERE tenant = ? AND name = ? AND deleting_since IS NOT NULL",
            vec![tenant(tenant_id), text(name.as_str())],
        )
        .await?;
        if dropped == 0 {
            return Ok(DropNamespace::NotFound);
        }
        self.send([namespace_changed(tenant_id, name)]);
        Ok(DropNamespace::Dropped)
    }
}
