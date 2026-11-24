//! Format v1 is frozen: these fixtures were written once and must decode forever, to the same
//! domain values, and re-encode to the same bytes.

#![allow(clippy::unwrap_used)]

use rewind_core::{
    AbortKind, AbortReason, ActionFailure, ActionFailureKind, Annotation, Callsite, CancelReason,
    Command, Completion, Digest, Epoch, Event, Lifecycle, LogLevel, Payload, RunId, RunMode,
    RunStarted, RunState, RunStatus, ScriptError, Seed, Seq, Timestamp,
};
use rewind_format::{
    FormatError, decode_event, decode_run_export, encode_event, encode_run_export,
};
use serde_json::json;

const FIXTURES: [(&str, &str); 3] = [
    (
        "suspended-then-cancelled",
        include_str!("fixtures/suspended-then-cancelled.v1.json"),
    ),
    (
        "completed-after-ttl",
        include_str!("fixtures/completed-after-ttl.v1.json"),
    ),
    ("aborted", include_str!("fixtures/aborted.v1.json")),
];

fn digest(hex: &str) -> Digest {
    Digest::from_hex(hex).unwrap()
}

#[test]
fn every_fixture_decodes_to_a_valid_run_export_and_re_encodes_to_the_same_bytes() {
    for (name, fixture) in FIXTURES {
        let export = decode_run_export(fixture).unwrap_or_else(|err| panic!("{name}: {err}"));
        // The history is valid, and the code and action set are those of its run_started.
        export.check().unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(encode_run_export(&export), fixture, "{name}");
        for entry in &export.entries {
            let event = encode_event(&entry.event);
            assert_eq!(
                decode_event(&event).unwrap(),
                entry.event,
                "{name}: {event}"
            );
        }
    }
}

#[test]
fn decodes_to_the_expected_domain_values() {
    let export = decode_run_export(FIXTURES[0].1).unwrap();
    assert_eq!(
        export.run,
        "019b7a3c-8f00-7000-8000-000000000001"
            .parse::<RunId>()
            .unwrap()
    );
    assert!(
        export
            .code
            .source()
            .starts_with("import { github } from \"rewind:tools\";")
    );
    assert_eq!(export.action_set.actions().len(), 7);
    let history = export.entries;
    assert_eq!(history.len(), 16);
    assert_eq!(
        history[0].event,
        Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
            code: digest("88b30ba7a18c0a5dcd87a939c120336570ea6c747836b1a6b59ce93108d57c31"),
            action_set: digest("2ff7b72e331ef568c6041bd5c9509e3fb44ed29516f6b47bc815ea2a4dd537e3"),
            input: json!({ "repo": "acme/api" }).into(),
            seed: Seed::from_bytes(
                *digest("19b25856e1c150ca834cffc8b59b23adbd0ec0389e58eb22b3b64768098d002b")
                    .as_bytes()
            ),
            epoch: Epoch(1),
            expires_at: Timestamp(1_769_817_600_000),
            mode: RunMode::Live,
        }))
    );
    assert_eq!(history[0].recorded_at, Timestamp(1_767_225_600_000));
    assert_eq!(
        history[1].event,
        Event::Command(Command::ScheduleCall {
            seq: Seq(1),
            action: "github.list_issues".into(),
            args: json!({ "repo": "acme/api", "state": "open" }).into(),
            callsite: Some(Callsite {
                line: 5,
                column: 24
            }),
        })
    );
    assert_eq!(
        history[2].event,
        Event::Completion(Completion::CallCompleted {
            seq: Seq(1),
            result: Payload::Digest(digest(
                "f6a214f7a5fcda0c2cee9660b7fc29f5649e3c68aad48e20e950137c98913a68"
            )),
        })
    );
    assert_eq!(
        history[5].event,
        Event::Annotation(Annotation::SignalAwaited {
            name: "approval".into(),
            timeout: Some(Seq(2)),
            callsite: Some(Callsite {
                line: 8,
                column: 26
            }),
        })
    );
    assert_eq!(
        history[10].event,
        Event::Completion(Completion::CallFailed {
            seq: Seq(3),
            error: ActionFailure {
                kind: ActionFailureKind::OutcomeUnknown,
                message: "connection reset".into(),
                data: Some(json!({ "status": 503 })),
            },
        })
    );
    assert_eq!(
        history[14].event,
        Event::Completion(Completion::CancelRequested {
            reason: CancelReason::Requested(Some("user asked".into())),
        })
    );
    assert_eq!(
        RunState::from_history(&history).unwrap().status(),
        RunStatus::Cancelled
    );

    let ttl = decode_run_export(FIXTURES[1].1).unwrap().entries;
    assert_eq!(
        ttl[9].event,
        Event::Completion(Completion::CancelRequested {
            reason: CancelReason::Ttl
        })
    );
    assert_eq!(
        RunState::from_history(&ttl).unwrap().status(),
        RunStatus::Completed
    );

    let aborted = decode_run_export(FIXTURES[2].1).unwrap().entries;
    assert_eq!(
        aborted[1].event,
        Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
            kind: AbortKind::LimitExceeded,
            message: "the activation used more than 5000ms of CPU time".into(),
        }))
    );
}

