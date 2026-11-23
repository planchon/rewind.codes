//! Saving a workflow version through its gate (`archi/01-product.md`, "Workflows";
//! `archi/05-testing-and-observability.md`, "Suites and the gate").
//!
//! ```text
//! register_workflow ── checks (name, schemas, script, attachments, tsgo) ──┐
//!   lineage: the parent named, or the latest version under the same labels │
//!   the suite: the parent's, retired, updated, added ─► run_tests ─► all pass?
//!                                     reserve test rows, put their blobs ◄─┘
//!                                     put_script, put_workflow (the catalog)
//! ```
//!
//! Everything a save reads and writes is the control plane's: no partition takes part. The suite
//! runs in the driver, on this process's executor ([`Edge::run_tests`]).

use rewind_core::{Attachments, Script, Toolset, Workflow};

use super::Edge;
use crate::attachments::{self, AttachRights, AttachmentCheck};
use crate::plane::Plane;
use crate::ports::{
    CatalogStore, Clock, ScriptExecutor, Severity, StoreError, StoredWorkflow, TenantId,
    TypeChecker,
};
use crate::schema::Validator;
use crate::suites::{SuiteChanges, SuiteError, Suites, TestFormat, TestKit};
use crate::testing::{MockLoader, Subject};
use crate::text;
use crate::workflows::{RegisterError, RegisterWorkflow, same_content};

