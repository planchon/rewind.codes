//! The gate: a version is made only if the workflow's suite passes; tests are retired and
//! replaced with a reason; tests added alone; tests recorded from runs, one per path
//! (`archi/05-testing-and-observability.md`, "Suites and the gate", "Tests recorded from runs").

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::BTreeMap;

use rewind_core::{
    ActionOutcome, ActionRule, ActionSet, Code, ERASED_TEST_REASON, Expectations, ExpectedCall,
    RunStatus, Test, TestFromRun, TestSignal, WorkflowSchemas,
};
use rewind_engine::adapters::StaticTypeChecker;
use rewind_engine::edge::{RecordFrom, RecordingError};
use rewind_engine::ports::BlobStore as _;
use rewind_engine::ports::{
    ActionDeclarations, CatalogStore, Handoff, NamespaceName, StoreError, TenantId,
};
use rewind_engine::ports::{WorkflowScope, current_suite};
use rewind_engine::suites::{
    Recorder, ReplaceWith, SuiteChanges, SuiteError, TestInput, TestReplacement, TestRetirement,
};
use rewind_engine::testing::TestProblem;
use rewind_engine::{ChangeTests, CreateWorkflow, MakeVersion, WorkflowError, WrittenWorkflow};
use serde_json::json;
use support::plane::World;
use support::*;

/// `close` takes `{ repo }` and completes with `{ closed: 1 }` once `close` answered; `close2`
/// closes twice; `close-quiet` closes once and says nothing to `notify`… The workflow's
/// versions are these programs.
fn programs() -> FakeExecutor {
    FakeExecutor::new()
        .with(
            "v1",
            |run| {
                let repo = run.input["repo"].clone();
                run.call("github.close_issue", json!({ "repo": repo, "number": 4 }));
            },
            |run, delivery| match delivery {
                Handoff::CallCompleted { .. } => run.complete(json!({ "closed": 1 })),
                Handoff::CallFailed { error, .. } => run.fail("ActionError", &error.message),
                _ => run.fail("Error", "unexpected"),
            },
        )
        .with(
            "v2",
            |run| {
                let repo = run.input["repo"].clone();
                run.call("github.close_issue", json!({ "repo": repo, "number": 4 }));
            },
            |run, delivery| match delivery {
                // A regression: it reports nothing closed.
                Handoff::CallCompleted { .. } => run.complete(json!({ "closed": 0 })),
                Handoff::CallFailed { error, .. } => run.fail("ActionError", &error.message),
                _ => run.fail("Error", "unexpected"),
            },
        )
}

fn action_set() -> ActionSet {
    ActionSet::from_harness_actions(["github.close_issue", "slack.post"]).unwrap()
}

fn schemas() -> WorkflowSchemas {
    WorkflowSchemas {
        input: json!({ "type": "object", "required": ["repo"] }),
        output: json!({ "type": "object", "required": ["closed"] }),
        signals: BTreeMap::new(),
    }
}

/// Makes the next version of the organization's `triage` from `code`, through the gate: its
/// first version when it has none yet, whose suite is the tests given.
async fn save(
    world: &World,
    code: &str,
    suite: SuiteChanges,
) -> Result<WrittenWorkflow, WorkflowError> {
    save_with(world, code, None, suite).await
}

async fn save_with(
    world: &World,
    code: &str,
    interface: Option<WorkflowSchemas>,
    suite: SuiteChanges,
) -> Result<WrittenWorkflow, WorkflowError> {
    let checker = StaticTypeChecker::default();
    let kit = workflow_kit(&checker);
    let existing = world
        .plane
        .catalog
        .workflow_named(&TenantId::local(), None, "triage", &BTreeMap::new())
        .await
        .unwrap();
    match existing {
        None => {
            assert!(suite.replace.is_empty() && suite.retire.is_empty());
            world
                .edge
                .create_workflow(
                    CreateWorkflow {
                        tenant: TenantId::local(),
                        scope: WorkflowScope::Organization,
                        name: Some("triage".into()),
                        description: None,
                        code: Code::new(code),
                        action_set: action_set().digest(),
                        interface: Some(interface.unwrap_or_else(schemas)),
                        secrets: Default::default(),
                        tests: suite.tests,
                        maker: maker(),
                    },
                    &kit,
                )
                .await
        }
        Some(workflow) => {
            world
                .edge
                .make_version(
                    MakeVersion {
                        tenant: TenantId::local(),
                        workflow,
                        code: Some(Code::new(code)),
                        action_set: None,
                        interface,
                        secrets: None,
                        suite,
                        maker: maker(),
                    },
                    &kit,
                )
                .await
        }
    }
}

