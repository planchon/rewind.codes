//! The harness cannot pass silently: a wrong expectation, an altered history or a case in the
//! wrong color is reported. These checks never record: they alter cases in memory.

#![allow(clippy::unwrap_used)]

use rewind_core::{Code, Command, Event, Payload};
use rewind_e2e::{Case, Color, Mode, Replayed};
use serde_json::json;

#[tokio::test]
async fn a_wrong_expectation_is_reported() {
    let mut case = Case::load("epoch-1/blue/durable_sleep").unwrap();
    case.scenario.expect.output = Some(json!({ "elapsed": 1 }));
    case.scenario.expect.events = Some(vec!["run_started".into()]);
    let failures = rewind_e2e::check(&case, Mode::Verify).await;
    assert_eq!(failures.len(), 2, "{failures:#?}");
    assert!(failures[0].contains("does not contain"), "{failures:#?}");
    assert!(failures[1].starts_with("events are"), "{failures:#?}");
}

#[tokio::test]
async fn a_history_that_no_longer_matches_its_code_does_not_replay() {
    let case = Case::load("epoch-1/blue/fan_out_out_of_order").unwrap();
    let export = std::fs::read_to_string(case.history_path()).unwrap();
    let mut document = rewind_format::decode_run_export(&export).unwrap();
    let Event::Command(Command::ScheduleCall { args, .. }) = &mut document.events[2].event else {
        panic!("event 2 is the second call");
    };
    *args = Payload::Value(json!({ "number": 20, "repo": "acme/api" }));

    let replayed = rewind_e2e::replay_export(&case, &document, &case.code).await;
    let Replayed::Fails(message) = replayed else {
        panic!("{replayed:?}")
    };
    assert!(message.starts_with("event 2 is call #2"), "{message}");

    // The same history, untouched, replays.
    let document = rewind_format::decode_run_export(&export).unwrap();
    assert_eq!(document.code, case.code);
    assert!(matches!(
        rewind_e2e::replay_export(&case, &document, &case.code).await,
        Replayed::Matches(_)
    ));
}

#[tokio::test]
async fn a_script_that_changed_is_reported_by_the_recording() {
    let mut case = Case::load("epoch-1/blue/forgotten_await").unwrap();
    case.code = Code::new(case.code.source().replace("triaged", "closed"));
    let failures = rewind_e2e::check(&case, Mode::Verify).await;
    assert!(
        failures
            .iter()
            .any(|failure| failure.starts_with("the history differs from")),
        "{failures:#?}"
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure.starts_with("the committed history does not replay")),
        "{failures:#?}"
    );
}

#[test]
fn a_case_in_the_wrong_color_is_refused() {
    let mut blue = Case::load("epoch-1/blue/durable_sleep").unwrap();
    blue.scenario.expect.status = Some("failed".into());
    assert!(blue.check_color().unwrap_err().contains("move it to red/"));

    let mut red = Case::load("epoch-1/red/deadlock").unwrap();
    assert_eq!(red.color, Color::Red);
    red.scenario.expect.status = Some("completed".into());
    red.scenario.expect.error = None;
    assert!(
        red.check_color()
            .unwrap_err()
            .contains("must expect a failure")
    );
}
