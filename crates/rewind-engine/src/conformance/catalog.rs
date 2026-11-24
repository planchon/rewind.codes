//! The conformance suite of `CatalogStore`: scripts and action sets by digest, workflow versions
//! and their order, the tests and the suites that reference them, subscriptions, tenants,
//! namespaces with their partition, display names, listing and erasure, and the change every
//! write sends.
//!
//! The factory returns the catalog and the cluster its changes reach, over the same backend.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU16;
use std::time::Duration;

use futures_util::{Stream, StreamExt as _};
use rewind_core::{
    ActionSet, Digest, Epoch, Script, Suite, SuiteTest, Timestamp, Workflow, WorkflowSchemas,
};
use serde_json::json;

use super::{T0, tenant};
use crate::ports::catalog::{NamespaceFilter, NamespacePage};
use crate::ports::cluster::Membership;
use crate::ports::{
    ActionDeclarations, CatalogError, CatalogStore, Cluster, CompiledScript, ControlPlaneChange,
    DeleteNamespace, DisplayName, DropNamespace, NamespaceName, NamespaceWrite, NodeId, NodeInfo,
    Quotas, ScriptTitle, Secret, Subscription, TEST_GRACE, Tenant, TenantId, TestRecord,
    WebhookEventType, WorkflowFilter, namespace_partition,
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
    catalog_check!(scripts_and_action_sets_are_stored_by_tenant_and_digest);
    catalog_check!(workflow_versions_are_stored_by_tenant_and_listed_newest_first);
    catalog_check!(scripts_take_the_titles_people_give_them);
    catalog_check!(workflow_versions_are_numbered_per_workflow);
    catalog_check!(tests_are_kept_while_a_version_references_them);
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

fn workflow(name: &str, script: &str) -> Workflow {
    Workflow::new(
        name,
        Some(format!("The {name} workflow")),
        Digest::of(script.as_bytes()),
        action_set_digest(),
        WorkflowSchemas {
            input: json!({ "type": "object" }),
            output: json!({}),
            signals: BTreeMap::from([("approval".to_owned(), json!({ "type": "boolean" }))]),
        },
    )
    .expect("a valid workflow")
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn test_record(name: &str, namespace: Option<&str>) -> TestRecord {
    TestRecord {
        digest: Digest::of(name.as_bytes()),
        namespace: namespace.map(self::namespace),
        size: 100,
    }
}

fn with_tests(workflow: Workflow, tests: &[&TestRecord]) -> Workflow {
    workflow
        .with_suite(Suite {
            parent: None,
            tests: tests
                .iter()
                .enumerate()
                .map(|(position, test)| SuiteTest {
                    name: format!("test {position}"),
                    test: test.digest,
                })
                .collect(),
            retired: Vec::new(),
            updated: Vec::new(),
        })
        .expect("a valid suite")
}

async fn ensure<C: CatalogStore>(catalog: &C, tenant: &TenantId, name: &NamespaceName) {
    let written = catalog
        .ensure_namespace(tenant, name, deployment(), T0)
        .await
        .expect("ensure_namespace");
    assert!(matches!(written, NamespaceWrite::Written(_)), "{written:?}");
}

// -------------------------------------------------------------------------------------------
// Scripts, action sets, workflows

pub async fn scripts_and_action_sets_are_stored_by_tenant_and_digest<C: CatalogStore>(catalog: &C) {
    let other = tenant("other");
    let script = Script::new("export default () => 1");
    let compiled = CompiledScript {
        javascript: "export default () => 1".into(),
        source_map: "{}".into(),
    };
    assert_eq!(
        catalog
            .get_script(&acme(), script.digest())
            .await
            .expect("get_script"),
        None
    );
    catalog
        .put_script(&acme(), &script, &compiled)
        .await
        .expect("put_script");
    let recompiled = CompiledScript {
        javascript: "changed".into(),
        source_map: String::new(),
    };
    catalog
        .put_script(&acme(), &script, &recompiled)
        .await
        .expect("put_script is idempotent");
    let stored = catalog
        .get_script(&acme(), script.digest())
        .await
        .expect("get_script")
        .expect("stored");
    assert_eq!(
        (stored.script, stored.compiled),
        (script.clone(), compiled),
        "the first put wins"
    );
    assert_eq!(
        catalog
            .get_script(&other, script.digest())
            .await
            .expect("get_script"),
        None,
        "another tenant does not see it"
    );

    let action_set = ActionSet::from_host_tools(["a", "b"]).expect("action set");
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
    let second = ActionSet::from_host_tools(["c"]).expect("action set");
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

pub async fn workflow_versions_are_stored_by_tenant_and_listed_newest_first<C: CatalogStore>(
    catalog: &C,
) {
    let none = BTreeMap::new();
    let scoped = labels(&[("conversation", "c-1")]);
    let first = workflow("triage", "v1");
    let second = workflow("triage", "v2").with_labels(scoped.clone());
    let third = workflow("triage", "v3");
    let digest = workflow("digest", "d1").with_labels(scoped.clone());
    let put = |workflow: Workflow, at: Timestamp| async move {
        catalog
            .put_workflow(&acme(), &workflow, at)
            .await
            .expect("put_workflow")
    };
    let stored = put(first.clone(), T0).await;
    assert_eq!(
        (&stored.workflow, &stored.labels, stored.created_at),
        (&first, &none, T0)
    );
    put(second.clone(), later(T0, 10)).await;
    put(digest.clone(), later(T0, 20)).await;
    put(third.clone(), later(T0, 30)).await;
    let again = put(first.clone(), later(T0, 40)).await;
    assert_eq!(
        (&again.labels, again.created_at),
        (&none, T0),
        "a version is immutable: a second put returns the first"
    );
    let first_scoped = first.clone().with_labels(scoped.clone());
    assert_ne!(first_scoped.version(), first.version());
    let stored = put(first_scoped.clone(), later(T0, 5)).await;
    assert_eq!(
        (&stored.workflow, &stored.labels, stored.created_at),
        (&first_scoped, &scoped, later(T0, 5))
    );
    catalog
        .put_workflow(
            &tenant("other"),
            &workflow("triage", "elsewhere"),
            later(T0, 50),
        )
        .await
        .expect("put_workflow");

    let found = catalog
        .get_workflow(&acme(), second.version())
        .await
        .expect("get_workflow")
        .expect("the version is stored");
    assert_eq!(
        (&found.workflow, &found.labels, found.created_at),
        (&second, &scoped, later(T0, 10))
    );
    assert_eq!(
        catalog
            .get_workflow(&tenant("other"), second.version())
            .await
            .expect("get_workflow"),
        None,
        "another tenant's version"
    );

    let versions = |labels: BTreeMap<String, String>| async move {
        catalog
            .workflow_versions(&acme(), "triage", &labels)
            .await
            .expect("workflow_versions")
            .into_iter()
            .map(|stored| stored.workflow)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        versions(none.clone()).await,
        [
            third.clone(),
            second.clone(),
            first_scoped.clone(),
            first.clone()
        ],
        "newest first"
    );
    assert_eq!(
        versions(scoped.clone()).await,
        [second.clone(), first_scoped.clone()],
        "only the versions carrying the labels"
    );

    let latest = |labels: BTreeMap<String, String>, after: Option<String>, limit: Option<usize>| async move {
        catalog
            .latest_workflows(&WorkflowFilter {
                tenant: acme(),
                labels,
                after,
                limit,
            })
            .await
            .expect("latest_workflows")
            .into_iter()
            .map(|stored| stored.workflow)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        latest(none.clone(), None, None).await,
        [digest.clone(), third.clone()],
        "the latest version of each workflow, by name"
    );
    assert_eq!(
        latest(scoped.clone(), None, None).await,
        [digest.clone(), second.clone()]
    );
    assert_eq!(
        latest(none.clone(), None, Some(1)).await,
        std::slice::from_ref(&digest)
    );
    assert_eq!(
        latest(none.clone(), Some("digest".into()), None).await,
        std::slice::from_ref(&third)
    );

    let shared = workflow("digest", "v3").with_labels(scoped.clone());
    put(shared.clone(), later(T0, 60)).await;
    let with_script = |script: &str, labels: BTreeMap<String, String>| {
        let script = Digest::of(script.as_bytes());
        async move {
            catalog
                .workflows_with_script(&acme(), script, &labels)
                .await
                .expect("workflows_with_script")
                .into_iter()
                .map(|stored| stored.workflow)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(
        with_script("v3", none.clone()).await,
        [shared.clone(), third],
        "newest first, across workflows"
    );
    assert_eq!(with_script("v3", scoped).await, [shared]);
    assert_eq!(with_script("v1", none.clone()).await, [first_scoped, first]);
    assert_eq!(with_script("nothing", none).await, []);
}

// -------------------------------------------------------------------------------------------
// Tests

/// A stored script takes the title and description a person gives it, for its tenant; a new
/// title replaces the last, `None` removes it, and a script the tenant does not have takes none.
pub async fn scripts_take_the_titles_people_give_them<C: CatalogStore>(catalog: &C) {
    let compiled = CompiledScript {
        javascript: "export default () => 1".into(),
        source_map: "{}".into(),
    };
    let titled = Script::new("// Triage\nexport default () => 1");
    let plain = Script::new("export default () => 2");
    for script in [&titled, &plain] {
        catalog
            .put_script(&acme(), script, &compiled)
            .await
            .expect("put_script");
    }
    let title = |text: &str, description: Option<&str>| ScriptTitle {
        title: text.into(),
        description: description.map(str::to_owned),
        updated_by: "user:user_1".into(),
        updated_at: T0,
    };
    let scripts = [titled.digest(), plain.digest()];
    assert_eq!(
        catalog
            .script_titles(&acme(), &scripts)
            .await
            .expect("script_titles"),
        []
    );
    let first = title("Triage new issues", Some("Labels each new issue."));
    assert!(
        catalog
            .set_script_title(&acme(), titled.digest(), Some(&first))
            .await
            .expect("set_script_title")
    );
    let second = title("Morning digest", None);
    assert!(
        catalog
            .set_script_title(&acme(), titled.digest(), Some(&second))
            .await
            .expect("set_script_title")
    );
    assert_eq!(
        catalog
            .script_titles(&acme(), &scripts)
            .await
            .expect("script_titles"),
        [(titled.digest(), second.clone())],
        "the last title given"
    );
    assert_eq!(
        catalog
            .script_titles(&tenant("other"), &scripts)
            .await
            .expect("script_titles"),
        [],
        "another tenant's scripts have none"
    );
    assert!(
        !catalog
            .set_script_title(&tenant("other"), titled.digest(), Some(&first))
            .await
            .expect("set_script_title"),
        "a script the tenant does not have"
    );
    assert!(
        !catalog
            .set_script_title(&acme(), Digest::of(b"missing"), None)
            .await
            .expect("set_script_title")
    );
    assert!(
        catalog
            .set_script_title(&acme(), titled.digest(), None)
            .await
            .expect("set_script_title")
    );
    assert_eq!(
        catalog
            .script_titles(&acme(), &scripts)
            .await
            .expect("script_titles"),
        [],
        "removed"
    );
}

/// A new version takes the next number of its workflow in its tenant; a version stored again
/// keeps its number; `workflow_number` finds a version by its number, whatever its labels.
pub async fn workflow_versions_are_numbered_per_workflow<C: CatalogStore>(catalog: &C) {
    let first = workflow("triage", "t1").with_labels(labels(&[("team", "a")]));
    let stored = catalog
        .put_workflow(&acme(), &first, T0)
        .await
        .expect("put_workflow");
    assert_eq!(stored.number, 1);
    let second = catalog
        .put_workflow(&acme(), &workflow("triage", "t2"), later(T0, 1))
        .await
        .expect("put_workflow");
    assert_eq!(second.number, 2);
    let digest = catalog
        .put_workflow(&acme(), &workflow("digest", "d1"), later(T0, 2))
        .await
        .expect("put_workflow");
    assert_eq!(digest.number, 1, "another workflow counts its own");
    let theirs = catalog
        .put_workflow(&tenant("other"), &workflow("triage", "x1"), T0)
        .await
        .expect("put_workflow");
    assert_eq!(theirs.number, 1, "another tenant counts its own");
    assert_eq!(
        catalog
            .put_workflow(&acme(), &first, later(T0, 3))
            .await
            .expect("put_workflow")
            .number,
        1,
        "stored again, it keeps its number"
    );
    assert_eq!(
        catalog
            .get_workflow(&acme(), second.workflow.version())
            .await
            .expect("get_workflow")
            .map(|stored| stored.number),
        Some(2)
    );
    let found = catalog
        .workflow_number(&acme(), "triage", 1)
        .await
        .expect("workflow_number")
        .expect("version 1");
    assert_eq!(found.workflow, first, "whatever its labels");
    assert_eq!(
        catalog
            .workflow_number(&acme(), "triage", 3)
            .await
            .expect("workflow_number"),
        None
    );
    assert_eq!(
        catalog
            .workflow_number(&acme(), "nope", 1)
            .await
            .expect("workflow_number"),
        None
    );
}

pub async fn tests_are_kept_while_a_version_references_them<C: CatalogStore>(catalog: &C) {
    let referenced = test_record("referenced", None);
    let recorded = test_record("recorded", Some("alice"));
    let late = test_record("late", None);
    assert_eq!(
        catalog
            .reserve_tests(&acme(), &[referenced.clone(), recorded.clone()], T0)
            .await
            .expect("reserve_tests"),
        Vec::<Digest>::new()
    );
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
        .put_workflow(
            &acme(),
            &with_tests(workflow("triage", "v0"), &[&referenced, &late]),
            T0,
        )
        .await;
    assert!(
        matches!(refused, Err(CatalogError::Conflict(_))),
        "a version referencing a test without a row: {refused:?}"
    );
    catalog
        .put_workflow(
            &acme(),
            &with_tests(workflow("triage", "v1"), &[&referenced]),
            T0,
        )
        .await
        .expect("put_workflow");

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
        "after it, the test no version references is marked"
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
        .put_workflow(
            &acme(),
            &with_tests(workflow("triage", "v2"), &[&recorded]),
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
        Some(referenced)
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
    let version = with_tests(workflow("triage", "v1"), &[&alice_test, &written]);
    catalog
        .put_workflow(&acme(), &version, T0)
        .await
        .expect("put_workflow");
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
        .get_workflow(&acme(), version.version())
        .await
        .expect("get_workflow")
        .expect("the version is still stored");
    assert_eq!(
        stored.workflow.suite().tests.len(),
        2,
        "the version keeps the digest in its document"
    );
    assert_eq!(
        catalog
            .unreferenced_tests(T0.saturating_add(TEST_GRACE), 10)
            .await
            .expect("unreferenced_tests"),
        [(acme(), bob_test.digest)],
        "the version's other test is still referenced"
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
        events: BTreeSet::from([WebhookEventType::RunCompleted]),
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
        scripts: Some(500),
        max_ttl: Some(Duration::from_secs(7 * 86_400)),
        max_script_lines: Some(2_000),
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
    let script = Script::new("export default () => 2");
    catalog
        .put_script(
            &acme(),
            &script,
            &CompiledScript {
                javascript: "export default () => 2".into(),
                source_map: "{}".into(),
            },
        )
        .await
        .expect("put_script");
    assert_eq!(
        next_change(&mut changes).await,
        ControlPlaneChange::Script {
            tenant: acme(),
            digest: script.digest()
        }
    );
    let title = ScriptTitle {
        title: "Triage".into(),
        description: None,
        updated_by: "user:user_1".into(),
        updated_at: T0,
    };
    assert!(
        catalog
            .set_script_title(&acme(), script.digest(), Some(&title))
            .await
            .expect("set_script_title")
    );
    assert_eq!(
        next_change(&mut changes).await,
        ControlPlaneChange::Script {
            tenant: acme(),
            digest: script.digest()
        },
        "a title given"
    );
    assert!(
        catalog
            .set_script_title(&acme(), script.digest(), None)
            .await
            .expect("set_script_title")
    );
    assert_eq!(
        next_change(&mut changes).await,
        ControlPlaneChange::Script {
            tenant: acme(),
            digest: script.digest()
        },
        "a title removed"
    );
    let action_set = ActionSet::from_host_tools(["a"]).expect("action set");
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
    let version = with_tests(workflow("triage", "v1"), &[&test]);
    catalog
        .put_workflow(&acme(), &version, T0)
        .await
        .expect("put_workflow");
    assert_eq!(
        next_change(&mut changes).await,
        ControlPlaneChange::Workflow {
            tenant: acme(),
            name: "triage".into(),
            version: version.version()
        }
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
