//! Starts and forks at the edge, in the driver (`archi/03-execution.md`, "Starting a run",
//! "Forks"): what is refused before a run exists, a fork that reuses its parent's recorded
//! calls and goes live where it differs, and whether a corrected script is compatible with a
//! journal.

#![allow(clippy::unwrap_used)]

use crate::support;

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{
    Event, Lifecycle, RunStatus, Script, Seq, Timestamp, ToolFailure, ToolFailureKind,
};
use rewind_engine::adapters::StaticTypeChecker;
use rewind_engine::ports::{
    Delivery, Diagnostic, DiagnosticSource, ForkParent, Page, RunOrder, RunQuery, RunRef, Scope,
    Severity, TenantId, TraceContext,
};
use rewind_engine::{ForkError, ForkPoint, ForkRun, Limits, ReplayVerdict, StartError};
use serde_json::json;
use support::plane::World;
use support::{FakeExecutor, HOUR, RecordingDispatcher, request};

/// `a` answers with its args; `b` refuses "bad".
fn dispatcher() -> RecordingDispatcher {
    RecordingDispatcher::new(|call| {
        if call.args == json!("bad") {
            return Err(ToolFailure {
                kind: ToolFailureKind::Error,
                message: "bad argument".into(),
                data: None,
            });
        }
        Ok(call.args.clone())
    })
}

/// Calls `a`, then `b` with `arg`; completes with `b`'s result, fails if it failed.
fn two_steps(executor: FakeExecutor, name: &str, arg: &'static str) -> FakeExecutor {
    executor.with(
        name,
        |run| {
            run.call("a", json!(1));
        },
        move |run, delivery| match delivery {
            Delivery::ToolCallCompleted { seq: Seq(1), .. } => {
                run.call("b", json!(arg));
            }
            Delivery::ToolCallCompleted { result, .. } => run.complete(result.clone()),
            _ => run.fail("ToolError", "b failed"),
        },
    )
}

fn programs() -> FakeExecutor {
    let programs = two_steps(FakeExecutor::new(), "buggy", "bad");
    two_steps(programs, "fixed", "good")
}

/// The number of runs the tenant has.
async fn runs(world: &mut World) -> usize {
    let tenant = TenantId::local();
    let mut gateway = world.driver.gateway(&world.at.clone());
    world
        .edge
        .search(
            &mut gateway,
            &Scope::Tenant(tenant.clone()),
            &RunQuery::new(tenant),
            RunOrder::Started,
            &Page::first(100),
        )
        .await
        .unwrap()
        .summaries
        .len()
}

/// A run of `buggy` that failed, an hour ago.
async fn failed_parent(world: &mut World) -> RunRef {
    let parent = world.run(request("buggy", json!({ "n": 1 }))).await;
    assert_eq!(world.view(&parent).await.head.status, RunStatus::Failed);
    world.advance(HOUR);
    parent
}

async fn fork(world: &mut World, request: ForkRun) -> Result<RunRef, ForkError> {
    let mut gateway = world.driver.gateway(&world.at.clone());
    Ok(world.edge.fork(&mut gateway, request).await?.run)
}

#[tokio::test]
async fn invalid_starts_are_refused_before_a_run_exists() {
    let mut world = World::new(programs(), dispatcher()).await;
    let refused = world
        .start(request("syntax error here", json!({})))
        .await
        .unwrap_err();
    assert_eq!(
        refused.to_string(),
        "the script is invalid: syntax error here: unexpected token"
    );
    let mut too_long = request("buggy", json!({}));
    too_long.ttl = Some(Duration::from_secs(400 * 86_400));
    assert!(matches!(
        world.start(too_long).await,
        Err(StartError::InvalidTtl { .. })
    ));
    let mut too_large = request("buggy", json!("x".repeat(9 * 1024 * 1024)));
    too_large.ttl = None;
    assert!(matches!(
        world.start(too_large).await,
        Err(StartError::InputTooLarge { .. })
    ));
    assert_eq!(runs(&mut world).await, 0);
}

