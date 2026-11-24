//! What committed entries project, in the driver (`archi/05-testing-and-observability.md`,
//! "The projector", "Span model"): a run's snapshot and headline, and the spans a run exports
//! through its partition's outbox, joined to the harness's trace, unless it is unsampled or a
//! mock run.

#![allow(clippy::unwrap_used)]

use crate::support;

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{
    AbortKind, AbortReason, Annotation, Callsite, CancelReason, Command, Completion, Digest, Epoch,
    Event, HistoryEntry, Lifecycle, McpServer, McpTransport, RunMode, RunStarted, RunStatus,
    ScriptError, Seed, Seq, Timestamp, Tool, ToolRoute, Toolset,
};
use rewind_engine::failures;
use rewind_engine::plane::NodeConfig;
use rewind_engine::ports::{
    Delivery, Ending, RunMetadata, RunSnapshot, SpanId, TraceContext, Wait, WaitKind,
};
use rewind_engine::projector::{headline, run_snapshot};
use serde_json::json;
use support::plane::{World, config};
use support::{DAY, FakeExecutor, RecordingDispatcher, T0, request};

/// Calls `a`, logs, then waits for `go` and completes.
fn programs() -> FakeExecutor {
    FakeExecutor::new().with(
        "flow",
        |run| {
            run.call("a", json!({ "n": 1 }));
        },
        |run, delivery| match delivery {
            Delivery::ToolCallCompleted { .. } => {
                run.log("a answered");
                run.await_signal("go");
            }
            Delivery::SignalReceived { .. } => run.complete(json!({ "done": true })),
            _ => {}
        },
    )
}

async fn world_with(node: NodeConfig) -> World {
    World::with(programs(), RecordingDispatcher::echo(), 1, &[node]).await
}

#[tokio::test]
async fn a_run_started_with_a_traceparent_joins_the_harness_s_trace() {
    let mut node = config("node-a", 1);
    node.telemetry.exported_labels = BTreeSet::from(["agent".to_owned()]);
    let mut world = world_with(node).await;
    let mut start = request("flow", json!({}));
    start.metadata = RunMetadata {
        labels: BTreeMap::from([
            ("agent".to_owned(), "triage".to_owned()),
            ("user".to_owned(), "alice@example.com".to_owned()),
        ]),
        trace_context: Some(TraceContext {
            traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
            tracestate: None,
        }),
        ..RunMetadata::default()
    };
    let run = world.run(start).await;
    world.signal(&run, "go", json!(null), None).await.unwrap();
    world.idle().await;

    let spans = world.plane.spans.spans();
    let root = spans.iter().find(|span| span.name == "run").unwrap();
    assert_eq!(
        root.trace_id.to_string(),
        "0af7651916cd43dd8448eb211c80319c"
    );
    assert_eq!(
        root.parent,
        Some(SpanId([0xb7, 0xad, 0x6b, 0x71, 0x69, 0x20, 0x33, 0x31]))
    );
    // Only allow-listed labels are exported.
    assert!(root.attributes.contains_key("rewind.label.agent"));
    assert!(!root.attributes.contains_key("rewind.label.user"));
    assert!(
        spans.iter().all(|span| span.trace_id == root.trace_id),
        "every span of the run is in the harness's trace"
    );
    assert!(!world.plane.spans.logs().is_empty(), "the log is exported");
}

#[tokio::test]
async fn unsampled_and_mock_runs_export_nothing() {
    let mut node = config("node-a", 1);
    node.telemetry.sampled_per_million = 0;
    let mut world = world_with(node).await;
    let run = world.run(request("flow", json!({}))).await;
    let mut mock = request("flow", json!({}));
    mock.mode = RunMode::Mock;
    world.run(mock).await;
    world.signal(&run, "go", json!(null), None).await.unwrap();
    world.idle().await;
    assert!(world.plane.spans.spans().is_empty());
    assert!(world.plane.spans.logs().is_empty());
    assert!(
        world.view(&run).await.summary.is_some(),
        "its summary is kept"
    );

    // Mock runs are not exported even when every run is sampled.
    let mut world = world_with(config("node-a", 1)).await;
    let mut mock = request("flow", json!({}));
    mock.mode = RunMode::Mock;
    world.run(mock).await;
    assert!(world.plane.spans.spans().is_empty());
}

fn ms(offset: u64) -> Timestamp {
    Timestamp(T0.0 + offset)
}

