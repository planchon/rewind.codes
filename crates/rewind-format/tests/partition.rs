//! The partition records v1 are frozen: each fixture of `fixtures/partition/` was written once and
//! must decode forever to the same record, and re-encode to the same bytes. A reader refuses an
//! unknown version, field or variant.
//!
//! `REWIND_FORMAT_RECORD=1` writes the fixtures again from the records below: only for a record
//! added to the format, never to change one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use rewind_core::{
    ActionPolicy, ActionRoute, Attachments, Builtin, Completion, Digest, Epoch, Event, Lifecycle,
    Payload, RunId, RunMode, RunStarted, SecretRef, Seed, Shape as CoreShape, Timestamp,
    WorkflowOp,
};
use rewind_format::FormatError;
use rewind_format::partition::{
    ActionCounts, Admission, Attribute, Callsite, Counters, CredentialsReason, Ending,
    FailureCounts, FailureKind, ForkSource, Gauges, HistoryEvent, InboxItem, LatestRun, Level, Log,
    Metadata, Mode, Organization, Outbox, ParkKind, PayloadValue, Projection, Quarantine,
    QuarantineKind, QuarantineNote, QuotaAlert, QuotaKind, Record, RunCounts, RunHead, RunWorkflow,
    ScriptCounts, Shape, SignalKey, SignalUrlToken, Sketch, Span, SpanKind, SpanStatus, StartKey,
    StartedBy, Status, Summary, Task, TaskPhase, TaskState, Threshold, Timer, TimerKind,
    TraceContext, Wait, WaitKind, WaitingFor, Watcher, WebhookEvent, WebhookKind, decode, encode,
};
use serde_json::json;

const T0: u64 = 1_767_225_600_000;

fn run(n: u128) -> RunId {
    format!("019b7a3c-8f00-8003-8000-{n:012x}").parse().unwrap()
}

fn digest(text: &str) -> Digest {
    Digest::of(text.as_bytes())
}

fn wait() -> Wait {
    Wait {
        kind: WaitKind::Call,
        seq: Some(2),
        name: Some("github.merge".into()),
        since: T0 + 5,
        until: Some(T0 + 60_000),
        callsite: Some(Callsite {
            line: 12,
            column: 7,
        }),
        task: Some(TaskPhase::Approval),
    }
}

fn alert() -> QuotaAlert {
    QuotaAlert {
        tenant: "acme".into(),
        namespace: Some("alice".into()),
        quota: QuotaKind::CpuPerMonth,
        year: 2026,
        month: 1,
        threshold: Threshold::Near,
        used: 80,
        limit: 100,
        raised_at: T0,
        sent_at: Some(T0 + 10),
    }
}

fn sketch() -> Sketch {
    Sketch {
        count: 2,
        sum_ms: 1_120,
        max_ms: 1_000,
        buckets: BTreeMap::from([(56, 1), (104, 1)]),
    }
}

fn run_head() -> RunHead {
    let mut secrets = Attachments::new();
    secrets.insert("github", SecretRef::parse("ns/github").unwrap());
    RunHead {
        run: run(1),
        tenant: "acme".into(),
        namespace: "alice".into(),
        origin: "script".into(),
        suffix_len: 12,
        code: digest("script"),
        action_set: digest("action_set"),
        status: Status::Active,
        epoch: 1,
        attempt: 1,
        sealed: None,
        failures: 1,
        attempts: 2,
        quarantine: Some(Quarantine {
            kind: QuarantineKind::NonDeterminism,
            reason: "replay mismatch at event 3".into(),
            since: T0 + 50,
            retried_by: Some("build-2".into()),
        }),
        waits: vec![wait()],
        expires_at: T0 + 86_400_000,
        history_len: 4,
        metadata: Metadata {
            labels: BTreeMap::from([("agent".into(), "triage".into())]),
            trace_context: Some(TraceContext {
                traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
                tracestate: None,
            }),
            source: Some(ForkSource {
                run: run(9),
                at: Some(3),
            }),
            workflow: Some(RunWorkflow {
                name: "triage".into(),
                version: digest("version"),
            }),
            started_by: Some(StartedBy {
                run: run(8),
                seq: 4,
                detached: false,
            }),
            scope: BTreeMap::from([("env".into(), "dev".into())]),
            secrets,
        },
        idempotency_key: Some("start-1".into()),
        created_at: T0,
        updated_at: T0 + 5,
        ended_at: None,
    }
}

