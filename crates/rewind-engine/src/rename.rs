//! What `rewind server rename-data` does to the catalog to give every run a workflow
//! (`archi/06-storage-and-scale.md`, "Rewriting the stored names": workflows;
//! `archi/08-decisions.md`, "Taken while reviewing the vocabulary against main": A4).
//!
//! The schema migration of the workflow model moved what SQL can move: each saved workflow, by
//! its name, is an organization's workflow with an id, its versions numbered as they were and
//! its suite; the titles people gave codes wait in the catalog ([`CatalogMigration`]). What needs
//! the runs, which live in the partitions, is this step's:
//!
//! ```text
//! the runs (CodeUse: namespace, code, saved version?) ─┐
//!   a saved workflow one namespace ran ───────────────┼─► scope_workflow: that namespace's
//!   a code run without a saved workflow ──────────────┼─► workflow_with_code, else
//!                                                     │   create_workflow (version 1, as a run makes it)
//! the titles people gave codes ───────────────────────┼─► a valid name, free in its scope: the name;
//!                                                     │   else listed
//!                                                     └─► each run's workflow and version (RunWorkflow)
//! ```
//!
//! It is idempotent: run again over the same runs, it moves, creates and names nothing more, and
//! answers the same workflow for each run. Its caller rewrites the runs' records with the
//! answer.

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{
    Digest, RunId, ServerSecrets, Timestamp, WorkflowId, WorkflowVersion, check_workflow_name,
};

use crate::ports::{
    CatalogError, CatalogMigration, Entropy, Labels, NamespaceName, NewWorkflow, RunWorkflow,
    ScopeFilter, StoredVersion, StoredWorkflow, TenantId, WorkflowCreated, WorkflowFilter,
    WorkflowPatch, WorkflowScope, WorkflowWrite,
};

/// Who the workflows this step creates were created by, as the audit log names it.
pub const RENAME_PRINCIPAL: &str = "rename-data";

/// What a run used, as its records hold it: its namespace, its code, the action set, secrets
/// and allowed labels of its start, and the saved workflow version it ran, if it named one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeUse {
    pub tenant: TenantId,
    pub namespace: NamespaceName,
    pub run: RunId,
    pub code: Digest,
    pub action_set: Digest,
    pub secrets: ServerSecrets,
    /// Its allowed labels: those of the grant that started its tree.
    pub labels: Labels,
    /// The digest of the saved workflow version it ran, as its records hold it.
    pub version: Option<Digest>,
    pub started_at: Timestamp,
}

/// A title that did not become a name, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedTitle {
    pub tenant: TenantId,
    pub code: Digest,
    pub title: String,
    pub reason: String,
}

/// What the step did, and each run's workflow.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkflowsRenamed {
    /// Each run's workflow and version.
    pub runs: BTreeMap<RunId, RunWorkflow>,
    /// The workflow of each code a namespace ran: what keys its counters.
    pub codes: BTreeMap<(TenantId, NamespaceName, Digest), WorkflowId>,
    /// The organization's workflows that became the namespace's that ran them.
    pub scoped: Vec<(TenantId, WorkflowId, NamespaceName)>,
    /// Saved workflows one namespace ran that stay the organization's: the namespace has one of
    /// that name.
    pub kept: Vec<(TenantId, WorkflowId, NamespaceName)>,
    /// The workflows created for the codes that ran without a saved workflow.
    pub created: Vec<(TenantId, StoredWorkflow)>,
    /// The titles that became names.
    pub named: Vec<(TenantId, WorkflowId, String)>,
    /// The titles that could not, listed for the release notes.
    pub dropped: Vec<DroppedTitle>,
}

