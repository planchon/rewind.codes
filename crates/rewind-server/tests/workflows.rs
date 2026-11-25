//! Workflows over HTTP, on Turso (`archi/01-product.md`, "Workflows"): a run given code finds or
//! creates its workflow and a check or a test creates none; an organization's workflow pinned in
//! an action set, whose runs a code starts, signals and waits for; runs of a workflow started by
//! name, and the input and signals they refuse; what a key limited to labels sees; names that
//! resolve in the namespace first; forks that pull their source through their suite; promotion.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::collections::BTreeMap;

use rewind_api::access::BuiltInRole;
use rewind_api::action_sets::CreateActionSet;
use rewind_api::checks::{CheckRequest, TestRequest};
use rewind_api::common::RunStatus;
use rewind_api::runs::{RunFilter, StartRun};
use rewind_api::testing::{Test, TestInput};
use rewind_api::workflows::{
    ChangeTests, CreateVersion, CreateWorkflow, PatchWorkflow, TestRetirement, WorkflowQuery,
    WorkflowScope,
};
use rewind_api::{Client, Problem, ProblemType};
use serde_json::json;
use support::{TestServer, eventually, test_namespace, wait};

const TRIAGE: &str = r#"import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const approval = await signal("approval");
  return { closed: approval.close ? 3 : 0, repo: input.repo };
}
"#;

const PARENT: &str = r#"import { triage } from "rewind:workflows";

export default async function main(input: { repo: string }) {
  const run = await triage.start({ repo: input.repo });
  await run.signal("approval", { close: true });
  return { id: run.id, result: await run.result() };
}
"#;

/// A code that ends at once with `n`.
fn returns(n: u32) -> String {
    format!("export default async function main() {{\n  return {{ n: {n} }};\n}}\n")
}