fn history_event() -> HistoryEvent {
    HistoryEvent {
        index: 0,
        recorded_at: T0,
        event: Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
            code: digest("script"),
            action_set: digest("action_set"),
            input: json!({ "b": 1, "a": [1.5, "x"] }).into(),
            seed: Seed::from_bytes([9; 32]),
            epoch: Epoch(1),
            expires_at: Timestamp(T0 + 86_400_000),
            mode: RunMode::Live,
        })),
    }
}

fn inbox_item() -> InboxItem {
    InboxItem {
        id: 2,
        received_at: T0 + 7,
        trigger: Event::Completion(Completion::SignalReceived {
            name: "approval".into(),
            payload: Payload::Digest(digest("payload")),
        }),
    }
}

fn task() -> Task {
    Task {
        tenant: "acme".into(),
        run: run(1),
        seq: 2,
        action_set: digest("action_set"),
        action: "workflows.triage.start".into(),
        route: ActionRoute::Builtin(Builtin::Workflow {
            workflow: "triage".into(),
            op: WorkflowOp::Start,
        }),
        args: json!({ "input": { "issue": 7 } }).into(),
        policy: ActionPolicy {
            idempotent: false,
            approval: true,
            execution_timeout: Some(Duration::from_secs(30)),
            reconnect_timeout: Duration::from_secs(60),
            concurrency: 4,
        },
        state: TaskState::Parked {
            kind: ParkKind::CredentialsRefused,
            message: "401 from github".into(),
        },
        delivery: 1,
        schema_change_approved: true,
        created_at: T0 + 5,
        token_hash: Some(digest("token")),
    }
}

fn timer() -> Timer {
    Timer {
        at: T0 + 1_000,
        tenant: "acme".into(),
        kind: TimerKind::Code {
            run: run(1),
            seq: 3,
        },
    }
}

fn payload_value() -> PayloadValue {
    PayloadValue {
        digest: digest("payload"),
        size: 1_200,
        preview: "{\"text\":".into(),
        value: Some(json!({ "text": "large", "n": 1e21, "z": null })),
    }
}

fn summary() -> Summary {
    Summary {
        tenant: "acme".into(),
        run: run(1),
        namespace: "alice".into(),
        origin: "script".into(),
        suffix_len: 12,
        code: digest("script"),
        headline: "export default async () => {".into(),
        action_set: digest("action_set"),
        status: Status::Failed,
        mode: Some(Mode::Test),
        labels: BTreeMap::from([("agent".into(), "triage".into())]),
        source: Some(run(9)),
        workflow: Some("triage".into()),
        started_by: Some(run(8)),
        started_at: Some(T0),
        ended_at: Some(T0 + 100),
        last_activity_at: T0 + 100,
        calls: 3,
        errors: 1,
        history_len: 9,
        waits: Vec::new(),
        wakes_at: None,
        ending: Some(Ending {
            error: "TypeError".into(),
            message: "x is not a function".into(),
            line: Some(7),
            signature: "TypeError: x is not a function".into(),
            fingerprint: "0123456789abcdef".into(),
        }),
        quarantine: Some(QuarantineNote {
            kind: QuarantineKind::Failures,
            reason: "5 failures".into(),
            since: T0 + 90,
        }),
        actions: BTreeSet::from(["github.close_issue".into()]),
        call_errors: BTreeSet::from([FailureKind::Timeout]),
    }
}