/// The versions of `triage`, newest first.
async fn versions(
    world: &World,
    written: &WrittenWorkflow,
) -> Vec<rewind_engine::ports::StoredVersion> {
    world
        .plane
        .catalog
        .versions(&TenantId::local(), &written.workflow.id, &BTreeMap::new())
        .await
        .unwrap()
}

/// Closes issue 4 of acme/api, and expects one closed.
fn closes() -> Test {
    let mut test = Test::new("closes one");
    test.input = json!({ "repo": "acme/api" });
    test.world.actions.insert(
        "github.close_issue".into(),
        vec![ActionRule::always(ActionOutcome::Result(json!({})))],
    );
    test.expect = Expectations {
        status: Some(RunStatus::Completed),
        output: Some(json!({ "closed": 1 })),
        calls: vec![ExpectedCall {
            action: "github.close_issue".into(),
            args: Some(json!({ "number": 4 })),
            times: Some(1),
        }],
        ..Expectations::default()
    };
    test
}

async fn setup() -> World {
    let world = World::new(programs(), RecordingDispatcher::echo()).await;
    world
        .plane
        .catalog
        .put_action_set(
            &TenantId::local(),
            &action_set(),
            &ActionDeclarations::default(),
        )
        .await
        .unwrap();
    world
}

#[tokio::test]
async fn a_version_passes_the_workflow_s_suite_and_records_it() {
    let world = setup().await;
    let v1 = save(
        &world,
        "v1",
        SuiteChanges {
            tests: vec![TestInput::Inline(Box::new(closes()))],
            ..SuiteChanges::default()
        },
    )
    .await
    .unwrap();
    let suite = v1.version.document.suite().clone();
    assert_eq!(suite.base, None);
    assert_eq!(suite.tests.len(), 1);
    assert_eq!(suite.tests[0].name, "closes one");
    // The test is stored, under the digest the version holds, and is the workflow's suite.
    let digest = suite.tests[0].test;
    assert!(
        world
            .plane
            .catalog
            .get_test(&TenantId::local(), digest)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        world
            .edge
            .suites(test_format())
            .load_test(&TenantId::local(), digest)
            .await
            .unwrap(),
        Some(closes())
    );
    let members = world
        .plane
        .catalog
        .suite(&TenantId::local(), &v1.workflow.id)
        .await
        .unwrap();
    assert_eq!(current_suite(&members), suite.tests);

    // v2 regresses: the suite refuses it, and nothing is stored.
    let refused = save(&world, "v2", SuiteChanges::default()).await;
    let Err(WorkflowError::TestsFailed(reports)) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].name, "closes one");
    assert_eq!(reports[0].problem, Some(TestProblem::Failed));
    assert_eq!(
        reports[0].failures,
        ["the output is {\"closed\":0}, which does not contain {\"closed\":1}"]
    );
    assert_eq!(versions(&world, &v1).await.len(), 1, "nothing was stored");

    // Replacing the test's expectation, with a reason, lets v2 through.
    let updated = Expectations {
        output: Some(json!({ "closed": 0 })),
        ..closes().expect
    };
    let v2 = save(
        &world,
        "v2",
        SuiteChanges {
            replace: vec![TestReplacement {
                name: "closes one".into(),
                reason: "closed issues are counted elsewhere now".into(),
                with: ReplaceWith::Expect(updated.clone()),
            }],
            ..SuiteChanges::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        (v2.workflow.id.clone(), v2.version.version),
        (v1.workflow.id.clone(), 2)
    );
    let suite = v2.version.document.suite();
    assert_eq!(suite.base, Some(v1.version.digest));
    assert_eq!(suite.replaced.len(), 1);
    assert_eq!(suite.replaced[0].old, digest);
    assert_eq!(suite.replaced[0].new, suite.tests[0].test);
    assert_eq!(
        suite.replaced[0].reason,
        "closed issues are counted elsewhere now"
    );
    let replaced = world
        .edge
        .suites(test_format())
        .load_test(&TenantId::local(), suite.tests[0].test)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replaced.expect, updated);
    assert_eq!(replaced.name, "closes one");
    let members = world
        .plane
        .catalog
        .suite(&TenantId::local(), &v2.workflow.id)
        .await
        .unwrap();
    assert_eq!(current_suite(&members), suite.tests);
    assert_eq!(
        members.len(),
        2,
        "the replaced test left the suite with its reason"
    );
    assert_eq!(
        members
            .iter()
            .find(|member| member.digest == digest)
            .and_then(|member| member.reason.clone()),
        Some("closed issues are counted elsewhere now".to_owned())
    );
}

