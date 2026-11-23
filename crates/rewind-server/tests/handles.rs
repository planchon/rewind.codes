//! Runs named by their handles, scripts' titles and workflows' version numbers over HTTP, on
//! the embedded server (`archi/06-storage-and-scale.md`, "Run handles"; `archi/04-tools.md`, "Script
//! titles"): a handle names a run wherever its id does, a run and its summary show their handle
//! and their script's title, a person's title comes before the comment's, and `name@v<number>`
//! names a workflow's version.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;

use rewind_api::access::BuiltInRole;
use rewind_api::common::RunStatus;
use rewind_api::runs::{CancelRun, ForkRun, RunFilter, StartRun, TraceFormat};
use rewind_api::scripts::{ScriptFilter, SetScriptTitle, TitleSource};
use rewind_api::toolsets::CreateToolset;
use rewind_api::workflows::{CreateWorkflow, WorkflowQuery};
use rewind_api::{Problem, ProblemType};
use rewind_core::SUFFIX_LEN;
use serde_json::json;
use support::{TestServer, eventually, test_namespace, wait};

/// A script whose comment gives it a title and a description.
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

fn titled(title: Option<&str>, description: Option<&str>) -> SetScriptTitle {
    SetScriptTitle {
        title: title.map(str::to_owned),
        description: description.map(str::to_owned),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_handle_names_a_run_wherever_its_id_does() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;

    // The handle: the namespace, the script's title as a slug, the run's suffix.
    let run = harness
        .start_run(&StartRun::new(APPROVAL), None)
        .await
        .unwrap();
    let prefix = format!("{}/wait-for-approval/", test_namespace().as_str());
    assert!(run.handle.starts_with(&prefix), "{}", run.handle);
    assert_eq!(run.handle.len(), prefix.len() + SUFFIX_LEN);
    assert_eq!(run.script_title.as_deref(), Some("Wait for approval"));

    // Its state, its trace and a signal, by its handle.
    let read = harness.get_run(&run.handle, None).await.unwrap();
    assert_eq!(
        (read.id.as_str(), read.handle.as_str()),
        (run.id.as_str(), run.handle.as_str())
    );
    let trace = harness
        .run_trace(&run.handle, TraceFormat::Llm)
        .await
        .unwrap();
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
        format!("bob/wait-for-approval/{suffix}"),
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

    // A fork, by its parent's handle: of the parent's namespace and origin, naming its parent
    // by its handle; the runs of a parent are listed by its handle.
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
    let parent = fork.parent.unwrap();
    assert_eq!(
        (parent.run, parent.handle),
        (run.id.clone(), run.handle.clone())
    );
    let forks = eventually(|| async {
        let page = harness
            .list_runs(&RunFilter {
                parent: Some(run.handle.clone()),
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
            parent: Some(format!("{prefix}zzzzzzzzzzzz")),
            ..RunFilter::default()
        })
        .await
        .unwrap();
    assert!(none.runs.is_empty());
    let refused = problem(
        harness
            .list_runs(&RunFilter {
                parent: Some("not a run".into()),
                ..RunFilter::default()
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(refused.status, 400, "{}", refused.detail);

    // The summaries show the handle and the script's title.
    let listed = eventually(|| async {
        let page = harness.list_runs(&RunFilter::default()).await.unwrap();
        page.runs.into_iter().find(|summary| summary.id == run.id)
    })
    .await;
    assert_eq!(listed.handle, run.handle);
    assert_eq!(listed.script_title.as_deref(), Some("Wait for approval"));

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
async fn a_person_titles_a_script_over_its_comment() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let run = harness
        .start_run(&StartRun::new(APPROVAL), None)
        .await
        .unwrap();
    let script = run.script.clone();

    // The comment's title and description, until a person gives others. A key bound to a
    // namespace reads the script once the read model lists its run.
    let read = eventually(|| async { harness.get_script(&script).await.ok() }).await;
    assert_eq!(
        (read.title.as_deref(), read.title_source),
        (Some("Wait for approval"), Some(TitleSource::Comment))
    );
    assert_eq!(
        read.description.as_deref(),
        Some("Waits for the signal `approval`, and returns its payload.")
    );

    // A person's title, trimmed, for the whole organization.
    let given = harness
        .set_script_title(
            &script,
            &titled(
                Some("  Approve a deploy  "),
                Some(" Asks the on-call person. "),
            ),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            given.title.as_deref(),
            given.description.as_deref(),
            given.title_source
        ),
        (
            Some("Approve a deploy"),
            Some("Asks the on-call person."),
            Some(TitleSource::Person)
        )
    );
    assert_eq!(given.source, APPROVAL, "the source does not change");
    assert_eq!(
        harness
            .get_run(&run.id, None)
            .await
            .unwrap()
            .script_title
            .as_deref(),
        Some("Approve a deploy")
    );
    let listed = eventually(|| async {
        harness
            .list_scripts(&ScriptFilter::default())
            .await
            .unwrap()
            .scripts
            .into_iter()
            .find(|summary| summary.digest == script)
    })
    .await;
    assert_eq!(
        (listed.title.as_deref(), listed.title_source),
        (Some("Approve a deploy"), Some(TitleSource::Person))
    );
    let summary = eventually(|| async {
        let page = harness.list_runs(&RunFilter::default()).await.unwrap();
        page.runs.into_iter().find(|summary| summary.id == run.id)
    })
    .await;
    assert_eq!(summary.script_title.as_deref(), Some("Approve a deploy"));
    // A new run of the script takes its origin from the person's title; the first keeps its own.
    let again = harness
        .start_run(&StartRun::new(APPROVAL), None)
        .await
        .unwrap();
    let origin = |handle: &str| handle.split('/').nth(1).unwrap().to_owned();
    assert_eq!(origin(&again.handle), "approve-a-deploy");
    assert_eq!(
        origin(&harness.get_run(&run.id, None).await.unwrap().handle),
        "wait-for-approval"
    );

    // Removed, the comment's show again.
    let removed = harness
        .set_script_title(&script, &titled(None, None))
        .await
        .unwrap();
    assert_eq!(
        (removed.title.as_deref(), removed.title_source),
        (Some("Wait for approval"), Some(TitleSource::Comment))
    );

    // A title out of bounds is refused, and nothing changes.
    for (title, description) in [
        ("   ", None),
        (&*"t".repeat(121), None),
        ("Approve", Some("d".repeat(2_001))),
    ] {
        let refused = problem(
            harness
                .set_script_title(&script, &titled(Some(title), description.as_deref()))
                .await
                .unwrap_err(),
        );
        assert_eq!(
            (refused.status, refused.kind()),
            (422, Some(ProblemType::InvalidTitle)),
            "{}",
            refused.detail
        );
    }
    assert_eq!(
        harness.get_script(&script).await.unwrap().title_source,
        Some(TitleSource::Comment)
    );
    // A script the tenant does not have, or that is no digest.
    for unknown in ["00".repeat(32), "nope".to_owned()] {
        let missing = problem(
            harness
                .set_script_title(&unknown, &titled(Some("Anything"), None))
                .await
                .unwrap_err(),
        );
        assert_eq!(missing.status, 404, "{}", missing.detail);
    }
    // A reader may read the title, not give one.
    let reader = server.key(BuiltInRole::Reader, &[]).await;
    let forbidden = problem(
        reader
            .set_script_title(&script, &titled(Some("Mine"), None))
            .await
            .unwrap_err(),
    );
    assert_eq!(forbidden.status, 403);
    assert!(
        forbidden.detail.contains("`scripts:write`"),
        "{}",
        forbidden.detail
    );

    // A script without a comment has no title.
    let bare = harness
        .start_run(
            &StartRun::new("export default async function main() {\n  return 1;\n}\n"),
            None,
        )
        .await
        .unwrap();
    assert_eq!(bare.script_title, None);
    assert!(bare.handle.contains("/script/"), "{}", bare.handle);
    let read = eventually(|| async { harness.get_script(&bare.script).await.ok() }).await;
    assert_eq!(
        (read.title, read.description, read.title_source),
        (None, None, None)
    );
    server.stop().await;
}

const TRIAGE: &str = r#"import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const approval = await signal("approval");
  return { closed: approval.close ? 3 : 0 };
}
"#;

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
        secrets: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_workflow_s_versions_are_numbered_and_named_by_their_number() {
    let server = TestServer::start(&[]).await;
    let harness = server.key(BuiltInRole::Harness, &[]).await;
    let toolset = harness
        .create_toolset(&CreateToolset::default())
        .await
        .unwrap();
    let first = harness
        .create_workflow(&triage(&toolset.id, TRIAGE))
        .await
        .unwrap();
    // Saved again, a version keeps its number.
    let again = harness
        .create_workflow(&triage(&toolset.id, TRIAGE))
        .await
        .unwrap();
    assert_eq!(
        (again.version.as_str(), again.number),
        (first.version.as_str(), 1)
    );
    let second = harness
        .create_workflow(&triage(&toolset.id, &TRIAGE.replace("? 3", "? 4")))
        .await
        .unwrap();
    assert_eq!((first.number, second.number), (1, 2));
    let versions = harness.workflow_versions("triage").await.unwrap();
    assert_eq!(
        versions
            .versions
            .iter()
            .map(|version| version.number)
            .collect::<Vec<_>>(),
        [2, 1]
    );
    let latest = harness
        .list_workflows(&WorkflowQuery::default())
        .await
        .unwrap();
    assert_eq!(latest.workflows[0].number, 2);

    // `triage@v1` names the first version; a run of it shows the number and is named after
    // its workflow.
    let mut start = StartRun::new("");
    start.workflow = Some("triage@v1".into());
    start.input = json!({ "repo": "acme/api" });
    let run = harness.start_run(&start, None).await.unwrap();
    let workflow = run.workflow.unwrap();
    assert_eq!(
        (workflow.version.as_str(), workflow.number),
        (first.version.as_str(), 1)
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