fn counters() -> Counters {
    let runs = RunCounts {
        started: 2,
        ended: BTreeMap::from([(Status::Completed, 1), (Status::Failed, 1)]),
    };
    Counters {
        created: 2,
        runs: runs.clone(),
        durations: sketch(),
        cpu_ms: 1_500,
        workflows: BTreeMap::from([("triage".into(), runs.clone())]),
        failures: BTreeMap::from([(
            "0123456789abcdef".into(),
            FailureCounts {
                signature: "TypeError: x is not a function".into(),
                error: "TypeError".into(),
                runs: 1,
                scripts: BTreeMap::from([(digest("script"), 1)]),
                lines: BTreeMap::from([(7, 1)]),
                latest: vec![LatestRun {
                    run: run(1),
                    ended_at: T0 + 100,
                    status: Status::Failed,
                    message: "x is not a function".into(),
                }],
            },
        )]),
        actions: BTreeMap::from([(
            "github.close_issue".into(),
            ActionCounts {
                calls: 2,
                completed: 1,
                failed: 1,
                errors: BTreeMap::from([(FailureKind::Timeout, 1)]),
                redeliveries: 1,
                latency: sketch(),
            },
        )]),
        scripts: BTreeMap::from([(
            digest("script"),
            ScriptCounts {
                runs,
                actions: BTreeSet::from(["github.close_issue".into()]),
                first_started_at: Some(T0),
                last_started_at: Some(T0 + 50),
            },
        )]),
    }
}

fn shape() -> Shape {
    let a = CoreShape::of(&json!({ "a": 1, "list": [] }));
    let b = CoreShape::of(&json!({ "b": "x", "list": [true] }));
    Shape { shape: a.merge(b) }
}

fn gauges() -> Gauges {
    Gauges {
        active_runs: 4,
        waiting: BTreeMap::from([(WaitingFor::Signal, 2), (WaitingFor::Approval, 1)]),
        quarantined: 1,
        scripts: BTreeMap::from([(digest("script"), 3), (digest("second"), 1)]),
    }
}

fn outbox_otel() -> Outbox {
    Outbox {
        id: 1,
        tenant: "acme".into(),
        run: Some(run(1)),
        projection: Projection::Otel {
            spans: vec![Span {
                trace_id: [0x0a; 16],
                span_id: [0xb7; 8],
                parent: Some([0x01; 8]),
                name: "action github.close_issue".into(),
                kind: SpanKind::Client,
                start: T0,
                end: T0 + 120,
                status: SpanStatus::Error {
                    message: "timeout".into(),
                },
                attributes: BTreeMap::from([
                    ("rewind.seq".into(), Attribute::Int(2)),
                    ("rewind.ratio".into(), Attribute::Float(0.5)),
                    ("rewind.infinite".into(), Attribute::Float(f64::INFINITY)),
                    ("rewind.retried".into(), Attribute::Bool(true)),
                    ("rewind.action".into(), Attribute::String("github".into())),
                ]),
            }],
            logs: vec![Log {
                trace_id: [0x0a; 16],
                span_id: None,
                time: T0 + 3,
                level: Level::Warn,
                message: "slow".into(),
                attributes: BTreeMap::new(),
            }],
        },
    }
}

fn outbox_webhooks() -> Outbox {
    Outbox {
        id: 2,
        tenant: "acme".into(),
        run: Some(run(1)),
        projection: Projection::Webhooks {
            event: WebhookEvent {
                id: format!("{}/2/1", run(1)),
                tenant: "acme".into(),
                namespace: Some("alice".into()),
                run: Some(run(1)),
                labels: BTreeMap::from([("agent".into(), "triage".into())]),
                occurred_at: T0 + 5,
                kind: WebhookKind::CredentialsExpired {
                    seq: 2,
                    action: "github.close_issue".into(),
                    message: "401".into(),
                    reason: CredentialsReason::Refused,
                    secret: Some(SecretRef::parse("org/github").unwrap()),
                },
            },
        },
    }
}