#[tokio::test]
async fn retiring_and_replacing_need_a_reason_and_name_tests_of_the_suite() {
    let world = setup().await;
    save(
        &world,
        "v1",
        SuiteChanges {
            tests: vec![TestInput::Inline(Box::new(closes()))],
            ..SuiteChanges::default()
        },
    )
    .await
    .unwrap();
    let retire = |names: &[&str], reason: &str| TestRetirement {
        names: names.iter().map(|name| (*name).to_owned()).collect(),
        reason: reason.into(),
    };
    let cases = [
        (
            SuiteChanges {
                retire: vec![retire(&["closes one"], " ")],
                ..SuiteChanges::default()
            },
            "the test \"closes one\" is retired or replaced without a reason: say why",
        ),
        (
            SuiteChanges {
                retire: vec![retire(&["closes two"], "gone")],
                ..SuiteChanges::default()
            },
            "retire names \"closes two\", which the workflow's suite does not have (it has \
             \"closes one\")",
        ),
        (
            SuiteChanges {
                tests: vec![TestInput::Inline(Box::new(closes()))],
                ..SuiteChanges::default()
            },
            "the suite already has a test named \"closes one\": give the new test another name, \
             or replace the inherited one with a reason",
        ),
        (
            SuiteChanges {
                retire: vec![retire(&["closes one"], "gone")],
                replace: vec![TestReplacement {
                    name: "closes one".into(),
                    reason: "changed".into(),
                    with: ReplaceWith::Expect(Expectations::default()),
                }],
                ..SuiteChanges::default()
            },
            "the test \"closes one\" is both retired and replaced: do one",
        ),
    ];
    for (changes, message) in cases {
        match save(&world, "v2", changes).await {
            Err(WorkflowError::Suite(error)) => assert_eq!(error.to_string(), message),
            other => panic!("{message}: {other:?}"),
        }
    }

    // Retired with a reason, the regression is made a version, and the reason stays in the
    // lineage.
    let v2 = save(
        &world,
        "v2",
        SuiteChanges {
            retire: vec![retire(&["closes one"], "v2 only counts, it closes nothing")],
            ..SuiteChanges::default()
        },
    )
    .await
    .unwrap();
    let suite = v2.version.document.suite();
    assert!(suite.tests.is_empty());
    assert_eq!(suite.retired.len(), 1);
    assert_eq!(suite.retired[0].reason, "v2 only counts, it closes nothing");
}

#[tokio::test]
async fn an_unchanged_version_answers_the_latest() {
    let world = setup().await;
    let first = save(
        &world,
        "v1",
        SuiteChanges {
            tests: vec![TestInput::Inline(Box::new(closes()))],
            ..SuiteChanges::default()
        },
    )
    .await
    .unwrap();
    let again = save(&world, "v1", SuiteChanges::default()).await.unwrap();
    assert_eq!(
        (again.version.clone(), again.created),
        (first.version.clone(), false)
    );
}

