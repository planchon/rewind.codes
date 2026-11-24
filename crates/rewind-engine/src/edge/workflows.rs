//! The workflows of the catalog (`archi/01-product.md`, "Workflows";
//! `archi/05-testing-and-observability.md`, "Suites and the gate"): finding one by a reference,
//! creating one, making its next version through its gate, changing its tests, naming it,
//! promoting, forking and pulling it.
//!
//! ```text
//! create_workflow ── checks (name, schemas, code, secrets, tsgo) ─► the gate ─► create_workflow
//! make_version ───── the same, defaults the latest version's ─────► the gate on the workflow's
//!                                                                    suite ─► put_version
//! change_tests ───── the tests given, against the latest version ──► change_suite
//! fork_workflow ──── an organization's workflow, its suite copied ──► create_workflow (source)
//! pull_workflow ──── the source's latest version, the fork's suite ─► put_version (pulled)
//! ```
//!
//! Everything a write reads and writes is the control plane's: no partition takes part. A suite
//! runs in the driver, on this process's executor ([`Edge::run_tests`]).

use rewind_core::{
    ActionSet, Code, Digest, ERASED_TEST_REASON, ReplacedTest, RetiredTest, ServerSecrets, Suite,
    Test, WorkflowId, WorkflowSchemas, WorkflowVersion, check_workflow_name,
};

use super::Edge;
use crate::plane::Plane;
use crate::ports::{
    CatalogStore, Clock, CodeExecutor, CompiledCode, Entropy, Labels, NamespaceName, NewVersion,
    NewWorkflow, ScopeFilter, Severity, StoreError, StoredActionSet, StoredVersion, StoredWorkflow,
    SuiteChange, SuiteMember, TenantId, TestRecord, TypeChecker, VersionRef, WorkflowCreated,
    WorkflowDeclarations, WorkflowFilter, WorkflowPatch, WorkflowScope, WorkflowSource,
    WorkflowWrite, current_suite,
};
use crate::schema::Validator;
use crate::secrets::{self, SecretCheck, SecretRights};
use crate::suites::{SuiteChanges, Suites, TestFormat, TestKit};
use crate::testing::{HandlerLoader, Subject};
use crate::text;
use crate::workflows::{
    ChangeTests, CreateWorkflow, ForkWorkflow, MAX_DESCRIPTION_CHARS, MakeVersion, PullWorkflow,
    WorkflowError, WorkflowMaker, WrittenWorkflow,
};

/// Hex digits a reference to a version by its digest gives at least.
pub const MIN_VERSION_DIGITS: usize = 12;

/// What the workflows' writes need besides the plane: the type-checker, the tests' kit, and how
/// `rewind-codegen` declares an interface (the composition root's).
pub struct WorkflowKit<'a, K, L> {
    pub checker: &'a K,
    pub tests: TestKit<'a, L>,
    pub declare: fn(&str, &WorkflowSchemas) -> WorkflowDeclarations,
}

impl<K, L> std::fmt::Debug for WorkflowKit<'_, K, L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowKit").finish_non_exhaustive()
    }
}

/// Why a reference to a workflow names none the caller sees.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// The reference cannot name a workflow or a version: the message says how to write one.
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<crate::ports::CatalogError> for ResolveError {
    fn from(error: crate::ports::CatalogError) -> Self {
        ResolveError::Store(error.into())
    }
}

impl From<ResolveError> for WorkflowError {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::Invalid(message) | ResolveError::NotFound(message) => {
                WorkflowError::NotFound(message)
            }
            ResolveError::Store(error) => WorkflowError::Store(error),
        }
    }
}

/// A version named after `@`: `v12`, or a digest, its first 12 hex digits at least.
#[derive(Debug, Clone, PartialEq, Eq)]
enum VersionSpec {
    Number(u32),
    DigestPrefix(String),
}

/// A reference split: the workflow (its id or its name) and the version, if it names one.
fn split_reference(reference: &str) -> Result<(&str, Option<VersionSpec>), ResolveError> {
    let Some((workflow, version)) = reference.split_once('@') else {
        return Ok((reference, None));
    };
    let version = version.to_ascii_lowercase();
    // A digest's hex digits hold no `v`: `v12` is a number.
    if let Some(number) = version.strip_prefix('v') {
        let number = number.parse::<u32>().map_err(|_| {
            ResolveError::Invalid(format!(
                "workflow {reference:?}: name a version by its number, as in triage@v12, or by \
                 its digest or its first {MIN_VERSION_DIGITS} hex digits at least"
            ))
        })?;
        return Ok((workflow, Some(VersionSpec::Number(number))));
    }
    if version.len() < MIN_VERSION_DIGITS
        || version.len() > 64
        || !version.chars().all(|c| c.is_ascii_hexdigit())
    {
        return Err(ResolveError::Invalid(format!(
            "workflow {reference:?}: name a version by its digest or its first \
             {MIN_VERSION_DIGITS} hex digits at least, as in triage@9f2c4e1a07b3, or by its \
             number, as in triage@v12"
        )));
    }
    Ok((workflow, Some(VersionSpec::DigestPrefix(version))))
}

