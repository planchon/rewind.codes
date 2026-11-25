//! The workflow model (`archi/01-product.md`, "Workflows"; `archi/03-execution.md`, "Starting a
//! run"): a run given code is a run of the workflow that has it in its namespace or the
//! organization, else of a new workflow of its namespace; a workflow is named, given versions,
//! promoted, forked and pulled; the quota `workflows` counts workflows, not codes.

#![allow(clippy::unwrap_used)]

use crate::support;

use std::collections::BTreeMap;

use rewind_core::{
    ActionSet, Code, Expectations, RunStatus, ServerSecrets, Test, TestFromRun, WorkflowSchemas,
};
use rewind_engine::adapters::StaticTypeChecker;
use rewind_engine::ports::{
    ActionDeclarations, CatalogStore, NamespaceName, QuotaKind, Quotas, TenantId, WorkflowPatch,
    WorkflowScope, WorkflowSource, current_suite,
};
use rewind_engine::suites::{SuiteChanges, TestInput};
use rewind_engine::{
    CreateWorkflow, ForkWorkflow, MakeVersion, PullWorkflow, StartError, StartRun, StartWorkflow,
    WorkflowError, WrittenWorkflow,
};
use serde_json::json;
use support::plane::World;
use support::{FakeExecutor, RecordingDispatcher, maker, request, workflow_kit};

fn done(output: i64) -> impl Fn(&mut support::FakeRun) + Send + Sync + 'static {
    move |run: &mut support::FakeRun| run.complete(json!(output))
}

/// Codes that complete at once: `one` with 1, `two` with 2.
fn programs() -> FakeExecutor {
    FakeExecutor::new()
        .with("one", done(1), |_, _| {})
        .with("two", done(2), |_, _| {})
}

fn ns(name: &str) -> NamespaceName {
    NamespaceName::new(name).unwrap()
}

fn tenant() -> TenantId {
    TenantId::local()
}

fn action_set() -> ActionSet {
    ActionSet::from_harness_actions(["a", "b", "cleanup"]).unwrap()
}

fn in_namespace(code: &str, namespace: &str) -> StartRun {
    let mut start = request(code, json!({}));
    start.namespace = ns(namespace);
    start
}

async fn setup() -> World {
    let world = World::new(programs(), RecordingDispatcher::echo()).await;
    world
        .plane
        .catalog
        .put_action_set(&tenant(), &action_set(), &ActionDeclarations::default())
        .await
        .unwrap();
    world
}

/// An organization's workflow `name` of `code`, with an interface.
async fn organization(
    world: &World,
    name: &str,
    code: &str,
    tests: Vec<TestInput>,
) -> WrittenWorkflow {
    world
        .edge
        .create_workflow(
            CreateWorkflow {
                tenant: tenant(),
                scope: WorkflowScope::Organization,
                name: Some(name.into()),
                description: Some(format!("The {name} workflow.")),
                code: Code::new(code),
                action_set: action_set().digest(),
                interface: Some(WorkflowSchemas {
                    input: json!({ "type": "object" }),
                    output: json!({ "type": "integer" }),
                    signals: BTreeMap::new(),
                }),
                secrets: ServerSecrets::new(),
                tests,
                maker: maker(),
            },
            &workflow_kit(&StaticTypeChecker::default()),
        )
        .await
        .unwrap()
}

/// Expects the output `output` of a run given `{}`.
fn outputs(name: &str, output: i64) -> Test {
    let mut test = Test::new(name);
    test.input = json!({});
    test.expect = Expectations {
        status: Some(RunStatus::Completed),
        output: Some(json!(output)),
        ..Expectations::default()
    };
    test
}