#[tokio::test]
async fn a_version_sent_again_answers_the_version_it_made() {
    let world = setup().await;
    let first = || SuiteChanges {
        tests: vec![TestInput::Inline(Box::new(closes()))],
        ..SuiteChanges::default()
    };
    let v1 = save(&world, "v1", first()).await.unwrap();
    let again = save(&world, "v1", first()).await.unwrap();
    assert_eq!(
        (again.version, again.created),
        (v1.version.clone(), false),
        "its test is not added a second time"
    );

    // A version that retires the test and adds another, sent twice: the second answers the
    // first, rather than retiring a test the suite no longer has.
    let second = || {
        let mut renamed = closes();
        renamed.name = "closes issue 4".into();
        SuiteChanges {
            tests: vec![TestInput::Inline(Box::new(renamed))],
            retire: vec![TestRetirement {
                names: vec!["closes one".into()],
                reason: "renamed".into(),
            }],
            ..SuiteChanges::default()
        }
    };
    let other = Some(WorkflowSchemas {
        input: json!({ "type": "object" }),
        ..schemas()
    });
    let v2 = save_with(&world, "v1", other.clone(), second())
        .await
        .unwrap();
    assert_eq!(v2.version.document.suite().base, Some(v1.version.digest));
    let again = save_with(&world, "v1", other, second()).await.unwrap();
    assert_eq!((again.version, again.created), (v2.version, false));
}

#[tokio::test]
async fn tests_added_alone_join_the_suite_without_a_version() {
    let world = setup().await;
    let checker = StaticTypeChecker::default();
    let kit = workflow_kit(&checker);
    let v1 = save(
        &world,
        "v1",
        SuiteChanges {
            tests: vec![TestInput::Inline(Box::new(closes()))],
            ..SuiteChanges::default()
        },
    )
    .await
    .unwrap();
    let change = |suite: SuiteChanges| ChangeTests {
        tenant: TenantId::local(),
        workflow: v1.workflow.clone(),
        suite,
        maker: maker(),
    };
    // A test that does not pass against the latest version is refused.
    let mut wrong = closes();
    wrong.name = "closes two".into();
    wrong.expect.output = Some(json!({ "closed": 2 }));
    let refused = world
        .edge
        .change_tests(
            change(SuiteChanges {
                tests: vec![TestInput::Inline(Box::new(wrong))],
                ..SuiteChanges::default()
            }),
            &kit,
        )
        .await;
    assert!(
        matches!(refused, Err(WorkflowError::TestsFailed(_))),
        "{refused:?}"
    );
    let mut right = closes();
    right.name = "closes four".into();
    let (_, suite) = world
        .edge
        .change_tests(
            change(SuiteChanges {
                tests: vec![TestInput::Inline(Box::new(right))],
                retire: vec![TestRetirement {
                    names: vec!["closes one".into()],
                    reason: "named after its issue".into(),
                }],
                ..SuiteChanges::default()
            }),
            &kit,
        )
        .await
        .unwrap();
    let names: Vec<String> = current_suite(&suite)
        .into_iter()
        .map(|test| test.name)
        .collect();
    assert_eq!(names, ["closes four"]);
    assert_eq!(versions(&world, &v1).await.len(), 1, "no version");
    // The next version lists them, and the retirement made since with its reason.
    let other = Some(WorkflowSchemas {
        input: json!({ "type": "object" }),
        ..schemas()
    });
    let v2 = save_with(&world, "v1", other, SuiteChanges::default())
        .await
        .unwrap();
    let document = v2.version.document.suite();
    assert_eq!(
        document
            .tests
            .iter()
            .map(|test| test.name.as_str())
            .collect::<Vec<_>>(),
        ["closes four"]
    );
    assert_eq!(document.retired.len(), 1);
    assert_eq!(document.retired[0].name, "closes one");
    assert_eq!(document.retired[0].reason, "named after its issue");
}