impl<P: Plane> Edge<P> {
    /// The tests of the control plane, in a stored format.
    pub fn suites(&self, format: TestFormat) -> Suites<'_, P::Catalog, P::Blobs> {
        Suites {
            catalog: self.plane.catalog(),
            blobs: self.plane.blobs(),
            format,
        }
    }

    /// Checks a workflow and stores it as a new version: its name and signals, its schemas, its
    /// script against its toolset (the checks at start), its attachments, then tsgo against
    /// the toolset's and its own declarations; then the gate: its suite, composed from its
    /// parent's, runs against it, and the version is stored only if every test passes. Saving
    /// a version again under the same labels changes nothing; under other labels, it is another
    /// version, carrying them.
    pub async fn register_workflow<K: TypeChecker, L: MockLoader>(
        &self,
        request: RegisterWorkflow,
        checker: &K,
        kit: &TestKit<'_, L>,
    ) -> Result<StoredWorkflow, RegisterError> {
        text::refuse_nul("the script", request.script.source())?;
        text::refuse_nul("the workflow's name", &request.name)?;
        if let Some(description) = &request.description {
            text::refuse_nul("the workflow's description", description)?;
        }
        text::refuse_nul_in_labels(&request.labels)?;
        let workflow = Workflow::new(
            request.name,
            request.description,
            request.script.digest(),
            request.toolset,
            request.schemas,
        )?
        .with_labels(request.labels)
        .with_secrets(request.secrets);
        let schemas = workflow.schemas();
        let named = [
            ("input".to_owned(), &schemas.input),
            ("output".to_owned(), &schemas.output),
        ];
        let signals = schemas
            .signals
            .iter()
            .map(|(name, schema)| (format!("{name:?} signal"), schema));
        for (which, schema) in named.into_iter().chain(signals) {
            if let Some(message) = Validator::compile(schema).unusable() {
                return Err(RegisterError::Schema {
                    which,
                    message: message.to_owned(),
                });
            }
        }
        let tenant = request.tenant;
        let catalog = self.plane.catalog();
        let stored = catalog
            .get_toolset(&tenant, request.toolset)
            .await
            .map_err(StoreError::from)?
            .ok_or(RegisterError::ToolsetNotFound(request.toolset))?;
        let compiled = self
            .plane
            .executor()
            .compile(&request.script, &stored.toolset)
            .map_err(RegisterError::InvalidScript)?;
        self.check_saved_attachments(
            &tenant,
            &request.script,
            &stored.toolset,
            workflow.secrets(),
            &request.attach,
        )
        .await?;
        let diagnostics = checker
            .check_workflow(&request.script, &stored.declarations, &request.declarations)
            .await?;
        if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error)
        {
            return Err(RegisterError::TypeErrors(diagnostics));
        }

        // The latest version saved under the same labels: the lineage of this scope. A version
        // saved under other labels, which a caller sees when its scope holds theirs, is another
        // lineage's.
        let latest = catalog
            .workflow_versions(&tenant, workflow.name(), workflow.labels())
            .await
            .map_err(StoreError::from)?
            .into_iter()
            .find(|stored| &stored.labels == workflow.labels());
        let suites = self.suites(kit.format);
        // A save sent again, such as a retry whose answer was lost: the version it made.
        if let Some(latest) = &latest
            && self
                .saved_by(
                    &suites,
                    &tenant,
                    &latest.workflow,
                    &workflow,
                    &request.suite,
                )
                .await?
        {
            return Ok(latest.clone());
        }

        // The gate.
        let parent = match request.suite.parent {
            Some(version) => {
                let parent = catalog
                    .get_workflow(&tenant, version)
                    .await
                    .map_err(StoreError::from)?
                    .filter(|stored| stored.carries(workflow.labels()))
                    .ok_or(SuiteError::ParentNotFound(version))?;
                Some(parent.workflow)
            }
            None => latest.map(|stored| stored.workflow),
        };
        // The same workflow saved again on a parent named, its suite unchanged: that version.
        if let Some(parent) = &parent
            && same_content(parent, &workflow)
            && request.suite.tests.is_empty()
            && request.suite.update.is_empty()
            && request.suite.retire.is_empty()
            && let Some(stored) = catalog
                .get_workflow(&tenant, parent.version())
                .await
                .map_err(StoreError::from)?
        {
            return Ok(stored);
        }
        let composed = suites
            .compose_suite(&tenant, parent.as_ref(), &request.suite)
            .await?;
        let reports = self
            .run_tests(
                kit.loader,
                &Subject {
                    script: &request.script,
                    toolset: &stored.toolset,
                    interface: Some(workflow.schemas()),
                },
                &composed.tests,
                &kit.options,
            )
            .await?;
        if reports.iter().any(|report| !report.passed) {
            return Err(RegisterError::TestsFailed(reports));
        }
        let workflow = workflow.with_suite(composed.suite)?;
        suites
            .store_tests(&tenant, &composed.tests, self.now())
            .await?;
        catalog
            .put_script(&tenant, &request.script, &compiled)
            .await
            .map_err(StoreError::from)?;
        let stored = catalog
            .put_workflow(&tenant, &workflow, self.now())
            .await
            .map_err(StoreError::from)?;
        tracing::debug!(workflow = %stored.workflow, "workflow registered");
        Ok(stored)
    }

    /// Whether `latest` is the version this save makes: the same content, and the suite the
    /// changes compose on `latest`'s own parent. So a save sent again (a retry whose answer was
    /// lost, a seed run twice) answers the version it made, instead of applying its changes a
    /// second time on top of it.
    async fn saved_by(
        &self,
        suites: &Suites<'_, P::Catalog, P::Blobs>,
        tenant: &TenantId,
        latest: &Workflow,
        workflow: &Workflow,
        changes: &SuiteChanges,
    ) -> Result<bool, RegisterError> {
        let base = latest.suite().parent;
        if !same_content(latest, workflow)
            || changes.parent.is_some_and(|named| Some(named) != base)
        {
            return Ok(false);
        }
        let base = match base {
            Some(version) => match self
                .plane
                .catalog()
                .get_workflow(tenant, version)
                .await
                .map_err(StoreError::from)?
            {
                Some(stored) => Some(stored.workflow),
                None => return Ok(false),
            },
            None => None,
        };
        // A change that does not apply to the parent is reported by the save itself.
        let Ok(composed) = suites.compose_suite(tenant, base.as_ref(), changes).await else {
            return Ok(false);
        };
        Ok(workflow
            .clone()
            .with_suite(composed.suite)
            .is_ok_and(|made| made.version() == latest.version()))
    }

    /// Refuses the attachments of a version to save that do not fit its toolset and its script,
    /// or that the saver may not attach: an organization's secret must exist and be theirs to
    /// attach; an `ns/` secret is checked by its name only, since no namespace is known yet
    /// (`archi/04-tools.md`, "Secrets": a workflow version is the permission).
    async fn check_saved_attachments(
        &self,
        tenant: &TenantId,
        script: &Script,
        toolset: &Toolset,
        secrets: &Attachments,
        rights: &AttachRights,
    ) -> Result<(), RegisterError> {
        if secrets.is_empty() && toolset.taking_secrets().next().is_none() {
            return Ok(());
        }
        let imports = self.plane.executor().tool_imports(script);
        let check = AttachmentCheck {
            tenant,
            namespace: None,
            toolset,
            imports: imports.as_ref(),
            attachments: secrets,
            rights,
        };
        attachments::check(self.plane.secrets(), self.plane.clock().now(), check)
            .await?
            .map_err(RegisterError::SecretRequired)
    }
}