#[tokio::test]
async fn a_run_given_new_code_creates_a_workflow_of_its_namespace() {
    let mut world = setup().await;
    let mut start = in_namespace("one", "alice");
    start.metadata.allowed_labels = BTreeMap::from([("team".to_owned(), "core".to_owned())]);
    start.metadata.labels = start.metadata.allowed_labels.clone();
    start.principal = "member:user_1".into();
    let first = world.run(start.clone()).await;
    let of_first = world.view(&first).await.head.metadata.workflow.unwrap();
    assert_eq!(
        (of_first.name.clone(), of_first.version, of_first.interface),
        (None, 1, false)
    );
    let workflow = world
        .plane
        .catalog
        .workflow(&tenant(), &of_first.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(workflow.scope, WorkflowScope::Namespace(ns("alice")));
    assert_eq!(workflow.created_by, "member:user_1");
    let version = world
        .plane
        .catalog
        .versions(&tenant(), &of_first.id, &BTreeMap::new())
        .await
        .unwrap()
        .remove(0);
    // Version 1: no interface, the run's action set, secrets and labels.
    let document = &version.document;
    assert_eq!(document.interface(), None);
    assert_eq!(document.action_set(), action_set().digest());
    assert_eq!(document.secrets(), &start.metadata.secrets);
    assert_eq!(document.labels(), &start.metadata.allowed_labels);
    assert_eq!(version.digest, of_first.digest);

    // The same code again in the namespace is the same workflow; elsewhere another one.
    let again = world.run(start).await;
    assert_eq!(
        world.view(&again).await.head.metadata.workflow.unwrap().id,
        of_first.id
    );
    let bob = world.run(in_namespace("one", "bob")).await;
    assert_ne!(
        world.view(&bob).await.head.metadata.workflow.unwrap().id,
        of_first.id
    );
    // Other code is another workflow.
    let two = world.run(in_namespace("two", "alice")).await;
    assert_ne!(
        world.view(&two).await.head.metadata.workflow.unwrap().id,
        of_first.id
    );
}

#[tokio::test]
async fn a_run_given_the_code_of_an_organization_workflow_is_a_run_of_it() {
    let mut world = setup().await;
    let org = organization(&world, "gym", "one", Vec::new()).await;
    let run = world.run(in_namespace("one", "alice")).await;
    let of = world.view(&run).await.head.metadata.workflow.unwrap();
    assert_eq!(
        (of.id, of.name.as_deref(), of.version),
        (org.workflow.id.clone(), Some("gym"), 1)
    );
    assert_eq!(world.view(&run).await.head.origin, "gym");
    // A run of it by name, in any namespace.
    let mut gateway = world.driver.gateway(&world.at.clone());
    let started = world
        .edge
        .start_workflow(
            &mut gateway,
            StartWorkflow {
                tenant: tenant(),
                namespace: ns("bob"),
                reference: "gym".into(),
                input: json!({}),
                ttl: None,
                metadata: Default::default(),
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    world.idle().await;
    let of = world
        .view(&started.run)
        .await
        .head
        .metadata
        .workflow
        .unwrap();
    assert_eq!(of.id, org.workflow.id);
    assert!(of.interface);
}

#[tokio::test]
async fn a_start_that_creates_a_workflow_counts_toward_the_namespace_s_workflows() {
    let mut world = setup().await;
    world
        .plane
        .catalog
        .put_namespace(
            &tenant(),
            &ns("alice"),
            Some(&Quotas {
                workflows: Some(1),
                ..Quotas::default()
            }),
            world.partitions,
            world.now(),
        )
        .await
        .unwrap();
    world.run(in_namespace("one", "alice")).await;
    // The same workflow again is not a new one.
    world.run(in_namespace("one", "alice")).await;
    let refused = world.start(in_namespace("two", "alice")).await;
    let Err(StartError::QuotaExceeded(exceeded)) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!(exceeded.quota, QuotaKind::Workflows);
    assert_eq!((exceeded.limit, exceeded.used), (1, 1));
}

#[tokio::test]
async fn a_workflow_is_named_versioned_and_promoted_without_changing_its_id() {
    let mut world = setup().await;
    let checker = StaticTypeChecker::default();
    let kit = workflow_kit(&checker);
    let run = world.run(in_namespace("one", "alice")).await;
    let id = world.view(&run).await.head.metadata.workflow.unwrap().id;
    let workflow = world
        .plane
        .catalog
        .workflow(&tenant(), &id)
        .await
        .unwrap()
        .unwrap();
    let named = world
        .edge
        .update_workflow(
            &tenant(),
            &workflow,
            &WorkflowPatch {
                name: Some(Some("counter".into())),
                description: Some(Some("Counts.".into())),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (named.id.clone(), named.name.as_deref(), named.latest),
        (id.clone(), Some("counter"), 1)
    );
    assert!(matches!(
        world
            .edge
            .update_workflow(
                &tenant(),
                &workflow,
                &WorkflowPatch {
                    name: Some(Some("wf_nope".into())),
                    description: None,
                },
            )
            .await,
        Err(WorkflowError::Invalid(_))
    ));
    // Its next version: new code, the latest's action set, an interface given.
    let next = world
        .edge
        .make_version(
            MakeVersion {
                tenant: tenant(),
                workflow: named.clone(),
                code: Some(Code::new("two")),
                action_set: None,
                interface: None,
                secrets: None,
                suite: SuiteChanges::default(),
                maker: maker(),
            },
            &kit,
        )
        .await
        .unwrap();
    assert_eq!(
        (next.workflow.id.clone(), next.version.version, next.created),
        (id.clone(), 2, true)
    );
    // The runs of its namespace given that code are runs of version 2; its runs by name too.
    let by_code = world.run(in_namespace("two", "alice")).await;
    assert_eq!(
        world
            .view(&by_code)
            .await
            .head
            .metadata
            .workflow
            .unwrap()
            .version,
        2
    );
    assert_eq!(world.view(&by_code).await.head.origin, "counter");
    // Promoted: the organization's, with its id; another namespace's runs given its code run it.
    let promoted = world
        .edge
        .promote_workflow(&tenant(), &next.workflow)
        .await
        .unwrap();
    assert_eq!(
        (promoted.id.clone(), promoted.scope.clone()),
        (id.clone(), WorkflowScope::Organization)
    );
    let bob = world.run(in_namespace("two", "bob")).await;
    assert_eq!(
        world.view(&bob).await.head.metadata.workflow.unwrap().id,
        id
    );
}

#[tokio::test]
async fn a_fork_keeps_its_source_s_name_and_pulls_its_new_versions_through_its_suite() {
    let mut world = setup().await;
    let checker = StaticTypeChecker::default();
    let kit = workflow_kit(&checker);
    // A test recorded from bob's runs, and one written by hand.
    let mut recorded = outputs("recorded", 1);
    recorded.from = Some(TestFromRun {
        run: rewind_core::RunId::from_u128(9),
        version: rewind_core::Digest::of(b"v"),
        namespace: "bob".into(),
    });
    let gym = organization(
        &world,
        "gym",
        "one",
        vec![
            TestInput::Inline(Box::new(outputs("outputs one", 1))),
            TestInput::Inline(Box::new(recorded)),
        ],
    )
    .await;
    // A namespace's workflow is not forkable.
    let alice_run = world.run(in_namespace("two", "alice")).await;
    let alice_id = world
        .view(&alice_run)
        .await
        .head
        .metadata
        .workflow
        .unwrap()
        .id;
    let own = world
        .plane
        .catalog
        .workflow(&tenant(), &alice_id)
        .await
        .unwrap()
        .unwrap();
    let fork = |source| ForkWorkflow {
        tenant: tenant(),
        source,
        version: None,
        namespace: ns("alice"),
        maker: maker(),
    };
    assert!(matches!(
        world.edge.fork_workflow(fork(own)).await,
        Err(WorkflowError::NotForkable(_))
    ));
    let forked = world
        .edge
        .fork_workflow(fork(gym.workflow.clone()))
        .await
        .unwrap();
    assert_eq!(forked.workflow.name.as_deref(), Some("gym"));
    assert_eq!(forked.workflow.scope, WorkflowScope::Namespace(ns("alice")));
    assert_eq!(
        forked.workflow.source,
        Some(WorkflowSource {
            workflow: gym.workflow.id.clone(),
            version: 1
        })
    );
    let suite = world
        .plane
        .catalog
        .suite(&tenant(), &forked.workflow.id)
        .await
        .unwrap();
    assert_eq!(
        current_suite(&suite)
            .into_iter()
            .map(|test| test.name)
            .collect::<Vec<_>>(),
        ["outputs one"],
        "without the tests recorded from another namespace's runs"
    );
    // The namespace's runs that name `gym` get the fork; another namespace's the source.
    assert_eq!(
        world
            .edge
            .find_workflow(&tenant(), Some(&ns("alice")), &BTreeMap::new(), "gym")
            .await
            .unwrap()
            .id,
        forked.workflow.id
    );
    assert_eq!(
        world
            .edge
            .find_workflow(&tenant(), Some(&ns("bob")), &BTreeMap::new(), "gym")
            .await
            .unwrap()
            .id,
        gym.workflow.id
    );
    assert!(matches!(
        world.edge.fork_workflow(fork(gym.workflow.clone())).await,
        Err(WorkflowError::NameTaken(_))
    ));

    // Nothing to pull: the fork as it is.
    let pull = || PullWorkflow {
        tenant: tenant(),
        fork: forked.workflow.clone(),
        maker: maker(),
    };
    let unchanged = world.edge.pull_workflow(pull(), &kit).await.unwrap();
    assert!(!unchanged.created);
    // The source's next version breaks the fork's test: nothing changes.
    let mut replace = outputs("outputs one", 2);
    replace.name = "outputs one".into();
    let v2 = world
        .edge
        .make_version(
            MakeVersion {
                tenant: tenant(),
                workflow: gym.workflow.clone(),
                code: Some(Code::new("two")),
                action_set: None,
                interface: None,
                secrets: None,
                suite: SuiteChanges {
                    replace: vec![rewind_engine::suites::TestReplacement {
                        name: "outputs one".into(),
                        reason: "it outputs two now".into(),
                        with: rewind_engine::suites::ReplaceWith::Expect(replace.expect.clone()),
                    }],
                    retire: vec![rewind_engine::suites::TestRetirement {
                        names: vec!["recorded".into()],
                        reason: "bob's run outputs one".into(),
                    }],
                    ..SuiteChanges::default()
                },
                maker: maker(),
            },
            &kit,
        )
        .await
        .unwrap();
    assert_eq!(v2.version.version, 2);
    let refused = world.edge.pull_workflow(pull(), &kit).await;
    assert!(
        matches!(refused, Err(WorkflowError::TestsFailed(_))),
        "{refused:?}"
    );
    // Once the fork's suite allows it, the source's version becomes the fork's next.
    world
        .edge
        .change_tests(
            rewind_engine::ChangeTests {
                tenant: tenant(),
                workflow: forked.workflow.clone(),
                suite: SuiteChanges {
                    retire: vec![rewind_engine::suites::TestRetirement {
                        names: vec!["outputs one".into()],
                        reason: "we follow the source".into(),
                    }],
                    ..SuiteChanges::default()
                },
                maker: maker(),
            },
            &kit,
        )
        .await
        .unwrap();
    let pulled = world.edge.pull_workflow(pull(), &kit).await.unwrap();
    assert_eq!((pulled.version.version, pulled.created), (2, true));
    assert_eq!(pulled.version.document.code(), Code::new("two").digest());
    assert_eq!(
        pulled.workflow.source,
        Some(WorkflowSource {
            workflow: gym.workflow.id.clone(),
            version: 2
        })
    );
    // A workflow that is no fork has nothing to pull.
    assert!(matches!(
        world
            .edge
            .pull_workflow(
                PullWorkflow {
                    tenant: tenant(),
                    fork: gym.workflow.clone(),
                    maker: maker(),
                },
                &kit,
            )
            .await,
        Err(WorkflowError::NotAFork(_))
    ));
}
