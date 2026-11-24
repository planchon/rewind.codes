//! The conformance suite of `CatalogStore`: code and action sets by digest; workflows, their
//! scopes, names, versions, codes and suites, their promotion; the tests and the suites that
//! reference them; subscriptions, tenants, namespaces with their partition, display names,
//! listing and erasure with their workflows; and the change every write sends. The suite of
//! `CatalogMigration` checks what `rename-data` needs beyond it.
//!
//! The factory returns the catalog and the cluster its changes reach, over the same backend.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;
use std::time::Duration;

use futures_util::{Stream, StreamExt as _};
use rewind_core::{
    ActionSet, Code, Digest, Epoch, ReplacedTest, RetiredTest, Suite, SuiteTest, Timestamp,
    WorkflowId, WorkflowVersion,
};

use super::{T0, tenant};
use crate::ports::catalog::{NamespaceFilter, NamespacePage};
use crate::ports::cluster::Membership;
use crate::ports::{
    ActionDeclarations, CatalogError, CatalogMigration, CatalogStore, Cluster, CompiledCode,
    ControlPlaneChange, DeleteNamespace, DisplayName, DropNamespace, NamespaceName, NamespaceWrite,
    NewVersion, NewWorkflow, NodeId, NodeInfo, Quotas, ScopeFilter, Secret, StoredVersion,
    StoredWorkflow, Subscription, SuiteChange, SuiteMember, TEST_GRACE, Tenant, TenantId,
    TestRecord, VersionRef, WebhookType, WorkflowCreated, WorkflowFilter, WorkflowPatch,
    WorkflowScope, WorkflowSource, WorkflowWrite, namespace_partition,
};

/// Runs every check, each on a new catalog and its cluster.
pub async fn run_all<C: CatalogStore, K: Cluster>(new_store: impl Fn() -> (C, K)) {
    macro_rules! catalog_check {
        ($check:ident) => {{
            eprintln!(concat!("catalog conformance: ", stringify!($check)));
            let (catalog, _cluster) = new_store();
            $check(&catalog).await;
        }};
    }
    catalog_check!(code_and_action_sets_are_stored_by_tenant_and_digest);
    catalog_check!(workflows_are_created_with_their_first_version);
    catalog_check!(names_resolve_in_the_namespace_then_the_organization);
    catalog_check!(codes_find_their_workflow_in_the_namespace_then_the_organization);
    catalog_check!(versions_are_numbered_per_workflow_and_change_its_suite);
    catalog_check!(suites_change_without_a_version);
    catalog_check!(workflows_are_listed_by_id_in_pages);
    catalog_check!(names_and_descriptions_change_without_a_version);
    catalog_check!(a_promoted_workflow_is_the_organizations);
    catalog_check!(tests_are_kept_while_a_suite_references_them);
    catalog_check!(tests_of_an_erased_namespace_go_whatever_references_them);
    catalog_check!(subscriptions_are_stored_per_tenant);
    catalog_check!(tenants_are_stored);
    catalog_check!(namespaces_are_created_at_first_use_in_their_partition);
    catalog_check!(namespaces_have_display_names_found_by_prefix);
    catalog_check!(namespaces_are_listed_by_name_in_pages);
    catalog_check!(an_erased_namespace_is_dropped_once_marked);
    catalog_check!(a_run_not_moved_has_no_legacy_partition);
    catalog_check!(a_moved_run_is_found_in_its_partition);
    eprintln!("catalog conformance: every_write_sends_its_change");
    let (catalog, cluster) = new_store();
    every_write_sends_its_change(&catalog, &cluster).await;
}

// -------------------------------------------------------------------------------------------
// Helpers

fn acme() -> TenantId {
    tenant("acme")
}

fn namespace(name: &str) -> NamespaceName {
    NamespaceName::new(name).expect("the suites use valid namespace names")
}

fn alice() -> NamespaceName {
    namespace("alice")
}

fn bob() -> NamespaceName {
    namespace("bob")
}

fn later(at: Timestamp, ms: u64) -> Timestamp {
    Timestamp(at.0 + ms)
}

fn partitions(count: u16) -> NonZeroU16 {
    NonZeroU16::new(count).expect("at least one partition")
}

/// The partitions of the deployment the checks create namespaces in.
fn deployment() -> NonZeroU16 {
    partitions(64)
}

fn action_set_digest() -> Digest {
    Digest::of(b"action_set")
}

