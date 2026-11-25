//! Writes that race from two servers sharing one database. A new workflow version and the
//! cleanup of tests race for the version's test: either the version is stored and its test kept,
//! or the test is marked for deletion and the version refused. A version never references a test
//! whose blob may be deleted. Versions of one workflow stored at once take distinct numbers.
//! Workflows created at once for one code without a name are one workflow, and with one name,
//! one is created.

use crate::support;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use rewind_core::{Digest, Suite, SuiteTest, Timestamp, WorkflowId, WorkflowVersion};
use rewind_engine::ports::{
    CatalogError, CatalogStore, NamespaceName, NewVersion, NewWorkflow, TEST_GRACE, TenantId,
    TestRecord, VersionRef, WorkflowCreated, WorkflowScope,
};
use rewind_store_postgres::PostgresCatalog;
use support::Postgres;

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_millis();
    Timestamp(u64::try_from(ms).expect("in range"))
}

fn version(n: usize) -> WorkflowVersion {
    WorkflowVersion::new(
        Digest::of(format!("code {n}").as_bytes()),
        Digest::of(b"action_set"),
    )
}

/// Version `n` of a workflow whose suite is `test`.
fn version_with_test(n: usize, test: Digest) -> WorkflowVersion {
    version(n)
        .with_suite(Suite {
            base: None,
            tests: vec![SuiteTest {
                name: "the test".to_owned(),
                test,
            }],
            retired: Vec::new(),
            replaced: Vec::new(),
        })
        .expect("a valid suite")
}

/// The workflow `n` of alice, running `version`.
fn new_workflow(n: u64, name: Option<&str>, version: WorkflowVersion) -> NewWorkflow {
    NewWorkflow {
        id: WorkflowId::from_bits(n),
        scope: WorkflowScope::Namespace(NamespaceName::new("alice").expect("namespace")),
        name: name.map(str::to_owned),
        description: None,
        labels: BTreeMap::new(),
        source: None,
        created_by: "key:k1".into(),
        version,
    }
}

