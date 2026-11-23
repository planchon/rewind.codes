//! The `CatalogStore` port: the control plane's catalog (`archi/07-code-architecture.md`,
//! "`CatalogStore`"; `archi/06-storage-and-scale.md`, "The control plane").
//!
//! Scripts, toolsets and workflow versions by digest, the index of tests and the suite of each
//! version; tenants; namespaces, with their partition, display name, exceptions and erasure;
//! subscriptions. It is written at the pace of people, and every write sends its
//! [`ControlPlaneChange`](crate::ports::cluster::ControlPlaneChange) in the same transaction, which
//! every process receives through [`Membership::changes`](crate::ports::cluster::Membership).

use std::collections::BTreeMap;
use std::future::Future;
use std::num::NonZeroU16;
use std::time::Duration;

use rewind_core::{Digest, PartitionId, RunId, Script, Timestamp, Toolset, Workflow};

use crate::ports::checker::ToolDeclarations;
use crate::ports::executor::CompiledScript;
use crate::ports::journal::{
    StoredScript, StoredToolset, StoredWorkflow, TestRecord, WorkflowFilter,
};
use crate::ports::namespaces::{
    DeleteNamespace, DisplayName, DropNamespace, Namespace, NamespaceName, NamespaceWrite, Quotas,
};
use crate::ports::tenancy::{Tenant, TenantId};
use crate::ports::webhooks::Subscription;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    /// A write that would break an invariant of the catalog, such as a version that references
    /// a test without a row.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The control plane cannot be reached; the call may be tried again.
    #[error("the catalog is unavailable: {0}")]
    Unavailable(String),
    #[error("catalog backend error: {0}")]
    Backend(String),
}

/// What a use case reports of a catalog that failed: a conflict as a conflict, the rest as the
/// backend's error.
impl From<CatalogError> for crate::ports::StoreError {
    fn from(error: CatalogError) -> Self {
        match error {
            CatalogError::Conflict(message) => crate::ports::StoreError::Conflict(message),
            other => crate::ports::StoreError::Backend(other.to_string()),
        }
    }
}

/// How long the deletion of tests may take: a test whose deletion started longer ago is marked
/// again by [`CatalogStore::unreferenced_tests`], since the cleanup that started it stopped.
pub const TEST_DELETION_TIMEOUT: Duration = Duration::from_secs(3_600);

/// Namespaces to list, in one tenant, by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceFilter {
    pub tenant: TenantId,
    /// This namespace alone.
    pub name: Option<NamespaceName>,
    /// Names starting with this, or display names starting with it case aside
    /// ([`DisplayName::starts_with`]).
    pub prefix: Option<String>,
    /// Namespaces whose name comes after this one: the last of the previous page.
    pub after: Option<NamespaceName>,
    pub limit: usize,
}

/// A page of [`CatalogStore::list_namespaces`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NamespacePage {
    pub namespaces: Vec<Namespace>,
    /// How many namespaces match the filter across every page: `after` and `limit` aside.
    pub total: u64,
}

/// The control plane's catalog.
///
/// The contract, checked by the conformance suite (feature `conformance`, module
/// `conformance::catalog`):
///
/// - **Tenants.** Every key starts with the tenant: a script, a toolset, a version, a test, a
///   namespace or a subscription of one tenant is never visible to another.
/// - **Content.** Scripts and toolsets are stored by digest, and a second put changes nothing.
///   Only scripts that compile are stored.
/// - **Workflows.** A version is immutable: a second `put_workflow` of a version changes nothing
///   and returns the first. `workflow_versions` lists a name's versions carrying `labels`,
///   newest first (created last first, then stored last first): the first is the latest;
///   `workflows_with_script` lists the versions of any name whose script is `script`, in the
///   same order; `latest_workflows` gives the latest version of each name, by name.
/// - **Tests.** `reserve_tests` inserts a test's row, or makes an existing one new again, before
///   its blob is written, and returns the digests whose deletion has started. `put_workflow`
///   writes the rows of `workflow_tests` from the version's suite, and refuses (`Conflict`) a
///   new version that references a test without a row, or whose deletion has started.
///   `unreferenced_tests` marks the rows no version references, stored more than
///   [`TEST_GRACE`](crate::ports::TEST_GRACE) ago, and returns them; their blobs are deleted,
///   then `drop_tests` removes the rows still marked. With `erased`, it removes the given rows
///   whatever references them.
/// - **Namespaces.** `ensure_namespace` and `put_namespace` create a namespace at its first use
///   with the tenant's defaults and its partition, [`namespace_partition`]; the partition of a
///   namespace never changes. A namespace being deleted is left as it is (`Deleting`).
///   `delete_namespace` marks it; `namespaces_to_erase` lists the marked ones, the oldest mark
///   first; `drop_namespace` removes a marked one, once its partition holds nothing of it.
/// - **Changes.** Every write sends what it changed as a `ControlPlaneChange`, in the same
///   transaction: a script, a toolset, a workflow version, a test, a subscription, a tenant, a
///   namespace.
/// - **Time.** The catalog never reads a clock: every method that depends on time takes `now`.
///
/// [`namespace_partition`]: crate::ports::partition::namespace_partition
pub trait CatalogStore: Send + Sync {
    // ---------------------------------------------------------------------------------------
    // Scripts and toolsets

