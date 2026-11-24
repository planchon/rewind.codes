//! Workflows across runs, in the driver (`archi/03-execution.md`, "Workflows across runs"): a
//! script starts and signals runs of a workflow, and receives the end of the runs it started;
//! a version is saved checked, and the signals to its runs are checked against it.

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::BTreeMap;

use rewind_core::{
    ActionSet, Attachments, Command, Completion, Digest, Event, HistoryEntry, JsonValue, Lifecycle,
    RunMode, RunStatus, Script, Seq, WorkflowSchemas, end_signal_name, ended_run,
};
use rewind_engine::adapters::StaticTypeChecker;
use rewind_engine::attachments::AttachRights;
use rewind_engine::edge::EdgeError;
use rewind_engine::ports::link::Refusal;
use rewind_engine::ports::{
    ActionDeclarations, CatalogStore, Created, Delivery, Diagnostic, DiagnosticSource, Enqueued,
    NamespaceName, RunMetadata, RunRef, Severity, StartedBy, StoredWorkflow, TenantId,
    WorkflowDeclarations,
};
use rewind_engine::suites::SuiteChanges;
use rewind_engine::{
    ForkRun, RegisterError, RegisterWorkflow, StartError, StartRun, StartWorkflow,
};
use serde_json::json;
use support::plane::World;
use support::{FakeExecutor, RecordingDispatcher, kit};

async fn status(world: &mut World, run: &RunRef) -> RunStatus {
    world.view(run).await.head.status
}

fn output(history: &[HistoryEntry]) -> JsonValue {
    history
        .iter()
        .find_map(|entry| match &entry.event {
            Event::Command(Command::CompleteRun { output }) => output.value().cloned(),
            _ => None,
        })
        .expect("the run completed")
}

/// The message of the failure that ended the run.
async fn failure(world: &mut World, run: &RunRef) -> Option<String> {
    world
        .history(run)
        .await
        .iter()
        .find_map(|entry| match &entry.event {
            Event::Command(Command::FailRun { error }) => Some(error.message.clone()),
            _ => None,
        })
}

/// The `runId` a start's result names.
fn run_id(result: &JsonValue) -> String {
    result["runId"].as_str().unwrap().to_owned()
}

/// The child action set: one host tool.
fn child_action_set() -> ActionSet {
    ActionSet::from_host_tools(["notify"]).unwrap()
}

fn schemas() -> WorkflowSchemas {
    WorkflowSchemas {
        input: json!({
            "type": "object",
            "properties": { "repo": { "type": "string" } },
            "required": ["repo"],
        }),
        output: json!({ "type": "object" }),
        signals: BTreeMap::from([(
            "approval".to_owned(),
            json!({
                "type": "object",
                "properties": { "close": { "type": "boolean" } },
                "required": ["close"],
            }),
        )]),
    }
}

/// `triage` waits for `approval`, then completes with how many issues it closed. A
/// cancellation fails it with `CancelledError`, as the glue does.
fn triage(executor: FakeExecutor) -> FakeExecutor {
    executor.with(
        "triage",
        |run| run.await_signal("approval"),
        |run, delivery| match delivery {
            Delivery::SignalReceived { name, payload } if name == "approval" => {
                let closed = if payload["close"] == json!(true) {
                    3
                } else {
                    0
                };
                run.complete(json!({ "closed": closed, "repo": run.input["repo"].clone() }));
            }
            Delivery::CancelRequested { .. } => run.fail("CancelledError", "cancelled"),
            // Another signal waits in its mailbox.
            Delivery::SignalReceived { .. } => {}
            other => panic!("triage got {other:?}"),
        },
    )
}

/// `parent` starts `triage` (detached when its input says so), signals it, then completes with
/// the end it receives. With `fail_after_start`, it fails once the child exists.
fn parent(executor: FakeExecutor) -> FakeExecutor {
    executor.with(
        "parent",
        |run| {
            let mut args = json!({ "input": { "repo": "acme/api" } });
            if run.input["detached"] == json!(true) {
                args["detached"] = json!(true);
            }
            run.call("workflows.triage.start", args);
        },
        |run, delivery| match delivery {
            Delivery::CallCompleted { seq: Seq(1), result } => {
                if run.input["fail_after_start"] == json!(true) {
                    run.fail("Error", "the parent gave up");
                    return;
                }
                run.call(
                    "workflows.triage.signal",
                    json!({ "run": run_id(result), "name": "approval", "payload": { "close": true } }),
                );
            }
            Delivery::CallCompleted { seq: Seq(2), .. } => {}
            Delivery::SignalReceived { name, payload } if ended_run(name).is_some() => {
                run.complete(payload.clone());
            }
            other => panic!("parent got {other:?}"),
        },
    )
}

