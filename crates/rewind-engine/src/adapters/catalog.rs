use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::sync::{Mutex, MutexGuard};

use rewind_core::{ActionSet, Code, Digest, PartitionId, RunId, Timestamp, WorkflowId};

use crate::adapters::ControlPlaneChanges;
use crate::ports::catalog::{NamespaceFilter, NamespacePage};
use crate::ports::{
    ActionDeclarations, CatalogError, CatalogMigration, CatalogStore, CodeTitle, CompiledCode,
    ControlPlaneChange, DeleteNamespace, DisplayName, DropNamespace, Labels, Namespace,
    NamespaceName, NamespaceWrite, NewVersion, NewWorkflow, Quotas, RunRef, StoredActionSet,
    StoredCode, StoredVersion, StoredWorkflow, Subscription, SuiteChange, SuiteMember,
    TEST_DELETION_TIMEOUT, TEST_GRACE, Tenant, TenantId, TestRecord, VersionRef, WorkflowCreated,
    WorkflowFilter, WorkflowPatch, WorkflowScope, WorkflowWrite, carries, namespace_partition,
};

/// `CatalogStore` in memory: engine tests and test runs. Every write sends its change through
/// the [`ControlPlaneChanges`] it was given, under the lock of the write, so the changes leave
/// in commit order. It passes the conformance suite: it is the reference implementation of the
/// contract.
#[derive(Debug, Default)]
pub struct MemoryCatalog {
    state: Mutex<State>,
    changes: ControlPlaneChanges,
}

#[derive(Debug, Default)]
struct State {
    code: BTreeMap<(TenantId, Digest), StoredCode>,
    action_sets: BTreeMap<(TenantId, Digest), StoredActionSet>,
    workflows: BTreeMap<(TenantId, WorkflowId), StoredWorkflow>,
    versions: BTreeMap<(TenantId, WorkflowId, u32), StoredVersion>,
    /// `workflow_codes`: each code a scope has seen under a set of labels, the workflow that
    /// first had it there, and the order the rows were written in.
    codes: BTreeMap<(TenantId, WorkflowScope, Labels, Digest), (u64, WorkflowId)>,
    next_code: u64,
    /// `workflow_tests`: every test each workflow's suite holds or held.
    suites: BTreeMap<(TenantId, WorkflowId), Vec<SuiteMember>>,
    /// The titles people gave codes before the rename release, for `CatalogMigration`.
    code_titles: Vec<CodeTitle>,
    /// The index of the tests whose documents are blobs.
    tests: BTreeMap<(TenantId, Digest), StoredTest>,
    subscriptions: BTreeMap<(TenantId, String), Subscription>,
    tenants: BTreeMap<TenantId, Tenant>,
    namespaces: BTreeMap<(TenantId, NamespaceName), Namespace>,
    /// `legacy_runs`: where the runs `migrate-runs` moved live.
    legacy: BTreeMap<RunId, PartitionId>,
}

#[derive(Debug, Clone)]
struct StoredTest {
    record: TestRecord,
    /// When it was stored or reserved last: its grace starts there.
    reserved_at: Timestamp,
    deletion_started: Option<Timestamp>,
}

impl State {
    fn test_referenced(&self, tenant: &TenantId, digest: Digest) -> bool {
        self.suites.iter().any(|((owner, _), members)| {
            owner == tenant && members.iter().any(|member| member.digest == digest)
        })
    }

    /// A test without a row, or whose deletion has started, among `digests`.
    fn missing_test(
        &self,
        tenant: &TenantId,
        digests: impl IntoIterator<Item = Digest>,
    ) -> Option<Digest> {
        digests.into_iter().find(|digest| {
            !self
                .tests
                .get(&(tenant.clone(), *digest))
                .is_some_and(|stored| stored.deletion_started.is_none())
        })
    }

    /// Whether a caller limited to `labels` sees the workflow: one of its versions carries them.
    fn visible(&self, tenant: &TenantId, id: &WorkflowId, labels: &Labels) -> bool {
        self.versions_of(tenant, id)
            .any(|version| version.carries(labels))
    }

