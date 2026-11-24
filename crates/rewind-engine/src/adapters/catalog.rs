use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;
use std::sync::{Mutex, MutexGuard};

use rewind_core::{ActionSet, Digest, PartitionId, RunId, Script, Timestamp, Workflow};

use crate::adapters::ControlPlaneChanges;
use crate::ports::catalog::{NamespaceFilter, NamespacePage};
use crate::ports::{
    CatalogError, CatalogStore, CompiledScript, ControlPlaneChange, DeleteNamespace, DisplayName,
    DropNamespace, Namespace, NamespaceName, NamespaceWrite, Quotas, RunRef, ScriptTitle,
    StoredActionSet, StoredScript, StoredWorkflow, Subscription, TEST_DELETION_TIMEOUT, TEST_GRACE,
    Tenant, TenantId, TestRecord, ToolDeclarations, WorkflowFilter, namespace_partition,
};

/// `CatalogStore` in memory: engine tests and mock mode. Every write sends its change through
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
    scripts: BTreeMap<(TenantId, Digest), StoredScript>,
    /// The titles people gave scripts.
    titles: BTreeMap<(TenantId, Digest), ScriptTitle>,
    action_sets: BTreeMap<(TenantId, Digest), StoredActionSet>,
    /// Workflow versions, with the order they were stored in.
    workflows: BTreeMap<(TenantId, Digest), (u64, StoredWorkflow)>,
    next_workflow: u64,
    /// The index of the tests whose documents are blobs.
    tests: BTreeMap<(TenantId, Digest), StoredTest>,
    /// (tenant, test, version): the tests of each version's suite.
    workflow_tests: BTreeSet<(TenantId, Digest, Digest)>,
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
        self.workflow_tests
            .range((tenant.clone(), digest, Digest::from_bytes([0; 32]))..)
            .next()
            .is_some_and(|(owner, test, _)| owner == tenant && *test == digest)
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

    fn test_changed(&self, tenant: &TenantId, digest: Digest) {
        self.send(ControlPlaneChange::Test {
            tenant: tenant.clone(),
            digest,
        });
    }
}

/// Versions newest first: created last first, then stored last first.
fn newest_first(versions: &mut [&(u64, StoredWorkflow)]) {
    versions.sort_by(|(a_order, a), (b_order, b)| {
        (b.created_at, b_order).cmp(&(a.created_at, a_order))
    });
}

fn missing_test(digest: Digest) -> CatalogError {
    CatalogError::Conflict(format!(
        "the test {digest} has no row, or its deletion has started: reserve it and write its \
         blob again before saving the version"
    ))
}

impl CatalogStore for MemoryCatalog {
    async fn put_script(
        &self,
        tenant: &TenantId,
        script: &Script,
        compiled: &CompiledScript,
    ) -> Result<(), CatalogError> {
        let mut state = self.lock()?;
        state
            .scripts
            .entry((tenant.clone(), script.digest()))
            .or_insert_with(|| StoredScript {
                script: script.clone(),
                compiled: compiled.clone(),
            });
        self.send(ControlPlaneChange::Script {
            tenant: tenant.clone(),
            digest: script.digest(),
        });
        Ok(())
    }

    async fn get_script(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<StoredScript>, CatalogError> {
        Ok(self.lock()?.scripts.get(&(tenant.clone(), digest)).cloned())
    }

    async fn set_script_title(
        &self,
        tenant: &TenantId,
        script: Digest,
        title: Option<&ScriptTitle>,
    ) -> Result<bool, CatalogError> {
        let mut state = self.lock()?;
        let key = (tenant.clone(), script);
        if !state.scripts.contains_key(&key) {
            return Ok(false);
        }
        match title {
            Some(title) => state.titles.insert(key, title.clone()),
            None => state.titles.remove(&key),
        };
        self.send(ControlPlaneChange::Script {
            tenant: tenant.clone(),
            digest: script,
        });
        Ok(true)
    }

    async fn script_titles(
        &self,
        tenant: &TenantId,
        scripts: &[Digest],
    ) -> Result<Vec<(Digest, ScriptTitle)>, CatalogError> {
        let state = self.lock()?;
        Ok(scripts
            .iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|script| {
                state
                    .titles
                    .get(&(tenant.clone(), *script))
                    .map(|title| (*script, title.clone()))
            })
            .collect())
    }