/// A history of these events, recorded at these offsets from `T0`.
fn history(events: Vec<(u64, Event)>) -> Vec<HistoryEntry> {
    events
        .into_iter()
        .enumerate()
        .map(|(index, (offset, event))| HistoryEntry {
            index: index as u64,
            recorded_at: ms(offset),
            event,
        })
        .collect()
}

fn run_started() -> Event {
    Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
        script: Digest::of(b"script"),
        toolset: Digest::of(b"toolset"),
        input: json!({}).into(),
        seed: Seed::from_bytes([0; 32]),
        epoch: Epoch(1),
        expires_at: ms(DAY),
        mode: RunMode::Live,
    }))
}

fn at_line(line: u32, column: u32) -> Option<Callsite> {
    Some(Callsite { line, column })
}

fn schedule(seq: u32, tool: &str, line: u32) -> Event {
    Event::Command(Command::ScheduleToolCall {
        seq: Seq(seq),
        tool: tool.into(),
        args: json!({}).into(),
        callsite: at_line(line, 1),
    })
}

fn timer(seq: u32, fire_at: u64, callsite: Option<Callsite>) -> Event {
    Event::Command(Command::StartTimer {
        seq: Seq(seq),
        duration_ms: 1,
        fire_at: ms(fire_at),
        callsite,
    })
}

fn awaited(name: &str, timeout: Option<u32>, callsite: Option<Callsite>) -> Event {
    Event::Annotation(Annotation::SignalAwaited {
        name: name.into(),
        timeout: timeout.map(Seq),
        callsite,
    })
}

/// `github.get_issue`, an MCP tool (behind an approval or not), and `ask_user`, a host tool.
fn gated_toolset(approval: bool) -> Toolset {
    let server = McpServer::new(
        "github",
        McpTransport::Http {
            url: "https://mcp.example.com/github".into(),
        },
    );
    let mut issue = Tool::new(
        "github.get_issue",
        ToolRoute::Mcp {
            server: "github".into(),
        },
        json!({}),
    );
    issue.policy.approval = approval;
    let ask = Tool::new("ask_user", ToolRoute::Host { group: None }, json!({}));
    Toolset::new([server], [issue, ask]).unwrap()
}

fn wait(
    kind: WaitKind,
    seq: Option<u32>,
    name: Option<&str>,
    since: u64,
    until: Option<u64>,
    callsite: Option<Callsite>,
) -> Wait {
    Wait {
        kind,
        seq: seq.map(Seq),
        name: name.map(str::to_owned),
        since: ms(since),
        until: until.map(ms),
        callsite,
        task: None,
    }
}

#[test]
fn a_snapshot_holds_what_an_active_run_waits_for() {
    let entries = history(vec![
        (0, run_started()),
        (0, schedule(1, "ask_user", 2)),
        (0, schedule(2, "github.get_issue", 3)),
        (10, timer(3, 60_010, at_line(4, 3))),
        // The signal's timeout, then the signal.
        (20, timer(4, 90_020, at_line(5, 9))),
        (20, awaited("approval", Some(4), at_line(5, 9))),
        // A timer cancelled waits for nothing.
        (30, timer(5, 30_030, None)),
        (40, Event::Command(Command::CancelTimer { seq: Seq(5) })),
        (40, awaited("review", None, None)),
        (
            50,
            Event::Completion(Completion::TimerFired { seq: Seq(4) }),
        ),
        (
            60,
            Event::Completion(Completion::SignalReceived {
                name: "review".into(),
                payload: json!(null).into(),
            }),
        ),
    ]);
    let toolset = gated_toolset(false);
    let calls = [
        wait(
            WaitKind::Call,
            Some(2),
            Some("github.get_issue"),
            0,
            None,
            at_line(3, 1),
        ),
        wait(
            WaitKind::Host,
            Some(1),
            Some("ask_user"),
            0,
            None,
            at_line(2, 1),
        ),
    ];
    let sleep = wait(
        WaitKind::Timer,
        Some(3),
        None,
        10,
        Some(60_010),
        at_line(4, 3),
    );
    let approval = wait(
        WaitKind::Signal,
        Some(4),
        Some("approval"),
        20,
        Some(90_020),
        at_line(5, 9),
    );
    let review = wait(WaitKind::Signal, None, Some("review"), 40, None, None);
    assert_eq!(
        run_snapshot(&entries[..9], Some(&toolset)),
        RunSnapshot {
            history_len: 9,
            status: RunStatus::Active,
            mode: Some(RunMode::Live),
            waits: vec![
                calls[0].clone(),
                calls[1].clone(),
                sleep.clone(),
                approval,
                review.clone()
            ],
            ending: None,
        },
        "by since, then kind: the signal's timeout is its wait's end, not a timer of its own"
    );
    // Without the toolset, a host tool's call is a call.
    let kinds: Vec<(WaitKind, Option<Seq>)> = run_snapshot(&entries[..9], None)
        .waits
        .iter()
        .map(|wait| (wait.kind, wait.seq))
        .collect();
    assert_eq!(
        kinds[..2],
        [
            (WaitKind::Call, Some(Seq(1))),
            (WaitKind::Call, Some(Seq(2)))
        ]
    );
    // The timeout fired: the signal's wait is over.
    assert_eq!(
        run_snapshot(&entries[..10], Some(&toolset)).waits,
        [calls[0].clone(), calls[1].clone(), sleep.clone(), review]
    );
    // The signal came.
    let last = run_snapshot(&entries, Some(&toolset));
    assert_eq!(last.history_len, 11);
    assert_eq!(last.waits, [calls[0].clone(), calls[1].clone(), sleep]);
    // A history that cannot be folded stops before the entry that breaks it.
    let broken = run_snapshot(&entries[1..], Some(&toolset));
    assert_eq!(
        (broken.history_len, broken.status, broken.waits.len()),
        (0, RunStatus::Pending, 0)
    );
}

