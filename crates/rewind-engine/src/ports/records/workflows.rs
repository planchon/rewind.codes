//! Workflows as the catalog keeps them, and who watches a run's end (`archi/01-product.md`,
//! "Workflows"; `archi/03-execution.md`, "Workflows across runs"; `archi/06-storage-and-scale.md`,
//! "The control plane").
//!
//! ```text
//! StoredWorkflow (workflows)         id wf_…, scope (a namespace or the organization), name?,
//! │                                  description?, labels, source? (a fork's), created, promoted
//! ├── StoredVersion (workflow_versions)  1, 2, 3…: a WorkflowVersion document and its digest
//! ├── workflow_codes                 each code its scope has seen, under a set of labels
//! └── SuiteMember (workflow_tests)   every test it holds or held, with when and why it left
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use rewind_core::{
    Digest, ReplacedTest, RetiredTest, RunId, Seq, SuiteTest, Timestamp, WorkflowId,
    WorkflowVersion,
};

use crate::ports::namespaces::NamespaceName;
use crate::ports::tenancy::TenantId;

/// Labels: a bounded map of strings.
pub type Labels = BTreeMap<String, String>;

/// Where a workflow belongs (`archi/01-product.md`, "Workflows": its scope): the namespace whose
/// run created it, seen only in it, or the organization once promoted, generic and seen from
/// every namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WorkflowScope {
    Organization,
    Namespace(NamespaceName),
}

impl WorkflowScope {
    /// The namespace, for a namespace's workflow.
    pub fn namespace(&self) -> Option<&NamespaceName> {
        match self {
            WorkflowScope::Organization => None,
            WorkflowScope::Namespace(namespace) => Some(namespace),
        }
    }

    /// The scope of a workflow created in `namespace`, the organization's without one.
    pub fn of(namespace: Option<&NamespaceName>) -> Self {
        namespace.map_or(WorkflowScope::Organization, |namespace| {
            WorkflowScope::Namespace(namespace.clone())
        })
    }

    /// Whether a caller of `namespace` (`None`: every namespace) sees a workflow of this scope:
    /// the organization's always, a namespace's only from itself.
    pub fn seen_from(&self, namespace: Option<&NamespaceName>) -> bool {
        match (self, namespace) {
            (WorkflowScope::Organization, _) | (WorkflowScope::Namespace(_), None) => true,
            (WorkflowScope::Namespace(own), Some(namespace)) => own == namespace,
        }
    }
}

impl fmt::Display for WorkflowScope {
    /// `*` for the organization, as the catalog's `namespace` column writes it; else the
    /// namespace's name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WorkflowScope::Organization => f.write_str("*"),
            WorkflowScope::Namespace(namespace) => namespace.fmt(f),
        }
    }
}

/// A fork's source: the organization's workflow it copies, and the version of it it last took
/// (`archi/01-product.md`, "Workflows": its forks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowSource {
    pub workflow: WorkflowId,
    pub version: u32,
}

/// A workflow as the catalog keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredWorkflow {
    pub id: WorkflowId,
    pub scope: WorkflowScope,
    /// Unique in its scope; `None` for a workflow without one.
    pub name: Option<String>,
    /// The one given, display only: never part of its identity.
    pub description: Option<String>,
    /// The labels of the grant that created it.
    pub labels: Labels,
    /// For a fork, the organization's workflow it copies and the version it last took.
    pub source: Option<WorkflowSource>,
    /// Who created it: a principal, as the audit log names it.
    pub created_by: String,
    pub created_at: Timestamp,
    /// When a member promoted it to the organization.
    pub promoted_at: Option<Timestamp>,
    /// The number of its latest version, whatever its labels.
    pub latest: u32,
}

impl StoredWorkflow {
    /// How its runs' handles name it, the origin of a new run (`archi/06-storage-and-scale.md`,
    /// "Run handles"): its name, else its short id.
    pub fn origin(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.id.short())
    }

    /// `triage` or `wf_7k2m9q4x8d1c`: how a message names it.
    pub fn reference(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.id.to_string())
    }
}

/// A version of a workflow as the catalog keeps it: immutable.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredVersion {
    pub workflow: WorkflowId,
    /// Its number among the versions of its workflow, from 1: `triage@v12`.
    pub version: u32,
    pub document: WorkflowVersion,
    /// The digest of its document: `triage@<digest>`.
    pub digest: Digest,
    /// The labels it is visible under: a principal limited to labels only sees the versions
    /// that carry them. Its document's.
    pub labels: Labels,
    pub created_at: Timestamp,
}

impl StoredVersion {
    /// Whether a caller limited to `labels` sees this version: it carries all of them.
    pub fn carries(&self, labels: &Labels) -> bool {
        carries(&self.labels, labels)
    }
}

/// Whether `held` holds every label of `wanted`.
pub fn carries(held: &Labels, wanted: &Labels) -> bool {
    wanted
        .iter()
        .all(|(key, value)| held.get(key) == Some(value))
}

/// A version named by its number (`@v12`) or its digest (`@<digest>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionRef {
    Number(u32),
    Digest(Digest),
}

/// A workflow to create, with its version 1 (`CatalogStore::create_workflow`).
#[derive(Debug, Clone, PartialEq)]
pub struct NewWorkflow {
    /// Drawn by the engine from `Entropy`.
    pub id: WorkflowId,
    pub scope: WorkflowScope,
    pub name: Option<String>,
    pub description: Option<String>,
    /// The labels of the grant that creates it.
    pub labels: Labels,
    /// For a fork: the organization's workflow and the version of it it copies.
    pub source: Option<WorkflowSource>,
    pub created_by: String,
    /// Its version 1. Its suite's tests are the workflow's first suite.
    pub version: WorkflowVersion,
}

