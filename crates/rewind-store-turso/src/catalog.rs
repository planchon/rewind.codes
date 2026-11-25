//! `CatalogStore` on Turso: the embedded control plane's catalog
//! (`archi/07-code-architecture.md`, "`CatalogStore`"; `archi/06-storage-and-scale.md`, "The
//! control plane"). The in-memory `MemoryCatalog` of `rewind-engine` is the reference of every
//! behavior here; both pass the same conformance suite.
//!
//! The catalog keeps code and action sets; workflows, their versions, the codes each scope has
//! seen and their suites; tests, tenants, namespaces with each one's `partition`, and
//! subscriptions; and, for `CatalogMigration`, the titles people gave codes before the workflow
//! model. Every call holds the store's one connection; a write that changes something sends its
//! change once it committed, while it still holds the connection, so changes leave in commit
//! order ([`TursoStore::changes`]). A write that changes nothing sends nothing. Rows of
//! namespaces created before the `partition` column have none: `ensure_namespace` places the one
//! it is asked for, [`TursoCatalog::place_namespaces`] all of them.
//!
//! Labels are JSON objects in their columns. Whether a row's labels carry a caller's is asked in
//! SQL (`carries!`): the caller's labels are bound as a JSON object, and each of them must be
//! one of the row's.

use std::collections::BTreeSet;
use std::num::NonZeroU16;

