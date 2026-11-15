//! The projector: the feed's consumers fill the read model, export spans and logs, and send
//! lifecycle webhooks, idempotently (`archi/05-testing-and-observability.md`).

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rewind_core::{
    AbortKind, AbortReason, Annotation, Callsite, CancelReason, Command, Completion, Digest, Epoch,
    Event, HistoryEntry, Lifecycle, McpServer, McpTransport, RunMode, RunStarted, RunStatus,
    Script, ScriptError, Seed, Seq, Timestamp, Tool, ToolRoute, Toolset,
};
use rewind_engine::adapters::{MemoryReadModel, MemorySpanExporter, MemoryWebhooks};
use rewind_engine::failures;
use rewind_engine::ports::{
    Delivery, Ending, FeedConsumer, JournalStore, Predicate, QuarantineKind, QuarantineNote,
    ReadModel, RunMetadata, RunOrder, RunQuery, RunRef, RunSnapshot, RunSummary, Secret, SpanId,
    SpanStatus, Subscription, TaskFilter, TaskPhase, TaskRef, TenantScope, TraceContext, TraceId,
    VirtualTime, Wait, WaitKind, WaitingFor, WebhookEventKind,
};
use rewind_engine::projector::{Projector, ProjectorConfig, headline, run_snapshot};
use rewind_engine::telemetry::{SpanKey, span_id};
use rewind_engine::{DriveOptions, EngineConfig, ForkRun, StartRun};
use serde_json::json;
use support::*;

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

struct Sinks {
    read_model: MemoryReadModel,
    exporter: MemorySpanExporter,
    webhooks: MemoryWebhooks,
}

impl Sinks {
    fn new() -> Self {
        Self {
            read_model: MemoryReadModel::new(),
            exporter: MemorySpanExporter::new(),
            webhooks: MemoryWebhooks::new(),
        }
    }

    fn projector<'a>(
        &'a self,
        engine: &'a TestEngine,
        config: ProjectorConfig,
    ) -> Projector<
        'a,
        rewind_engine::adapters::MemoryJournal,
        MemoryReadModel,
        MemorySpanExporter,
        MemoryWebhooks,
    > {
        Projector {
            journal: engine.journal(),
            read_model: &self.read_model,
            exporter: &self.exporter,
            webhooks: &self.webhooks,
            config,
        }
    }
}

fn now(engine: &TestEngine) -> rewind_core::Timestamp {
    rewind_engine::ports::Clock::now(engine.clock())
}

async fn drive(engine: &TestEngine) {
    let report = engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    assert!(report.failures.is_empty() && !report.interrupted);
}

async fn subscribe(engine: &TestEngine) {
    engine
        .journal()
        .put_subscription(&Subscription {
            tenant: rewind_engine::ports::TenantId::local(),
            id: "sub".into(),
            url: "https://harness.example.com/hooks".into(),
            secret: Secret::new("whsec_x"),
            events: BTreeSet::new(),
            labels: BTreeMap::new(),
        })
        .await
        .unwrap();
}

/// Every run of the tenant in the read model, newest first.
async fn summaries(read_model: &MemoryReadModel, run: &RunRef) -> Vec<RunSummary> {
    read_model
        .search_runs(
            &RunQuery::new(run.tenant.clone()),
            RunOrder::Started,
            None,
            10,
        )
        .await
        .unwrap()
}

async fn start(engine: &TestEngine, metadata: RunMetadata) -> RunRef {
    let mut request = request("flow", json!({}));
    request.metadata = metadata;
    engine.start_run(request).await.unwrap().run
}