/// Postgres cannot keep a NUL character (U+0000) in text: it is refused at the way in, before
/// anything is stored. Payload values are JSON, where it is escaped: they keep it.
#[tokio::test]
async fn text_with_a_nul_character_is_refused_at_the_way_in() {
    let mut world = World::new(programs(), dispatcher()).await;
    let refused = |error: String| assert!(error.contains("NUL character (U+0000)"), "{error}");
    let mut source = request("buggy", json!({}));
    source.script = Script::new("buggy\0");
    let mut label = request("buggy", json!({}));
    label.metadata.labels.insert("agent\0".into(), "x".into());
    let mut key = request("buggy", json!({}));
    key.idempotency_key = Some("call\0".into());
    let mut trace = request("buggy", json!({}));
    trace.metadata.trace_context = Some(TraceContext {
        traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
        tracestate: Some("vendor=\0".into()),
    });
    for start in [source, label, key, trace] {
        refused(world.start(start).await.unwrap_err().to_string());
    }
    assert_eq!(runs(&mut world).await, 0);

    let run = world.run(request("buggy", json!({ "text": "a\0b" }))).await;
    refused(
        world
            .signal(&run, "go", json!(1), Some("delivery\0"))
            .await
            .unwrap_err()
            .to_string(),
    );
    let mut labelled = ForkRun::new(run.clone(), Script::new("fixed"));
    labelled.labels = Some(BTreeMap::from([("agent".into(), "\0".into())]));
    refused(fork(&mut world, labelled).await.unwrap_err().to_string());
    let mut keyed = ForkRun::new(run, Script::new("fixed"));
    keyed.idempotency_key = Some("\0".into());
    refused(fork(&mut world, keyed).await.unwrap_err().to_string());
    assert_eq!(runs(&mut world).await, 1);
}

#[tokio::test]
async fn a_script_with_a_type_error_is_refused_before_any_run_exists() {
    let mut world = World::new(programs(), dispatcher()).await;
    let refused = request("buggy", json!({}));
    let error = Diagnostic {
        line: 3,
        column: 28,
        severity: Severity::Error,
        source: DiagnosticSource::TypeScript { code: 2322 },
        message: "Type 'number' is not assignable to type 'string'.".into(),
        hint: None,
        frame: String::new(),
    };
    let warning = Diagnostic {
        severity: Severity::Warning,
        ..error.clone()
    };
    let checker = StaticTypeChecker::new().with(&refused.script, vec![error.clone()]);
    let mut gateway = world.driver.gateway(&world.at.clone());
    let started = world
        .edge
        .start_run_checked(&mut gateway, refused.clone(), &checker)
        .await;
    let Err(StartError::TypeErrors(diagnostics)) = started else {
        panic!("a type error refuses the script: {started:?}");
    };
    assert_eq!(diagnostics, [error]);
    assert_eq!(runs(&mut world).await, 0, "no run exists");

    // Warnings alone do not refuse it.
    let checker = StaticTypeChecker::new().with(&refused.script, vec![warning]);
    let mut gateway = world.driver.gateway(&world.at.clone());
    assert!(
        world
            .edge
            .start_run_checked(&mut gateway, refused, &checker)
            .await
            .unwrap()
            .created
    );
}

