//! The gate: a version is saved only if its suite, inherited from its parent, passes; tests are
//! retired and updated with a reason; tests recorded from runs, one per path
//! (`archi/05-testing-and-observability.md`, "Suites and the gate", "Tests recorded from runs").

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::BTreeMap;

use rewind_core::{
    ActionOutcome, ActionRule, ActionSet, ERASED_TEST_REASON, Expectations, ExpectedCall,
    RunStatus, Script, Test, TestOrigin, TestSignal, WorkflowSchemas,
};
use rewind_engine::adapters::StaticTypeChecker;
use rewind_engine::edge::{RecordFrom, RecordingError};
use rewind_engine::ports::BlobStore as _;
use rewind_engine::ports::{
    ActionDeclarations, CatalogStore, Delivery, NamespaceName, StoreError, TenantId,
    WorkflowDeclarations,
};
use rewind_engine::suites::{
    Recorder, SuiteChanges, SuiteError, TestInput, TestRetirement, TestUpdate, UpdateWith,
};
use rewind_engine::testing::TestProblem;
use rewind_engine::{RegisterError, RegisterWorkflow};
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
                Delivery::CallCompleted { .. } => run.complete(json!({ "closed": 1 })),
                Delivery::CallFailed { error, .. } => run.fail("ActionError", &error.message),
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
                Delivery::CallCompleted { .. } => run.complete(json!({ "closed": 0 })),
                Delivery::CallFailed { error, .. } => run.fail("ActionError", &error.message),
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

fn save(script: &str, suite: SuiteChanges) -> RegisterWorkflow {
    RegisterWorkflow {
        tenant: TenantId::local(),
        name: "triage".into(),
        description: None,
        script: Script::new(script),
        action_set: action_set().digest(),
        schemas: schemas(),
        labels: BTreeMap::new(),
        secrets: Default::default(),
        attach: Default::default(),
        declarations: WorkflowDeclarations::default(),
        suite,
    }
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
async fn a_version_inherits_its_parents_suite_and_is_saved_only_if_it_passes() {
    let world = setup().await;
    let checker = StaticTypeChecker::default();
    let v1 = world
        .edge
        .register_workflow(
            save(
                "v1",
                SuiteChanges {
                    tests: vec![TestInput::Inline(Box::new(closes()))],
                    ..SuiteChanges::default()
                },
            ),
            &checker,
            &kit(),
        )
        .await
        .unwrap();
    let suite = v1.workflow.suite();
    assert_eq!(suite.parent, None);
    assert_eq!(suite.tests.len(), 1);
    assert_eq!(suite.tests[0].name, "closes one");
    // The test is stored, under the digest the version holds.
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

    // v2 regresses: the inherited test refuses it, and nothing is stored.
    let refused = world
        .edge
        .register_workflow(save("v2", SuiteChanges::default()), &checker, &kit())
        .await;
    let Err(RegisterError::TestsFailed(reports)) = refused else {
        panic!("{refused:?}");
    };
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].name, "closes one");
    assert_eq!(reports[0].problem, Some(TestProblem::Failed));
    assert_eq!(
        reports[0].failures,
        ["the output is {\"closed\":0}, which does not contain {\"closed\":1}"]
    );
    let versions = world
        .plane
        .catalog
        .workflow_versions(&TenantId::local(), "triage", &BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(versions.len(), 1, "nothing was stored");

    // Updating the test's expectation, with a reason, lets v2 through.
    let updated = Expectations {
        output: Some(json!({ "closed": 0 })),
        ..closes().expect
    };
    let v2 = world
        .edge
        .register_workflow(
            save(
                "v2",
                SuiteChanges {
                    update: vec![TestUpdate {
                        name: "closes one".into(),
                        reason: "closed issues are counted elsewhere now".into(),
                        with: UpdateWith::Expect(updated.clone()),
                    }],
                    ..SuiteChanges::default()
                },
            ),
            &checker,
            &kit(),
        )
        .await
        .unwrap();
    let suite = v2.workflow.suite();
    assert_eq!(suite.parent, Some(v1.workflow.version()));
    assert_eq!(suite.updated.len(), 1);
    assert_eq!(suite.updated[0].from, digest);
    assert_eq!(suite.updated[0].to, suite.tests[0].test);
    assert_eq!(
        suite.updated[0].reason,
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
}

#[tokio::test]
async fn retiring_and_updating_need_a_reason_and_name_inherited_tests() {
    let world = setup().await;
    let checker = StaticTypeChecker::default();
    let with = |suite: SuiteChanges| save("v1", suite);
    world
        .edge
        .register_workflow(
            with(SuiteChanges {
                tests: vec![TestInput::Inline(Box::new(closes()))],
                ..SuiteChanges::default()
            }),
            &checker,
            &kit(),
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
            "the test \"closes one\" is retired or updated without a reason: say why",
        ),
        (
            SuiteChanges {
                retire: vec![retire(&["closes two"], "gone")],
                ..SuiteChanges::default()
            },
            "retire names \"closes two\", which the parent's suite does not have (it has \
             \"closes one\")",
        ),
        (
            SuiteChanges {
                tests: vec![TestInput::Inline(Box::new(closes()))],
                ..SuiteChanges::default()
            },
            "the suite already has a test named \"closes one\": give the new test another name, \
             or update the inherited one with a reason",
        ),
        (
            SuiteChanges {
                retire: vec![retire(&["closes one"], "gone")],
                update: vec![TestUpdate {
                    name: "closes one".into(),
                    reason: "changed".into(),
                    with: UpdateWith::Expect(Expectations::default()),
                }],
                ..SuiteChanges::default()
            },
            "the test \"closes one\" is both retired and updated: do one",
        ),
    ];
    for (changes, message) in cases {
        let mut request = save("v2", changes);
        request.description = Some("v2".into());
        match world
            .edge
            .register_workflow(request, &checker, &kit())
            .await
        {
            Err(RegisterError::Suite(error)) => assert_eq!(error.to_string(), message),
            other => panic!("{message}: {other:?}"),
        }
    }

    // Retired with a reason, the regression is saved, and the reason stays in the lineage.
    let v2 = world
        .edge
        .register_workflow(
            save(
                "v2",
                SuiteChanges {
                    retire: vec![retire(&["closes one"], "v2 only counts, it closes nothing")],
                    ..SuiteChanges::default()
                },
            ),
            &checker,
            &kit(),
        )
        .await
        .unwrap();
    let suite = v2.workflow.suite();
    assert!(suite.tests.is_empty());
    assert_eq!(suite.retired.len(), 1);
    assert_eq!(suite.retired[0].reason, "v2 only counts, it closes nothing");
}