fn not_found(reference: &str) -> ResolveError {
    ResolveError::NotFound(format!(
        "no workflow {reference} that this caller sees: list them with GET /v1/workflows; a run \
         given code creates one"
    ))
}

impl<P: Plane> Edge<P> {
    /// The tests of the control plane, in a stored format.
    pub fn suites(&self, format: TestFormat) -> Suites<'_, P::Catalog, P::Blobs> {
        Suites {
            catalog: self.plane.catalog(),
            blobs: self.plane.blobs(),
            format,
        }
    }

    /// The workflow a reference names among those a caller sees: its id, or its name, which
    /// resolves in `namespace` first, then among the organization's workflows. Without a
    /// namespace, the caller sees every namespace's workflows by id, and the organization's by
    /// name. A reference that names a version is refused.
    pub async fn find_workflow(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        labels: &Labels,
        reference: &str,
    ) -> Result<StoredWorkflow, ResolveError> {
        let (workflow, version) = split_reference(reference)?;
        if version.is_some() {
            return Err(ResolveError::Invalid(format!(
                "{reference:?} names a version: name the workflow alone, by its id or its name"
            )));
        }
        self.workflow_of(tenant, namespace, labels, workflow)
            .await?
            .ok_or_else(|| not_found(reference))
    }

    async fn workflow_of(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        labels: &Labels,
        workflow: &str,
    ) -> Result<Option<StoredWorkflow>, ResolveError> {
        let catalog = self.plane.catalog();
        if let Ok(id) = workflow.parse::<WorkflowId>() {
            let Some(found) = catalog.workflow(tenant, &id).await? else {
                return Ok(None);
            };
            if !found.scope.seen_from(namespace) {
                return Ok(None);
            }
            let visible = !catalog.versions(tenant, &id, labels).await?.is_empty();
            return Ok(visible.then_some(found));
        }
        Ok(catalog
            .workflow_named(tenant, namespace, workflow, labels)
            .await?)
    }

    /// The workflow and the version a reference names among those a caller sees:
    /// `triage`, `wf_7k2m9q4x8d1c`, `triage@v12` or `triage@<digest>` (its first 12 hex digits
    /// at least); the latest version the caller sees when it names none.
    pub async fn resolve_workflow(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        labels: &Labels,
        reference: &str,
    ) -> Result<(StoredWorkflow, StoredVersion), ResolveError> {
        let (workflow, spec) = split_reference(reference)?;
        let found = self
            .workflow_of(tenant, namespace, labels, workflow)
            .await?
            .ok_or_else(|| not_found(reference))?;
        let catalog = self.plane.catalog();
        let version = match spec {
            None => catalog
                .versions(tenant, &found.id, labels)
                .await?
                .into_iter()
                .next(),
            Some(VersionSpec::Number(number)) => catalog
                .version(tenant, &found.id, VersionRef::Number(number))
                .await?
                .filter(|version| version.carries(labels)),
            Some(VersionSpec::DigestPrefix(prefix)) => catalog
                .versions(tenant, &found.id, labels)
                .await?
                .into_iter()
                .find(|version| version.digest.to_hex().starts_with(&prefix)),
        };
        let version = version.ok_or_else(|| not_found(reference))?;
        Ok((found, version))
    }

    /// Creates a workflow with its version 1, through its gate (`POST /v1/workflows`). Without
    /// a name, a code its scope already has answers that code's workflow, unchanged. A creation
    /// sent again (its answer lost) answers the workflow it created.
    pub async fn create_workflow<K: TypeChecker, L: HandlerLoader>(
        &self,
        request: CreateWorkflow,
        kit: &WorkflowKit<'_, K, L>,
    ) -> Result<WrittenWorkflow, WorkflowError> {
        check_texts(request.name.as_deref(), request.description.as_deref())?;
        text::refuse_nul("the code", request.code.source())?;
        let tenant = &request.tenant;
        let maker = &request.maker;
        let catalog = self.plane.catalog();
        let candidate = WorkflowVersion::new(request.code.digest(), request.action_set)
            .with_interface(request.interface.clone())?
            .with_labels(maker.labels.clone())
            .with_secrets(request.secrets.clone());
        if request.name.is_none()
            && let Some(existing) = self
                .scope_has_code(tenant, &request.scope, request.code.digest(), &maker.labels)
                .await?
        {
            return Ok(existing);
        }
        if let Some(name) = &request.name
            && let Some(existing) = self
                .made_before(
                    tenant,
                    &request.scope,
                    name,
                    &candidate,
                    &request.tests,
                    kit,
                )
                .await?
        {
            return Ok(existing);
        }
        let checked = self
            .check_version(
                tenant,
                &request.code,
                &candidate,
                request.name.as_deref(),
                kit,
                maker,
            )
            .await?;
        let suites = self.suites(kit.tests.format);
        let composed = suites
            .compose_suite(
                tenant,
                None,
                &[],
                &SuiteChanges {
                    tests: request.tests.clone(),
                    ..SuiteChanges::default()
                },
            )
            .await?;
        self.gate(
            &request.code,
            &checked.action_set,
            &candidate,
            &composed.tests,
            kit,
        )
        .await?;
        let version = candidate.with_suite(composed.suite)?;
        suites
            .store_tests(tenant, &composed.tests, self.now())
            .await?;
        catalog
            .put_code(tenant, &request.code, &checked.compiled)
            .await?;
        let created = catalog
            .create_workflow(
                tenant,
                &NewWorkflow {
                    id: self.plane.entropy().workflow_id(),
                    scope: request.scope.clone(),
                    name: request.name.clone(),
                    description: request.description.clone(),
                    labels: maker.labels.clone(),
                    source: None,
                    created_by: maker.principal.clone(),
                    version,
                },
                self.now(),
            )
            .await?;
        let (workflow, created) = match created {
            WorkflowCreated::Created(workflow) => (workflow, true),
            WorkflowCreated::Existing(workflow) => (workflow, false),
            WorkflowCreated::NameTaken => {
                return Err(name_taken(
                    request.name.as_deref().unwrap_or_default(),
                    &request.scope,
                ));
            }
        };
        let version = self
            .latest_with_code(tenant, &workflow, request.code.digest(), &maker.labels)
            .await?;
        tracing::debug!(workflow = %workflow.id, created, "workflow created");
        Ok(WrittenWorkflow {
            workflow,
            version,
            created,
        })
    }

    /// Makes the next version of a workflow, through its gate: what the request leaves out is
    /// its latest version's. The same content, without a change of tests, makes no version: the
    /// latest one is answered.
    pub async fn make_version<K: TypeChecker, L: HandlerLoader>(
        &self,
        request: MakeVersion,
        kit: &WorkflowKit<'_, K, L>,
    ) -> Result<WrittenWorkflow, WorkflowError> {
        let tenant = &request.tenant;
        let maker = &request.maker;
        let workflow = &request.workflow;
        let catalog = self.plane.catalog();
        let versions = catalog
            .versions(tenant, &workflow.id, &maker.labels)
            .await?;
        let latest = versions.first().cloned().ok_or_else(|| {
            WorkflowError::NotFound(format!(
                "the workflow {} has no version this caller sees",
                workflow.reference()
            ))
        })?;
        // Its lineage: the latest version made under the caller's labels.
        let base = versions
            .iter()
            .find(|version| version.labels == maker.labels)
            .cloned();
        let code = match &request.code {
            Some(code) => code.clone(),
            None => self.code_of(tenant, &latest).await?,
        };
        text::refuse_nul("the code", code.source())?;
        let interface = match &request.interface {
            Some(interface) => Some(interface.clone()),
            None => latest.document.interface().cloned(),
        };
        let candidate = WorkflowVersion::new(
            code.digest(),
            request.action_set.unwrap_or(latest.document.action_set()),
        )
        .with_interface(interface)?
        .with_labels(maker.labels.clone())
        .with_secrets(
            request
                .secrets
                .clone()
                .unwrap_or_else(|| latest.document.secrets().clone()),
        );
        if let Some(base) = &base
            && base.document.same_content(&candidate)
            && (request.suite.is_empty() || self.made_by(tenant, base, &request.suite, kit).await?)
        {
            return Ok(WrittenWorkflow {
                workflow: workflow.clone(),
                version: base.clone(),
                created: false,
            });
        }
        let checked = self
            .check_version(
                tenant,
                &code,
                &candidate,
                workflow.name.as_deref(),
                kit,
                maker,
            )
            .await?;
        let members = catalog.suite(tenant, &workflow.id).await?;
        let suites = self.suites(kit.tests.format);
        let composed = suites
            .compose_suite(
                tenant,
                base.as_ref().map(|base| base.digest),
                &current_suite(&members),
                &request.suite,
            )
            .await?;
        self.gate(&code, &checked.action_set, &candidate, &composed.tests, kit)
            .await?;
        let suite = with_changes_since(composed.suite, &members, &latest);
        let version = candidate.with_suite(suite)?;
        suites
            .store_tests(tenant, &composed.tests, self.now())
            .await?;
        catalog.put_code(tenant, &code, &checked.compiled).await?;
        let stored = catalog
            .put_version(
                tenant,
                &workflow.id,
                &NewVersion {
                    document: version,
                    pulled: None,
                },
                self.now(),
            )
            .await?;
        let workflow = self.reread(tenant, &workflow.id).await?;
        tracing::debug!(workflow = %workflow.id, version = stored.version, "workflow version made");
        Ok(WrittenWorkflow {
            workflow,
            version: stored,
            created: true,
        })
    }

    /// Adds tests to a workflow's suite, replaces or retires some, without a version: each
    /// test given, and each replacement, runs against its latest version first.
    pub async fn change_tests<K: TypeChecker, L: HandlerLoader>(
        &self,
        request: ChangeTests,
        kit: &WorkflowKit<'_, K, L>,
    ) -> Result<(StoredWorkflow, Vec<SuiteMember>), WorkflowError> {
        let tenant = &request.tenant;
        let workflow = &request.workflow;
        let catalog = self.plane.catalog();
        let latest = catalog
            .versions(tenant, &workflow.id, &request.maker.labels)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                WorkflowError::NotFound(format!(
                    "the workflow {} has no version this caller sees",
                    workflow.reference()
                ))
            })?;
        let members = catalog.suite(tenant, &workflow.id).await?;
        if request.suite.is_empty() {
            return Ok((workflow.clone(), members));
        }
        let suites = self.suites(kit.tests.format);
        let composed = suites
            .compose_suite(
                tenant,
                Some(latest.digest),
                &current_suite(&members),
                &request.suite,
            )
            .await?;
        let code = self.code_of(tenant, &latest).await?;
        let action_set = self
            .action_set_of(tenant, latest.document.action_set())
            .await?;
        self.gate(
            &code,
            &action_set.action_set,
            &latest.document,
            &composed.new,
            kit,
        )
        .await?;
        suites
            .store_tests(tenant, &composed.new, self.now())
            .await?;
        let replaced: Vec<ReplacedTest> = composed.suite.replaced.clone();
        let replacing: Vec<&str> = replaced.iter().map(|test| test.name.as_str()).collect();
        let added = composed
            .suite
            .tests
            .iter()
            .filter(|test| {
                !replacing.contains(&test.name.as_str())
                    && !members.iter().any(|member| {
                        member.is_current()
                            && member.name == test.name
                            && member.digest == test.test
                    })
            })
            .cloned()
            .collect();
        let suite = catalog
            .change_suite(
                tenant,
                &workflow.id,
                &SuiteChange {
                    added,
                    replaced,
                    retired: composed.suite.retired.clone(),
                },
                self.now(),
            )
            .await?;
        Ok((self.reread(tenant, &workflow.id).await?, suite))
    }

    /// Names or describes a workflow, without a version: its id never changes.
    pub async fn update_workflow(
        &self,
        tenant: &TenantId,
        workflow: &StoredWorkflow,
        patch: &WorkflowPatch,
    ) -> Result<StoredWorkflow, WorkflowError> {
        check_texts(
            patch.name.as_ref().and_then(Option::as_deref),
            patch.description.as_ref().and_then(Option::as_deref),
        )?;
        match self
            .plane
            .catalog()
            .update_workflow(tenant, &workflow.id, patch, self.now())
            .await?
        {
            WorkflowWrite::Written(workflow) => Ok(workflow),
            WorkflowWrite::NameTaken => Err(name_taken(
                patch
                    .name
                    .as_ref()
                    .and_then(Option::as_deref)
                    .unwrap_or_default(),
                &workflow.scope,
            )),
            WorkflowWrite::NotFound => Err(WorkflowError::NotFound(format!(
                "no workflow {}",
                workflow.id
            ))),
        }
    }

    /// Moves a namespace's workflow to the organization, with its id, versions and suite.
    pub async fn promote_workflow(
        &self,
        tenant: &TenantId,
        workflow: &StoredWorkflow,
    ) -> Result<StoredWorkflow, WorkflowError> {
        match self
            .plane
            .catalog()
            .promote_workflow(tenant, &workflow.id, self.now())
            .await?
        {
            WorkflowWrite::Written(workflow) => Ok(workflow),
            WorkflowWrite::NameTaken => Err(name_taken(
                workflow.name.as_deref().unwrap_or_default(),
                &WorkflowScope::Organization,
            )),
            WorkflowWrite::NotFound => Err(WorkflowError::NotFound(format!(
                "no workflow {}",
                workflow.id
            ))),
        }
    }

    /// Forks an organization's workflow into a namespace: a workflow of the namespace with its
    /// name and description, its source the version forked, its version 1 that version (code,
    /// interface, action set, secrets), and a copy of its suite without the tests recorded from
    /// another namespace's runs. The secrets are checked against the maker's rights.
    pub async fn fork_workflow(
        &self,
        request: ForkWorkflow,
    ) -> Result<WrittenWorkflow, WorkflowError> {
        let tenant = &request.tenant;
        let source = &request.source;
        let maker = &request.maker;
        if source.scope != WorkflowScope::Organization {
            return Err(WorkflowError::NotForkable(format!(
                "the workflow {} is a namespace's: only the organization's workflows are \
                 forked; promote it first",
                source.reference()
            )));
        }
        let catalog = self.plane.catalog();
        let version = match request.version {
            Some(version) => version,
            None => catalog
                .versions(tenant, &source.id, &maker.labels)
                .await?
                .into_iter()
                .next()
                .ok_or_else(|| {
                    WorkflowError::NotFound(format!(
                        "the workflow {} has no version this caller sees",
                        source.reference()
                    ))
                })?,
        };
        let document = &version.document;
        let code = self.code_of(tenant, &version).await?;
        let action_set = self.action_set_of(tenant, document.action_set()).await?;
        self.check_version_secrets(
            tenant,
            &code,
            &action_set.action_set,
            document.secrets(),
            &maker.secret_rights,
        )
        .await?;
        let mut tests = Vec::new();
        for test in &document.suite().tests {
            let recorded = catalog.get_test(tenant, test.test).await?;
            if recorded_elsewhere(recorded.as_ref(), &request.namespace) {
                continue;
            }
            tests.push(test.clone());
        }
        let copy = WorkflowVersion::new(document.code(), document.action_set())
            .with_interface(document.interface().cloned())?
            .with_labels(maker.labels.clone())
            .with_secrets(document.secrets().clone())
            .with_suite(Suite {
                tests,
                ..Suite::default()
            })?;
        let scope = WorkflowScope::Namespace(request.namespace.clone());
        let created = catalog
            .create_workflow(
                tenant,
                &NewWorkflow {
                    id: self.plane.entropy().workflow_id(),
                    scope: scope.clone(),
                    name: source.name.clone(),
                    description: source.description.clone(),
                    labels: maker.labels.clone(),
                    source: Some(WorkflowSource {
                        workflow: source.id.clone(),
                        version: version.version,
                    }),
                    created_by: maker.principal.clone(),
                    version: copy,
                },
                self.now(),
            )
            .await?;
        match created {
            WorkflowCreated::Created(workflow) | WorkflowCreated::Existing(workflow) => {
                let version = catalog
                    .version(tenant, &workflow.id, VersionRef::Number(1))
                    .await?
                    .ok_or_else(|| {
                        WorkflowError::Store(StoreError::Backend(format!(
                            "the fork {} has no version 1",
                            workflow.id
                        )))
                    })?;
                tracing::debug!(fork = %workflow.id, source = %source.id, "workflow forked");
                Ok(WrittenWorkflow {
                    workflow,
                    version,
                    created: true,
                })
            }
            WorkflowCreated::NameTaken => Err(name_taken(
                source.name.as_deref().unwrap_or_default(),
                &scope,
            )),
        }
    }

    /// Takes a fork's source's latest version (its code, interface, action set and secrets) as
    /// the fork's next version once the fork's own suite passes against it. Nothing to pull
    /// answers the fork unchanged.
    pub async fn pull_workflow<K: TypeChecker, L: HandlerLoader>(
        &self,
        request: PullWorkflow,
        kit: &WorkflowKit<'_, K, L>,
    ) -> Result<WrittenWorkflow, WorkflowError> {
        let tenant = &request.tenant;
        let fork = &request.fork;
        let maker = &request.maker;
        let Some(source) = &fork.source else {
            return Err(WorkflowError::NotAFork(format!(
                "the workflow {} is no fork: it has no source to pull",
                fork.reference()
            )));
        };
        let catalog = self.plane.catalog();
        let latest = catalog
            .versions(tenant, &fork.id, &maker.labels)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                WorkflowError::NotFound(format!(
                    "the workflow {} has no version this caller sees",
                    fork.reference()
                ))
            })?;
        let pulled = catalog
            .versions(tenant, &source.workflow, &maker.labels)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| {
                WorkflowError::NotFound(format!(
                    "the source {} of {} has no version this caller sees",
                    source.workflow,
                    fork.reference()
                ))
            })?;
        if pulled.version <= source.version {
            return Ok(WrittenWorkflow {
                workflow: fork.clone(),
                version: latest,
                created: false,
            });
        }
        let document = &pulled.document;
        let code = self.code_of(tenant, &pulled).await?;
        let candidate = WorkflowVersion::new(document.code(), document.action_set())
            .with_interface(document.interface().cloned())?
            .with_labels(maker.labels.clone())
            .with_secrets(document.secrets().clone());
        let checked = self
            .check_version(tenant, &code, &candidate, fork.name.as_deref(), kit, maker)
            .await?;
        let members = catalog.suite(tenant, &fork.id).await?;
        let suites = self.suites(kit.tests.format);
        let composed = suites
            .compose_suite(
                tenant,
                Some(latest.digest),
                &current_suite(&members),
                &SuiteChanges::default(),
            )
            .await?;
        self.gate(&code, &checked.action_set, &candidate, &composed.tests, kit)
            .await?;
        let suite = with_changes_since(composed.suite, &members, &latest);
        let version = candidate.with_suite(suite)?;
        let stored = catalog
            .put_version(
                tenant,
                &fork.id,
                &NewVersion {
                    document: version,
                    pulled: Some(pulled.version),
                },
                self.now(),
            )
            .await?;
        Ok(WrittenWorkflow {
            workflow: self.reread(tenant, &fork.id).await?,
            version: stored,
            created: true,
        })
    }

    // ---------------------------------------------------------------------------------------
    // What the writes share

    /// Checks a version to make: its schemas, its code against its action set (the checks at
    /// start), its secrets against the maker's rights, then tsgo against the action set's and
    /// its own declarations.
    async fn check_version<K: TypeChecker, L>(
        &self,
        tenant: &TenantId,
        code: &Code,
        candidate: &WorkflowVersion,
        name: Option<&str>,
        kit: &WorkflowKit<'_, K, L>,
        maker: &WorkflowMaker,
    ) -> Result<Checked, WorkflowError> {
        if let Some(schemas) = candidate.interface() {
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
                    return Err(WorkflowError::Schema {
                        which,
                        message: message.to_owned(),
                    });
                }
            }
        }
        let stored = self.action_set_of(tenant, candidate.action_set()).await?;
        let compiled = self
            .plane
            .executor()
            .compile(code, &stored.action_set)
            .map_err(WorkflowError::InvalidCode)?;
        self.check_version_secrets(
            tenant,
            code,
            &stored.action_set,
            candidate.secrets(),
            &maker.secret_rights,
        )
        .await?;
        let diagnostics = match candidate.interface() {
            Some(schemas) => {
                let declarations = (kit.declare)(name.unwrap_or("workflow"), schemas);
                kit.checker
                    .check_workflow(code, &stored.declarations, &declarations)
                    .await?
            }
            None => kit.checker.check(code, &stored.declarations).await?,
        };
        if diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error)
        {
            return Err(WorkflowError::TypeErrors(diagnostics));
        }
        Ok(Checked {
            action_set: stored.action_set,
            compiled,
        })
    }

    /// The gate: every test runs against the version; it is stored only if each passes.
    async fn gate<K, L: HandlerLoader>(
        &self,
        code: &Code,
        action_set: &ActionSet,
        candidate: &WorkflowVersion,
        tests: &[Test],
        kit: &WorkflowKit<'_, K, L>,
    ) -> Result<(), WorkflowError> {
        if tests.is_empty() {
            return Ok(());
        }
        let reports = self
            .run_tests(
                kit.tests.loader,
                &Subject {
                    code,
                    action_set,
                    interface: candidate.interface(),
                },
                tests,
                &kit.tests.options,
            )
            .await?;
        if reports.iter().any(|report| !report.passed) {
            return Err(WorkflowError::TestsFailed(reports));
        }
        Ok(())
    }

    /// Refuses the secrets of a version that do not fit its action set and its code, or that
    /// its maker may not use: an organization's secret must exist and be theirs to use; an
    /// `ns/` secret is checked by its name only, since a version has no namespace
    /// (`archi/04-tools.md`, "Secrets": a workflow version is the permission).
    async fn check_version_secrets(
        &self,
        tenant: &TenantId,
        code: &Code,
        action_set: &ActionSet,
        secrets: &ServerSecrets,
        rights: &SecretRights,
    ) -> Result<(), WorkflowError> {
        if secrets.is_empty() && action_set.taking_secrets().next().is_none() {
            return Ok(());
        }
        let imports = self.plane.executor().action_imports(code);
        let check = SecretCheck {
            tenant,
            namespace: None,
            action_set,
            imports: imports.as_ref(),
            secrets,
            rights,
        };
        secrets::check(self.plane.secrets(), self.plane.clock().now(), check)
            .await?
            .map_err(WorkflowError::SecretRequired)
    }

    async fn action_set_of(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<StoredActionSet, WorkflowError> {
        self.plane
            .catalog()
            .get_action_set(tenant, digest)
            .await?
            .ok_or(WorkflowError::ActionSetNotFound(digest))
    }

    /// The code of a version, as stored.
    async fn code_of(
        &self,
        tenant: &TenantId,
        version: &StoredVersion,
    ) -> Result<Code, WorkflowError> {
        self.plane
            .catalog()
            .get_code(tenant, version.document.code())
            .await?
            .map(|stored| stored.code)
            .ok_or_else(|| {
                WorkflowError::Store(StoreError::Backend(format!(
                    "the code {} of the version {} of {} is not stored",
                    version.document.code(),
                    version.version,
                    version.workflow
                )))
            })
    }

    async fn reread(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
    ) -> Result<StoredWorkflow, WorkflowError> {
        self.plane
            .catalog()
            .workflow(tenant, id)
            .await?
            .ok_or_else(|| WorkflowError::NotFound(format!("no workflow {id}")))
    }

    /// The workflow of `scope` itself (not the organization's for a namespace) that has `code`
    /// under labels carrying `labels`, and its latest version with it.
    async fn scope_has_code(
        &self,
        tenant: &TenantId,
        scope: &WorkflowScope,
        code: Digest,
        labels: &Labels,
    ) -> Result<Option<WrittenWorkflow>, WorkflowError> {
        let catalog = self.plane.catalog();
        let found = match scope {
            WorkflowScope::Namespace(namespace) => catalog
                .workflow_with_code(tenant, namespace, code, labels)
                .await?
                .filter(|(workflow, _)| workflow.scope == *scope),
            WorkflowScope::Organization => {
                let filter = WorkflowFilter {
                    tenant: tenant.clone(),
                    scope: ScopeFilter::Organization,
                    code: Some(code),
                    labels: labels.clone(),
                    after: None,
                    limit: 1,
                };
                match catalog.list_workflows(&filter).await?.into_iter().next() {
                    Some(workflow) => {
                        let version = self
                            .latest_with_code(tenant, &workflow, code, labels)
                            .await?;
                        Some((workflow, version))
                    }
                    None => None,
                }
            }
        };
        Ok(found.map(|(workflow, version)| WrittenWorkflow {
            workflow,
            version,
            created: false,
        }))
    }

    /// The latest version of a workflow with `code` carrying `labels`, else its latest.
    async fn latest_with_code(
        &self,
        tenant: &TenantId,
        workflow: &StoredWorkflow,
        code: Digest,
        labels: &Labels,
    ) -> Result<StoredVersion, WorkflowError> {
        let versions = self
            .plane
            .catalog()
            .versions(tenant, &workflow.id, labels)
            .await?;
        let found = versions
            .iter()
            .find(|version| version.document.code() == code)
            .or_else(|| versions.first())
            .cloned();
        found.ok_or_else(|| {
            WorkflowError::NotFound(format!(
                "the workflow {} has no version this caller sees",
                workflow.reference()
            ))
        })
    }

    /// A creation by name sent again: the workflow of that name in `scope` whose latest
    /// version is the one this creation would make, with the same tests.
    async fn made_before<K, L>(
        &self,
        tenant: &TenantId,
        scope: &WorkflowScope,
        name: &str,
        candidate: &WorkflowVersion,
        tests: &[crate::suites::TestInput],
        kit: &WorkflowKit<'_, K, L>,
    ) -> Result<Option<WrittenWorkflow>, WorkflowError> {
        let catalog = self.plane.catalog();
        let named = catalog
            .workflow_named(tenant, scope.namespace(), name, candidate.labels())
            .await?
            .filter(|workflow| workflow.scope == *scope);
        let Some(workflow) = named else {
            return Ok(None);
        };
        let Some(latest) = catalog
            .versions(tenant, &workflow.id, candidate.labels())
            .await?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        if workflow.latest != 1 || !latest.document.same_content(candidate) {
            return Ok(None);
        }
        let Ok(composed) = self
            .suites(kit.tests.format)
            .compose_suite(
                tenant,
                None,
                &[],
                &SuiteChanges {
                    tests: tests.to_vec(),
                    ..SuiteChanges::default()
                },
            )
            .await
        else {
            return Ok(None);
        };
        Ok(
            (composed.suite.tests == latest.document.suite().tests).then_some(WrittenWorkflow {
                workflow,
                version: latest,
                created: false,
            }),
        )
    }

    /// Whether `base` is what these changes made of the version before it: a version sent
    /// again (a retry whose answer was lost) answers the version it made, instead of applying
    /// its changes a second time on top of it.
    async fn made_by<K, L>(
        &self,
        tenant: &TenantId,
        base: &StoredVersion,
        changes: &SuiteChanges,
        kit: &WorkflowKit<'_, K, L>,
    ) -> Result<bool, WorkflowError> {
        let suite = base.document.suite();
        let before = match suite.base {
            Some(digest) => match self
                .plane
                .catalog()
                .version(tenant, &base.workflow, VersionRef::Digest(digest))
                .await?
            {
                Some(version) => version.document.suite().tests.clone(),
                None => return Ok(false),
            },
            None => Vec::new(),
        };
        // A change that does not apply to the suite before is reported by the version itself.
        let Ok(composed) = self
            .suites(kit.tests.format)
            .compose_suite(tenant, suite.base, &before, changes)
            .await
        else {
            return Ok(false);
        };
        Ok(composed.suite.tests == suite.tests)
    }
}

