//! Workflow test suites over HTTP, on Turso: the gate on save, `POST /v1/test` with a workflow's
//! suite, tests recorded from past runs, and the suite a fork of a run of a workflow passes
//! first (`archi/05-testing-and-observability.md`, "Testing and mocks").

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;

use rewind_api::access::BuiltInRole;
use rewind_api::common::RunStatus;
use rewind_api::runs::{ForkRun, StartRun};
use rewind_api::scripts::TestRequest;
use rewind_api::testing::{Test, TestFrom, TestInput, TestProblem};
use rewind_api::toolsets::CreateToolset;
use rewind_api::workflows::{
    CreateWorkflow, RecordTests, TestRetirement, TestUpdate, WorkflowQuery,
};
use rewind_api::{Problem, ProblemType};
use serde_json::json;
use support::{TestServer, eventually, wait};

/// Closes three issues once approved, none otherwise.
const TRIAGE: &str = r#"import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const approval = await signal("approval");
  return { closed: approval.close ? 3 : 0, repo: input.repo };
}
"#;

/// A regression: it closes two.
const TWO: &str = r#"import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const approval = await signal("approval");
  return { closed: approval.close ? 2 : 0, repo: input.repo };
}
"#;

fn problem(error: rewind_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

fn triage(toolset: &str, script: &str) -> CreateWorkflow {
    CreateWorkflow {
        name: "triage".into(),
        description: None,
        toolset: toolset.into(),
        script: script.into(),
        input: json!({
            "type": "object",
            "properties": { "repo": { "type": "string" } },
            "required": ["repo"],
        }),
        output: json!({
            "type": "object",
            "properties": { "closed": { "type": "integer" } },
            "required": ["closed"],
        }),
        signals: BTreeMap::from([(
            "approval".to_owned(),
            json!({
                "type": "object",
                "properties": { "close": { "type": "boolean" } },
                "required": ["close"],
            }),
        )]),
        parent: None,
        tests: Vec::new(),
        update: Vec::new(),
        retire: Vec::new(),
    }
}

/// Approved, it closes three.
fn closes_three() -> Test {
    serde_json::from_value(json!({
        "name": "closes three once approved",
        "input": { "repo": "acme/api" },
        "signals": [{ "name": "approval", "payload": { "close": true } }],
        "expect": { "status": "completed", "output": { "closed": 3 } }
    }))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_version_is_saved_only_when_its_inherited_suite_passes() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let toolset = harness
        .create_toolset(&CreateToolset::default())
        .await
        .unwrap();

    let mut first = triage(&toolset.id, TRIAGE);
    first.tests = vec![TestInput::Inline(Box::new(closes_three()))];
    let v1 = harness.create_workflow(&first).await.unwrap();
    assert_eq!(v1.parent, None);
    assert_eq!(v1.tests.len(), 1);
    assert_eq!(v1.tests[0].name, "closes three once approved");
    let stored = harness.get_test(&v1.tests[0].digest).await.unwrap();
    assert_eq!(stored, closes_three());

    // The regression is refused by the inherited test, and nothing is saved.
    let refused = problem(
        harness
            .create_workflow(&triage(&toolset.id, TWO))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::TestsFailed));
    assert_eq!(refused.status, 422);
    assert_eq!(refused.tests.len(), 1);
    assert_eq!(refused.tests[0].problem, Some(TestProblem::Failed));
    assert_eq!(
        refused.tests[0].failures,
        [
            "the output is {\"closed\":2,\"repo\":\"acme/api\"}, which does not contain {\"closed\":3}"
        ]
    );
    assert!(
        refused
            .detail
            .starts_with("1 of 1 tests of the suite did not pass: \"closes three once approved\""),
        "{}",
        refused.detail
    );

    // Updating the expectation with a reason saves it, its parent the first version.
    let mut second = triage(&toolset.id, TWO);
    second.update = vec![TestUpdate {
        name: "closes three once approved".into(),
        reason: "two issues are kept open for the next sprint".into(),
        test: None,
        expect: Some(
            serde_json::from_value(json!({ "status": "completed", "output": { "closed": 2 } }))
                .unwrap(),
        ),
    }];
    let v2 = harness.create_workflow(&second).await.unwrap();
    assert_eq!(v2.parent.as_deref(), Some(v1.version.as_str()));
    assert_eq!(v2.updated.len(), 1);
    assert_eq!(v2.updated[0].from, v1.tests[0].digest);
    assert_eq!(v2.updated[0].to, v2.tests[0].digest);

    // Retiring needs a reason, and names an inherited test.
    let mut retired = triage(&toolset.id, TRIAGE);
    retired.retire = vec![TestRetirement {
        names: vec!["closes four".into()],
        reason: "gone".into(),
    }];
    assert_eq!(
        problem(harness.create_workflow(&retired).await.unwrap_err()).kind(),
        Some(ProblemType::InvalidTest)
    );
    retired.retire[0].names = vec!["closes three once approved".into()];
    let v3 = harness.create_workflow(&retired).await.unwrap();
    assert!(v3.tests.is_empty());
    assert_eq!(v3.retired[0].reason, "gone");

    // The versions whose script is TRIAGE, newest first: the script's page shows their suites.
    let with_script = |script: &str| WorkflowQuery {
        script: Some(script.to_owned()),
        ..WorkflowQuery::default()
    };
    let page = harness
        .list_workflows(&with_script(&v1.script))
        .await
        .unwrap();
    let versions: Vec<&str> = page.workflows.iter().map(|w| w.version.as_str()).collect();
    assert_eq!(versions, [v3.version.as_str(), v1.version.as_str()]);
    assert_eq!(page.next, None);
    assert!(harness.list_workflows(&with_script("nope")).await.is_err());

    // A test given inline cannot claim to be recorded.
    let mut forged = triage(&toolset.id, TRIAGE);
    let mut test = closes_three();
    test.name = "forged".into();
    test.origin = Some(rewind_api::testing::TestOrigin {
        run: "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10".into(),
        version: v1.version.clone(),
        namespace: "alice".into(),
    });
    forged.tests = vec![TestInput::Inline(Box::new(test))];
    assert_eq!(
        problem(harness.create_workflow(&forged).await.unwrap_err()).kind(),
        Some(ProblemType::InvalidTest)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_test_runs_a_workflows_suite_against_a_candidate() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let toolset = harness
        .create_toolset(&CreateToolset::default())
        .await
        .unwrap();
    let mut first = triage(&toolset.id, TRIAGE);
    first.tests = vec![TestInput::Inline(Box::new(closes_three()))];
    harness.create_workflow(&first).await.unwrap();

    let candidate = |script: &str| TestRequest {
        script: Some(script.into()),
        workflow: Some("triage".into()),
        ..TestRequest::default()
    };
    let run = harness.test(&candidate(TWO)).await.unwrap();
    assert!(!run.passed);
    assert_eq!(run.tests.len(), 1);
    assert_eq!(run.tests[0].status, Some(RunStatus::Completed));
    let run = harness.test(&candidate(TRIAGE)).await.unwrap();
    assert!(run.passed, "{run:?}");

    // A test the interface refuses is not run.
    let mut wrong = closes_three();
    wrong.name = "an undeclared signal".into();
    wrong.signals[0].name = "merged".into();
    let mut request = candidate(TRIAGE);
    request.tests = vec![TestInput::Inline(Box::new(wrong))];
    let run = harness.test(&request).await.unwrap();
    assert_eq!(run.tests[0].problem, Some(TestProblem::Incompatible));
    assert_eq!(run.tests[0].status, None);
    assert!(run.tests[1].passed, "the suite runs after the tests given");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn past_runs_are_recorded_one_per_path_and_join_a_suite_through_a_save() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let toolset = harness
        .create_toolset(&CreateToolset::default())
        .await
        .unwrap();
    harness
        .create_workflow(&triage(&toolset.id, TRIAGE))
        .await
        .unwrap();
    let mut runs = Vec::new();
    for (repo, close) in [("acme/api", true), ("acme/web", true), ("acme/db", false)] {
        let mut start = StartRun::new("");
        start.workflow = Some("triage".into());
        start.input = json!({ "repo": repo });
        let run = harness.start_run(&start, None).await.unwrap();
        harness
            .signal_run(&run.id, "approval", &json!({ "close": close }), None)
            .await
            .unwrap();
        harness.get_run(&run.id, wait("20s")).await.unwrap();
        runs.push(run.id);
    }

    // Approved or not, the runs took one path: the newest is recorded, once the read model
    // has projected the three of them.
    let recorded = eventually(|| async {
        let recorded = harness
            .record_tests("triage", &RecordTests::default())
            .await
            .unwrap();
        (recorded.tests.first().map(|test| &test.run) == Some(&runs[2])).then_some(recorded)
    })
    .await;
    assert_eq!(recorded.tests.len(), 1, "{recorded:?}");
    assert_eq!(recorded.tests[0].run, runs[2]);
    assert_eq!(recorded.tests[0].path, "signal approval → completed");
    let test = harness.get_test(&recorded.tests[0].digest).await.unwrap();
    assert_eq!(test.input, json!({ "repo": "acme/db" }));
    assert_eq!(test.origin.as_ref().unwrap().run, runs[2]);
    assert!(test.world.cassette.is_some());

    // Named runs are recorded whatever their path.
    let named = harness
        .record_tests(
            "triage",
            &RecordTests {
                runs: vec![runs[0].clone()],
                ..RecordTests::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(named.tests[0].run, runs[0]);

    // The recorded tests join a suite by digest, through the gate.
    let mut saved = triage(&toolset.id, TRIAGE);
    saved.description = Some("with its recorded tests".into());
    saved.tests = vec![
        TestInput::Digest(recorded.tests[0].digest.clone()),
        TestInput::Digest(named.tests[0].digest.clone()),
    ];
    let version = harness.create_workflow(&saved).await.unwrap();
    assert_eq!(version.tests.len(), 2);

    // Corrected with `from`, a recorded test pins a new behavior without its cassette.
    let corrected = harness
        .test(&TestRequest {
            script: Some(TWO.into()),
            toolset: Some(toolset.id.clone()),
            tests: vec![TestInput::From(TestFrom {
                from: named.tests[0].digest.clone(),
                expect: serde_json::from_value(json!({ "output": { "closed": 2 } })).unwrap(),
            })],
            workflow: None,
        })
        .await
        .unwrap();
    assert!(corrected.passed, "{corrected:?}");

    // A fork of a run of that version passes its suite first.
    let mut start = StartRun::new("");
    start.workflow = Some("triage".into());
    start.input = json!({ "repo": "acme/api" });
    let run = harness.start_run(&start, None).await.unwrap();
    assert_eq!(run.workflow.as_ref().unwrap().version, version.version);
    harness
        .signal_run(&run.id, "approval", &json!({ "close": true }), None)
        .await
        .unwrap();
    harness.get_run(&run.id, wait("20s")).await.unwrap();
    let mut fork = ForkRun::new(TWO);
    fork.check = Some(false);
    let refused = problem(harness.fork_run(&run.id, &fork, None).await.unwrap_err());
    assert_eq!(refused.kind(), Some(ProblemType::TestsFailed));
    assert!(
        refused.detail.contains("tests of the suite of triage@"),
        "{}",
        refused.detail
    );
    let mut fork = ForkRun::new(TRIAGE);
    fork.check = Some(false);
    harness.fork_run(&run.id, &fork, None).await.unwrap();
}