/// A version of `code`, without an interface, made under `labels`.
fn version(code: &str, labels: &BTreeMap<String, String>) -> WorkflowVersion {
    WorkflowVersion::new(Digest::of(code.as_bytes()), action_set_digest())
        .with_labels(labels.clone())
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn no_labels() -> BTreeMap<String, String> {
    BTreeMap::new()
}

fn id(n: u64) -> WorkflowId {
    WorkflowId::from_bits(n)
}

fn new_workflow(
    n: u64,
    scope: WorkflowScope,
    name: Option<&str>,
    version: WorkflowVersion,
) -> NewWorkflow {
    NewWorkflow {
        id: id(n),
        scope,
        name: name.map(str::to_owned),
        description: name.map(|name| format!("The {name} workflow")),
        labels: version.labels().clone(),
        source: None,
        created_by: "key:k1".into(),
        version,
    }
}

fn in_namespace(name: &NamespaceName) -> WorkflowScope {
    WorkflowScope::Namespace(name.clone())
}

fn test_record(name: &str, namespace: Option<&str>) -> TestRecord {
    TestRecord {
        digest: Digest::of(name.as_bytes()),
        namespace: namespace.map(self::namespace),
        size: 100,
    }
}

fn with_tests(version: WorkflowVersion, tests: &[(&str, &TestRecord)]) -> WorkflowVersion {
    version
        .with_suite(Suite {
            base: None,
            tests: tests
                .iter()
                .map(|(name, test)| SuiteTest {
                    name: (*name).to_owned(),
                    test: test.digest,
                })
                .collect(),
            retired: Vec::new(),
            replaced: Vec::new(),
        })
        .expect("a valid suite")
}

async fn create<C: CatalogStore>(
    catalog: &C,
    workflow: &NewWorkflow,
    at: Timestamp,
) -> StoredWorkflow {
    match catalog
        .create_workflow(&acme(), workflow, at)
        .await
        .expect("create_workflow")
    {
        WorkflowCreated::Created(stored) => stored,
        other => panic!("{} is not created: {other:?}", workflow.id),
    }
}

async fn ensure<C: CatalogStore>(catalog: &C, tenant: &TenantId, name: &NamespaceName) {
    let written = catalog
        .ensure_namespace(tenant, name, deployment(), T0)
        .await
        .expect("ensure_namespace");
    assert!(matches!(written, NamespaceWrite::Written(_)), "{written:?}");
}

fn numbers(versions: &[StoredVersion]) -> Vec<u32> {
    versions.iter().map(|version| version.version).collect()
}

fn ids(workflows: &[StoredWorkflow]) -> Vec<WorkflowId> {
    workflows
        .iter()
        .map(|workflow| workflow.id.clone())
        .collect()
}

fn current(members: &[SuiteMember]) -> Vec<(String, Digest)> {
    members
        .iter()
        .filter(|member| member.retired_at.is_none())
        .map(|member| (member.name.clone(), member.digest))
        .collect()
}

// -------------------------------------------------------------------------------------------
// Code and action sets

pub async fn code_and_action_sets_are_stored_by_tenant_and_digest<C: CatalogStore>(catalog: &C) {
    let other = tenant("other");
    let code = Code::new("export default () => 1");
    let compiled = CompiledCode {
        javascript: "export default () => 1".into(),
        source_map: "{}".into(),
    };
    assert_eq!(
        catalog
            .get_code(&acme(), code.digest())
            .await
            .expect("get_code"),
        None
    );
    catalog
        .put_code(&acme(), &code, &compiled)
        .await
        .expect("put_code");
    let recompiled = CompiledCode {
        javascript: "changed".into(),
        source_map: String::new(),
    };
    catalog
        .put_code(&acme(), &code, &recompiled)
        .await
        .expect("put_code is idempotent");
    let stored = catalog
        .get_code(&acme(), code.digest())
        .await
        .expect("get_code")
        .expect("stored");
    assert_eq!(
        (stored.code, stored.compiled),
        (code.clone(), compiled),
        "the first put wins"
    );
    assert_eq!(
        catalog
            .get_code(&other, code.digest())
            .await
            .expect("get_code"),
        None,
        "another tenant does not see it"
    );

    let action_set = ActionSet::from_harness_actions(["a", "b"]).expect("action set");
    let declarations = ActionDeclarations {
        typescript: "declare module \"rewind:actions\" {}".into(),
        prompt: "a() · b()".into(),
    };
    catalog
        .put_action_set(&acme(), &action_set, &declarations)
        .await
        .expect("put_action_set");
    catalog
        .put_action_set(&acme(), &action_set, &ActionDeclarations::default())
        .await
        .expect("put_action_set is idempotent");
    let stored = catalog
        .get_action_set(&acme(), action_set.digest())
        .await
        .expect("get_action_set")
        .expect("stored");
    assert_eq!(stored.action_set, action_set);
    assert_eq!(stored.declarations, declarations, "the first put wins");
    assert_eq!(
        catalog
            .get_action_set(&other, action_set.digest())
            .await
            .expect("get_action_set"),
        None
    );
    let second = ActionSet::from_harness_actions(["c"]).expect("action set");
    for tenant in [acme(), other] {
        catalog
            .put_action_set(&tenant, &second, &ActionDeclarations::default())
            .await
            .expect("put_action_set");
    }
    let mut expected = vec![action_set.digest(), second.digest()];
    expected.sort();
    assert_eq!(
        catalog
            .list_action_sets(&acme())
            .await
            .expect("list_action_sets")
            .iter()
            .map(|stored| stored.action_set.digest())
            .collect::<Vec<_>>(),
        expected,
        "the tenant's action sets, by digest"
    );
    assert_eq!(
        catalog
            .list_action_sets(&tenant("nobody"))
            .await
            .expect("list_action_sets"),
        []
    );
}

// -------------------------------------------------------------------------------------------
// Workflows

pub async fn workflows_are_created_with_their_first_version<C: CatalogStore>(catalog: &C) {
    let first = new_workflow(
        1,
        in_namespace(&alice()),
        None,
        version("triage", &no_labels()),
    );
    let created = create(catalog, &first, T0).await;
    assert_eq!(
        created,
        StoredWorkflow {
            id: id(1),
            scope: in_namespace(&alice()),
            name: None,
            description: None,
            labels: no_labels(),
            source: None,
            created_by: "key:k1".into(),
            created_at: T0,
            promoted_at: None,
            latest: 1,
        }
    );
    assert_eq!(
        catalog.workflow(&acme(), &id(1)).await.expect("workflow"),
        Some(created.clone())
    );
    assert_eq!(
        catalog
            .workflow(&tenant("other"), &id(1))
            .await
            .expect("workflow"),
        None,
        "another tenant's"
    );
    let versions = catalog
        .versions(&acme(), &id(1), &no_labels())
        .await
        .expect("versions");
    assert_eq!(numbers(&versions), [1]);
    let one = &versions[0];
    assert_eq!(
        (
            &one.workflow,
            &one.document,
            one.digest,
            &one.labels,
            one.created_at
        ),
        (
            &id(1),
            &first.version,
            first.version.digest(),
            &no_labels(),
            T0
        )
    );
    assert_eq!(
        catalog
            .version(&acme(), &id(1), VersionRef::Number(1))
            .await
            .expect("version"),
        Some(one.clone())
    );
    assert_eq!(
        catalog
            .version(&acme(), &id(1), VersionRef::Digest(one.digest))
            .await
            .expect("version"),
        Some(one.clone())
    );
    assert_eq!(
        catalog
            .version(&acme(), &id(1), VersionRef::Number(2))
            .await
            .expect("version"),
        None
    );

    // Without a name, a code its scope has under labels it carries: that workflow, unchanged.
    let again = new_workflow(
        2,
        in_namespace(&alice()),
        None,
        version("triage", &no_labels()),
    );
    assert_eq!(
        catalog
            .create_workflow(&acme(), &again, later(T0, 1))
            .await
            .expect("create_workflow"),
        WorkflowCreated::Existing(created.clone())
    );
    assert_eq!(
        catalog.workflow(&acme(), &id(2)).await.expect("workflow"),
        None
    );
    // In another namespace, or the organization, the same code is another workflow.
    create(
        catalog,
        &new_workflow(
            3,
            in_namespace(&bob()),
            None,
            version("triage", &no_labels()),
        ),
        T0,
    )
    .await;
    create(
        catalog,
        &new_workflow(
            4,
            WorkflowScope::Organization,
            None,
            version("triage", &no_labels()),
        ),
        T0,
    )
    .await;
    // A name is unique in its scope.
    let named = new_workflow(
        5,
        in_namespace(&alice()),
        Some("triage"),
        version("other", &no_labels()),
    );
    let named = create(catalog, &named, T0).await;
    assert_eq!(named.name.as_deref(), Some("triage"));
    assert_eq!(named.description.as_deref(), Some("The triage workflow"));
    assert_eq!(
        catalog
            .create_workflow(
                &acme(),
                &new_workflow(
                    6,
                    in_namespace(&alice()),
                    Some("triage"),
                    version("third", &no_labels())
                ),
                T0
            )
            .await
            .expect("create_workflow"),
        WorkflowCreated::NameTaken
    );
    create(
        catalog,
        &new_workflow(
            7,
            WorkflowScope::Organization,
            Some("triage"),
            version("third", &no_labels()),
        ),
        T0,
    )
    .await;
    create(
        catalog,
        &new_workflow(
            8,
            in_namespace(&bob()),
            Some("triage"),
            version("fourth", &no_labels()),
        ),
        T0,
    )
    .await;
    // A name given creates a workflow even for a code its scope has.
    create(
        catalog,
        &new_workflow(
            9,
            in_namespace(&alice()),
            Some("again"),
            version("triage", &no_labels()),
        ),
        T0,
    )
    .await;

    // A version made under labels is seen only by the callers those labels carry.
    let dev = labels(&[("env", "dev")]);
    create(
        catalog,
        &new_workflow(10, in_namespace(&alice()), None, version("dev only", &dev)),
        T0,
    )
    .await;
    assert_eq!(
        numbers(
            &catalog
                .versions(&acme(), &id(10), &dev)
                .await
                .expect("versions")
        ),
        [1]
    );
    assert_eq!(
        numbers(
            &catalog
                .versions(&acme(), &id(10), &no_labels())
                .await
                .expect("versions")
        ),
        [1],
        "a caller without labels sees every version"
    );
    assert_eq!(
        numbers(
            &catalog
                .versions(&acme(), &id(10), &labels(&[("env", "prod")]))
                .await
                .expect("versions")
        ),
        Vec::<u32>::new()
    );
    // A test without a row refuses the creation.
    let missing = test_record("missing", None);
    let refused = catalog
        .create_workflow(
            &acme(),
            &new_workflow(
                11,
                in_namespace(&alice()),
                None,
                with_tests(version("tested", &no_labels()), &[("t", &missing)]),
            ),
            T0,
        )
        .await;
    assert!(
        matches!(refused, Err(CatalogError::Conflict(_))),
        "{refused:?}"
    );
}

pub async fn names_resolve_in_the_namespace_then_the_organization<C: CatalogStore>(catalog: &C) {
    let org = create(
        catalog,
        &new_workflow(
            1,
            WorkflowScope::Organization,
            Some("gym"),
            version("org gym", &no_labels()),
        ),
        T0,
    )
    .await;
    let forked = create(
        catalog,
        &new_workflow(
            2,
            in_namespace(&alice()),
            Some("gym"),
            version("alice gym", &no_labels()),
        ),
        T0,
    )
    .await;
    let dev = labels(&[("env", "dev")]);
    create(
        catalog,
        &new_workflow(
            3,
            in_namespace(&bob()),
            Some("water"),
            version("water", &dev),
        ),
        T0,
    )
    .await;
    let named = |namespace: Option<NamespaceName>,
                 name: &'static str,
                 labels: BTreeMap<String, String>| async move {
        catalog
            .workflow_named(&acme(), namespace.as_ref(), name, &labels)
            .await
            .expect("workflow_named")
            .map(|workflow| workflow.id)
    };
    assert_eq!(
        named(Some(alice()), "gym", no_labels()).await,
        Some(forked.id.clone()),
        "the namespace's shadows the organization's"
    );
    assert_eq!(
        named(Some(bob()), "gym", no_labels()).await,
        Some(org.id.clone()),
        "else the organization's"
    );
    assert_eq!(
        named(None, "gym", no_labels()).await,
        Some(org.id.clone()),
        "without a namespace, the organization's"
    );
    assert_eq!(
        named(Some(alice()), "water", no_labels()).await,
        None,
        "never another namespace's"
    );
    assert_eq!(named(Some(bob()), "water", dev.clone()).await, Some(id(3)));
    assert_eq!(
        named(Some(bob()), "water", labels(&[("env", "prod")])).await,
        None,
        "nor one whose versions do not carry the caller's labels"
    );
    assert_eq!(
        catalog
            .workflow_named(&tenant("other"), Some(&alice()), "gym", &no_labels())
            .await
            .expect("workflow_named"),
        None
    );
}

pub async fn codes_find_their_workflow_in_the_namespace_then_the_organization<C: CatalogStore>(
    catalog: &C,
) {
    let code = |text: &str| Digest::of(text.as_bytes());
    let org = create(
        catalog,
        &new_workflow(
            1,
            WorkflowScope::Organization,
            Some("gym"),
            version("gym v1", &no_labels()),
        ),
        T0,
    )
    .await;
    catalog
        .put_version(
            &acme(),
            &org.id,
            &NewVersion {
                document: version("gym v2", &no_labels()),
                pulled: None,
            },
            later(T0, 1),
        )
        .await
        .expect("put_version");
    catalog
        .put_version(
            &acme(),
            &org.id,
            &NewVersion {
                document: version("gym v1", &no_labels()),
                pulled: None,
            },
            later(T0, 2),
        )
        .await
        .expect("put_version");
    let found = |namespace: NamespaceName, text: &'static str, labels: BTreeMap<String, String>| async move {
        catalog
            .workflow_with_code(&acme(), &namespace, code(text), &labels)
            .await
            .expect("workflow_with_code")
            .map(|(workflow, version)| (workflow.id, version.version))
    };
    assert_eq!(
        found(alice(), "gym v1", no_labels()).await,
        Some((org.id.clone(), 3)),
        "the latest version with that code"
    );
    assert_eq!(
        found(alice(), "gym v2", no_labels()).await,
        Some((org.id.clone(), 2))
    );
    let alice_gym = create(
        catalog,
        &new_workflow(
            2,
            in_namespace(&alice()),
            None,
            version("gym v1", &no_labels()),
        ),
        T0,
    )
    .await;
    assert_eq!(
        found(alice(), "gym v1", no_labels()).await,
        Some((alice_gym.id.clone(), 1)),
        "the namespace's first"
    );
    assert_eq!(
        found(bob(), "gym v1", no_labels()).await,
        Some((org.id.clone(), 3)),
        "another namespace's is not seen"
    );
    assert_eq!(found(alice(), "nothing", no_labels()).await, None);
    // Under labels: the workflow whose code row carries the caller's labels.
    let dev = labels(&[("env", "dev")]);
    let dev_flow = create(
        catalog,
        &new_workflow(3, in_namespace(&bob()), None, version("labelled", &dev)),
        T0,
    )
    .await;
    assert_eq!(
        found(bob(), "labelled", dev.clone()).await,
        Some((dev_flow.id.clone(), 1))
    );
    assert_eq!(
        found(bob(), "labelled", no_labels()).await,
        Some((dev_flow.id.clone(), 1)),
        "a caller without labels sees it"
    );
    assert_eq!(
        found(bob(), "labelled", labels(&[("env", "prod")])).await,
        None
    );
    // The first workflow to have a code there keeps it.
    create(
        catalog,
        &new_workflow(
            4,
            in_namespace(&bob()),
            Some("named"),
            version("labelled", &dev),
        ),
        later(T0, 5),
    )
    .await;
    assert_eq!(found(bob(), "labelled", dev).await, Some((dev_flow.id, 1)));
}

