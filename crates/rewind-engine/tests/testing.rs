//! Tests run in virtual time against rules, mocks, cassettes and static responses
//! (`archi/05-testing-and-observability.md`, "Testing and mocks").

#![allow(clippy::unwrap_used)]

mod support;

use std::time::Duration;

use rewind_core::{
    ActionOutcome, ActionRule, ActionSet, CallOrder, Code, Expectations, ExpectedCall,
    ExpectedError, Matching, RunStatus, SignalWhen, Test, TestOrigin, TestSignal,
};
use rewind_engine::ports::Delivery;
use rewind_engine::testing::{
    DEFAULT_START, NoMockLoader, Subject, TestOptions, TestProblem, run_test, run_tests,
};
use serde_json::json;
use support::plane::World;
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
            Delivery::CallCompleted { .. } => {
                run.timer(10 * DAY);
            }
            Delivery::CallFailed { error, .. } => run.fail("ActionError", &error.message),
            Delivery::TimerFired { .. } => run.await_signal("approval"),
            Delivery::SignalReceived { payload, .. } => {
                run.complete(json!({ "approved": payload, "at": run.now.0 }));
            }
            Delivery::CancelRequested { .. } => run.fail("CancelledError", "cancelled"),
        },
    )
}

fn code() -> Code {
    Code::new("sleepy")
}

fn action_set() -> ActionSet {
    ActionSet::from_harness_actions(["a", "b"]).unwrap()
}

/// `a` answers `{ ok: true }`; `approval` comes once the run is blocked.
fn test() -> Test {
    let mut test = Test::new("approves after ten days");
    test.world.actions.insert(
        "a".into(),
        vec![ActionRule::always(ActionOutcome::Result(
            json!({ "ok": true }),
        ))],
    );
    test.signals.push(TestSignal {
        name: "approval".into(),
        payload: json!(true),
        when: SignalWhen::Blocked,
    });
    test
}