/// `signaller` signals the run its input names as a run of `triage`, and fails with the
/// failure of that call. `starter` starts `triage` with the labels its input gives, and
/// completes with the end it receives.
fn others(executor: FakeExecutor) -> FakeExecutor {
    executor
        .with(
            "signaller",
            |run| {
                let target = run.input["target"].clone();
                run.call(
                    "workflows.triage.signal",
                    json!({ "run": target, "name": "approval", "payload": { "close": true } }),
                );
            },
            |run, delivery| match delivery {
                Delivery::CallCompleted { .. } => run.complete(json!("sent")),
                Delivery::CallFailed { error, .. } => run.fail("ActionError", &error.message),
                other => panic!("signaller got {other:?}"),
            },
        )
        .with(
            "starter",
            |run| {
                let mut args = json!({ "input": { "repo": "acme/api" } });
                if let Some(labels) = run.input.get("labels") {
                    args["labels"] = labels.clone();
                }
                run.call("workflows.triage.start", args);
            },
            |run, delivery| match delivery {
                Delivery::CallCompleted { .. } => {}
                Delivery::CallFailed { error, .. } => run.fail("ActionError", &error.message),
                Delivery::SignalReceived { name, payload } if ended_run(name).is_some() => {
                    run.complete(payload.clone());
                }
                other => panic!("starter got {other:?}"),
            },
        )
}

/// A world where `triage` is registered, and the action set that pins it.
async fn registered() -> (World, StoredWorkflow, ActionSet) {
    registered_with(RecordingDispatcher::echo()).await
}

async fn registered_with(dispatcher: RecordingDispatcher) -> (World, StoredWorkflow, ActionSet) {
    let world = World::new(others(parent(triage(FakeExecutor::new()))), dispatcher).await;
    world
        .plane
        .catalog
        .put_action_set(
            &TenantId::local(),
            &child_action_set(),
            &ActionDeclarations::default(),
        )
        .await
        .unwrap();
    let stored = world
        .edge
        .register_workflow(
            register("triage", schemas()),
            &StaticTypeChecker::default(),
            &kit(),
        )
        .await
        .unwrap();
    let action_set = ActionSet::from_host_tools(["notify"])
        .unwrap()
        .with_workflows([stored.workflow.pinned()])
        .unwrap();
    (world, stored, action_set)
}

fn register(name: &str, schemas: WorkflowSchemas) -> RegisterWorkflow {
    RegisterWorkflow {
        tenant: TenantId::local(),
        name: name.into(),
        description: Some("Triage the stale issues of a repository.".into()),
        script: Script::new("triage"),
        action_set: child_action_set().digest(),
        schemas,
        labels: BTreeMap::new(),
        secrets: Attachments::new(),
        attach: AttachRights::NamespaceOnly,
        declarations: WorkflowDeclarations::default(),
        suite: SuiteChanges::default(),
    }
}

/// The namespace the tests' runs act for.
fn local() -> NamespaceName {
    NamespaceName::new(NamespaceName::LOCAL).unwrap()
}

fn start_parent(action_set: &ActionSet, input: JsonValue) -> StartRun {
    let mut request = StartRun::new(Script::new("parent"), action_set.clone(), input);
    request.metadata.labels = BTreeMap::from([("agent".to_owned(), "triage".to_owned())]);
    request
}

/// A run of `triage`'s version in `namespace`.
fn of_version(
    stored: &StoredWorkflow,
    namespace: NamespaceName,
    input: JsonValue,
) -> StartWorkflow {
    StartWorkflow {
        tenant: TenantId::local(),
        namespace,
        version: stored.workflow.version(),
        input,
        ttl: None,
        metadata: RunMetadata::default(),
        idempotency_key: None,
    }
}