/// The spelling of every enum value, one event each.
#[test]
fn pins_the_spelling_of_every_variant() {
    let abort = |kind| {
        Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
            kind,
            message: "m".into(),
        }))
    };
    let log = |level| {
        Event::Annotation(Annotation::Log {
            level,
            message: "m".into(),
            callsite: None,
        })
    };
    let failure = |kind| {
        Event::Completion(Completion::CallFailed {
            seq: Seq(1),
            error: ActionFailure {
                kind,
                message: "m".into(),
                data: None,
            },
        })
    };
    let cases = [
        (
            abort(AbortKind::LimitExceeded),
            r#"{"v":1,"type":"run_aborted","reason":{"kind":"limit_exceeded","message":"m"}}"#,
        ),
        (
            abort(AbortKind::NonDeterminism),
            r#"{"v":1,"type":"run_aborted","reason":{"kind":"non_determinism","message":"m"}}"#,
        ),
        (
            abort(AbortKind::Cancelled),
            r#"{"v":1,"type":"run_aborted","reason":{"kind":"cancelled","message":"m"}}"#,
        ),
        (
            abort(AbortKind::InvalidCode),
            r#"{"v":1,"type":"run_aborted","reason":{"kind":"invalid_code","message":"m"}}"#,
        ),
        (
            abort(AbortKind::Internal),
            r#"{"v":1,"type":"run_aborted","reason":{"kind":"internal","message":"m"}}"#,
        ),
        (
            log(LogLevel::Debug),
            r#"{"v":1,"type":"log","level":"debug","message":"m"}"#,
        ),
        (
            log(LogLevel::Info),
            r#"{"v":1,"type":"log","level":"info","message":"m"}"#,
        ),
        (
            log(LogLevel::Warn),
            r#"{"v":1,"type":"log","level":"warn","message":"m"}"#,
        ),
        (
            log(LogLevel::Error),
            r#"{"v":1,"type":"log","level":"error","message":"m"}"#,
        ),
        (
            failure(ActionFailureKind::Error),
            r#"{"v":1,"type":"call_failed","seq":1,"error":{"kind":"error","message":"m"}}"#,
        ),
        (
            failure(ActionFailureKind::Unavailable),
            r#"{"v":1,"type":"call_failed","seq":1,"error":{"kind":"unavailable","message":"m"}}"#,
        ),
        (
            failure(ActionFailureKind::Timeout),
            r#"{"v":1,"type":"call_failed","seq":1,"error":{"kind":"timeout","message":"m"}}"#,
        ),
        (
            failure(ActionFailureKind::OutcomeUnknown),
            r#"{"v":1,"type":"call_failed","seq":1,"error":{"kind":"outcome_unknown","message":"m"}}"#,
        ),
        (
            Event::Completion(Completion::CancelRequested {
                reason: CancelReason::Requested(None),
            }),
            r#"{"v":1,"type":"cancel_requested","reason":{"kind":"requested"}}"#,
        ),
        (
            Event::Command(Command::FailRun {
                error: ScriptError {
                    name: "DeadlockError".into(),
                    message: "m".into(),
                    stack: None,
                },
            }),
            r#"{"v":1,"type":"fail_run","error":{"name":"DeadlockError","message":"m"}}"#,
        ),
    ];
    for (event, json) in cases {
        assert_eq!(encode_event(&event), json);
        assert_eq!(decode_event(json).unwrap(), event, "{json}");
    }
}