async fn run(test: &Test) -> rewind_engine::testing::TestReport {
    let code = code();
    let action_set = action_set();
    let subject = Subject {
        code: &code,
        action_set: &action_set,
        interface: None,
    };
    run_test(
        &sleepy(),
        &NoMockLoader,
        &subject,
        test,
        &TestOptions::default(),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn a_test_runs_in_virtual_time_and_checks_its_expectations() {
    let mut test = test();
    test.expect = Expectations {
        status: Some(RunStatus::Completed),
        output: Some(json!({ "approved": true })),
        calls: vec![
            ExpectedCall {
                action: "a".into(),
                args: Some(json!({ "n": 1 })),
                times: Some(1),
            },
            ExpectedCall {
                action: "b".into(),
                args: None,
                times: Some(0),
            },
        ],
        ..Expectations::default()
    };
    let report = run(&test).await;
    assert!(report.passed, "{report:#?}");
    assert_eq!(report.problem, None);
    assert_eq!(report.status, Some(RunStatus::Completed));
    // Ten days of sleep took no time: the signal came once the run was blocked.
    let at = report.output.as_ref().unwrap()["at"].as_u64().unwrap();
    assert_eq!(at, DEFAULT_START.0 + 10 * DAY);
    assert!(report.trace.contains("timer #2 10d"), "{}", report.trace);
    assert!(report.trace.contains("completed"), "{}", report.trace);
}

#[tokio::test]
async fn failed_expectations_and_unanswered_calls_are_reported_for_a_model() {
    let mut test = test();
    test.world.actions.clear();
    test.expect = Expectations {
        status: Some(RunStatus::Completed),
        output: Some(json!({ "approved": true })),
        error: Some(ExpectedError {
            name: "TypeError".into(),
            message: None,
        }),
        calls: vec![ExpectedCall {
            action: "a".into(),
            args: None,
            times: Some(2),
        }],
        order: CallOrder::Any,
    };
    let report = run(&test).await;
    assert!(!report.passed);
    assert_eq!(report.problem, Some(TestProblem::Failed));
    assert_eq!(report.status, Some(RunStatus::Failed));
    assert_eq!(
        report.failures,
        [
            "the signal \"approval\" was not delivered: the run had already ended",
            "the code called a({\"n\":1}), which the test's world does not answer",
            "the run is failed (ActionError: no recorded response for `a({\"n\":1})`), expected \
             completed",
            "the run has no output (it is failed), expected one containing {\"approved\":true}",
            "the run failed with ActionError: no recorded response for `a({\"n\":1})`, expected \
             TypeError",
            "a was called once, expected 2 times",
        ]
    );
}

#[tokio::test]
async fn an_unanswered_call_fails_the_test_even_when_the_script_handles_it() {
    // The code fails with its own error when `a` fails: no expectation says anything about
    // it, yet the test fails, since the world does not answer `a`.
    let mut test = test();
    test.world.actions.clear();
    let report = run(&test).await;
    assert!(!report.passed);
    assert!(
        report.failures.contains(
            &"the code called a({\"n\":1}), which the test's world does not answer".to_owned()
        ),
        "{:?}",
        report.failures
    );
}

#[tokio::test]
async fn a_signal_can_be_sent_at_a_time_of_the_run() {
    let mut test = test();
    // Sent while the run sleeps: it waits in the mailbox until the run asks for it.
    test.signals[0].when = SignalWhen::After(Duration::from_millis(DAY));
    let report = run(&test).await;
    assert_eq!(report.status, Some(RunStatus::Completed), "{report:#?}");
    let received = report
        .history
        .iter()
        .find(|entry| entry.event.name() == "signal_received")
        .unwrap();
    assert_eq!(received.recorded_at.0, DEFAULT_START.0 + DAY);
}

#[tokio::test]
async fn a_run_waiting_for_the_outside_world_ends_its_test_active() {
    let mut test = test();
    test.signals.clear();
    test.expect.status = Some(RunStatus::Active);
    let report = run(&test).await;
    assert!(report.passed, "{report:#?}");
}

#[tokio::test]
async fn a_recorded_run_replays_as_a_test_with_its_seed_and_start() {
    // A "real" run, answered by an action that is not there in the test.
    let mut world = World::new(
        sleepy(),
        RecordingDispatcher::new(|_| Ok(json!({ "live": 42 }))),
    )
    .await;
    let run = world.run(request("sleepy", json!({}))).await;
    world
        .signal(&run, "approval", json!("yes"), None)
        .await
        .unwrap();
    world.idle().await;
    let original = world.history(&run).await;
    let recorded = Test::record(
        &original,
        TestOrigin {
            run: run.id,
            version: rewind_core::Digest::of(b"sleepy@1"),
            namespace: "alice".into(),
        },
    )
    .unwrap();
    assert_eq!(
        recorded.world.cassette.as_ref().unwrap().matching,
        Matching::ByArgs
    );
    assert_eq!(recorded.start, Some(T0));

    // The cassette answers the call and replays the signal at the time it arrived; the run
    // passes what it recorded, with the same journal call for call, but its start.
    let report = self::run(&recorded).await;
    assert!(report.passed, "{report:#?}");
    assert_eq!(report.history[1..], original[1..]);
    let (
        rewind_core::Event::Lifecycle(rewind_core::Lifecycle::RunStarted(replayed)),
        rewind_core::Event::Lifecycle(rewind_core::Lifecycle::RunStarted(started)),
    ) = (&report.history[0].event, &original[0].event)
    else {
        panic!("a history starts with run_started");
    };
    assert_eq!(replayed.seed, started.seed);

    // A version that calls `a` with other args gets no answer: the recorded test fails.
    let mut changed = recorded.clone();
    changed.world.cassette.as_mut().unwrap().calls[0].args = json!({ "n": 2 });
    let report = self::run(&changed).await;
    assert!(!report.passed);
    assert!(
        report
            .failures
            .iter()
            .any(|failure| failure.starts_with("the code called a({\"n\":1})")),
        "{:?}",
        report.failures
    );
}

#[tokio::test]
async fn a_suite_runs_in_order_and_stale_tests_do_not_run() {
    let code = code();
    let action_set = action_set();
    let subject = Subject {
        code: &code,
        action_set: &action_set,
        interface: None,
    };
    let mut first = test();
    first.name = "first".into();
    let mut stale = test();
    stale.name = "stale".into();
    stale.world.actions.insert(
        "c".into(),
        vec![ActionRule::always(ActionOutcome::Result(json!(1)))],
    );
    let mut last = test();
    last.name = "last".into();
    last.expect.status = Some(RunStatus::Completed);
    let reports = run_tests(
        &sleepy(),
        &NoMockLoader,
        &subject,
        &[first, stale, last],
        &TestOptions::default(),
    )
    .await
    .unwrap();
    let names: Vec<&str> = reports.iter().map(|report| report.name.as_str()).collect();
    assert_eq!(names, ["first", "stale", "last"]);
    assert!(reports[0].passed && reports[2].passed);
    assert_eq!(reports[1].problem, Some(TestProblem::Stale));
    assert_eq!(reports[1].status, None);
    assert_eq!(reports[1].failures, ["the action set has no action c"]);

    // A test with mock handlers where none can run fails, alone.
    let mut mocked = test();
    mocked.world.mocks = Some("export default {};".into());
    let report = run_test(
        &sleepy(),
        &NoMockLoader,
        &subject,
        &mocked,
        &TestOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        report.failures,
        ["the mock handlers cannot load: TypeScript mock handlers cannot run here"]
    );
}

#[tokio::test]
async fn rules_answer_by_args_and_static_responses_come_last() {
    let mut test = test();
    test.world.actions.insert(
        "a".into(),
        vec![ActionRule {
            args: Some(json!({ "n": 2 })),
            answer: ActionOutcome::Result(json!("not this one")),
            times: None,
        }],
    );
    test.expect.output = Some(json!({ "approved": true }));
    let code = code();
    let action_set = action_set();
    let subject = Subject {
        code: &code,
        action_set: &action_set,
        interface: None,
    };
    let options = TestOptions {
        ttl: None,
        statics: Some(json!({ "a": { "static": true } })),
    };
    let report = run_test(&sleepy(), &NoMockLoader, &subject, &test, &options)
        .await
        .unwrap();
    assert!(report.passed, "{report:#?}");
    let completed = report
        .history
        .iter()
        .find(|entry| entry.event.name() == "call_completed")
        .unwrap();
    assert!(format!("{:?}", completed.event).contains("static"));
}

#[test]
fn a_run_of_tests_is_a_future_a_server_can_spawn() {
    fn spawnable<T: Send>(_: &T) {}
    let code = code();
    let action_set = action_set();
    let subject = Subject {
        code: &code,
        action_set: &action_set,
        interface: None,
    };
    let tests = [test()];
    let executor = sleepy();
    let options = TestOptions::default();
    let run = run_tests(&executor, &NoMockLoader, &subject, &tests, &options);
    spawnable(&run);
}