pub async fn versions_are_numbered_per_workflow_and_change_its_suite<C: CatalogStore>(catalog: &C) {
    let a = test_record("a", None);
    let b = test_record("b", None);
    let b2 = test_record("b2", None);
    catalog
        .reserve_tests(&acme(), &[a.clone(), b.clone(), b2.clone()], T0)
        .await
        .expect("reserve_tests");
    let first = with_tests(version("v1", &no_labels()), &[("a", &a), ("b", &b)]);
    let workflow = create(
        catalog,
        &new_workflow(1, in_namespace(&alice()), Some("triage"), first),
        T0,
    )
    .await;
    create(
        catalog,
        &new_workflow(
            2,
            in_namespace(&alice()),
            Some("other"),
            version("o1", &no_labels()),
        ),
        T0,
    )
    .await;
    let second = version("v2", &no_labels())
        .with_suite(Suite {
            base: None,
            tests: vec![SuiteTest {
                name: "b".into(),
                test: b2.digest,
            }],
            retired: vec![RetiredTest {
                name: "a".into(),
                test: a.digest,
                reason: "deprecated".into(),
            }],
            replaced: vec![ReplacedTest {
                name: "b".into(),
                old: b.digest,
                new: b2.digest,
                reason: "out of date".into(),
            }],
        })
        .expect("suite");
    let stored = catalog
        .put_version(
            &acme(),
            &workflow.id,
            &NewVersion {
                document: second.clone(),
                pulled: None,
            },
            later(T0, 10),
        )
        .await
        .expect("put_version");
    assert_eq!(
        (stored.version, stored.digest, stored.created_at),
        (2, second.digest(), later(T0, 10))
    );
    let third = catalog
        .put_version(
            &acme(),
            &id(2),
            &NewVersion {
                document: version("o2", &no_labels()),
                pulled: None,
            },
            later(T0, 11),
        )
        .await
        .expect("put_version");
    assert_eq!(third.version, 2, "numbered per workflow");
    assert_eq!(
        numbers(
            &catalog
                .versions(&acme(), &workflow.id, &no_labels())
                .await
                .expect("versions")
        ),
        [2, 1],
        "newest first"
    );
    assert_eq!(
        catalog
            .workflow(&acme(), &workflow.id)
            .await
            .expect("workflow")
            .map(|w| w.latest),
        Some(2)
    );
    // The version's code is the scope's now.
    assert_eq!(
        catalog
            .workflow_with_code(&acme(), &alice(), Digest::of(b"v2"), &no_labels())
            .await
            .expect("workflow_with_code")
            .map(|(workflow, version)| (workflow.id, version.version)),
        Some((workflow.id.clone(), 2))
    );
    // The suite is the version's: the tests it no longer has left with its reasons.
    let suite = catalog.suite(&acme(), &workflow.id).await.expect("suite");
    assert_eq!(current(&suite), [("b".to_owned(), b2.digest)]);
    let left: Vec<(&str, Digest, Option<&str>, Option<Timestamp>)> = suite
        .iter()
        .filter(|member| member.retired_at.is_some())
        .map(|member| {
            (
                member.name.as_str(),
                member.digest,
                member.reason.as_deref(),
                member.retired_at,
            )
        })
        .collect();
    assert_eq!(
        left,
        [
            ("a", a.digest, Some("deprecated"), Some(later(T0, 10))),
            ("b", b.digest, Some("out of date"), Some(later(T0, 10)))
        ]
    );
    assert_eq!(suite[0].added_at, T0);
    // Refused: an unknown workflow, a test without a row.
    let refused = catalog
        .put_version(
            &acme(),
            &id(99),
            &NewVersion {
                document: version("x", &no_labels()),
                pulled: None,
            },
            T0,
        )
        .await;
    assert!(
        matches!(refused, Err(CatalogError::Conflict(_))),
        "{refused:?}"
    );
    let missing = test_record("missing", None);
    let refused = catalog
        .put_version(
            &acme(),
            &workflow.id,
            &NewVersion {
                document: with_tests(version("v3", &no_labels()), &[("m", &missing)]),
                pulled: None,
            },
            T0,
        )
        .await;
    assert!(
        matches!(refused, Err(CatalogError::Conflict(_))),
        "{refused:?}"
    );

    // A fork's pull moves its source.
    let source = create(
        catalog,
        &new_workflow(
            3,
            WorkflowScope::Organization,
            Some("gym"),
            version("gym", &no_labels()),
        ),
        T0,
    )
    .await;
    let mut fork = new_workflow(
        4,
        in_namespace(&bob()),
        Some("gym"),
        version("gym", &no_labels()),
    );
    fork.source = Some(WorkflowSource {
        workflow: source.id.clone(),
        version: 1,
    });
    let fork = create(catalog, &fork, T0).await;
    assert_eq!(
        fork.source,
        Some(WorkflowSource {
            workflow: source.id.clone(),
            version: 1
        })
    );
    catalog
        .put_version(
            &acme(),
            &fork.id,
            &NewVersion {
                document: version("gym 2", &no_labels()),
                pulled: Some(3),
            },
            later(T0, 20),
        )
        .await
        .expect("put_version");
    assert_eq!(
        catalog
            .workflow(&acme(), &fork.id)
            .await
            .expect("workflow")
            .and_then(|w| w.source),
        Some(WorkflowSource {
            workflow: source.id,
            version: 3
        })
    );
}