async fn create(catalog: &PostgresCatalog, tenant: &TenantId, workflow: &NewWorkflow) {
    let created = catalog
        .create_workflow(tenant, workflow, now())
        .await
        .expect("create_workflow");
    assert!(
        matches!(created, WorkflowCreated::Created(_)),
        "{created:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_version_and_the_cleanup_of_its_test_never_both_win() {
    const TESTS: usize = 40;
    let Some(postgres) = Postgres::start("tests") else {
        return;
    };
    let database = postgres.create_database();
    let writer = postgres.connect(&database).await.catalog();
    let cleanup = postgres.connect(&database).await.catalog();
    let tenant = TenantId::new("acme").expect("tenant");
    let workflow = new_workflow(1, Some("triage"), version(TESTS));
    create(&writer, &tenant, &workflow).await;
    let reserved_at = now();
    let due = reserved_at.saturating_add(TEST_GRACE);
    let (mut stored, mut refused) = (0, 0);
    // One test at a time: the tests of the rounds before are in the suite, or left it, or are
    // being deleted, so the cleanup can only mark this round's.
    for n in 0..TESTS {
        let test = TestRecord {
            digest: Digest::of(format!("test {n}").as_bytes()),
            namespace: None,
            size: 100,
        };
        writer
            .reserve_tests(&tenant, std::slice::from_ref(&test), reserved_at)
            .await
            .expect("reserve_tests");
        let document = version_with_test(n, test.digest);
        let next = NewVersion {
            document: document.clone(),
            pulled: None,
        };
        let (put, marked) = tokio::join!(
            writer.put_version(&tenant, &workflow.id, &next, due),
            cleanup.unreferenced_tests(due, TESTS),
        );
        let marked = marked
            .expect("unreferenced_tests")
            .contains(&(tenant.clone(), test.digest));
        let kept = writer
            .get_test(&tenant, test.digest)
            .await
            .expect("get_test")
            .is_some();
        let found = writer
            .version(&tenant, &workflow.id, VersionRef::Digest(document.digest()))
            .await
            .expect("version")
            .is_some();
        match put {
            Ok(_) => {
                assert!(
                    kept && !marked && found,
                    "round {n}: stored, yet its test is marked"
                );
                stored += 1;
            }
            Err(CatalogError::Conflict(_)) => {
                assert!(
                    !kept && marked && !found,
                    "round {n}: refused, yet its test is kept"
                );
                refused += 1;
            }
            Err(err) => panic!("round {n}: put_version: {err}"),
        }
    }
    eprintln!("{stored} versions stored, {refused} refused");
}

/// Versions of one workflow stored at once, from two servers, take the numbers after its first,
/// each once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn versions_stored_at_once_take_distinct_numbers() {
    const VERSIONS: usize = 20;
    let Some(postgres) = Postgres::start("numbers") else {
        return;
    };
    let database = postgres.create_database();
    let first = postgres.connect(&database).await.catalog();
    let second = postgres.connect(&database).await.catalog();
    let tenant = TenantId::new("acme").expect("tenant");
    let workflow = new_workflow(1, Some("triage"), version(VERSIONS));
    create(&first, &tenant, &workflow).await;
    let puts = futures_util::future::join_all((0..VERSIONS).map(|n| {
        let catalog = if n % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        let (tenant, id) = (tenant.clone(), workflow.id.clone());
        async move {
            let next = NewVersion {
                document: version(n),
                pulled: None,
            };
            catalog
                .put_version(&tenant, &id, &next, now())
                .await
                .expect("put_version")
                .version
        }
    }))
    .await;
    let mut numbers = puts.clone();
    numbers.sort_unstable();
    assert_eq!(
        numbers,
        (2..=u32::try_from(VERSIONS + 1).expect("in range")).collect::<Vec<u32>>(),
        "{puts:?}"
    );
    assert_eq!(
        first
            .workflow(&tenant, &workflow.id)
            .await
            .expect("workflow")
            .map(|workflow| workflow.latest),
        u32::try_from(VERSIONS + 1).ok()
    );
}

/// Workflows created at once, from two servers, for one code of a namespace and without a name
/// are one: the first created, the others the existing one. Created at once with one name, one
/// is created and the others find the name taken.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflows_created_at_once_for_one_code_or_name_are_one() {
    const CREATIONS: u64 = 12;
    let Some(postgres) = Postgres::start("creations") else {
        return;
    };
    let database = postgres.create_database();
    let first = postgres.connect(&database).await.catalog();
    let second = postgres.connect(&database).await.catalog();
    let tenant = TenantId::new("acme").expect("tenant");
    for name in [None, Some("triage")] {
        let offset = if name.is_some() { 100 } else { 0 };
        let creations = futures_util::future::join_all((0..CREATIONS).map(|n| {
            let catalog = if n % 2 == 0 {
                first.clone()
            } else {
                second.clone()
            };
            let tenant = tenant.clone();
            // The named ones run another code, which the unnamed ones did not create.
            let workflow = new_workflow(offset + n, name, version(usize::from(name.is_some())));
            async move {
                catalog
                    .create_workflow(&tenant, &workflow, now())
                    .await
                    .expect("create_workflow")
            }
        }))
        .await;
        let created: Vec<WorkflowId> = creations
            .iter()
            .filter_map(|outcome| match outcome {
                WorkflowCreated::Created(workflow) => Some(workflow.id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(created.len(), 1, "{name:?}: {creations:?}");
        let others: BTreeSet<String> = creations
            .iter()
            .filter_map(|outcome| match outcome {
                WorkflowCreated::Created(_) => None,
                WorkflowCreated::Existing(workflow) => Some(workflow.id.to_string()),
                WorkflowCreated::NameTaken => Some("name taken".to_owned()),
            })
            .collect();
        let expected = match name {
            None => created[0].to_string(),
            Some(_) => "name taken".to_owned(),
        };
        assert_eq!(others, BTreeSet::from([expected]), "{name:?}");
    }
}
