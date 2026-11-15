//! Workflows across runs: a script starts and signals runs of a workflow, and receives the end
//! of the runs it started (`archi/03-execution.md`, "Workflows across runs").

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::BTreeMap;

use rewind_core::{
    Command, Completion, Digest, Event, HistoryEntry, JsonValue, Lifecycle, RunMode, RunStatus,
    Script, Seq, Toolset, WorkflowSchemas, end_signal_name, ended_run,
};
use rewind_engine::adapters::StaticTypeChecker;
use rewind_engine::ports::{
    Delivery, Diagnostic, DiagnosticSource, Enqueued, JournalStore, NamespaceName, RunMetadata,
    RunRef, Severity, StartedBy, StoredWorkflow, TenantId, ToolDeclarations, WorkflowDeclarations,
};
use rewind_engine::{
    DriveOptions, EngineError, ForkRun, RegisterError, RegisterWorkflow, StartError, StartRun,
    StartWorkflow,
};
use serde_json::json;
use support::*;

async fn drive(engine: &TestEngine) {
    let report = engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    assert!(!report.interrupted);
    assert!(report.failures.is_empty(), "{report:?}");
}

async fn status(engine: &TestEngine, run: &RunRef) -> RunStatus {
    engine.get_run(run).await.unwrap().unwrap().status
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

/// The `runId` a start's result names.
fn run_id(result: &JsonValue) -> String {
    result["runId"].as_str().unwrap().to_owned()
}

/// The child toolset: one host tool.
fn child_toolset() -> Toolset {
    Toolset::from_host_tools(["notify"]).unwrap()
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
            Delivery::ToolCallCompleted { seq: Seq(1), result } => {
                if run.input["fail_after_start"] == json!(true) {
                    run.fail("Error", "the parent gave up");
                    return;
                }
                run.call(
                    "workflows.triage.signal",
                    json!({ "run": run_id(result), "name": "approval", "payload": { "close": true } }),
                );
            }
            Delivery::ToolCallCompleted { seq: Seq(2), .. } => {}
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
                Delivery::ToolCallCompleted { .. } => run.complete(json!("sent")),
                Delivery::ToolCallFailed { error, .. } => run.fail("ToolError", &error.message),
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
                Delivery::ToolCallCompleted { .. } => {}
                Delivery::ToolCallFailed { error, .. } => run.fail("ToolError", &error.message),
                Delivery::SignalReceived { name, payload } if ended_run(name).is_some() => {
                    run.complete(payload.clone());
                }
                other => panic!("starter got {other:?}"),
            },
        )
}

/// An engine where `triage` is registered, and the toolset that pins it.
async fn registered() -> (TestEngine, StoredWorkflow, Toolset) {
    registered_with(RecordingDispatcher::echo()).await
}

async fn registered_with(dispatcher: RecordingDispatcher) -> (TestEngine, StoredWorkflow, Toolset) {
    let engine = engine(others(parent(triage(FakeExecutor::new()))), dispatcher);
    engine
        .journal()
        .put_toolset(
            &TenantId::local(),
            &child_toolset(),
            &ToolDeclarations::default(),
        )
        .await
        .unwrap();
    let stored = engine
        .register_workflow(register("triage", schemas()), &StaticTypeChecker::default())
        .await
        .unwrap();
    let toolset = Toolset::from_host_tools(["notify"])
        .unwrap()
        .with_workflows([stored.workflow.pinned()])
        .unwrap();
    (engine, stored, toolset)
}

fn register(name: &str, schemas: WorkflowSchemas) -> RegisterWorkflow {
    RegisterWorkflow {
        tenant: TenantId::local(),
        name: name.into(),
        description: Some("Triage the stale issues of a repository.".into()),
        script: Script::new("triage"),
        toolset: child_toolset().digest(),
        schemas,
        labels: BTreeMap::new(),
        declarations: WorkflowDeclarations::default(),
    }
}

/// The namespace the tests' runs act for.
fn local() -> NamespaceName {
    NamespaceName::new(NamespaceName::LOCAL).unwrap()
}

fn start_parent(toolset: &Toolset, input: JsonValue) -> StartRun {
    let mut request = StartRun::new(Script::new("parent"), toolset.clone(), input);
    request.metadata.labels = BTreeMap::from([("agent".to_owned(), "triage".to_owned())]);
    request
}