    fn versions_of<'a>(
        &'a self,
        tenant: &'a TenantId,
        id: &'a WorkflowId,
    ) -> impl Iterator<Item = &'a StoredVersion> + 'a {
        self.versions
            .range((tenant.clone(), id.clone(), 0)..=(tenant.clone(), id.clone(), u32::MAX))
            .map(|(_, version)| version)
    }

    /// The workflow of `scope` named `name`, if any.
    fn named(
        &self,
        tenant: &TenantId,
        scope: &WorkflowScope,
        name: &str,
    ) -> Option<&StoredWorkflow> {
        self.workflows
            .iter()
            .find(|((owner, _), workflow)| {
                owner == tenant
                    && workflow.scope == *scope
                    && workflow.name.as_deref() == Some(name)
            })
            .map(|(_, workflow)| workflow)
    }

    /// The workflow that first had `code` in `scope` under labels carrying `labels`.
    fn with_code(
        &self,
        tenant: &TenantId,
        scope: &WorkflowScope,
        code: Digest,
        labels: &Labels,
    ) -> Option<(StoredWorkflow, StoredVersion)> {
        let mut rows: Vec<&(u64, WorkflowId)> = self
            .codes
            .iter()
            .filter(|((owner, of, held, digest), _)| {
                owner == tenant && of == scope && *digest == code && carries(held, labels)
            })
            .map(|(_, row)| row)
            .collect();
        rows.sort();
        rows.into_iter().find_map(|(_, id)| {
            let workflow = self.workflows.get(&(tenant.clone(), id.clone()))?;
            let version = self
                .versions_of(tenant, id)
                .filter(|version| version.document.code() == code && version.carries(labels))
                .max_by_key(|version| version.version)?;
            Some((workflow.clone(), version.clone()))
        })
    }

    /// Adds `code` to `workflow_codes` for `scope` under `labels`, unless the scope has it.
    fn add_code(
        &mut self,
        tenant: &TenantId,
        scope: &WorkflowScope,
        labels: &Labels,
        code: Digest,
        id: &WorkflowId,
    ) {
        let key = (tenant.clone(), scope.clone(), labels.clone(), code);
        if !self.codes.contains_key(&key) {
            let order = self.next_code;
            self.next_code += 1;
            self.codes.insert(key, (order, id.clone()));
        }
    }

    /// Moves the codes of workflow `id` from its scope `from` to `to`; a code `to` has already
    /// under the same labels keeps its workflow.
    fn move_codes(
        &mut self,
        tenant: &TenantId,
        id: &WorkflowId,
        from: &WorkflowScope,
        to: &WorkflowScope,
    ) {
        let moved: Vec<_> = self
            .codes
            .iter()
            .filter(|((owner, scope, _, _), (_, of))| owner == tenant && scope == from && of == id)
            .map(|(key, row)| (key.clone(), row.clone()))
            .collect();
        for ((owner, _, labels, code), row) in moved {
            self.codes
                .remove(&(owner.clone(), from.clone(), labels.clone(), code));
            self.codes
                .entry((owner, to.clone(), labels, code))
                .or_insert(row);
        }
    }

    /// The namespace, created with its partition if it does not exist; whether it was created.
    fn ensure(
        &mut self,
        tenant: &TenantId,
        name: &NamespaceName,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> (&mut Namespace, bool) {
        let key = (tenant.clone(), name.clone());
        let created = !self.namespaces.contains_key(&key);
        let namespace = self.namespaces.entry(key).or_insert_with(|| {
            Namespace::new(
                tenant.clone(),
                name.clone(),
                namespace_partition(tenant, name, partitions),
                now,
            )
        });
        (namespace, created)
    }
}

impl MemoryCatalog {
    /// A catalog whose changes reach nobody.
    pub fn new() -> Self {
        Self::default()
    }