async fn start_workflow(world: &mut World, request: StartWorkflow) -> Result<Created, StartError> {
    let mut gateway = world.driver.gateway(&world.at);
    world.edge.start_workflow(&mut gateway, request).await
}

/// The child the parent's start created.
async fn child_of(world: &mut World, parent: &RunRef) -> RunRef {
    let history = world.history(parent).await;
    let id = history
        .iter()
        .find_map(|entry| match &entry.event {
            Event::Completion(Completion::CallCompleted {
                seq: Seq(1),
                result,
            }) => result.value().map(run_id),
            _ => None,
        })
        .expect("the start has a result");
    RunRef::new(parent.tenant.clone(), id.parse().unwrap())
}

#[tokio::test]
async fn a_script_starts_signals_and_receives_the_end_of_a_workflow_run() {
    let (mut world, stored, action_set) = registered().await;
    let parent = world.run(start_parent(&action_set, json!({}))).await;

    assert_eq!(status(&mut world, &parent).await, RunStatus::Completed);
    let child = child_of(&mut world, &parent).await;
    let history = world.history(&parent).await;
    assert_eq!(
        output(&history),
        json!({ "status": "completed", "output": { "closed": 3, "repo": "acme/api" } })
    );
    let ended = history.iter().find_map(|entry| match &entry.event {
        Event::Completion(Completion::SignalReceived { name, .. }) => Some(name.clone()),
        _ => None,
    });
    assert_eq!(ended, Some(end_signal_name(child.id)));

    let head = world.view(&child).await.head;
    assert_eq!(head.status, RunStatus::Completed);
    assert_eq!(head.script, Script::new("triage").digest());
    assert_eq!(head.action_set, child_action_set().digest());
    let workflow = head.metadata.workflow.clone().unwrap();
    assert_eq!(
        (workflow.name.as_str(), workflow.version),
        ("triage", stored.workflow.version())
    );
    assert_eq!(
        head.metadata.started_by,
        Some(StartedBy {
            run: parent.id,
            seq: Seq(1),
            detached: false,
        })
    );
    assert_eq!(
        head.metadata.labels.get("agent").map(String::as_str),
        Some("triage"),
        "a child inherits its parent's labels"
    );
    assert!(
        head.metadata.trace_context.is_some(),
        "the start call's trace context"
    );
    assert_eq!(
        head.idempotency_key,
        Some(format!("rewind:{}/1", parent.id))
    );
    let started = world.history(&child).await;
    let Event::Lifecycle(Lifecycle::RunStarted(started)) = &started[0].event else {
        panic!("a history starts with run_started");
    };
    assert_eq!(started.input.value(), Some(&json!({ "repo": "acme/api" })));
}

#[tokio::test]
async fn a_parent_that_ends_cancels_its_attached_children_not_its_detached_ones() {
    let (mut world, _, action_set) = registered().await;
    let attached = world
        .run(start_parent(
            &action_set,
            json!({ "fail_after_start": true }),
        ))
        .await;
    let detached = world
        .run(start_parent(
            &action_set,
            json!({ "fail_after_start": true, "detached": true }),
        ))
        .await;

    assert_eq!(status(&mut world, &attached).await, RunStatus::Failed);
    let child = child_of(&mut world, &attached).await;
    assert_eq!(status(&mut world, &child).await, RunStatus::Cancelled);
    let reason = world
        .history(&child)
        .await
        .iter()
        .find_map(|entry| match &entry.event {
            Event::Completion(Completion::CancelRequested { reason }) => Some(reason.to_string()),
            _ => None,
        });
    assert_eq!(
        reason,
        Some(format!("the run {} that started it ended", attached.id))
    );

    assert_eq!(status(&mut world, &detached).await, RunStatus::Failed);
    let independent = child_of(&mut world, &detached).await;
    assert_eq!(status(&mut world, &independent).await, RunStatus::Active);
    // Its end goes nowhere: its parent has ended.
    world
        .signal(&independent, "approval", json!({ "close": false }), None)
        .await
        .unwrap();
    world.idle().await;
    assert_eq!(status(&mut world, &independent).await, RunStatus::Completed);
}

