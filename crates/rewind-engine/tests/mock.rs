//! Mock mode: scenarios run in virtual time against cassettes, mocks and static responses
//! (`archi/05-testing-and-observability.md`, "Testing and mocks").

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{Matching, RunStatus, Script, Toolset};
use rewind_engine::DriveOptions;
use rewind_engine::mock::{
    Expectations, ExpectedError, MockDispatcher, Scenario, ScenarioSignal, SignalWhen, run_scenario,
};
use rewind_engine::ports::Delivery;
use serde_json::json;
use support::*;

/// Calls `a`, sleeps ten days, waits for `approval`, and completes with the approval and the
/// result of `a`.
fn sleepy() -> FakeExecutor {
    FakeExecutor::new().with(
        "sleepy",
        |run| {
            run.call("a", json!({ "n": 1 }));
        },
        |run, delivery| match delivery {
            Delivery::ToolCallCompleted { .. } => {
                run.timer(10 * DAY);
            }
            Delivery::ToolCallFailed { error, .. } => run.fail("ToolError", &error.message),
            Delivery::TimerFired { .. } => run.await_signal("approval"),
            Delivery::SignalReceived { payload, .. } => {
                run.complete(json!({ "approved": payload, "at": run.now.0 }));
            }
            Delivery::CancelRequested { .. } => run.fail("CancelledError", "cancelled"),
        },
    )
}

fn scenario() -> Scenario {
    let mut scenario = Scenario::new(
        Script::new("sleepy"),
        Toolset::from_host_tools(["a"]).unwrap(),
        json!({}),
    );
    scenario.signals = vec![ScenarioSignal {
        name: "approval".into(),
        payload: json!(true),
        when: SignalWhen::Blocked,
    }];
    scenario
}

#[tokio::test]
async fn a_scenario_runs_in_virtual_time_and_checks_its_expectations() {
    let executor = sleepy();
    let dispatcher = MockDispatcher::new()
        .with_static_responses(&json!({ "a": { "ok": true } }))
        .unwrap();
    let mut scenario = scenario();
    scenario.expect = Expectations {
        status: Some(RunStatus::Completed),
        output: Some(json!({ "approved": true })),
        error: None,
        calls: Some(BTreeMap::from([("a".to_owned(), 1)])),
    };
    let report = run_scenario(&executor, dispatcher, scenario).await.unwrap();
    assert!(report.passed, "{report:#?}");
    assert_eq!(report.status, RunStatus::Completed);
    // Ten days of sleep took no time: the signal came once the run was blocked.
    let at = report.output.as_ref().unwrap()["at"].as_u64().unwrap();
    assert_eq!(at, Scenario::DEFAULT_START.0 + 10 * DAY);
    assert!(report.trace.contains("timer #2 10d"), "{}", report.trace);
    assert!(report.trace.contains("completed"), "{}", report.trace);
}

#[tokio::test]
async fn failed_expectations_are_reported_for_a_model() {
    let executor = sleepy();
    let dispatcher = MockDispatcher::new();
    let mut scenario = scenario();
    scenario.expect = Expectations {
        status: Some(RunStatus::Completed),
        output: Some(json!({ "approved": true })),
        error: Some(ExpectedError {
            name: "TypeError".into(),
            message: None,
        }),
        calls: Some(BTreeMap::from([("a".to_owned(), 2)])),
    };
    let report = run_scenario(&executor, dispatcher, scenario).await.unwrap();
    assert!(!report.passed);
    assert_eq!(report.status, RunStatus::Failed);
    assert_eq!(
        report.failures,
        [
            "the signal \"approval\" was not delivered: the run had already ended",
            "the run is failed (ToolError: no recorded response for `a({\"n\":1})`), expected completed",
            "the run has no output (it is failed), expected one containing {\"approved\":true}",
            "the run failed with ToolError, expected TypeError",
            "a was called 1 times, expected 2",
        ]
    );
}

#[tokio::test]
async fn a_signal_can_be_sent_at_a_time_of_the_run() {
    let executor = sleepy();
    let dispatcher = MockDispatcher::new()
        .with_static_responses(&json!({ "a": 1 }))
        .unwrap();
    let mut scenario = scenario();
    // Sent while the run sleeps: it waits in the mailbox until the run asks for it.
    scenario.signals[0].when = SignalWhen::After(Duration::from_millis(DAY));
    let report = run_scenario(&executor, dispatcher, scenario).await.unwrap();
    assert_eq!(report.status, RunStatus::Completed, "{report:#?}");
    let received = report
        .history
        .iter()
        .find(|entry| entry.event.name() == "signal_received")
        .unwrap();
    assert_eq!(received.recorded_at.0, Scenario::DEFAULT_START.0 + DAY);
}

#[tokio::test]
async fn a_cassette_recorded_from_a_real_run_replays_it_as_a_scenario() {
    // A "real" run, answered by a tool that is not there in the scenario.
    let engine = engine(
        sleepy(),
        RecordingDispatcher::new(|_| Ok(json!({ "live": 42 }))),
    );
    let run = engine
        .start_run(request("sleepy", json!({})))
        .await
        .unwrap()
        .run;
    engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    engine
        .signal(&run, "approval", json!("yes"), None)
        .await
        .unwrap();
    engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    let cassette = engine
        .export_cassette(&run, Matching::Sequential)
        .await
        .unwrap();
    assert_eq!(cassette.calls.len(), 1);
    assert_eq!(cassette.signals.len(), 1);

    // The cassette answers the call and replays the signal at the time it arrived.
    let executor = sleepy();
    let dispatcher = MockDispatcher::new().with_cassette(cassette);
    let mut scenario = scenario();
    scenario.signals.clear();
    scenario.start = T0;
    scenario.expect.output = Some(json!({ "approved": "yes" }));
    let report = run_scenario(&executor, dispatcher, scenario).await.unwrap();
    assert!(report.passed, "{report:#?}");
    // The same journal, call for call, but its start: a mock run of another toolset.
    let original = engine.export_history(&run).await.unwrap();
    assert_eq!(report.history[1..], original[1..]);
}