    async fn put_action_set(
        &self,
        tenant: &TenantId,
        action_set: &ActionSet,
        declarations: &ToolDeclarations,
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

    async fn put_workflow(
        &self,
        tenant: &TenantId,
        workflow: &Workflow,
        now: Timestamp,
    ) -> Result<StoredWorkflow, CatalogError> {
        let mut state = self.lock()?;
        let key = (tenant.clone(), workflow.version());
        if !state.workflows.contains_key(&key) {
            for test in &workflow.suite().tests {
                let live = state
                    .tests
                    .get(&(tenant.clone(), test.test))
                    .is_some_and(|stored| stored.deletion_started.is_none());
                if !live {
                    return Err(missing_test(test.test));
                }
            }
            for test in &workflow.suite().tests {
                state
                    .workflow_tests
                    .insert((tenant.clone(), test.test, workflow.version()));
            }
        }
        let order = state.next_workflow;
        state.next_workflow += 1;
        let number = state
            .workflows
            .iter()
            .filter(|((owner, _), (_, stored))| {
                owner == tenant && stored.workflow.name() == workflow.name()
            })
            .map(|(_, (_, stored))| stored.number)
            .max()
            .unwrap_or(0)
            + 1;
        let (_, stored) = state.workflows.entry(key).or_insert_with(|| {
            (
                order,
                StoredWorkflow {
                    workflow: workflow.clone(),
                    labels: workflow.labels().clone(),
                    created_at: now,
                    number,
                },
            )
        });
        let stored = stored.clone();
        self.send(ControlPlaneChange::Workflow {
            tenant: tenant.clone(),
            name: workflow.name().to_owned(),
            version: workflow.version(),
        });
        Ok(stored)
    }

    async fn get_workflow(
        &self,
        tenant: &TenantId,
        version: Digest,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        Ok(self
            .lock()?
            .workflows
            .get(&(tenant.clone(), version))
            .map(|(_, stored)| stored.clone()))
    }

    async fn workflow_number(
        &self,
        tenant: &TenantId,
        name: &str,
        number: u32,
    ) -> Result<Option<StoredWorkflow>, CatalogError> {
        Ok(self
            .lock()?
            .workflows
            .iter()
            .find(|((owner, _), (_, stored))| {
                owner == tenant && stored.workflow.name() == name && stored.number == number
            })
            .map(|(_, (_, stored))| stored.clone()))
    }

    async fn workflow_versions(
        &self,
        tenant: &TenantId,
        name: &str,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        let state = self.lock()?;
        let mut versions: Vec<&(u64, StoredWorkflow)> = state
            .workflows
            .iter()
            .filter(|((owner, _), (_, stored))| {
                owner == tenant && stored.workflow.name() == name && stored.carries(labels)
            })
            .map(|(_, row)| row)
            .collect();
        newest_first(&mut versions);
        Ok(versions
            .into_iter()
            .map(|(_, stored)| stored.clone())
            .collect())
    }

    async fn workflows_with_script(
        &self,
        tenant: &TenantId,
        script: Digest,
        labels: &BTreeMap<String, String>,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        let state = self.lock()?;
        let mut versions: Vec<&(u64, StoredWorkflow)> = state
            .workflows
            .iter()
            .filter(|((owner, _), (_, stored))| {
                owner == tenant && stored.workflow.script() == script && stored.carries(labels)
            })
            .map(|(_, row)| row)
            .collect();
        newest_first(&mut versions);
        Ok(versions
            .into_iter()
            .map(|(_, stored)| stored.clone())
            .collect())
    }

    async fn latest_workflows(
        &self,
        filter: &WorkflowFilter,
    ) -> Result<Vec<StoredWorkflow>, CatalogError> {
        let state = self.lock()?;
        let mut latest: BTreeMap<&str, &(u64, StoredWorkflow)> = BTreeMap::new();
        for ((owner, _), row) in &state.workflows {
            let (order, stored) = row;
            if owner != &filter.tenant || !stored.carries(&filter.labels) {
                continue;
            }
            let name = stored.workflow.name();
            if filter.after.as_deref().is_some_and(|after| name <= after) {
                continue;
            }
            let newer = latest.get(name).is_none_or(|(known_order, known)| {
                (stored.created_at, order) > (known.created_at, known_order)
            });
            if newer {
                latest.insert(name, row);
            }
        }
        Ok(latest
            .into_values()
            .take(filter.limit.unwrap_or(usize::MAX))
            .map(|(_, stored)| stored.clone())
            .collect())
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
                state
                    .workflow_tests
                    .retain(|(owner, test, _)| !(owner == tenant && test == digest));
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
        self.namespace_changed(tenant, name);
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