/// What `create_workflow` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowCreated {
    Created(StoredWorkflow),
    /// Without a name, a code its scope already had under those labels: that code's workflow,
    /// unchanged.
    Existing(StoredWorkflow),
    /// Its scope already has a workflow of that name.
    NameTaken,
}

/// The next version of a workflow (`CatalogStore::put_version`).
#[derive(Debug, Clone, PartialEq)]
pub struct NewVersion {
    pub document: WorkflowVersion,
    /// For a fork taking its source's version: that version's number, which the fork's `source`
    /// moves to in the same transaction.
    pub pulled: Option<u32>,
}

/// A change of a workflow's name or description, without a version: `None` leaves a field as it
/// is, `Some(None)` removes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowPatch {
    pub name: Option<Option<String>>,
    pub description: Option<Option<String>>,
}

/// What a write of a workflow did.
#[allow(
    clippy::large_enum_variant,
    reason = "the answer of one write, moved once: its size does not matter"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowWrite {
    Written(StoredWorkflow),
    /// Its scope (the one it moves to) already has a workflow of that name.
    NameTaken,
    NotFound,
}

/// A test of a workflow's suite, as `workflow_tests` keeps it: every test it holds or held, by
/// name and digest, with when it joined and when it left. A test replaced leaves with the reason
/// of its replacement, and its replacement joins under its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuiteMember {
    pub name: String,
    pub digest: Digest,
    pub added_at: Timestamp,
    pub retired_at: Option<Timestamp>,
    /// Why it left: its retirement's or its replacement's reason.
    pub reason: Option<String>,
}

impl SuiteMember {
    /// Whether it is in the suite now.
    pub fn is_current(&self) -> bool {
        self.retired_at.is_none()
    }
}

/// The tests of a suite as they are now: the members still in it, by name.
pub fn current_suite(members: &[SuiteMember]) -> Vec<SuiteTest> {
    let mut tests: Vec<SuiteTest> = members
        .iter()
        .filter(|member| member.is_current())
        .map(|member| SuiteTest {
            name: member.name.clone(),
            test: member.digest,
        })
        .collect();
    tests.sort();
    tests
}

/// Tests added to a workflow's suite, replaced or retired, without a version
/// (`CatalogStore::change_suite`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SuiteChange {
    pub added: Vec<SuiteTest>,
    pub replaced: Vec<ReplacedTest>,
    pub retired: Vec<RetiredTest>,
}

/// Which workflows a list holds, by scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeFilter {
    /// Every workflow of the tenant: the organization's and every namespace's.
    All,
    /// What a caller of this namespace sees: its workflows and the organization's.
    Visible(NamespaceName),
    /// The organization's alone.
    Organization,
    /// This namespace's alone.
    Namespace(NamespaceName),
}

impl ScopeFilter {
    pub fn holds(&self, scope: &WorkflowScope) -> bool {
        match (self, scope) {
            (ScopeFilter::All, _)
            | (ScopeFilter::Visible(_), WorkflowScope::Organization)
            | (ScopeFilter::Organization, WorkflowScope::Organization) => true,
            (ScopeFilter::Visible(wanted), WorkflowScope::Namespace(namespace))
            | (ScopeFilter::Namespace(wanted), WorkflowScope::Namespace(namespace)) => {
                wanted == namespace
            }
            _ => false,
        }
    }
}

/// The workflows to list (`CatalogStore::list_workflows`), by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowFilter {
    pub tenant: TenantId,
    pub scope: ScopeFilter,
    /// Only the workflows one of whose versions has this code.
    pub code: Option<Digest>,
    /// Only the workflows one of whose versions carries all of these labels.
    pub labels: Labels,
    /// Workflows whose id comes after this one: the last of the previous page.
    pub after: Option<WorkflowId>,
    pub limit: usize,
}

/// The index row of a test, whose document is a blob of the `BlobStore` under its digest
/// (`archi/05-testing-and-observability.md`, "Where tests are stored").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRecord {
    pub digest: Digest,
    /// The namespace of the run a recorded test came from; `None` for a test written by hand.
    pub namespace: Option<NamespaceName>,
    /// Size of the document, in bytes.
    pub size: u64,
}

/// How long a test no suite references is kept after it was stored or reserved again.
pub const TEST_GRACE: Duration = Duration::from_secs(24 * 3_600);

/// A run a code started: that run, the call, and whether it is detached
/// (`archi/03-execution.md`, "Workflows across runs").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartedBy {
    pub run: RunId,
    /// The `seq` of the call of `workflows.<name>.start`.
    pub seq: Seq,
    /// A detached run lives on its own: its starter's end does not cancel it.
    pub detached: bool,
}

/// The workflow a run is a run of, and the version it executes: every run started live is a
/// run of a workflow (`archi/01-product.md`, "Workflows").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWorkflow {
    pub id: WorkflowId,
    /// Its name when the run was created.
    pub name: Option<String>,
    /// The version's number.
    pub version: u32,
    /// The version's digest.
    pub digest: Digest,
    /// Whether the version has an interface: its runs then accept only the signals it declares.
    pub interface: bool,
}

impl RunWorkflow {
    pub fn of(workflow: &StoredWorkflow, version: &StoredVersion) -> Self {
        Self {
            id: workflow.id.clone(),
            name: workflow.name.clone(),
            version: version.version,
            digest: version.digest,
            interface: version.document.interface().is_some(),
        }
    }

    /// The origin of a run of it: its name, else its short id.
    pub fn origin(&self) -> String {
        self.name.clone().unwrap_or_else(|| self.id.short())
    }
}