#[tokio::test]
async fn signals_to_a_run_of_a_workflow_are_checked_against_its_version() {
    let (mut world, stored, _) = registered().await;
    let run = start_workflow(
        &mut world,
        of_version(&stored, local(), json!({ "repo": "acme/api" })),
    )
    .await
    .unwrap()
    .run;
    world.idle().await;
    assert_eq!(
        world.view(&run).await.head.metadata.workflow.unwrap().name,
        "triage"
    );

    let refused = |result: Result<Enqueued, EdgeError>| match result {
        Err(EdgeError::Refused(Refusal::Signal(message))) => message,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let undeclared = refused(world.signal(&run, "aproval", json!({}), None).await);
    assert!(
        undeclared.contains(r#"does not declare the signal "aproval""#),
        "{undeclared}"
    );
    assert!(
        undeclared.contains(r#"it declares "approval""#),
        "{undeclared}"
    );
    let payload = refused(
        world
            .signal(&run, "approval", json!({ "close": "yes" }), None)
            .await,
    );
    assert!(payload.contains("payload.close"), "{payload}");
    let reserved = refused(
        world
            .signal(&run, &end_signal_name(run.id), json!({}), None)
            .await,
    );
    assert!(reserved.contains("reserved"), "{reserved}");

    // A run that is no workflow's takes any name, but not Rewind's.
    let ad_hoc = world
        .run(StartRun::new(
            Script::new("triage"),
            child_action_set(),
            json!({}),
        ))
        .await;
    assert_eq!(
        world
            .signal(&ad_hoc, "anything", json!(1), None)
            .await
            .unwrap(),
        Enqueued::Accepted
    );
    refused(
        world
            .signal(&ad_hoc, "rewind:ended:x", json!(1), None)
            .await,
    );

    assert_eq!(
        world
            .signal(&run, "approval", json!({ "close": true }), None)
            .await
            .unwrap(),
        Enqueued::Accepted
    );
    world.idle().await;
    assert_eq!(status(&mut world, &run).await, RunStatus::Completed);
}

#[tokio::test]
async fn a_start_is_refused_when_its_input_does_not_match() {
    let (mut world, stored, _) = registered().await;
    let error = start_workflow(
        &mut world,
        of_version(&stored, local(), json!({ "repo": 42 })),
    )
    .await
    .unwrap_err();
    let StartError::InvalidInput(message) = error else {
        panic!("expected an invalid input, got {error:?}");
    };
    assert!(message.contains("input.repo"), "{message}");
    let mut elsewhere = of_version(&stored, local(), json!({}));
    elsewhere.version = Digest::of(b"nothing");
    assert!(matches!(
        start_workflow(&mut world, elsewhere).await,
        Err(StartError::WorkflowNotFound(_))
    ));
}

#[tokio::test]
async fn registering_checks_the_name_the_schemas_the_action_set_and_the_types() {
    let (world, stored, _) = registered().await;
    let checker = StaticTypeChecker::default();
    assert!(matches!(
        world
            .edge
            .register_workflow(register("default", schemas()), &checker, &kit())
            .await,
        Err(RegisterError::Invalid(_))
    ));
    let mut unusable = schemas();
    unusable.output = json!({ "$ref": "https://schemas.example.com/out.json" });
    assert!(matches!(
        world.edge.register_workflow(register("triage", unusable), &checker, &kit()).await,
        Err(RegisterError::Schema { which, .. }) if which == "output"
    ));
    let mut elsewhere = register("triage", schemas());
    elsewhere.action_set = Digest::of(b"unknown");
    assert!(matches!(
        world
            .edge
            .register_workflow(elsewhere, &checker, &kit())
            .await,
        Err(RegisterError::ActionSetNotFound(_))
    ));
    let mut broken = register("triage", schemas());
    broken.script = Script::new("syntax error here");
    assert!(matches!(
        world.edge.register_workflow(broken, &checker, &kit()).await,
        Err(RegisterError::InvalidScript(_))
    ));
    let error = Diagnostic {
        line: 1,
        column: 1,
        severity: Severity::Error,
        source: DiagnosticSource::TypeScript { code: 2322 },
        message: "Type 'number' is not assignable to type 'string'.".into(),
        hint: None,
        frame: String::new(),
    };
    let refusing = StaticTypeChecker::default().with(&Script::new("triage"), vec![error]);
    assert!(matches!(
        world
            .edge
            .register_workflow(register("triage", schemas()), &refusing, &kit())
            .await,
        Err(RegisterError::TypeErrors(_))
    ));

    // Saving the same version again changes nothing; a new description is a new version.
    let again = world
        .edge
        .register_workflow(register("triage", schemas()), &checker, &kit())
        .await
        .unwrap();
    assert_eq!(again, stored);
    let mut next = register("triage", schemas());
    next.description = None;
    let next = world
        .edge
        .register_workflow(next, &checker, &kit())
        .await
        .unwrap();
    assert_ne!(next.workflow.version(), stored.workflow.version());
    let versions = world
        .plane
        .catalog
        .workflow_versions(&TenantId::local(), "triage", &BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(versions.len(), 2);
}

#[tokio::test]
async fn a_script_reaches_only_the_runs_of_its_workflow_and_its_scope() {
    let (mut world, stored, action_set) = registered().await;
    let scope = BTreeMap::from([("conversation".to_owned(), "c-1".to_owned())]);
    let in_scope = |scope: &BTreeMap<String, String>| {
        let mut start = of_version(&stored, local(), json!({ "repo": "acme/api" }));
        start.metadata.labels = scope.clone();
        start
    };
    let visible = start_workflow(&mut world, in_scope(&scope))
        .await
        .unwrap()
        .run;
    let hidden = start_workflow(&mut world, in_scope(&BTreeMap::new()))
        .await
        .unwrap()
        .run;
    let mut ad_hoc = StartRun::new(Script::new("triage"), child_action_set(), json!({}));
    ad_hoc.metadata.labels = scope.clone();
    let ad_hoc = world.start(ad_hoc).await.unwrap().run;
    let unknown: rewind_core::RunId = "0193a5c4-0000-7000-8000-00000000abcd".parse().unwrap();
    world.idle().await;

    let signal = |target: rewind_core::RunId| {
        let mut request = StartRun::new(
            Script::new("signaller"),
            action_set.clone(),
            json!({ "target": target.to_string() }),
        );
        request.metadata.scope = scope.clone();
        request.metadata.labels = scope.clone();
        request
    };
    let mut outcomes = Vec::new();
    for target in [visible.id, hidden.id, ad_hoc.id, unknown] {
        let run = world.run(signal(target)).await;
        outcomes.push(match status(&mut world, &run).await {
            RunStatus::Completed => Some("sent".to_owned()),
            _ => failure(&mut world, &run).await,
        });
    }
    assert_eq!(outcomes[0].as_deref(), Some("sent"));
    assert!(
        outcomes[1]
            .as_deref()
            .is_some_and(|message| message.contains("is visible to this run")),
        "out of scope: {outcomes:?}"
    );
    assert!(
        outcomes[2]
            .as_deref()
            .is_some_and(|message| message.contains("is not a run of workflow triage")),
        "{outcomes:?}"
    );
    assert!(
        outcomes[3]
            .as_deref()
            .is_some_and(|message| message.contains("is visible to this run")),
        "{outcomes:?}"
    );
    assert_eq!(status(&mut world, &visible).await, RunStatus::Completed);
    assert_eq!(status(&mut world, &hidden).await, RunStatus::Active);

    // A start may add labels, never contradict the scope.
    let mut conflicting = StartRun::new(
        Script::new("starter"),
        action_set.clone(),
        json!({ "labels": { "conversation": "c-2" } }),
    );
    conflicting.metadata.scope = scope.clone();
    let run = world.run(conflicting).await;
    let failed = failure(&mut world, &run).await;
    assert!(
        failed
            .as_deref()
            .is_some_and(|message| message.contains("its scope fixes it")),
        "{failed:?}"
    );

    // A version saved without the scope's labels is not started from a scoped run: it may attach
    // the organization's secrets of other labels.
    let mut scoped = StartRun::new(Script::new("starter"), action_set.clone(), json!({}));
    scoped.metadata.scope = scope.clone();
    scoped.metadata.labels = scope.clone();
    let run = world.run(scoped).await;
    let failed = failure(&mut world, &run).await;
    assert!(
        failed
            .as_deref()
            .is_some_and(|message| message.contains("was not saved under this run's scope")),
        "{failed:?}"
    );
}

#[tokio::test]
async fn a_run_reaches_the_runs_of_its_own_end_user_only() {
    let (mut world, stored, action_set) = registered().await;
    let alice = NamespaceName::new("alice").unwrap();
    let bob = NamespaceName::new("bob").unwrap();

    // A run a script starts acts for the same end user as the script.
    let mut parent = start_parent(&action_set, json!({}));
    parent.namespace = alice.clone();
    let parent = world.run(parent).await;
    let child = child_of(&mut world, &parent).await;
    assert_eq!(
        world.view(&child).await.head.namespace,
        alice,
        "a child is in its parent's namespace"
    );

    // Bob's run of the workflow is not visible to a run of Alice's, whatever its labels.
    let bobs = start_workflow(
        &mut world,
        of_version(&stored, bob.clone(), json!({ "repo": "acme/api" })),
    )
    .await
    .unwrap()
    .run;
    world.idle().await;
    let signal = |namespace: &NamespaceName| {
        let mut request = StartRun::new(
            Script::new("signaller"),
            action_set.clone(),
            json!({ "target": bobs.id.to_string() }),
        );
        request.namespace = namespace.clone();
        request
    };
    let alices = world.run(signal(&alice)).await;
    let failed = failure(&mut world, &alices).await;
    assert!(
        failed
            .as_deref()
            .is_some_and(|message| message.contains("is visible to this run")),
        "{failed:?}"
    );
    assert_eq!(status(&mut world, &bobs).await, RunStatus::Active);

    // Bob's own script reaches it.
    let own = world.run(signal(&bob)).await;
    assert_eq!(status(&mut world, &own).await, RunStatus::Completed);
    assert_eq!(status(&mut world, &bobs).await, RunStatus::Completed);
}

#[tokio::test]
async fn in_mock_mode_the_mocks_answer_a_start_with_the_child_s_end() {
    let dispatcher = RecordingDispatcher::new(|call| match call.action.name.as_str() {
        "workflows.triage.start" => Ok(json!({ "closed": 7 })),
        other => panic!("unexpected call of {other}"),
    });
    let (mut world, _, action_set) = registered_with(dispatcher).await;
    let mut request = StartRun::new(Script::new("starter"), action_set, json!({}));
    request.mode = RunMode::Mock;
    let parent = world.run(request).await;
    assert_eq!(status(&mut world, &parent).await, RunStatus::Completed);
    assert_eq!(
        output(&world.history(&parent).await),
        json!({ "status": "completed", "output": { "closed": 7 } })
    );
    let child = child_of(&mut world, &parent).await;
    let mut gateway = world.driver.gateway(&world.at);
    assert!(
        world
            .edge
            .get_run(&mut gateway, &child)
            .await
            .unwrap()
            .is_none(),
        "no child runs in mock mode"
    );
    assert_eq!(world.plane.dispatcher.calls().len(), 1);
    assert_eq!(
        world.events(&parent).await,
        [
            "run_started",
            "schedule_call",
            "call_completed",
            "signal_received",
            "complete_run"
        ],
        "the end follows the start's result"
    );
}

#[tokio::test]
async fn a_fork_receives_the_end_of_the_runs_its_copied_history_started() {
    let (mut world, _, action_set) = registered().await;
    let parent = world
        .run(start_parent(
            &action_set,
            json!({ "fail_after_start": true, "detached": true }),
        ))
        .await;
    assert_eq!(status(&mut world, &parent).await, RunStatus::Failed);
    let child = child_of(&mut world, &parent).await;
    assert_eq!(status(&mut world, &child).await, RunStatus::Active);

    // The fork follows the start, then signals the child and waits for its end, as `parent`
    // does when it does not give up.
    let mut request = ForkRun::new(parent.clone(), Script::new("parent"));
    request.input = Some(json!({ "detached": true }));
    let mut gateway = world.driver.gateway(&world.at);
    let fork = world.edge.fork(&mut gateway, request).await.unwrap().run;
    world.idle().await;
    assert_eq!(status(&mut world, &child).await, RunStatus::Completed);
    assert_eq!(status(&mut world, &fork).await, RunStatus::Completed);
    assert_eq!(
        output(&world.history(&fork).await)["output"]["closed"],
        json!(3)
    );
    // The child stays the parent's.
    assert_eq!(
        world
            .view(&child)
            .await
            .head
            .metadata
            .started_by
            .unwrap()
            .run,
        parent.id
    );
}
