//! Workflow versions as the store keeps them, and who watches a run's end
//! (`archi/01-product.md`, "Workflows"; `archi/03-execution.md`, "Workflows across runs").

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{Digest, RunId, Seq, Timestamp, Workflow};

use crate::ports::namespaces::NamespaceName;
use crate::ports::tenancy::TenantId;

/// A workflow version as stored: immutable, keyed by its version in its tenant.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredWorkflow {
    pub workflow: Workflow,
    /// The labels it is visible under: a principal limited to labels only sees the versions
    /// that carry them. Its [`Workflow::labels`], except for a version stored before they were
    /// part of the version, which kept the labels of the key that saved it first.
    pub labels: BTreeMap<String, String>,
    pub created_at: Timestamp,
    /// The version's number among the versions of its workflow, from 1 in the order they were
    /// stored (`triage v12`): `put_workflow` gives it.
    pub number: u32,
}

impl StoredWorkflow {
    /// Whether a caller limited to `labels` sees this version: it carries all of them.
    pub fn carries(&self, labels: &BTreeMap<String, String>) -> bool {
        labels
            .iter()
            .all(|(key, value)| self.labels.get(key) == Some(value))
    }
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

/// How long a test no version references is kept after it was stored or reserved again.
pub const TEST_GRACE: Duration = Duration::from_secs(24 * 3_600);

/// The latest version of each workflow of a tenant, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowFilter {
    pub tenant: TenantId,
    /// Only the versions carrying all of these labels count.
    pub labels: BTreeMap<String, String>,
    /// Workflows whose name comes after this one: the last name of the previous page.
    pub after: Option<String>,
    pub limit: Option<usize>,
}

/// A run a script started: that run, the call, and whether it is detached
/// (`archi/03-execution.md`, "Workflows across runs").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartedBy {
    pub run: RunId,
    /// The `seq` of the call of `workflows.<name>.start`.
    pub seq: Seq,
    /// A detached run lives on its own: its starter's end does not cancel it.
    pub detached: bool,
}

/// The workflow version a run executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunWorkflow {
    pub name: String,
    pub version: Digest,
}