pub async fn suites_change_without_a_version<C: CatalogStore>(catalog: &C) {
    let a = test_record("a", None);
    let b = test_record("b", None);
    let c = test_record("c", None);
    let b2 = test_record("b2", None);
    catalog
        .reserve_tests(&acme(), &[a.clone(), b.clone(), c.clone(), b2.clone()], T0)
        .await
        .expect("reserve_tests");
    let first = with_tests(version("v1", &no_labels()), &[("a", &a), ("b", &b)]);
    let workflow = create(
        catalog,
        &new_workflow(1, in_namespace(&alice()), None, first),
        T0,
    )
    .await;
    let suite = catalog
        .change_suite(
            &acme(),
            &workflow.id,
            &SuiteChange {
                added: vec![SuiteTest {
                    name: "c".into(),
                    test: c.digest,
                }],
                replaced: vec![ReplacedTest {
                    name: "b".into(),
                    old: b.digest,
                    new: b2.digest,
                    reason: "out of date".into(),
                }],
                retired: vec![RetiredTest {
                    name: "a".into(),
                    test: a.digest,
                    reason: "deprecated".into(),
                }],
            },
            later(T0, 5),
        )
        .await
        .expect("change_suite");
    assert_eq!(
        suite,
        catalog.suite(&acme(), &workflow.id).await.expect("suite")
    );
    assert_eq!(
        current(&suite),
        [("b".to_owned(), b2.digest), ("c".to_owned(), c.digest)]
    );
    let names: Vec<(&str, Digest, Option<&str>)> = suite
        .iter()
        .map(|member| {
            (
                member.name.as_str(),
                member.digest,
                member.reason.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [
            ("a", a.digest, Some("deprecated")),
            ("b", b.digest, Some("out of date")),
            ("b", b2.digest, None),
            ("c", c.digest, None)
        ],
        "by name, then when they joined"
    );
    assert_eq!(
        numbers(
            &catalog
                .versions(&acme(), &workflow.id, &no_labels())
                .await
                .expect("versions")
        ),
        [1],
        "no version"
    );
    // A test added again under its name and digest joins again.
    let suite = catalog
        .change_suite(
            &acme(),
            &workflow.id,
            &SuiteChange {
                added: vec![SuiteTest {
                    name: "a".into(),
                    test: a.digest,
                }],
                ..SuiteChange::default()
            },
            later(T0, 6),
        )
        .await
        .expect("change_suite");
    assert_eq!(current(&suite).len(), 3);
    for refused in [
        SuiteChange {
            added: vec![SuiteTest {
                name: "m".into(),
                test: Digest::of(b"missing"),
            }],
            ..SuiteChange::default()
        },
        SuiteChange {
            retired: vec![RetiredTest {
                name: "zz".into(),
                test: c.digest,
                reason: "r".into(),
            }],
            ..SuiteChange::default()
        },
        SuiteChange {
            retired: vec![RetiredTest {
                name: "b".into(),
                test: b.digest,
                reason: "already gone".into(),
            }],
            ..SuiteChange::default()
        },
    ] {
        let result = catalog
            .change_suite(&acme(), &workflow.id, &refused, later(T0, 7))
            .await;
        assert!(
            matches!(result, Err(CatalogError::Conflict(_))),
            "{refused:?}: {result:?}"
        );
    }
    let result = catalog
        .change_suite(&acme(), &id(99), &SuiteChange::default(), T0)
        .await;
    assert!(
        matches!(result, Err(CatalogError::Conflict(_))),
        "{result:?}"
    );
}

pub async fn workflows_are_listed_by_id_in_pages<C: CatalogStore>(catalog: &C) {
    let dev = labels(&[("env", "dev")]);
    create(
        catalog,
        &new_workflow(
            5,
            WorkflowScope::Organization,
            Some("gym"),
            version("gym", &no_labels()),
        ),
        T0,
    )
    .await;
    create(
        catalog,
        &new_workflow(3, in_namespace(&alice()), None, version("a", &no_labels())),
        T0,
    )
    .await;
    create(
        catalog,
        &new_workflow(1, in_namespace(&alice()), None, version("b", &dev)),
        T0,
    )
    .await;
    create(
        catalog,
        &new_workflow(4, in_namespace(&bob()), None, version("a", &no_labels())),
        T0,
    )
    .await;
    create(
        catalog,
        &new_workflow(2, WorkflowScope::Organization, None, version("c", &dev)),
        T0,
    )
    .await;
    let list = |scope: ScopeFilter,
                code: Option<&str>,
                labels: BTreeMap<String, String>,
                after: Option<u64>,
                limit: usize| {
        let filter = WorkflowFilter {
            tenant: acme(),
            scope,
            code: code.map(|code| Digest::of(code.as_bytes())),
            labels,
            after: after.map(id),
            limit,
        };
        async move {
            ids(&catalog
                .list_workflows(&filter)
                .await
                .expect("list_workflows"))
        }
    };
    assert_eq!(
        list(ScopeFilter::All, None, no_labels(), None, 10).await,
        [id(1), id(2), id(3), id(4), id(5)],
        "by id"
    );
    assert_eq!(
        list(ScopeFilter::All, None, no_labels(), None, 2).await,
        [id(1), id(2)]
    );
    assert_eq!(
        list(ScopeFilter::All, None, no_labels(), Some(2), 2).await,
        [id(3), id(4)]
    );
    assert_eq!(
        list(ScopeFilter::Visible(alice()), None, no_labels(), None, 10).await,
        [id(1), id(2), id(3), id(5)]
    );
    assert_eq!(
        list(ScopeFilter::Organization, None, no_labels(), None, 10).await,
        [id(2), id(5)]
    );
    assert_eq!(
        list(ScopeFilter::Namespace(bob()), None, no_labels(), None, 10).await,
        [id(4)]
    );
    assert_eq!(
        list(ScopeFilter::All, Some("a"), no_labels(), None, 10).await,
        [id(3), id(4)]
    );
    assert_eq!(
        list(ScopeFilter::All, None, dev, None, 10).await,
        [id(1), id(2)],
        "the workflows a version of which carries the labels"
    );
    let other = WorkflowFilter {
        tenant: tenant("other"),
        scope: ScopeFilter::All,
        code: None,
        labels: no_labels(),
        after: None,
        limit: 10,
    };
    assert_eq!(
        catalog
            .list_workflows(&other)
            .await
            .expect("list_workflows"),
        []
    );
}

pub async fn names_and_descriptions_change_without_a_version<C: CatalogStore>(catalog: &C) {
    let workflow = create(
        catalog,
        &new_workflow(1, in_namespace(&alice()), None, version("a", &no_labels())),
        T0,
    )
    .await;
    create(
        catalog,
        &new_workflow(
            2,
            in_namespace(&alice()),
            Some("taken"),
            version("b", &no_labels()),
        ),
        T0,
    )
    .await;
    let patch = WorkflowPatch {
        name: Some(Some("triage".into())),
        description: Some(Some("Triage.".into())),
    };
    let WorkflowWrite::Written(named) = catalog
        .update_workflow(&acme(), &workflow.id, &patch, later(T0, 1))
        .await
        .expect("update_workflow")
    else {
        panic!("not written");
    };
    assert_eq!(
        (named.name.as_deref(), named.description.as_deref()),
        (Some("triage"), Some("Triage."))
    );
    assert_eq!(named.id, workflow.id);
    assert_eq!(
        catalog
            .workflow(&acme(), &workflow.id)
            .await
            .expect("workflow"),
        Some(named.clone())
    );
    assert_eq!(
        numbers(
            &catalog
                .versions(&acme(), &workflow.id, &no_labels())
                .await
                .expect("versions")
        ),
        [1]
    );
    assert_eq!(
        catalog
            .update_workflow(
                &acme(),
                &workflow.id,
                &WorkflowPatch {
                    name: Some(Some("taken".into())),
                    description: None
                },
                T0
            )
            .await
            .expect("update_workflow"),
        WorkflowWrite::NameTaken
    );
    assert_eq!(
        catalog
            .update_workflow(
                &acme(),
                &workflow.id,
                &WorkflowPatch {
                    name: Some(Some("triage".into())),
                    description: None
                },
                T0
            )
            .await
            .expect("update_workflow"),
        WorkflowWrite::Written(named.clone()),
        "its own name again"
    );
    let WorkflowWrite::Written(described) = catalog
        .update_workflow(
            &acme(),
            &workflow.id,
            &WorkflowPatch {
                name: None,
                description: Some(None),
            },
            T0,
        )
        .await
        .expect("update_workflow")
    else {
        panic!("not written");
    };
    assert_eq!(
        (described.name.as_deref(), described.description),
        (Some("triage"), None),
        "a field left out stays"
    );
    let WorkflowWrite::Written(unnamed) = catalog
        .update_workflow(
            &acme(),
            &workflow.id,
            &WorkflowPatch {
                name: Some(None),
                description: None,
            },
            T0,
        )
        .await
        .expect("update_workflow")
    else {
        panic!("not written");
    };
    assert_eq!(unnamed.name, None);
    // Its name is free again: another workflow of the scope takes it.
    assert!(matches!(
        catalog
            .update_workflow(
                &acme(),
                &id(2),
                &WorkflowPatch {
                    name: Some(Some("triage".into())),
                    description: None
                },
                T0
            )
            .await
            .expect("update_workflow"),
        WorkflowWrite::Written(_)
    ));
    assert_eq!(
        catalog
            .update_workflow(&acme(), &id(99), &WorkflowPatch::default(), T0)
            .await
            .expect("update_workflow"),
        WorkflowWrite::NotFound
    );
}

pub async fn a_promoted_workflow_is_the_organizations<C: CatalogStore>(catalog: &C) {
    let a = test_record("a", None);
    catalog
        .reserve_tests(&acme(), std::slice::from_ref(&a), T0)
        .await
        .expect("reserve_tests");
    let first = with_tests(version("gym", &no_labels()), &[("a", &a)]);
    let workflow = create(
        catalog,
        &new_workflow(1, in_namespace(&alice()), Some("gym"), first),
        T0,
    )
    .await;
    let WorkflowWrite::Written(promoted) = catalog
        .promote_workflow(&acme(), &workflow.id, later(T0, 3))
        .await
        .expect("promote_workflow")
    else {
        panic!("not promoted");
    };
    assert_eq!(
        (
            &promoted.id,
            &promoted.scope,
            promoted.promoted_at,
            promoted.latest
        ),
        (
            &workflow.id,
            &WorkflowScope::Organization,
            Some(later(T0, 3)),
            1
        )
    );
    assert_eq!(
        current(&catalog.suite(&acme(), &workflow.id).await.expect("suite")).len(),
        1,
        "with its suite"
    );
    assert_eq!(
        catalog
            .workflow_with_code(&acme(), &bob(), Digest::of(b"gym"), &no_labels())
            .await
            .expect("workflow_with_code")
            .map(|(workflow, _)| workflow.id),
        Some(workflow.id.clone()),
        "and its codes: every namespace finds it"
    );
    assert_eq!(
        catalog
            .workflow_named(&acme(), Some(&bob()), "gym", &no_labels())
            .await
            .expect("workflow_named")
            .map(|workflow| workflow.id),
        Some(workflow.id.clone())
    );
    assert_eq!(
        catalog
            .promote_workflow(&acme(), &workflow.id, later(T0, 4))
            .await
            .expect("promote_workflow"),
        WorkflowWrite::Written(promoted),
        "an organization's workflow, as it is"
    );
    create(
        catalog,
        &new_workflow(
            2,
            in_namespace(&bob()),
            Some("gym"),
            version("bob gym", &no_labels()),
        ),
        T0,
    )
    .await;
    assert_eq!(
        catalog
            .promote_workflow(&acme(), &id(2), T0)
            .await
            .expect("promote_workflow"),
        WorkflowWrite::NameTaken,
        "the organization has its name"
    );
    assert_eq!(
        catalog
            .workflow(&acme(), &id(2))
            .await
            .expect("workflow")
            .map(|w| w.scope),
        Some(in_namespace(&bob()))
    );
    assert_eq!(
        catalog
            .promote_workflow(&acme(), &id(99), T0)
            .await
            .expect("promote_workflow"),
        WorkflowWrite::NotFound
    );
}

// -------------------------------------------------------------------------------------------
// Tests

pub async fn tests_are_kept_while_a_suite_references_them<C: CatalogStore>(catalog: &C) {
    let referenced = test_record("referenced", None);
    let recorded = test_record("recorded", Some("alice"));
    assert_eq!(
        catalog
            .reserve_tests(&acme(), &[referenced.clone(), recorded.clone()], T0)
            .await
            .expect("reserve_tests"),
        Vec::<Digest>::new()
    );
    let late = test_record("late", None);
    assert_eq!(
        catalog
            .get_test(&acme(), recorded.digest)
            .await
            .expect("get_test"),
        Some(recorded.clone())
    );
    assert_eq!(
        catalog
            .get_test(&tenant("other"), recorded.digest)
            .await
            .expect("get_test"),
        None,
        "another tenant's test"
    );
    let refused = catalog
        .create_workflow(
            &acme(),
            &new_workflow(
                1,
                in_namespace(&alice()),
                None,
                with_tests(
                    version("v0", &no_labels()),
                    &[("a", &referenced), ("b", &late)],
                ),
            ),
            T0,
        )
        .await;
    assert!(
        matches!(refused, Err(CatalogError::Conflict(_))),
        "a suite referencing a test without a row: {refused:?}"
    );
    let workflow = create(
        catalog,
        &new_workflow(
            2,
            in_namespace(&alice()),
            None,
            with_tests(version("v1", &no_labels()), &[("a", &referenced)]),
        ),
        T0,
    )
    .await;

    let grace = T0.saturating_add(TEST_GRACE);
    assert_eq!(
        catalog
            .unreferenced_tests(later(T0, 1), 10)
            .await
            .expect("unreferenced_tests"),
        [],
        "within their grace, no test is cleaned up"
    );
    assert_eq!(
        catalog
            .unreferenced_tests(grace, 10)
            .await
            .expect("unreferenced_tests"),
        [(acme(), recorded.digest)],
        "after it, the test no suite references is marked"
    );
    assert_eq!(
        catalog
            .unreferenced_tests(grace, 10)
            .await
            .expect("unreferenced_tests"),
        [],
        "only once"
    );
    assert_eq!(
        catalog
            .get_test(&acme(), recorded.digest)
            .await
            .expect("get_test"),
        None,
        "a test being deleted cannot be read"
    );
    let refused = catalog
        .put_version(
            &acme(),
            &workflow.id,
            &NewVersion {
                document: with_tests(version("v2", &no_labels()), &[("r", &recorded)]),
                pulled: None,
            },
            grace,
        )
        .await;
    assert!(
        matches!(refused, Err(CatalogError::Conflict(_))),
        "a version referencing a test being deleted: {refused:?}"
    );
    assert_eq!(
        catalog
            .reserve_tests(&acme(), std::slice::from_ref(&recorded), grace)
            .await
            .expect("reserve_tests"),
        [recorded.digest],
        "nor reserved again until its row is gone"
    );
    assert_eq!(
        catalog
            .drop_tests(
                &[(acme(), recorded.digest), (acme(), referenced.digest)],
                false
            )
            .await
            .expect("drop_tests"),
        1,
        "only the marked row goes"
    );
    assert_eq!(
        catalog
            .reserve_tests(&acme(), std::slice::from_ref(&recorded), grace)
            .await
            .expect("reserve_tests"),
        Vec::<Digest>::new(),
        "a dropped test is stored anew"
    );
    assert_eq!(
        catalog
            .get_test(&acme(), referenced.digest)
            .await
            .expect("get_test"),
        Some(referenced.clone())
    );
    // A test that left a suite still belongs to it: the versions that passed it name it.
    catalog
        .change_suite(
            &acme(),
            &workflow.id,
            &SuiteChange {
                retired: vec![RetiredTest {
                    name: "a".into(),
                    test: referenced.digest,
                    reason: "deprecated".into(),
                }],
                ..SuiteChange::default()
            },
            later(T0, 2),
        )
        .await
        .expect("change_suite");
    assert!(
        !catalog
            .unreferenced_tests(grace.saturating_add(TEST_GRACE), 10)
            .await
            .expect("unreferenced_tests")
            .contains(&(acme(), referenced.digest)),
        "a retired test is kept"
    );

    let renewed = test_record("renewed", None);
    catalog
        .reserve_tests(&acme(), std::slice::from_ref(&renewed), T0)
        .await
        .expect("reserve_tests");
    catalog
        .reserve_tests(
            &acme(),
            std::slice::from_ref(&renewed),
            later(T0, 3_600_000),
        )
        .await
        .expect("reserve_tests");
    assert!(
        !catalog
            .unreferenced_tests(grace, 10)
            .await
            .expect("unreferenced_tests")
            .contains(&(acme(), renewed.digest)),
        "reserving a test starts its grace again"
    );
}

pub async fn tests_of_an_erased_namespace_go_whatever_references_them<C: CatalogStore>(
    catalog: &C,
) {
    let alice_test = test_record("alice's", Some("alice"));
    let bob_test = test_record("bob's", Some("bob"));
    let written = test_record("written", None);
    catalog
        .reserve_tests(
            &acme(),
            &[alice_test.clone(), bob_test.clone(), written.clone()],
            T0,
        )
        .await
        .expect("reserve_tests");
    let workflow = create(
        catalog,
        &new_workflow(
            1,
            WorkflowScope::Organization,
            Some("triage"),
            with_tests(
                version("v1", &no_labels()),
                &[("a", &alice_test), ("w", &written)],
            ),
        ),
        T0,
    )
    .await;
    assert_eq!(
        catalog
            .namespace_tests(&acme(), &alice(), 10)
            .await
            .expect("namespace_tests"),
        [alice_test.digest]
    );
    assert_eq!(
        catalog
            .namespace_tests(&tenant("other"), &alice(), 10)
            .await
            .expect("namespace_tests"),
        []
    );
    assert_eq!(
        catalog
            .drop_tests(&[(acme(), alice_test.digest)], true)
            .await
            .expect("drop_tests"),
        1,
        "not marked, and referenced: an erasure drops it anyway"
    );
    assert_eq!(
        catalog
            .get_test(&acme(), alice_test.digest)
            .await
            .expect("get_test"),
        None
    );
    let stored = catalog
        .version(&acme(), &workflow.id, VersionRef::Number(1))
        .await
        .expect("version")
        .expect("the version is still stored");
    assert_eq!(
        stored.document.suite().tests.len(),
        2,
        "the version keeps the digest in its document"
    );
    assert_eq!(
        current(&catalog.suite(&acme(), &workflow.id).await.expect("suite")),
        [("w".to_owned(), written.digest)],
        "the suite does not"
    );
    assert_eq!(
        catalog
            .unreferenced_tests(T0.saturating_add(TEST_GRACE), 10)
            .await
            .expect("unreferenced_tests"),
        [(acme(), bob_test.digest)],
        "the suite's other test is still referenced"
    );
}

// -------------------------------------------------------------------------------------------
// Subscriptions and tenants

fn subscription(tenant: TenantId, id: &str, url: &str) -> Subscription {
    Subscription {
        tenant,
        id: id.into(),
        url: url.into(),
        secret: Secret::new("whsec_1"),
        types: BTreeSet::from([WebhookType::RunCompleted]),
        labels: BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
    }
}

pub async fn subscriptions_are_stored_per_tenant<C: CatalogStore>(catalog: &C) {
    for subscription in [
        subscription(acme(), "s2", "https://b"),
        subscription(acme(), "s1", "https://a"),
        subscription(tenant("other"), "s1", "https://c"),
        subscription(acme(), "s2", "https://b2"),
    ] {
        catalog
            .put_subscription(&subscription)
            .await
            .expect("put_subscription");
    }
    assert_eq!(
        catalog.subscriptions(&acme()).await.expect("subscriptions"),
        [
            subscription(acme(), "s1", "https://a"),
            subscription(acme(), "s2", "https://b2")
        ],
        "by id, a second put replacing the first"
    );
    assert!(
        catalog
            .delete_subscription(&acme(), "s1")
            .await
            .expect("delete")
    );
    assert!(
        !catalog
            .delete_subscription(&acme(), "s1")
            .await
            .expect("delete")
    );
    assert_eq!(
        catalog
            .subscriptions(&tenant("other"))
            .await
            .expect("subscriptions")
            .len(),
        1
    );
}

/// `legacy_partition` answers `None` for a run `migrate-runs` did not move: a new catalog holds
/// none.
pub async fn a_run_not_moved_has_no_legacy_partition<C: CatalogStore>(catalog: &C) {
    let run = rewind_core::RunId::from_u128(7);
    assert_eq!(
        catalog
            .legacy_partition(run)
            .await
            .expect("legacy_partition"),
        None
    );
}

/// `put_legacy_run` records where a moved run lives, once: recording it again changes nothing.
pub async fn a_moved_run_is_found_in_its_partition<C: CatalogStore>(catalog: &C) {
    let run = crate::ports::RunRef::new(acme(), rewind_core::RunId::from_u128(8));
    let partition = rewind_core::PartitionId::new(5);
    for _ in 0..2 {
        catalog
            .put_legacy_run(&run, partition)
            .await
            .expect("put_legacy_run");
        assert_eq!(
            catalog
                .legacy_partition(run.id)
                .await
                .expect("legacy_partition"),
            Some(partition)
        );
    }
    assert_eq!(
        catalog
            .legacy_partition(rewind_core::RunId::from_u128(9))
            .await
            .expect("legacy_partition"),
        None
    );
}

pub async fn tenants_are_stored<C: CatalogStore>(catalog: &C) {
    assert_eq!(catalog.get_tenant(&acme()).await.expect("get_tenant"), None);
    let mut record = Tenant::new(acme());
    record.quotas = Quotas {
        cpu_per_month: Some(Duration::from_secs(3_600)),
        active_runs: Some(100),
        runs_per_month: Some(10_000),
        workflows: Some(500),
        max_ttl: Some(Duration::from_secs(7 * 86_400)),
        max_code_lines: Some(2_000),
    };
    record.namespace_defaults = Quotas {
        active_runs: Some(10),
        ..Quotas::default()
    };
    catalog.put_tenant(&record).await.expect("put_tenant");
    catalog
        .put_tenant(&Tenant::new(tenant("beta")))
        .await
        .expect("put_tenant");
    assert_eq!(
        catalog.get_tenant(&acme()).await.expect("get_tenant"),
        Some(record.clone())
    );
    record.quotas = Quotas::default();
    catalog.put_tenant(&record).await.expect("put replaces");
    assert_eq!(
        catalog.get_tenant(&acme()).await.expect("get_tenant"),
        Some(record)
    );
    let ids: Vec<String> = catalog
        .list_tenants()
        .await
        .expect("list_tenants")
        .iter()
        .map(|tenant| tenant.id.to_string())
        .collect();
    assert_eq!(ids, ["acme", "beta"]);
}

// -------------------------------------------------------------------------------------------
// Namespaces

pub async fn namespaces_are_created_at_first_use_in_their_partition<C: CatalogStore>(catalog: &C) {
    assert_eq!(
        catalog
            .get_namespace(&acme(), &alice())
            .await
            .expect("get_namespace"),
        None
    );
    let written = catalog
        .ensure_namespace(&acme(), &alice(), deployment(), T0)
        .await
        .expect("ensure_namespace");
    let NamespaceWrite::Written(first) = written else {
        panic!("a new namespace is written: {written:?}");
    };
    assert_eq!((&first.tenant, &first.name), (&acme(), &alice()));
    assert_eq!(
        first.partition,
        namespace_partition(&acme(), &alice(), deployment()),
        "its partition is hash(tenant, name) mod partitions"
    );
    assert_eq!(first.exceptions, Quotas::default());
    assert_eq!((first.created_at, first.display_name.clone()), (T0, None));
    let again = catalog
        .ensure_namespace(&acme(), &alice(), partitions(7), later(T0, 1))
        .await
        .expect("ensure_namespace");
    assert_eq!(
        again,
        NamespaceWrite::Written(first.clone()),
        "an existing namespace keeps its partition, whatever the count given"
    );

    let exceptions = Quotas {
        active_runs: Some(50),
        cpu_per_month: Some(Duration::from_secs(36_000)),
        ..Quotas::default()
    };
    let changed = catalog
        .put_namespace(
            &acme(),
            &alice(),
            Some(&exceptions),
            partitions(3),
            later(T0, 2),
        )
        .await
        .expect("put_namespace");
    let NamespaceWrite::Written(changed) = changed else {
        panic!("alice is not being erased: {changed:?}");
    };
    assert_eq!(changed.exceptions, exceptions);
    assert_eq!(
        (changed.partition, changed.created_at),
        (first.partition, T0),
        "exceptions are replaced, the partition and the creation kept"
    );
    let kept = catalog
        .put_namespace(&acme(), &alice(), None, deployment(), later(T0, 3))
        .await
        .expect("put_namespace");
    assert!(
        matches!(kept, NamespaceWrite::Written(ref n) if n.exceptions == exceptions),
        "without exceptions, they are left as they are"
    );
    let created = catalog
        .put_namespace(&acme(), &bob(), Some(&exceptions), partitions(1), T0)
        .await
        .expect("put_namespace");
    assert!(
        matches!(created, NamespaceWrite::Written(ref n)
            if n.partition.get() == 0 && n.exceptions == exceptions),
        "put_namespace creates a namespace in its partition: {created:?}"
    );
    assert_eq!(
        catalog
            .get_namespace(&tenant("other"), &alice())
            .await
            .expect("get_namespace"),
        None,
        "namespaces are per tenant"
    );
    assert!(
        catalog
            .revoke_namespace_tokens(&acme(), &alice(), later(T0, 5))
            .await
            .expect("revoke_namespace_tokens")
    );
    assert_eq!(
        catalog
            .get_namespace(&acme(), &alice())
            .await
            .expect("get_namespace")
            .expect("alice")
            .tokens_revoked_before,
        Some(later(T0, 5))
    );
    assert!(
        !catalog
            .revoke_namespace_tokens(&acme(), &namespace("nobody"), T0)
            .await
            .expect("revoke_namespace_tokens")
    );
}

pub async fn namespaces_have_display_names_found_by_prefix<C: CatalogStore>(catalog: &C) {
    let display = |text: &str| DisplayName::new(text).expect("the suites use valid display names");
    let set = |name: NamespaceName, text: Option<&str>| {
        let display_name = text.map(display);
        async move {
            catalog
                .set_namespace_display_name(&acme(), &name, display_name.as_ref())
                .await
                .expect("set_namespace_display_name")
        }
    };
    assert_eq!(
        set(alice(), Some("Acme Corp")).await,
        None,
        "a display name never creates its namespace"
    );
    for name in [alice(), bob(), namespace("carol"), namespace("dave")] {
        ensure(catalog, &acme(), &name).await;
    }
    ensure(catalog, &tenant("other"), &namespace("erin")).await;
    let Some(NamespaceWrite::Written(written)) = set(alice(), Some("Acme Corp")).await else {
        panic!("alice exists");
    };
    assert_eq!(written.display_name, Some(display("Acme Corp")));
    set(bob(), Some("Élodie Martin")).await;
    set(namespace("carol"), Some("acme labs")).await;
    catalog
        .set_namespace_display_name(
            &tenant("other"),
            &namespace("erin"),
            Some(&display("Acme Other")),
        )
        .await
        .expect("set_namespace_display_name");
    assert_eq!(
        catalog
            .namespace_display_names(&acme())
            .await
            .expect("namespace_display_names"),
        [
            (alice(), display("Acme Corp")),
            (bob(), display("Élodie Martin")),
            (namespace("carol"), display("acme labs")),
        ],
        "the tenant's own, by name, those with a display name only"
    );
    let listed = |name: Option<NamespaceName>, prefix: &str| {
        let filter = NamespaceFilter {
            tenant: acme(),
            name,
            prefix: Some(prefix.to_owned()),
            after: None,
            limit: 10,
        };
        async move {
            let page = catalog
                .list_namespaces(&filter)
                .await
                .expect("list_namespaces");
            let names: Vec<String> = page
                .namespaces
                .iter()
                .map(|namespace| namespace.name.to_string())
                .collect();
            (names, page.total)
        }
    };
    assert_eq!(
        listed(None, "ACME").await,
        (vec!["alice".to_owned(), "carol".to_owned()], 2),
        "a display name matches its prefix case aside"
    );
    assert_eq!(listed(None, "élo").await, (vec!["bob".to_owned()], 1));
    assert_eq!(
        listed(None, "Ca").await,
        (Vec::new(), 0),
        "a name matches its prefix case-sensitively"
    );
    assert_eq!(listed(None, "d").await, (vec!["dave".to_owned()], 1));
    assert_eq!(
        listed(Some(bob()), "acme").await,
        (Vec::new(), 0),
        "the name filter and the prefix both apply"
    );
    let Some(NamespaceWrite::Written(cleared)) = set(alice(), None).await else {
        panic!("alice exists");
    };
    assert_eq!(cleared.display_name, None);
    catalog
        .delete_namespace(&acme(), &namespace("carol"), later(T0, 2))
        .await
        .expect("delete_namespace");
    assert_eq!(
        set(namespace("carol"), Some("Renamed")).await,
        Some(NamespaceWrite::Deleting)
    );
}

pub async fn namespaces_are_listed_by_name_in_pages<C: CatalogStore>(catalog: &C) {
    for name in ["dave", "alice", "carol", "bob", "erin"] {
        ensure(catalog, &acme(), &namespace(name)).await;
    }
    ensure(catalog, &tenant("other"), &namespace("aaron")).await;
    let page = |after: Option<&str>, limit: usize| {
        let filter = NamespaceFilter {
            tenant: acme(),
            name: None,
            prefix: None,
            after: after.map(namespace),
            limit,
        };
        async move {
            let NamespacePage { namespaces, total } = catalog
                .list_namespaces(&filter)
                .await
                .expect("list_namespaces");
            let names: Vec<String> = namespaces
                .iter()
                .map(|namespace| namespace.name.to_string())
                .collect();
            (names, total)
        }
    };
    assert_eq!(
        page(None, 2).await,
        (vec!["alice".to_owned(), "bob".to_owned()], 5)
    );
    assert_eq!(
        page(Some("bob"), 2).await,
        (vec!["carol".to_owned(), "dave".to_owned()], 5)
    );
    assert_eq!(page(Some("dave"), 2).await, (vec!["erin".to_owned()], 5));
    assert_eq!(page(Some("erin"), 2).await, (Vec::new(), 5));
    let only = catalog
        .list_namespaces(&NamespaceFilter {
            tenant: acme(),
            name: Some(namespace("carol")),
            prefix: None,
            after: None,
            limit: 10,
        })
        .await
        .expect("list_namespaces");
    assert_eq!(only.total, 1);
    assert_eq!(
        only.namespaces[0].partition,
        namespace_partition(&acme(), &namespace("carol"), deployment()),
        "a namespace lists with its partition"
    );
}

pub async fn an_erased_namespace_is_dropped_once_marked<C: CatalogStore>(catalog: &C) {
    ensure(catalog, &acme(), &alice()).await;
    ensure(catalog, &acme(), &bob()).await;
    assert_eq!(
        catalog
            .drop_namespace(&acme(), &alice())
            .await
            .expect("drop_namespace"),
        DropNamespace::NotFound,
        "a namespace not marked is not dropped"
    );
    assert_eq!(
        catalog
            .delete_namespace(&acme(), &bob(), later(T0, 2))
            .await
            .expect("delete_namespace"),
        DeleteNamespace::Started
    );
    assert_eq!(
        catalog
            .delete_namespace(&acme(), &alice(), later(T0, 3))
            .await
            .expect("delete_namespace"),
        DeleteNamespace::Started
    );
    assert_eq!(
        catalog
            .delete_namespace(&acme(), &alice(), later(T0, 9))
            .await
            .expect("delete_namespace"),
        DeleteNamespace::Started,
        "marking it again keeps the first mark"
    );
    assert_eq!(
        catalog
            .delete_namespace(&acme(), &namespace("nobody"), T0)
            .await
            .expect("delete_namespace"),
        DeleteNamespace::NotFound
    );
    assert_eq!(
        catalog
            .ensure_namespace(&acme(), &alice(), deployment(), later(T0, 4))
            .await
            .expect("ensure_namespace"),
        NamespaceWrite::Deleting
    );
    assert_eq!(
        catalog
            .put_namespace(
                &acme(),
                &alice(),
                Some(&Quotas::default()),
                deployment(),
                later(T0, 4)
            )
            .await
            .expect("put_namespace"),
        NamespaceWrite::Deleting
    );
    assert_eq!(
        catalog
            .namespaces_to_erase(10)
            .await
            .expect("namespaces_to_erase"),
        [(acme(), bob()), (acme(), alice())],
        "the oldest mark first"
    );
    assert_eq!(
        catalog
            .namespaces_to_erase(1)
            .await
            .expect("namespaces_to_erase"),
        [(acme(), bob())]
    );
    let own = create(
        catalog,
        &new_workflow(
            1,
            in_namespace(&alice()),
            Some("triage"),
            version("a", &no_labels()),
        ),
        T0,
    )
    .await;
    let organization = create(
        catalog,
        &new_workflow(
            2,
            WorkflowScope::Organization,
            Some("gym"),
            version("b", &no_labels()),
        ),
        T0,
    )
    .await;
    assert_eq!(
        catalog
            .drop_namespace(&acme(), &alice())
            .await
            .expect("drop_namespace"),
        DropNamespace::Dropped
    );
    assert_eq!(
        catalog
            .get_namespace(&acme(), &alice())
            .await
            .expect("get_namespace"),
        None
    );
    assert_eq!(
        catalog.workflow(&acme(), &own.id).await.expect("workflow"),
        None,
        "its workflows go with it"
    );
    assert_eq!(
        catalog
            .versions(&acme(), &own.id, &no_labels())
            .await
            .expect("versions"),
        []
    );
    assert_eq!(
        catalog
            .workflow_with_code(&acme(), &alice(), Digest::of(b"a"), &no_labels())
            .await
            .expect("workflow_with_code"),
        None,
        "and their codes"
    );
    assert_eq!(
        catalog
            .workflow(&acme(), &organization.id)
            .await
            .expect("workflow"),
        Some(organization),
        "the organization's stay"
    );
    assert_eq!(
        catalog
            .namespaces_to_erase(10)
            .await
            .expect("namespaces_to_erase"),
        [(acme(), bob())]
    );
    assert!(
        matches!(
            catalog
                .ensure_namespace(&acme(), &alice(), deployment(), later(T0, 5))
                .await
                .expect("ensure_namespace"),
            NamespaceWrite::Written(ref n) if n.created_at == later(T0, 5)
        ),
        "the name can be used again"
    );
}

// -------------------------------------------------------------------------------------------
// Changes

async fn next_change<S: Stream<Item = ControlPlaneChange> + Unpin>(
    changes: &mut S,
) -> ControlPlaneChange {
    changes.next().await.expect("a change")
}

pub async fn every_write_sends_its_change<C: CatalogStore, K: Cluster>(catalog: &C, cluster: &K) {
    let member = cluster
        .join(NodeInfo {
            id: NodeId::new("listener").expect("node id"),
            address: "listener.internal:7070".into(),
            build: "build-1".into(),
            epochs: BTreeSet::from([Epoch(1)]),
            pool: None,
            executors_only: false,
        })
        .await
        .expect("join");
    let mut changes = member.changes().await.expect("changes");
    let action_set = ActionSet::from_harness_actions(["a"]).expect("action set");
    catalog
        .put_action_set(&acme(), &action_set, &ActionDeclarations::default())
        .await
        .expect("put_action_set");
    assert_eq!(
        next_change(&mut changes).await,
        ControlPlaneChange::ActionSet {
            tenant: acme(),
            digest: action_set.digest()
        }
    );
    let test = test_record("changed", None);
    catalog
        .reserve_tests(&acme(), std::slice::from_ref(&test), T0)
        .await
        .expect("reserve_tests");
    assert_eq!(
        next_change(&mut changes).await,
        ControlPlaneChange::Test {
            tenant: acme(),
            digest: test.digest
        }
    );
    let workflow_changed = ControlPlaneChange::Workflow {
        tenant: acme(),
        id: id(1),
    };
    let first = with_tests(version("v1", &no_labels()), &[("t", &test)]);
    create(
        catalog,
        &new_workflow(1, in_namespace(&bob()), None, first),
        T0,
    )
    .await;
    assert_eq!(
        next_change(&mut changes).await,
        workflow_changed,
        "a workflow created"
    );
    catalog
        .update_workflow(
            &acme(),
            &id(1),
            &WorkflowPatch {
                name: Some(Some("triage".into())),
                description: None,
            },
            T0,
        )
        .await
        .expect("update_workflow");
    assert_eq!(next_change(&mut changes).await, workflow_changed, "named");
    catalog
        .put_version(
            &acme(),
            &id(1),
            &NewVersion {
                document: version("v2", &no_labels()),
                pulled: None,
            },
            T0,
        )
        .await
        .expect("put_version");
    assert_eq!(
        next_change(&mut changes).await,
        workflow_changed,
        "given a version"
    );
    catalog
        .change_suite(
            &acme(),
            &id(1),
            &SuiteChange {
                added: vec![SuiteTest {
                    name: "t".into(),
                    test: test.digest,
                }],
                ..SuiteChange::default()
            },
            T0,
        )
        .await
        .expect("change_suite");
    assert_eq!(
        next_change(&mut changes).await,
        workflow_changed,
        "given tests"
    );
    catalog
        .promote_workflow(&acme(), &id(1), T0)
        .await
        .expect("promote_workflow");
    assert_eq!(
        next_change(&mut changes).await,
        workflow_changed,
        "promoted"
    );
    catalog
        .put_subscription(&subscription(acme(), "s1", "https://a"))
        .await
        .expect("put_subscription");
    assert_eq!(
        next_change(&mut changes).await,
        ControlPlaneChange::Subscription {
            tenant: acme(),
            id: "s1".into()
        }
    );
    catalog
        .put_tenant(&Tenant::new(acme()))
        .await
        .expect("put_tenant");
    assert_eq!(
        next_change(&mut changes).await,
        ControlPlaneChange::Tenant { tenant: acme() }
    );
    let alice_changed = ControlPlaneChange::Namespace {
        tenant: acme(),
        name: alice(),
    };
    ensure(catalog, &acme(), &alice()).await;
    assert_eq!(
        next_change(&mut changes).await,
        alice_changed,
        "a namespace created"
    );
    catalog
        .put_namespace(
            &acme(),
            &alice(),
            Some(&Quotas::default()),
            deployment(),
            T0,
        )
        .await
        .expect("put_namespace");
    assert_eq!(
        next_change(&mut changes).await,
        alice_changed,
        "its exceptions"
    );
    catalog
        .set_namespace_display_name(
            &acme(),
            &alice(),
            Some(&DisplayName::new("A").expect("name")),
        )
        .await
        .expect("set_namespace_display_name");
    assert_eq!(
        next_change(&mut changes).await,
        alice_changed,
        "its display name"
    );
    catalog
        .revoke_namespace_tokens(&acme(), &alice(), T0)
        .await
        .expect("revoke_namespace_tokens");
    assert_eq!(
        next_change(&mut changes).await,
        alice_changed,
        "its tokens revoked"
    );
    catalog
        .delete_namespace(&acme(), &alice(), T0)
        .await
        .expect("delete_namespace");
    assert_eq!(
        next_change(&mut changes).await,
        alice_changed,
        "its erasure started"
    );
    catalog
        .drop_namespace(&acme(), &alice())
        .await
        .expect("drop_namespace");
    assert_eq!(
        next_change(&mut changes).await,
        alice_changed,
        "its row dropped"
    );
    catalog
        .delete_subscription(&acme(), "s1")
        .await
        .expect("delete_subscription");
    assert_eq!(
        next_change(&mut changes).await,
        ControlPlaneChange::Subscription {
            tenant: acme(),
            id: "s1".into()
        }
    );
}

// -------------------------------------------------------------------------------------------
// What `rename-data` needs beyond the catalog

/// Runs every check of [`CatalogMigration`], each on a new catalog. The titles people gave
/// codes are the schema migration's: each adapter's tests put them as its old tables held them.
pub async fn run_migration<C: CatalogMigration>(new_store: impl Fn() -> C) {
    eprintln!("catalog migration conformance: an_organization_workflow_moves_into_a_namespace");
    an_organization_workflow_moves_into_a_namespace(&new_store()).await;
    eprintln!("catalog migration conformance: titles_are_dropped_once");
    titles_are_dropped_once(&new_store()).await;
}

pub async fn an_organization_workflow_moves_into_a_namespace<C: CatalogMigration>(catalog: &C) {
    let gym = create(
        catalog,
        &new_workflow(
            1,
            WorkflowScope::Organization,
            Some("gym"),
            version("gym", &no_labels()),
        ),
        T0,
    )
    .await;
    let WorkflowWrite::Written(moved) = catalog
        .scope_workflow(&acme(), &gym.id, &alice(), later(T0, 1))
        .await
        .expect("scope_workflow")
    else {
        panic!("not moved");
    };
    assert_eq!(
        (&moved.id, &moved.scope),
        (&gym.id, &in_namespace(&alice()))
    );
    assert_eq!(
        catalog
            .workflow_with_code(&acme(), &alice(), Digest::of(b"gym"), &no_labels())
            .await
            .expect("workflow_with_code")
            .map(|(workflow, _)| workflow.id),
        Some(gym.id.clone()),
        "with its codes"
    );
    assert_eq!(
        catalog
            .workflow_with_code(&acme(), &bob(), Digest::of(b"gym"), &no_labels())
            .await
            .expect("workflow_with_code"),
        None
    );
    assert_eq!(
        catalog
            .scope_workflow(&acme(), &gym.id, &alice(), later(T0, 2))
            .await
            .expect("scope_workflow"),
        WorkflowWrite::Written(moved),
        "already there: as it is"
    );
    create(
        catalog,
        &new_workflow(
            2,
            WorkflowScope::Organization,
            Some("water"),
            version("water", &no_labels()),
        ),
        T0,
    )
    .await;
    create(
        catalog,
        &new_workflow(
            3,
            in_namespace(&bob()),
            Some("water"),
            version("bob water", &no_labels()),
        ),
        T0,
    )
    .await;
    assert_eq!(
        catalog
            .scope_workflow(&acme(), &id(2), &bob(), T0)
            .await
            .expect("scope_workflow"),
        WorkflowWrite::NameTaken
    );
    assert_eq!(
        catalog
            .scope_workflow(&acme(), &id(99), &bob(), T0)
            .await
            .expect("scope_workflow"),
        WorkflowWrite::NotFound
    );
}

pub async fn titles_are_dropped_once<C: CatalogMigration>(catalog: &C) {
    catalog.drop_code_titles().await.expect("drop_code_titles");
    assert_eq!(catalog.code_titles().await.expect("code_titles"), []);
    catalog
        .drop_code_titles()
        .await
        .expect("dropping them again changes nothing");
}
