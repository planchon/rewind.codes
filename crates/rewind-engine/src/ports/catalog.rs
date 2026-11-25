//! The `CatalogStore` port: the control plane's catalog (`archi/07-code-architecture.md`,
//! "`CatalogStore`"; `archi/06-storage-and-scale.md`, "The control plane").
//!
//! Code and action sets by digest; workflows, their versions, the codes each scope has seen and
//! their suites; the index of tests; tenants; namespaces, with their partition, display name,
//! exceptions and erasure; subscriptions. It is written at the pace of people, and every write sends its
//! [`ControlPlaneChange`](crate::ports::cluster::ControlPlaneChange) in the same transaction, which
//! every process receives through [`Membership::changes`](crate::ports::cluster::Membership).

use std::future::Future;
use std::num::NonZeroU16;
use std::time::Duration;

use rewind_core::{
    ActionSet, Code, Digest, PartitionId, RunId, Timestamp, WorkflowId, WorkflowVersion,
};

use crate::ports::checker::ActionDeclarations;
use crate::ports::executor::CompiledCode;
use crate::ports::namespaces::{
    DeleteNamespace, DisplayName, DropNamespace, Namespace, NamespaceName, NamespaceWrite, Quotas,
};
use crate::ports::records::{
    Labels, NewVersion, NewWorkflow, StoredActionSet, StoredCode, StoredVersion, StoredWorkflow,
    SuiteChange, SuiteMember, TestRecord, VersionRef, WorkflowCreated, WorkflowFilter,
    WorkflowPatch, WorkflowWrite,
};
use crate::ports::tenancy::{RunRef, Tenant, TenantId};
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
/// - **Tenants.** Every key starts with the tenant: a code, an action set, a version, a test, a
///   namespace or a subscription of one tenant is never visible to another.
/// - **Content.** Code and action sets are stored by digest, and a second put changes nothing.
///   Only code that compiles is stored.
/// - **Workflows** (`archi/07-code-architecture.md`, "`CatalogStore`"). A workflow's id is the
///   engine's, its scope a namespace or the organization, its name unique in its scope.
///   `create_workflow` writes a workflow, its version 1, its suite (version 1's tests) and its
///   code in `workflow_codes`, in one transaction: without a name, for a code its scope already
///   has under labels the version carries, it answers `Existing` with that workflow and changes
///   nothing; a name its scope already has is `NameTaken`. `workflow_named` resolves a name in
///   the namespace given, then among the organization's workflows; `workflow_with_code` reads
///   `workflow_codes` in the namespace then in the organization, the workflow that first had the
///   code there under labels that carry the caller's, and its latest version with that code.
///   Both, `list_workflows` and `versions` see only the workflows and the versions carrying the
///   caller's labels; `workflow` and `version` see them whatever their labels.
/// - **Versions.** A version is immutable. `put_version` takes the next number of its workflow,
///   from 1, under a lock of the workflow; it writes the version's code in `workflow_codes` when
///   its scope has not seen it under those labels, and makes the workflow's suite the version's
///   tests (the tests it no longer has leave with the reason the version gives); with `pulled`, it
///   moves the fork's `source` to that version. `versions` lists them newest first.
/// - **Suites.** `change_suite` adds, replaces and retires tests of a workflow's suite without a
///   version; `suite` reads every test it holds or held. A test replaced or retired leaves with
///   its reason; one added again under its name and digest joins again.
/// - **Names and scopes.** `update_workflow` sets a workflow's name and description without a
///   version. `promote_workflow` moves a namespace's workflow to the organization, with its id,
///   versions, suite and codes, unless the organization has its name (`NameTaken`).
/// - **Tests.** `reserve_tests` inserts a test's row, or makes an existing one new again, before
///   its blob is written, and returns the digests whose deletion has started. `create_workflow`,
///   `put_version` and `change_suite` refuse (`Conflict`) a test without a row, or whose deletion
///   has started. `unreferenced_tests` marks the rows no suite references (a test that left a
///   suite still belongs to it), stored more than [`TEST_GRACE`](crate::ports::TEST_GRACE) ago,
///   and returns them; their blobs are deleted, then `drop_tests` removes the rows still marked.
///   With `erased`, it removes the given rows whatever references them, and the suites' rows
///   that reference them.
/// - **Namespaces.** `ensure_namespace` and `put_namespace` create a namespace at its first use
///   with the tenant's defaults and its partition, [`namespace_partition`]; the partition of a
///   namespace never changes. A namespace being deleted is left as it is (`Deleting`).
///   `delete_namespace` marks it; `namespaces_to_erase` lists the marked ones, the oldest mark
///   first; `drop_namespace` removes a marked one, with its workflows, their versions, codes and
///   suites, once its partition holds nothing of it.
/// - **Changes.** Every write sends what it changed as a `ControlPlaneChange`, in the same
///   transaction: an action set, a workflow (created, named, described, given a version or
///   tests, promoted, moved), a test, a subscription, a tenant, a namespace.
/// - **Time.** The catalog never reads a clock: every method that depends on time takes `now`.
///
/// [`namespace_partition`]: crate::ports::partition::namespace_partition
pub trait CatalogStore: Send + Sync {
    // ---------------------------------------------------------------------------------------
    // Workflows and action sets

