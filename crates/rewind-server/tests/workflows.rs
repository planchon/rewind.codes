//! Workflows over HTTP, on Turso: saving versions, pinning them in toolsets, a script that starts
//! and signals runs of a workflow and receives their end, runs of a workflow started by the
//! harness, and the signals a run of a workflow refuses (`archi/01-product.md`, "Workflows").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::collections::BTreeMap;

use rewind_api::access::BuiltInRole;
use rewind_api::common::RunStatus;
use rewind_api::runs::{RunFilter, StartRun};
use rewind_api::toolsets::CreateToolset;
use rewind_api::workflows::{CreateWorkflow, WorkflowQuery};
use rewind_api::{Problem, ProblemType};
use serde_json::json;
use support::{TestServer, eventually, wait};

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

fn problem(error: rewind_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

fn triage(toolset: &str, script: &str) -> CreateWorkflow {
    CreateWorkflow {
        name: "triage".into(),
        description: Some("Triage the stale issues of a repository, after approval.".into()),
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
        secrets: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_script_starts_signals_and_receives_the_end_of_a_workflow_run() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let toolset = harness
        .create_toolset(&CreateToolset::default())
        .await
        .unwrap();

    // Saving checks the script against its schemas; saving it again returns the same version.
    let saved = harness
        .create_workflow(&triage(&toolset.id, TRIAGE))
        .await
        .unwrap();
    assert_eq!(saved.name, "triage");
    assert!(
        saved
            .declarations
            .dts
            .contains("export const triage: Workflow<"),
        "{}",
        saved.declarations.dts
    );
    assert!(
        saved
            .declarations
            .prompt
            .starts_with("triage.start({ repo: string }) → run"),
        "{}",
        saved.declarations.prompt
    );
    let again = harness
        .create_workflow(&triage(&toolset.id, TRIAGE))
        .await
        .unwrap();
    assert_eq!(again.version, saved.version);

    let refused = problem(
        harness
            .create_workflow(&triage(
                &toolset.id,
                &TRIAGE.replace("input: { repo: string }", "input: { repo: number }"),
            ))
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.kind(), Some(ProblemType::InvalidScript));
    assert!(
        refused.diagnostics.iter().any(|diagnostic| diagnostic
            .message
            .starts_with("the default export does not match the workflow's schemas")),
        "{refused:?}"
    );
    let mut misnamed = triage(&toolset.id, TRIAGE);
    misnamed.name = "default".into();
    assert_eq!(
        problem(harness.create_workflow(&misnamed).await.unwrap_err()).kind(),
        Some(ProblemType::InvalidWorkflow)
    );

    // A toolset pins it; its scripts start it, signal it and receive its end.
    let parent_toolset = harness
        .create_toolset(&CreateToolset {
            workflows: vec!["triage".into()],
            ..CreateToolset::default()
        })
        .await
        .unwrap();
    assert_eq!(parent_toolset.workflows.len(), 1);
    assert_eq!(parent_toolset.workflows[0].version, saved.version);
    assert!(
        parent_toolset
            .declarations
            .dts
            .contains("declare module \"rewind:workflows\""),
        "{}",
        parent_toolset.declarations.dts
    );
    let mut start = StartRun::new(PARENT);
    start.toolset = Some(parent_toolset.id.clone());
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
    assert_eq!(child.workflow.unwrap().version, saved.version);
    let started_by = child.started_by.unwrap();
    assert_eq!(
        (started_by.run.as_str(), started_by.detached),
        (parent.id.as_str(), false)
    );

    // A search finds the runs a run started, and the runs of a workflow.
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
    assert_eq!(children[0].workflow.as_deref(), Some("triage"));
    assert_eq!(children[0].started_by.as_deref(), Some(parent.id.as_str()));
    let of_triage = harness
        .list_runs(&RunFilter {
            q: Some(format!("workflow:tri* started_by:{}", parent.id)),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(of_triage.runs.len(), 1);
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

    // A toolset derived from it pins the workflow again, without its servers listed again.
    let derived = harness
        .create_toolset(&CreateToolset {
            base: Some(toolset.id.clone()),
            workflows: vec![format!("triage@{}", &saved.version[..12])],
            ..CreateToolset::default()
        })
        .await
        .unwrap();
    assert_eq!(derived.id, parent_toolset.id);

    let listed = harness
        .list_workflows(&WorkflowQuery::default())
        .await
        .unwrap();
    assert_eq!(listed.workflows.len(), 1);
    let versions = harness.workflow_versions("triage").await.unwrap();
    assert_eq!(versions.versions.len(), 1);
    assert_eq!(
        problem(harness.workflow_versions("nothing").await.unwrap_err()).kind(),
        Some(ProblemType::NotFound)
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_of_a_workflow_takes_only_its_input_and_its_signals() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let toolset = harness
        .create_toolset(&CreateToolset::default())
        .await
        .unwrap();
    let saved = harness
        .create_workflow(&triage(&toolset.id, TRIAGE))
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
    assert_eq!(run.workflow.as_ref().unwrap().version, saved.version);

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

    // A key limited to labels only sees the workflows saved with its labels.
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
    let mut own = triage(&toolset.id, TRIAGE);
    own.name = "digest".into();
    let digest = scoped.create_workflow(&own).await.unwrap();
    assert_eq!(
        digest.labels.0.get("conversation").map(String::as_str),
        Some("c-1")
    );
    let seen: Vec<String> = eventually(|| async {
        let listed = scoped
            .list_workflows(&WorkflowQuery::default())
            .await
            .unwrap();
        (!listed.workflows.is_empty())
            .then(|| listed.workflows.iter().map(|w| w.name.clone()).collect())
    })
    .await;
    assert_eq!(seen, ["digest"]);
    server.stop().await;
}

/// A workflow saved under a grant limited to labels is a version of its own, carrying them, even
/// when someone outside the scope saved the same workflow first: the scoped caller sees it,
/// pins it and starts it. A tenant-wide save keeps the version it always had.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workflow_saved_under_labels_is_a_version_of_its_own() {
    let server = TestServer::start(&[]).await;
    let tenant_wide = server.key(BuiltInRole::Harness, &[]).await;
    let dev = server.key(BuiltInRole::Harness, &[("env", "dev")]).await;
    let prod = server.key(BuiltInRole::Harness, &[("env", "prod")]).await;
    let toolset = tenant_wide
        .create_toolset(&CreateToolset::default())
        .await
        .unwrap();
    let body = triage(&toolset.id, TRIAGE);

    let first = tenant_wide.create_workflow(&body).await.unwrap();
    assert!(first.labels.0.is_empty());
    let ours = dev.create_workflow(&body).await.unwrap();
    assert_ne!(
        ours.version, first.version,
        "the scope is part of the version"
    );
    assert_eq!(
        ours.labels.0,
        BTreeMap::from([("env".to_owned(), "dev".to_owned())])
    );
    assert_eq!(
        dev.create_workflow(&body).await.unwrap().version,
        ours.version,
        "saving it again under the same labels returns it"
    );
    let theirs = prod.create_workflow(&body).await.unwrap();
    assert!(theirs.version != ours.version && theirs.version != first.version);
    let again = tenant_wide.create_workflow(&body).await.unwrap();
    assert_eq!(
        (again.version.as_str(), again.labels.0.len()),
        (first.version.as_str(), 0),
        "a tenant-wide save keeps its version"
    );

    // The scoped caller sees its own version only, and starts it by name.
    let listed = dev.list_workflows(&WorkflowQuery::default()).await.unwrap();
    assert_eq!(
        listed
            .workflows
            .iter()
            .map(|workflow| (workflow.name.as_str(), workflow.version.as_str()))
            .collect::<Vec<_>>(),
        [("triage", ours.version.as_str())]
    );
    let versions = dev.workflow_versions("triage").await.unwrap();
    assert_eq!(versions.versions.len(), 1);
    let pinned = dev
        .create_toolset(&CreateToolset {
            workflows: vec!["triage".into()],
            ..CreateToolset::default()
        })
        .await
        .unwrap();
    assert_eq!(pinned.workflows[0].version, ours.version);
    let mut start = StartRun::new("");
    start.workflow = Some("triage".into());
    start.input = json!({ "repo": "acme/api" });
    let run = dev.start_run(&start, None).await.unwrap();
    assert_eq!(run.workflow.unwrap().version, ours.version);
    assert_eq!(
        run.labels.0.get("env").map(String::as_str),
        Some("dev"),
        "{:?}",
        run.labels
    );

    // A tenant-wide caller sees every scope's versions, newest first.
    let all = tenant_wide.workflow_versions("triage").await.unwrap();
    assert_eq!(all.versions.len(), 3);
    server.stop().await;
}