fn outbox_alert() -> Outbox {
    Outbox {
        id: 3,
        tenant: "acme".into(),
        run: None,
        projection: Projection::QuotaAlert { alert: alert() },
    }
}

fn organization() -> Organization {
    Organization {
        alerts: vec![alert()],
        admissions: vec![Admission {
            run: run(2),
            namespace: "alice".into(),
            code: digest("script"),
            admitted_at: T0,
        }],
    }
}

/// Checks one fixture: it decodes to `record` and re-encodes to its bytes.
fn golden<R: Record + PartialEq + std::fmt::Debug>(name: &str, record: R) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/partition")
        .join(format!("{name}.v1.json"));
    let encoded = encode(&record);
    if std::env::var_os("REWIND_FORMAT_RECORD").is_some() {
        let mut bytes = encoded.clone();
        bytes.push(b'\n');
        std::fs::write(&path, bytes).unwrap();
    }
    let fixture = std::fs::read(&path).unwrap_or_else(|err| panic!("{name}: {err}"));
    let fixture = fixture.strip_suffix(b"\n").unwrap_or(&fixture);
    assert_eq!(
        String::from_utf8_lossy(&encoded),
        String::from_utf8_lossy(fixture),
        "{name}: the bytes"
    );
    assert_eq!(decode::<R>(fixture).unwrap(), record, "{name}: the record");
}

#[test]
fn every_record_decodes_from_its_fixture_and_re_encodes_to_the_same_bytes() {
    golden("run-head", run_head());
    golden("history-event", history_event());
    golden("inbox-item", inbox_item());
    golden("signal-key", SignalKey {});
    golden("task", task());
    golden("timer", timer());
    golden("payload", payload_value());
    golden(
        "watcher",
        Watcher {
            run: run(2),
            seq: 4,
            attached: true,
        },
    );
    golden(
        "webhook-token",
        SignalUrlToken {
            tenant: "acme".into(),
            run: run(1),
            name: "approval".into(),
        },
    );
    golden("summary", summary());
    golden("start-key", StartKey { run: run(1) });
    golden("counters", counters());
    golden("shape", shape());
    golden("gauges", gauges());
    golden("outbox-otel", outbox_otel());
    golden("outbox-webhooks", outbox_webhooks());
    golden("outbox-quota-alert", outbox_alert());
    golden("organization", organization());
}