/// The child the parent's start created.
async fn child_of(engine: &TestEngine, parent: &RunRef) -> RunRef {
    let history = engine.history(parent).await.unwrap();
    let id = history
        .iter()
        .find_map(|entry| match &entry.event {
            Event::Completion(Completion::ToolCallCompleted {
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
    let (engine, stored, toolset) = registered().await;
    let parent = engine
        .start_run(start_parent(&toolset, json!({})))
        .await
        .unwrap()
        .run;
    drive(&engine).await;

    assert_eq!(status(&engine, &parent).await, RunStatus::Completed);
    let child = child_of(&engine, &parent).await;
    assert_eq!(
        output(&engine.history(&parent).await.unwrap()),
        json!({ "status": "completed", "output": { "closed": 3, "repo": "acme/api" } })
    );
    let history = engine.history(&parent).await.unwrap();
    let ended = history.iter().find_map(|entry| match &entry.event {
        Event::Completion(Completion::SignalReceived { name, .. }) => Some(name.clone()),
        _ => None,
    });
    assert_eq!(ended, Some(end_signal_name(child.id)));

    let record = engine.get_run(&child).await.unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Completed);
    assert_eq!(record.script, Script::new("triage").digest());
    assert_eq!(record.toolset, child_toolset().digest());
    let workflow = record.metadata.workflow.clone().unwrap();
    assert_eq!(
        (workflow.name.as_str(), workflow.version),
        ("triage", stored.workflow.version())
    );
    assert_eq!(
        record.metadata.started_by,
        Some(StartedBy {
            run: parent.id,
            seq: Seq(1),
            detached: false,
        })
    );
    assert_eq!(
        record.metadata.labels.get("agent").map(String::as_str),
        Some("triage"),
        "a child inherits its parent's labels"
    );
    assert!(
        record.metadata.trace_context.is_some(),
        "the start call's trace context"
    );
    assert_eq!(
        record.idempotency_key,
        Some(format!("rewind:{}/1", parent.id))
    );
    let started = engine.history(&child).await.unwrap();
    let Event::Lifecycle(Lifecycle::RunStarted(started)) = &started[0].event else {
        panic!("a history starts with run_started");
    };
    assert_eq!(started.input.value(), Some(&json!({ "repo": "acme/api" })));
}

#[tokio::test]
async fn a_parent_that_ends_cancels_its_attached_children_not_its_detached_ones() {
    let (engine, _, toolset) = registered().await;
    let attached = engine
        .start_run(start_parent(&toolset, json!({ "fail_after_start": true })))
        .await
        .unwrap()
        .run;
    let detached = engine
        .start_run(start_parent(
            &toolset,
            json!({ "fail_after_start": true, "detached": true }),
        ))
        .await
        .unwrap()
        .run;
    drive(&engine).await;

    assert_eq!(status(&engine, &attached).await, RunStatus::Failed);
    let child = child_of(&engine, &attached).await;
    assert_eq!(status(&engine, &child).await, RunStatus::Cancelled);
    let history = engine.history(&child).await.unwrap();
    let reason = history.iter().find_map(|entry| match &entry.event {
        Event::Completion(Completion::CancelRequested { reason }) => Some(reason.to_string()),
        _ => None,
    });
    assert_eq!(
        reason,
        Some(format!("the run {} that started it ended", attached.id))
    );

    assert_eq!(status(&engine, &detached).await, RunStatus::Failed);
    let independent = child_of(&engine, &detached).await;
    assert_eq!(status(&engine, &independent).await, RunStatus::Active);
    // Its end goes nowhere: its parent has ended.
    engine
        .signal(&independent, "approval", json!({ "close": false }), None)
        .await
        .unwrap();
    drive(&engine).await;
    assert_eq!(status(&engine, &independent).await, RunStatus::Completed);
}

#[tokio::test]
async fn signals_to_a_run_of_a_workflow_are_checked_against_its_version() {
    let (engine, stored, _) = registered().await;
    let run = engine
        .start_workflow(StartWorkflow {
            tenant: TenantId::local(),
            namespace: local(),
            version: stored.workflow.version(),
            input: json!({ "repo": "acme/api" }),
            ttl: None,
            metadata: RunMetadata::default(),
            idempotency_key: None,
            credentials: None,
        })
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    assert_eq!(
        engine
            .get_run(&run)
            .await
            .unwrap()
            .unwrap()
            .metadata
            .workflow
            .unwrap()
            .name,
        "triage"
    );

    let refused = |result: Result<Enqueued, EngineError>| match result {
        Err(EngineError::SignalRefused(message)) => message,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let undeclared = refused(engine.signal(&run, "aproval", json!({}), None).await);
    assert!(
        undeclared.contains(r#"does not declare the signal "aproval""#),
        "{undeclared}"
    );
    assert!(
        undeclared.contains(r#"it declares "approval""#),
        "{undeclared}"
    );
    let payload = refused(
        engine
            .signal(&run, "approval", json!({ "close": "yes" }), None)
            .await,
    );
    assert!(payload.contains("payload.close"), "{payload}");
    let reserved = refused(
        engine
            .signal(&run, &end_signal_name(run.id), json!({}), None)
            .await,
    );
    assert!(reserved.contains("reserved"), "{reserved}");

    // A run that is no workflow's takes any name, but not Rewind's.
    let ad_hoc = engine
        .start_run(StartRun::new(
            Script::new("triage"),
            child_toolset(),
            json!({}),
        ))
        .await
        .unwrap()
        .run;
    assert_eq!(
        engine
            .signal(&ad_hoc, "anything", json!(1), None)
            .await
            .unwrap(),
        Enqueued::Accepted
    );
    refused(
        engine
            .signal(&ad_hoc, "rewind:ended:x", json!(1), None)
            .await,
    );

    assert_eq!(
        engine
            .signal(&run, "approval", json!({ "close": true }), None)
            .await
            .unwrap(),
        Enqueued::Accepted
    );
    drive(&engine).await;
    assert_eq!(status(&engine, &run).await, RunStatus::Completed);
}

#[tokio::test]
async fn a_start_is_refused_when_its_input_does_not_match() {
    let (engine, stored, _) = registered().await;
    let error = engine
        .start_workflow(StartWorkflow {
            tenant: TenantId::local(),
            namespace: local(),
            version: stored.workflow.version(),
            input: json!({ "repo": 42 }),
            ttl: None,
            metadata: RunMetadata::default(),
            idempotency_key: None,
            credentials: None,
        })
        .await
        .unwrap_err();
    let StartError::InvalidInput(message) = error else {
        panic!("expected an invalid input, got {error:?}");
    };
    assert!(message.contains("input.repo"), "{message}");
    assert!(matches!(
        engine
            .start_workflow(StartWorkflow {
                tenant: TenantId::local(),
                namespace: local(),
                version: Digest::of(b"nothing"),
                input: json!({}),
                ttl: None,
                metadata: RunMetadata::default(),
                idempotency_key: None,
                credentials: None,
            })
            .await,
        Err(StartError::WorkflowNotFound(_))
    ));
}

#[tokio::test]
async fn registering_checks_the_name_the_schemas_the_toolset_and_the_types() {
    let (engine, stored, _) = registered().await;
    let checker = StaticTypeChecker::default();
    assert!(matches!(
        engine
            .register_workflow(register("default", schemas()), &checker)
            .await,
        Err(RegisterError::Invalid(_))
    ));
    let mut unusable = schemas();
    unusable.output = json!({ "$ref": "https://schemas.example.com/out.json" });
    assert!(matches!(
        engine.register_workflow(register("triage", unusable), &checker).await,
        Err(RegisterError::Schema { which, .. }) if which == "output"
    ));
    let mut elsewhere = register("triage", schemas());
    elsewhere.toolset = Digest::of(b"unknown");
    assert!(matches!(
        engine.register_workflow(elsewhere, &checker).await,
        Err(RegisterError::ToolsetNotFound(_))
    ));
    let mut broken = register("triage", schemas());
    broken.script = Script::new("syntax error here");
    assert!(matches!(
        engine.register_workflow(broken, &checker).await,
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
        engine
            .register_workflow(register("triage", schemas()), &refusing)
            .await,
        Err(RegisterError::TypeErrors(_))
    ));

    // Saving the same version again changes nothing; a new script is a new version.
    let again = engine
        .register_workflow(register("triage", schemas()), &checker)
        .await
        .unwrap();
    assert_eq!(again, stored);
    let mut next = register("triage", schemas());
    next.description = None;
    let next = engine.register_workflow(next, &checker).await.unwrap();
    assert_ne!(next.workflow.version(), stored.workflow.version());
    let versions = engine
        .journal()
        .workflow_versions(&TenantId::local(), "triage", &BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(versions.len(), 2);
}

#[tokio::test]
async fn a_script_reaches_only_the_runs_of_its_workflow_and_its_scope() {
    let (engine, stored, toolset) = registered().await;
    let scope = BTreeMap::from([("conversation".to_owned(), "c-1".to_owned())]);
    let in_scope = |scope: &BTreeMap<String, String>| {
        let metadata = RunMetadata {
            labels: scope.clone(),
            ..RunMetadata::default()
        };
        StartWorkflow {
            tenant: TenantId::local(),
            namespace: local(),
            version: stored.workflow.version(),
            input: json!({ "repo": "acme/api" }),
            ttl: None,
            metadata,
            idempotency_key: None,
            credentials: None,
        }
    };
    let visible = engine.start_workflow(in_scope(&scope)).await.unwrap().run;
    let hidden = engine
        .start_workflow(in_scope(&BTreeMap::new()))
        .await
        .unwrap()
        .run;
    let mut ad_hoc = StartRun::new(Script::new("triage"), child_toolset(), json!({}));
    ad_hoc.metadata.labels = scope.clone();
    let ad_hoc = engine.start_run(ad_hoc).await.unwrap().run;
    let unknown: rewind_core::RunId = "0193a5c4-0000-7000-8000-00000000abcd".parse().unwrap();
    drive(&engine).await;

    let signal = |target: rewind_core::RunId| {
        let mut request = StartRun::new(
            Script::new("signaller"),
            toolset.clone(),
            json!({ "target": target.to_string() }),
        );
        request.metadata.scope = scope.clone();
        request.metadata.labels = scope.clone();
        request
    };
    let mut outcomes = Vec::new();
    for target in [visible.id, hidden.id, ad_hoc.id, unknown] {
        let run = engine.start_run(signal(target)).await.unwrap().run;
        drive(&engine).await;
        let history = engine.history(&run).await.unwrap();
        outcomes.push(history.iter().find_map(|entry| match &entry.event {
            Event::Command(Command::CompleteRun { .. }) => Some("sent".to_owned()),
            Event::Command(Command::FailRun { error }) => Some(error.message.clone()),
            _ => None,
        }));
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
    assert_eq!(status(&engine, &visible).await, RunStatus::Completed);
    assert_eq!(status(&engine, &hidden).await, RunStatus::Active);

    // A start may add labels, never contradict the scope.
    let mut conflicting = StartRun::new(
        Script::new("starter"),
        toolset.clone(),
        json!({ "labels": { "conversation": "c-2" } }),
    );
    conflicting.metadata.scope = scope.clone();
    let run = engine.start_run(conflicting).await.unwrap().run;
    drive(&engine).await;
    let history = engine.history(&run).await.unwrap();
    let failed = history.iter().find_map(|entry| match &entry.event {
        Event::Command(Command::FailRun { error }) => Some(error.message.clone()),
        _ => None,
    });
    assert!(
        failed
            .as_deref()
            .is_some_and(|message| message.contains("its scope fixes it")),
        "{failed:?}"
    );
}

#[tokio::test]
async fn a_run_reaches_the_runs_of_its_own_end_user_only() {
    let (engine, stored, toolset) = registered().await;
    let alice = NamespaceName::new("alice").unwrap();
    let bob = NamespaceName::new("bob").unwrap();

    // A run a script starts acts for the same end user as the script.
    let mut parent = start_parent(&toolset, json!({}));
    parent.namespace = alice.clone();
    let parent = engine.start_run(parent).await.unwrap().run;
    drive(&engine).await;
    let child = child_of(&engine, &parent).await;
    assert_eq!(
        engine.get_run(&child).await.unwrap().unwrap().namespace,
        alice,
        "a child is in its parent's namespace"
    );

    // Bob's run of the workflow is not visible to a run of Alice's, whatever its labels.
    let bobs = engine
        .start_workflow(StartWorkflow {
            tenant: TenantId::local(),
            namespace: bob.clone(),
            version: stored.workflow.version(),
            input: json!({ "repo": "acme/api" }),
            ttl: None,
            metadata: RunMetadata::default(),
            idempotency_key: None,
            credentials: None,
        })
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    let signal = |namespace: &NamespaceName| {
        let mut request = StartRun::new(
            Script::new("signaller"),
            toolset.clone(),
            json!({ "target": bobs.id.to_string() }),
        );
        request.namespace = namespace.clone();
        request
    };
    let alices = engine.start_run(signal(&alice)).await.unwrap().run;
    drive(&engine).await;
    let failed = engine
        .history(&alices)
        .await
        .unwrap()
        .iter()
        .find_map(|entry| match &entry.event {
            Event::Command(Command::FailRun { error }) => Some(error.message.clone()),
            _ => None,
        });
    assert!(
        failed
            .as_deref()
            .is_some_and(|message| message.contains("is visible to this run")),
        "{failed:?}"
    );
    assert_eq!(status(&engine, &bobs).await, RunStatus::Active);

    // Bob's own script reaches it.
    let own = engine.start_run(signal(&bob)).await.unwrap().run;
    drive(&engine).await;
    assert_eq!(status(&engine, &own).await, RunStatus::Completed);
    assert_eq!(status(&engine, &bobs).await, RunStatus::Completed);
}

#[tokio::test]
async fn in_mock_mode_the_mocks_answer_a_start_with_the_child_s_end() {
    let dispatcher = RecordingDispatcher::new(|call| match call.tool.name.as_str() {
        "workflows.triage.start" => Ok(json!({ "closed": 7 })),
        other => panic!("unexpected call of {other}"),
    });
    let (engine, _, toolset) = registered_with(dispatcher.clone()).await;
    let mut request = StartRun::new(Script::new("starter"), toolset, json!({}));
    request.mode = RunMode::Mock;
    let parent = engine.start_run(request).await.unwrap().run;
    drive(&engine).await;
    assert_eq!(status(&engine, &parent).await, RunStatus::Completed);
    assert_eq!(
        output(&engine.history(&parent).await.unwrap()),
        json!({ "status": "completed", "output": { "closed": 7 } })
    );
    let child = child_of(&engine, &parent).await;
    assert!(
        engine.get_run(&child).await.unwrap().is_none(),
        "no child runs in mock mode"
    );
    assert_eq!(dispatcher.calls().len(), 1);
    let events: Vec<&str> = engine
        .history(&parent)
        .await
        .unwrap()
        .iter()
        .map(|entry| entry.event.name())
        .collect();
    assert_eq!(
        events,
        [
            "run_started",
            "schedule_tool_call",
            "tool_call_completed",
            "signal_received",
            "complete_run"
        ],
        "the end follows the start's result"
    );
}

#[tokio::test]
async fn a_fork_receives_the_end_of_the_runs_its_copied_history_started() {
    let (engine, _, toolset) = registered().await;
    let parent = engine
        .start_run(start_parent(
            &toolset,
            json!({ "fail_after_start": true, "detached": true }),
        ))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    assert_eq!(status(&engine, &parent).await, RunStatus::Failed);
    let child = child_of(&engine, &parent).await;
    assert_eq!(status(&engine, &child).await, RunStatus::Active);

    // The fork follows the start, then signals the child and waits for its end, as `parent`
    // does when it does not give up.
    let mut request = ForkRun::new(parent.clone(), Script::new("parent"));
    request.input = Some(json!({ "detached": true }));
    let fork = engine.fork(request).await.unwrap().run;
    drive(&engine).await;
    assert_eq!(status(&engine, &child).await, RunStatus::Completed);
    assert_eq!(status(&engine, &fork).await, RunStatus::Completed);
    assert_eq!(
        output(&engine.history(&fork).await.unwrap())["output"]["closed"],
        json!(3)
    );
    // The child stays the parent's.
    assert_eq!(
        engine
            .get_run(&child)
            .await
            .unwrap()
            .unwrap()
            .metadata
            .started_by
            .unwrap()
            .run,
        parent.id
    );
}

#[tokio::test]
async fn a_derived_toolset_takes_the_credentials_of_its_base() {
    use rewind_engine::ports::{Credentials, Secret, SecretStore, ServerCredentials};

    let (engine, _, derived) = registered().await;
    let base = child_toolset();
    let tenant = TenantId::local();
    let credentials = Credentials {
        servers: BTreeMap::from([(
            "github".to_owned(),
            ServerCredentials {
                headers: BTreeMap::from([("Authorization".to_owned(), Secret::new("Bearer t"))]),
                env: BTreeMap::new(),
            },
        )]),
    };
    engine
        .set_toolset_credentials(&tenant, base.digest(), &credentials)
        .await
        .unwrap();
    engine
        .journal()
        .put_toolset(&tenant, &derived, &ToolDeclarations::default())
        .await
        .unwrap();
    assert!(
        engine
            .inherit_toolset_credentials(&tenant, base.digest(), derived.digest())
            .await
            .unwrap()
    );
    let no_run = rewind_core::RunId::from_u128(0);
    assert_eq!(
        engine
            .secrets()
            .resolve(&tenant, no_run, &local(), derived.digest())
            .await
            .unwrap(),
        Some(credentials)
    );
    // A toolset that has credentials of its own keeps them; a toolset is not its own base.
    assert!(
        !engine
            .inherit_toolset_credentials(&tenant, base.digest(), derived.digest())
            .await
            .unwrap()
    );
    assert!(
        !engine
            .inherit_toolset_credentials(&tenant, base.digest(), base.digest())
            .await
            .unwrap()
    );
}