    /// A catalog that sends its changes through `changes`: a clone of a
    /// [`MemoryCluster`](crate::adapters::MemoryCluster)'s.
    pub fn with_changes(changes: ControlPlaneChanges) -> Self {
        Self {
            state: Mutex::default(),
            changes,
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>, CatalogError> {
        self.state
            .lock()
            .map_err(|_| CatalogError::Backend("the memory catalog's lock is poisoned".into()))
    }

    fn send(&self, change: ControlPlaneChange) {
        self.changes.send(change);
    }

    fn namespace_changed(&self, tenant: &TenantId, name: &NamespaceName) {
        self.send(ControlPlaneChange::Namespace {
            tenant: tenant.clone(),
            name: name.clone(),
        });
    }

    fn workflow_changed(&self, tenant: &TenantId, id: &WorkflowId) {
        self.send(ControlPlaneChange::Workflow {
            tenant: tenant.clone(),
            id: id.clone(),
        });
    }

    /// Keeps a title a person gave a code before the rename release, as the schema migration
    /// of the workflow model leaves them: what tests of `CatalogMigration` start from.
    pub fn put_code_title(&self, title: CodeTitle) -> Result<(), CatalogError> {
        self.lock()?.code_titles.push(title);
        Ok(())
    }

    fn test_changed(&self, tenant: &TenantId, digest: Digest) {
        self.send(ControlPlaneChange::Test {
            tenant: tenant.clone(),
            digest,
        });
    }
}

fn missing_test(digest: Digest) -> CatalogError {
    CatalogError::Conflict(format!(
        "the test {digest} has no row, or its deletion has started: reserve it and write its \
         blob again before giving it to a suite"
    ))
}

fn no_workflow(id: &WorkflowId) -> CatalogError {
    CatalogError::Conflict(format!("the workflow {id} does not exist"))
}

/// The version a workflow's new version is stored as.
fn stored_version(
    id: &WorkflowId,
    number: u32,
    document: &rewind_core::WorkflowVersion,
    now: Timestamp,
) -> StoredVersion {
    StoredVersion {
        workflow: id.clone(),
        version: number,
        digest: document.digest(),
        labels: document.labels().clone(),
        document: document.clone(),
        created_at: now,
    }
}

impl CatalogStore for MemoryCatalog {
    async fn put_code(
        &self,
        tenant: &TenantId,
        code: &Code,
        compiled: &CompiledCode,
    ) -> Result<(), CatalogError> {
        let mut state = self.lock()?;
        state
            .code
            .entry((tenant.clone(), code.digest()))
            .or_insert_with(|| StoredCode {
                code: code.clone(),
                compiled: compiled.clone(),
            });
        Ok(())
    }

    async fn get_code(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredCode>, CatalogError> {
        Ok(self.lock()?.code.get(&(tenant.clone(), digest)).cloned())
    }

    async fn put_action_set(
        &self,
        tenant: &TenantId,
        action_set: &ActionSet,
        declarations: &ActionDeclarations,
    ) -> Result<(), CatalogError> {
        let mut state = self.lock()?;
        state
            .action_sets
            .entry((tenant.clone(), action_set.digest()))
            .or_insert_with(|| StoredActionSet {
                action_set: action_set.clone(),
                declarations: declarations.clone(),
            });
        self.send(ControlPlaneChange::ActionSet {
            tenant: tenant.clone(),
            digest: action_set.digest(),
        });
        Ok(())
    }

    async fn get_action_set(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredActionSet>, CatalogError> {
        Ok(self
            .lock()?
            .action_sets
            .get(&(tenant.clone(), digest))
            .cloned())
    }

    async fn list_action_sets(
        &self,
        tenant: &TenantId,
    ) -> Result<Vec<StoredActionSet>, CatalogError> {
        Ok(self
            .lock()?
            .action_sets
            .iter()
            .filter(|((owner, _), _)| owner == tenant)
            .map(|(_, stored)| stored.clone())
            .collect())
    }

    async fn create_workflow(
        &self,
        tenant: &TenantId,
        workflow: &NewWorkflow,
        now: Timestamp,
    ) -> Result<WorkflowCreated, CatalogError> {
        let mut state = self.lock()?;
        let version = &workflow.version;
        match &workflow.name {
            Some(name) => {
                if state.named(tenant, &workflow.scope, name).is_some() {
                    return Ok(WorkflowCreated::NameTaken);
                }
            }
            None => {
                if let Some((existing, _)) =
                    state.with_code(tenant, &workflow.scope, version.code(), version.labels())
                {
                    return Ok(WorkflowCreated::Existing(existing));
                }
            }
        }
        if let Some(digest) =
            state.missing_test(tenant, version.suite().tests.iter().map(|test| test.test))
        {
            return Err(missing_test(digest));
        }
        if state
            .workflows
            .contains_key(&(tenant.clone(), workflow.id.clone()))
        {
            return Err(CatalogError::Conflict(format!(
                "the workflow {} exists already",
                workflow.id
            )));
        }
        let stored = StoredWorkflow {
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
        };
        state
            .workflows
            .insert((tenant.clone(), workflow.id.clone()), stored.clone());
        state.versions.insert(
            (tenant.clone(), workflow.id.clone(), 1),
            stored_version(&workflow.id, 1, version, now),
        );
        state.add_code(
            tenant,
            &workflow.scope,
            version.labels(),
            version.code(),
            &workflow.id,
        );
        let members = version
            .suite()
            .tests
            .iter()
            .map(|test| SuiteMember {
                name: test.name.clone(),
                digest: test.test,
                added_at: now,
                retired_at: None,
                reason: None,
            })
            .collect();
        state
            .suites
            .insert((tenant.clone(), workflow.id.clone()), members);
        self.workflow_changed(tenant, &workflow.id);
        Ok(WorkflowCreated::Created(stored))
    }

    async fn workflow(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        Ok(self
            .lock()?
            .workflows
            .get(&(tenant.clone(), id.clone()))
            .cloned())
    }

    async fn workflow_named(
        &self,
        tenant: &TenantId,
        namespace: Option<&NamespaceName>,
        name: &str,
        labels: &Labels,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        let state = self.lock()?;
        let scopes = namespace
            .map(|namespace| WorkflowScope::Namespace(namespace.clone()))
            .into_iter()
            .chain([WorkflowScope::Organization]);
        for scope in scopes {
            if let Some(workflow) = state.named(tenant, &scope, name)
                && state.visible(tenant, &workflow.id, labels)
            {
                return Ok(Some(workflow.clone()));
            }
        }
        Ok(None)
    }

    async fn workflow_with_code(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        code: Digest,
        labels: &Labels,
    ) -> Result<Option<(StoredWorkflow, StoredVersion)>, CatalogError> {
        let state = self.lock()?;
        Ok(state
            .with_code(
                tenant,
                &WorkflowScope::Namespace(namespace.clone()),
                code,
                labels,
            )
            .or_else(|| state.with_code(tenant, &WorkflowScope::Organization, code, labels)))
    }

    async fn list_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        let state = self.lock()?;
        let tenant = &filter.tenant;
        Ok(state
            .workflows
            .iter()
            .filter(|((owner, id), workflow)| {
                owner == tenant
                    && filter.after.as_ref().is_none_or(|after| id > after)
                    && filter.scope.holds(&workflow.scope)
                    && state.versions_of(tenant, id).any(|version| {
                        version.carries(&filter.labels)
                            && filter
                                .code
                                .is_none_or(|code| version.document.code() == code)
                    })
            })
            .map(|(_, workflow)| workflow.clone())
            .take(filter.limit)
            .collect())
    }

    async fn update_workflow(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        patch: &WorkflowPatch,
        _now: Timestamp,
    ) -> Result<WorkflowWrite, CatalogError> {
        let mut state = self.lock()?;
        let Some(workflow) = state.workflows.get(&(tenant.clone(), id.clone())).cloned() else {
            return Ok(WorkflowWrite::NotFound);
        };
        if let Some(Some(name)) = &patch.name
            && state
                .named(tenant, &workflow.scope, name)
                .is_some_and(|other| other.id != *id)
        {
            return Ok(WorkflowWrite::NameTaken);
        }
        let Some(stored) = state.workflows.get_mut(&(tenant.clone(), id.clone())) else {
            return Ok(WorkflowWrite::NotFound);
        };
        if let Some(name) = &patch.name {
            stored.name.clone_from(name);
        }
        if let Some(description) = &patch.description {
            stored.description.clone_from(description);
        }
        let stored = stored.clone();
        self.workflow_changed(tenant, id);
        Ok(WorkflowWrite::Written(stored))
    }

    async fn put_version(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        version: &NewVersion,
        now: Timestamp,
    ) -> Result<StoredVersion, CatalogError> {
        let mut state = self.lock()?;
        let Some(workflow) = state.workflows.get(&(tenant.clone(), id.clone())).cloned() else {
            return Err(no_workflow(id));
        };
        let document = &version.document;
        if let Some(digest) =
            state.missing_test(tenant, document.suite().tests.iter().map(|test| test.test))
        {
            return Err(missing_test(digest));
        }
        let number = workflow.latest + 1;
        let stored = stored_version(id, number, document, now);
        state
            .versions
            .insert((tenant.clone(), id.clone(), number), stored.clone());
        state.add_code(
            tenant,
            &workflow.scope,
            document.labels(),
            document.code(),
            id,
        );
        let suite = document.suite();
        let reason = |name: &str| {
            suite
                .retired
                .iter()
                .find(|retired| retired.name == name)
                .map(|retired| retired.reason.clone())
                .or_else(|| {
                    suite
                        .replaced
                        .iter()
                        .find(|replaced| replaced.name == name)
                        .map(|replaced| replaced.reason.clone())
                })
        };
        let members = state
            .suites
            .entry((tenant.clone(), id.clone()))
            .or_default();
        for member in members.iter_mut().filter(|member| member.is_current()) {
            if suite.test(&member.name) != Some(member.digest) {
                member.retired_at = Some(now);
                member.reason = reason(&member.name);
            }
        }
        for test in &suite.tests {
            join(members, &test.name, test.test, now);
        }
        if let Some(stored) = state.workflows.get_mut(&(tenant.clone(), id.clone())) {
            stored.latest = number;
            if let (Some(pulled), Some(source)) = (version.pulled, stored.source.as_mut()) {
                source.version = pulled;
            }
        }
        self.workflow_changed(tenant, id);
        Ok(stored)
    }

    async fn versions(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        labels: &Labels,
    ) -> Result<Vec<StoredVersion>, CatalogError> {
        let state = self.lock()?;
        let mut versions: Vec<StoredVersion> = state
            .versions_of(tenant, id)
            .filter(|version| version.carries(labels))
            .cloned()
            .collect();
        versions.reverse();
        Ok(versions)
    }

    async fn version(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        version: VersionRef,
    ) -> Result<Option<StoredVersion>, CatalogError> {
        let state = self.lock()?;
        Ok(match version {
            VersionRef::Number(number) => state
                .versions
                .get(&(tenant.clone(), id.clone(), number))
                .cloned(),
            VersionRef::Digest(digest) => state
                .versions_of(tenant, id)
                .filter(|stored| stored.digest == digest)
                .max_by_key(|stored| stored.version)
                .cloned(),
        })
    }

    async fn change_suite(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        change: &SuiteChange,
        now: Timestamp,
    ) -> Result<Vec<SuiteMember>, CatalogError> {
        let mut state = self.lock()?;
        if !state.workflows.contains_key(&(tenant.clone(), id.clone())) {
            return Err(no_workflow(id));
        }
        let joining = change
            .added
            .iter()
            .map(|test| test.test)
            .chain(change.replaced.iter().map(|replaced| replaced.new));
        if let Some(digest) = state.missing_test(tenant, joining) {
            return Err(missing_test(digest));
        }
        let members = state
            .suites
            .entry((tenant.clone(), id.clone()))
            .or_default();
        let leaving = change
            .retired
            .iter()
            .map(|retired| (&retired.name, retired.test, &retired.reason))
            .chain(
                change
                    .replaced
                    .iter()
                    .map(|replaced| (&replaced.name, replaced.old, &replaced.reason)),
            );
        for (name, digest, _) in leaving.clone() {
            if !members.iter().any(|member| {
                member.is_current() && member.name == *name && member.digest == digest
            }) {
                return Err(CatalogError::Conflict(format!(
                    "the suite of {id} has no test {name:?} of digest {digest}"
                )));
            }
        }
        for (name, digest, reason) in leaving {
            for member in members.iter_mut().filter(|member| {
                member.is_current() && member.name == *name && member.digest == digest
            }) {
                member.retired_at = Some(now);
                member.reason = Some(reason.clone());
            }
        }
        for replaced in &change.replaced {
            join(members, &replaced.name, replaced.new, now);
        }
        for test in &change.added {
            join(members, &test.name, test.test, now);
        }
        let suite = sorted(members);
        self.workflow_changed(tenant, id);
        Ok(suite)
    }

    async fn suite(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
    ) -> Result<Vec<SuiteMember>, CatalogError> {
        Ok(self
            .lock()?
            .suites
            .get(&(tenant.clone(), id.clone()))
            .map(|members| sorted(members))
            .unwrap_or_default())
    }

    async fn promote_workflow(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        now: Timestamp,
    ) -> Result<WorkflowWrite, CatalogError> {
        let mut state = self.lock()?;
        let Some(workflow) = state.workflows.get(&(tenant.clone(), id.clone())).cloned() else {
            return Ok(WorkflowWrite::NotFound);
        };
        if workflow.scope == WorkflowScope::Organization {
            return Ok(WorkflowWrite::Written(workflow));
        }
        if let Some(name) = &workflow.name
            && state
                .named(tenant, &WorkflowScope::Organization, name)
                .is_some()
        {
            return Ok(WorkflowWrite::NameTaken);
        }
        state.move_codes(tenant, id, &workflow.scope, &WorkflowScope::Organization);
        let Some(stored) = state.workflows.get_mut(&(tenant.clone(), id.clone())) else {
            return Ok(WorkflowWrite::NotFound);
        };
        stored.scope = WorkflowScope::Organization;
        stored.promoted_at = Some(now);
        let stored = stored.clone();
        self.workflow_changed(tenant, id);
        Ok(WorkflowWrite::Written(stored))
    }

    async fn reserve_tests(
        &self,
        tenant: &TenantId,
        tests: &[TestRecord],
        now: Timestamp,
    ) -> Result<Vec<Digest>, CatalogError> {
        let mut state = self.lock()?;
        let mut deleting = Vec::new();
        for record in tests {
            match state.tests.get_mut(&(tenant.clone(), record.digest)) {
                Some(stored) if stored.deletion_started.is_some() => {
                    deleting.push(record.digest);
                    continue;
                }
                Some(stored) => stored.reserved_at = now,
                None => {
                    state.tests.insert(
                        (tenant.clone(), record.digest),
                        StoredTest {
                            record: record.clone(),
                            reserved_at: now,
                            deletion_started: None,
                        },
                    );
                }
            }
            self.test_changed(tenant, record.digest);
        }
        Ok(deleting)
    }

    async fn get_test(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<TestRecord>, CatalogError> {
        Ok(self
            .lock()?
            .tests
            .get(&(tenant.clone(), digest))
            .filter(|stored| stored.deletion_started.is_none())
            .map(|stored| stored.record.clone()))
    }

    async fn namespace_tests(
        &self,
        tenant: &TenantId,
        namespace: &NamespaceName,
        limit: usize,
    ) -> Result<Vec<Digest>, CatalogError> {
        Ok(self
            .lock()?
            .tests
            .iter()
            .filter(|((owner, _), stored)| {
                owner == tenant && stored.record.namespace.as_ref() == Some(namespace)
            })
            .map(|((_, digest), _)| *digest)
            .take(limit)
            .collect())
    }

    async fn unreferenced_tests(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<(TenantId, Digest)>, CatalogError> {
        let mut state = self.lock()?;
        let due: Vec<(TenantId, Digest)> = state
            .tests
            .iter()
            .filter(|((tenant, digest), stored)| {
                stored.reserved_at.saturating_add(TEST_GRACE) <= now
                    && stored
                        .deletion_started
                        .is_none_or(|started| started.saturating_add(TEST_DELETION_TIMEOUT) <= now)
                    && !state.test_referenced(tenant, *digest)
            })
            .map(|(key, _)| key.clone())
            .take(limit)
            .collect();
        for key in &due {
            if let Some(stored) = state.tests.get_mut(key) {
                stored.deletion_started = Some(now);
            }
            self.test_changed(&key.0, key.1);
        }
        Ok(due)
    }

    async fn drop_tests(
        &self,
        tests: &[(TenantId, Digest)],
        erased: bool,
    ) -> Result<usize, CatalogError> {
        let mut state = self.lock()?;
        let mut dropped = 0;
        for (tenant, digest) in tests {
            let key = (tenant.clone(), *digest);
            let droppable = erased
                || state.tests.get(&key).is_some_and(|stored| {
                    stored.deletion_started.is_some() && !state.test_referenced(tenant, *digest)
                });
            if !droppable {
                continue;
            }
            if state.tests.remove(&key).is_some() {
                dropped += 1;
                self.test_changed(tenant, *digest);
            }
            if erased {
                for ((owner, _), members) in &mut state.suites {
                    if owner == tenant {
                        members.retain(|member| member.digest != *digest);
                    }
                }
            }
        }
        Ok(dropped)
    }

    async fn put_subscription(&self, subscription: &Subscription) -> Result<(), CatalogError> {
        let mut state = self.lock()?;
        state.subscriptions.insert(
            (subscription.tenant.clone(), subscription.id.clone()),
            subscription.clone(),
        );
        self.send(ControlPlaneChange::Subscription {
            tenant: subscription.tenant.clone(),
            id: subscription.id.clone(),
        });
        Ok(())
    }

    async fn subscriptions(&self, tenant: &TenantId) -> Result<Vec<Subscription>, CatalogError> {
        Ok(self
            .lock()?
            .subscriptions
            .values()
            .filter(|subscription| subscription.tenant == *tenant)
            .cloned()
            .collect())
    }

    async fn delete_subscription(&self, tenant: &TenantId, id: &str) -> Result<bool, CatalogError> {
        let mut state = self.lock()?;
        let existed = state
            .subscriptions
            .remove(&(tenant.clone(), id.to_owned()))
            .is_some();
        if existed {
            self.send(ControlPlaneChange::Subscription {
                tenant: tenant.clone(),
                id: id.to_owned(),
            });
        }
        Ok(existed)
    }

    async fn put_tenant(&self, tenant: &Tenant) -> Result<(), CatalogError> {
        let mut state = self.lock()?;
        state.tenants.insert(tenant.id.clone(), tenant.clone());
        self.send(ControlPlaneChange::Tenant {
            tenant: tenant.id.clone(),
        });
        Ok(())
    }

    async fn get_tenant(&self, id: &TenantId) -> Result<Option<Tenant>, CatalogError> {
        Ok(self.lock()?.tenants.get(id).cloned())
    }

    async fn list_tenants(&self) -> Result<Vec<Tenant>, CatalogError> {
        Ok(self.lock()?.tenants.values().cloned().collect())
    }

    async fn ensure_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> Result<NamespaceWrite, CatalogError> {
        let mut state = self.lock()?;
        let (namespace, created) = state.ensure(tenant, name, partitions, now);
        let written = if namespace.deleting_since.is_some() {
            NamespaceWrite::Deleting
        } else {
            NamespaceWrite::Written(namespace.clone())
        };
        if created {
            self.namespace_changed(tenant, name);
        }
        Ok(written)
    }

    async fn put_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        exceptions: Option<&Quotas>,
        partitions: NonZeroU16,
        now: Timestamp,
    ) -> Result<NamespaceWrite, CatalogError> {
        let mut state = self.lock()?;
        let (namespace, created) = state.ensure(tenant, name, partitions, now);
        if namespace.deleting_since.is_some() {
            return Ok(NamespaceWrite::Deleting);
        }
        if let Some(exceptions) = exceptions {
            namespace.exceptions = exceptions.clone();
        }
        let written = NamespaceWrite::Written(namespace.clone());
        if created || exceptions.is_some() {
            self.namespace_changed(tenant, name);
        }
        Ok(written)
    }

    async fn get_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> Result<Option<Namespace>, CatalogError> {
        Ok(self
            .lock()?
            .namespaces
            .get(&(tenant.clone(), name.clone()))
            .cloned())
    }

    async fn set_namespace_display_name(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        display_name: Option<&DisplayName>,
    ) -> Result<Option<NamespaceWrite>, CatalogError> {
        let mut state = self.lock()?;
        let Some(namespace) = state.namespaces.get_mut(&(tenant.clone(), name.clone())) else {
            return Ok(None);
        };
        if namespace.deleting_since.is_some() {
            return Ok(Some(NamespaceWrite::Deleting));
        }
        namespace.display_name = display_name.cloned();
        let written = NamespaceWrite::Written(namespace.clone());
        self.namespace_changed(tenant, name);
        Ok(Some(written))
    }

    async fn namespace_display_names(
        &self,
        tenant: &TenantId,
    ) -> Result<Vec<(NamespaceName, DisplayName)>, CatalogError> {
        Ok(self
            .lock()?
            .namespaces
            .iter()
            .filter(|((of, _), _)| of == tenant)
            .filter_map(|((_, name), namespace)| {
                Some((name.clone(), namespace.display_name.clone()?))
            })
            .collect())
    }

    async fn list_namespaces(
        &self,
        filter: &NamespaceFilter,
    ) -> Result<NamespacePage, CatalogError> {
        let state = self.lock()?;
        let matching: Vec<&Namespace> = state
            .namespaces
            .iter()
            .filter(|((tenant, name), namespace)| {
                *tenant == filter.tenant
                    && filter.name.as_ref().is_none_or(|only| only == name)
                    && filter.prefix.as_deref().is_none_or(|prefix| {
                        name.as_str().starts_with(prefix)
                            || namespace
                                .display_name
                                .as_ref()
                                .is_some_and(|display| display.starts_with(prefix))
                    })
            })
            .map(|(_, namespace)| namespace)
            .collect();
        let total = u64::try_from(matching.len()).unwrap_or(u64::MAX);
        let namespaces = matching
            .into_iter()
            .filter(|namespace| {
                filter
                    .after
                    .as_ref()
                    .is_none_or(|after| namespace.name > *after)
            })
            .take(filter.limit)
            .cloned()
            .collect();
        Ok(NamespacePage { namespaces, total })
    }

    async fn revoke_namespace_tokens(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<bool, CatalogError> {
        let mut state = self.lock()?;
        let Some(namespace) = state.namespaces.get_mut(&(tenant.clone(), name.clone())) else {
            return Ok(false);
        };
        namespace.tokens_revoked_before = Some(now);
        self.namespace_changed(tenant, name);
        Ok(true)
    }

    async fn delete_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
        now: Timestamp,
    ) -> Result<DeleteNamespace, CatalogError> {
        let mut state = self.lock()?;
        let Some(namespace) = state.namespaces.get_mut(&(tenant.clone(), name.clone())) else {
            return Ok(DeleteNamespace::NotFound);
        };
        namespace.deleting_since.get_or_insert(now);
        self.namespace_changed(tenant, name);
        Ok(DeleteNamespace::Started)
    }

    async fn namespaces_to_erase(
        &self,
        limit: usize,
    ) -> Result<Vec<(TenantId, NamespaceName)>, CatalogError> {
        let state = self.lock()?;
        let mut marked: Vec<(Timestamp, TenantId, NamespaceName)> = state
            .namespaces
            .values()
            .filter_map(|namespace| {
                namespace
                    .deleting_since
                    .map(|since| (since, namespace.tenant.clone(), namespace.name.clone()))
            })
            .collect();
        marked.sort();
        Ok(marked
            .into_iter()
            .take(limit)
            .map(|(_, tenant, name)| (tenant, name))
            .collect())
    }

    async fn drop_namespace(
        &self,
        tenant: &TenantId,
        name: &NamespaceName,
    ) -> Result<DropNamespace, CatalogError> {
        let mut state = self.lock()?;
        let key = (tenant.clone(), name.clone());
        if !state
            .namespaces
            .get(&key)
            .is_some_and(|namespace| namespace.deleting_since.is_some())
        {
            return Ok(DropNamespace::NotFound);
        }
        state.namespaces.remove(&key);
        let scope = WorkflowScope::Namespace(name.clone());
        let dropped: Vec<WorkflowId> = state
            .workflows
            .iter()
            .filter(|((owner, _), workflow)| owner == tenant && workflow.scope == scope)
            .map(|((_, id), _)| id.clone())
            .collect();
        for id in &dropped {
            state.workflows.remove(&(tenant.clone(), id.clone()));
            state
                .versions
                .retain(|(owner, of, _), _| !(owner == tenant && of == id));
            state.suites.remove(&(tenant.clone(), id.clone()));
        }
        state
            .codes
            .retain(|(owner, of, _, _), _| !(owner == tenant && *of == scope));
        self.namespace_changed(tenant, name);
        for id in &dropped {
            self.workflow_changed(tenant, id);
        }
        Ok(DropNamespace::Dropped)
    }

    async fn legacy_partition(&self, run: RunId) -> Result<Option<PartitionId>, CatalogError> {
        Ok(self.lock()?.legacy.get(&run).copied())
    }

    async fn put_legacy_run(
        &self,
        run: &RunRef,
        partition: PartitionId,
    ) -> Result<(), CatalogError> {
        self.lock()?.legacy.insert(run.id, partition);
        Ok(())
    }
}

/// Makes `digest` the test `name` of a suite: a member that left under that name and digest
/// joins again; one already in it stays.
fn join(members: &mut Vec<SuiteMember>, name: &str, digest: Digest, now: Timestamp) {
    if members
        .iter()
        .any(|member| member.is_current() && member.name == name && member.digest == digest)
    {
        return;
    }
    if let Some(member) = members
        .iter_mut()
        .find(|member| member.name == name && member.digest == digest)
    {
        member.added_at = now;
        member.retired_at = None;
        member.reason = None;
        return;
    }
    members.push(SuiteMember {
        name: name.to_owned(),
        digest,
        added_at: now,
        retired_at: None,
        reason: None,
    });
}

/// A suite's members by name, then when they joined.
fn sorted(members: &[SuiteMember]) -> Vec<SuiteMember> {
    let mut members = members.to_vec();
    members.sort_by(|a, b| (&a.name, a.added_at, a.digest).cmp(&(&b.name, b.added_at, b.digest)));
    members
}

impl CatalogMigration for MemoryCatalog {
    async fn code_titles(&self) -> Result<Vec<CodeTitle>, CatalogError> {
        Ok(self.lock()?.code_titles.clone())
    }

    async fn scope_workflow(
        &self,
        tenant: &TenantId,
        id: &WorkflowId,
        namespace: &NamespaceName,
        _now: Timestamp,
    ) -> Result<WorkflowWrite, CatalogError> {
        let mut state = self.lock()?;
        let Some(workflow) = state.workflows.get(&(tenant.clone(), id.clone())).cloned() else {
            return Ok(WorkflowWrite::NotFound);
        };
        let scope = WorkflowScope::Namespace(namespace.clone());
        if workflow.scope == scope {
            return Ok(WorkflowWrite::Written(workflow));
        }
        if let Some(name) = &workflow.name
            && state.named(tenant, &scope, name).is_some()
        {
            return Ok(WorkflowWrite::NameTaken);
        }
        state.move_codes(tenant, id, &workflow.scope, &scope);
        let Some(stored) = state.workflows.get_mut(&(tenant.clone(), id.clone())) else {
            return Ok(WorkflowWrite::NotFound);
        };
        stored.scope = scope;
        let stored = stored.clone();
        self.workflow_changed(tenant, id);
        Ok(WorkflowWrite::Written(stored))
    }

    async fn drop_code_titles(&self) -> Result<(), CatalogError> {
        self.lock()?.code_titles.clear();
        Ok(())
    }
}

/// The memory catalog holds nothing written before the rename release: it has no document to
/// rename.
impl crate::ports::CatalogDocuments for MemoryCatalog {
    async fn raw_action_sets(&self) -> Result<Vec<crate::ports::RawDocument>, CatalogError> {
        Ok(Vec::new())
    }

    async fn replace_action_set(
        &self,
        tenant: &TenantId,
        _old: Digest,
        action_set: &rewind_core::ActionSet,
        declarations: &crate::ports::ActionDeclarations,
    ) -> Result<(), CatalogError> {
        self.put_action_set(tenant, action_set, declarations).await
    }

    async fn raw_versions(&self) -> Result<Vec<crate::ports::RawVersion>, CatalogError> {
        Ok(Vec::new())
    }

    async fn replace_version(
        &self,
        _tenant: &TenantId,
        _workflow: &WorkflowId,
        _version: u64,
        _document: &rewind_core::WorkflowVersion,
    ) -> Result<(), CatalogError> {
        Ok(())
    }

    async fn raw_tests(&self) -> Result<Vec<(TenantId, Digest)>, CatalogError> {
        Ok(Vec::new())
    }

    async fn replace_test(
        &self,
        _tenant: &TenantId,
        _old: Digest,
        _new: Digest,
        _size: u64,
    ) -> Result<(), CatalogError> {
        Ok(())
    }
}