#[test]
fn every_variant_round_trips() {
    let states = [
        TaskState::Approval {},
        TaskState::Ready {
            next_delivery_at: T0,
        },
        TaskState::Claimed {
            owner: "node-1".into(),
            until: T0,
        },
        TaskState::Deferred { until: T0 },
        TaskState::Parked {
            kind: ParkKind::SchemaChanged,
            message: "x".into(),
        },
        TaskState::Parked {
            kind: ParkKind::SecretMissing,
            message: "x".into(),
        },
    ];
    let routes = [
        ActionRoute::Mcp {
            server: "github".into(),
        },
        ActionRoute::Api { api: "dax".into() },
        ActionRoute::Harness { group: None },
        ActionRoute::Harness {
            group: Some("ui".into()),
        },
        ActionRoute::Builtin(Builtin::SignalUrl),
        ActionRoute::Builtin(Builtin::Http {
            allowed_hosts: vec!["api.example.com".into()],
        }),
        ActionRoute::Builtin(Builtin::Workflow {
            workflow: "triage".into(),
            op: WorkflowOp::Signal,
        }),
    ];
    for (state, route) in states.into_iter().zip(routes.iter().cycle()) {
        let record = Task {
            state,
            route: route.clone(),
            token_hash: None,
            schema_change_approved: false,
            ..task()
        };
        assert_eq!(decode::<Task>(&encode(&record)).unwrap(), record);
    }
    for route in routes {
        let record = Task { route, ..task() };
        assert_eq!(decode::<Task>(&encode(&record)).unwrap(), record);
    }
    let kinds = [
        TimerKind::Code {
            run: run(1),
            seq: 1,
        },
        TimerKind::CallTimeout {
            run: run(1),
            seq: 1,
        },
        TimerKind::DeferredDeadline {
            run: run(1),
            seq: 1,
        },
        TimerKind::RedeliveryBackoff {
            run: run(1),
            seq: 1,
        },
        TimerKind::RunDeadline { run: run(1) },
        TimerKind::ActivationRetry { run: run(1) },
        TimerKind::RetentionEnd { run: run(1) },
        TimerKind::WebhookRetry { entry: 7 },
    ];
    for kind in kinds {
        let record = Timer { kind, ..timer() };
        assert_eq!(decode::<Timer>(&encode(&record)).unwrap(), record);
    }
    let webhooks = [
        WebhookKind::RunCompleted {
            output: json!({ "ok": true }).into(),
        },
        WebhookKind::RunFailed {
            status: Status::Aborted,
            error: "limit".into(),
        },
        WebhookKind::RunSuspended {
            waiting_for: vec!["signal \"approval\"".into()],
        },
        WebhookKind::RunQuarantined { reason: "x".into() },
        WebhookKind::ActionSchemaChanged {
            seq: 1,
            action: "t".into(),
            message: "m".into(),
        },
        WebhookKind::CredentialsExpired {
            seq: 1,
            action: "t".into(),
            message: "m".into(),
            reason: CredentialsReason::Missing,
            secret: None,
        },
        WebhookKind::QuotaAlert { alert: alert() },
    ];
    for kind in webhooks {
        let Projection::Webhooks { mut event } = outbox_webhooks().projection else {
            unreachable!()
        };
        event.kind = kind;
        let record = Outbox {
            projection: Projection::Webhooks { event },
            ..outbox_webhooks()
        };
        assert_eq!(decode::<Outbox>(&encode(&record)).unwrap(), record);
    }
    let spans = Outbox {
        projection: Projection::Otel {
            spans: vec![Span {
                status: SpanStatus::Unset {},
                kind: SpanKind::Internal,
                parent: None,
                attributes: BTreeMap::from([
                    ("a".into(), Attribute::Float(f64::NEG_INFINITY)),
                    ("b".into(), Attribute::Float(-2.25)),
                ]),
                ..match outbox_otel().projection {
                    Projection::Otel { spans, .. } => spans[0].clone(),
                    _ => unreachable!(),
                }
            }],
            logs: Vec::new(),
        },
        ..outbox_otel()
    };
    assert_eq!(decode::<Outbox>(&encode(&spans)).unwrap(), spans);
    let nan = encode(&Outbox {
        projection: Projection::Otel {
            spans: vec![Span {
                status: SpanStatus::Ok {},
                attributes: BTreeMap::from([("n".into(), Attribute::Float(f64::NAN))]),
                ..match outbox_otel().projection {
                    Projection::Otel { spans, .. } => spans[0].clone(),
                    _ => unreachable!(),
                }
            }],
            logs: Vec::new(),
        },
        ..outbox_otel()
    });
    let Projection::Otel { spans, .. } = decode::<Outbox>(&nan).unwrap().projection else {
        unreachable!()
    };
    assert!(matches!(spans[0].attributes["n"], Attribute::Float(value) if value.is_nan()));
    let quarantined = RunHead {
        quarantine: Some(Quarantine {
            kind: QuarantineKind::Failures,
            reason: "x".into(),
            since: T0,
            retried_by: None,
        }),
        metadata: Metadata::default(),
        waits: Vec::new(),
        sealed: Some(1),
        status: Status::Cancelled,
        ended_at: Some(T0),
        ..run_head()
    };
    assert_eq!(
        decode::<RunHead>(&encode(&quarantined)).unwrap(),
        quarantined
    );
    // A head and a summary keep an origin and a suffix that differ from the defaults; with the
    // defaults they write neither.
    let named = RunHead {
        origin: "triage-new-issues".into(),
        suffix_len: 17,
        ..run_head()
    };
    let bytes = encode(&named);
    assert!(
        String::from_utf8_lossy(&bytes).contains(r#""origin":"triage-new-issues","suffix_len":17"#)
    );
    assert_eq!(decode::<RunHead>(&bytes).unwrap(), named);
    assert!(!String::from_utf8_lossy(&encode(&run_head())).contains("origin"));
    let summarized = Summary {
        origin: "triage".into(),
        suffix_len: 13,
        ..summary()
    };
    assert_eq!(decode::<Summary>(&encode(&summarized)).unwrap(), summarized);
    for status in [
        Status::Pending,
        Status::Completed,
        Status::Aborted,
        Status::Active,
    ] {
        let record = Summary {
            status,
            mode: Some(Mode::Live),
            waits: vec![
                Wait {
                    kind: WaitKind::Signal,
                    seq: None,
                    name: Some("approval".into()),
                    since: T0,
                    until: None,
                    callsite: None,
                    task: None,
                },
                Wait {
                    kind: WaitKind::Harness,
                    task: Some(TaskPhase::Deferred),
                    ..wait()
                },
                Wait {
                    kind: WaitKind::Timer,
                    task: Some(TaskPhase::Claimed),
                    ..wait()
                },
                Wait {
                    task: Some(TaskPhase::Parked),
                    ..wait()
                },
                Wait {
                    task: Some(TaskPhase::Ready),
                    ..wait()
                },
            ],
            ..summary()
        };
        assert_eq!(decode::<Summary>(&encode(&record)).unwrap(), record);
    }
    for (quota, threshold) in [
        (QuotaKind::ActiveRuns, Threshold::Reached),
        (QuotaKind::RunsPerMonth, Threshold::Near),
        (QuotaKind::Workflows, Threshold::Near),
        (QuotaKind::MaxTtl, Threshold::Near),
        (QuotaKind::MaxCodeLines, Threshold::Near),
    ] {
        let record = Organization {
            alerts: vec![QuotaAlert {
                quota,
                threshold,
                namespace: None,
                sent_at: None,
                ..alert()
            }],
            admissions: Vec::new(),
        };
        assert_eq!(decode::<Organization>(&encode(&record)).unwrap(), record);
    }
    let levels = [Level::Debug, Level::Info, Level::Error];
    for level in levels {
        let record = Outbox {
            projection: Projection::Otel {
                spans: Vec::new(),
                logs: vec![Log {
                    level,
                    span_id: Some([3; 8]),
                    trace_id: [4; 16],
                    time: T0,
                    message: "m".into(),
                    attributes: BTreeMap::new(),
                }],
            },
            ..outbox_otel()
        };
        assert_eq!(decode::<Outbox>(&encode(&record)).unwrap(), record);
    }
    let failure_kinds = [
        FailureKind::Error,
        FailureKind::Unavailable,
        FailureKind::OutcomeUnknown,
    ];
    let record = Summary {
        call_errors: failure_kinds.into_iter().collect(),
        ..summary()
    };
    assert_eq!(decode::<Summary>(&encode(&record)).unwrap(), record);
    let waiting = Gauges {
        waiting: [
            WaitingFor::Signal,
            WaitingFor::Timer,
            WaitingFor::Call,
            WaitingFor::Harness,
            WaitingFor::Approval,
            WaitingFor::Parked,
            WaitingFor::Deferred,
        ]
        .into_iter()
        .map(|kind| (kind, 1))
        .collect(),
        ..gauges()
    };
    assert_eq!(decode::<Gauges>(&encode(&waiting)).unwrap(), waiting);
    let empty = Counters::default();
    assert_eq!(
        String::from_utf8(encode(&empty)).unwrap(),
        r#"{"v":1,"created":0}"#
    );
    assert_eq!(decode::<Counters>(&encode(&empty)).unwrap(), empty);
}

#[test]
fn payload_values_are_written_in_their_canonical_form() {
    let record = PayloadValue {
        value: Some(json!({ "b": [1.0, 2.5e-7], "a": "é\u{1}" })),
        ..payload_value()
    };
    let text = String::from_utf8(encode(&record)).unwrap();
    assert!(
        text.contains(r#""value":{"a":"é\u0001","b":[1,2.5e-7]}"#),
        "{text}"
    );
    let missing = PayloadValue {
        value: None,
        ..payload_value()
    };
    assert!(
        !String::from_utf8(encode(&missing))
            .unwrap()
            .contains("value")
    );
    assert_eq!(decode::<PayloadValue>(&encode(&missing)).unwrap(), missing);
}

#[test]
fn rejects_what_v1_does_not_define() {
    let fixture = String::from_utf8(encode(&timer())).unwrap();
    let cases = [
        (fixture.replace(r#""v":1"#, r#""v":2"#), "a version"),
        (fixture.replace(r#""v":1,"#, ""), "no version"),
        (
            fixture.replace(r#""tenant":"acme""#, r#""tenant":"acme","extra":1"#),
            "a field",
        ),
        (
            fixture.replace(r#""seq":3"#, r#""seq":3,"extra":1"#),
            "a field of a variant",
        ),
        (
            fixture.replace(r#""type":"code""#, r#""type":"alarm""#),
            "a variant",
        ),
        (
            fixture.replace(&run(1).to_string(), "not-a-run"),
            "a run id",
        ),
        ("[1]".to_owned(), "an array"),
        ("{".to_owned(), "JSON"),
    ];
    for (bytes, what) in cases {
        assert!(
            decode::<Timer>(bytes.as_bytes()).is_err(),
            "{what}: {bytes}"
        );
    }
    assert!(matches!(
        decode::<Timer>(fixture.replace(r#""v":1"#, r#""v":2"#).as_bytes()),
        Err(FormatError::UnsupportedVersion(_))
    ));
    assert!(matches!(
        decode::<Timer>(fixture.replace(r#""tenant""#, r#""tenants""#).as_bytes()),
        Err(FormatError::InvalidRecord {
            record: "timer",
            ..
        })
    ));
    let head = String::from_utf8(encode(&run_head())).unwrap();
    for (bytes, what) in [
        (
            head.replace(r#""status":"active""#, r#""status":"sleeping""#),
            "a status",
        ),
        (
            head.replace(
                &digest("script").to_hex(),
                &digest("script").to_hex().to_uppercase(),
            ),
            "an uppercase digest",
        ),
        (head.replace(r#""ns/github""#, r#""github""#), "a secret"),
        (
            head.replace(r#""labels""#, r#""tags""#),
            "a field of the metadata",
        ),
    ] {
        assert!(
            decode::<RunHead>(bytes.as_bytes()).is_err(),
            "{what}: {bytes}"
        );
    }
    let task = String::from_utf8(encode(&task())).unwrap();
    for (bytes, what) in [
        (
            task.replace(r#""type":"parked""#, r#""type":"lost""#),
            "a task state",
        ),
        (
            task.replace(r#""op":"start""#, r#""op":"start","allowed_hosts":[]"#),
            "a route mixing built-ins",
        ),
    ] {
        assert!(decode::<Task>(bytes.as_bytes()).is_err(), "{what}: {bytes}");
    }
    let otel = String::from_utf8(encode(&outbox_otel())).unwrap();
    assert!(decode::<Outbox>(otel.replace("0a0a0a0a", "0A0A0A0A").as_bytes()).is_err());
    let shape = String::from_utf8(encode(&shape())).unwrap();
    assert!(decode::<Shape>(shape.replace(r#""number""#, r#""integer""#).as_bytes()).is_err());
    assert!(decode::<SignalKey>(br#"{"v":1,"key":"x"}"#).is_err());
}