#[tokio::test]
async fn committed_entries_are_projected_once_whatever_the_deliveries() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    subscribe(&engine).await;
    let sinks = Sinks::new();
    let projector = sinks.projector(&engine, ProjectorConfig::default());
    let run = start(&engine, RunMetadata::default()).await;
    drive(&engine).await;

    // Suspended twice: waiting for the host tool `a`, then for the signal.
    let applied = projector.project_all(now(&engine)).await.unwrap();
    assert_eq!(applied, 6, "two activations, three consumers");
    let deliveries = sinks.webhooks.deliveries();
    let suspended: Vec<(&str, Vec<String>)> = deliveries
        .iter()
        .map(|(_, event)| match &event.kind {
            WebhookEventKind::RunSuspended { waiting_for } => {
                (event.id.as_str(), waiting_for.clone())
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        suspended,
        [
            (
                format!("{}/1", run.id).as_str(),
                vec!["tool a #1".to_owned()]
            ),
            (
                format!("{}/4", run.id).as_str(),
                vec!["signal \"go\"".to_owned()]
            )
        ]
    );

    engine.signal(&run, "go", json!(null), None).await.unwrap();
    drive(&engine).await;
    projector.project_all(now(&engine)).await.unwrap();

    // The read model.
    let projected = summaries(&sinks.read_model, &run).await;
    assert_eq!(projected.len(), 1);
    assert_eq!(projected[0].status, RunStatus::Completed);
    assert_eq!(projected[0].tool_calls, 1);
    assert_eq!(projected[0].history_len, 7);
    assert_eq!(projected[0].headline, "flow", "the script's first line");
    assert!(projected[0].waits.is_empty());

    // The spans: the run, as a root of the run's own trace, and its tool call under it.
    let trace = TraceId(*run.id.as_uuid().as_bytes());
    let spans = sinks.exporter.spans();
    let names: BTreeSet<&str> = spans.iter().map(|span| span.name.as_str()).collect();
    assert_eq!(names, BTreeSet::from(["a", "run", "signal go"]));
    let root = spans.iter().find(|span| span.name == "run").unwrap();
    assert_eq!(root.span_id, span_id(run.id, SpanKey::Run));
    assert_eq!((root.trace_id, root.parent), (trace, None));
    assert_eq!(root.status, SpanStatus::Ok);
    let call = spans.iter().find(|span| span.name == "a").unwrap();
    assert_eq!(call.span_id, span_id(run.id, SpanKey::Command(Seq(1))));
    assert_eq!(call.parent, Some(root.span_id));
    assert!(call.attributes.contains_key("rewind.args.digest"));
    // The log, under the activation that committed it.
    let logs = sinks.exporter.logs();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0].message, "a answered");
    assert!(logs[0].span_id.is_some());

    // Lifecycle: run.completed, named by the entry.
    let deliveries = sinks.webhooks.deliveries();
    assert_eq!(deliveries.len(), 3);
    assert!(matches!(
        deliveries[2].1.kind,
        WebhookEventKind::RunCompleted { .. }
    ));
    assert_eq!(deliveries[2].1.id, format!("{}/6", run.id));

    // Every feed item is acknowledged; projecting the whole history again changes nothing.
    for consumer in FeedConsumer::ALL {
        let left = engine
            .journal()
            .claim_feed(
                consumer,
                "x",
                now(&engine),
                std::time::Duration::from_secs(1),
                10,
            )
            .await
            .unwrap();
        assert!(left.is_empty());
    }
    let history = engine.history(&run).await.unwrap();
    let record = engine.get_run(&run).await.unwrap().unwrap();
    sinks
        .read_model
        .apply(
            &rewind_engine::ports::ProjectedRun {
                run: run.clone(),
                script: record.script,
                headline: "flow".into(),
                toolset: record.toolset,
                labels: record.metadata.labels.clone(),
                parent: None,
                workflow: None,
                started_by: None,
            },
            &history,
            &run_snapshot(&history, None),
        )
        .await
        .unwrap();
    assert_eq!(summaries(&sinks.read_model, &run).await, projected);
    assert_eq!(sinks.exporter.spans().len(), spans.len());
}

#[tokio::test]
async fn a_run_started_with_a_traceparent_joins_the_harness_s_trace() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    let sinks = Sinks::new();
    let mut config = ProjectorConfig::default();
    config.telemetry.exported_labels = BTreeSet::from(["agent".to_owned()]);
    let projector = sinks.projector(&engine, config);
    let run = start(
        &engine,
        RunMetadata {
            labels: BTreeMap::from([
                ("agent".to_owned(), "triage".to_owned()),
                ("user".to_owned(), "alice@example.com".to_owned()),
            ]),
            trace_context: Some(TraceContext {
                traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
                tracestate: None,
            }),
            ..RunMetadata::default()
        },
    )
    .await;
    drive(&engine).await;
    engine.signal(&run, "go", json!(null), None).await.unwrap();
    drive(&engine).await;
    projector.project_all(now(&engine)).await.unwrap();

    let spans = sinks.exporter.spans();
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
}

