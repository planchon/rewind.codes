//! The harness cannot pass silently: a wrong expectation, an altered journal or a case in the
//! wrong color is reported. These checks never record: they alter cases in memory.

#![allow(clippy::unwrap_used)]

use ptc_core::{Command, Event, Payload, Script};
use ptc_e2e::{Case, Color, Mode, Replayed};
use serde_json::json;

#[tokio::test]
async fn a_wrong_expectation_is_reported() {
    let mut case = Case::load("epoch-1/blue/durable_sleep").unwrap();
    case.scenario.expect.output = Some(json!({ "elapsed": 1 }));
    case.scenario.expect.events = Some(vec!["run_started".into()]);
    let failures = ptc_e2e::check(&case, Mode::Verify).await;
    assert_eq!(failures.len(), 2, "{failures:#?}");
    assert!(failures[0].contains("does not contain"), "{failures:#?}");
    assert!(failures[1].starts_with("events are"), "{failures:#?}");
}

#[tokio::test]
async fn a_journal_that_no_longer_matches_its_script_does_not_replay() {
    let case = Case::load("epoch-1/blue/fan_out_out_of_order").unwrap();
    let journal = std::fs::read_to_string(case.journal_path()).unwrap();
    let mut document = ptc_format::decode_journal(&journal).unwrap();
    let Event::Command(Command::ScheduleToolCall { args, .. }) = &mut document.entries[2].event
    else {
        panic!("entry 2 is the second call");
    };
    *args = Payload::Value(json!({ "number": 20, "repo": "acme/api" }));

    let replayed = ptc_e2e::replay_journal(&case, &document, &case.script).await;
    let Replayed::Fails(message) = replayed else {
        panic!("{replayed:?}")
    };
    assert!(
        message.starts_with("history entry 2 is tool call #2"),
        "{message}"
    );

    // The same journal, untouched, replays.
    let document = ptc_format::decode_journal(&journal).unwrap();
    assert_eq!(document.script, case.script);
    assert!(matches!(
        ptc_e2e::replay_journal(&case, &document, &case.script).await,
        Replayed::Matches(_)
    ));
}

#[tokio::test]
async fn a_script_that_changed_is_reported_by_the_recording() {
    let mut case = Case::load("epoch-1/blue/forgotten_await").unwrap();
    case.script = Script::new(case.script.source().replace("triaged", "closed"));
    let failures = ptc_e2e::check(&case, Mode::Verify).await;
    assert!(
        failures
            .iter()
            .any(|failure| failure.starts_with("the journal differs from")),
        "{failures:#?}"
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure.starts_with("the committed journal does not replay")),
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
