//! Format v1 is frozen: these fixtures were written once and must decode forever, to the same
//! domain values, and re-encode to the same bytes.

#![allow(clippy::unwrap_used)]

use ptc_core::{
    AbortKind, AbortReason, Annotation, Callsite, CancelReason, Command, Completion, Digest, Epoch,
    Event, Lifecycle, LogLevel, Payload, RunMode, RunStarted, RunState, RunStatus, ScriptError,
    Seed, Seq, Timestamp, ToolFailure, ToolFailureKind, validate_history,
};
use ptc_format::{FormatError, decode_event, decode_history, encode_event, encode_history};
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
fn every_fixture_decodes_to_a_valid_history_and_re_encodes_to_the_same_bytes() {
    for (name, fixture) in FIXTURES {
        let history = decode_history(fixture).unwrap_or_else(|err| panic!("{name}: {err}"));
        validate_history(&history).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(encode_history(&history), fixture, "{name}");
        for entry in &history {
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
    let history = decode_history(FIXTURES[0].1).unwrap();
    assert_eq!(history.len(), 16);
    assert_eq!(
        history[0].event,
        Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
            script: digest("21a0270b7f66a1e4c25933f13a1e5a1bbb4757578072930c8189131f9c6aaae1"),
            toolset: digest("5598149567a46bd3164b1d42e871b4d65cf49b78dd8b4547436efd1784b533c6"),
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
        Event::Command(Command::ScheduleToolCall {
            seq: Seq(1),
            tool: "github.list_issues".into(),
            args: json!({ "repo": "acme/api", "state": "open" }).into(),
            callsite: Some(Callsite {
                line: 5,
                column: 24
            }),
        })
    );
    assert_eq!(
        history[2].event,
        Event::Completion(Completion::ToolCallCompleted {
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
        Event::Completion(Completion::ToolCallFailed {
            seq: Seq(3),
            error: ToolFailure {
                kind: ToolFailureKind::OutcomeUnknown,
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

    let ttl = decode_history(FIXTURES[1].1).unwrap();
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

    let aborted = decode_history(FIXTURES[2].1).unwrap();
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
        Event::Completion(Completion::ToolCallFailed {
            seq: Seq(1),
            error: ToolFailure {
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
            abort(AbortKind::InvalidScript),
            r#"{"v":1,"type":"run_aborted","reason":{"kind":"invalid_script","message":"m"}}"#,
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
            failure(ToolFailureKind::Error),
            r#"{"v":1,"type":"tool_call_failed","seq":1,"error":{"kind":"error","message":"m"}}"#,
        ),
        (
            failure(ToolFailureKind::Unavailable),
            r#"{"v":1,"type":"tool_call_failed","seq":1,"error":{"kind":"unavailable","message":"m"}}"#,
        ),
        (
            failure(ToolFailureKind::Timeout),
            r#"{"v":1,"type":"tool_call_failed","seq":1,"error":{"kind":"timeout","message":"m"}}"#,
        ),
        (
            failure(ToolFailureKind::OutcomeUnknown),
            r#"{"v":1,"type":"tool_call_failed","seq":1,"error":{"kind":"outcome_unknown","message":"m"}}"#,
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
    let event = Event::Completion(Completion::ToolCallFailed {
        seq: Seq(1),
        error: ToolFailure {
            kind: ToolFailureKind::Error,
            message: "m".into(),
            data: Some(value.clone()),
        },
    });
    assert_eq!(
        encode_event(&event),
        r#"{"v":1,"type":"tool_call_failed","seq":1,"error":{"kind":"error","message":"m","data":{"a":[1.5,{"b":null,"y":true}],"z":1e+21,"é":"é"}}}"#
    );
    let result = Event::Completion(Completion::ToolCallCompleted {
        seq: Seq(1),
        result: value.clone().into(),
    });
    assert_eq!(
        encode_event(&result),
        r#"{"v":1,"type":"tool_call_completed","seq":1,"result":{"value":{"a":[1.5,{"b":null,"y":true}],"z":1e+21,"é":"é"}}}"#
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
        r#"{"v":1,"type":"tool_call_completed","seq":1,"result":{"value":1,"digest":"00"}}"#,
        r#"{"v":1,"type":"tool_call_completed","seq":1,"result":{"digest":"F6A214F7A5FCDA0C2CEE9660B7FC29F5649E3C68AAD48E20E950137C98913A68"}}"#,
        r#"{"v":1,"type":"tool_call_completed","seq":1,"result":{"digest":"f6a2"}}"#,
        r#"{"v":1,"type":"cancel_requested","reason":{"kind":"bored"}}"#,
        r#"{"v":1,"type":"log","level":"trace","message":"m"}"#,
    ];
    for json in invalid {
        assert!(
            matches!(decode_event(json), Err(FormatError::InvalidEvent(_))),
            "{json} must be rejected, got {:?}",
            decode_event(json)
        );
    }

    assert!(matches!(
        decode_history(r#"{"v":1,"entries":[],"run":"x"}"#),
        Err(FormatError::InvalidDocument(_))
    ));
    assert!(matches!(
        decode_history(r#"{"v":1}"#),
        Err(FormatError::InvalidDocument(_))
    ));
    assert!(matches!(
        decode_history(r#"{"v":1,"entries":[{"index":0,"recorded_at":0}]}"#),
        Err(FormatError::InvalidEntry { position: 0, .. })
    ));
    assert_eq!(decode_history(r#"{"v":1,"entries":[]}"#), Ok(vec![]));
}