    /// Stores a script and its compiled form. Idempotent by digest: a second put changes
    /// nothing.
    fn put_script(
        &self,
        tenant: &TenantId,
        script: &Script,
        compiled: &CompiledScript,
    ) -> impl Future<Output = Result<(), CatalogError>> + Send;

    fn get_script(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<StoredScript>, CatalogError>> + Send;

    /// Stores a pinned toolset and its declarations. Idempotent by digest: a second put changes
    /// nothing.
    fn put_toolset(
        &self,
        tenant: &TenantId,
        toolset: &Toolset,
        declarations: &ToolDeclarations,
    ) -> impl Future<Output = Result<(), CatalogError>> + Send;

    fn get_toolset(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<StoredToolset>, CatalogError>> + Send;

    /// The tenant's toolsets, by digest.
    fn list_toolsets(
        &self,
        tenant: &TenantId,
    ) -> impl Future<Output = Result<Vec<StoredToolset>, CatalogError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Workflows

    /// Stores a workflow version, visible to the principals that see its labels
    /// ([`Workflow::labels`]), and returns it as stored, with a row of `workflow_tests` for each
    /// test of its suite, in one transaction. Idempotent by version.
    fn put_workflow(
        &self,
        tenant: &TenantId,
        workflow: &Workflow,
        now: Timestamp,
    ) -> impl Future<Output = Result<StoredWorkflow, CatalogError>> + Send;

    fn get_workflow(
        &self,
        tenant: &TenantId,
        version: Digest,
    ) -> impl Future<Output = Result<Option<StoredWorkflow>, CatalogError>> + Send;

    /// The versions of the workflow `name` that carry all of `labels`, newest first.
    fn workflow_versions(
        &self,
        tenant: &TenantId,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> impl Future<Output = Result<Vec<StoredWorkflow>, CatalogError>> + Send;

    /// The versions of any workflow whose script is `script` and that carry all of `labels`,
    /// newest first.
    fn workflows_with_script(
        &self,
        tenant: &TenantId,
        script: Digest,
        labels: &BTreeMap<String, String>,
    ) -> impl Future<Output = Result<Vec<StoredWorkflow>, CatalogError>> + Send;

    /// The latest version of each workflow of the tenant, among the versions that carry all of
    /// the filter's labels, by name.
    fn latest_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> impl Future<Output = Result<Vec<StoredWorkflow>, CatalogError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Tests (`archi/05-testing-and-observability.md`, "Suites and the gate")

    /// Reserves the rows of tests before their blobs are written, in one transaction: a missing
    /// row is created as its record describes it; an existing row starts its grace again at
    /// `now`. Returns the tests whose deletion has started, left as they are.
    fn reserve_tests(
        &self,
        tenant: &TenantId,
        tests: &[TestRecord],
        now: Timestamp,
    ) -> impl Future<Output = Result<Vec<Digest>, CatalogError>> + Send;

    /// A test's row; `None` for a test never stored, erased, or whose deletion has started.
    fn get_test(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<TestRecord>, CatalogError>> + Send;

    /// Up to `limit` tests recorded from the runs of a namespace, for its erasure.
    fn namespace_tests(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<Digest>, CatalogError>> + Send;

    /// Marks as being deleted, and returns, up to `limit` tests that no version references,
    /// stored or reserved at least `TEST_GRACE` before `now`, whose deletion has not started,
    /// or started at least [`TEST_DELETION_TIMEOUT`] ago.
    fn unreferenced_tests(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<(TenantId, Digest)>, CatalogError>> + Send;

    /// Removes the rows of the given tests once their blobs are deleted: those still marked and
    /// that no version references; with `erased`, every given row, with the rows of
    /// `workflow_tests` that reference it. Returns how many rows were removed.
    fn drop_tests(
        &self,
        tests: &[(TenantId, Digest)],
        erased: bool,
    ) -> impl Future<Output = Result<usize, CatalogError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Subscriptions

    /// Stores or replaces a subscription, by tenant and id.
    fn put_subscription(
        &self,
        subscription: &Subscription,
    ) -> impl Future<Output = Result<(), CatalogError>> + Send;

    /// The tenant's subscriptions, by id.
    fn subscriptions(
        &self,
        tenant: &TenantId,
    ) -> impl Future<Output = Result<Vec<Subscription>, CatalogError>> + Send;

    /// Returns whether the subscription existed.
    fn delete_subscription(
        &self,
        tenant: &TenantId,
        id: &str,
    ) -> impl Future<Output = Result<bool, CatalogError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Tenants

    /// Stores or replaces a tenant's settings.
    fn put_tenant(&self, tenant: &Tenant) -> impl Future<Output = Result<(), CatalogError>> + Send;

    fn get_tenant(
        &self,
        id: &TenantId,
    ) -> impl Future<Output = Result<Option<Tenant>, CatalogError>> + Send;

    /// The tenants with a record, by id.
    fn list_tenants(&self) -> impl Future<Output = Result<Vec<Tenant>, CatalogError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Namespaces

    /// The namespace, created at its first use with the tenant's defaults and its partition
    /// among `partitions`. An existing namespace is returned as it is, its partition kept; a
    /// namespace being deleted is `Deleting`.
    fn ensure_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> impl Future<Output = Result<NamespaceWrite, CatalogError>> + Send;

    /// Creates the namespace if it does not exist, as `ensure_namespace` does. With
    /// `exceptions`, replaces its exceptions; without, leaves them. A namespace being erased is
    /// left as it is (`Deleting`).
    fn put_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        exceptions: Option<&Quotas>,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> impl Future<Output = Result<NamespaceWrite, CatalogError>> + Send;

    fn get_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> impl Future<Output = Result<Option<Namespace>, CatalogError>> + Send;

    /// Sets the namespace's display name, or removes it (`None`). Never creates the namespace:
    /// `None` when it does not exist. A namespace being erased is left as it is (`Deleting`).
    fn set_namespace_display_name(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        display_name: Option<&DisplayName>,
    ) -> impl Future<Output = Result<Option<NamespaceWrite>, CatalogError>> + Send;

    /// The tenant's namespaces that have a display name, with it, by name.
    fn namespace_display_names(
        &self,
        tenant: &TenantId,
    ) -> impl Future<Output = Result<Vec<(NamespaceName, DisplayName)>, CatalogError>> + Send;

    /// The tenant's namespaces matching the filter, by name, after `filter.after`. The page says
    /// how many match the filter across every page.
    fn list_namespaces(
        &self,
        filter: &NamespaceFilter,
    ) -> impl Future<Output = Result<NamespacePage, CatalogError>> + Send;

    /// Sets the namespace's `tokens_revoked_before` to `now`: its signed tokens issued earlier
    /// are refused. Returns whether the namespace exists.
    fn revoke_namespace_tokens(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> impl Future<Output = Result<bool, CatalogError>> + Send;

    /// Marks the namespace for erasure at `now`, if it is not yet: it takes no run any more.
    fn delete_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> impl Future<Output = Result<DeleteNamespace, CatalogError>> + Send;

    /// Up to `limit` namespaces marked for erasure, the oldest mark first.
    fn namespaces_to_erase(
        &self,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<(TenantId, NamespaceName)>, CatalogError>> + Send;

    /// Removes a namespace marked for erasure: `Dropped`, or `NotFound` for a namespace that
    /// is not marked. The caller drops it once its partition holds nothing of it, so the
    /// catalog never answers `RunsLeft`.
    fn drop_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> impl Future<Output = Result<DropNamespace, CatalogError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Runs `migrate-runs` moved

    /// The partition a run `migrate-runs` moved from the `legacy` schema lives in: its id is a
    /// version 7 UUID, whose bits name no partition (`archi/06-storage-and-scale.md`,
    /// "Partitions, owners and nodes": `legacy_runs`). `None` for a run `migrate-runs` did not
    /// move. A catalog that holds no such run answers `None`, which is the default.
    fn legacy_partition(
        &self,
        run: RunId,
    ) -> impl Future<Output = Result<Option<PartitionId>, CatalogError>> + Send {
        let _ = run;
        std::future::ready(Ok(None))
    }
}