#[test]
fn a_snapshot_tells_how_the_run_ended() {
    let message = "Cannot read properties of undefined (reading 'id')";
    let failed = history(vec![
        (0, run_started()),
        (
            10,
            Event::Command(Command::FailRun {
                error: ScriptError {
                    name: "TypeError".into(),
                    message: message.into(),
                    stack: Some(format!(
                        "TypeError: {message}\n    at main (file:///script.ts:7:13)"
                    )),
                },
            }),
        ),
    ]);
    assert_eq!(
        run_snapshot(&failed, None),
        RunSnapshot {
            history_len: 2,
            status: RunStatus::Failed,
            mode: Some(RunMode::Live),
            waits: Vec::new(),
            ending: Some(Ending {
                error: "TypeError".into(),
                message: message.into(),
                line: Some(7),
                signature: format!("TypeError: {message}"),
                fingerprint: "d874f3620ac13ff9".into(),
            }),
        }
    );

    let aborted = history(vec![
        (0, run_started()),
        (0, schedule(1, "ask_user", 2)),
        (
            10,
            Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
                kind: AbortKind::LimitExceeded,
                message: "the script used 5000 ms of CPU time".into(),
            })),
        ),
    ]);
    let snapshot = run_snapshot(&aborted, Some(&gated_toolset(false)));
    assert_eq!(snapshot.status, RunStatus::Aborted);
    assert!(snapshot.waits.is_empty(), "an ended run waits for nothing");
    let signature = "limit_exceeded: the script used <n> ms of CPU time";
    assert_eq!(
        snapshot.ending,
        Some(Ending {
            error: "limit_exceeded".into(),
            message: "the script used 5000 ms of CPU time".into(),
            line: None,
            signature: signature.into(),
            fingerprint: failures::fingerprint(signature),
        })
    );

    let cancelled = history(vec![
        (0, run_started()),
        (0, awaited("go", None, None)),
        (
            10,
            Event::Completion(Completion::CancelRequested {
                reason: CancelReason::Requested(None),
            }),
        ),
        (
            10,
            Event::Command(Command::FailRun {
                error: ScriptError {
                    name: ScriptError::CANCELLED.into(),
                    message: "the run was cancelled".into(),
                    stack: None,
                },
            }),
        ),
    ]);
    let snapshot = run_snapshot(&cancelled, None);
    assert_eq!(snapshot.status, RunStatus::Cancelled);
    assert_eq!(
        snapshot.ending.map(|ending| ending.error),
        Some(ScriptError::CANCELLED.to_owned())
    );

    let completed = history(vec![
        (0, run_started()),
        (
            5,
            Event::Command(Command::CompleteRun {
                output: json!(1).into(),
            }),
        ),
    ]);
    assert_eq!(run_snapshot(&completed, None).ending, None);
}

#[test]
fn the_headline_is_the_first_line_that_is_not_blank() {
    assert_eq!(
        headline("\n   \n  // Close the stale issues  \nconst x = 1;"),
        "// Close the stale issues"
    );
    assert_eq!(headline("  \n\t\n"), "");
}