/// A version checked: its action set, and its code compiled.
struct Checked {
    action_set: ActionSet,
    compiled: CompiledCode,
}

/// Checks a workflow's name and description.
fn check_texts(name: Option<&str>, description: Option<&str>) -> Result<(), WorkflowError> {
    if let Some(name) = name {
        text::refuse_nul("the workflow's name", name)?;
        check_workflow_name(name)?;
    }
    if let Some(description) = description {
        text::refuse_nul("the workflow's description", description)?;
        let chars = description.chars().count();
        if chars > MAX_DESCRIPTION_CHARS {
            return Err(WorkflowError::DescriptionTooLong(chars));
        }
    }
    Ok(())
}

fn name_taken(name: &str, scope: &WorkflowScope) -> WorkflowError {
    let scope = match scope {
        WorkflowScope::Organization => "the organization".to_owned(),
        WorkflowScope::Namespace(namespace) => format!("the namespace {namespace}"),
    };
    WorkflowError::NameTaken(format!(
        "{scope} already has a workflow named {name:?}: a name is unique in its scope; give \
         another name, or change that workflow"
    ))
}

/// Whether a test was recorded from another namespace's runs than `namespace`: a fork does not
/// copy it, since it holds that end user's data.
fn recorded_elsewhere(record: Option<&TestRecord>, namespace: &NamespaceName) -> bool {
    record
        .and_then(|record| record.namespace.as_ref())
        .is_some_and(|recorded| recorded != namespace)
}