fn problem(error: rewind_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

fn triage(action_set: &str, code: &str) -> CreateWorkflow {
    CreateWorkflow {
        name: Some("triage".into()),
        description: Some("Triage the stale issues of a repository, after approval.".into()),
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
        signals: Some(BTreeMap::from([(
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

/// A workflow of `code`, named `name`, without an interface.
fn named(name: &str, action_set: &str, code: &str) -> CreateWorkflow {
    CreateWorkflow {
        name: Some(name.into()),
        description: None,
        action_set: action_set.into(),
        code: code.into(),
        input: None,
        output: None,
        signals: None,
        tests: Vec::new(),
        secrets: Default::default(),
    }
}

/// A test that expects the output `{ n }`.
fn returns_test(name: &str, n: u32) -> TestInput {
    let test: Test = serde_json::from_value(json!({
        "name": name,
        "input": {},
        "expect": { "status": "completed", "output": { "n": n } }
    }))
    .unwrap();
    TestInput::Inline(Box::new(test))
}

async fn run_code(client: &Client, code: &str) -> rewind_api::runs::Run {
    let mut start = StartRun::new(code);
    start.wait = wait("20s");
    let run = client.start_run(&start, None).await.unwrap();
    assert_eq!(run.status, RunStatus::Completed, "{run:?}");
    run
}

async fn run_named(client: &Client, workflow: &str) -> rewind_api::runs::Run {
    let mut start = StartRun::new("");
    start.workflow = Some(workflow.into());
    start.wait = wait("20s");
    client.start_run(&start, None).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_given_code_finds_or_creates_its_workflow_and_a_check_creates_none() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;

    // The first run of a code creates a workflow of its namespace, version 1, without a name
    // or an interface; the next runs of the code are runs of it.
    let first = run_code(&harness, &returns(1)).await;
    let of = first.workflow.clone().unwrap();
    assert_eq!((of.name.as_deref(), of.version), (None, 1));
    assert!(of.id.starts_with("wf_") && of.id.len() == 15, "{}", of.id);
    let again = run_code(&harness, &returns(1)).await;
    assert_eq!(again.workflow.as_ref(), Some(&of));
    let workflow = harness.get_workflow(&of.id).await.unwrap();
    assert_eq!(
        workflow.namespace.as_ref().map(|name| name.as_str()),
        Some(test_namespace().as_str())
    );
    assert_eq!(
        (
            &workflow.version.input,
            &workflow.version.output,
            &workflow.version.signals
        ),
        (&None, &None, &None)
    );

    // New code that names no workflow is a new workflow.
    let other = run_code(&harness, &returns(2)).await;
    assert_ne!(other.workflow.as_ref().unwrap().id, of.id);

    // A check names the workflow that has the code, and creates nothing; nor does a test.
    let check = |code: String| CheckRequest {
        code,
        action_set: None,
    };
    let checked = harness.check(&check(returns(1))).await.unwrap();
    assert!(checked.ok);
    assert_eq!(
        checked.workflow.as_ref().map(|w| w.id.as_str()),
        Some(of.id.as_str())
    );
    let unknown = harness.check(&check(returns(3))).await.unwrap();
    assert_eq!(unknown.workflow, None);
    let tested = harness
        .test(&TestRequest {
            code: Some(returns(3)),
            tests: vec![returns_test("returns three", 3)],
            ..TestRequest::default()
        })
        .await
        .unwrap();
    assert!(tested.passed, "{tested:?}");
    let with_code = |code: &str| WorkflowQuery {
        code: Some(rewind_core::Code::new(code.to_owned()).digest().to_hex()),
        ..WorkflowQuery::default()
    };
    assert!(
        harness
            .list_workflows(&with_code(&returns(3)))
            .await
            .unwrap()
            .workflows
            .is_empty()
    );

    // A code the organization's workflow has is a run of it, from any namespace.
    let organization = server.tenant_key("acme", "harness").await;
    let action_set = organization
        .create_action_set(&CreateActionSet::default())
        .await
        .unwrap();
    let report = organization
        .create_workflow(&named("report", &action_set.id, &returns(4)))
        .await
        .unwrap();
    assert_eq!(report.namespace, None);
    let run = run_code(&harness, &returns(4)).await;
    let ran = run.workflow.unwrap();
    assert_eq!(
        (ran.id.as_str(), ran.name.as_deref()),
        (report.id.as_str(), Some("report"))
    );
    assert!(run.handle.contains("/report/"), "{}", run.handle);

    // A name created twice in its scope is taken.
    assert_eq!(
        problem(
            organization
                .create_workflow(&named("report", &action_set.id, &returns(5)))
                .await
                .unwrap_err()
        )
        .kind(),
        Some(ProblemType::NameTaken)
    );
    // Without a name, a code its scope has answers its workflow.
    let mut unnamed = named("x", &action_set.id, &returns(4));
    unnamed.name = None;
    let found = organization.create_workflow(&unnamed).await.unwrap();
    assert_eq!(found.id, report.id);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_starts_signals_and_receives_the_end_of_a_workflow_run() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let organization = server.tenant_key("acme", "harness").await;
    let action_set = harness
        .create_action_set(&CreateActionSet::default())
        .await
        .unwrap();

    // Creating checks the code against its interface; the organization's workflow.
    let saved = organization
        .create_workflow(&triage(&action_set.id, TRIAGE))
        .await
        .unwrap();
    assert_eq!(
        (saved.name.as_deref(), saved.namespace.as_ref()),
        (Some("triage"), None)
    );
    let declarations = saved.version.declarations.clone().unwrap();
    assert!(
        declarations.dts.contains("export const triage: Workflow<"),
        "{}",
        declarations.dts
    );
    assert!(
        declarations
            .prompt
            .starts_with("triage.start({ repo: string }) → run"),
        "{}",
        declarations.prompt
    );
    let refused = problem(
        organization
            .create_workflow(&triage(
                &action_set.id,
                &TRIAGE.replace("input: { repo: string }", "input: { repo: number }"),
            ))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::InvalidCode));
    assert!(
        refused.diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .starts_with("the default export does not match the workflow's schemas")),
        "{refused:?}"
    );
    let mut misnamed = triage(&action_set.id, TRIAGE);
    misnamed.name = Some("default".into());
    assert_eq!(
        problem(organization.create_workflow(&misnamed).await.unwrap_err()).kind(),
        Some(ProblemType::InvalidWorkflow)
    );

    // An action set pins it; its runs start it, signal it and receive its end.
    let parent_action_set = harness
        .create_action_set(&CreateActionSet {
            workflows: vec!["triage".into()],
            ..CreateActionSet::default()
        })
        .await
        .unwrap();
    assert_eq!(parent_action_set.workflows.len(), 1);
    let pinned = &parent_action_set.workflows[0];
    assert_eq!(
        (pinned.id.as_str(), pinned.version, pinned.digest.as_str()),
        (saved.id.as_str(), 1, saved.version.digest.as_str())
    );
    assert!(
        parent_action_set
            .declarations
            .dts
            .contains("declare module \"rewind:workflows\""),
        "{}",
        parent_action_set.declarations.dts
    );
    let mut start = StartRun::new(PARENT);
    start.action_set = Some(parent_action_set.id.clone());
    start.input = json!({ "repo": "acme/api" });
    start.wait = wait("30s");
    let parent = harness.start_run(&start, None).await.unwrap();
    assert_eq!(parent.status, RunStatus::Completed, "{parent:?}");
    let output = parent.output.unwrap().value.unwrap();
    assert_eq!(output["result"], json!({ "closed": 3, "repo": "acme/api" }));
    let child = harness
        .get_run(output["id"].as_str().unwrap(), None)
        .await
        .unwrap();
    assert_eq!(child.status, RunStatus::Completed);
    let of = child.workflow.unwrap();
    assert_eq!(
        (of.id.as_str(), of.version, of.digest.as_str()),
        (saved.id.as_str(), 1, saved.version.digest.as_str())
    );
    let started_by = child.started_by.unwrap();
    assert_eq!(
        (started_by.run.as_str(), started_by.detached),
        (parent.id.as_str(), false)
    );

    // A search finds the runs a run started, and the runs of a workflow by name or id.
    let children = eventually(|| async {
        let page = harness
            .list_runs(&RunFilter {
                started_by: Some(parent.id.clone()),
                ..RunFilter::default()
            })
            .await
            .unwrap();
        (!page.runs.is_empty()).then_some(page.runs)
    })
    .await;
    assert_eq!(children.len(), 1);
    assert_eq!(
        children[0]
            .workflow
            .as_ref()
            .and_then(|workflow| workflow.name.as_deref()),
        Some("triage")
    );
    assert_eq!(children[0].started_by.as_deref(), Some(parent.id.as_str()));
    for term in ["workflow:tri*".to_owned(), format!("workflow:{}", saved.id)] {
        let of_triage = harness
            .list_runs(&RunFilter {
                q: Some(format!("{term} started_by:{}", parent.id)),
                ..RunFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(of_triage.runs.len(), 1, "{term}");
    }
    let by_filter = harness
        .list_runs(&RunFilter {
            workflow: Some(saved.id.clone()),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(by_filter.runs.len(), 1);
    let not_started = harness
        .list_runs(&RunFilter {
            q: Some(format!("-started_by:{}", parent.id)),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert!(
        not_started.runs.iter().all(|run| run.id != children[0].id),
        "{not_started:?}"
    );

    // An action set derived from it pins the workflow again, by its number.
    let derived = harness
        .create_action_set(&CreateActionSet {
            base: Some(action_set.id.clone()),
            workflows: vec!["triage@v1".into()],
            ..CreateActionSet::default()
        })
        .await
        .unwrap();
    assert_eq!(derived.id, parent_action_set.id);

    // A namespace's workflow is not pinned, nor is a workflow without an interface.
    let parent_workflow = parent.workflow.unwrap().id;
    let refused = problem(
        harness
            .create_action_set(&CreateActionSet {
                workflows: vec![parent_workflow.clone()],
                ..CreateActionSet::default()
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::InvalidActionSet));
    assert!(refused.detail.contains("promote"), "{}", refused.detail);
    organization
        .create_workflow(&named("bare", &action_set.id, &returns(1)))
        .await
        .unwrap();
    let refused = problem(
        harness
            .create_action_set(&CreateActionSet {
                workflows: vec!["bare".into()],
                ..CreateActionSet::default()
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::InvalidActionSet));
    assert!(refused.detail.contains("interface"), "{}", refused.detail);

    // The namespace sees its workflows and the organization's.
    let listed = harness
        .list_workflows(&WorkflowQuery::default())
        .await
        .unwrap();
    let mut ids: Vec<&str> = listed.workflows.iter().map(|w| w.id.as_str()).collect();
    ids.sort_unstable();
    assert!(ids.contains(&saved.id.as_str()) && ids.contains(&parent_workflow.as_str()));
    let organizations = harness
        .list_workflows(&WorkflowQuery {
            scope: Some(WorkflowScope::Organization),
            ..WorkflowQuery::default()
        })
        .await
        .unwrap();
    assert!(
        organizations
            .workflows
            .iter()
            .all(|workflow| workflow.namespace.is_none())
    );
    let own = harness
        .list_workflows(&WorkflowQuery {
            scope: Some(WorkflowScope::Namespace),
            ..WorkflowQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(
        own.workflows
            .iter()
            .map(|workflow| workflow.id.as_str())
            .collect::<Vec<_>>(),
        [parent_workflow.as_str()]
    );
    assert_eq!(
        problem(harness.get_workflow("nothing").await.unwrap_err()).kind(),
        Some(ProblemType::NotFound)
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_of_a_workflow_takes_only_its_input_and_its_signals() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let action_set = harness
        .create_action_set(&CreateActionSet::default())
        .await
        .unwrap();
    let saved = harness
        .create_workflow(&triage(&action_set.id, TRIAGE))
        .await
        .unwrap();

    let mut start = StartRun::new("");
    start.workflow = Some("triage".into());
    start.input = json!({ "repo": 42 });
    let refused = problem(harness.start_run(&start, None).await.unwrap_err());
    assert_eq!(refused.kind(), Some(ProblemType::InvalidInput));
    assert!(refused.detail.contains("input.repo"), "{}", refused.detail);

    start.input = json!({ "repo": "acme/api" });
    let run = harness.start_run(&start, None).await.unwrap();
    assert_eq!(run.workflow.as_ref().unwrap().digest, saved.version.digest);

    for (name, payload) in [
        ("aproval", json!({ "close": true })),
        ("approval", json!({ "close": "yes" })),
        ("rewind:ended:x", json!({})),
    ] {
        let refused = problem(
            harness
                .signal_run(&run.id, name, &payload, None)
                .await
                .unwrap_err(),
        );
        assert_eq!(refused.kind(), Some(ProblemType::InvalidSignal), "{name}");
    }
    harness
        .signal_run(&run.id, "approval", &json!({ "close": false }), None)
        .await
        .unwrap();
    let ended = harness.get_run(&run.id, wait("20s")).await.unwrap();
    assert_eq!(
        ended.output.unwrap().value,
        Some(json!({ "closed": 0, "repo": "acme/api" }))
    );

    // A key limited to labels only sees the workflows with versions made under its labels.
    let scoped = server
        .key(BuiltInRole::Harness, &[("conversation", "c-1")])
        .await;
    assert!(
        scoped
            .list_workflows(&WorkflowQuery::default())
            .await
            .unwrap()
            .workflows
            .is_empty()
    );
    let mut own = triage(&action_set.id, TRIAGE);
    own.name = Some("digest".into());
    let digest = scoped.create_workflow(&own).await.unwrap();
    assert_eq!(
        digest.labels.0.get("conversation").map(String::as_str),
        Some("c-1")
    );
    let seen: Vec<Option<String>> = eventually(|| async {
        let listed = scoped
            .list_workflows(&WorkflowQuery::default())
            .await
            .unwrap();
        (!listed.workflows.is_empty())
            .then(|| listed.workflows.iter().map(|w| w.name.clone()).collect())
    })
    .await;
    assert_eq!(seen, [Some("digest".to_owned())]);
    server.stop().await;
}

/// A run given code under labels finds the workflow made under labels that carry them: a key
/// limited to labels has its own, a tenant-wide key the first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_run_under_labels_has_a_workflow_of_its_own() {
    let server = TestServer::start(&[]).await;
    let tenant_wide = server.key(BuiltInRole::Harness, &[]).await;
    let dev = server.key(BuiltInRole::Harness, &[("env", "dev")]).await;
    let prod = server.key(BuiltInRole::Harness, &[("env", "prod")]).await;
    let code = returns(1);

    let first = run_code(&tenant_wide, &code).await.workflow.unwrap();
    let ours = run_code(&dev, &code).await.workflow.unwrap();
    assert_ne!(ours.id, first.id, "the labels are part of what a key sees");
    assert_eq!(run_code(&dev, &code).await.workflow.unwrap().id, ours.id);
    let theirs = run_code(&prod, &code).await.workflow.unwrap();
    assert!(theirs.id != ours.id && theirs.id != first.id);
    assert_eq!(
        run_code(&tenant_wide, &code).await.workflow.unwrap().id,
        first.id,
        "a tenant-wide key keeps its own"
    );
    let workflow = dev.get_workflow(&ours.id).await.unwrap();
    assert_eq!(
        workflow.labels.0,
        BTreeMap::from([("env".to_owned(), "dev".to_owned())])
    );

    // The key limited to labels sees its own only; a tenant-wide key sees them all.
    let listed = dev.list_workflows(&WorkflowQuery::default()).await.unwrap();
    assert_eq!(
        listed
            .workflows
            .iter()
            .map(|workflow| workflow.id.as_str())
            .collect::<Vec<_>>(),
        [ours.id.as_str()]
    );
    assert_eq!(
        problem(dev.get_workflow(&first.id).await.unwrap_err()).status,
        404
    );
    let all = tenant_wide
        .list_workflows(&WorkflowQuery::default())
        .await
        .unwrap();
    assert_eq!(all.workflows.len(), 3);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_namespace_forks_an_organization_s_workflow_and_pulls_its_versions_through_its_suite() {
    let server = TestServer::start(&[]).await;
    let organization = server.tenant_key("acme", "harness").await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let alice = harness.with_namespace("alice".parse().unwrap());
    let bob = harness.with_namespace("bob".parse().unwrap());
    let action_set = organization
        .create_action_set(&CreateActionSet::default())
        .await
        .unwrap();
    let mut gym = named("gym", &action_set.id, &returns(1));
    gym.tests = vec![returns_test("returns one", 1)];
    let source = organization.create_workflow(&gym).await.unwrap();

    // Alice's runs of `gym` are of the organization's, until she forks it.
    assert_eq!(
        run_named(&alice, "gym").await.workflow.unwrap().id,
        source.id
    );
    let fork = alice.fork_workflow("gym").await.unwrap();
    assert_ne!(fork.id, source.id);
    assert_eq!(fork.name.as_deref(), Some("gym"));
    assert_eq!(fork.namespace.as_ref().map(|n| n.as_str()), Some("alice"));
    let from = fork.source.clone().unwrap();
    assert_eq!(
        (from.workflow.as_str(), from.version),
        (source.id.as_str(), 1)
    );
    assert_eq!(
        (fork.version.version, fork.version.code_digest.as_str()),
        (1, source.version.code_digest.as_str())
    );
    assert_eq!(fork.suite.len(), 1, "the source's suite is copied");
    // Her name shadows the organization's; Bob's runs keep the organization's.
    assert_eq!(run_named(&alice, "gym").await.workflow.unwrap().id, fork.id);
    assert_eq!(run_named(&bob, "gym").await.workflow.unwrap().id, source.id);

    // Forked again, the name is taken; a namespace's workflow is not forked; a workflow that is
    // no fork pulls nothing.
    assert_eq!(
        problem(alice.fork_workflow("gym").await.unwrap_err()).kind(),
        Some(ProblemType::NameTaken)
    );
    assert_eq!(
        problem(bob.fork_workflow(&fork.id).await.unwrap_err()).kind(),
        Some(ProblemType::NotForkable)
    );
    assert_eq!(
        problem(organization.pull_workflow(&source.id).await.unwrap_err()).kind(),
        Some(ProblemType::NotAFork)
    );

    // Nothing to pull answers the fork unchanged; a new version of the source is pulled through
    // the fork's suite.
    assert_eq!(alice.pull_workflow("gym").await.unwrap().version.version, 1);
    let mut v2 = CreateVersion {
        code: Some(returns(2)),
        ..CreateVersion::default()
    };
    v2.retire = vec![TestRetirement {
        names: vec!["returns one".into()],
        reason: "returns two now".into(),
    }];
    let source_v2 = organization.create_version(&source.id, &v2).await.unwrap();
    assert_eq!(source_v2.version.version, 2);
    let refused = problem(alice.pull_workflow("gym").await.unwrap_err());
    assert_eq!(
        refused.kind(),
        Some(ProblemType::TestsFailed),
        "the fork's own test still expects one"
    );
    alice
        .change_tests(
            "gym",
            &ChangeTests {
                retire: vec![TestRetirement {
                    names: vec!["returns one".into()],
                    reason: "Alice takes two".into(),
                }],
                ..ChangeTests::default()
            },
        )
        .await
        .unwrap();
    let pulled = alice.pull_workflow("gym").await.unwrap();
    assert_eq!(pulled.version.version, 2);
    assert_eq!(pulled.version.code_digest, source_v2.version.code_digest);
    let from = pulled.source.unwrap();
    assert_eq!(
        (from.workflow.as_str(), from.version),
        (source.id.as_str(), 2)
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_namespace_s_workflow_is_promoted_to_the_organization() {
    let server = TestServer::start(&[]).await;
    let organization = server.tenant_key("acme", "harness").await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let alice = harness.with_namespace("alice".parse().unwrap());
    let bob = harness.with_namespace("bob".parse().unwrap());

    let run = run_code(&alice, &returns(7)).await;
    let id = run.workflow.unwrap().id;
    assert_eq!(
        problem(bob.get_workflow(&id).await.unwrap_err()).status,
        404,
        "another namespace's"
    );
    alice
        .update_workflow(
            &id,
            &PatchWorkflow {
                name: Some(Some("seven".into())),
                description: None,
            },
        )
        .await
        .unwrap();
    let promoted = alice.promote_workflow("seven").await.unwrap();
    assert_eq!(
        (promoted.id.as_str(), promoted.namespace),
        (id.as_str(), None)
    );
    assert!(promoted.promoted_at.is_some());
    assert_eq!(run_named(&bob, "seven").await.workflow.unwrap().id, id);

    // The organization's name is taken: a namespace's workflow of that name stays.
    let action_set = organization
        .create_action_set(&CreateActionSet::default())
        .await
        .unwrap();
    organization
        .create_workflow(&named("eight", &action_set.id, &returns(8)))
        .await
        .unwrap();
    let mine = alice
        .create_workflow(&named("eight", &action_set.id, &returns(80)))
        .await
        .unwrap();
    assert_eq!(mine.namespace.as_ref().map(|n| n.as_str()), Some("alice"));
    assert_eq!(
        problem(alice.promote_workflow(&mine.id).await.unwrap_err()).kind(),
        Some(ProblemType::NameTaken)
    );
    // A reader neither promotes nor forks.
    let reader = server.key(BuiltInRole::Reader, &[]).await;
    assert_eq!(
        problem(reader.promote_workflow(&mine.id).await.unwrap_err()).status,
        403
    );
    server.stop().await;
}
