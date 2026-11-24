//! A new workflow version and the cleanup of tests race for the version's test, from two
//! servers sharing one database: either the version is stored and its test kept, or the test is
//! marked for deletion and the version refused. A version never references a test whose blob may
//! be deleted. Versions of one workflow saved at once take distinct numbers.

mod support;

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use rewind_core::{Digest, Suite, SuiteTest, Timestamp, Workflow, WorkflowSchemas};
use rewind_engine::ports::{CatalogError, CatalogStore, TEST_GRACE, TenantId, TestRecord};
use serde_json::json;
use support::Postgres;

fn now() -> Timestamp {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_millis();
    Timestamp(u64::try_from(ms).expect("in range"))
}

/// Version `n` of a workflow whose suite is `test`.
fn version_with_test(n: usize, test: Digest) -> Workflow {
    Workflow::new(
        "triage",
        None,
        Digest::of(format!("script {n}").as_bytes()),
        Digest::of(b"action_set"),
        WorkflowSchemas {
            input: json!({ "type": "object" }),
            output: json!({}),
            signals: BTreeMap::new(),
        },
    )
    .expect("a valid workflow")
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
    let reserved_at = now();
    let due = reserved_at.saturating_add(TEST_GRACE);
    let (mut stored, mut refused) = (0, 0);
    // One test at a time: the tests of the rounds before are referenced or being deleted, so
    // the cleanup can only mark this round's.
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
        let version = version_with_test(n, test.digest);
        let (put, marked) = tokio::join!(
            writer.put_workflow(&tenant, &version, due),
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
            .get_workflow(&tenant, version.version())
            .await
            .expect("get_workflow")
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
            Err(err) => panic!("round {n}: put_workflow: {err}"),
        }
    }
    eprintln!("{stored} versions stored, {refused} refused");
}

/// Versions of one workflow saved at once, from two servers, take the numbers 1 to n, each
/// once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn versions_saved_at_once_take_distinct_numbers() {
    const VERSIONS: usize = 20;
    let Some(postgres) = Postgres::start("numbers") else {
        return;
    };
    let database = postgres.create_database();
    let first = postgres.connect(&database).await.catalog();
    let second = postgres.connect(&database).await.catalog();
    let tenant = TenantId::new("acme").expect("tenant");
    let puts = futures_util::future::join_all((0..VERSIONS).map(|n| {
        let catalog = if n % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        let tenant = tenant.clone();
        async move {
            let version = Workflow::new(
                "triage",
                None,
                Digest::of(format!("script {n}").as_bytes()),
                Digest::of(b"action_set"),
                WorkflowSchemas {
                    input: json!({ "type": "object" }),
                    output: json!({}),
                    signals: BTreeMap::new(),
                },
            )
            .expect("a valid workflow");
            catalog
                .put_workflow(&tenant, &version, now())
                .await
                .expect("put_workflow")
                .number
        }
    }))
    .await;
    let mut numbers = puts.clone();
    numbers.sort_unstable();
    assert_eq!(
        numbers,
        (1..=u32::try_from(VERSIONS).expect("in range")).collect::<Vec<u32>>(),
        "{puts:?}"
    );
}
