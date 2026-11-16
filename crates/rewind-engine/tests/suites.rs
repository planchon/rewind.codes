//! The gate: a version is saved only if its suite, inherited from its parent, passes; tests are
//! retired and updated with a reason; tests recorded from runs, one per path
//! (`archi/05-testing-and-observability.md`, "Suites and the gate", "Tests recorded from runs").

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::BTreeMap;

use rewind_core::{
    ERASED_TEST_REASON, Expectations, ExpectedCall, RunStatus, Script, Test, TestOrigin,
    TestSignal, ToolOutcome, ToolRule, Toolset, WorkflowSchemas,
};
use rewind_engine::DriveOptions;
use rewind_engine::adapters::StaticTypeChecker;
use rewind_engine::ports::{
    Delivery, JournalStore, NamespaceName, StoreError, TenantId, ToolDeclarations,
    WorkflowDeclarations,
};
use rewind_engine::suites::{
    Recorder, SuiteChanges, SuiteError, TestInput, TestRetirement, TestUpdate, UpdateWith,
};
use rewind_engine::testing::TestProblem;
use rewind_engine::{RegisterError, RegisterWorkflow};
use serde_json::json;
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
                Delivery::ToolCallCompleted { .. } => run.complete(json!({ "closed": 1 })),
                Delivery::ToolCallFailed { error, .. } => run.fail("ToolError", &error.message),
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
                Delivery::ToolCallCompleted { .. } => run.complete(json!({ "closed": 0 })),
                Delivery::ToolCallFailed { error, .. } => run.fail("ToolError", &error.message),
                _ => run.fail("Error", "unexpected"),
            },
        )
}

fn toolset() -> Toolset {
    Toolset::from_host_tools(["github.close_issue", "slack.post"]).unwrap()
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
        toolset: toolset().digest(),
        schemas: schemas(),
        labels: BTreeMap::new(),
        declarations: WorkflowDeclarations::default(),
        suite,
    }
}

/// Closes issue 4 of acme/api, and expects one closed.
fn closes() -> Test {
    let mut test = Test::new("closes one");
    test.input = json!({ "repo": "acme/api" });
    test.world.tools.insert(
        "github.close_issue".into(),
        vec![ToolRule::always(ToolOutcome::Result(json!({})))],
    );
    test.expect = Expectations {
        status: Some(RunStatus::Completed),
        output: Some(json!({ "closed": 1 })),
        calls: vec![ExpectedCall {
            tool: "github.close_issue".into(),
            args: Some(json!({ "number": 4 })),
            times: Some(1),
        }],
        ..Expectations::default()
    };
    test
}

async fn setup() -> TestEngine {
    let engine = engine(programs(), RecordingDispatcher::echo());
    engine
        .journal()
        .put_toolset(&TenantId::local(), &toolset(), &ToolDeclarations::default())
        .await
        .unwrap();
    engine
}

#[tokio::test]
async fn a_version_inherits_its_parents_suite_and_is_saved_only_if_it_passes() {
    let engine = setup().await;
    let checker = StaticTypeChecker::default();
    let v1 = engine
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
        engine
            .journal()
            .get_test(&TenantId::local(), digest)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        engine
            .load_test(&TenantId::local(), digest, test_format())
            .await
            .unwrap(),
        Some(closes())
    );

    // v2 regresses: the inherited test refuses it, and nothing is stored.
    let refused = engine
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
    let versions = engine
        .journal()
        .workflow_versions(&TenantId::local(), "triage", &BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(versions.len(), 1, "nothing was stored");

    // Updating the test's expectation, with a reason, lets v2 through.
    let updated = Expectations {
        output: Some(json!({ "closed": 0 })),
        ..closes().expect
    };
    let v2 = engine
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
    let replaced = engine
        .load_test(&TenantId::local(), suite.tests[0].test, test_format())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replaced.expect, updated);
    assert_eq!(replaced.name, "closes one");
}

#[tokio::test]
async fn retiring_and_updating_need_a_reason_and_name_inherited_tests() {
    let engine = setup().await;
    let checker = StaticTypeChecker::default();
    let with = |suite: SuiteChanges| save("v1", suite);
    engine
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
        match engine.register_workflow(request, &checker, &kit()).await {
            Err(RegisterError::Suite(error)) => assert_eq!(error.to_string(), message),
            other => panic!("{message}: {other:?}"),
        }
    }

    // Retired with a reason, the regression is saved, and the reason stays in the lineage.
    let v2 = engine
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
    let engine = setup().await;
    let checker = StaticTypeChecker::default();
    let suite = || SuiteChanges {
        tests: vec![TestInput::Inline(Box::new(closes()))],
        ..SuiteChanges::default()
    };
    let first = engine
        .register_workflow(save("v1", suite()), &checker, &kit())
        .await
        .unwrap();
    let again = engine
        .register_workflow(save("v1", SuiteChanges::default()), &checker, &kit())
        .await
        .unwrap();
    assert_eq!(again, first);
}

#[tokio::test]
async fn tests_of_an_erased_namespace_are_left_out_of_the_next_suite() {
    let engine = setup().await;
    let checker = StaticTypeChecker::default();
    let mut recorded = closes();
    recorded.name = "recorded".into();
    recorded.origin = Some(TestOrigin {
        run: rewind_core::RunId::from_u128(7),
        version: rewind_core::Digest::of(b"v0"),
        namespace: "alice".into(),
    });
    let digests = engine
        .store_tests(&TenantId::local(), &[recorded.clone()], test_format())
        .await
        .unwrap();
    let alice = NamespaceName::new("alice").unwrap();
    assert_eq!(
        engine
            .journal()
            .namespace_tests(&TenantId::local(), &alice, 10)
            .await
            .unwrap(),
        digests
    );
    // Given by digest, with a new expectation.
    let v1 = engine
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
    let v1 = engine
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
    engine
        .journal()
        .drop_tests(&[(TenantId::local(), digests[0])], true)
        .await
        .unwrap();
    let mut next = save("v1", SuiteChanges::default());
    next.description = Some("the same, described".into());
    let v2 = engine
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
    let unknown = engine
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
    let engine = setup().await;
    let digests = engine
        .store_tests(&TenantId::local(), &[closes()], test_format())
        .await
        .unwrap();
    let later = T0.saturating_add(rewind_engine::ports::TEST_GRACE);
    assert_eq!(
        engine
            .journal()
            .unreferenced_tests(later, 10)
            .await
            .unwrap(),
        [(TenantId::local(), digests[0])]
    );
    let refused = engine
        .store_tests(&TenantId::local(), &[closes()], test_format())
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

#[tokio::test]
async fn recording_keeps_the_newest_run_of_each_path() {
    let engine = engine(
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
                Delivery::ToolCallCompleted { seq, .. }
                    if seq.0 >= 2 || run.input["count"] == 1 =>
                {
                    run.complete(json!({ "closed": run.input["count"].clone() }));
                }
                _ => {}
            },
        ),
        RecordingDispatcher::echo(),
    );
    let mut runs = Vec::new();
    for count in [1, 2, 3, 0] {
        let run = engine
            .start_run(request("closer", json!({ "count": count })))
            .await
            .unwrap()
            .run;
        engine
            .run_until_idle_virtual(&DriveOptions::default())
            .await
            .unwrap();
        runs.push(run);
    }
    // The run waiting for approval never ends: it is not recorded.
    let mut recorder = Recorder::new(20, test_format());
    for run in runs.iter().rev() {
        let history = engine.export_history(run).await.unwrap();
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