/// Gives every run a workflow: see the [module](self).
pub async fn workflows<C: CatalogMigration, E: Entropy>(
    catalog: &C,
    entropy: &E,
    uses: &[CodeUse],
    now: Timestamp,
) -> Result<WorkflowsRenamed, CatalogError> {
    let mut renamed = WorkflowsRenamed::default();
    let titles = catalog.code_titles().await?;
    let tenants: BTreeSet<TenantId> = uses
        .iter()
        .map(|used| used.tenant.clone())
        .chain(titles.iter().map(|title| title.tenant.clone()))
        .collect();
    // The versions of every workflow, by their digest.
    let mut by_digest: BTreeMap<(TenantId, Digest), (StoredWorkflow, StoredVersion)> =
        BTreeMap::new();
    for tenant in &tenants {
        for workflow in every_workflow(catalog, tenant).await? {
            for version in catalog
                .versions(tenant, &workflow.id, &Labels::new())
                .await?
            {
                by_digest
                    .entry((tenant.clone(), version.digest))
                    .or_insert_with(|| (workflow.clone(), version));
            }
        }
    }
    let saved = |used: &CodeUse| {
        used.version
            .and_then(|digest| by_digest.get(&(used.tenant.clone(), digest)))
    };

    // A saved workflow belongs to the namespace all its runs came from, or to the organization
    // when several ran it or none did.
    let mut namespaces: BTreeMap<(TenantId, WorkflowId), BTreeSet<NamespaceName>> = BTreeMap::new();
    for used in uses {
        if let Some((workflow, _)) = saved(used) {
            namespaces
                .entry((used.tenant.clone(), workflow.id.clone()))
                .or_default()
                .insert(used.namespace.clone());
        }
    }
    for ((tenant, id), ran) in &namespaces {
        let ran: Vec<&NamespaceName> = ran.iter().collect();
        let (&[namespace], Some((workflow, _))) = (
            ran.as_slice(),
            by_digest.values().find(|(workflow, _)| workflow.id == *id),
        ) else {
            continue;
        };
        if workflow.scope != WorkflowScope::Organization {
            continue;
        }
        match catalog.scope_workflow(tenant, id, namespace, now).await? {
            WorkflowWrite::Written(_) => {
                renamed
                    .scoped
                    .push((tenant.clone(), id.clone(), namespace.clone()));
            }
            WorkflowWrite::NameTaken => {
                renamed
                    .kept
                    .push((tenant.clone(), id.clone(), namespace.clone()));
            }
            WorkflowWrite::NotFound => {}
        }
    }

    // Each run's workflow and version, its first run first.
    let mut ordered: Vec<&CodeUse> = uses.iter().collect();
    ordered.sort_by_key(|used| (used.started_at, used.run));
    let mut ran: BTreeMap<RunId, (TenantId, WorkflowId, StoredVersion)> = BTreeMap::new();
    for used in ordered {
        let (workflow, version) = match saved(used) {
            Some((workflow, version)) => (workflow.id.clone(), version.clone()),
            None => {
                let (workflow, version, created) = workflow_of_code(catalog, entropy, used).await?;
                if created {
                    renamed
                        .created
                        .push((used.tenant.clone(), workflow.clone()));
                }
                (workflow.id, version)
            }
        };
        renamed
            .codes
            .entry((used.tenant.clone(), used.namespace.clone(), used.code))
            .or_insert_with(|| workflow.clone());
        ran.insert(used.run, (used.tenant.clone(), workflow, version));
    }

    // A title a person gave a code becomes the name of its workflows that have none, where it
    // is a valid name, free in their scope; the others are listed.
    for title in &titles {
        let tenant = &title.tenant;
        let filter = WorkflowFilter {
            tenant: tenant.clone(),
            scope: ScopeFilter::All,
            code: Some(title.code),
            labels: Labels::new(),
            after: None,
            limit: usize::MAX,
        };
        let workflows = catalog.list_workflows(&filter).await?;
        let drop = |reason: String| DroppedTitle {
            tenant: tenant.clone(),
            code: title.code,
            title: title.title.clone(),
            reason,
        };
        if workflows.is_empty() {
            renamed
                .dropped
                .push(drop("no workflow has its code: no run used it".into()));
            continue;
        }
        let nameless: Vec<&StoredWorkflow> = workflows
            .iter()
            .filter(|workflow| workflow.name.is_none())
            .collect();
        if nameless.is_empty() {
            let names: Vec<&str> = workflows
                .iter()
                .filter_map(|workflow| workflow.name.as_deref())
                .collect();
            renamed.dropped.push(drop(format!(
                "its workflow already has a name: {}",
                names.join(", ")
            )));
            continue;
        }
        let valid = check_workflow_name(&title.title).is_ok();
        if !valid {
            renamed.dropped.push(drop(
                "it is not a valid name: an identifier of at most 64 characters, not a reserved \
                 word, not starting with wf_"
                    .into(),
            ));
        }
        for workflow in nameless {
            let patch = WorkflowPatch {
                name: valid.then(|| Some(title.title.clone())),
                description: (workflow.description.is_none() && title.description.is_some())
                    .then(|| title.description.clone()),
            };
            if patch == WorkflowPatch::default() {
                continue;
            }
            match catalog
                .update_workflow(tenant, &workflow.id, &patch, now)
                .await?
            {
                WorkflowWrite::Written(_) if valid => {
                    renamed
                        .named
                        .push((tenant.clone(), workflow.id.clone(), title.title.clone()));
                }
                WorkflowWrite::Written(_) | WorkflowWrite::NotFound => {}
                WorkflowWrite::NameTaken => {
                    renamed.dropped.push(drop(format!(
                        "the scope of {} has a workflow of that name",
                        workflow.id
                    )));
                    if let Some(description) = patch.description {
                        let patch = WorkflowPatch {
                            name: None,
                            description: Some(description),
                        };
                        catalog
                            .update_workflow(tenant, &workflow.id, &patch, now)
                            .await?;
                    }
                }
            }
        }
    }
    catalog.drop_code_titles().await?;

    // The runs, with their workflow's name as it is now.
    let mut names: BTreeMap<(TenantId, WorkflowId), Option<StoredWorkflow>> = BTreeMap::new();
    for (run, (tenant, id, version)) in ran {
        let key = (tenant.clone(), id.clone());
        if !names.contains_key(&key) {
            let workflow = catalog.workflow(&tenant, &id).await?;
            names.insert(key.clone(), workflow);
        }
        if let Some(Some(workflow)) = names.get(&key) {
            renamed
                .runs
                .insert(run, RunWorkflow::of(workflow, &version));
        }
    }
    Ok(renamed)
}