#[tokio::test]
async fn unsampled_and_mock_runs_export_nothing_but_fill_the_read_model() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    let sinks = Sinks::new();
    let mut config = ProjectorConfig::default();
    config.telemetry.sampled_per_million = 0;
    let projector = sinks.projector(&engine, config);
    let run = start(&engine, RunMetadata::default()).await;
    let mut mock = request("flow", json!({}));
    mock.mode = RunMode::Mock;
    engine.start_run(mock).await.unwrap();
    drive(&engine).await;
    engine.signal(&run, "go", json!(null), None).await.unwrap();
    drive(&engine).await;
    projector.project_all(now(&engine)).await.unwrap();
    assert!(sinks.exporter.spans().is_empty());
    assert!(sinks.exporter.logs().is_empty());
    assert_eq!(summaries(&sinks.read_model, &run).await.len(), 2);

    // Mock runs are not exported even when every run is sampled.
    let sinks = Sinks::new();
    let engine = support::engine(programs(), RecordingDispatcher::echo());
    let projector = sinks.projector(&engine, ProjectorConfig::default());
    let mut mock = request("flow", json!({}));
    mock.mode = RunMode::Mock;
    engine.start_run(mock).await.unwrap();
    drive(&engine).await;
    projector.project_all(now(&engine)).await.unwrap();
    assert!(sinks.exporter.spans().is_empty());
}

#[tokio::test]
async fn a_quarantined_run_is_reported_to_the_harness() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    subscribe(&engine).await;
    let sinks = Sinks::new();
    let projector = sinks.projector(&engine, ProjectorConfig::default());
    let run = start(&engine, RunMetadata::default()).await;
    let lease = engine
        .journal()
        .acquire_run(&run, "w", now(&engine), std::time::Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    engine
        .journal()
        .quarantine(
            &lease,
            QuarantineKind::NonDeterminism,
            "history entry 1 is …".into(),
            now(&engine),
        )
        .await
        .unwrap();
    projector.notify_quarantined(&run).await.unwrap();
    let deliveries = sinks.webhooks.deliveries();
    assert_eq!(deliveries.len(), 1);
    assert!(matches!(
        &deliveries[0].1.kind,
        WebhookEventKind::RunQuarantined { reason } if reason == "history entry 1 is …"
    ));
}

#[tokio::test]
async fn a_composition_root_turns_webhook_items_into_their_deliveries() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    subscribe(&engine).await;
    let run = start(&engine, RunMetadata::default()).await;
    drive(&engine).await;
    let items = engine
        .journal()
        .claim_feed(
            FeedConsumer::Webhooks,
            "root",
            now(&engine),
            std::time::Duration::from_secs(300),
            10,
        )
        .await
        .unwrap();
    assert_eq!(items.len(), 2, "two activations");
    let mut deliveries = Vec::new();
    for item in &items {
        deliveries.extend(
            rewind_engine::projector::webhook_deliveries(engine.journal(), item)
                .await
                .unwrap(),
        );
    }
    // Suspended twice: waiting for the host tool `a`, then for the signal.
    assert_eq!(deliveries.len(), 2);
    for delivery in &deliveries {
        assert_eq!(delivery.subscription.id, "sub");
        assert_eq!(delivery.event.run, run.id);
        assert!(matches!(
            &delivery.event.kind,
            WebhookEventKind::RunSuspended { .. }
        ));
    }
    // Nothing was sent, and the item is the caller's to acknowledge.
    let sinks = Sinks::new();
    assert!(sinks.webhooks.deliveries().is_empty());
    let ids: Vec<u64> = items.iter().map(|item| item.id).collect();
    engine.journal().ack_feed(&ids).await.unwrap();
    assert!(
        engine
            .journal()
            .claim_feed(
                FeedConsumer::Webhooks,
                "root",
                rewind_core::Timestamp(now(&engine).0 + 600_000),
                std::time::Duration::from_secs(300),
                10,
            )
            .await
            .unwrap()
            .is_empty()
    );
}

// -------------------------------------------------------------------------------------------
// Snapshots and service state

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

/// Calls `github.get_issue` and `ask_user`, then waits.
fn gated() -> FakeExecutor {
    FakeExecutor::new().with(
        "gated",
        |run| {
            run.call("github.get_issue", json!({ "id": 1 }));
            run.call("ask_user", json!({}));
        },
        |_, _| {},
    )
}

