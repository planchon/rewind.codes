//! Runs named by their handles and workflows' version numbers over HTTP, on the embedded server
//! (`archi/06-storage-and-scale.md`, "Run handles"; `archi/04-tools.md`, "Workflow names"): a
//! handle names a run wherever its id does, a run and its summary show their handle and their
//! workflow, a run's origin is its workflow's name, else its short id, and `name@v<number>` names
//! a workflow's version.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use rewind_api::access::BuiltInRole;
use rewind_api::action_sets::CreateActionSet;
use rewind_api::common::RunStatus;
use rewind_api::runs::{CancelRun, ForkRun, RunFilter, StartRun};
use rewind_api::workflows::{CreateVersion, CreateWorkflow, PatchWorkflow, WorkflowQuery};
use rewind_api::{Problem, ProblemType};
use rewind_core::SUFFIX_LEN;
use serde_json::json;
use support::{TestServer, eventually, test_namespace, wait};

/// A code whose comment gives it a title and a description.
const APPROVAL: &str = r#"/**
 * Wait for approval
 *
 * Waits for the signal `approval`, and returns its payload.
 */
import { signal } from "rewind";

export default async function main() {
  return await signal("approval");
}
"#;

fn problem(error: rewind_api::Error) -> Problem {
    error
        .problem()
        .cloned()
        .unwrap_or_else(|| panic!("not a problem: {error}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handle_names_a_run_wherever_its_id_does() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;

    // The handle: the namespace, the short id of the workflow the run created (it has no
    // name), the run's suffix.
    let run = harness
        .start_run(&StartRun::new(APPROVAL), None)
        .await
        .unwrap();
    let of = run.workflow.clone().unwrap();
    let short = format!("wf-{}", &of.id[3..9]);
    let prefix = format!("{}/{short}/", test_namespace().as_str());
    assert!(run.handle.starts_with(&prefix), "{}", run.handle);
    assert_eq!(run.handle.len(), prefix.len() + SUFFIX_LEN);

    // Its state, its transcript and a signal, by its handle.
    let read = harness.get_run(&run.handle, None).await.unwrap();
    assert_eq!(
        (read.id.as_str(), read.handle.as_str()),
        (run.id.as_str(), run.handle.as_str())
    );
    let trace = harness.run_transcript(&run.handle).await.unwrap();
    assert_eq!(trace.run_id, run.id);
    let accepted = harness
        .signal_run(&run.handle, "approval", &json!({ "ok": true }), None)
        .await
        .unwrap();
    assert_eq!(accepted.run_id, run.id);
    let ended = harness.get_run(&run.handle, wait("30s")).await.unwrap();
    assert_eq!(ended.status, RunStatus::Completed, "{ended:?}");
    assert_eq!(ended.output.unwrap().value, Some(json!({ "ok": true })));

    // Another namespace, another origin or another suffix name no run: the answer names what
    // was asked.
    let suffix = run.handle.rsplit('/').next().unwrap();
    for wrong in [
        format!("bob/{short}/{suffix}"),
        format!("{}/digest/{suffix}", test_namespace().as_str()),
        format!("{prefix}zzzzzzzzzzzz"),
    ] {
        let missing = problem(harness.get_run(&wrong, None).await.unwrap_err());
        assert_eq!(missing.status, 404, "{wrong}");
        assert!(missing.detail.contains(&wrong), "{}", missing.detail);
    }
    // A request bound to another namespace does not see it, by its handle or its id.
    let other = harness.with_namespace("bob".parse().unwrap());
    for name in [&run.handle, &run.id] {
        assert_eq!(
            problem(other.get_run(name, None).await.unwrap_err()).status,
            404
        );
    }

    // A fork, by its source's handle: of the source's namespace and origin, naming its source
    // by its handle; the runs of a source are listed by its handle.
    let fork = harness
        .fork_run(
            &run.handle,
            &ForkRun::new(APPROVAL.replace("approval\")", "go\")")),
            None,
        )
        .await
        .unwrap();
    assert!(fork.handle.starts_with(&prefix), "{}", fork.handle);
    assert_ne!(fork.handle, run.handle);
    let parent = fork.source.unwrap();
    assert_eq!(
        (parent.run, parent.handle),
        (run.id.clone(), run.handle.clone())
    );
    let forks = eventually(|| async {
        let page = harness
            .list_runs(&RunFilter {
                source: Some(run.handle.clone()),
                ..RunFilter::default()
            })
            .await
            .unwrap();
        (!page.runs.is_empty()).then_some(page.runs)
    })
    .await;
    assert_eq!(forks.len(), 1);
    assert_eq!(
        (forks[0].id.as_str(), forks[0].handle.as_str()),
        (fork.id.as_str(), fork.handle.as_str())
    );
    // A handle naming no run lists none; what is neither an id nor a handle is refused.
    let none = harness
        .list_runs(&RunFilter {
            source: Some(format!("{prefix}zzzzzzzzzzzz")),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert!(none.runs.is_empty());
    let refused = problem(
        harness
            .list_runs(&RunFilter {
                source: Some("not a run".into()),
                ..RunFilter::default()
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.status, 400, "{}", refused.detail);

    // The summaries show the handle and the workflow.
    let listed = eventually(|| async {
        let page = harness.list_runs(&RunFilter::default()).await.unwrap();
        page.runs.into_iter().find(|summary| summary.id == run.id)
    })
    .await;
    assert_eq!(listed.handle, run.handle);
    assert_eq!(listed.workflow, Some(of));

    // Cancelled and erased by its handle.
    let cancelled = harness
        .cancel_run(
            &fork.handle,
            &CancelRun {
                reason: None,
                wait: wait("30s"),
            },
        )
        .await
        .unwrap();
    assert_eq!(cancelled.status, RunStatus::Cancelled);
    harness.delete_run(&fork.handle).await.unwrap();
    assert_eq!(
        problem(harness.get_run(&fork.id, None).await.unwrap_err()).status,
        404
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_s_handle_takes_its_workflow_s_name_else_its_short_id() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let run = harness
        .start_run(&StartRun::new(APPROVAL), None)
        .await
        .unwrap();
    let of = run.workflow.clone().unwrap();
    assert_eq!((of.name.as_deref(), of.version), (None, 1));

    // The workflow the run created shows the comment at the top of its code, display only.
    let workflow = harness.get_workflow(&of.id).await.unwrap();
    let comment = workflow.version.comment.clone().unwrap();
    assert_eq!(
        (comment.title.as_str(), comment.description.as_deref()),
        (
            "Wait for approval",
            Some("Waits for the signal `approval`, and returns its payload.")
        )
    );
    assert_eq!(workflow.name, None);
    assert_eq!(
        workflow.namespace.as_ref().map(|name| name.as_str()),
        Some(test_namespace().as_str())
    );

    // Named, its next runs take its name; the first keeps its own handle.
    let named = harness
        .update_workflow(
            &of.id,
            &PatchWorkflow {
                name: Some(Some("approve_deploy".into())),
                description: Some(Some("Asks the on-call person.".into())),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        (named.id.as_str(), named.version.version),
        (of.id.as_str(), 1),
        "a name keeps its id and makes no version"
    );
    let again = harness
        .start_run(&StartRun::new(APPROVAL), None)
        .await
        .unwrap();
    let origin = |handle: &str| handle.split('/').nth(1).unwrap().to_owned();
    assert_eq!(origin(&again.handle), "approve_deploy");
    let again_of = again.workflow.unwrap();
    assert_eq!(
        (again_of.id.as_str(), again_of.name.as_deref()),
        (of.id.as_str(), Some("approve_deploy"))
    );
    assert_eq!(
        origin(&harness.get_run(&run.id, None).await.unwrap().handle),
        format!("wf-{}", &of.id[3..9])
    );

    // A name out of bounds is refused, and nothing changes.
    for name in ["has space", "default", "wf_approve", &"t".repeat(65)] {
        let refused = problem(
            harness
                .update_workflow(
                    &of.id,
                    &PatchWorkflow {
                        name: Some(Some(name.to_owned())),
                        description: None,
                    },
                )
                .await
                .unwrap_err(),
        );
        assert_eq!(
            (refused.status, refused.kind()),
            (422, Some(ProblemType::InvalidWorkflow)),
            "{}",
            refused.detail
        );
    }
    assert_eq!(
        harness.get_workflow(&of.id).await.unwrap().name.as_deref(),
        Some("approve_deploy")
    );
    // A reader may read the workflow, not name it.
    let reader = server.key(BuiltInRole::Reader, &[]).await;
    let forbidden = problem(
        reader
            .update_workflow(
                &of.id,
                &PatchWorkflow {
                    name: Some(None),
                    description: None,
                },
            )
            .await
            .unwrap_err(),
    );
    assert_eq!(forbidden.status, 403);
    assert!(
        forbidden.detail.contains("`workflows:write`"),
        "{}",
        forbidden.detail
    );

    // A code without a comment: a workflow without a comment, its runs named by its short id.
    let bare = harness
        .start_run(
            &StartRun::new("export default async function main() {\n  return 1;\n}\n"),
            None,
        )
        .await
        .unwrap();
    let bare_of = bare.workflow.unwrap();
    assert_ne!(bare_of.id, of.id);
    assert_eq!(origin(&bare.handle), format!("wf-{}", &bare_of.id[3..9]));
    let read = harness.get_workflow(&bare_of.id).await.unwrap();
    assert_eq!(read.version.comment, None);
    server.stop().await;
}

const TRIAGE: &str = r#"import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const approval = await signal("approval");
  return { closed: approval.close ? 3 : 0 };
}
"#;

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

fn new_code(code: &str) -> CreateVersion {
    CreateVersion {
        code: Some(code.into()),
        ..CreateVersion::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workflow_s_versions_are_numbered_and_named_by_their_number() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let action_set = harness
        .create_action_set(&CreateActionSet::default())
        .await
        .unwrap();
    let first = harness
        .create_workflow(&triage(&action_set.id, TRIAGE))
        .await
        .unwrap();
    // The same code again makes no version; new code is the next.
    let again = harness
        .create_version("triage", &new_code(TRIAGE))
        .await
        .unwrap();
    assert_eq!(
        (again.version.digest.as_str(), again.version.version),
        (first.version.digest.as_str(), 1)
    );
    let second = harness
        .create_version("triage", &new_code(&TRIAGE.replace("? 3", "? 4")))
        .await
        .unwrap();
    assert_eq!((first.version.version, second.version.version), (1, 2));
    assert_eq!(second.id, first.id);
    let versions = harness.get_workflow("triage").await.unwrap();
    assert_eq!(
        versions
            .versions
            .iter()
            .map(|version| version.version)
            .collect::<Vec<_>>(),
        [2, 1]
    );
    let listed = harness
        .list_workflows(&WorkflowQuery::default())
        .await
        .unwrap();
    let latest = listed
        .workflows
        .iter()
        .find(|workflow| workflow.id == first.id)
        .unwrap();
    assert_eq!(latest.latest.version, 2);

    // `triage@v1` names the first version; a run of it shows the number and is named after
    // its workflow.
    let mut start = StartRun::new("");
    start.workflow = Some("triage@v1".into());
    start.input = json!({ "repo": "acme/api" });
    let run = harness.start_run(&start, None).await.unwrap();
    let workflow = run.workflow.unwrap();
    assert_eq!(
        (
            workflow.digest.as_str(),
            workflow.version,
            workflow.id.as_str()
        ),
        (first.version.digest.as_str(), 1, first.id.as_str())
    );
    assert!(run.handle.contains("/triage/"), "{}", run.handle);

    start.workflow = Some("triage@v3".into());
    assert_eq!(
        problem(harness.start_run(&start, None).await.unwrap_err()).status,
        404
    );
    start.workflow = Some("triage@vlatest".into());
    assert_eq!(
        problem(harness.start_run(&start, None).await.unwrap_err()).status,
        400
    );
    server.stop().await;
}