#[tokio::test]
async fn an_unchanged_save_returns_the_version_saved_first() {
    let world = setup().await;
    let checker = StaticTypeChecker::default();
    let suite = || SuiteChanges {
        tests: vec![TestInput::Inline(Box::new(closes()))],
        ..SuiteChanges::default()
    };
    let first = world
        .edge
        .register_workflow(save("v1", suite()), &checker, &kit())
        .await
        .unwrap();
    let again = world
        .edge
        .register_workflow(save("v1", SuiteChanges::default()), &checker, &kit())
        .await
        .unwrap();
    assert_eq!(again, first);
}

#[tokio::test]
async fn a_save_sent_again_returns_the_version_it_made() {
    let world = setup().await;
    let checker = StaticTypeChecker::default();
    let first = || {
        save(
            "v1",
            SuiteChanges {
                tests: vec![TestInput::Inline(Box::new(closes()))],
                ..SuiteChanges::default()
            },
        )
    };
    let v1 = world
        .edge
        .register_workflow(first(), &checker, &kit())
        .await
        .unwrap();
    let again = world
        .edge
        .register_workflow(first(), &checker, &kit())
        .await;
    assert_eq!(again.unwrap(), v1, "its test is not added a second time");

    // A version that retires the test and adds another, sent twice: the second answers the
    // first, rather than retiring a test the version no longer has.
    let second = || {
        let mut renamed = closes();
        renamed.name = "closes issue 4".into();
        let mut request = save(
            "v1",
            SuiteChanges {
                tests: vec![TestInput::Inline(Box::new(renamed))],
                retire: vec![TestRetirement {
                    names: vec!["closes one".into()],
                    reason: "renamed".into(),
                }],
                ..SuiteChanges::default()
            },
        );
        request.description = Some("v2".into());
        request
    };
    let v2 = world
        .edge
        .register_workflow(second(), &checker, &kit())
        .await
        .unwrap();
    assert_eq!(v2.workflow.suite().parent, Some(v1.workflow.version()));
    let again = world
        .edge
        .register_workflow(second(), &checker, &kit())
        .await;
    assert_eq!(again.unwrap(), v2);

    // Named as the parent, the version itself gets the changes, which do not apply to it.
    let mut onto = second();
    onto.suite.parent = Some(v2.workflow.version());
    match world.edge.register_workflow(onto, &checker, &kit()).await {
        Err(RegisterError::Suite(error)) => assert!(
            error.to_string().starts_with("retire names \"closes one\""),
            "{error}"
        ),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn tests_of_an_erased_namespace_are_left_out_of_the_next_suite() {
    let world = setup().await;
    let checker = StaticTypeChecker::default();
    let mut recorded = closes();
    recorded.name = "recorded".into();
    recorded.origin = Some(TestOrigin {
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
    // Given by digest, with a new expectation.
    let v1 = world
        .edge
        .register_workflow(
            save(
                "v1",
                SuiteChanges {
                    tests: vec![
                        TestInput::Stored(digests[0]),
                        TestInput::From {
                            from: digests[0],
                            expect: Expectations::default(),
                        },
                    ],
                    ..SuiteChanges::default()
                },
            ),
            &checker,
            &kit(),
        )
        .await;
    // Both are named "recorded": the second is a duplicate.
    assert!(matches!(
        v1,
        Err(RegisterError::Suite(SuiteError::Duplicate(_)))
    ));
    let v1 = world
        .edge
        .register_workflow(
            save(
                "v1",
                SuiteChanges {
                    tests: vec![
                        TestInput::Stored(digests[0]),
                        TestInput::Inline(Box::new(closes())),
                    ],
                    ..SuiteChanges::default()
                },
            ),
            &checker,
            &kit(),
        )
        .await
        .unwrap();
    assert_eq!(v1.workflow.suite().tests.len(), 2);

    // Alice's namespace is erased: her test goes.
    world
        .plane
        .catalog
        .drop_tests(&[(TenantId::local(), digests[0])], true)
        .await
        .unwrap();
    let mut next = save("v1", SuiteChanges::default());
    next.description = Some("the same, described".into());
    let v2 = world
        .edge
        .register_workflow(next, &checker, &kit())
        .await
        .unwrap();
    let suite = v2.workflow.suite();
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
    let unknown = world
        .edge
        .register_workflow(
            save(
                "v1",
                SuiteChanges {
                    tests: vec![TestInput::Stored(digests[0])],
                    ..SuiteChanges::default()
                },
            ),
            &checker,
            &kit(),
        )
        .await;
    assert!(matches!(
        unknown,
        Err(RegisterError::Suite(SuiteError::UnknownTest(_)))
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
            Delivery::SignalReceived { .. } => run.complete(json!({ "closed": 0 })),
            Delivery::CallCompleted { seq, .. } if seq.0 >= 2 || run.input["count"] == 1 => {
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
            TestOrigin {
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
    // A recorded test passes against the script it was recorded from.
    let test = &recorder.recorded[0].test;
    assert_eq!(test.expect.output, Some(json!({ "closed": 3 })));
    let _: &[TestSignal] = &test.signals;
}

/// The workflow `closer`, saved without tests over the actions `a` and `b`.
async fn closer_workflow(world: &World) -> rewind_engine::ports::StoredWorkflow {
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
    let mut save = save("closer", SuiteChanges::default());
    save.name = "closer".into();
    save.action_set = action_set.digest();
    save.schemas = WorkflowSchemas {
        input: json!({ "type": "object" }),
        output: json!({}),
        signals: BTreeMap::from([("approval".to_owned(), json!({}))]),
    };
    world
        .edge
        .register_workflow(save, &StaticTypeChecker::default(), &kit())
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
            version: version.workflow.version(),
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
    // A run that is no workflow's.
    let other = world.run(request("closer", json!({ "count": 1 }))).await;

    let scope = rewind_engine::ports::Scope::Tenant(TenantId::local());
    let query = rewind_engine::ports::RunQuery::new(TenantId::local());
    let mut gateway = world.driver.gateway(&world.at.clone());
    let recording = world
        .edge
        .record_tests(
            &mut gateway,
            "closer",
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
    let origin = stored.origin.unwrap();
    assert_eq!(origin.version, version.workflow.version());
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
            "closer",
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
            "closer",
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
async fn the_daily_job_deletes_the_tests_no_version_references_after_their_grace() {
    let world = setup().await;
    let suites = world.edge.suites(test_format());
    let mut kept = closes();
    kept.name = "kept".into();
    let v1 = world
        .edge
        .register_workflow(
            save(
                "v1",
                SuiteChanges {
                    tests: vec![TestInput::Inline(Box::new(kept))],
                    ..SuiteChanges::default()
                },
            ),
            &StaticTypeChecker::default(),
            &kit(),
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
    let referenced = v1.workflow.suite().tests[0].test;
    assert!(
        suites
            .load_test(&tenant, referenced)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(clean(later).await.unwrap(), 0);
}
