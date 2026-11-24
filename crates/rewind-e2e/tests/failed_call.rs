//! `ScriptError::failed_call` against the runtime's own `ActionError`s: in every journal of the
//! corpus that ends on an uncaught `ActionError`, it finds the call that failed. The API's
//! `failedCall` (`GET /v1/runs/{id}`) and the agent tools' failure text rest on it.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use rewind_core::{Command, Completion, Event, ScriptError, Seq};

#[test]
fn every_uncaught_action_error_of_the_corpus_names_its_call() {
    let mut found = BTreeMap::new();
    for color in ["blue", "red"] {
        let dir = rewind_e2e::corpus_dir().join("epoch-1").join(color);
        for case in std::fs::read_dir(&dir).unwrap() {
            let case = case.unwrap().path();
            let Ok(journal) = std::fs::read_to_string(case.join("journal.v1.json")) else {
                continue;
            };
            let document = rewind_format::decode_journal(&journal).unwrap();
            let Some(Event::Command(Command::FailRun { error })) =
                document.entries.last().map(|entry| &entry.event)
            else {
                continue;
            };
            if error.name != ScriptError::ACTION_ERROR {
                continue;
            }
            let name = format!("{color}/{}", case.file_name().unwrap().to_string_lossy());
            let seq = error
                .failed_call(&document.entries)
                .unwrap_or_else(|| panic!("{name}: no call for {error:?}"));
            let failed = document.entries.iter().any(|entry| {
                matches!(&entry.event,
                    Event::Completion(Completion::CallFailed { seq: failed, .. }) if *failed == seq)
            });
            assert!(failed, "{name}: call #{} did not fail", seq.0);
            found.insert(name, seq);
        }
    }
    assert_eq!(
        found.get("red/forgotten_await_fails"),
        Some(&Seq(1)),
        "{found:?}"
    );
    assert_eq!(
        found.get("red/unhandled_rejection"),
        Some(&Seq(1)),
        "{found:?}"
    );
    assert!(found.len() >= 5, "{found:?}");
}
