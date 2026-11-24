//! Workflow test suites over HTTP, on Turso: the gate on save, `POST /v1/test` with a workflow's
//! suite, tests recorded from past runs, and the suite a fork of a run of a workflow passes
//! first (`archi/05-testing-and-observability.md`, "Test runs").

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use rewind_api::access::BuiltInRole;
use rewind_api::action_sets::CreateActionSet;
use rewind_api::checks::TestRequest;
use rewind_api::common::RunStatus;
use rewind_api::runs::{ForkRun, StartRun};
use rewind_api::testing::{Test, TestFrom, TestInput, TestProblem};
use rewind_api::workflows::{
    ChangeTests, CreateVersion, CreateWorkflow, RecordTests, TestReplacement, TestRetirement,
    WorkflowQuery,
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

fn triage(action_set: &str, code: &str) -> CreateWorkflow {
    CreateWorkflow {
        name: Some("triage".into()),
        description: None,
        action_set: action_set.into(),
        code: code.into(),
        input: Some(json!({
            "type": "object",
            "properties": { "repo": { "type": "string" } },
            "required": ["repo"],
        })),
        output: Some(json!({
            "type": "object",
            "properties": { "closed": { "type": "integer" } },
            "required": ["closed"],
        })),
        signals: Some(std::collections::BTreeMap::from([(
            "approval".to_owned(),
            json!({
                "type": "object",
                "properties": { "close": { "type": "boolean" } },
                "required": ["close"],
            }),
        )])),
        tests: Vec::new(),
        secrets: Default::default(),
    }
}

/// The next version of a workflow: this code, the rest its latest version's.
fn code(code: &str) -> CreateVersion {
    CreateVersion {
        code: Some(code.into()),
        ..CreateVersion::default()
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
async fn a_version_is_made_only_when_the_workflow_s_suite_passes() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let action_set = harness
        .create_action_set(&CreateActionSet::default())
        .await
        .unwrap();

    let mut first = triage(&action_set.id, TRIAGE);
    first.tests = vec![TestInput::Inline(Box::new(closes_three()))];
    let v1 = harness.create_workflow(&first).await.unwrap();
    assert_eq!(v1.version.version, 1);
    assert_eq!(v1.version.base, None);
    assert_eq!(v1.version.tests.len(), 1);
    assert_eq!(v1.version.tests[0].name, "closes three once approved");
    assert_eq!(v1.suite.len(), 1);
    let stored = harness.get_test(&v1.version.tests[0].digest).await.unwrap();
    assert_eq!(stored, closes_three());

    // The regression is refused by the suite, and nothing is made.
    let refused = problem(
        harness
            .create_version("triage", &code(TWO))
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
    assert_eq!(
        harness.get_workflow("triage").await.unwrap().versions.len(),
        1
    );

    // Replacing the expectation with a reason makes it, its base the first version.
    let mut second = code(TWO);
    second.replace = vec![TestReplacement {
        name: "closes three once approved".into(),
        reason: "two issues are kept open for the next sprint".into(),
        new: None,
        expect: Some(
            serde_json::from_value(json!({ "status": "completed", "output": { "closed": 2 } }))
                .unwrap(),
        ),
    }];
    let v2 = harness.create_version("triage", &second).await.unwrap();
    assert_eq!((v2.id.as_str(), v2.version.version), (v1.id.as_str(), 2));
    let made = &v2.version;
    assert_eq!(made.base.as_deref(), Some(v1.version.digest.as_str()));
    assert_eq!(made.replaced.len(), 1);
    assert_eq!(made.replaced[0].old, v1.version.tests[0].digest);
    assert_eq!(made.replaced[0].new, made.tests[0].digest);

    // Retiring needs a reason, and names a test of the suite.
    let mut retired = code(TRIAGE);
    retired.retire = vec![TestRetirement {
        names: vec!["closes four".into()],
        reason: "gone".into(),
    }];
    assert_eq!(
        problem(
            harness
                .create_version("triage", &retired)
                .await
                .unwrap_err()
        )
        .kind(),
        Some(ProblemType::InvalidTest)
    );
    retired.retire[0].names = vec!["closes three once approved".into()];
    let v3 = harness.create_version("triage", &retired).await.unwrap();
    assert_eq!(v3.version.version, 3);
    assert!(v3.version.tests.is_empty());
    assert_eq!(v3.version.retired[0].reason, "gone");
    // The suite keeps every test it held, with why each left.
    assert_eq!(v3.suite.len(), 2);
    assert!(v3.suite.iter().all(|test| test.retired_at.is_some()));

    // Tests join the suite on their own, against the latest version, without a version.
    let mut fails = closes_three();
    fails.name = "closes two".into();
    fails.expect = serde_json::from_value(json!({ "output": { "closed": 2 } })).unwrap();
    let refused = problem(
        harness
            .change_tests(
                "triage",
                &ChangeTests {
                    tests: vec![TestInput::Inline(Box::new(fails))],
                    ..ChangeTests::default()
                },
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::TestsFailed));
    let joined = harness
        .change_tests(
            "triage",
            &ChangeTests {
                tests: vec![TestInput::Inline(Box::new(closes_three()))],
                ..ChangeTests::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(joined.version.version, 3, "no version");
    assert!(
        joined
            .suite
            .iter()
            .any(|test| test.name == "closes three once approved" && test.retired_at.is_none())
    );

    // The workflows one of whose versions has TRIAGE.
    let with_code = |code: &str| WorkflowQuery {
        code: Some(code.to_owned()),
        ..WorkflowQuery::default()
    };
    let page = harness
        .list_workflows(&with_code(&v1.version.code_digest))
        .await
        .unwrap();
    let ids: Vec<&str> = page.workflows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, [v1.id.as_str()]);
    assert_eq!(page.workflows[0].latest.version, 3);
    assert_eq!(page.next, None);
    assert!(harness.list_workflows(&with_code("nope")).await.is_err());

    // A test given inline cannot claim to be recorded.
    let mut forged = code(TRIAGE.replace("? 3", "? 3 ").as_str());
    let mut test = closes_three();
    test.name = "forged".into();
    test.from = Some(rewind_api::testing::TestFromRun {
        run: "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10".into(),
        version: v1.version.digest.clone(),
        namespace: "alice".into(),
    });
    forged.tests = vec![TestInput::Inline(Box::new(test))];
    assert_eq!(
        problem(harness.create_version("triage", &forged).await.unwrap_err()).kind(),
        Some(ProblemType::InvalidTest)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_limited_to_labels_sees_the_versions_made_under_them() {
    let server = TestServer::start(&[]).await;
    let tenant_wide = server.key(BuiltInRole::Harness, &[]).await;
    let dev = server.key(BuiltInRole::Harness, &[("env", "dev")]).await;
    let action_set = tenant_wide
        .create_action_set(&CreateActionSet::default())
        .await
        .unwrap();
    let mut first = triage(&action_set.id, TRIAGE);
    first.tests = vec![TestInput::Inline(Box::new(closes_three()))];
    let tenant_wide_triage = tenant_wide.create_workflow(&first).await.unwrap();
    // A version made tenant-wide carries no label: env=dev does not see it.
    assert_eq!(
        problem(dev.get_workflow("triage").await.unwrap_err()).status,
        404
    );
    assert_eq!(
        problem(dev.get_workflow(&tenant_wide_triage.id).await.unwrap_err()).status,
        404
    );

    // env=dev's workflow, which the tenant-wide key sees; its next version, made tenant-wide,
    // passes its suite, and env=dev does not see it.
    let mut ours = triage(&action_set.id, TRIAGE);
    ours.name = Some("dev_triage".into());
    ours.tests = vec![TestInput::Inline(Box::new(closes_three()))];
    let ours = dev.create_workflow(&ours).await.unwrap();
    assert_eq!(ours.labels.get("env").map(String::as_str), Some("dev"));
    let refused = problem(
        tenant_wide
            .create_version("dev_triage", &code(TWO))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::TestsFailed));
    let mut retired = code(TWO);
    retired.retire = vec![TestRetirement {
        names: vec!["closes three once approved".into()],
        reason: "closes two now".into(),
    }];
    let v2 = tenant_wide
        .create_version("dev_triage", &retired)
        .await
        .unwrap();
    assert_eq!(v2.version.version, 2);
    let seen = dev.get_workflow("dev_triage").await.unwrap();
    assert_eq!(seen.version.version, 1);
    assert_eq!(
        seen.versions
            .iter()
            .map(|version| version.version)
            .collect::<Vec<_>>(),
        [1]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn post_test_runs_a_workflows_suite_against_a_candidate() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let action_set = harness
        .create_action_set(&CreateActionSet::default())
        .await
        .unwrap();
    let mut first = triage(&action_set.id, TRIAGE);
    first.tests = vec![TestInput::Inline(Box::new(closes_three()))];
    harness.create_workflow(&first).await.unwrap();

    let candidate = |code: &str| TestRequest {
        code: Some(code.into()),
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
async fn past_runs_are_recorded_one_per_path_and_join_a_suite_through_the_gate() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let action_set = harness
        .create_action_set(&CreateActionSet::default())
        .await
        .unwrap();
    harness
        .create_workflow(&triage(&action_set.id, TRIAGE))
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

    // Approved or not, the runs took one path: the newest is recorded, once the three of them
    // are found.
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
    assert_eq!(test.from.as_ref().unwrap().run, runs[2]);
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

    // The recorded tests join the suite by digest, through the gate, with a version that lists
    // them.
    let version = harness
        .create_version(
            "triage",
            &CreateVersion {
                tests: vec![
                    TestInput::Digest(recorded.tests[0].digest.clone()),
                    TestInput::Digest(named.tests[0].digest.clone()),
                ],
                ..CreateVersion::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(version.version.version, 2);
    assert_eq!(version.version.tests.len(), 2);

    // Corrected with `from`, a recorded test pins a new behavior without its cassette.
    let corrected = harness
        .test(&TestRequest {
            code: Some(TWO.into()),
            action_set: Some(action_set.id.clone()),
            tests: vec![TestInput::From(TestFrom {
                test: named.tests[0].digest.clone(),
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
    assert_eq!(
        run.workflow.as_ref().unwrap().digest,
        version.version.digest
    );
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
        refused.detail.contains("tests of the suite of triage@v2"),
        "{}",
        refused.detail
    );
    let mut fork = ForkRun::new(TRIAGE);
    fork.check = Some(false);
    harness.fork_run(&run.id, &fork, None).await.unwrap();
}