    /// Stores a code and its compiled form. Idempotent by digest: a second put changes
    /// nothing.
    fn put_code(
        &self,
        tenant: &TenantId,
        code: &Code,
        compiled: &CompiledCode,
    ) -> impl Future<Output = Result<(), CatalogError>> + Send;

    fn get_code(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<StoredCode>, CatalogError>> + Send;

    /// Stores a pinned action set and its declarations. Idempotent by digest: a second put changes
    /// nothing.
    fn put_action_set(
        &self,
        tenant: &TenantId,
        action_set: &ActionSet,
        declarations: &ActionDeclarations,
    ) -> impl Future<Output = Result<(), CatalogError>> + Send;

    fn get_action_set(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> impl Future<Output = Result<Option<StoredActionSet>, CatalogError>> + Send;

    /// The tenant's action sets, by digest.
    fn list_action_sets(
        &self,
        tenant: &TenantId,
    ) -> impl Future<Output = Result<Vec<StoredActionSet>, CatalogError>> + Send;

    // ---------------------------------------------------------------------------------------
    // Workflows (`archi/01-product.md`, "Workflows")

    /// Creates a workflow with its version 1, its suite and its code, in one transaction.
    /// Without a name, for a code its scope already has under labels its version carries:
    /// `Existing`, that workflow, unchanged. A name its scope already has: `NameTaken`.
    fn create_workflow(
        &self,
        tenant: &TenantId,
        workflow: &NewWorkflow,
        now: Timestamp,
    ) -> impl Future<Output = Result<WorkflowCreated, CatalogError>> + Send;

    /// A workflow by its id, whatever its labels and its scope.
    fn workflow(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
    ) -> impl Future<Output = Result<Option<StoredWorkflow>, CatalogError>> + Send;

    /// The workflow named `name` in `namespace`, else the organization's of that name: a
    /// namespace's shadows the organization's. Without a namespace, the organization's alone.
    /// Only a workflow one of whose versions carries `labels`.
    fn workflow_named(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        name: &str,
        labels: &Labels,
    ) -> impl Future<Output = Result<Option<StoredWorkflow>, CatalogError>> + Send;

    /// The workflow that first had `code` in `namespace` under labels carrying `labels`, else in
    /// the organization, and its latest version with that code carrying `labels`.
    fn workflow_with_code(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        code: Digest,
        labels: &Labels,
    ) -> impl Future<Output = Result<Option<(StoredWorkflow, StoredVersion)>, CatalogError>> + Send;

    /// The workflows the filter holds, by id, after `filter.after`.
    fn list_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> impl Future<Output = Result<Vec<StoredWorkflow>, CatalogError>> + Send;

    /// Sets a workflow's name and description, without a version: its id never changes.
    fn update_workflow(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        patch: &WorkflowPatch,
        now: Timestamp,
    ) -> impl Future<Output = Result<WorkflowWrite, CatalogError>> + Send;

    /// Stores the next version of a workflow, numbered after its latest, under a lock of the
    /// workflow, and makes the workflow's suite the version's tests. `Conflict` for a workflow
    /// that does not exist, or a test without a row.
    fn put_version(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        version: &NewVersion,
        now: Timestamp,
    ) -> impl Future<Output = Result<StoredVersion, CatalogError>> + Send;

    /// The versions of a workflow that carry all of `labels`, newest first.
    fn versions(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        labels: &Labels,
    ) -> impl Future<Output = Result<Vec<StoredVersion>, CatalogError>> + Send;

    /// A version of a workflow by its number or its digest, whatever its labels.
    fn version(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        version: VersionRef,
    ) -> impl Future<Output = Result<Option<StoredVersion>, CatalogError>> + Send;

    /// Adds, replaces and retires tests of a workflow's suite, without a version, and returns
    /// the suite. `Conflict` for a workflow that does not exist, a test without a row, or a test
    /// replaced or retired that is not in the suite.
    fn change_suite(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        change: &SuiteChange,
        now: Timestamp,
    ) -> impl Future<Output = Result<Vec<SuiteMember>, CatalogError>> + Send;

    /// Every test a workflow's suite holds or held, by name, then when it joined.
    fn suite(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
    ) -> impl Future<Output = Result<Vec<SuiteMember>, CatalogError>> + Send;

    /// Moves a namespace's workflow to the organization, with its id, its versions, its suite
    /// and its codes; a code the organization had already keeps the organization's workflow.
    /// An organization's workflow is returned as it is.
    fn promote_workflow(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        now: Timestamp,
    ) -> impl Future<Output = Result<WorkflowWrite, CatalogError>> + Send;

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

    /// Marks as being deleted, and returns, up to `limit` tests that no suite references,
    /// stored or reserved at least `TEST_GRACE` before `now`, whose deletion has not started,
    /// or started at least [`TEST_DELETION_TIMEOUT`] ago.
    fn unreferenced_tests(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<(TenantId, Digest)>, CatalogError>> + Send;

    /// Removes the rows of the given tests once their blobs are deleted: those still marked and
    /// that no suite references; with `erased`, every given row, with the rows of
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

    /// Records where a run `migrate-runs` moved lives (`legacy_runs`), once its partition holds
    /// it. Recording it again, with the same partition, changes nothing; nothing is announced:
    /// the command runs while no node does.
    fn put_legacy_run(
        &self,
        run: &RunRef,
        partition: PartitionId,
    ) -> impl Future<Output = Result<(), CatalogError>> + Send;
}

/// A title a person gave a code before the rename release, with its description
/// (`archi/04-tools.md`, "Workflow names": the migration). The schema migration of the workflow
/// model keeps them for [`CatalogMigration`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeTitle {
    pub tenant: TenantId,
    pub code: Digest,
    pub title: String,
    pub description: Option<String>,
}

/// What `rewind server rename-data` needs of the catalog beyond [`CatalogStore`] to give every run
/// a workflow ([`crate::rename::workflows`]): the titles people gave codes, and the move of an
/// organization's workflow into the namespace that ran it.
pub trait CatalogMigration: CatalogStore {
    /// The titles people gave codes that the catalog still holds: none once
    /// [`drop_code_titles`](Self::drop_code_titles) ran.
    fn code_titles(&self) -> impl Future<Output = Result<Vec<CodeTitle>, CatalogError>> + Send;

    /// Moves an organization's workflow into `namespace`, with its versions, its suite and its
    /// codes: what a saved workflow that one namespace ran becomes. `NameTaken` when the
    /// namespace has a workflow of its name.
    fn scope_workflow(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        namespace: &NamespaceName,
        now: Timestamp,
    ) -> impl Future<Output = Result<WorkflowWrite, CatalogError>> + Send;

    /// Forgets the titles people gave codes, once each became a name or was listed.
    fn drop_code_titles(&self) -> impl Future<Output = Result<(), CatalogError>> + Send;
}

/// A content-addressed document as the catalog stores it, in the names it was written with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDocument {
    pub tenant: TenantId,
    pub digest: Digest,
    pub json: String,
}

/// A workflow version as the catalog stores it, in the names its document was written with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawVersion {
    pub tenant: TenantId,
    pub workflow: WorkflowId,
    pub version: u64,
    pub digest: Digest,
    pub json: String,
}