use rewind_core::{
    ActionSet, Code, Digest, PartitionId, RunId, Timestamp, WorkflowId, WorkflowVersion,
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
use turso::{Connection, Row, Value};

use crate::codec::{decode_event_types, decode_labels, encode_event_types, encode_labels};
use crate::namespaces::{
    self as tenancy, display_name_of, quota_columns, quota_values, quotas_from_row,
};
use crate::sql::{
    self, Columns, digest, int, limit, ms, opt_text, opt_ts, tenant, text, transaction, ts,
};
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

fn action_set_from_row(row: &Row, key: Digest, first: usize) -> Result<StoredActionSet, Error> {
    let action_set = rewind_format::decode_action_set(&row.text(first)?)
        .map_err(|err| Error::Corrupt(format!("action set {key}: {err}")))?;
    if action_set.digest() != key {
        return Err(Error::Corrupt(format!(
            "action set {key} does not hash to its key"
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

/// A suite referencing a test the catalog cannot keep for it.
fn missing_test(test: Digest) -> CatalogError {
    CatalogError::Conflict(format!(
        "the test {test} has no row, or its deletion has started: reserve it and write its blob \
         again before giving it to a suite"
    ))
}

fn no_workflow(id: &WorkflowId) -> CatalogError {
    CatalogError::Conflict(format!("the workflow {id} does not exist"))
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

fn workflow_changed(tenant_id: &TenantId, id: &WorkflowId) -> ControlPlaneChange {
    ControlPlaneChange::Workflow {
        tenant: tenant_id.clone(),
        id: id.clone(),
    }
}

// -------------------------------------------------------------------------------------------
// Workflows

/// The condition that the labels of the JSON object column `$column` carry every label of the
/// JSON object bound to its one `?`: the caller's. `{}` is carried by every row.
macro_rules! carries {
    ($column:literal) => {
        concat!(
            "NOT EXISTS (SELECT 1 FROM json_each(?) AS wanted WHERE NOT EXISTS (",
            "SELECT 1 FROM json_each(",
            $column,
            ") AS held WHERE held.key = wanted.key AND held.value = wanted.value))"
        )
    };
}

/// The columns of a [`StoredWorkflow`] of `workflows w`, read by [`workflow_from_row`].
macro_rules! workflow_columns {
    () => {
        "w.id, w.namespace, w.name, w.description, w.labels, w.source_workflow, \
         w.source_version, w.created_by, w.created_at, w.promoted_at, w.latest"
    };
}

/// The columns of a [`StoredVersion`] of `workflow_versions v`, read by [`version_from_row`].
macro_rules! version_columns {
    () => {
        "v.workflow, v.version, v.digest, v.document, v.labels, v.created_at"
    };
}

/// How many columns [`workflow_columns!`] reads.
const WORKFLOW_COLUMN_COUNT: usize = 11;

/// The `namespace` of the organization's workflows and codes.
const ORGANIZATION: &str = "*";

/// A scope as the `namespace` columns write it.
fn scope_value(scope: &WorkflowScope) -> Value {
    text(scope.to_string())
}

fn scope_of(namespace: String) -> Result<WorkflowScope, Error> {
    if namespace == ORGANIZATION {
        return Ok(WorkflowScope::Organization);
    }
    name_of(namespace).map(WorkflowScope::Namespace)
}

fn workflow_id(id: &str) -> Result<WorkflowId, Error> {
    id.parse()
        .map_err(|err: rewind_core::InvalidWorkflowId| Error::Corrupt(err.to_string()))
}

fn labels_value(labels: &Labels) -> Value {
    text(encode_labels(labels))
}

fn workflow_from_row(row: &Row) -> Result<StoredWorkflow, Error> {
    let source = match row.opt_text(5)? {
        None => None,
        Some(workflow) => Some(WorkflowSource {
            workflow: workflow_id(&workflow)?,
            version: row.u32(6)?,
        }),
    };
    Ok(StoredWorkflow {
        id: workflow_id(&row.text(0)?)?,
        scope: scope_of(row.text(1)?)?,
        name: row.opt_text(2)?,
        description: row.opt_text(3)?,
        labels: decode_labels(&row.text(4)?)?,
        source,
        created_by: row.text(7)?,
        created_at: row.ts(8)?,
        promoted_at: row.opt_ts(9)?,
        latest: row.u32(10)?,
    })
}

/// A version read from its columns, from `first` on.
fn version_from_row(row: &Row, first: usize) -> Result<StoredVersion, Error> {
    let digest = row.digest(first + 2)?;
    Ok(StoredVersion {
        workflow: workflow_id(&row.text(first)?)?,
        version: row.u32(first + 1)?,
        document: version_document(&row.text(first + 3)?, digest)?,
        digest,
        labels: decode_labels(&row.text(first + 4)?)?,
        created_at: row.ts(first + 5)?,
    })
}

/// The version a stored document describes. A version the schema migration of the workflow
/// model moved (`schema::V14`) keeps the document it was saved with, in the names of before the
/// rename release, until `rewind server rename-data` rewrites it: until then it cannot be read.
fn version_document(json: &str, digest: Digest) -> Result<WorkflowVersion, Error> {
    rewind_format::decode_workflow_version(json)
        .map_err(|err| Error::Corrupt(format!("workflow version {digest}: {err}")))
}

async fn find_workflow(
    connection: &Connection,
    tenant_id: &TenantId,
    id: &WorkflowId,
) -> Result<Option<StoredWorkflow>, Error> {
    let row = sql::query_row(
        connection,
        concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows w WHERE w.tenant = ? AND w.id = ?"
        ),
        vec![tenant(tenant_id), text(id.as_str())],
    )
    .await?;
    row.map(|row| workflow_from_row(&row)).transpose()
}

/// The workflow of `scope` named `name`, if any.
async fn named(
    connection: &Connection,
    tenant_id: &TenantId,
    scope: &WorkflowScope,
    name: &str,
) -> Result<Option<StoredWorkflow>, Error> {
    let row = sql::query_row(
        connection,
        concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows w WHERE w.tenant = ? AND w.namespace = ? AND w.name = ?"
        ),
        vec![tenant(tenant_id), scope_value(scope), text(name)],
    )
    .await?;
    row.map(|row| workflow_from_row(&row)).transpose()
}

/// The workflow that first had `code` in `scope` under labels carrying `labels`, and its latest
/// version with that code carrying them: the first row of `workflow_codes` whose workflow has
/// such a version.
async fn with_code(
    connection: &Connection,
    tenant_id: &TenantId,
    scope: &WorkflowScope,
    code: Digest,
    labels: &Labels,
) -> Result<Option<(StoredWorkflow, StoredVersion)>, Error> {
    let row = sql::query_row(
        connection,
        concat!(
            "SELECT ",
            workflow_columns!(),
            ", ",
            version_columns!(),
            " FROM workflow_codes c \
             JOIN workflows w ON w.tenant = c.tenant AND w.id = c.workflow \
             JOIN workflow_versions v ON v.tenant = c.tenant AND v.workflow = c.workflow \
                 AND v.code = c.code AND ",
            carries!("v.labels"),
            " WHERE c.tenant = ? AND c.namespace = ? AND c.code = ? AND ",
            carries!("c.labels"),
            " ORDER BY c.added_order, v.version DESC LIMIT 1"
        ),
        vec![
            labels_value(labels),
            tenant(tenant_id),
            scope_value(scope),
            digest(code),
            labels_value(labels),
        ],
    )
    .await?;
    row.map(|row| {
        Ok((
            workflow_from_row(&row)?,
            version_from_row(&row, WORKFLOW_COLUMN_COUNT)?,
        ))
    })
    .transpose()
}

/// Adds `code` to the `workflow_codes` of `scope` under `labels` for the workflow `id`, unless
/// the scope has it there: after every row of its tenant.
async fn add_code(
    connection: &Connection,
    tenant_id: &TenantId,
    scope: &WorkflowScope,
    version: &WorkflowVersion,
    id: &WorkflowId,
    now: Timestamp,
) -> Result<(), Error> {
    sql::execute(
        connection,
        "INSERT OR IGNORE INTO workflow_codes (tenant, namespace, labels, code, workflow, \
         added_at, added_order) \
         SELECT ?, ?, ?, ?, ?, ?, COALESCE(MAX(added_order), 0) + 1 FROM workflow_codes \
         WHERE tenant = ?",
        vec![
            tenant(tenant_id),
            scope_value(scope),
            labels_value(version.labels()),
            digest(version.code()),
            text(id.as_str()),
            ts(now),
            tenant(tenant_id),
        ],
    )
    .await?;
    Ok(())
}

/// Moves the codes of the workflow `id` from its scope `from` to `to`, each keeping its place in
/// the order; a code `to` has already under the same labels keeps its workflow.
async fn move_codes(
    connection: &Connection,
    tenant_id: &TenantId,
    id: &WorkflowId,
    from: &WorkflowScope,
    to: &WorkflowScope,
) -> Result<(), Error> {
    sql::execute(
        connection,
        "DELETE FROM workflow_codes WHERE tenant = ? AND namespace = ? AND workflow = ? \
         AND EXISTS (SELECT 1 FROM workflow_codes there \
                     WHERE there.tenant = workflow_codes.tenant AND there.namespace = ? \
                     AND there.labels = workflow_codes.labels \
                     AND there.code = workflow_codes.code)",
        vec![
            tenant(tenant_id),
            scope_value(from),
            text(id.as_str()),
            scope_value(to),
        ],
    )
    .await?;
    sql::execute(
        connection,
        "UPDATE workflow_codes SET namespace = ? WHERE tenant = ? AND namespace = ? AND workflow = ?",
        vec![
            scope_value(to),
            tenant(tenant_id),
            scope_value(from),
            text(id.as_str()),
        ],
    )
    .await?;
    Ok(())
}

/// A test without a row, or whose deletion has started, among `tests`.
async fn find_missing_test(
    connection: &Connection,
    tenant_id: &TenantId,
    tests: impl IntoIterator<Item = Digest>,
) -> Result<Option<Digest>, Error> {
    for test in tests.into_iter().collect::<BTreeSet<_>>() {
        let live = sql::query_row(
            connection,
            "SELECT 1 FROM tests WHERE tenant = ? AND digest = ? AND deletion_started_at IS NULL",
            vec![tenant(tenant_id), digest(test)],
        )
        .await?;
        if live.is_none() {
            return Ok(Some(test));
        }
    }
    Ok(None)
}

/// Makes `test` the test `name` of a suite: a member that left under that name and digest joins
/// again at `now`; one already in it stays.
async fn join(
    connection: &Connection,
    tenant_id: &TenantId,
    id: &WorkflowId,
    name: &str,
    test: Digest,
    now: Timestamp,
) -> Result<(), Error> {
    sql::execute(
        connection,
        "INSERT INTO workflow_tests (tenant, workflow, name, digest, added_at, retired_at, \
         reason) VALUES (?, ?, ?, ?, ?, NULL, NULL) \
         ON CONFLICT (tenant, workflow, name, digest) DO UPDATE SET \
         added_at = excluded.added_at, retired_at = NULL, reason = NULL \
         WHERE workflow_tests.retired_at IS NOT NULL",
        vec![
            tenant(tenant_id),
            text(id.as_str()),
            text(name),
            digest(test),
            ts(now),
        ],
    )
    .await?;
    Ok(())
}

/// Makes the current member `name` of digest `test` leave its suite at `now`, for `reason`.
async fn retire(
    connection: &Connection,
    tenant_id: &TenantId,
    id: &WorkflowId,
    (name, test): (&str, Digest),
    reason: Option<&str>,
    now: Timestamp,
) -> Result<(), Error> {
    sql::execute(
        connection,
        "UPDATE workflow_tests SET retired_at = ?, reason = ? \
         WHERE tenant = ? AND workflow = ? AND name = ? AND digest = ? AND retired_at IS NULL",
        vec![
            ts(now),
            opt_text(reason),
            tenant(tenant_id),
            text(id.as_str()),
            text(name),
            digest(test),
        ],
    )
    .await?;
    Ok(())
}

/// Every test a workflow's suite holds or held, by name, then when it joined.
async fn suite_of(
    connection: &Connection,
    tenant_id: &TenantId,
    id: &WorkflowId,
) -> Result<Vec<SuiteMember>, Error> {
    let rows = sql::query(
        connection,
        "SELECT name, digest, added_at, retired_at, reason FROM workflow_tests \
         WHERE tenant = ? AND workflow = ? ORDER BY name, added_at, digest",
        vec![tenant(tenant_id), text(id.as_str())],
    )
    .await?;
    rows.iter()
        .map(|row| {
            Ok(SuiteMember {
                name: row.text(0)?,
                digest: row.digest(1)?,
                added_at: row.ts(2)?,
                retired_at: row.opt_ts(3)?,
                reason: row.opt_text(4)?,
            })
        })
        .collect()
}

/// Stores the version `number` of the workflow `id`, its code in its scope's `workflow_codes`.
async fn insert_version(
    connection: &Connection,
    tenant_id: &TenantId,
    workflow: (&WorkflowId, &WorkflowScope),
    number: u32,
    document: &WorkflowVersion,
    now: Timestamp,
) -> Result<StoredVersion, Error> {
    let (id, scope) = workflow;
    sql::execute(
        connection,
        "INSERT INTO workflow_versions (tenant, workflow, version, digest, code, document, \
         labels, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            tenant(tenant_id),
            text(id.as_str()),
            int(u64::from(number)),
            digest(document.digest()),
            digest(document.code()),
            text(rewind_format::encode_workflow_version(document)),
            labels_value(document.labels()),
            ts(now),
        ],
    )
    .await?;
    add_code(connection, tenant_id, scope, document, id, now).await?;
    Ok(StoredVersion {
        workflow: id.clone(),
        version: number,
        document: document.clone(),
        digest: document.digest(),
        labels: document.labels().clone(),
        created_at: now,
    })
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

    /// Moves a workflow into `to` with its codes, at `promoted_at` for a promotion: `Written` as
    /// it is when it is there already, `NameTaken` when `to` has a workflow of its name.
    async fn move_workflow(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        to: &WorkflowScope,
        promoted_at: Option<Timestamp>,
    ) -> Result<WorkflowWrite, CatalogError> {
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let (written, moved) = transaction(connection, async {
            let Some(mut workflow) = find_workflow(connection, tenant_id, id).await? else {
                return Ok::<_, Error>((WorkflowWrite::NotFound, false));
            };
            if workflow.scope == *to {
                return Ok((WorkflowWrite::Written(workflow), false));
            }
            if let Some(name) = &workflow.name
                && named(connection, tenant_id, to, name).await?.is_some()
            {
                return Ok((WorkflowWrite::NameTaken, false));
            }
            move_codes(connection, tenant_id, id, &workflow.scope, to).await?;
            workflow.scope = to.clone();
            workflow.promoted_at = promoted_at.or(workflow.promoted_at);
            sql::execute(
                connection,
                "UPDATE workflows SET namespace = ?, promoted_at = ? WHERE tenant = ? AND id = ?",
                vec![
                    scope_value(to),
                    opt_ts(workflow.promoted_at),
                    tenant(tenant_id),
                    text(id.as_str()),
                ],
            )
            .await?;
            Ok((WorkflowWrite::Written(workflow), true))
        })
        .await?;
        if moved {
            self.send([workflow_changed(tenant_id, id)]);
        }
        Ok(written)
    }
}

impl CatalogStore for TursoCatalog {
    // ---------------------------------------------------------------------------------------
    // Code and action sets

    async fn put_code(
        &self,
        tenant_id: &TenantId,
        code: &Code,
        compiled: &CompiledCode,
    ) -> Result<(), CatalogError> {
        let connection = self.store.connection().await?;
        sql::execute(
            &connection,
            "INSERT OR IGNORE INTO code (tenant, digest, source, transpiled, source_map) \
             VALUES (?, ?, ?, ?, ?)",
            vec![
                tenant(tenant_id),
                digest(code.digest()),
                text(code.source()),
                text(compiled.javascript.as_str()),
                text(compiled.source_map.as_str()),
            ],
        )
        .await?;
        Ok(())
    }

    async fn get_code(
        &self,
        tenant_id: &TenantId,
        key: Digest,
    ) -> Result<Option<StoredCode>, CatalogError> {
        let connection = self.store.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT source, transpiled, source_map FROM code WHERE tenant = ? AND digest = ?",
            vec![tenant(tenant_id), digest(key)],
        )
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let code = Code::new(row.text(0)?);
        if code.digest() != key {
            return Err(Error::Corrupt(format!("code {key} does not hash to its key")).into());
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
        let connection = self.store.connection().await?;
        let inserted = sql::execute(
            &connection,
            "INSERT OR IGNORE INTO action_sets (tenant, digest, spec, declarations, prompt) \
             VALUES (?, ?, ?, ?, ?)",
            vec![
                tenant(tenant_id),
                digest(action_set.digest()),
                text(rewind_format::encode_action_set(action_set)),
                text(declarations.typescript.as_str()),
                text(declarations.prompt.as_str()),
            ],
        )
        .await?;
        if inserted > 0 {
            self.send([ControlPlaneChange::ActionSet {
                tenant: tenant_id.clone(),
                digest: action_set.digest(),
            }]);
        }
        Ok(())
    }

    async fn get_action_set(
        &self,
        tenant_id: &TenantId,
        key: Digest,
    ) -> Result<Option<StoredActionSet>, CatalogError> {
        let connection = self.store.connection().await?;
        let row = sql::query_row(
            &connection,
            "SELECT spec, declarations, prompt FROM action_sets WHERE tenant = ? AND digest = ?",
            vec![tenant(tenant_id), digest(key)],
        )
        .await?;
        Ok(row
            .map(|row| action_set_from_row(&row, key, 0))
            .transpose()?)
    }

    async fn list_action_sets(
        &self,
        tenant_id: &TenantId,
    ) -> Result<Vec<StoredActionSet>, CatalogError> {
        let connection = self.store.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT digest, spec, declarations, prompt FROM action_sets WHERE tenant = ? \
             ORDER BY digest",
            vec![tenant(tenant_id)],
        )
        .await?;
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
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let version = &workflow.version;
        let created = transaction(connection, async {
            match &workflow.name {
                Some(name) => {
                    if named(connection, tenant_id, &workflow.scope, name)
                        .await?
                        .is_some()
                    {
                        return Ok(WorkflowCreated::NameTaken);
                    }
                }
                None => {
                    if let Some((existing, _)) = with_code(
                        connection,
                        tenant_id,
                        &workflow.scope,
                        version.code(),
                        version.labels(),
                    )
                    .await?
                    {
                        return Ok(WorkflowCreated::Existing(existing));
                    }
                }
            }
            let tests = version.suite().tests.iter().map(|test| test.test);
            if let Some(test) = find_missing_test(connection, tenant_id, tests).await? {
                return Err(missing_test(test));
            }
            if find_workflow(connection, tenant_id, &workflow.id)
                .await?
                .is_some()
            {
                return Err(CatalogError::Conflict(format!(
                    "the workflow {} exists already",
                    workflow.id
                )));
            }
            let source = workflow.source.as_ref();
            sql::execute(
                connection,
                "INSERT INTO workflows (tenant, id, namespace, name, description, labels, \
                 source_workflow, source_version, created_by, created_at, promoted_at, latest) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 1)",
                vec![
                    tenant(tenant_id),
                    text(workflow.id.as_str()),
                    scope_value(&workflow.scope),
                    opt_text(workflow.name.as_deref()),
                    opt_text(workflow.description.as_deref()),
                    labels_value(&workflow.labels),
                    opt_text(source.map(|source| source.workflow.as_str())),
                    source.map_or(Value::Null, |source| int(u64::from(source.version))),
                    text(workflow.created_by.as_str()),
                    ts(now),
                ],
            )
            .await?;
            insert_version(
                connection,
                tenant_id,
                (&workflow.id, &workflow.scope),
                1,
                version,
                now,
            )
            .await?;
            for test in &version.suite().tests {
                join(
                    connection,
                    tenant_id,
                    &workflow.id,
                    &test.name,
                    test.test,
                    now,
                )
                .await?;
            }
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
        })
        .await?;
        if let WorkflowCreated::Created(stored) = &created {
            self.send([workflow_changed(tenant_id, &stored.id)]);
        }
        Ok(created)
    }

    async fn workflow(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        let connection = self.store.connection().await?;
        Ok(find_workflow(&connection, tenant_id, id).await?)
    }

    async fn workflow_named(
        &self,
        tenant_id: &TenantId,
        namespace: Option<&NamespaceName>,
        name: &str,
        labels: &Labels,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        let connection = self.store.connection().await?;
        let scopes = namespace
            .map(|namespace| WorkflowScope::Namespace(namespace.clone()))
            .into_iter()
            .chain([WorkflowScope::Organization]);
        for scope in scopes {
            let row = sql::query_row(
                &connection,
                concat!(
                    "SELECT ",
                    workflow_columns!(),
                    " FROM workflows w WHERE w.tenant = ? AND w.namespace = ? AND w.name = ? \
                     AND EXISTS (SELECT 1 FROM workflow_versions v \
                                 WHERE v.tenant = w.tenant AND v.workflow = w.id AND ",
                    carries!("v.labels"),
                    ")"
                ),
                vec![
                    tenant(tenant_id),
                    scope_value(&scope),
                    text(name),
                    labels_value(labels),
                ],
            )
            .await?;
            if let Some(row) = row {
                return Ok(Some(workflow_from_row(&row)?));
            }
        }
        Ok(None)
    }

    async fn workflow_with_code(
        &self,
        tenant_id: &TenantId,
        namespace: &NamespaceName,
        code: Digest,
        labels: &Labels,
    ) -> Result<Option<(StoredWorkflow, StoredVersion)>, CatalogError> {
        let connection = self.store.connection().await?;
        let scopes = [
            WorkflowScope::Namespace(namespace.clone()),
            WorkflowScope::Organization,
        ];
        for scope in &scopes {
            if let Some(found) = with_code(&connection, tenant_id, scope, code, labels).await? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    async fn list_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        if filter.limit == 0 {
            return Ok(Vec::new());
        }
        let connection = self.store.connection().await?;
        let mut statement = concat!(
            "SELECT ",
            workflow_columns!(),
            " FROM workflows w WHERE w.tenant = ?"
        )
        .to_owned();
        let mut params = vec![tenant(&filter.tenant)];
        if let Some(after) = &filter.after {
            statement.push_str(" AND w.id > ?");
            params.push(text(after.as_str()));
        }
        match &filter.scope {
            ScopeFilter::All => {}
            ScopeFilter::Visible(namespace) => {
                statement.push_str(" AND w.namespace IN (?, ?)");
                params.extend([text(ORGANIZATION), text(namespace.as_str())]);
            }
            ScopeFilter::Organization => {
                statement.push_str(" AND w.namespace = ?");
                params.push(text(ORGANIZATION));
            }
            ScopeFilter::Namespace(namespace) => {
                statement.push_str(" AND w.namespace = ?");
                params.push(text(namespace.as_str()));
            }
        }
        statement.push_str(
            " AND EXISTS (SELECT 1 FROM workflow_versions v \
             WHERE v.tenant = w.tenant AND v.workflow = w.id",
        );
        if let Some(code) = filter.code {
            statement.push_str(" AND v.code = ?");
            params.push(digest(code));
        }
        statement.push_str(concat!(
            " AND ",
            carries!("v.labels"),
            ") ORDER BY w.id LIMIT ?"
        ));
        params.extend([labels_value(&filter.labels), limit(filter.limit)]);
        let rows = sql::query_dynamic(&connection, &statement, params).await?;
        Ok(rows
            .iter()
            .map(workflow_from_row)
            .collect::<Result<_, Error>>()?)
    }

    async fn update_workflow(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        patch: &WorkflowPatch,
        _now: Timestamp,
    ) -> Result<WorkflowWrite, CatalogError> {
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let written = transaction(connection, async {
            let Some(mut workflow) = find_workflow(connection, tenant_id, id).await? else {
                return Ok::<_, Error>(WorkflowWrite::NotFound);
            };
            if let Some(Some(name)) = &patch.name
                && named(connection, tenant_id, &workflow.scope, name)
                    .await?
                    .is_some_and(|other| other.id != *id)
            {
                return Ok(WorkflowWrite::NameTaken);
            }
            if let Some(name) = &patch.name {
                workflow.name.clone_from(name);
            }
            if let Some(description) = &patch.description {
                workflow.description.clone_from(description);
            }
            sql::execute(
                connection,
                "UPDATE workflows SET name = ?, description = ? WHERE tenant = ? AND id = ?",
                vec![
                    opt_text(workflow.name.as_deref()),
                    opt_text(workflow.description.as_deref()),
                    tenant(tenant_id),
                    text(id.as_str()),
                ],
            )
            .await?;
            Ok(WorkflowWrite::Written(workflow))
        })
        .await?;
        if let WorkflowWrite::Written(_) = &written {
            self.send([workflow_changed(tenant_id, id)]);
        }
        Ok(written)
    }

    async fn put_version(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        version: &NewVersion,
        now: Timestamp,
    ) -> Result<StoredVersion, CatalogError> {
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let document = &version.document;
        let suite = document.suite();
        let stored = transaction(connection, async {
            let Some(workflow) = find_workflow(connection, tenant_id, id).await? else {
                return Err(no_workflow(id));
            };
            let tests = suite.tests.iter().map(|test| test.test);
            if let Some(test) = find_missing_test(connection, tenant_id, tests).await? {
                return Err(missing_test(test));
            }
            let number = workflow.latest + 1;
            let stored = insert_version(
                connection,
                tenant_id,
                (id, &workflow.scope),
                number,
                document,
                now,
            )
            .await?;
            // The suite becomes the version's: the tests it no longer has leave with the reason
            // it gives their name.
            let reason = |name: &str| {
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
            };
            for member in suite_of(connection, tenant_id, id).await? {
                if member.is_current() && suite.test(&member.name) != Some(member.digest) {
                    let leaving = (member.name.as_str(), member.digest);
                    retire(
                        connection,
                        tenant_id,
                        id,
                        leaving,
                        reason(&member.name),
                        now,
                    )
                    .await?;
                }
            }
            for test in &suite.tests {
                join(connection, tenant_id, id, &test.name, test.test, now).await?;
            }
            let source_version = workflow
                .source
                .map(|source| version.pulled.unwrap_or(source.version));
            sql::execute(
                connection,
                "UPDATE workflows SET latest = ?, source_version = ? WHERE tenant = ? AND id = ?",
                vec![
                    int(u64::from(number)),
                    source_version.map_or(Value::Null, |pulled| int(u64::from(pulled))),
                    tenant(tenant_id),
                    text(id.as_str()),
                ],
            )
            .await?;
            Ok::<_, CatalogError>(stored)
        })
        .await?;
        self.send([workflow_changed(tenant_id, id)]);
        Ok(stored)
    }

    async fn versions(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        labels: &Labels,
    ) -> Result<Vec<StoredVersion>, CatalogError> {
        let connection = self.store.connection().await?;
        let rows = sql::query(
            &connection,
            concat!(
                "SELECT ",
                version_columns!(),
                " FROM workflow_versions v WHERE v.tenant = ? AND v.workflow = ? AND ",
                carries!("v.labels"),
                " ORDER BY v.version DESC"
            ),
            vec![tenant(tenant_id), text(id.as_str()), labels_value(labels)],
        )
        .await?;
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
        let connection = self.store.connection().await?;
        let row = match version {
            VersionRef::Number(number) => {
                sql::query_row(
                    &connection,
                    concat!(
                        "SELECT ",
                        version_columns!(),
                        " FROM workflow_versions v \
                         WHERE v.tenant = ? AND v.workflow = ? AND v.version = ?"
                    ),
                    vec![tenant(tenant_id), text(id.as_str()), int(u64::from(number))],
                )
                .await?
            }
            VersionRef::Digest(key) => {
                sql::query_row(
                    &connection,
                    concat!(
                        "SELECT ",
                        version_columns!(),
                        " FROM workflow_versions v \
                         WHERE v.tenant = ? AND v.workflow = ? AND v.digest = ? \
                         ORDER BY v.version DESC LIMIT 1"
                    ),
                    vec![tenant(tenant_id), text(id.as_str()), digest(key)],
                )
                .await?
            }
        };
        Ok(row.map(|row| version_from_row(&row, 0)).transpose()?)
    }

    async fn change_suite(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        change: &SuiteChange,
        now: Timestamp,
    ) -> Result<Vec<SuiteMember>, CatalogError> {
        let connection = self.store.connection().await?;
        let connection = &*connection;
        let suite = transaction(connection, async {
            if find_workflow(connection, tenant_id, id).await?.is_none() {
                return Err(no_workflow(id));
            }
            let joining = change
                .added
                .iter()
                .map(|test| test.test)
                .chain(change.replaced.iter().map(|replaced| replaced.new));
            if let Some(test) = find_missing_test(connection, tenant_id, joining).await? {
                return Err(missing_test(test));
            }
            let leaving: Vec<(&str, Digest, &str)> = change
                .retired
                .iter()
                .map(|retired| (retired.name.as_str(), retired.test, retired.reason.as_str()))
                .chain(change.replaced.iter().map(|replaced| {
                    (
                        replaced.name.as_str(),
                        replaced.old,
                        replaced.reason.as_str(),
                    )
                }))
                .collect();
            let members = suite_of(connection, tenant_id, id).await?;
            for (name, test, _) in &leaving {
                if !members.iter().any(|member| {
                    member.is_current() && member.name == *name && member.digest == *test
                }) {
                    return Err(CatalogError::Conflict(format!(
                        "the suite of {id} has no test {name:?} of digest {test}"
                    )));
                }
            }
            for (name, test, reason) in leaving {
                retire(connection, tenant_id, id, (name, test), Some(reason), now).await?;
            }
            for replaced in &change.replaced {
                join(connection, tenant_id, id, &replaced.name, replaced.new, now).await?;
            }
            for test in &change.added {
                join(connection, tenant_id, id, &test.name, test.test, now).await?;
            }
            Ok(suite_of(connection, tenant_id, id).await?)
        })
        .await?;
        self.send([workflow_changed(tenant_id, id)]);
        Ok(suite)
    }

    async fn suite(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
    ) -> Result<Vec<SuiteMember>, CatalogError> {
        let connection = self.store.connection().await?;
        Ok(suite_of(&connection, tenant_id, id).await?)
    }

    async fn promote_workflow(
        &self,
        tenant_id: &TenantId,
        id: &WorkflowId,
        now: Timestamp,
    ) -> Result<WorkflowWrite, CatalogError> {
        self.move_workflow(tenant_id, id, &WorkflowScope::Organization, Some(now))
            .await
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
            "INSERT OR REPLACE INTO subscriptions (tenant, id, url, secret, types, labels) \
             VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                tenant(&subscription.tenant),
                text(subscription.id.as_str()),
                text(subscription.url.as_str()),
                text(subscription.secret.expose()),
                text(encode_event_types(&subscription.types)),
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
            "SELECT id, url, secret, types, labels FROM subscriptions WHERE tenant = ? \
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
        let connection = &*connection;
        let dropped = transaction(connection, async {
            let dropped = sql::execute(
                connection,
                "DELETE FROM namespaces WHERE tenant = ? AND name = ? \
                 AND deleting_since IS NOT NULL",
                vec![tenant(tenant_id), text(name.as_str())],
            )
            .await?;
            if dropped == 0 {
                return Ok::<_, Error>(None);
            }
            // Its workflows go with it, with their versions, suites and codes; the
            // organization's stay.
            let scope = || vec![tenant(tenant_id), text(name.as_str())];
            let workflows = sql::query(
                connection,
                "SELECT id FROM workflows WHERE tenant = ? AND namespace = ? ORDER BY id",
                scope(),
            )
            .await?
            .iter()
            .map(|row| workflow_id(&row.text(0)?))
            .collect::<Result<Vec<_>, Error>>()?;
            for statement in [
                "DELETE FROM workflow_tests WHERE tenant = ? AND workflow IN \
                 (SELECT id FROM workflows WHERE tenant = ? AND namespace = ?)",
                "DELETE FROM workflow_versions WHERE tenant = ? AND workflow IN \
                 (SELECT id FROM workflows WHERE tenant = ? AND namespace = ?)",
            ] {
                let params = vec![tenant(tenant_id), tenant(tenant_id), text(name.as_str())];
                sql::execute(connection, statement, params).await?;
            }
            for statement in [
                "DELETE FROM workflow_codes WHERE tenant = ? AND namespace = ?",
                "DELETE FROM workflows WHERE tenant = ? AND namespace = ?",
            ] {
                sql::execute(connection, statement, scope()).await?;
            }
            Ok(Some(workflows))
        })
        .await?;
        let Some(workflows) = dropped else {
            return Ok(DropNamespace::NotFound);
        };
        self.send(
            std::iter::once(namespace_changed(tenant_id, name))
                .chain(workflows.iter().map(|id| workflow_changed(tenant_id, id))),
        );
        Ok(DropNamespace::Dropped)
    }
}

impl CatalogMigration for TursoCatalog {
    async fn code_titles(&self) -> Result<Vec<CodeTitle>, CatalogError> {
        let connection = self.store.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT tenant, digest, title, description FROM code_titles ORDER BY tenant, digest",
            Vec::new(),
        )
        .await?;
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
        self.move_workflow(tenant_id, id, &scope, None).await
    }

    async fn drop_code_titles(&self) -> Result<(), CatalogError> {
        let connection = self.store.connection().await?;
        sql::execute(&connection, "DELETE FROM code_titles", Vec::new()).await?;
        Ok(())
    }
}

impl CatalogDocuments for TursoCatalog {
    async fn raw_action_sets(&self) -> Result<Vec<RawDocument>, CatalogError> {
        let connection = self.store.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT tenant, digest, spec FROM action_sets ORDER BY tenant, digest",
            Vec::new(),
        )
        .await?;
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
        let connection = self.store.connection().await?;
        let connection = &connection;
        transaction(connection, async {
            sql::execute(
                connection,
                "INSERT OR REPLACE INTO action_sets (tenant, digest, spec, declarations, prompt) \
                 VALUES (?, ?, ?, ?, ?)",
                vec![
                    tenant(tenant_id),
                    digest(action_set.digest()),
                    text(rewind_format::encode_action_set(action_set)),
                    text(declarations.typescript.as_str()),
                    text(declarations.prompt.as_str()),
                ],
            )
            .await?;
            if old != action_set.digest() {
                sql::execute(
                    connection,
                    "DELETE FROM action_sets WHERE tenant = ? AND digest = ?",
                    vec![tenant(tenant_id), digest(old)],
                )
                .await?;
            }
            Ok::<(), Error>(())
        })
        .await?;
        Ok(())
    }

    async fn raw_versions(&self) -> Result<Vec<RawVersion>, CatalogError> {
        let connection = self.store.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT tenant, workflow, version, digest, document FROM workflow_versions \
             ORDER BY tenant, workflow, version",
            Vec::new(),
        )
        .await?;
        Ok(rows
            .iter()
            .map(|row| {
                Ok(RawVersion {
                    tenant: row.tenant(0)?,
                    workflow: workflow_id(&row.text(1)?)?,
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
        let connection = self.store.connection().await?;
        sql::execute(
            &connection,
            "UPDATE workflow_versions SET digest = ?, document = ? \
             WHERE tenant = ? AND workflow = ? AND version = ?",
            vec![
                digest(document.digest()),
                text(rewind_format::encode_workflow_version(document)),
                tenant(tenant_id),
                text(workflow.as_str()),
                int(version),
            ],
        )
        .await?;
        Ok(())
    }

    async fn raw_tests(&self) -> Result<Vec<(TenantId, Digest)>, CatalogError> {
        let connection = self.store.connection().await?;
        let rows = sql::query(
            &connection,
            "SELECT tenant, digest FROM tests ORDER BY tenant, digest",
            Vec::new(),
        )
        .await?;
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
        let connection = self.store.connection().await?;
        let connection = &connection;
        transaction(connection, async {
            sql::execute(
                connection,
                "INSERT OR IGNORE INTO tests \
                 (tenant, digest, namespace, size, reserved_at, deletion_started_at) \
                 SELECT tenant, ?, namespace, ?, reserved_at, deletion_started_at FROM tests \
                 WHERE tenant = ? AND digest = ?",
                vec![digest(new), int(size), tenant(tenant_id), digest(old)],
            )
            .await?;
            sql::execute(
                connection,
                "UPDATE workflow_tests SET digest = ? WHERE tenant = ? AND digest = ?",
                vec![digest(new), tenant(tenant_id), digest(old)],
            )
            .await?;
            sql::execute(
                connection,
                "DELETE FROM tests WHERE tenant = ? AND digest = ?",
                vec![tenant(tenant_id), digest(old)],
            )
            .await?;
            Ok::<(), Error>(())
        })
        .await?;
        Ok(())
    }
}
