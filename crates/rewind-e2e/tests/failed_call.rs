//! `ErrorInfo::failed_call` against the runtime's own `ActionError`s: in every history of the
//! corpus that ends on an uncaught `ActionError` (a `ToolError` in the histories of epoch 1), it
//! finds the call that failed. The API's
//! `failedCall` (`GET /v1/runs/{id}`) and the agent tools' failure text rest on it.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use rewind_core::{Command, Completion, ErrorInfo, Event, Seq};

#[test]
fn every_uncaught_action_error_of_the_corpus_names_its_call() {
    let mut found = BTreeMap::new();
    for (epoch, color) in [
        ("epoch-1", "blue"),
        ("epoch-1", "red"),
        ("epoch-2", "blue"),
        ("epoch-2", "red"),
    ] {
        let dir = rewind_e2e::corpus_dir().join(epoch).join(color);
        for case in std::fs::read_dir(&dir).unwrap() {
            let case = case.unwrap().path();
            let document = if let Ok(export) = std::fs::read_to_string(case.join("history.v1.json"))
            {
                rewind_format::decode_run_export(&export).unwrap()
            } else if let Ok(journal) = std::fs::read_to_string(case.join("journal.v1.json")) {
                rewind_format::rename::run_export(
                    &journal,
                    "local",
                    &rewind_format::rename::NoContext,
                )
                .unwrap()
            } else {
                continue;
            };
            let Some(Event::Command(Command::FailRun { error })) =
                document.events.last().map(|entry| &entry.event)
            else {
                continue;
            };
            if error.name != ErrorInfo::ACTION_ERROR && error.name != ErrorInfo::TOOL_ERROR {
                continue;
            }
            let name = format!(
                "{epoch}/{color}/{}",
                case.file_name().unwrap().to_string_lossy()
            );
            let seq = error
                .failed_call(&document.events)
                .unwrap_or_else(|| panic!("{name}: no call for {error:?}"));
            let failed = document.events.iter().any(|entry| {
                matches!(&entry.event,
                    Event::Completion(Completion::CallFailed { seq: failed, .. }) if *failed == seq)
            });
            assert!(failed, "{name}: call #{} did not fail", seq.0);
            found.insert(name, seq);
        }
    }
    assert_eq!(
        found.get("epoch-2/red/forgotten_await_fails"),
        Some(&Seq(1)),
        "{found:?}"
    );
    assert_eq!(
        found.get("epoch-2/red/unhandled_rejection"),
        Some(&Seq(1)),
        "{found:?}"
    );
    assert!(
        found
            .keys()
            .filter(|name| name.starts_with("epoch-1/"))
            .count()
            >= 3,
        "the ToolErrors of epoch 1 name their calls: {found:?}"
    );
    assert!(found.len() >= 8, "{found:?}");
}