/// Values are stored in their canonical form (RFC 8785), whatever order their keys were built
/// in and however the JSON library writes numbers.
#[test]
fn values_are_written_in_their_canonical_form() {
    let value: serde_json::Value =
        serde_json::from_str(r#"{"z":1e21,"a":[1.50,{"y":true,"b":null}],"é":"\u00e9"}"#).unwrap();
    let event = Event::Completion(Completion::CallFailed {
        seq: Seq(1),
        error: ActionFailure {
            kind: ActionFailureKind::Error,
            message: "m".into(),
            data: Some(value.clone()),
        },
    });
    assert_eq!(
        encode_event(&event),
        r#"{"v":1,"type":"call_failed","seq":1,"error":{"kind":"error","message":"m","data":{"a":[1.5,{"b":null,"y":true}],"z":1e+21,"é":"é"}}}"#
    );
    let result = Event::Completion(Completion::CallCompleted {
        seq: Seq(1),
        result: value.clone().into(),
    });
    assert_eq!(
        encode_event(&result),
        r#"{"v":1,"type":"call_completed","seq":1,"result":{"value":{"a":[1.5,{"b":null,"y":true}],"z":1e+21,"é":"é"}}}"#
    );
    assert_eq!(decode_event(&encode_event(&result)).unwrap(), result);
}

#[test]
fn rejects_what_v1_does_not_define() {
    let event = r#"{"v":1,"type":"timer_fired","seq":1}"#;
    assert!(decode_event(event).is_ok());

    assert_eq!(
        decode_event(r#"{"type":"timer_fired","seq":1}"#),
        Err(FormatError::MissingVersion)
    );
    assert_eq!(
        decode_event(r#"{"v":2,"type":"timer_fired","seq":1}"#),
        Err(FormatError::UnsupportedVersion(json!(2)))
    );
    assert_eq!(
        decode_event(r#"{"v":"1","type":"timer_fired","seq":1}"#),
        Err(FormatError::UnsupportedVersion(json!("1")))
    );
    assert_eq!(decode_event("[1]"), Err(FormatError::NotAnObject));
    assert!(matches!(decode_event("{"), Err(FormatError::Json(_))));

    let invalid = [
        r#"{"v":1,"type":"timer_fired","seq":1,"extra":true}"#,
        r#"{"v":1,"type":"timer_exploded","seq":1}"#,
        r#"{"v":1,"type":"timer_fired"}"#,
        r#"{"v":1,"type":"timer_fired","seq":-1}"#,
        r#"{"v":1,"type":"call_completed","seq":1,"result":{"value":1,"digest":"00"}}"#,
        r#"{"v":1,"type":"call_completed","seq":1,"result":{"digest":"F6A214F7A5FCDA0C2CEE9660B7FC29F5649E3C68AAD48E20E950137C98913A68"}}"#,
        r#"{"v":1,"type":"call_completed","seq":1,"result":{"digest":"f6a2"}}"#,
        r#"{"v":1,"type":"cancel_requested","reason":{"kind":"bored"}}"#,
        r#"{"v":1,"type":"cancel_requested","reason":{"kind":"ttl","extra":1}}"#,
        r#"{"v":1,"type":"log","level":"trace","message":"m"}"#,
    ];
    for json in invalid {
        assert!(
            matches!(decode_event(json), Err(FormatError::InvalidEvent(_))),
            "{json} must be rejected, got {:?}",
            decode_event(json)
        );
    }

    let action_set = r#"{"actions":[],"servers":[],"v":1}"#;
    let document = |fields: &str| format!(r#"{{"v":1,{fields}}}"#);
    let valid = document(&format!(
        r#""run":"019b7a3c-8f00-7000-8000-000000000003","code":"s","action_set":{action_set},"entries":[]"#
    ));
    assert_eq!(decode_run_export(&valid).unwrap().entries, vec![]);
    let invalid_documents = [
        // An unknown field.
        format!(
            r#""run":"019b7a3c-8f00-7000-8000-000000000003","code":"s","action_set":{action_set},"entries":[],"x":1"#
        ),
        // A missing field.
        r#""entries":[]"#.to_owned(),
        format!(r#""code":"s","action_set":{action_set},"entries":[]"#),
        format!(
            r#""run":"019b7a3c-8f00-7000-8000-000000000003","action_set":{action_set},"entries":[]"#
        ),
        r#""run":"019b7a3c-8f00-7000-8000-000000000003","code":"s","entries":[]"#.to_owned(),
        // A run that is not a run id.
        format!(r#""run":"x","code":"s","action_set":{action_set},"entries":[]"#),
        // A code that is not a source.
        format!(
            r#""run":"019b7a3c-8f00-7000-8000-000000000003","code":1,"action_set":{action_set},"entries":[]"#
        ),
    ];
    for json in invalid_documents {
        let json = document(&json);
        assert!(
            matches!(
                decode_run_export(&json),
                Err(FormatError::InvalidDocument(_))
            ),
            "{json} must be rejected, got {:?}",
            decode_run_export(&json)
        );
    }
    assert!(matches!(
        decode_run_export(&document(
            r#""run":"019b7a3c-8f00-7000-8000-000000000003","code":"s","action_set":{"servers":[],"v":1},"entries":[]"#
        )),
        Err(FormatError::InvalidActionSet(_))
    ));
    assert!(matches!(
        decode_run_export(&document(&format!(
            r#""run":"019b7a3c-8f00-7000-8000-000000000003","code":"s","action_set":{action_set},"entries":[{{"index":0,"recorded_at":0}}]"#
        ))),
        Err(FormatError::InvalidEntry { position: 0, .. })
    ));
}