/// What `rewind server rename-data` needs of the catalog to give its content-addressed documents
/// their new names and digests (`archi/06-storage-and-scale.md`, "Rewriting the stored names":
/// digests): read them as they are stored, whatever names they were written with, and put each
/// one back under the digest of its new form, every reference to it in the catalog with it.
pub trait CatalogDocuments: Send + Sync {
    /// Every pinned action set, every tenant's.
    fn raw_action_sets(
        &self,
    ) -> impl Future<Output = Result<Vec<RawDocument>, CatalogError>> + Send;

    /// Puts the action set stored under `old` back under its new digest, with its declarations,
    /// and removes `old` when it differs.
    fn replace_action_set(
        &self,
        tenant: &TenantId,
        old: Digest,
        action_set: &ActionSet,
        declarations: &ActionDeclarations,
    ) -> impl Future<Output = Result<(), CatalogError>> + Send;

    /// Every workflow version, every tenant's.
    fn raw_versions(&self) -> impl Future<Output = Result<Vec<RawVersion>, CatalogError>> + Send;

    /// Writes a version's new document and digest; its code does not change.
    fn replace_version(
        &self,
        tenant: &TenantId,
        workflow: &WorkflowId,
        version: u64,
        document: &WorkflowVersion,
    ) -> impl Future<Output = Result<(), CatalogError>> + Send;

    /// The tests the catalog keeps, by tenant and digest: their blobs hold their documents.
    fn raw_tests(
        &self,
    ) -> impl Future<Output = Result<Vec<(TenantId, Digest)>, CatalogError>> + Send;

    /// Moves the row of the test `old`, and the suites' references to it, to `new`, of `size`
    /// bytes.
    fn replace_test(
        &self,
        tenant: &TenantId,
        old: Digest,
        new: Digest,
        size: u64,
    ) -> impl Future<Output = Result<(), CatalogError>> + Send;
}