#[tokio::test]
async fn task_states_quarantine_and_forks_reach_the_read_model() {
    // Host tools wait for a harness's worker, as on a server.
    let config = EngineConfig {
        tool_tasks: TaskFilter::Dispatched {
            tenants: TenantScope::All,
        },
        ..EngineConfig::default()
    };
    let engine = engine_with(gated(), RecordingDispatcher::echo(), config);
    let sinks = Sinks::new();
    let projector = sinks.projector(&engine, ProjectorConfig::default());
    let run = engine
        .start_run(StartRun::new(
            Script::new("gated"),
            gated_toolset(true),
            json!({}),
        ))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    projector.project_all(now(&engine)).await.unwrap();

    let waits =
        |summary: &RunSummary| -> Vec<(WaitKind, Option<String>, Option<TaskPhase>, WaitingFor)> {
            summary
                .waits
                .iter()
                .map(|wait| (wait.kind, wait.name.clone(), wait.task, wait.waiting_for()))
                .collect()
        };
    let projected = summaries(&sinks.read_model, &run).await.remove(0);
    assert_eq!(
        waits(&projected),
        [
            (
                WaitKind::Call,
                Some("github.get_issue".into()),
                Some(TaskPhase::Approval),
                WaitingFor::Approval
            ),
            (
                WaitKind::Host,
                Some("ask_user".into()),
                Some(TaskPhase::Ready),
                WaitingFor::Host
            ),
        ],
        "a task created waiting for its approval shows up with the entries"
    );
    assert_eq!(
        projected
            .waits
            .iter()
            .map(|wait| wait.callsite)
            .collect::<Vec<_>>(),
        [at_line(1, 1), at_line(2, 1)]
    );
    assert_eq!(projected.headline, "gated");
    let approvals = RunQuery::new(run.tenant.clone())
        .with(Predicate::Waiting(BTreeSet::from([WaitingFor::Approval])));
    let waiting_for_approval = || async {
        sinks
            .read_model
            .search_runs(&approvals, RunOrder::Wakes, None, 10)
            .await
            .unwrap()
            .len()
    };
    assert_eq!(waiting_for_approval().await, 1);

    // Whoever approves records the task's new state.
    engine
        .approve_task(&TaskRef {
            run: run.clone(),
            seq: Seq(1),
        })
        .await
        .unwrap();
    projector.record_tasks(&run).await.unwrap();
    assert_eq!(waiting_for_approval().await, 0);
    let approved = summaries(&sinks.read_model, &run).await.remove(0);
    assert_eq!(waits(&approved)[0].3, WaitingFor::Call);

    // Once the call is answered, only the host tool's call is left.
    drive(&engine).await;
    projector.project_all(now(&engine)).await.unwrap();
    let answered = summaries(&sinks.read_model, &run).await.remove(0);
    assert_eq!(
        waits(&answered),
        [(
            WaitKind::Host,
            Some("ask_user".into()),
            Some(TaskPhase::Ready),
            WaitingFor::Host
        )]
    );

    // A fork names its parent.
    let fork = engine
        .fork(ForkRun::new(run.clone(), Script::new("gated")))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    projector.project_all(now(&engine)).await.unwrap();
    let forked = summaries(&sinks.read_model, &run)
        .await
        .into_iter()
        .find(|summary| summary.run == fork)
        .unwrap();
    assert_eq!(forked.parent, Some(run.id));

    // Quarantine, then release: each recorded by whoever changes it.
    let lease = engine
        .journal()
        .acquire_run(&run, "w", now(&engine), Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    let quarantined_at = now(&engine);
    engine
        .journal()
        .quarantine(
            &lease,
            QuarantineKind::NonDeterminism,
            "history entry 1 is …".into(),
            quarantined_at,
        )
        .await
        .unwrap();
    projector
        .record_quarantine(&run, quarantined_at)
        .await
        .unwrap();
    let quarantine = |summaries: Vec<RunSummary>| {
        summaries
            .into_iter()
            .find(|summary| summary.run == run)
            .unwrap()
            .quarantine
    };
    assert_eq!(
        quarantine(summaries(&sinks.read_model, &run).await),
        Some(QuarantineNote {
            kind: QuarantineKind::NonDeterminism,
            reason: "history entry 1 is …".into(),
            since: quarantined_at,
        })
    );
    engine.clock().advance_to(ms(HOUR));
    assert!(engine.release_quarantine(&run).await.unwrap());
    projector
        .record_quarantine(&run, now(&engine))
        .await
        .unwrap();
    assert_eq!(quarantine(summaries(&sinks.read_model, &run).await), None);
}