#[tokio::test]
async fn a_fork_reuses_the_recorded_calls_and_goes_live_where_it_differs() {
    let mut world = World::new(programs(), dispatcher()).await;
    let parent = failed_parent(&mut world).await;
    let parent_history = world.history(&parent).await;

    let fork = fork(
        &mut world,
        ForkRun::new(parent.clone(), Script::new("fixed")),
    )
    .await
    .unwrap();
    world.idle().await;
    assert_eq!(world.view(&fork).await.head.status, RunStatus::Completed);
    let history = world.history(&fork).await;
    assert_eq!(
        history
            .iter()
            .map(|entry| entry.event.name())
            .collect::<Vec<_>>(),
        [
            "run_started",
            "schedule_tool_call",
            "tool_call_completed",
            "schedule_tool_call",
            "tool_call_completed",
            "complete_run"
        ]
    );
    // The parent's start and its result are copied with their time; what is live is later.
    assert_eq!(history[0].recorded_at, parent_history[0].recorded_at);
    assert_eq!(history[2].event, parent_history[2].event);
    assert_eq!(history[2].recorded_at, parent_history[2].recorded_at);
    assert_eq!(history[4].recorded_at, Timestamp(support::T0.0 + HOUR));
    let (
        Event::Lifecycle(Lifecycle::RunStarted(started)),
        Event::Lifecycle(Lifecycle::RunStarted(parent_started)),
    ) = (&history[0].event, &parent_history[0].event)
    else {
        panic!("a history starts with run_started")
    };
    assert_eq!(started.seed, parent_started.seed);
    assert_eq!(started.input, parent_started.input);
    assert_eq!(started.script, Script::new("fixed").digest());
    assert!(started.expires_at > parent_started.expires_at, "a new TTL");

    // `a` was called once, by the parent; `b` twice, the fork's call with the fork's key.
    let calls = world.plane.dispatcher.calls();
    let tools: Vec<&str> = calls.iter().map(|call| call.tool.name.as_str()).collect();
    assert_eq!(tools, ["a", "b", "b"]);
    assert_eq!(calls[2].idempotency_key, format!("{}/2", fork.id));
    assert_eq!(calls[2].args, json!("good"));
    assert_eq!(
        world.view(&fork).await.head.metadata.parent,
        Some(ForkParent {
            run: parent.id,
            at: None
        })
    );
    // Self-contained: it replays without its parent.
    let mut gateway = world.driver.gateway(&world.at.clone());
    assert!(matches!(
        world.edge.replay(&mut gateway, &fork, None).await.unwrap(),
        ReplayVerdict::Matches(_)
    ));
}

#[tokio::test]
async fn a_fork_is_refused_like_a_run_that_cannot_start() {
    let mut world = World::new(programs(), dispatcher()).await;
    let parent = failed_parent(&mut world).await;
    let unknown = RunRef::new(parent.tenant.clone(), rewind_core::RunId::from_u128(7));
    assert!(matches!(
        fork(&mut world, ForkRun::new(unknown.clone(), Script::new("fixed"))).await,
        Err(ForkError::ParentNotFound(run)) if run == unknown.id
    ));
    assert!(matches!(
        fork(
            &mut world,
            ForkRun::new(parent.clone(), Script::new("syntax error"))
        )
        .await,
        Err(ForkError::Start(StartError::InvalidScript(_)))
    ));
    let mut request = ForkRun::new(parent, Script::new("fixed"));
    request.ttl = Some(Duration::ZERO);
    assert!(matches!(
        fork(&mut world, request).await,
        Err(ForkError::Start(StartError::InvalidTtl { .. }))
    ));
}

#[tokio::test]
async fn compatibility_tells_where_a_corrected_script_diverges_and_what_a_fork_reuses() {
    let mut world = World::new(programs(), dispatcher()).await;
    let parent = failed_parent(&mut world).await;
    let mut gateway = world.driver.gateway(&world.at.clone());
    let journal = world
        .edge
        .export_journal(&mut gateway, &parent)
        .await
        .unwrap();
    let executor = &world.plane.executor;
    let limits = Limits::default();
    let scripts: BTreeMap<&str, Script> = ["buggy", "fixed", "syntax error"]
        .into_iter()
        .map(|name| (name, Script::new(name)))
        .collect();
    let check = |script: &str, at| {
        rewind_engine::fork::compatibility(executor, &journal, &scripts[script], at, &limits)
    };

    let same = check("buggy", None).await.unwrap();
    assert!(same.is_compatible(), "{same:?}");
    assert_eq!(same.point, ForkPoint::EndOfParent);
    assert_eq!(same.status, RunStatus::Failed);

    let fixed = check("fixed", None).await.unwrap();
    assert!(!fixed.is_compatible());
    let ForkPoint::Diverged(mismatch) = &fixed.point else {
        panic!("{fixed:?}")
    };
    assert_eq!(
        mismatch.to_string(),
        "history entry 3 is tool call #2 b(\"bad\"), but the script emitted tool call #2 b(\"good\")"
    );
    assert_eq!(fixed.status, RunStatus::Active);

    let forced = check("buggy", Some(2)).await.unwrap();
    assert_eq!(forced.point, ForkPoint::At(2));
    assert_eq!(forced.reused, [Seq(1)], "a's result is reused");
    assert!(matches!(
        check("syntax error", None).await,
        Err(ForkError::Start(StartError::InvalidScript(_)))
    ));
}