#[tokio::test]
async fn tests_of_an_erased_namespace_are_left_out_of_the_next_suite() {
    let world = setup().await;
    let mut recorded = closes();
    recorded.name = "recorded".into();
    recorded.from = Some(TestFromRun {
        run: rewind_core::RunId::from_u128(7),
        version: rewind_core::Digest::of(b"v0"),
        namespace: "alice".into(),
    });
    let digests = world
        .edge
        .suites(test_format())
        .store_tests(&TenantId::local(), &[recorded.clone()], world.now())
        .await
        .unwrap();
    let alice = NamespaceName::new("alice").unwrap();
    assert_eq!(
        world
            .plane
            .catalog
            .namespace_tests(&TenantId::local(), &alice, 10)
            .await
            .unwrap(),
        digests
    );
    // Given by digest, with a new expectation: both are named "recorded", the second is a
    // duplicate.
    let v1 = save(
        &world,
        "v1",
        SuiteChanges {
            tests: vec![
                TestInput::Stored(digests[0]),
                TestInput::From {
                    test: digests[0],
                    expect: Expectations::default(),
                },
            ],
            ..SuiteChanges::default()
        },
    )
    .await;
    assert!(matches!(
        v1,
        Err(WorkflowError::Suite(SuiteError::Duplicate(_)))
    ));
    let v1 = save(
        &world,
        "v1",
        SuiteChanges {
            tests: vec![
                TestInput::Stored(digests[0]),
                TestInput::Inline(Box::new(closes())),
            ],
            ..SuiteChanges::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(v1.version.document.suite().tests.len(), 2);

    // Alice's namespace is erased: her test goes.
    world
        .plane
        .catalog
        .drop_tests(&[(TenantId::local(), digests[0])], true)
        .await
        .unwrap();
    let other = Some(WorkflowSchemas {
        input: json!({ "type": "object" }),
        ..schemas()
    });
    let v2 = save_with(&world, "v1", other, SuiteChanges::default())
        .await
        .unwrap();
    let suite = v2.version.document.suite();
    assert_eq!(
        suite
            .tests
            .iter()
            .map(|test| test.name.as_str())
            .collect::<Vec<_>>(),
        ["closes one"]
    );
    assert_eq!(suite.retired.len(), 1);
    assert_eq!(suite.retired[0].name, "recorded");
    assert_eq!(suite.retired[0].reason, ERASED_TEST_REASON);
    // A test unknown by digest cannot be given.
    let unknown = save(
        &world,
        "v1",
        SuiteChanges {
            tests: vec![TestInput::Stored(digests[0])],
            ..SuiteChanges::default()
        },
    )
    .await;
    assert!(matches!(
        unknown,
        Err(WorkflowError::Suite(SuiteError::UnknownTest(_)))
    ));
}

#[tokio::test]
async fn a_test_being_deleted_cannot_be_stored_until_its_row_is_gone() {
    let world = setup().await;
    let digests = world
        .edge
        .suites(test_format())
        .store_tests(&TenantId::local(), &[closes()], world.now())
        .await
        .unwrap();
    let later = T0.saturating_add(rewind_engine::ports::TEST_GRACE);
    assert_eq!(
        world
            .plane
            .catalog
            .unreferenced_tests(later, 10)
            .await
            .unwrap(),
        [(TenantId::local(), digests[0])]
    );
    let refused = world
        .edge
        .suites(test_format())
        .store_tests(&TenantId::local(), &[closes()], world.now())
        .await;
    assert!(
        matches!(
            refused,
            Err(rewind_engine::suites::SuiteLoadError::Store(
                StoreError::Conflict(_)
            ))
        ),
        "{refused:?}"
    );
}

/// Calls `a` `count` times and completes with the count; with a count of 0, waits for
/// `approval`.
fn closer() -> FakeExecutor {
    FakeExecutor::new().with(
        "closer",
        |run| {
            let count = run.input["count"].as_u64().unwrap();
            for number in 0..count {
                run.call("a", json!({ "number": number }));
            }
            if count == 0 {
                run.await_signal("approval");
            }
        },
        |run, delivery| match delivery {
            Handoff::SignalReceived { .. } => run.complete(json!({ "closed": 0 })),
            Handoff::CallCompleted { seq, .. } if seq.0 >= 2 || run.input["count"] == 1 => {
                run.complete(json!({ "closed": run.input["count"].clone() }));
            }
            _ => {}
        },
    )
}

#[tokio::test]
async fn recording_keeps_the_newest_run_of_each_path() {
    let mut world = World::new(closer(), RecordingDispatcher::echo()).await;
    let mut runs = Vec::new();
    for count in [1, 2, 3, 0] {
        runs.push(
            world
                .run(request("closer", json!({ "count": count })))
                .await,
        );
    }
    // The run waiting for approval never ends: it is not recorded.
    let mut recorder = Recorder::new(20, test_format());
    for run in runs.iter().rev() {
        let history = world.history(run).await;
        recorder.offer(
            &history,
            TestFromRun {
                run: run.id,
                version: rewind_core::Digest::of(b"closer"),
                namespace: "alice".into(),
            },
        );
    }
    // Three closes and two closes are one path: the newest is kept.
    let recorded: Vec<(String, String)> = recorder
        .recorded
        .iter()
        .map(|recorded| (recorded.run.to_string(), recorded.path.to_string()))
        .collect();
    assert_eq!(
        recorded,
        [
            (runs[2].id.to_string(), "a×n → completed".to_owned()),
            (runs[0].id.to_string(), "a → completed".to_owned()),
        ]
    );
    assert_eq!(recorder.skipped.len(), 1);
    assert_eq!(recorder.skipped[0].run, runs[3].id);
    assert_eq!(
        recorder.skipped[0].reason,
        "it is active: only a run that completed or failed is recorded"
    );
    // A recorded test passes against the code it was recorded from.
    let test = &recorder.recorded[0].test;
    assert_eq!(test.expect.output, Some(json!({ "closed": 3 })));
    let _: &[TestSignal] = &test.signals;
}

/// The workflow `closer` of the namespace alice, made without tests over the actions `a` and `b`.
async fn closer_workflow(world: &World) -> WrittenWorkflow {
    let action_set = ActionSet::from_harness_actions(["a", "b", "cleanup"]).unwrap();
    world
        .plane
        .catalog
        .put_action_set(
            &TenantId::local(),
            &action_set,
            &ActionDeclarations::default(),
        )
        .await
        .unwrap();
    world
        .edge
        .create_workflow(
            CreateWorkflow {
                tenant: TenantId::local(),
                scope: WorkflowScope::Namespace(NamespaceName::new("alice").unwrap()),
                name: Some("closer".into()),
                description: None,
                code: Code::new("closer"),
                action_set: action_set.digest(),
                interface: Some(WorkflowSchemas {
                    input: json!({ "type": "object" }),
                    output: json!({}),
                    signals: BTreeMap::from([("approval".to_owned(), json!({}))]),
                }),
                secrets: Default::default(),
                tests: Vec::new(),
                maker: maker(),
            },
            &workflow_kit(&StaticTypeChecker::default()),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn the_edge_records_the_workflow_s_ended_runs_it_finds_and_stores_their_tests() {
    let mut world = World::new(closer(), RecordingDispatcher::echo()).await;
    let version = closer_workflow(&world).await;
    let mut runs = Vec::new();
    for count in [1, 2, 3, 0] {
        let mut start = rewind_engine::StartWorkflow {
            tenant: TenantId::local(),
            namespace: NamespaceName::new("alice").unwrap(),
            reference: "closer".into(),
            input: json!({ "count": count }),
            ttl: None,
            metadata: Default::default(),
            idempotency_key: None,
        };
        start.metadata.labels.insert("team".into(), "core".into());
        let mut gateway = world.driver.gateway(&world.at.clone());
        let created = world
            .edge
            .start_workflow(&mut gateway, start)
            .await
            .unwrap();
        runs.push(created.run);
        world.idle().await;
    }
    // A run of another workflow: the same code, run in another namespace, is another workflow.
    let other = world.run(request("closer", json!({ "count": 1 }))).await;

    let scope = rewind_engine::ports::Scope::Tenant(TenantId::local());
    let query = rewind_engine::ports::RunQuery::new(TenantId::local());
    let mut gateway = world.driver.gateway(&world.at.clone());
    let recording = world
        .edge
        .record_tests(
            &mut gateway,
            &version.workflow.id,
            RecordFrom::Search { scope, query },
            20,
            test_format(),
        )
        .await
        .unwrap();
    // Newest first (the highest run id), one per path; the run still waiting is not even a
    // candidate.
    let mut recorded: Vec<_> = recording
        .recorded
        .iter()
        .map(|recorded| (recorded.path.to_string(), recorded.run))
        .collect();
    recorded.sort();
    assert_eq!(
        recorded,
        [
            ("a → completed".to_owned(), runs[0].id),
            ("a×n → completed".to_owned(), runs[1].id.max(runs[2].id)),
        ]
    );
    assert!(
        recording.recorded[0].run > recording.recorded[1].run,
        "newest first"
    );
    assert!(recording.skipped.is_empty(), "{:?}", recording.skipped);
    assert_eq!(recording.digests.len(), 2);
    let stored = world
        .edge
        .suites(test_format())
        .load_test(&TenantId::local(), recording.digests[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored, recording.recorded[0].test);
    let origin = stored.from.unwrap();
    assert_eq!(origin.version, version.version.digest);
    assert_eq!(origin.namespace, "alice");
    assert_eq!(
        world
            .plane
            .catalog
            .namespace_tests(
                &TenantId::local(),
                &NamespaceName::new("alice").unwrap(),
                10
            )
            .await
            .unwrap()
            .len(),
        2,
        "a recorded test is indexed under its namespace"
    );

    // Runs named: a run still waiting is skipped, a run of no workflow refused.
    let mut gateway = world.driver.gateway(&world.at.clone());
    let named = world
        .edge
        .record_tests(
            &mut gateway,
            &version.workflow.id,
            RecordFrom::Runs(vec![runs[3].clone(), runs[1].clone()]),
            20,
            test_format(),
        )
        .await
        .unwrap();
    assert_eq!(named.recorded.len(), 1);
    assert_eq!(named.recorded[0].run, runs[1].id);
    assert_eq!(named.skipped.len(), 1);
    assert_eq!(named.skipped[0].run, runs[3].id);
    let refused = world
        .edge
        .record_tests(
            &mut gateway,
            &version.workflow.id,
            RecordFrom::Runs(vec![other.clone()]),
            20,
            test_format(),
        )
        .await;
    assert!(
        matches!(refused, Err(RecordingError::NotOfWorkflow { run, .. }) if run == other.id),
        "{refused:?}"
    );
}

#[tokio::test]
async fn the_daily_job_deletes_the_tests_no_suite_references_after_their_grace() {
    let world = setup().await;
    let suites = world.edge.suites(test_format());
    let mut kept = closes();
    kept.name = "kept".into();
    let v1 = save(
        &world,
        "v1",
        SuiteChanges {
            tests: vec![TestInput::Inline(Box::new(kept))],
            ..SuiteChanges::default()
        },
    )
    .await
    .unwrap();
    let mut loose = closes();
    loose.name = "loose".into();
    let loose = suites
        .store_tests(&TenantId::local(), &[loose], world.now())
        .await
        .unwrap()[0];
    let tenant = TenantId::local();
    let clean =
        |at| rewind_engine::suites::clean_tests(&world.plane.catalog, &world.plane.blobs, at);
    assert_eq!(clean(world.now()).await.unwrap(), 0, "within its grace");
    let later = T0.saturating_add(rewind_engine::ports::TEST_GRACE);
    assert_eq!(clean(later).await.unwrap(), 1);
    assert_eq!(suites.load_test(&tenant, loose).await.unwrap(), None);
    assert_eq!(
        world
            .plane
            .blobs
            .get(
                &rewind_engine::ports::BlobOwner::Tests(tenant.clone()),
                loose
            )
            .await
            .unwrap(),
        None,
        "its blob is deleted"
    );
    let referenced = v1.version.document.suite().tests[0].test;
    assert!(
        suites
            .load_test(&tenant, referenced)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(clean(later).await.unwrap(), 0);
}