/// Every workflow of a tenant, whatever its scope and labels.
async fn every_workflow<C: CatalogMigration>(
    catalog: &C,
    tenant: &TenantId,
) -> Result<Vec<StoredWorkflow>, CatalogError> {
    const PAGE: usize = 500;
    let mut all = Vec::new();
    let mut after = None;
    loop {
        let page = catalog
            .list_workflows(&WorkflowFilter {
                tenant: tenant.clone(),
                scope: ScopeFilter::All,
                code: None,
                labels: Labels::new(),
                after: after.clone(),
                limit: PAGE,
            })
            .await?;
        let full = page.len() == PAGE;
        after = page.last().map(|workflow| workflow.id.clone());
        all.extend(page);
        if !full {
            return Ok(all);
        }
    }
}

/// The workflow a code run without a saved workflow is a run of: the one that has the code in
/// its namespace or the organization, else one created in its namespace, as a run creates it.
async fn workflow_of_code<C: CatalogMigration, E: Entropy>(
    catalog: &C,
    entropy: &E,
    used: &CodeUse,
) -> Result<(StoredWorkflow, StoredVersion, bool), CatalogError> {
    let tenant = &used.tenant;
    if let Some((workflow, version)) = catalog
        .workflow_with_code(tenant, &used.namespace, used.code, &used.labels)
        .await?
    {
        return Ok((workflow, version, false));
    }
    let version = WorkflowVersion::new(used.code, used.action_set)
        .with_labels(used.labels.clone())
        .with_secrets(used.secrets.clone());
    let created = catalog
        .create_workflow(
            tenant,
            &NewWorkflow {
                id: entropy.workflow_id(),
                scope: WorkflowScope::Namespace(used.namespace.clone()),
                name: None,
                description: None,
                labels: used.labels.clone(),
                source: None,
                created_by: RENAME_PRINCIPAL.to_owned(),
                version,
            },
            used.started_at,
        )
        .await?;
    let (workflow, created) = match created {
        WorkflowCreated::Created(workflow) => (workflow, true),
        WorkflowCreated::Existing(workflow) => (workflow, false),
        WorkflowCreated::NameTaken => {
            return Err(CatalogError::Conflict(
                "a workflow without a name took a name".into(),
            ));
        }
    };
    let version = catalog
        .versions(tenant, &workflow.id, &used.labels)
        .await?
        .into_iter()
        .find(|version| version.document.code() == used.code)
        .ok_or_else(|| {
            CatalogError::Conflict(format!(
                "the workflow {} has no version of the code {}",
                workflow.id, used.code
            ))
        })?;
    Ok((workflow, version, created))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rewind_core::{Code, Suite, SuiteTest, WorkflowVersion};

    use super::*;
    use crate::adapters::{MemoryCatalog, SeededEntropy};
    use crate::ports::{CatalogStore, CodeTitle, TestRecord};

    fn tenant() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn ns(name: &str) -> NamespaceName {
        NamespaceName::new(name).unwrap()
    }

    fn code(text: &str) -> Digest {
        Code::new(text).digest()
    }

    fn at(ms: u64) -> Timestamp {
        Timestamp::from_millis(1_767_225_600_000 + ms)
    }

    fn used(
        namespace: &str,
        run: u128,
        code_text: &str,
        version: Option<Digest>,
        ms: u64,
    ) -> CodeUse {
        CodeUse {
            tenant: tenant(),
            namespace: ns(namespace),
            run: RunId::from_u128(run),
            code: code(code_text),
            action_set: Digest::of(b"action set"),
            secrets: ServerSecrets::new(),
            labels: Labels::new(),
            version,
            started_at: at(ms),
        }
    }

    /// A saved workflow, as the schema migration leaves it: the organization's, named.
    async fn saved(
        catalog: &MemoryCatalog,
        name: &str,
        code_text: &str,
    ) -> (StoredWorkflow, Digest) {
        let version = WorkflowVersion::new(code(code_text), Digest::of(b"action set"));
        let digest = version.digest();
        let created = catalog
            .create_workflow(
                &tenant(),
                &NewWorkflow {
                    id: WorkflowId::from_bits(name.len() as u64 + 7),
                    scope: WorkflowScope::Organization,
                    name: Some(name.into()),
                    description: None,
                    labels: Labels::new(),
                    source: None,
                    created_by: "key:k".into(),
                    version,
                },
                at(0),
            )
            .await
            .unwrap();
        let WorkflowCreated::Created(workflow) = created else {
            panic!("{created:?}");
        };
        (workflow, digest)
    }

    #[tokio::test]
    async fn runs_get_workflows_by_their_use() {
        let catalog = MemoryCatalog::new();
        let entropy = SeededEntropy::new(3);
        // `prod_goal_alice` is run by alice alone, `prod_gym` by alice and bob.
        let (goal, goal_digest) = saved(&catalog, "prod_goal_alice", "goal").await;
        let (gym, gym_digest) = saved(&catalog, "prod_gym", "gym").await;
        catalog
            .put_code_title(CodeTitle {
                tenant: tenant(),
                code: code("triage"),
                title: "triage".into(),
                description: Some("Triage the issues.".into()),
            })
            .unwrap();
        catalog
            .put_code_title(CodeTitle {
                tenant: tenant(),
                code: code("digest"),
                title: "Daily digest".into(),
                description: None,
            })
            .unwrap();
        let uses = [
            used("alice", 1, "goal", Some(goal_digest), 10),
            used("alice", 2, "gym", Some(gym_digest), 11),
            used("bob", 3, "gym", Some(gym_digest), 12),
            used("alice", 4, "triage", None, 13),
            used("bob", 5, "triage", None, 14),
            used("alice", 6, "triage", None, 15),
            used("bob", 7, "digest", None, 16),
        ];
        let renamed = workflows(&catalog, &entropy, &uses, at(100)).await.unwrap();
        assert_eq!(
            renamed.scoped,
            [(tenant(), goal.id.clone(), ns("alice"))],
            "a saved workflow one namespace ran becomes that namespace's"
        );
        let goal_now = catalog
            .workflow(&tenant(), &goal.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(goal_now.scope, WorkflowScope::Namespace(ns("alice")));
        let gym_now = catalog.workflow(&tenant(), &gym.id).await.unwrap().unwrap();
        assert_eq!(gym_now.scope, WorkflowScope::Organization);
        // One workflow per code and namespace for the codes run without a saved workflow.
        assert_eq!(renamed.created.len(), 3);
        let run = |id: u128| renamed.runs[&RunId::from_u128(id)].clone();
        assert_eq!(run(1).id, goal.id);
        assert_eq!((run(2).id, run(3).id), (gym.id.clone(), gym.id.clone()));
        assert_eq!(
            run(4).id,
            run(6).id,
            "the same code in a namespace is one workflow"
        );
        assert_ne!(run(4).id, run(5).id, "another namespace's is another");
        assert_eq!(run(4).version, 1);
        // A title that is a valid name becomes the name of each workflow of its code; the
        // others are listed.
        assert_eq!(run(4).name.as_deref(), Some("triage"));
        assert_eq!(run(5).name.as_deref(), Some("triage"));
        let triage = catalog
            .workflow(&tenant(), &run(4).id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(triage.description.as_deref(), Some("Triage the issues."));
        assert_eq!(run(7).name, None);
        assert_eq!(renamed.dropped.len(), 1);
        assert_eq!(renamed.dropped[0].title, "Daily digest");
        assert!(catalog.code_titles().await.unwrap().is_empty());
        assert_eq!(
            renamed.codes[&(tenant(), ns("bob"), code("triage"))],
            run(5).id
        );

        // Run again: nothing more moves or is created, each run keeps its workflow.
        let again = workflows(&catalog, &entropy, &uses, at(200)).await.unwrap();
        assert!(again.scoped.is_empty() && again.created.is_empty() && again.named.is_empty());
        assert_eq!(again.runs, renamed.runs);
    }

    #[tokio::test]
    async fn a_saved_workflow_keeps_its_versions_and_suite_when_it_moves() {
        let catalog = MemoryCatalog::new();
        let entropy = SeededEntropy::new(4);
        let test = Digest::of(b"test");
        catalog
            .reserve_tests(
                &tenant(),
                &[TestRecord {
                    digest: test,
                    namespace: None,
                    size: 1,
                }],
                at(0),
            )
            .await
            .unwrap();
        let (goal, first) = saved(&catalog, "goal", "goal").await;
        let second = WorkflowVersion::new(code("goal 2"), Digest::of(b"action set"))
            .with_suite(Suite {
                tests: vec![SuiteTest {
                    name: "t".into(),
                    test,
                }],
                ..Suite::default()
            })
            .unwrap();
        let stored = catalog
            .put_version(
                &tenant(),
                &goal.id,
                &crate::ports::NewVersion {
                    document: second,
                    pulled: None,
                },
                at(1),
            )
            .await
            .unwrap();
        let uses = [
            used("alice", 1, "goal", Some(first), 10),
            used("alice", 2, "goal 2", Some(stored.digest), 11),
        ];
        let renamed = workflows(&catalog, &entropy, &uses, at(100)).await.unwrap();
        assert_eq!(renamed.scoped.len(), 1);
        assert_eq!(renamed.runs[&RunId::from_u128(2)].version, 2);
        let versions = catalog
            .versions(&tenant(), &goal.id, &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(catalog.suite(&tenant(), &goal.id).await.unwrap().len(), 1);
        // Its code is the namespace's now: a run of alice given it finds it.
        let found = catalog
            .workflow_with_code(&tenant(), &ns("alice"), code("goal 2"), &Labels::new())
            .await
            .unwrap()
            .map(|(workflow, version)| (workflow.id, version.version));
        assert_eq!(found, Some((goal.id.clone(), 2)));
        assert_eq!(
            catalog
                .workflow_with_code(&tenant(), &ns("bob"), code("goal 2"), &Labels::new())
                .await
                .unwrap(),
            None,
            "nor any other namespace's"
        );
    }
}