/// The suite of a new version, with what left the suite since the latest version: the
/// retirements and replacements made by changes of tests alone, with their reasons, and the
/// tests of an erased namespace, which left with it (`archi/05-testing-and-observability.md`,
/// "Suites and the gate").
fn with_changes_since(mut suite: Suite, members: &[SuiteMember], latest: &StoredVersion) -> Suite {
    for passed in &latest.document.suite().tests {
        let kept = suite
            .tests
            .iter()
            .any(|test| test.name == passed.name && test.test == passed.test);
        let listed = suite
            .retired
            .iter()
            .any(|retired| retired.name == passed.name)
            || suite
                .replaced
                .iter()
                .any(|replaced| replaced.name == passed.name);
        if kept || listed {
            continue;
        }
        let member = members
            .iter()
            .find(|member| member.name == passed.name && member.digest == passed.test);
        let reason = match member {
            None => ERASED_TEST_REASON.to_owned(),
            Some(member) => member
                .reason
                .clone()
                .unwrap_or_else(|| "retired".to_owned()),
        };
        match suite.tests.iter().find(|test| test.name == passed.name) {
            Some(new) => suite.replaced.push(ReplacedTest {
                name: passed.name.clone(),
                old: passed.test,
                new: new.test,
                reason,
            }),
            None => suite.retired.push(RetiredTest {
                name: passed.name.clone(),
                test: passed.test,
                reason,
            }),
        }
    }
    suite
}
